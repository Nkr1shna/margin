//! `service.toml`: where the service listens, who may sign in, where data lives.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::secrets::{GITHUB_CLIENT_SECRET, SecretStore};

pub const DEFAULT_PUBLIC_URL: &str = "https://margin.krishnanelloore.com";
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";
pub const DEFAULT_ALLOWED_LOGIN: &str = "Nkr1shna";
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
pub const DEFAULT_GITHUB_OAUTH_URL: &str = "https://github.com";
/// What a config without `environment` is (the owner's own Mac, set up before deploys).
pub const DEFAULT_ENVIRONMENT: &str = "prod";
/// The deploy environments (`docs/plan-deploy.md` §1); `GET /api/version` reports it.
pub const ENVIRONMENTS: [&str; 2] = ["dev", "prod"];
/// Where secrets are kept (`docs/plan-linux-service.md` §2.8): the login Keychain, or
/// 0600 files under `data_dir/secrets` (a headless Linux service has no unlocked keyring).
pub const SECRET_STORES: [&str; 2] = ["keychain", "file"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// The URL apps and GitHub reach the service at (no trailing slash).
    #[serde(default = "default_public_url")]
    pub public_url: String,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub github_client_id: String,
    #[serde(default = "default_allowed_login")]
    pub allowed_github_login: String,
    #[serde(default = "default_github_api_url")]
    pub github_api_url: String,
    #[serde(default = "default_github_oauth_url")]
    pub github_oauth_url: String,
    /// `"dev"` or `"prod"`: which deploy this service is (`docs/plan-deploy.md`).
    /// `deploy.sh` and `margin-eval` check it through `GET /api/version`.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// The embedding backend (`margin_embed::Backend` names: `"coreml"`, `"onnx-q8"`,
    /// `"onnx-fp32"`, `"onnx-fp16"`). Default: Core ML on macOS, ONNX 8-bit elsewhere.
    #[serde(default = "default_embedder")]
    pub embedder: String,
    /// `"keychain"` or `"file"` (`SECRET_STORES`). Default: the Keychain on macOS, files
    /// elsewhere.
    #[serde(default = "default_secrets")]
    pub secrets: String,
}

fn default_public_url() -> String {
    DEFAULT_PUBLIC_URL.into()
}
fn default_bind() -> String {
    DEFAULT_BIND.into()
}
fn default_allowed_login() -> String {
    DEFAULT_ALLOWED_LOGIN.into()
}
fn default_github_api_url() -> String {
    DEFAULT_GITHUB_API_URL.into()
}
fn default_github_oauth_url() -> String {
    DEFAULT_GITHUB_OAUTH_URL.into()
}
fn default_environment() -> String {
    DEFAULT_ENVIRONMENT.into()
}
fn default_embedder() -> String {
    margin_embed::Backend::default().name().into()
}
fn default_secrets() -> String {
    if cfg!(target_os = "macos") {
        "keychain".into()
    } else {
        "file".into()
    }
}
fn default_data_dir() -> PathBuf {
    app_support_dir().join("data")
}

/// `~/Library/Application Support/Margin`.
pub fn app_support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library")
        .join("Application Support")
        .join("Margin")
}

