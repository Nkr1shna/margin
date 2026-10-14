//! The device token lives in the OS keychain through keyring 4.2:
//! - macOS (and other desktops): keyring's `v1` mode, which uses Keychain Services on macOS.
//! - iOS: keyring's `cli` module with the "protected" (Data Protection keychain) store.
//! - Android: keyring's `cli` module with the "android" store (SharedPreferences encrypted
//!   with an Android Keystore key). Dioxus mobile initializes the `ndk-context` the
//!   store needs.
//!
//! One entry per service URL, so switching services never mixes tokens.

/// The Keychain service: the app's bundle identifier, which `MARGIN_APP_ID` changes at build
/// time for apps.yml's dev build, so its tokens are its own.
const SERVICE: &str = match option_env!("MARGIN_APP_ID") {
    Some(id) => id,
    None => "com.krishnanelloore.margin.app",
};

#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod platform {
    pub use keyring::Entry;
    pub use keyring::Error;

    pub fn entry(service: &str, user: &str) -> Result<Entry, Error> {
        Entry::new(service, user)
    }
}

#[cfg(any(target_os = "ios", target_os = "android"))]
mod platform {
    pub use keyring_core::Entry;
    pub use keyring_core::Error;
    use std::sync::OnceLock;

    static STORE: OnceLock<Result<(), String>> = OnceLock::new();

    fn ensure_store() -> Result<(), Error> {
        let result = STORE.get_or_init(|| {
            #[cfg(target_os = "ios")]
            let name = "protected";
            #[cfg(target_os = "android")]
            let name = "android";
            keyring::use_named_store(name).map_err(|e| e.to_string())
        });
        result.clone().map_err(Error::NotSupportedByStore)
    }

    pub fn entry(service: &str, user: &str) -> Result<Entry, Error> {
        ensure_store()?;
        Entry::new(service, user)
    }
}

fn describe(err: &platform::Error) -> String {
    format!("Keychain error: {err}")
}

/// The stored token for `service_url`, if any.
pub fn load_token(service_url: &str) -> Result<Option<String>, String> {
    let entry = platform::entry(SERVICE, service_url).map_err(|e| describe(&e))?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(platform::Error::NoEntry) => Ok(None),
        Err(e) => Err(describe(&e)),
    }
}

pub fn save_token(service_url: &str, token: &str) -> Result<(), String> {
    let entry = platform::entry(SERVICE, service_url).map_err(|e| describe(&e))?;
    entry.set_password(token).map_err(|e| describe(&e))
}

pub fn delete_token(service_url: &str) -> Result<(), String> {
    let entry = platform::entry(SERVICE, service_url).map_err(|e| describe(&e))?;
    match entry.delete_credential() {
        Ok(()) | Err(platform::Error::NoEntry) => Ok(()),
        Err(e) => Err(describe(&e)),
    }
}
