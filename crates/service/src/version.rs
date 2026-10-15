//! `GET /api/version`: which build this is and which deploy it serves
//! (`docs/plan-deploy.md` §4, §8). Public like `/health`, so `deploy.sh` can check a
//! new build through the tunnel without a device token. It carries no library data:
//! the build, the environment, when the process started, and its memory, because the
//! dev VM has 4 GB for the embedding model, SurrealDB and the service together.

use std::sync::OnceLock;

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
pub use margin_shared::VersionInfo;

use crate::state::AppState;

/// The commit the binary was built from: `MARGIN_BUILD_SHA` at compile time
/// (`build-service.yml` sets it), `"unknown"` for a local build.
pub const BUILD_SHA: &str = match option_env!("MARGIN_BUILD_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// The build's semantic version: `MARGIN_VERSION` at compile time (`build-service.yml`
/// computes it with `scripts/version.sh at`, docs/versioning.md), Cargo.toml's version for
/// a local build.
pub const VERSION: &str = match option_env!("MARGIN_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The lowest app `API_VERSION` this service serves. Raise it, with `API_VERSION`, when an
/// app built against an older contract would misread this service's answers.
pub const MIN_CLIENT_API: u32 = 1;

/// Below this much physical memory the service warns at start: the embedding model
/// (~1.4 GB loaded), SurrealDB's caches and a lesson job don't fit with room to spare.
pub const LOW_MEMORY_BYTES: u64 = 6 * 1024 * 1024 * 1024;

static STARTED_AT: OnceLock<DateTime<Utc>> = OnceLock::new();

/// Records the process start; the first call wins. `margin-service serve` calls it
/// first thing, and the router calls it too so tests and embedders get a time.
pub fn mark_started() -> DateTime<Utc> {
    *STARTED_AT.get_or_init(Utc::now)
}

pub fn info(environment: &str) -> VersionInfo {
    VersionInfo {
        version: VERSION.to_string(),
        sha: BUILD_SHA.to_string(),
        environment: environment.to_string(),
        started_at: mark_started(),
        rss_bytes: resident_bytes(),
        physical_memory_bytes: physical_memory_bytes(),
        alerts_active: 0,
        alerts_since: None,
        api_version: margin_shared::API_VERSION,
        min_client_api: MIN_CLIENT_API,
        schema_version: crate::db::SCHEMA_VERSION,
    }
}

pub async fn get_version(State(state): State<AppState>) -> Json<VersionInfo> {
    let mut v = info(&state.config.environment);
    (v.alerts_active, v.alerts_since) = state
        .alerts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .summary();
    Json(v)
}

/// The start-up warning for a machine with less than `LOW_MEMORY_BYTES`.
pub fn memory_warning(physical: Option<u64>) -> Option<String> {
    let bytes = physical?;
    (bytes < LOW_MEMORY_BYTES).then(|| {
        format!(
            "this machine has {:.1} GB of memory: the embedding model (~1.4 GB), SurrealDB and \
             lesson jobs need about 6 GB to run without swapping; watch rss_bytes in \
             GET /api/version",
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        )
    })
}

/// This process's resident set size, from `VmRSS` in `/proc/self/status` (`None` off
/// Linux).
pub fn resident_bytes() -> Option<u64> {
    proc_kib(
        &std::fs::read_to_string("/proc/self/status").ok()?,
        "VmRSS:",
    )
}

/// The `kB` value on the `/proc` line starting with `key`, in bytes.
fn proc_kib(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with(key))?;
    let mut parts = line[key.len()..].split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") | None => n.checked_mul(1024),
        _ => None,
    }
}

/// `MemTotal` from `/proc/meminfo`: the memory the guest sees (`None` off Linux).
pub fn physical_memory_bytes() -> Option<u64> {
    proc_kib(&std::fs::read_to_string("/proc/meminfo").ok()?, "MemTotal:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_reports_the_build_and_environment() {
        let first = mark_started();
        let v = info("dev");
        assert_eq!(v.environment, "dev");
        assert_eq!(v.version, VERSION);
        assert_eq!(
            (v.api_version, v.min_client_api, v.schema_version),
            (
                margin_shared::API_VERSION,
                MIN_CLIENT_API,
                crate::db::SCHEMA_VERSION
            )
        );
        assert_eq!(v.sha, BUILD_SHA);
        assert_eq!(v.started_at, first);
        // The JSON shape deploy.sh and margin-eval read.
        let json = serde_json::to_value(&v).unwrap();
        for key in [
            "version",
            "sha",
            "environment",
            "started_at",
            "rss_bytes",
            "alerts_active",
            "api_version",
            "min_client_api",
            "schema_version",
        ] {
            assert!(json.get(key).is_some(), "{key} missing: {json}");
        }
    }

    #[test]
    fn version_from_before_alerts_parses() {
        let v: VersionInfo = serde_json::from_value(serde_json::json!({
            "version": "0.1.0", "sha": "abc", "environment": "dev",
            "started_at": "2026-10-14T10:00:00Z"
        }))
        .unwrap();
        assert_eq!((v.alerts_active, v.alerts_since), (0, None));
        assert_eq!(
            (v.api_version, v.min_client_api, v.schema_version),
            (0, 0, 0)
        );
        assert!(!v.app_too_old());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_memory_on_linux() {
        let rss = resident_bytes().expect("VmRSS");
        let total = physical_memory_bytes().expect("MemTotal");
        assert!(rss > 0 && rss < total, "rss {rss}, total {total}");
    }

    #[test]
    fn parses_proc_lines() {
        let status = "Name:\tmargin\nVmRSS:\t  123456 kB\nThreads:\t9\n";
        assert_eq!(proc_kib(status, "VmRSS:"), Some(123_456 * 1024));
        assert_eq!(
            proc_kib("MemTotal:       8123456 kB\n", "MemTotal:"),
            Some(8_123_456 * 1024)
        );
        assert_eq!(proc_kib(status, "VmSwap:"), None);
        assert_eq!(proc_kib("VmRSS: x kB\n", "VmRSS:"), None);
    }

    #[test]
    fn warns_below_six_gigabytes() {
        let gb = 1024 * 1024 * 1024;
        let w = memory_warning(Some(4 * gb)).expect("4 GB warns");
        assert!(w.contains("4.0 GB"), "{w}");
        assert!(memory_warning(Some(8 * gb)).is_none());
        assert!(memory_warning(Some(6 * gb)).is_none());
        assert!(memory_warning(None).is_none());
    }
}