/// `MARGIN_CONFIG`, or `~/Library/Application Support/Margin/service.toml`.
pub fn config_path() -> PathBuf {
    match std::env::var_os("MARGIN_CONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => app_support_dir().join("service.toml"),
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_url: default_public_url(),
            bind: default_bind(),
            data_dir: default_data_dir(),
            github_client_id: String::new(),
            allowed_github_login: default_allowed_login(),
            github_api_url: default_github_api_url(),
            github_oauth_url: default_github_oauth_url(),
            environment: default_environment(),
            embedder: default_embedder(),
            secrets: default_secrets(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "couldn't read {} — run `margin-service setup` first",
                path.display()
            )
        })?;
        let mut c: Config =
            toml::from_str(&text).with_context(|| format!("{} isn't valid", path.display()))?;
        c.normalize();
        Ok(c)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("couldn't create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("couldn't write {}", path.display()))?;
        Ok(())
    }

    /// Strips trailing slashes from URLs so paths can be appended.
    pub fn normalize(&mut self) {
        for url in [
            &mut self.public_url,
            &mut self.github_api_url,
            &mut self.github_oauth_url,
        ] {
            let trimmed = url.trim().trim_end_matches('/').to_string();
            *url = trimmed;
        }
        self.github_client_id = self.github_client_id.trim().to_string();
        self.allowed_github_login = self.allowed_github_login.trim().to_string();
        self.environment = self.environment.trim().to_ascii_lowercase();
        self.embedder = self.embedder.trim().to_ascii_lowercase();
        self.secrets = self.secrets.trim().to_ascii_lowercase();
    }

    /// The configured embedding backend; an error for an unknown name or one this binary
    /// wasn't built with (never a silent fallback to another backend).
    pub fn embedder_backend(&self) -> anyhow::Result<margin_embed::Backend> {
        let backend: margin_embed::Backend = self.embedder.parse()?;
        if !backend.is_built() {
            bail!(
                "embedder is \"{backend}\", which this build of margin-service can't run \
                 (use \"{}\")",
                margin_embed::Backend::default()
            );
        }
        Ok(backend)
    }

    pub fn validate_for_serving(&self) -> anyhow::Result<()> {
        if self.github_client_id.is_empty() {
            bail!("github_client_id is empty — run `margin-service setup`");
        }
        if self.allowed_github_login.is_empty() {
            bail!("allowed_github_login is empty — run `margin-service setup`");
        }
        if !ENVIRONMENTS.contains(&self.environment.as_str()) {
            bail!(
                "environment is \"{}\"; it must be \"dev\" or \"prod\" (run `margin-service setup`)",
                self.environment
            );
        }
        self.embedder_backend()?;
        if !SECRET_STORES.contains(&self.secrets.as_str()) {
            bail!(
                "secrets is \"{}\"; it must be \"keychain\" or \"file\"",
                self.secrets
            );
        }
        for (name, url) in [
            ("public_url", &self.public_url),
            ("github_api_url", &self.github_api_url),
            ("github_oauth_url", &self.github_oauth_url),
        ] {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("{name} must start with http:// or https://");
            }
        }
        Ok(())
    }

    pub fn db_dir(&self) -> PathBuf {
        self.data_dir.join("db")
    }

    pub fn videos_dir(&self) -> PathBuf {
        self.data_dir.join("videos")
    }

    /// Uploaded sources, each as `{id}/file`.
    pub fn sources_dir(&self) -> PathBuf {
        self.data_dir.join("sources")
    }

    /// Sources being uploaded in parts, each as `{id}/file` and `{id}/upload.json`.
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }

    /// The downloaded embedding model of the configured backend (each backend has its
    /// own folder; an unknown backend falls back to the default's, and serving refuses
    /// it in `validate_for_serving`).
    pub fn embedding_model_dir(&self) -> PathBuf {
        let backend = self.embedder.parse().unwrap_or_default();
        self.data_dir
            .join("models")
            .join(margin_embed::Backend::dir_name(backend))
    }

    /// The configured backend's model files.
    pub fn model_files(&self) -> margin_embed::ModelFiles {
        let backend = self.embedder.parse().unwrap_or_default();
        margin_embed::ModelFiles::with_backend(self.embedding_model_dir(), backend)
    }
}

/// `margin-service setup`: asks for the environment (dev or prod), the public URL, the
/// GitHub OAuth app's client id and secret, and the allowed GitHub login; stores the secret in the Keychain and writes
/// the config file.
pub fn run_setup(path: &Path, secrets: &dyn SecretStore) -> anyhow::Result<()> {
    let existing = if path.exists() {
        Config::load(path).ok()
    } else {
        None
    };
    let mut config = existing.clone().unwrap_or_default();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();

    println!("Margin service setup — writes {}", path.display());
    println!("Press Return to keep the value in [brackets].\n");

    config.environment = ask_environment(&mut input, &config.environment)?;
    config.public_url = ask(&mut input, "Public URL", &config.public_url)?;
    loop {
        config.github_client_id = ask(
            &mut input,
            "GitHub OAuth client ID",
            &config.github_client_id,
        )?;
        if !config.github_client_id.trim().is_empty() {
            break;
        }
        println!(
            "  The client ID is required (GitHub → Settings → Developer settings → OAuth Apps)."
        );
    }

    let has_secret = secrets.get(GITHUB_CLIENT_SECRET)?.is_some();
    loop {
        let prompt = if has_secret {
            "GitHub OAuth client secret (input hidden; Return keeps the stored one)"
        } else {
            "GitHub OAuth client secret (input hidden)"
        };
        let secret = ask_hidden(&mut input, prompt)?;
        if !secret.is_empty() {
            secrets.set(GITHUB_CLIENT_SECRET, &secret)?;
            println!("  Stored in the Keychain.");
            break;
        }
        if has_secret {
            break;
        }
        println!("  The client secret is required.");
    }

    config.allowed_github_login = ask(
        &mut input,
        "Allowed GitHub login",
        &config.allowed_github_login,
    )?;
    config.normalize();
    config.validate_for_serving()?;
    config.save(path)?;
    println!("\nWrote {}.", path.display());
    println!("Data will be stored in {}.", config.data_dir.display());
    println!(
        "Run `margin-service serve` to start the service on {}.",
        config.bind
    );
    Ok(())
}

