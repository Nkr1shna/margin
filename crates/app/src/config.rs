//! Local, non-secret app settings: the service URL and the appearance choice, kept in
//! `app.json` in the platform data directory (`~/Library/Application Support/Margin` on
//! macOS and iOS, the app's files directory on Android; `Margin Dev` for the dev build).

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The service this build is pinned to: `MARGIN_DEFAULT_SERVICE_URL` at build time
/// (`apps.yml` sets it per environment, so "Margin" talks to prod and "Margin Dev" to
/// dev, and nothing else can be chosen). `None` for a developer build (`dx serve`,
/// `cargo run`), which asks for the address instead (`docs/plan-deploy.md` §10).
pub const PINNED_SERVICE_URL: Option<&str> = option_env!("MARGIN_DEFAULT_SERVICE_URL");

/// Where a developer build looks first: a service run on this machine
/// (`margin-service serve` binds 127.0.0.1:8787). The Android emulator reaches its host
/// as 10.0.2.2.
pub const LOCAL_SERVICE_URL: &str = if cfg!(target_os = "android") {
    "http://10.0.2.2:8787"
} else {
    "http://localhost:8787"
};

/// The app's name, and its folder in the platform data directory: "Margin", or what
/// `MARGIN_APP_NAME` says at build time ("Margin Dev" for apps.yml's dev build, so it
/// installs and keeps its settings beside the prod app).
pub const APP_NAME: &str = match option_env!("MARGIN_APP_NAME") {
    Some(name) => name,
    None => "Margin",
};

/// `url` without its scheme, for saying where the app connects: "devmargin.example.com".
pub fn service_host(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Appearance::System => Appearance::Light,
            Appearance::Light => Appearance::Dark,
            Appearance::Dark => Appearance::System,
        }
    }

    /// Value for `<html data-theme>`; `None` removes the attribute (follow the system).
    pub fn data_theme(self) -> Option<&'static str> {
        match self {
            Appearance::System => None,
            Appearance::Light => Some("light"),
            Appearance::Dark => Some("dark"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// The service base URL the user confirmed; `None` until first run completes.
    #[serde(default)]
    pub service_url: Option<String>,
    #[serde(default)]
    pub appearance: Appearance,
    /// The sidebar is collapsed (wide windows; narrow ones always use the drawer).
    #[serde(default)]
    pub sidebar_hidden: bool,
    /// Sidebar width in CSS pixels, set by dragging its edge; `None` is the default.
    #[serde(default)]
    pub sidebar_width: Option<u32>,
    /// Page zoom in percent (View > Zoom, Settings > General > Text size).
    #[serde(default = "default_zoom")]
    pub zoom: u32,
}

fn default_zoom() -> u32 {
    crate::zoom::DEFAULT
}

impl LocalConfig {
    /// The service this app talks to: the build's pinned service; in a developer build,
    /// the address confirmed at sign-in, or the local service until one is. A pinned
    /// build ignores any address an older install saved (its token for that address stays
    /// in the keychain, unused) and signs in to its own service.
    pub fn service_url(&self) -> String {
        PINNED_SERVICE_URL
            .map(String::from)
            .or_else(|| self.service_url.clone())
            .unwrap_or_else(|| LOCAL_SERVICE_URL.to_string())
    }
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            service_url: None,
            appearance: Appearance::System,
            sidebar_hidden: false,
            sidebar_width: None,
            zoom: default_zoom(),
        }
    }
}

/// Normalize what the user typed: trim, add `https://` when no scheme is given, drop
/// trailing slashes. Returns an error message for anything that is not http(s).
pub fn normalize_service_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter the address of your Margin service.".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = with_scheme.trim_end_matches('/').to_string();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| "The address must start with https:// or http://.".to_string())?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("That doesn't look like a web address.".into());
    }
    Ok(url)
}

pub fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    {
        android_dir("getFilesDir").map(|d| d.join(APP_NAME))
    }
    #[cfg(not(target_os = "android"))]
    {
        dirs::data_dir().map(|d| d.join(APP_NAME))
    }
}

/// The app's cache directory on Android (`Context.getCacheDir()`): files the system may
/// clear when space runs low.
#[cfg(target_os = "android")]
pub fn cache_dir() -> Option<PathBuf> {
    android_dir("getCacheDir")
}

/// A directory of the running Android app (`Context.getFilesDir()`, `getCacheDir()`),
/// through the JNI context that Dioxus mobile publishes via `ndk-context`.
#[cfg(target_os = "android")]
fn android_dir(getter: &str) -> Option<PathBuf> {
    use jni::objects::{JObject, JString};
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context hands out the process JavaVM and the application Context
    // that the Android activity glue registered; both outlive the app.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let dir = env
        .call_method(&context, getter, "()Ljava/io/File;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = JString::from(path);
    let path: String = env.get_string(&path).ok()?.into();
    Some(PathBuf::from(path))
}

pub fn config_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("app.json"))
}

pub fn load() -> LocalConfig {
    config_path().map(|p| load_from(&p)).unwrap_or_default()
}

/// The settings at `path`; defaults when there are none, or when the file was damaged
/// (it is then set aside, see [`read_json`]).
pub fn load_from(path: &Path) -> LocalConfig {
    read_json(path).unwrap_or_default()
}

