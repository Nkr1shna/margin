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
/// What a config without `environment` is (the owner's own instance, set up before
/// deploys).
pub const DEFAULT_ENVIRONMENT: &str = "prod";
/// The deploy environments (`docs/plan-deploy.md` §1); `GET /api/version` reports it.
pub const ENVIRONMENTS: [&str; 2] = ["dev", "prod"];
/// Where secrets are kept (`docs/plan-linux-service.md` §2.8): a 0600 file
/// (`secrets::FileStore`; a headless Linux service has no unlocked keyring).
pub const SECRET_STORES: [&str; 1] = ["file"];
/// The secrets file used when `secrets_file` isn't set: beside the data folder, not in
/// it, so backups of the data folder never copy keys.
pub const SECRETS_FILE: &str = "secrets.toml";

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
    /// The embedding backend (`margin_embed::Backend` names: `"onnx-q8"`, `"onnx-fp32"`,
    /// `"onnx-fp16"`). Default: ONNX 8-bit.
    #[serde(default = "default_embedder")]
    pub embedder: String,
    /// `"file"` (`SECRET_STORES`), the only store.
    #[serde(default = "default_secrets")]
    pub secrets: String,
    /// The secrets file (`secrets::FileStore`, a 0600 TOML file of `name = "value"`
    /// lines). Setting it means the file store whatever `secrets` says: the dev and prod
    /// VMs set it to a file on the persistent share, which the deploy jobs fill from
    /// GitHub Environment secrets. Unset, `SECRETS_FILE` beside `data_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_file: Option<PathBuf>,
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
    "file".into()
}
fn default_data_dir() -> PathBuf {
    app_support_dir().join("data")
}

/// The service's own folder: `/var/lib/margin` (the systemd service's state directory).
/// The data folder is `data` inside it, the secrets file sits beside that.
pub fn app_support_dir() -> PathBuf {
    PathBuf::from(LINUX_STATE_DIR)
}

/// Where the service keeps its data (`app_support_dir`).
pub const LINUX_STATE_DIR: &str = "/var/lib/margin";
/// Where the service's config is (`config_path`).
pub const LINUX_CONFIG_PATH: &str = "/etc/margin/service.toml";

/// `MARGIN_CONFIG`, or `/etc/margin/service.toml`.
pub fn config_path() -> PathBuf {
    match std::env::var_os("MARGIN_CONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => default_config_path(),
    }
}

/// `config_path` without `MARGIN_CONFIG`.
pub fn default_config_path() -> PathBuf {
    PathBuf::from(LINUX_CONFIG_PATH)
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
            secrets_file: None,
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

    /// The configured embedding backend; an error for an unknown name (never a silent
    /// fallback to another backend).
    pub fn embedder_backend(&self) -> anyhow::Result<margin_embed::Backend> {
        Ok(self.embedder.parse()?)
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
        self.secrets_path()?;
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

    /// The secrets file (`secrets_file`, else `SECRETS_FILE` beside `data_dir`); an error
    /// for a `secrets` other than `"file"` without `secrets_file` (the macOS Keychain store
    /// is gone: set `secrets = "file"` and move the keys into the file).
    pub fn secrets_path(&self) -> anyhow::Result<PathBuf> {
        if let Some(file) = &self.secrets_file {
            return Ok(file.clone());
        }
        match self.secrets.as_str() {
            "file" => {
                let beside = match self.data_dir.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => self.data_dir.clone(),
                };
                Ok(beside.join(SECRETS_FILE))
            }
            other => bail!("secrets is \"{other}\"; it must be \"file\""),
        }
    }

    /// The secret store this config names (`secrets::open_store`).
    pub fn secret_store(&self) -> anyhow::Result<std::sync::Arc<dyn SecretStore>> {
        crate::secrets::open_store(self)
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

    /// Where downloaded models are kept: `data_dir/models`.
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
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
/// GitHub OAuth app's client id and secret, and the allowed GitHub login; stores the secret
/// in the secrets file (`secrets_file`, else beside the data folder) and writes the config
/// file.
pub fn run_setup(path: &Path) -> anyhow::Result<()> {
    let existing = if path.exists() {
        Config::load(path).ok()
    } else {
        None
    };
    let mut config = existing.clone().unwrap_or_default();
    let store = config.secret_store()?;
    let secrets: &dyn SecretStore = &*store;
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
            println!("  Stored in {}.", secrets.location());
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
    fn paths_default_to_the_linux_service() {
        let data = default_data_dir();
        assert_eq!(data, Path::new("/var/lib/margin/data"));
        assert_eq!(default_config_path(), Path::new("/etc/margin/service.toml"));
        // The secrets file: /var/lib/margin/secrets.toml, outside the backed-up data.
        let c = Config::default();
        assert_eq!(
            c.secrets_path().unwrap(),
            Path::new("/var/lib/margin/secrets.toml")
        );
    }

    #[test]
    fn embedder_and_secrets_have_defaults_and_are_checked() {
        let c: Config = toml::from_str("github_client_id = \"id\"").unwrap();
        let backend = margin_embed::Backend::default();
        assert_eq!(c.embedder, backend.name());
        assert_eq!(c.embedder_backend().unwrap(), backend);
        assert!(c.embedding_model_dir().ends_with(backend.dir_name()));
        assert_eq!(c.model_files().backend, backend);
        assert_eq!(c.secrets, "file");
        c.validate_for_serving().unwrap();
        // Unknown names are refused, Core ML's (the Mac service's) among them.
        for name in ["tensorflow", "coreml"] {
            let mut bad = c.clone();
            bad.embedder = name.into();
            assert!(
                bad.validate_for_serving()
                    .unwrap_err()
                    .to_string()
                    .contains("onnx-q8")
            );
        }
        let mut other = c.clone();
        other.embedder = "onnx-fp32".into();
        other.validate_for_serving().unwrap();
        assert!(
            other
                .model_files()
                .dir
                .ends_with("embeddinggemma-2-onnx-fp32")
        );
        for name in ["vault", "keychain"] {
            let mut secrets = c.clone();
            secrets.secrets = name.into();
            assert!(secrets.validate_for_serving().is_err());
        }
    }
}
