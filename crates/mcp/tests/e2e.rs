//! margin-mcp against the in-process fake service: sign-in, the MCP tools over a real
//! JSON-RPC stream, git-aware ingest-dir, and purge. No Keychain (a memory store) and
//! never the real service.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use common::{Fake, TOKEN};
use margin_mcp::client::Client;
use margin_mcp::config::{TokenStore, require_token};
use margin_mcp::{ingest, login, serve};
use margin_shared::context::{LibraryKind, Origin};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

#[derive(Default)]
struct MemoryStore(Mutex<HashMap<String, String>>);

impl TokenStore for MemoryStore {
    fn get(&self, url: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(url).cloned())
    }
    fn set(&self, url: &str, token: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().insert(url.into(), token.into());
        Ok(())
    }
}

/// A line-delimited JSON-RPC client, as Claude Code speaks to `margin-mcp serve`.
struct Mcp {
    write: DuplexStream,
    read: Lines<BufReader<DuplexStream>>,
    next: u64,
}

impl Mcp {
    async fn start(client: Client) -> Mcp {
        let (client_write, server_read) = tokio::io::duplex(1 << 16);
        let (server_write, client_read) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve::run(client, server_read, server_write));
        let mut mcp = Mcp {
            write: client_write,
            read: BufReader::new(client_read).lines(),
            next: 0,
        };
        let init = mcp
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"}
                }),
            )
            .await;
        assert_eq!(init["serverInfo"]["name"], "margin");
        assert!(
            init["instructions"]
                .as_str()
                .unwrap()
                .contains("search_context")
        );
        mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        mcp
    }

    async fn send(&mut self, msg: Value) {
        let mut line = serde_json::to_vec(&msg).unwrap();
        line.push(b'\n');
        self.write.write_all(&line).await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(10), self.read.next_line())
                .await
                .expect("the server answers")
                .unwrap()
                .expect("the stream stays open");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == json!(id) {
                assert!(msg.get("error").is_none(), "{msg}");
                return msg["result"].clone();
            }
        }
    }

    /// Calls a tool; returns (is_error, text).
    async fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let result = self
            .call("tools/call", json!({"name": name, "arguments": args}))
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        (result["isError"] == json!(true), text)
    }
}

fn signed_in(base: &str) -> Client {
    Client::new(base, Some(TOKEN.into())).unwrap()
}

