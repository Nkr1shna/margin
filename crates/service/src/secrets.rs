//! Secrets live in the macOS Keychain (service `com.krishnanelloore.margin`), or, when
//! `service.toml` sets `secrets_file`, in that file (the dev and prod VMs: Orchard
//! re-clones a VM on every restart, which empties its Keychain, so their secrets sit on
//! the persistent share, written by the deploy jobs from GitHub Environment secrets).
//! Behind a trait so tests can use memory instead.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

pub const KEYCHAIN_SERVICE: &str = "com.krishnanelloore.margin";
pub const GITHUB_CLIENT_SECRET: &str = "github-client-secret";
pub const OPENAI_API_KEY: &str = "openai-api-key";
pub const MEDIA_SIGNING_KEY: &str = "media-signing-key";

/// Blocking key-value secret storage. Calls are short; the Keychain store is called
/// from async code through `tokio::task::spawn_blocking` where it matters.
pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Deleting a missing secret is not an error.
    fn delete(&self, name: &str) -> anyhow::Result<()>;
}

/// The login Keychain, through the `keyring` crate (v4, `v1` feature: Apple's native
/// keychain store on macOS).
pub struct KeychainStore;

impl KeychainStore {
    fn entry(name: &str) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN_SERVICE, name).context("couldn't open the Keychain")
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        match Self::entry(name)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "couldn't read {name} from the Keychain: {e}"
            )),
        }
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        Self::entry(name)?
            .set_password(value)
            .map_err(|e| anyhow::anyhow!("couldn't store {name} in the Keychain: {e}"))
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "couldn't delete {name} from the Keychain: {e}"
            )),
        }
    }
}

/// A TOML file of `name = "value"` lines, readable only by its owner (0600). Read on
/// every `get`, so a deploy that rewrites the file is picked up without a restart.
/// Writes replace the file atomically.
pub struct FileStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> anyhow::Result<BTreeMap<String, String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{} isn't a valid secrets file", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("couldn't read {}", self.path.display())),
        }
    }

    fn write(&self, map: &BTreeMap<String, String>) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .with_context(|| format!("couldn't create {}", dir.display()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let text = toml::to_string(map)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("couldn't write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("couldn't replace {}", self.path.display()))
    }
}

impl SecretStore for FileStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.read()?.remove(name).filter(|v| !v.is_empty()))
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let mut map = self.read()?;
        map.insert(name.into(), value.into());
        self.write(&map)
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let mut map = self.read()?;
        if map.remove(name).is_some() {
            self.write(&map)?;
        }
        Ok(())
    }
}

/// In-memory secrets, for tests.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(self, name: &str, value: &str) -> Self {
        self.map.lock().unwrap().insert(name.into(), value.into());
        self
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.map.lock().unwrap().get(name).cloned())
    }
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        self.map.lock().unwrap().insert(name.into(), value.into());
        Ok(())
    }
    fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.map.lock().unwrap().remove(name);
        Ok(())
    }
}

/// Reads the media signing key, generating and storing 32 random bytes on first run.
pub fn media_signing_key(store: &dyn SecretStore) -> anyhow::Result<Vec<u8>> {
    if let Some(hex_key) = store.get(MEDIA_SIGNING_KEY)? {
        if let Ok(bytes) = hex::decode(hex_key.trim())
            && bytes.len() >= 32
        {
            return Ok(bytes);
        }
        tracing::warn!("the stored media signing key is malformed; generating a new one");
    }
    let mut key = [0u8; 32];
    rand::fill(&mut key[..]);
    store.set(MEDIA_SIGNING_KEY, &hex::encode(key))?;
    Ok(key.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_key_is_generated_once() {
        let s = MemoryStore::new();
        let a = media_signing_key(&s).unwrap();
        let b = media_signing_key(&s).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
    }

    #[test]
    fn file_store_round_trips_privately_and_keeps_other_entries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("margin-secrets-{}", uuid::Uuid::new_v4()));
        let path = dir.join("secrets.toml");
        let s = FileStore::new(&path);
        assert_eq!(s.get(GITHUB_CLIENT_SECRET).unwrap(), None);
        s.set(GITHUB_CLIENT_SECRET, "abc \"quoted\" \\ value")
            .unwrap();
        let key = media_signing_key(&s).unwrap();
        assert_eq!(
            s.get(GITHUB_CLIENT_SECRET).unwrap().as_deref(),
            Some("abc \"quoted\" \\ value")
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A second store on the same file (the next start) sees the same media key.
        assert_eq!(media_signing_key(&FileStore::new(&path)).unwrap(), key);
        s.delete(GITHUB_CLIENT_SECRET).unwrap();
        s.delete(GITHUB_CLIENT_SECRET).unwrap();
        assert_eq!(s.get(GITHUB_CLIENT_SECRET).unwrap(), None);
        assert!(s.get(MEDIA_SIGNING_KEY).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_store_reads_what_the_deploy_script_writes() {
        // deploy/scripts/common.sh write_secret: `name = "value"` with \ and " escaped.
        let dir = std::env::temp_dir().join(format!("margin-secrets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(
            &path,
            "github-client-secret = \"s3cr\\\"et\\\\x\"\nopenai-api-key = \"sk-1\"\n",
        )
        .unwrap();
        let s = FileStore::new(&path);
        assert_eq!(
            s.get(GITHUB_CLIENT_SECRET).unwrap().as_deref(),
            Some("s3cr\"et\\x")
        );
        assert_eq!(s.get(OPENAI_API_KEY).unwrap().as_deref(), Some("sk-1"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
