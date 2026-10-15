//! Writing into Margin: one document (`margin-mcp ingest`, the `ingest_document` tool),
//! the repository's tracked Markdown (`margin-mcp ingest-dir`, git-aware), and removing
//! everything one agent wrote (`margin-mcp purge`). Every write is an `Origin::Agent`
//! document, so the service stamps it `actor: agent:<name>` and a purge can find it all
//! again without touching the owner's own documents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use margin_shared::SourceKind;
use margin_shared::context::{ContextIngest, Document, Origin};

use crate::client::Client;

/// The agent Claude Code's own writes are attributed to (`agent:claude-code`).
pub const CLAUDE_CODE: &str = "claude-code";
/// The agent the repository's documents are attributed to (`agent:repo`).
pub const REPO: &str = "repo";

/// `agent:<name>`, as the service stamps it.
pub fn actor(agent: &str) -> String {
    format!("agent:{agent}")
}

/// What a document is, as `--kind` / the tool's `kind` names it: a source kind the
/// service understands (`text`, `code`), or a label for an agent's own writing that
/// becomes the start of its name ("Decision: …", "Handoff: …", "Note: …") and is stored
/// as text.
pub fn resolve_kind(kind: Option<&str>, name: &str) -> anyhow::Result<(SourceKind, String)> {
    let label = match kind.map(|k| k.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("text") | Some("doc") | Some("document") => {
            return Ok((SourceKind::Text, name.to_string()));
        }
        Some("code") => return Ok((SourceKind::Code, name.to_string())),
        Some("decision") => "Decision",
        Some("handoff") | Some("summary") => "Handoff",
        Some("note") => "Note",
        Some(other) => bail!("unknown kind {other:?}: use text, code, decision, handoff or note"),
    };
    let prefix = format!("{label}: ");
    let named = if name
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        name.to_string()
    } else {
        format!("{prefix}{name}")
    };
    Ok((SourceKind::Text, named))
}

/// Adds one text document as `agent:<agent>`; a document of the same name by the same
/// agent is replaced by the service.
pub async fn ingest_text(
    client: &Client,
    agent: &str,
    name: &str,
    kind: Option<&str>,
    text: String,
) -> anyhow::Result<Document> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a document needs a name");
    }
    if text.trim().is_empty() {
        bail!("{name:?} is empty; nothing to add");
    }
    let (kind, name) = resolve_kind(kind, name)?;
    client
        .ingest(&ContextIngest {
            name,
            origin: Origin::Agent { name: agent.into() },
            kind,
            text,
            audience: None,
        })
        .await
}

/// What `ingest-dir` did (or, with `--dry-run`, would do).
#[derive(Debug, Default, PartialEq)]
pub struct DirReport {
    /// New names ingested.
    pub ingested: Vec<String>,
    /// Names already in Margin at this commit.
    pub unchanged: Vec<String>,
    /// Older names deleted: superseded by a newer commit, or the file is gone.
    pub removed: Vec<String>,
}

/// `name@sha` → `name`.
fn base_name(doc_name: &str) -> &str {
    doc_name.rsplit_once('@').map_or(doc_name, |(base, _)| base)
}

fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .context("couldn't run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn nul_list(s: &str) -> impl Iterator<Item = &str> {
    s.split('\0').filter(|p| !p.is_empty())
}

/// `prefix` + `arg`, `.`/`..` resolved, `/`-separated, relative to the repository root.
fn repo_relative(prefix: &str, arg: &str) -> anyhow::Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in PathBuf::from(prefix).join(arg).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    bail!("{arg} is outside the repository");
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("{arg}: give paths inside the repository, relative to it")
            }
        }
    }
    Ok(parts.join("/"))
}

fn under(path: &str, roots: &[String]) -> bool {
    roots
        .iter()
        .any(|r| r.is_empty() || path == r || path.starts_with(&format!("{r}/")))
}

