//! Opening a held folder on Android (4c): the tree URI the folder picker returned, with
//! the read grant it persisted (`MarginPicker.takePersistable`), read through a `SafDir`
//! over `DocumentsContract` (`docs/agent-phase4-brief.md` D1, D11).
//!
//! A document tree has no paths, only document ids, so `SafDir` walks a relative path
//! from the root one child query at a time, keeping every folder's children it has
//! listed (the sync lists each folder before it stats or opens what's in it, so those
//! cost nothing more). Files are read through a file descriptor the provider opens
//! (`openFileDescriptor(uri, "r")`, detached and owned here), which streams.
//!
//! The calls go to `MarginPicker.kt` through JNI; each answers a status first: "ok",
//! "revoked" (a `SecurityException`: the grant is gone), "notfound" or "error".

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JClass, JObject, JObjectArray, JString, JValue};
use jni::{JNIEnv, JavaVM};

use super::{Dir, DirEntry, DirError, DirKind, FolderHandle};

/// The Kotlin side, in the package dx gives wry's classes.
const CLASS: &str = "dev.dioxus.main.MarginPicker";

/// `DocumentsContract.Document.MIME_TYPE_DIR`.
const DIR_MIME: &str = "vnd.android.document/directory";

/// What a `MarginPicker` folder call answered.
#[derive(Debug)]
enum Answer {
    Ok(Vec<String>),
    /// The grant is gone; the rest of what was answered.
    Revoked(Vec<String>),
    NotFound(Vec<String>),
    Error(String),
}

/// The activity (also the `Context` for the `ContentResolver`).
fn activity<'a>() -> JObject<'a> {
    // SAFETY: ndk-context holds a global reference to the activity for the app's life.
    unsafe { JObject::from_raw(ndk_context::android_context().context().cast()) }
}

/// `MarginPicker`, loaded through the activity's class loader (a thread Rust attached
/// only sees the system classes).
fn class(env: &mut JNIEnv) -> jni::errors::Result<&'static GlobalRef> {
    static CLASS_REF: OnceLock<GlobalRef> = OnceLock::new();
    if let Some(class) = CLASS_REF.get() {
        return Ok(class);
    }
    let loader = env
        .call_method(
            activity(),
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )?
        .l()?;
    let name = env.new_string(CLASS)?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let global = env.new_global_ref(class)?;
    Ok(CLASS_REF.get_or_init(|| global))
}

/// Run `f` on this thread attached to the JVM, inside a local frame (the sync's blocking
/// thread stays attached and makes thousands of calls, so local references must not
/// pile up). A Java exception is cleared and returned as the error's text.
fn java<T>(f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<T>) -> Result<T, String> {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context hands out the process JavaVM that the activity glue registered.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    // Detached when the thread ends.
    let mut env = vm
        .attach_current_thread_permanently()
        .map_err(|e| e.to_string())?;
    let outcome = env.with_local_frame(16, |env| f(env));
    outcome.map_err(|error| {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        error.to_string()
    })
}

/// The `String[]` items.
fn strings(env: &mut JNIEnv, array: &JObjectArray) -> jni::errors::Result<Vec<String>> {
    let len = env.get_array_length(array)?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let item = env.get_object_array_element(array, i)?;
        let item = env.auto_local(JString::from(item));
        out.push(env.get_string(&item)?.into());
    }
    Ok(out)
}

