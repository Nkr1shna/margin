//! Where the service is (`~/.config/margin/mcp.toml`) and where the device token lives
//! (the macOS Keychain, service `margin-mcp`, one entry per service URL). No token is
//! ever written to a file.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

use crate::client::normalize_url;

/// The Keychain service the token is stored under.
pub const KEYCHAIN_SERVICE: &str = "margin-mcp";

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

/// Device-token storage. The binary uses `Keychain`; tests use memory.
pub trait TokenStore {
    fn get(&self, url: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, url: &str, token: &str) -> anyhow::Result<()>;
}

/// The login Keychain, through `keyring` (Keychain Services on macOS).
pub struct Keychain;

impl Keychain {
    fn entry(url: &str) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN_SERVICE, url).context("couldn't open the Keychain")
    }
}

impl TokenStore for Keychain {
    fn get(&self, url: &str) -> anyhow::Result<Option<String>> {
        match Self::entry(url)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(
                "couldn't read the Margin token from the Keychain: {e}"
            )),
        }
    }

    fn set(&self, url: &str, token: &str) -> anyhow::Result<()> {
        Self::entry(url)?
            .set_password(token)
            .map_err(|e| anyhow!("couldn't store the Margin token in the Keychain: {e}"))
    }
}

/// The stored token for `url`, or a sentence saying how to get one.
pub fn require_token(store: &dyn TokenStore, url: &str) -> anyhow::Result<String> {
    store
        .get(url)?
        .ok_or_else(|| anyhow!("not signed in to {url}: run `margin-mcp login` first"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
