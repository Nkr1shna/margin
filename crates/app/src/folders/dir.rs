//! `StdDir`: a held folder on a local file system (`std::fs`), for macOS, Linux and,
//! inside its security scope, iOS. Paths resolve component by component with
//! `symlink_metadata`, so a symbolic link at any step is `Outside`; a final
//! `canonicalize`, when it succeeds, must stay under the canonical root.

use std::fs::{self, Metadata};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{Dir, DirEntry, DirError, DirKind};

pub struct StdDir {
    root: PathBuf,
    /// The canonical root, when the platform can tell.
    canonical: Option<PathBuf>,
}

impl StdDir {
    /// A folder at `root`. `NotFound` / `NotADir` when it isn't one.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, DirError> {
        let root = root.into();
        let meta = fs::metadata(&root).map_err(io_error)?;
        if !meta.is_dir() {
            return Err(DirError::NotADir);
        }
        let canonical = fs::canonicalize(&root).ok();
        Ok(Self { root, canonical })
    }

    /// `rel` as a path under the root, refusing symbolic links at every component.
    fn resolve(&self, rel: &str) -> Result<(PathBuf, Metadata), DirError> {
        let components = components(rel)?;
        let mut path = self.root.clone();
        let mut meta = fs::symlink_metadata(&path).map_err(io_error)?;
        for part in components {
            if !meta.is_dir() {
                return Err(DirError::NotADir);
            }
            path.push(part);
            meta = fs::symlink_metadata(&path).map_err(io_error)?;
            if meta.file_type().is_symlink() {
                return Err(DirError::Outside);
            }
        }
        if let (Some(root), Ok(real)) = (&self.canonical, fs::canonicalize(&path))
            && !real.starts_with(root)
        {
            return Err(DirError::Outside);
        }
        Ok((path, meta))
    }
}

/// The components of a relative path: no leading `/` or `\\`, no drive letter, no
/// `..`, no NUL (`Outside`); `.` and empty components are dropped. Unlike
/// `margin_shared::relative_components`, hidden names are allowed: a `Dir` doesn't
/// filter, the sync does.
fn components(rel: &str) -> Result<Vec<&str>, DirError> {
    let bytes = rel.as_bytes();
    if rel.contains('\0')
        || rel.starts_with(['/', '\\'])
        || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
    {
        return Err(DirError::Outside);
    }
    let mut out = Vec::new();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(DirError::Outside),
            name => out.push(name),
        }
    }
    Ok(out)
}

fn io_error(e: std::io::Error) -> DirError {
    match e.kind() {
        ErrorKind::NotFound => DirError::NotFound,
        _ => DirError::Io(e.to_string()),
    }
}