/// Call `MarginPicker.<method>(Context, String...): String[]`.
fn call(method: &str, args: &[&str]) -> Answer {
    let sig = format!(
        "(Landroid/content/Context;{})[Ljava/lang/String;",
        "Ljava/lang/String;".repeat(args.len())
    );
    let answered = java(|env| {
        let class = class(env)?;
        let context = activity();
        let args = args
            .iter()
            .map(|s| env.new_string(s))
            .collect::<Result<Vec<_>, _>>()?;
        let mut values = vec![JValue::Object(&context)];
        values.extend(args.iter().map(|s| JValue::Object(s)));
        let result = env
            .call_static_method(<&JClass>::from(class.as_obj()), method, &sig, &values)?
            .l()?;
        let result = JObjectArray::from(result);
        if result.is_null() {
            return Ok(Vec::new());
        }
        strings(env, &result)
    });
    let mut answer = match answered {
        Ok(answer) => answer,
        Err(error) => {
            return Answer::Error(format!("Android's file access failed: {error}"));
        }
    };
    if answer.is_empty() {
        return Answer::Error(format!("Android's file access gave no answer to {method}."));
    }
    let status = answer.remove(0);
    match status.as_str() {
        "ok" => Answer::Ok(answer),
        "revoked" => Answer::Revoked(answer),
        "notfound" => Answer::NotFound(answer),
        _ => Answer::Error(
            answer
                .into_iter()
                .next()
                .unwrap_or_else(|| "The folder couldn't be read.".into()),
        ),
    }
}

/// The message the Folders pane shows when the grant is gone.
fn no_access(name: &str) -> String {
    format!("Margin no longer has access to the folder “{name}”; attach it again on this device.")
}

/// The message when the folder itself is gone.
fn missing(name: &str) -> String {
    format!("The folder “{name}” can't be found; attach it again on this device.")
}