/// Ingests the tracked `*.md` files under `paths` (relative to `cwd`, inside a git
/// checkout) as `agent:repo` documents named `<repo path>@<short sha>`: the sha of the
/// last commit that changed the file, with `-dirty` when the checkout differs from it.
/// A file already in Margin under the same clean name is skipped; an older name of a
/// file is deleted once the new one is in; a file that is no longer tracked under
/// `paths` has its document deleted. `say` gets one line per action.
pub async fn ingest_dir(
    client: &Client,
    cwd: &Path,
    paths: &[String],
    dry_run: bool,
    say: &mut dyn FnMut(&str),
) -> anyhow::Result<DirReport> {
    if paths.is_empty() {
        bail!("name the files or folders to ingest, e.g. `margin-mcp ingest-dir docs PLAN.md`");
    }
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let prefix = git(cwd, &["rev-parse", "--show-prefix"])?
        .trim()
        .to_string();
    git(&root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map_err(|_| anyhow!("the repository has no commits yet; commit first"))?;
    let roots = paths
        .iter()
        .map(|p| repo_relative(&prefix, p))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut ls_args = vec!["ls-files", "-z", "--full-name", "--"];
    ls_args.extend(
        roots
            .iter()
            .map(|r| if r.is_empty() { "." } else { r.as_str() }),
    );
    let listed = git(&root, &ls_args)?;
    let files: Vec<String> = nul_list(&listed)
        .filter(|p| p.to_ascii_lowercase().ends_with(".md"))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut diff_args = vec!["diff", "--name-only", "-z", "HEAD", "--"];
    diff_args.extend(files.iter().map(String::as_str));
    let dirty: BTreeSet<String> = if files.is_empty() {
        BTreeSet::new()
    } else {
        nul_list(&git(&root, &diff_args)?)
            .map(str::to_string)
            .collect()
    };
    let head = git(&root, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    // What Margin already holds from the repository, by file.
    let mut existing: BTreeMap<String, Vec<Document>> = BTreeMap::new();
    let repo_actor = actor(REPO);
    for doc in client.documents().await? {
        if doc.actor == repo_actor {
            existing
                .entry(base_name(&doc.name).to_string())
                .or_default()
                .push(doc);
        }
    }

    let mut report = DirReport::default();
    let verb = if dry_run { "would " } else { "" };
    for file in &files {
        let last = git(&root, &["log", "-1", "--format=%h", "--", file])?;
        let sha = match last.trim() {
            "" => head.as_str(),
            sha => sha,
        };
        let is_dirty = dirty.contains(file) || last.trim().is_empty();
        let name = if is_dirty {
            format!("{file}@{sha}-dirty")
        } else {
            format!("{file}@{sha}")
        };
        let olds = existing.remove(file.as_str()).unwrap_or_default();
        if !is_dirty && olds.iter().any(|d| d.name == name) {
            for old in olds.iter().filter(|d| d.name != name) {
                say(&format!("{verb}remove {} (superseded)", old.name));
                if !dry_run {
                    client.delete_document(&old.id).await?;
                }
                report.removed.push(old.name.clone());
            }
            report.unchanged.push(name);
            continue;
        }
        let text =
            std::fs::read(root.join(file)).with_context(|| format!("couldn't read {file}"))?;
        let text = String::from_utf8_lossy(&text).into_owned();
        if text.trim().is_empty() {
            say(&format!("skip {file} (empty)"));
            continue;
        }
        say(&format!("{verb}ingest {name}"));
        if !dry_run {
            client
                .ingest(&ContextIngest {
                    name: name.clone(),
                    origin: Origin::Agent { name: REPO.into() },
                    kind: SourceKind::Text,
                    text,
                    audience: None,
                })
                .await?;
        }
        for old in olds.iter().filter(|d| d.name != name) {
            say(&format!("{verb}remove {} (superseded)", old.name));
            if !dry_run {
                client.delete_document(&old.id).await?;
            }
            report.removed.push(old.name.clone());
        }
        report.ingested.push(name);
    }
    // Files that were ingested before and are no longer tracked under these paths.
    for (file, docs) in existing {
        if !under(&file, &roots) {
            continue;
        }
        for doc in docs {
            say(&format!(
                "{verb}remove {} (no longer in the repository)",
                doc.name
            ));
            if !dry_run {
                client.delete_document(&doc.id).await?;
            }
            report.removed.push(doc.name);
        }
    }
    say(&format!(
        "{} {}, {} unchanged, {} {}removed",
        report.ingested.len(),
        if dry_run { "to ingest" } else { "ingested" },
        report.unchanged.len(),
        report.removed.len(),
        if dry_run { "to be " } else { "" },
    ));
    Ok(report)
}

/// Deletes every document `actor` wrote (`agent:<name>` only; the owner's own documents
/// are never purged). Returns the names deleted (or, with `dry_run`, that would be).
pub async fn purge(
    client: &Client,
    actor: &str,
    dry_run: bool,
    say: &mut dyn FnMut(&str),
) -> anyhow::Result<Vec<String>> {
    let Some(agent) = actor.strip_prefix("agent:").filter(|a| !a.is_empty()) else {
        bail!("purge only removes what an agent wrote: give --actor agent:<name>, e.g. agent:repo");
    };
    let actor = format!("agent:{agent}");
    let verb = if dry_run { "would delete" } else { "delete" };
    let mut names = Vec::new();
    for doc in client.documents().await? {
        if doc.actor != actor {
            continue;
        }
        say(&format!("{verb} {} ({})", doc.name, doc.id));
        if !dry_run {
            client.delete_document(&doc.id).await?;
        }
        names.push(doc.name);
    }
    say(&format!(
        "{} document{} by {actor} {}",
        names.len(),
        if names.len() == 1 { "" } else { "s" },
        if dry_run {
            "would be deleted"
        } else {
            "deleted"
        }
    ));
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_become_source_kinds_and_name_prefixes() {
        assert_eq!(
            resolve_kind(None, "notes").unwrap(),
            (SourceKind::Text, "notes".into())
        );
        assert_eq!(
            resolve_kind(Some("decision"), "keep both memories").unwrap(),
            (SourceKind::Text, "Decision: keep both memories".into())
        );
        assert_eq!(
            resolve_kind(Some("Decision"), "Decision: x").unwrap().1,
            "Decision: x"
        );
        assert_eq!(
            resolve_kind(Some("code"), "a.rs").unwrap().0,
            SourceKind::Code
        );
        assert_eq!(resolve_kind(Some("summary"), "s").unwrap().1, "Handoff: s");
        assert!(resolve_kind(Some("pdf"), "x").is_err());
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(base_name("docs/a.md@abc123"), "docs/a.md");
        assert_eq!(base_name("docs/a.md@abc123-dirty"), "docs/a.md");
        assert_eq!(base_name("plain"), "plain");
        assert_eq!(repo_relative("", "docs/").unwrap(), "docs");
        assert_eq!(repo_relative("crates/mcp/", "../../docs").unwrap(), "docs");
        assert_eq!(repo_relative("", ".").unwrap(), "");
        assert!(repo_relative("", "../x").is_err());
        assert!(repo_relative("", "/etc").is_err());
        assert!(under("docs/a.md", &["docs".into()]));
        assert!(!under("docsx/a.md", &["docs".into()]));
        assert!(under("PLAN.md", &["PLAN.md".into()]));
        assert!(under("anything.md", &["".into()]));
        assert_eq!(actor(REPO), "agent:repo");
    }
}
