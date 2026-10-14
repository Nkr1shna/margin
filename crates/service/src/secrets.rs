//! Secrets live in the macOS Keychain (service `com.krishnanelloore.margin`), behind a
//! trait so tests can use memory instead.

use std::collections::HashMap;
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
}
