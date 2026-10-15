//! Where the service is (`~/.config/margin/mcp.toml`) and where the tokens live: the
//! system secret store (`keyring`; the login Keychain on a Mac), service `margin-mcp`,
//! one entry per service URL for the device token (`margin-mcp login`) and one under
//! `<url>#agent` for an agent token (`margin-mcp token`). The agent token is used first
//! when both exist. No token is ever written to a file or printed.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

use margin_shared::AGENT_TOKEN_PREFIX;

use crate::client::normalize_url;

/// The secret-store service the tokens are stored under.
pub const KEYCHAIN_SERVICE: &str = "margin-mcp";

/// The longest token `margin-mcp token` accepts (the service mints far shorter ones).
pub const AGENT_TOKEN_MAX: usize = 512;

#[derive(Debug, Deserialize)]
struct File {
    url: String,
}

/// `~/.config/margin/mcp.toml` (`$XDG_CONFIG_HOME/margin/mcp.toml` when that is set).
pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("margin/mcp.toml"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME isn't set"))?;
    Ok(PathBuf::from(home).join(".config/margin/mcp.toml"))
}

/// The service URL: `--url` when given, else the config file's `url`.
pub fn service_url(explicit: Option<&str>, config: &Path) -> anyhow::Result<String> {
    if let Some(url) = explicit {
        return normalize_url(url);
    }
    let text = std::fs::read_to_string(config).with_context(|| {
        format!(
            "no service URL: pass --url or create {} with a line like\n  url = \"https://margin.krishnanelloore.com\"",
            config.display()
        )
    })?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("{} doesn't parse", config.display()))?;
    normalize_url(&file.url)
}

/// Token storage, by entry key (`url` for the device token, `agent_key(url)` for the
/// agent token). The binary uses `Keychain`; tests use memory.
pub trait TokenStore {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, key: &str, token: &str) -> anyhow::Result<()>;
    /// Removes the entry; `false` when there was none.
    fn delete(&self, key: &str) -> anyhow::Result<bool>;
}

/// The system secret store through `keyring` (the login Keychain on a Mac): a developer
/// convenience, since margin-mcp runs on the developer's machine, never on the service.
pub struct Keychain;

impl Keychain {
    fn entry(key: &str) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN_SERVICE, key).context("couldn't open the secret store")
    }
}

impl TokenStore for Keychain {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        match Self::entry(key)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(
                "couldn't read the Margin token from the secret store: {e}"
            )),
        }
    }

    fn set(&self, key: &str, token: &str) -> anyhow::Result<()> {
        Self::entry(key)?
            .set_password(token)
            .map_err(|e| anyhow!("couldn't store the Margin token in the secret store: {e}"))
    }

    fn delete(&self, key: &str) -> anyhow::Result<bool> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(anyhow!(
                "couldn't remove the Margin token from the secret store: {e}"
            )),
        }
    }
}

/// The entry key of the agent token for `url` (the device token's key is `url` itself).
pub fn agent_key(url: &str) -> String {
    format!("{url}#agent")
}

/// Which kind of token a client signs in with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An `mga_` agent token (`margin-mcp token`): its agent's ceiling, context routes only.
    Agent,
    /// A device token (`margin-mcp login`): the person's whole access.
    Device,
}

/// The token to use for `url`: the agent token when one is stored, else the device token.
pub fn stored_token(
    store: &dyn TokenStore,
    url: &str,
) -> anyhow::Result<Option<(String, TokenKind)>> {
    if let Some(token) = store.get(&agent_key(url))? {
        return Ok(Some((token, TokenKind::Agent)));
    }
    Ok(store.get(url)?.map(|t| (t, TokenKind::Device)))
}

/// The stored token for `url` (the agent token first), or a sentence saying how to get one.
pub fn require_token(store: &dyn TokenStore, url: &str) -> anyhow::Result<String> {
    stored_token(store, url)?.map(|(t, _)| t).ok_or_else(|| {
        anyhow!(
            "not signed in to {url}: create an agent in Margin's Settings → Organisation → Agents, \
             mint its token and run `margin-mcp token <token>` (or `margin-mcp login` for a device sign-in)"
        )
    })
}

/// Checks the shape of an agent token without ever echoing it: `mga_` and then
/// printable, non-space ASCII. Surrounding whitespace (a pasted newline) is dropped.
pub fn clean_agent_token(raw: &str) -> anyhow::Result<String> {
    let token = raw.trim();
    let Some(rest) = token.strip_prefix(AGENT_TOKEN_PREFIX) else {
        anyhow::bail!(
            "that isn't an agent token: agent tokens start with {AGENT_TOKEN_PREFIX} \
             (mint one in Settings → Organisation → Agents)"
        );
    };
    if rest.is_empty()
        || token.len() > AGENT_TOKEN_MAX
        || !rest.bytes().all(|b| b.is_ascii_graphic())
    {
        anyhow::bail!(
            "that agent token is malformed: copy it again from Settings → Organisation → Agents"
        );
    }
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>);

    impl TokenStore for Memory {
        fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, token: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().insert(key.into(), token.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().remove(key).is_some())
        }
    }

    #[test]
    fn the_agent_token_is_preferred_over_the_device_token() {
        let store = Memory::default();
        let url = "https://margin.example.com";
        assert!(stored_token(&store, url).unwrap().is_none());
        let err = require_token(&store, url).unwrap_err().to_string();
        assert!(err.contains("margin-mcp token"), "{err}");
        store.set(url, "device").unwrap();
        assert_eq!(
            stored_token(&store, url).unwrap(),
            Some(("device".into(), TokenKind::Device))
        );
        store.set(&agent_key(url), "mga_x").unwrap();
        assert_eq!(agent_key(url), "https://margin.example.com#agent");
        assert_eq!(
            stored_token(&store, url).unwrap(),
            Some(("mga_x".into(), TokenKind::Agent))
        );
        assert_eq!(require_token(&store, url).unwrap(), "mga_x");
        // Another service's tokens are its own.
        assert!(
            stored_token(&store, "https://other.example.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn agent_tokens_are_checked_without_being_echoed() {
        assert_eq!(clean_agent_token("  mga_abc123\n").unwrap(), "mga_abc123");
        for bad in ["abc123", "mga_", "mga_a b", "mga_\u{e9}", "MGA_abc"] {
            let err = clean_agent_token(bad).unwrap_err().to_string();
            assert!(bad.len() < 5 || !err.contains(bad), "{err}");
        }
        assert!(clean_agent_token(&format!("mga_{}", "a".repeat(AGENT_TOKEN_MAX))).is_err());
    }

    #[test]
    fn the_url_comes_from_the_flag_or_the_file() {
        let dir = std::env::temp_dir().join(format!("margin-mcp-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.toml");
        assert!(
            service_url(None, &path)
                .unwrap_err()
                .to_string()
                .contains("--url")
        );
        std::fs::write(&path, "url = \"https://margin.example.com/\"\n").unwrap();
        assert_eq!(
            service_url(None, &path).unwrap(),
            "https://margin.example.com"
        );
        assert_eq!(
            service_url(Some("http://127.0.0.1:1"), &path).unwrap(),
            "http://127.0.0.1:1"
        );
        std::fs::write(&path, "url = 3").unwrap();
        assert!(service_url(None, &path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
