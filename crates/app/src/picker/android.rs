//! Android: the Storage Access Framework. `ACTION_OPEN_DOCUMENT` (several files) is
//! launched by `MarginPicker.kt`, which build.rs adds to dx's Gradle project: it
//! registers a one-off activity result callback with the activity's
//! `ActivityResultRegistry` (dx's `MainActivity` is an `AppCompatActivity` through wry's
//! `WryActivity`) and hands the picked URIs back to `on_picked`. The chosen documents are
//! copied through the `ContentResolver` into the staging folder.
//!
//! A folder to attach is picked with `ACTION_OPEN_DOCUMENT_TREE` the same way; the
//! callback persists the tree's read grant before handing it back, and
//! `folders::android` reads the tree in place from then on.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{JClass, JObject, JObjectArray, JString, JValue};
use jni::refs::{Global, IntoAuto as _};
use jni::signature::MethodSignature;
use jni::strings::JNIStr;
use jni::sys::jlong;
use jni::{Env, EnvUnowned, JavaVM, NativeMethod, Outcome, jni_sig, jni_str};
use tokio::sync::oneshot;

use crate::folders::{FolderHandle, FolderPick};
use crate::sources::{Selection, select_named};

/// The Kotlin side, in the package dx gives wry's classes.
const CLASS: &str = "dev.dioxus.main.MarginPicker";

/// What a pick hands back: the picked URIs (empty: cancelled), or `None` when the picker
/// couldn't be shown.
type Reply = oneshot::Sender<Option<Vec<String>>>;

/// A pick waiting for its result.
struct Pending {
    token: i64,
    reply: Reply,
    /// The app's event loop, to wake after replying.
    looper: EventLoopLooper,
}

/// Picks waiting for their result.
static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static NEXT_TOKEN: AtomicI64 = AtomicI64::new(1);

fn take_pending(token: i64) -> Option<Pending> {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let index = pending.iter().position(|p| p.token == token)?;
    Some(pending.swap_remove(index))
}

#[repr(C)]
struct ALooper {
    _private: [u8; 0],
}

// libandroid (the NDK looper API); dx links it into every Android build.
unsafe extern "C" {
    fn ALooper_forThread() -> *mut ALooper;
    fn ALooper_acquire(looper: *mut ALooper);
    fn ALooper_release(looper: *mut ALooper);
    fn ALooper_wake(looper: *mut ALooper);
}

/// The looper of the thread Dioxus runs on (tao's event loop).
///
/// Waking the app from the picker's result needs a nudge: the result arrives on the UI
/// thread while the activity resumes, and tao 0.34's Android loop handles one event source
/// per turn, so when its looper wakes for the resume events at the same moment, the wake
/// that Dioxus's waker sends is drained with them and the Dioxus task isn't polled until
/// the next event (a tap). Waking the looper again shortly after replying gets it polled.
struct EventLoopLooper(*mut ALooper);

// SAFETY: ALooper_wake may be called from any thread; the reference is acquired.
unsafe impl Send for EventLoopLooper {}

impl EventLoopLooper {
    fn current() -> Option<Self> {
        // SAFETY: returns this thread's looper (or null) without taking a reference.
        let looper = unsafe { ALooper_forThread() };
        if looper.is_null() {
            return None;
        }
        // SAFETY: a live looper; released in Drop.
        unsafe { ALooper_acquire(looper) };
        Some(Self(looper))
    }

    /// Wake the loop now and a few times over the next second, from another thread.
    fn wake_soon(self) {
        std::thread::spawn(move || {
            for delay in [0, 50, 200, 500, 1000] {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                self.wake();
            }
        });
    }

    fn wake(&self) {
        // SAFETY: acquired in `current`.
        unsafe { ALooper_wake(self.0) };
    }
}

impl Drop for EventLoopLooper {
    fn drop(&mut self) {
        // SAFETY: acquired in `current`.
        unsafe { ALooper_release(self.0) };
    }
}

