//! Secrets live in a 0600 file (`FileStore`, `docs/plan-linux-service.md` §2.8): a
//! headless Linux service has no unlocked keyring (the Secret Service needs a desktop
//! session and the kernel keyring forgets on reboot). The dev and prod VMs set
//! `secrets_file` to a file on the persistent share, written by the deploy jobs from
//! GitHub Environment secrets; otherwise it sits beside the data folder. `open_store`
//! opens it from `service.toml`. Behind a trait so tests can use memory instead.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;

pub const GITHUB_CLIENT_SECRET: &str = "github-client-secret";
pub const OPENAI_API_KEY: &str = "openai-api-key";
/// The Claude provider's key (`docs/agent-providers-brief.md` D9).
pub const ANTHROPIC_API_KEY: &str = "anthropic-api-key";
/// The ChatGPT sign-in as JSON: `client_id`, `ext_agent_host_id`, `access_token`,
/// `refresh_token`, `expires_at`, `scopes`, `sub`, `email`. Never leaves the service.
pub const CHATGPT_TOKENS: &str = "chatgpt-tokens";
pub const ELEVENLABS_API_KEY: &str = "elevenlabs-api-key";
pub const MEDIA_SIGNING_KEY: &str = "media-signing-key";

/// Blocking key-value secret storage. Calls are short (a small file read or write).
pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Deleting a missing secret is not an error.
    fn delete(&self, name: &str) -> anyhow::Result<()>;
    /// Where the secrets are kept, for messages ("Stored in …").
    fn location(&self) -> String {
        "the service's secret store".into()
    }
}

/// The `FileStore` at `Config::secrets_path`.
pub fn open_store(config: &crate::config::Config) -> anyhow::Result<Arc<dyn SecretStore>> {
    Ok(Arc::new(FileStore::new(config.secrets_path()?)))
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

    /// What's wrong when group or others can read the file (`None` when only its owner
    /// can, or it doesn't exist). Logged, not refused: a VM's shared folder may show
    /// other modes than the host wrote (the deploy jobs write 0600 on the host).
    pub fn privacy_warning(&self) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&self.path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then(|| {
            format!(
                "the secrets file {} can be read by other users (mode {mode:o}); run `chmod 600 {}`",
                self.path.display(),
                self.path.display()
            )
        })
    }

    fn read(&self) -> anyhow::Result<BTreeMap<String, String>> {
        if let Some(warning) = self.privacy_warning() {
            tracing::warn!("{warning}");
        }
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{} isn't a valid secrets file", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("couldn't read {}", self.path.display())),
        }
    }

    fn write(&self, map: &BTreeMap<String, String>) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        // A folder made here is the owner's only; an existing one keeps its mode.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
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

    fn location(&self) -> String {
        self.path.display().to_string()
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
    fn file_store_warns_about_a_file_others_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("margin-secrets-{}", uuid::Uuid::new_v4()));
        let path = dir.join("new").join("secrets.toml");
        let s = FileStore::new(&path);
        s.set(OPENAI_API_KEY, "sk-1").unwrap();
        // The folder it made is the owner's only.
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(s.location(), path.display().to_string());
        assert_eq!(s.privacy_warning(), None);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let warning = s.privacy_warning().unwrap();
        assert!(warning.contains("chmod 600"), "{warning}");
        // Still read (a shared folder may show other modes than the host wrote), and a
        // write puts it back to 0600.
        assert_eq!(s.get(OPENAI_API_KEY).unwrap().as_deref(), Some("sk-1"));
        s.set(OPENAI_API_KEY, "sk-2").unwrap();
        assert_eq!(s.privacy_warning(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_config_picks_the_store() {
        let dir = std::env::temp_dir().join(format!("margin-secrets-{}", uuid::Uuid::new_v4()));
        let mut c = crate::config::Config {
            data_dir: dir.join("data"),
            secrets: "file".into(),
            secrets_file: None,
            ..Default::default()
        };
        // Beside the data folder, so a backup of the data folder never copies keys.
        let store = open_store(&c).unwrap();
        assert_eq!(
            store.location(),
            dir.join(crate::config::SECRETS_FILE).display().to_string()
        );
        assert!(!c.secrets_path().unwrap().starts_with(&c.data_dir));
        store.set(GITHUB_CLIENT_SECRET, "s").unwrap();
        assert!(dir.join(crate::config::SECRETS_FILE).is_file());
        // An explicit file wins, whatever `secrets` says (the deploy VMs set only it).
        c.secrets = "keychain".into();
        c.secrets_file = Some(dir.join("share/secrets.toml"));
        assert_eq!(
            open_store(&c).unwrap().location(),
            dir.join("share/secrets.toml").display().to_string()
        );
        // Without one, the Keychain (gone with the Mac service) is refused, not ignored.
        c.secrets_file = None;
        assert!(open_store(&c).is_err());
        c.secrets = "vault".into();
        assert!(open_store(&c).is_err());
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
