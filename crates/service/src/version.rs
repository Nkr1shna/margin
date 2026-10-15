//! `GET /api/version`: which build this is and which deploy it serves
//! (`docs/plan-deploy.md` §4, §8). Public like `/health`, so `deploy.sh` can check a
//! new build through the tunnel without a device token. It carries no library data:
//! the build, the environment, when the process started, and its memory, because the
//! dev VM has 4 GB for the embedding model, SurrealDB and the service together.

use std::sync::OnceLock;

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// The commit the binary was built from: `MARGIN_BUILD_SHA` at compile time
/// (`build-service.yml` sets it), `"unknown"` for a local build.
pub const BUILD_SHA: &str = match option_env!("MARGIN_BUILD_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// Below this much physical memory the service warns at start: the embedding model
/// (~1.4 GB loaded), SurrealDB's caches and a lesson job don't fit with room to spare.
pub const LOW_MEMORY_BYTES: u64 = 6 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The crate version (`CARGO_PKG_VERSION`).
    pub version: String,
    pub sha: String,
    /// `"dev"` or `"prod"` from `service.toml`.
    pub environment: String,
    pub started_at: DateTime<Utc>,
    /// The process's resident memory now, when the platform reports it.
    #[serde(default)]
    pub rss_bytes: Option<u64>,
    /// The machine's (or VM's) physical memory, when the platform reports it.
    #[serde(default)]
    pub physical_memory_bytes: Option<u64>,
}

static STARTED_AT: OnceLock<DateTime<Utc>> = OnceLock::new();

/// Records the process start; the first call wins. `margin-service serve` calls it
/// first thing, and the router calls it too so tests and embedders get a time.
pub fn mark_started() -> DateTime<Utc> {
    *STARTED_AT.get_or_init(Utc::now)
}

pub fn info(environment: &str) -> VersionInfo {
    VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        sha: BUILD_SHA.to_string(),
        environment: environment.to_string(),
        started_at: mark_started(),
        rss_bytes: resident_bytes(),
        physical_memory_bytes: physical_memory_bytes(),
    }
}

pub async fn get_version(State(state): State<AppState>) -> Json<VersionInfo> {
    Json(info(&state.config.environment))
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
        assert_eq!(v.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(v.sha, BUILD_SHA);
        assert_eq!(v.started_at, first);
        // The JSON shape deploy.sh and margin-eval read.
        let json = serde_json::to_value(&v).unwrap();
        for key in ["version", "sha", "environment", "started_at", "rss_bytes"] {
            assert!(json.get(key).is_some(), "{key} missing: {json}");
        }
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