/// Run `f` with this thread attached to the JVM; a Java exception is logged, cleared and
/// returned as `None`.
fn java<T>(f: impl FnOnce(&mut Env) -> jni::errors::Result<T>) -> Option<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context hands out the process JavaVM that the activity glue registered.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let outcome = vm.attach_current_thread_for_scope(|env| {
        f(env).inspect_err(|_| {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
        })
    });
    match outcome {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!("picker: {error}");
            None
        }
    }
}

/// The activity (also the `Context` for the `ContentResolver`).
fn activity<'a>(env: &Env<'a>) -> JObject<'a> {
    // SAFETY: ndk-context holds a global reference to the activity for the app's life.
    unsafe { JObject::from_raw(env, ndk_context::android_context().context().cast()) }
}

/// `MarginPicker`, loaded through the activity's class loader (a thread Rust attached
/// only sees the system classes) with its native method registered.
fn picker_class(env: &mut Env) -> jni::errors::Result<&'static Global<JClass<'static>>> {
    static CLASS_REF: OnceLock<Global<JClass<'static>>> = OnceLock::new();
    if let Some(class) = CLASS_REF.get() {
        return Ok(class);
    }
    let activity = activity(env);
    let loader = env
        .call_method(
            activity,
            jni_str!("getClassLoader"),
            jni_sig!("()Ljava/lang/ClassLoader;"),
            &[],
        )?
        .l()?;
    let name = env.new_string(CLASS)?;
    let class = env
        .call_method(
            &loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = env.cast_local::<JClass>(class)?;
    // SAFETY: `on_picked` takes `(long, String[])` and returns nothing, as the signature says.
    unsafe {
        env.register_native_methods(
            &class,
            &[NativeMethod::from_raw_parts(
                jni_str!("onPicked"),
                jni_str!("(J[Ljava/lang/String;)V"),
                on_picked as *mut c_void,
            )],
        )
    }?;
    let global = env.new_global_ref(class)?;
    Ok(CLASS_REF.get_or_init(|| global))
}

/// `MarginPicker.onPicked`: the picker's result, on the UI thread.
extern "system" fn on_picked<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    token: jlong,
    uris: JObjectArray<'local, JString<'local>>,
) {
    let uris = if uris.is_null() {
        None
    } else {
        match env.with_env(|env| strings(env, &uris)).into_outcome() {
            Outcome::Ok(list) => Some(list),
            Outcome::Err(error) => {
                tracing::warn!("picker: couldn't read the picked documents: {error}");
                None
            }
            Outcome::Panic(_) => {
                tracing::warn!("picker: reading the picked documents panicked");
                None
            }
        }
    };
    if let Some(pending) = take_pending(token) {
        let _ = pending.reply.send(uris);
        pending.looper.wake_soon();
    }
}

fn strings(env: &mut Env, array: &JObjectArray<JString>) -> jni::errors::Result<Vec<String>> {
    let len = array.len(env)?;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let item = array.get_element(env, i)?.auto();
        out.push(item.try_to_string(env)?);
    }
    Ok(out)
}

/// Call a static `MarginPicker` method that returns a `String[]` (`None` for null).
fn call_strings(
    env: &mut Env,
    method: &JNIStr,
    sig: MethodSignature,
    args: &[&str],
) -> jni::errors::Result<Option<Vec<String>>> {
    let class = picker_class(env)?;
    let context = activity(env);
    let mut values = vec![JValue::Object(&context)];
    let strings_in = args
        .iter()
        .map(|s| env.new_string(s))
        .collect::<Result<Vec<_>, _>>()?;
    values.extend(strings_in.iter().map(|s| JValue::Object(s)));
    let result = env.call_static_method(class, method, sig, &values)?.l()?;
    let result = env.cast_local::<JObjectArray<JString>>(result)?.auto();
    if result.is_null() {
        return Ok(None);
    }
    strings(env, &result).map(Some)
}

/// The folder picker: the system's tree picker (`ACTION_OPEN_DOCUMENT_TREE`), whose read
/// grant `MarginPicker` persists in the result callback, so the folder stays readable
/// across restarts until `folders::forget` releases it. Named by the tree's root folder;
/// located by the storage or app holding it and the path inside.
pub async fn pick_folder() -> Option<FolderPick> {
    let tree = show(true).await?.into_iter().next()?;
    tokio::task::spawn_blocking(move || describe_tree(tree))
        .await
        .ok()
        .flatten()
}

