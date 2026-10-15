//! The owner-only export of one person's library (roadmap L2, `docs/l2-brief.md` D7,
//! owner Q3): `margin-service export --person <id> --out <dir>` on the service host.
//! It refuses while the service runs (the lock file, as `check-data` does), copies the
//! store to a temporary folder (never opening the live data), writes
//! `<dir>/<person>/{topics,lessons,notes,chats,sources,documents}.jsonl` and the source
//! files, and appends `{at, person, by, host, out}` to `<data>/exports.log`; the next
//! start turns that line into an `AuditKind::Export` row. There is no API route.
//!
//! The first commit declares the signature; L2-audit builds it.

use std::path::Path;

/// The file every export appends a line to, in the data folder.
pub const EXPORTS_LOG: &str = "exports.log";

/// Runs the export of `person`'s rows from `data_dir` (stopped) into `out`.
pub fn run(_data_dir: &Path, _person: &str, _out: &Path) -> anyhow::Result<()> {
    unimplemented!("L2-audit: export::run")
}

/// `--person <id> --out <dir>`, in either order.
pub fn parse_args(args: &[String]) -> anyhow::Result<(String, std::path::PathBuf)> {
    let mut person = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--person" => person = it.next().cloned(),
            "--out" => out = it.next().map(std::path::PathBuf::from),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    match (person, out) {
        (Some(p), Some(o)) if !p.is_empty() => Ok((p, o)),
        _ => anyhow::bail!("give --person <id> and --out <dir>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (p, o) = parse_args(&a(&["--out", "/tmp/x", "--person", "p1"])).unwrap();
        assert_eq!((p.as_str(), o.as_path()), ("p1", Path::new("/tmp/x")));
        assert!(parse_args(&a(&["--person", "p1"])).is_err());
        assert!(parse_args(&a(&["--person", "p1", "--out", "o", "--x"])).is_err());
    }
}