/// A time the provider gave in milliseconds since 1970; 0 and nothing are unknown.
fn millis(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    text.parse::<i64>()
        .ok()
        .filter(|ms| *ms > 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
}

/// A document of the tree: its id and its entry.
#[derive(Debug, Clone)]
struct Child {
    id: String,
    entry: DirEntry,
}

impl Child {
    /// From a display name, MIME type, size and last modified.
    fn new(id: String, name: String, mime: &str, size: &str, modified: &str) -> Self {
        let kind = if mime == DIR_MIME {
            DirKind::Dir
        } else {
            DirKind::File
        };
        let bytes = match kind {
            DirKind::Dir => 0,
            DirKind::File => size.parse().unwrap_or(0),
        };
        Child {
            id,
            entry: DirEntry {
                name,
                kind,
                bytes,
                modified: millis(modified),
            },
        }
    }
}

/// A held document tree, open for one sync.
struct SafDir {
    tree: String,
    root_id: String,
    /// The folder's name, for the messages.
    name: String,
    /// The children of every folder listed so far, by its document id.
    listed: Mutex<HashMap<String, Vec<Child>>>,
}

impl SafDir {
    fn fail<T>(&self, answer: Answer) -> Result<T, DirError> {
        Err(match answer {
            Answer::Ok(_) => DirError::Io("Android's file access answered oddly.".into()),
            Answer::Revoked(_) => DirError::Revoked(no_access(&self.name)),
            Answer::NotFound(_) => DirError::NotFound,
            Answer::Error(message) => DirError::Io(message),
        })
    }

    /// The children of the folder with this document id, listed once per `SafDir`.
    fn children(&self, id: &str) -> Result<Vec<Child>, DirError> {
        if let Some(children) = self.lock().get(id) {
            return Ok(children.clone());
        }
        let items = match call("children", &[&self.tree, id]) {
            Answer::Ok(items) => items,
            other => return self.fail(other),
        };
        let children: Vec<Child> = items
            .as_chunks::<5>()
            .0
            .iter()
            .map(|[id, name, mime, size, modified]| {
                Child::new(id.clone(), name.clone(), mime, size, modified)
            })
            // A document without a name can't be named by a path.
            .filter(|c| !c.entry.name.is_empty())
            .collect();
        self.lock().insert(id.to_string(), children.clone());
        Ok(children)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Child>>> {
        self.listed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The document `rel` names: `None` for the root.
    fn resolve(&self, rel: &str) -> Result<Option<Child>, DirError> {
        let mut found: Option<Child> = None;
        for part in rel.split('/').filter(|part| !part.is_empty()) {
            if part == "." || part == ".." {
                return Err(DirError::Outside);
            }
            let parent = match &found {
                None => self.root_id.clone(),
                Some(child) if child.entry.kind == DirKind::Dir => child.id.clone(),
                // A path through a file.
                Some(_) => return Err(DirError::NotFound),
            };
            let child = self
                .children(&parent)?
                .into_iter()
                .find(|child| child.entry.name == part)
                .ok_or(DirError::NotFound)?;
            found = Some(child);
        }
        Ok(found)
    }
}

impl Dir for SafDir {
    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError> {
        let id = match self.resolve(rel)? {
            None => self.root_id.clone(),
            Some(child) if child.entry.kind == DirKind::Dir => child.id,
            Some(_) => return Err(DirError::NotADir),
        };
        Ok(self
            .children(&id)?
            .into_iter()
            .map(|child| child.entry)
            .collect())
    }

    fn stat(&self, rel: &str) -> Result<DirEntry, DirError> {
        if let Some(child) = self.resolve(rel)? {
            return Ok(child.entry);
        }
        match call("documentInfo", &[&self.tree, &self.root_id]) {
            Answer::Ok(info) if info.len() == 4 => {
                let name = if info[0].is_empty() {
                    self.name.clone()
                } else {
                    info[0].clone()
                };
                Ok(Child::new(self.root_id.clone(), name, &info[1], &info[2], &info[3]).entry)
            }
            Answer::NotFound(_) => Err(DirError::Revoked(missing(&self.name))),
            other => self.fail(other),
        }
    }

    fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError> {
        let child = match self.resolve(rel)? {
            Some(child) if child.entry.kind == DirKind::File => child,
            _ => return Err(DirError::IsADir),
        };
        let fd = match call("openRead", &[&self.tree, &child.id]) {
            Answer::Ok(answer) => answer.first().and_then(|fd| fd.parse::<i32>().ok()),
            other => return self.fail(other),
        };
        match fd {
            // SAFETY: `detachFd` handed this descriptor over; nothing else closes it.
            Some(fd) if fd >= 0 => Ok(Box::new(unsafe { std::fs::File::from_raw_fd(fd) })),
            _ => Err(DirError::Io(format!(
                "Android didn't open “{}” for reading.",
                child.entry.name
            ))),
        }
    }
}

pub fn open(handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    let FolderHandle::AndroidTree { uri } = handle else {
        return Err(DirError::Io(
            "This folder was attached on another kind of device; Android can't open it.".into(),
        ));
    };
    // The answer after a failure's message is the name the tree's id gives.
    let name_after = |rest: &[String]| rest.get(1).cloned().unwrap_or_default();
    match call("treeRoot", &[uri]) {
        Answer::Ok(root) if root.len() >= 2 => Ok(Box::new(SafDir {
            tree: uri.clone(),
            root_id: root[0].clone(),
            name: root[1].clone(),
            listed: Mutex::new(HashMap::new()),
        })),
        Answer::Ok(_) => Err(DirError::Io("Android's file access answered oddly.".into())),
        Answer::Revoked(rest) => Err(DirError::Revoked(no_access(&name_after(&rest)))),
        Answer::NotFound(rest) => Err(DirError::Revoked(missing(&name_after(&rest)))),
        Answer::Error(message) => Err(DirError::Io(message)),
    }
}

/// Gives up the read grant kept for the folder.
pub fn forget(handle: &FolderHandle) {
    let FolderHandle::AndroidTree { uri } = handle else {
        return;
    };
    let released = java(|env| {
        let class = class(env)?;
        let context = activity();
        let uri = env.new_string(uri)?;
        env.call_static_method(
            <&JClass>::from(class.as_obj()),
            "releasePersistable",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(&context), JValue::Object(&uri)],
        )?;
        Ok(())
    });
    if let Err(error) = released {
        tracing::warn!("folders: couldn't release a folder's grant: {error}");
    }
}