/// Asks until the answer is `dev` or `prod` (any case); Return keeps `current`.
fn ask_environment(input: &mut impl BufRead, current: &str) -> anyhow::Result<String> {
    loop {
        let answer = ask(input, "Environment (dev or prod)", current)?
            .trim()
            .to_ascii_lowercase();
        if ENVIRONMENTS.contains(&answer.as_str()) {
            return Ok(answer);
        }
        println!("  Type dev or prod.");
    }
}

fn ask(input: &mut impl BufRead, label: &str, current: &str) -> anyhow::Result<String> {
    if current.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{current}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("setup was cancelled (end of input)");
    }
    let line = line.trim();
    Ok(if line.is_empty() {
        current.to_string()
    } else {
        line.to_string()
    })
}

/// Reads a line with terminal echo turned off (via `stty`) when stdin is a terminal.
fn ask_hidden(input: &mut impl BufRead, label: &str) -> anyhow::Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let echo_off = set_echo(false);
    let mut line = String::new();
    let read = input.read_line(&mut line);
    if echo_off {
        set_echo(true);
        println!();
    }
    if read? == 0 {
        bail!("setup was cancelled (end of input)");
    }
    Ok(line.trim().to_string())
}

fn set_echo(on: bool) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_missing_fields_and_urls_are_normalized() {
        let c: Config =
            toml::from_str("github_client_id = \"abc\"\npublic_url = \"https://x.example/\"\n")
                .unwrap();
        let mut c = c;
        c.normalize();
        assert_eq!(c.public_url, "https://x.example");
        assert_eq!(c.bind, DEFAULT_BIND);
        assert_eq!(c.allowed_github_login, "Nkr1shna");
        assert_eq!(c.github_oauth_url, "https://github.com");
        // A config from before deploys is the owner's own instance: prod.
        assert_eq!(c.environment, "prod");
        c.validate_for_serving().unwrap();
    }

    #[test]
    fn environment_is_dev_or_prod() {
        let mut c: Config =
            toml::from_str("github_client_id = \"abc\"\nenvironment = \" Dev \"\n").unwrap();
        c.normalize();
        assert_eq!(c.environment, "dev");
        c.validate_for_serving().unwrap();
        c.environment = "staging".into();
        let err = c.validate_for_serving().unwrap_err().to_string();
        assert!(err.contains("\"dev\" or \"prod\""), "{err}");
    }

    #[test]
    fn setup_asks_until_the_environment_is_known() {
        let mut input = std::io::Cursor::new(b"staging\nDEV\n".to_vec());
        assert_eq!(ask_environment(&mut input, "prod").unwrap(), "dev");
        let mut input = std::io::Cursor::new(b"\n".to_vec());
        assert_eq!(ask_environment(&mut input, "prod").unwrap(), "prod");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("margin-config-{}", uuid::Uuid::new_v4()));
        let path = dir.join("service.toml");
        let c = Config {
            github_client_id: "id".into(),
            data_dir: dir.join("data"),
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn embedder_and_secrets_default_per_os_and_are_checked() {
        let c: Config = toml::from_str("github_client_id = \"id\"").unwrap();
        let backend = margin_embed::Backend::default();
        assert_eq!(c.embedder, backend.name());
        assert_eq!(c.embedder_backend().unwrap(), backend);
        assert!(c.embedding_model_dir().ends_with(backend.dir_name()));
        assert_eq!(c.model_files().backend, backend);
        assert_eq!(
            c.secrets,
            if cfg!(target_os = "macos") {
                "keychain"
            } else {
                "file"
            }
        );
        c.validate_for_serving().unwrap();
        let mut bad = c.clone();
        bad.embedder = "tensorflow".into();
        assert!(
            bad.validate_for_serving()
                .unwrap_err()
                .to_string()
                .contains("onnx-q8")
        );
        let mut other = c.clone();
        other.embedder = if cfg!(target_os = "macos") {
            "onnx-q8"
        } else {
            "coreml"
        }
        .into();
        // Refused when this binary can't run it (macOS builds the service without `onnx`).
        let built = other.model_files().backend.is_built();
        assert_eq!(other.validate_for_serving().is_ok(), built);
        assert!(
            other
                .model_files()
                .dir
                .ends_with(other.model_files().backend.dir_name())
        );
        let mut secrets = c;
        secrets.secrets = "vault".into();
        assert!(secrets.validate_for_serving().is_err());
    }
}
