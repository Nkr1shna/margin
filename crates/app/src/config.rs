//! Local, non-secret app settings: the service URL and the appearance choice, kept in
//! `app.json` in the platform data directory (`~/Library/Application Support/Margin` on
//! macOS and iOS, the app's files directory on Android).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVICE_URL: &str = "https://margin.krishnanelloore.com";

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
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            service_url: None,
            appearance: Appearance::System,
            sidebar_hidden: false,
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
        android_files_dir().map(|d| d.join("Margin"))
    }
    #[cfg(not(target_os = "android"))]
    {
        dirs::data_dir().map(|d| d.join("Margin"))
    }
}

/// `Context.getFilesDir()` of the running Android app, through the JNI context that
/// Dioxus mobile publishes via `ndk-context`.
#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    use jni::objects::{JObject, JString};
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context hands out the process JavaVM and the application Context
    // that the Android activity glue registered; both outlive the app.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let dir = env
        .call_method(&context, "getFilesDir", "()Ljava/io/File;", &[])
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

pub fn load_from(path: &Path) -> LocalConfig {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(config: &LocalConfig) -> Result<(), String> {
    let path = config_path().ok_or("This platform has no app data directory.")?;
    save_to(&path, config)
}

pub fn save_to(path: &Path, config: &LocalConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
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
    fn round_trips_config_file() {
        let dir = std::env::temp_dir().join(format!("margin-app-test-{}", std::process::id()));
        let path = dir.join("app.json");
        let config = LocalConfig {
            service_url: Some("https://m.example".into()),
            appearance: Appearance::Dark,
            sidebar_hidden: true,
        };
        save_to(&path, &config).unwrap();
        assert_eq!(load_from(&path), config);
        std::fs::remove_dir_all(&dir).unwrap();
        // Missing file -> defaults.
        assert_eq!(load_from(&path), LocalConfig::default());
    }

    #[test]
    fn appearance_cycles() {
        assert_eq!(Appearance::System.next(), Appearance::Light);
        assert_eq!(Appearance::Dark.next(), Appearance::System);
        assert_eq!(Appearance::System.data_theme(), None);
    }
}