pub fn save(config: &LocalConfig) -> Result<(), String> {
    let path = config_path().ok_or("This platform has no app data directory.")?;
    save_to(&path, config)
}

pub fn save_to(path: &Path, config: &LocalConfig) -> Result<(), String> {
    write_json(path, config)
}

/// Where a failure to keep something on this device goes: a file the app couldn't write
/// (settings, unsaved note edits), or a damaged one it set aside. Stderr for now: the one
/// place to route these to the app's log.
pub fn report_save_failure(message: &str) {
    tracing::warn!("{message}");
}

/// The JSON file at `path`, or `None` when there is none. A file that can't be read or
/// parsed is renamed aside ([`corrupt_path`]) and reported, so the next save starts
/// afresh without overwriting what was there.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let parsed = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => Err(e.to_string()),
    };
    let why = match parsed {
        Ok(value) => return Some(value),
        Err(why) => why,
    };
    let aside = corrupt_path(path, chrono::Utc::now());
    let message = match std::fs::rename(path, &aside) {
        Ok(()) => format!(
            "couldn't read {} ({why}); moved it to {} and started afresh",
            path.display(),
            aside.display()
        ),
        Err(e) => format!(
            "couldn't read {} ({why}) nor move it aside ({e}); the next save replaces it",
            path.display()
        ),
    };
    report_save_failure(&message);
    None
}

/// Where a damaged `path` is kept, beside it: `drafts.json.corrupt-20261015T093000.123Z`.
pub fn corrupt_path(path: &Path, at: chrono::DateTime<chrono::Utc>) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        "{name}.corrupt-{}",
        at.format("%Y%m%dT%H%M%S%.3fZ")
    ))
}

/// Writes `value` to `path` as JSON, through a temporary file so a crash never leaves
/// half of it.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("Couldn't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_service_url(" margin.example.com/ ").unwrap(),
            "https://margin.example.com"
        );
        assert_eq!(
            normalize_service_url("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert!(normalize_service_url("").is_err());
        assert!(normalize_service_url("ftp://x").is_err());
        assert!(normalize_service_url("https://").is_err());
    }

    #[test]
    fn the_service_is_the_pinned_one_or_what_was_saved() {
        let saved = LocalConfig {
            service_url: Some("http://127.0.0.1:8787".into()),
            ..LocalConfig::default()
        };
        match PINNED_SERVICE_URL {
            Some(pinned) => {
                // A pinned address is already normalized and wins over a saved one.
                assert_eq!(normalize_service_url(pinned).unwrap(), pinned);
                assert_eq!(LocalConfig::default().service_url(), pinned);
                assert_eq!(saved.service_url(), pinned);
            }
            None => {
                assert_eq!(LocalConfig::default().service_url(), LOCAL_SERVICE_URL);
                assert_eq!(saved.service_url(), "http://127.0.0.1:8787");
            }
        }
        assert_eq!(
            normalize_service_url(LOCAL_SERVICE_URL).unwrap(),
            LOCAL_SERVICE_URL
        );
        assert_eq!(
            service_host("https://devmargin.example.com"),
            "devmargin.example.com"
        );
        assert_eq!(service_host("http://localhost:8787"), "localhost:8787");
    }

    #[test]
    fn round_trips_config_file() {
        let dir = std::env::temp_dir().join(format!("margin-app-test-{}", std::process::id()));
        let path = dir.join("app.json");
        let config = LocalConfig {
            service_url: Some("https://m.example".into()),
            appearance: Appearance::Dark,
            sidebar_hidden: true,
            sidebar_width: Some(300),
            zoom: 130,
        };
        save_to(&path, &config).unwrap();
        assert_eq!(load_from(&path), config);
        std::fs::remove_dir_all(&dir).unwrap();
        // Files from before the zoom setting load at 100%.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"service_url":"https://m.example"}"#).unwrap();
        assert_eq!(load_from(&path).zoom, 100);
        std::fs::remove_dir_all(&dir).unwrap();
        // Missing file -> defaults.
        assert_eq!(load_from(&path), LocalConfig::default());
    }

    #[test]
    fn a_damaged_settings_file_is_set_aside_not_overwritten() {
        let dir =
            std::env::temp_dir().join(format!("margin-config-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("app.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_from(&path), LocalConfig::default());
        assert!(!path.exists());
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let aside = names(&dir);
        assert_eq!(aside.len(), 1);
        assert!(aside[0].starts_with("app.json.corrupt-"), "{aside:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join(&aside[0])).unwrap(),
            "{not json"
        );
        // The next save writes a fresh file beside the kept one.
        let saved = LocalConfig {
            zoom: 120,
            ..LocalConfig::default()
        };
        save_to(&path, &saved).unwrap();
        assert_eq!(load_from(&path), saved);
        assert_eq!(names(&dir), vec!["app.json".to_string(), aside[0].clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_files_are_named_for_when_they_were_found() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-15T09:30:00.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            corrupt_path(Path::new("/d/drafts.json"), at),
            Path::new("/d/drafts.json.corrupt-20261015T093000.123Z")
        );
    }

    #[test]
    fn appearance_cycles() {
        assert_eq!(Appearance::System.next(), Appearance::Light);
        assert_eq!(Appearance::Dark.next(), Appearance::System);
        assert_eq!(Appearance::System.data_theme(), None);
    }
}