fn entry(name: String, meta: &Metadata) -> Option<DirEntry> {
    let kind = if meta.is_dir() {
        DirKind::Dir
    } else if meta.is_file() {
        DirKind::File
    } else {
        return None;
    };
    Some(DirEntry {
        name,
        kind,
        bytes: if kind == DirKind::File { meta.len() } else { 0 },
        modified: meta.modified().ok().map(DateTime::<Utc>::from),
    })
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Dir for StdDir {
    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError> {
        let (path, meta) = self.resolve(rel)?;
        if !meta.is_dir() {
            return Err(DirError::NotADir);
        }
        let mut out = Vec::new();
        for item in fs::read_dir(&path).map_err(io_error)? {
            let item = item.map_err(io_error)?;
            // `DirEntry::metadata` doesn't follow links: a link is neither a file nor a
            // folder here, so it is left out.
            let Ok(meta) = item.metadata() else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if let Some(e) = entry(item.file_name().to_string_lossy().into_owned(), &meta) {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn stat(&self, rel: &str) -> Result<DirEntry, DirError> {
        let (path, meta) = self.resolve(rel)?;
        entry(name_of(&path), &meta).ok_or(DirError::NotFound)
    }

    fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError> {
        let (path, meta) = self.resolve(rel)?;
        if meta.is_dir() {
            return Err(DirError::IsADir);
        }
        Ok(Box::new(fs::File::open(path).map_err(io_error)?))
    }

    /// macOS marks a file iCloud evicted "dataless" (`SF_DATALESS`).
    fn evicted(&self, rel: &str) -> bool {
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt as _;
            const SF_DATALESS: u32 = 0x4000_0000;
            self.resolve(rel)
                .is_ok_and(|(_, meta)| meta.st_flags() & SF_DATALESS != 0)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = rel;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "margin-dir-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_files_and_folders_with_sizes() {
        let tmp = TempDir::new("list");
        tmp.file("b.md", "hello");
        tmp.file("notes/ch1.md", "one");
        tmp.file("a.pdf", "%PDF");
        let dir = StdDir::new(&tmp.0).unwrap();
        let mut entries = dir.list("").unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<(&str, DirKind, u64)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.bytes))
            .collect();
        assert_eq!(
            names,
            [
                ("a.pdf", DirKind::File, 4),
                ("b.md", DirKind::File, 5),
                ("notes", DirKind::Dir, 0)
            ]
        );
        assert!(entries.iter().all(|e| e.modified.is_some()));
        assert_eq!(dir.list("notes").unwrap()[0].name, "ch1.md");
        assert_eq!(dir.list("b.md").err(), Some(DirError::NotADir));
        assert_eq!(dir.list("missing").err(), Some(DirError::NotFound));
    }

    #[test]
    fn stat_and_open_stream_files() {
        let tmp = TempDir::new("open");
        tmp.file("notes/ch1.md", "chapter one");
        let dir = StdDir::new(&tmp.0).unwrap();
        let stat = dir.stat("notes/ch1.md").unwrap();
        assert_eq!(
            (stat.name.as_str(), stat.kind, stat.bytes),
            ("ch1.md", DirKind::File, 11)
        );
        assert_eq!(dir.stat("notes").unwrap().kind, DirKind::Dir);
        let mut text = String::new();
        dir.open("notes/ch1.md")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "chapter one");
        assert_eq!(dir.open("notes").err(), Some(DirError::IsADir));
        assert_eq!(dir.open("notes/ch2.md").err(), Some(DirError::NotFound));
        assert_eq!(dir.stat("gone.md").err(), Some(DirError::NotFound));
        // Paths that leave the root are refused before anything is read.
        assert_eq!(dir.open("../x").err(), Some(DirError::Outside));
        assert_eq!(dir.stat("/etc/passwd").err(), Some(DirError::Outside));
        assert_eq!(dir.stat("a\0b").err(), Some(DirError::Outside));
        // Hidden names are the sync's business, not the Dir's.
        tmp.file(".hidden.md", "h");
        assert_eq!(dir.stat("./.hidden.md").unwrap().bytes, 1);
        assert!(matches!(
            StdDir::new(tmp.0.join("notes/ch1.md")),
            Err(DirError::NotADir)
        ));
        assert!(matches!(
            StdDir::new(tmp.0.join("nope")),
            Err(DirError::NotFound)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_outside() {
        let tmp = TempDir::new("links");
        let outside = TempDir::new("links-outside");
        let secret = outside.file("secret.md", "not yours");
        tmp.file("real.md", "mine");
        std::os::unix::fs::symlink(&secret, tmp.0.join("link.md")).unwrap();
        std::os::unix::fs::symlink(&outside.0, tmp.0.join("linked-folder")).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("real.md"), tmp.0.join("inside-link.md")).unwrap();
        let dir = StdDir::new(&tmp.0).unwrap();
        let names: Vec<String> = dir.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["real.md"], "links are left out of listings");
        for rel in [
            "link.md",
            "linked-folder",
            "linked-folder/secret.md",
            "inside-link.md",
        ] {
            assert_eq!(dir.stat(rel).err(), Some(DirError::Outside), "{rel}");
            assert_eq!(dir.open(rel).err(), Some(DirError::Outside), "{rel}");
        }
        assert_eq!(dir.list("linked-folder").err(), Some(DirError::Outside));
    }
}