/// The plan's integration test: login on the fake → ingest a doc → search finds it with
/// a citation → changes lists it.
#[tokio::test]
async fn login_ingest_search_changes() {
    let (fake, base) = Fake::start().await;
    let store = MemoryStore::default();
    let anon = Client::new(&base, None).unwrap();
    let mut said = Vec::new();
    let http = reqwest::Client::new();
    let github = login::login(
        &anon,
        "Claude Code (test-mac)",
        &store,
        |url| {
            // The "browser": GitHub approves.
            let url = url.to_string();
            let http = http.clone();
            tokio::spawn(async move { http.get(url).send().await.unwrap() });
            Ok(())
        },
        &mut |l| said.push(l.to_string()),
        Duration::from_millis(20),
    )
    .await
    .unwrap();
    assert_eq!(github, "Nkr1shna");
    assert!(said[0].contains("/fake-github?state=login1"));
    assert_eq!(fake.lock().logins["login1"].0, "Claude Code (test-mac)");
    let token = require_token(&store, &base).unwrap();
    assert_eq!(token, TOKEN);

    let mut mcp = Mcp::start(Client::new(&base, Some(token)).unwrap()).await;
    let tools = mcp.call("tools/list", json!({})).await;
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "document_outline",
            "ingest_document",
            "read_passages",
            "recent_changes",
            "search_context"
        ]
    );

    let (is_error, text) = mcp
        .tool(
            "ingest_document",
            json!({
                "name": "keep both memories",
                "kind": "decision",
                "text": "# Memory\n\n## Spectron\n\nKeep the Spectron agent memory for two weeks and mirror decisions.\n"
            }),
        )
        .await;
    assert!(!is_error, "{text}");
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["name"], "Decision: keep both memories");
    assert_eq!(doc["actor"], "agent:claude-code");

    let (is_error, text) = mcp
        .tool("search_context", json!({"query": "spectron memory weeks"}))
        .await;
    assert!(!is_error, "{text}");
    let hits: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(hits[0]["document"], "Decision: keep both memories");
    assert_eq!(
        hits[0]["citation"],
        "Decision: keep both memories § Memory > Spectron, lines 5"
    );
    assert!(hits[0]["text"].as_str().unwrap().contains("two weeks"));
    let q = fake.lock().searches[0].clone();
    assert_eq!(q.k, serve::DEFAULT_K);

    let (_, text) = mcp.tool("recent_changes", json!({})).await;
    let changes: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        changes["changed"][0]["name"],
        "Decision: keep both memories"
    );
    assert_eq!(changes["changed"][0]["origin"], "agent claude-code");
    let generation = changes["generation"].as_u64().unwrap();
    let (_, text) = mcp
        .tool("recent_changes", json!({"since": generation}))
        .await;
    let changes: Value = serde_json::from_str(&text).unwrap();
    assert!(changes["changed"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn outline_and_passages_name_documents_the_server_never_saw() {
    let (fake, base) = Fake::start().await;
    let body: String = (0..10)
        .map(|i| format!("## Part {i}\n\nParagraph number {i} about retrieval.\n\n"))
        .collect();
    // Written by the owner before the server started: its name comes from the feed.
    let id = fake.add(
        "Chat: retrieval",
        Origin::Library {
            kind: LibraryKind::Chat,
            id: "c1".into(),
        },
        "owner",
        &body,
    );
    let mut mcp = Mcp::start(signed_in(&base)).await;

    let (is_error, text) = mcp
        .tool("document_outline", json!({"document_id": id}))
        .await;
    assert!(!is_error, "{text}");
    let outline: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(outline["document"], "Chat: retrieval");
    assert_eq!(outline["sections"].as_array().unwrap().len(), 10);

    // Ten passages are read four at a time, and stop at the end of the document.
    let (is_error, text) = mcp
        .tool(
            "read_passages",
            json!({"document_id": id, "from": 0, "to": 12}),
        )
        .await;
    assert!(!is_error, "{text}");
    let hits: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 10);
    assert_eq!(hits[9]["citation"], "Chat: retrieval § Part 9, lines 39");
    assert!(hits[0].get("score").is_none());
    assert_eq!(
        fake.lock().passage_reads,
        [(id.clone(), 0, 3), (id.clone(), 4, 7), (id.clone(), 8, 11)]
    );

    let (is_error, text) = mcp
        .tool(
            "read_passages",
            json!({"document_id": id, "from": 0, "to": 40}),
        )
        .await;
    assert!(is_error);
    assert!(text.contains("at most 32"), "{text}");
    let (is_error, text) = mcp
        .tool(
            "read_passages",
            json!({"document_id": "nope", "from": 0, "to": 1}),
        )
        .await;
    assert!(is_error);
    assert!(text.contains("not_found"), "{text}");
    let (is_error, text) = mcp
        .tool(
            "search_context",
            json!({"query": "retrieval", "scope": "topic:t9"}),
        )
        .await;
    assert!(is_error);
    assert!(text.contains("bad_scope"), "{text}");
    let (is_error, text) = mcp.tool("search_context", json!({"query": "zebra"})).await;
    assert!(!is_error);
    assert!(text.starts_with("No passages matched"));
}

#[tokio::test]
async fn a_bad_token_says_to_sign_in_again() {
    let (_fake, base) = Fake::start().await;
    let mut mcp = Mcp::start(Client::new(&base, Some("revoked".into())).unwrap()).await;
    let (is_error, text) = mcp
        .tool("search_context", json!({"query": "anything"}))
        .await;
    assert!(is_error);
    assert!(text.contains("margin-mcp login"), "{text}");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn short_sha(dir: &Path, file: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%h", "--", file])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn ingest_dir_follows_the_repository() {
    let (fake, base) = Fake::start().await;
    let client = signed_in(&base);
    let repo = std::env::temp_dir().join(format!("margin-mcp-repo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("docs/a.md"), "# A\n\nAlpha.\n").unwrap();
    std::fs::write(repo.join("docs/b.md"), "# B\n\nBeta.\n").unwrap();
    std::fs::write(repo.join("docs/notes.txt"), "not markdown").unwrap();
    std::fs::write(repo.join("PLAN.md"), "# Plan\n\nThe plan.\n").unwrap();
    std::fs::write(repo.join("docs/untracked.md"), "# U\n\nUntracked.\n").unwrap();
    git(
        &repo,
        &["add", "docs/a.md", "docs/b.md", "docs/notes.txt", "PLAN.md"],
    );
    git(&repo, &["commit", "-q", "-m", "one"]);
    let owner_doc = fake.add(
        "docs/a.md@owner",
        Origin::Agent { name: "x".into() },
        "owner",
        "# O\n\nMine.\n",
    );
    let mut quiet = |_: &str| {};

    let sha1 = short_sha(&repo, "docs/a.md");
    let paths = vec!["docs".to_string(), "PLAN.md".to_string()];
    let dry = ingest::ingest_dir(
        &client,
        &repo.join("docs"),
        &["../docs".into(), "../PLAN.md".into()],
        true,
        &mut quiet,
    )
    .await
    .unwrap();
    assert_eq!(dry.ingested.len(), 3);
    assert_eq!(
        fake.names(),
        ["docs/a.md@owner"],
        "a dry run writes nothing"
    );

    let first = ingest::ingest_dir(&client, &repo, &paths, false, &mut quiet)
        .await
        .unwrap();
    assert_eq!(
        first.ingested,
        [
            format!("PLAN.md@{sha1}"),
            format!("docs/a.md@{sha1}"),
            format!("docs/b.md@{sha1}")
        ]
    );
    let repo_docs: Vec<_> = fake
        .lock()
        .docs
        .iter()
        .filter(|(d, _)| d.actor == "agent:repo")
        .map(|(d, _)| d.origin.clone())
        .collect();
    assert_eq!(repo_docs.len(), 3);
    assert!(repo_docs.iter().all(|o| *o
        == Origin::Agent {
            name: "repo".into()
        }));

    let again = ingest::ingest_dir(&client, &repo, &paths, false, &mut quiet)
        .await
        .unwrap();
    assert!(again.ingested.is_empty());
    assert_eq!(again.unchanged.len(), 3);

    // An edit not yet committed is ingested as -dirty, replacing by name on re-runs.
    std::fs::write(repo.join("docs/a.md"), "# A\n\nAlpha two.\n").unwrap();
    let dirty = ingest::ingest_dir(&client, &repo, &paths, false, &mut quiet)
        .await
        .unwrap();
    assert_eq!(dirty.ingested, [format!("docs/a.md@{sha1}-dirty")]);
    assert_eq!(dirty.removed, [format!("docs/a.md@{sha1}")]);

    // Committed: the new name replaces the dirty one; a deleted file's document goes.
    git(&repo, &["rm", "-q", "docs/b.md"]);
    git(&repo, &["commit", "-q", "-am", "two"]);
    let sha2 = short_sha(&repo, "docs/a.md");
    let second = ingest::ingest_dir(&client, &repo, &paths, false, &mut quiet)
        .await
        .unwrap();
    assert_eq!(second.ingested, [format!("docs/a.md@{sha2}")]);
    assert_eq!(
        second.removed,
        [
            format!("docs/a.md@{sha1}-dirty"),
            format!("docs/b.md@{sha1}")
        ]
    );
    let mut expected = vec![
        format!("PLAN.md@{sha1}"),
        "docs/a.md@owner".to_string(),
        format!("docs/a.md@{sha2}"),
    ];
    expected.sort();
    assert_eq!(fake.names(), expected);
    assert!(fake.lock().docs.iter().any(|(d, _)| d.id == owner_doc));

    // Purge removes only what the agent wrote.
    assert!(
        ingest::purge(&client, "owner", false, &mut quiet)
            .await
            .is_err()
    );
    let would = ingest::purge(&client, "agent:repo", true, &mut quiet)
        .await
        .unwrap();
    assert_eq!(would.len(), 2);
    assert_eq!(fake.names().len(), 3);
    let purged = ingest::purge(&client, "agent:repo", false, &mut quiet)
        .await
        .unwrap();
    assert_eq!(purged.len(), 2);
    assert_eq!(fake.names(), ["docs/a.md@owner"]);
    std::fs::remove_dir_all(&repo).unwrap();
}

#[tokio::test]
async fn the_ingest_command_writes_as_claude_code_and_refuses_empty_text() {
    let (fake, base) = Fake::start().await;
    let client = signed_in(&base);
    let doc = ingest::ingest_text(
        &client,
        ingest::CLAUDE_CODE,
        "Session 12",
        Some("handoff"),
        "Did things.".into(),
    )
    .await
    .unwrap();
    assert_eq!(doc.name, "Handoff: Session 12");
    assert_eq!(doc.actor, "agent:claude-code");
    assert!(
        ingest::ingest_text(&client, "claude-code", "x", None, "  ".into())
            .await
            .is_err()
    );
    assert_eq!(fake.names(), ["Handoff: Session 12"]);
}

#[test]
fn the_binary_needs_a_url_before_it_touches_the_keychain() {
    let home = std::env::temp_dir().join(format!("margin-mcp-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let bin = env!("CARGO_BIN_EXE_margin-mcp");
    let help = Command::new(bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("ingest-dir"));
    let out = Command::new(bin)
        .args(["purge", "--actor", "agent:repo", "--dry-run"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--url"));
    let bad = Command::new(bin)
        .arg("frobnicate")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown command"));
    std::fs::remove_dir_all(&home).unwrap();
}