fn describe_tree(tree: String) -> Option<FolderPick> {
    let root = java(|env| {
        call_strings(
            env,
            jni_str!("treeRoot"),
            jni_sig!("(Landroid/content/Context;Ljava/lang/String;)[Ljava/lang/String;"),
            &[&tree],
        )
    })
    .flatten();
    match root.as_deref() {
        Some([status, _id, name, location]) if status == "ok" => Some(FolderPick {
            name: name.clone(),
            location: location.clone(),
            handle: FolderHandle::AndroidTree { uri: tree },
        }),
        _ => {
            tracing::warn!("picker: the picked folder couldn't be read");
            // Don't keep a grant for a folder that isn't attached.
            crate::folders::forget(&FolderHandle::AndroidTree { uri: tree });
            None
        }
    }
}

pub async fn pick_files() -> Option<Selection> {
    let uris = show(false).await?;
    if uris.is_empty() {
        return None;
    }
    let dir = super::new_staging_dir()?;
    tokio::task::spawn_blocking(move || {
        select_documents(uris).stage(&dir, |doc, to| copy(&doc.uri, to))
    })
    .await
    .ok()
}

/// Show the files picker or the folder picker and wait for what was picked: the URIs
/// (empty: cancelled), or `None` when it couldn't be shown.
async fn show(folder: bool) -> Option<Vec<String>> {
    let Some(looper) = EventLoopLooper::current() else {
        tracing::warn!("picker: the app's thread has no looper");
        return None;
    };
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (reply, rx) = oneshot::channel();
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Pending {
            token,
            reply,
            looper,
        });
    let shown = java(|env| {
        let class = picker_class(env)?;
        let activity = activity(env);
        env.call_static_method(
            class,
            jni_str!("pick"),
            jni_sig!("(Landroid/app/Activity;ZJ)V"),
            &[
                JValue::Object(&activity),
                JValue::Bool(folder),
                JValue::Long(token),
            ],
        )?;
        Ok(())
    });
    if shown.is_none() {
        take_pending(token);
        return None;
    }
    rx.await.ok().flatten()
}

/// A document: its URI, and its size when the provider knows it.
#[derive(Debug, Clone)]
struct Doc {
    uri: String,
    size: Option<u64>,
}

fn parse_size(text: &str) -> Option<u64> {
    text.parse().ok()
}

/// Files picked one by one, named by their display names.
fn select_documents(uris: Vec<String>) -> Selection<Doc> {
    let docs = uris
        .into_iter()
        .map(|uri| {
            let described = java(|env| {
                call_strings(
                    env,
                    jni_str!("describe"),
                    jni_sig!("(Landroid/content/Context;Ljava/lang/String;)[Ljava/lang/String;"),
                    &[&uri],
                )
            })
            .flatten();
            let (name, size) = match described.as_deref() {
                Some([name, size]) => (name.clone(), parse_size(size)),
                _ => (String::new(), None),
            };
            let doc = Doc { uri, size };
            (doc, name)
        })
        .collect();
    select_named(docs, |doc| doc.size)
}

/// Copy a document into `to` through the `ContentResolver`.
fn copy(uri: &str, to: &std::path::Path) -> Result<(), String> {
    let to = to.to_string_lossy();
    let outcome = java(|env| {
        let class = picker_class(env)?;
        let context = activity(env);
        let uri = env.new_string(uri)?;
        let path = env.new_string(to.as_ref())?;
        let error = env
            .call_static_method(
                class,
                jni_str!("copy"),
                jni_sig!("(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                &[
                    JValue::Object(&context),
                    JValue::Object(&uri),
                    JValue::Object(&path),
                ],
            )?
            .l()?;
        if error.is_null() {
            return Ok(None);
        }
        let error = env.cast_local::<JString>(error)?;
        let message = error.try_to_string(env)?;
        Ok(Some(message))
    });
    match outcome {
        Some(None) => Ok(()),
        Some(Some(message)) => Err(message),
        None => Err("The document couldn't be copied.".into()),
    }
}
