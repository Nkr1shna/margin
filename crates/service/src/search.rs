//! ⌘K search (and the agent's `search_library`) over topics, lessons and notes.
//!
//! Lessons and notes are found through FULLTEXT indexes on their title, body and quote
//! (`db::LIBRARY_INDEXES`; analyzer `library`, which indexes every word's beginnings so a
//! word being typed already matches), not by reading every body. What the index finds
//! is then checked and ranked as before: a case-insensitive match of the whole query in
//! one field, topics first, then lessons, then notes, title matches first.
//!
//! Only rows the caller may read are searched (L1 D6): each statement carries
//! `state::ACCESS_WHERE`, and the rows found are checked again with `can_read`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use margin_shared::{Note, SearchHit, SearchKind, Topic};
use serde::Deserialize;

use crate::db::{self, Store};
use crate::error::AppResult;
use crate::principal::{CurrentPrincipal, Principal};
use crate::state::{ACCESS_WHERE, AppState, StoredLesson, access_binds, can_read, list_visible};

pub const MAX_HITS: usize = 30;
const SNIPPET_BEFORE: usize = 60;
const SNIPPET_LEN: usize = 160;

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// `GET /api/search?q` (⌘K): the caller's hits.
pub async fn search(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<Vec<SearchHit>>> {
    Ok(Json(find(&state.store, &p, &q.q).await?))
}

/// The library's hits for `query` among what `p` may read (`run`'s order), with lessons
/// and notes found through the FULLTEXT indexes.
pub async fn find(store: &Store, p: &Principal, query: &str) -> anyhow::Result<Vec<SearchHit>> {
    let needle = query.trim();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let topics: Vec<Topic> = list_visible(store, db::TOPIC, p).await?;
    let q = serde_json::json!(needle);
    let binds = |q: serde_json::Value| {
        let mut b = access_binds(p);
        b.push(("q", q));
        b
    };
    let mut lessons: Vec<StoredLesson> = store
        .query_rows(
            &format!(
                "SELECT VALUE doc FROM lesson \
                 WHERE (doc.title @0@ $q OR doc.body_md @1@ $q) AND {ACCESS_WHERE}"
            ),
            binds(q.clone()),
        )
        .await?;
    lessons.retain(|l| can_read(p, l));
    let mut notes: Vec<Note> = store
        .query_rows(
            &format!(
                "SELECT VALUE doc FROM note \
                 WHERE (doc.title @0@ $q OR doc.body_md @1@ $q OR doc.quote @2@ $q) \
                 AND {ACCESS_WHERE}"
            ),
            binds(q),
        )
        .await?;
    notes.retain(|n| can_read(p, n));
    Ok(run(needle, &topics, &lessons, &notes))
}

/// Ranks and cuts hits among `topics`, `lessons` and `notes` (every topic is needed for
/// the breadcrumbs; lessons and notes may be just the index's candidates).
pub fn run(
    needle: &str,
    topics: &[Topic],
    lessons: &[StoredLesson],
    notes: &[Note],
) -> Vec<SearchHit> {
    let by_id: HashMap<&str, &Topic> = topics.iter().map(|t| (t.id.as_str(), t)).collect();
    let crumbs = |topic_id: &str| breadcrumb(&by_id, topic_id);
    let lesson_titles: HashMap<&str, &str> = lessons
        .iter()
        .map(|l| (l.lesson.id.as_str(), l.lesson.title.as_str()))
        .collect();

    // (title matched, title, hit) — title matches rank first within a kind.
    let mut topic_hits = Vec::new();
    for t in topics {
        if find_ci(&t.title, needle).is_some() {
            let context = t.parent_id.as_deref().map(crumbs).unwrap_or_default();
            topic_hits.push((
                true,
                hit(SearchKind::Topic, &t.id, &t.title, context, plain(&t.title)),
            ));
        }
    }

    let mut lesson_hits = Vec::new();
    for l in lessons {
        let l = &l.lesson;
        let title_match = find_ci(&l.title, needle).is_some();
        let body = plain(&l.body_md);
        let snippet = if let Some(s) = snippet(&body, needle) {
            s
        } else if title_match {
            truncate(&plain(if l.summary.is_empty() {
                &l.body_md
            } else {
                &l.summary
            }))
        } else {
            continue;
        };
        lesson_hits.push((
            title_match,
            hit(
                SearchKind::Lesson,
                &l.id,
                &l.title,
                crumbs(&l.topic_id),
                snippet,
            ),
        ));
    }

    let mut note_hits = Vec::new();
    for n in notes {
        let title_match = find_ci(&n.title, needle).is_some();
        let body = plain(&n.body_md);
        let quote = n.quote.as_deref().map(plain).unwrap_or_default();
        let snippet = if let Some(s) = snippet(&body, needle) {
            s
        } else if let Some(s) = snippet(&quote, needle) {
            s
        } else if title_match {
            truncate(&body)
        } else {
            continue;
        };
        let mut context = crumbs(&n.topic_id);
        if let Some(title) = n.lesson_id.as_deref().and_then(|id| lesson_titles.get(id)) {
            context = if context.is_empty() {
                title.to_string()
            } else {
                format!("{context} / {title}")
            };
        }
        let title = if n.title.is_empty() {
            "Untitled note".to_string()
        } else {
            n.title.clone()
        };
        note_hits.push((
            title_match,
            hit(SearchKind::Note, &n.id, &title, context, snippet),
        ));
    }

    let mut out = Vec::new();
    for mut group in [topic_hits, lesson_hits, note_hits] {
        group.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase()))
        });
        out.extend(group.into_iter().map(|(_, h)| h));
    }
    out.truncate(MAX_HITS);
    out
}

fn hit(kind: SearchKind, id: &str, title: &str, context: String, snippet: String) -> SearchHit {
    SearchHit {
        kind,
        id: id.to_string(),
        title: title.to_string(),
        context,
        snippet,
    }
}

/// "Subject / Topic" for a topic id (the topic itself included).
fn breadcrumb(by_id: &HashMap<&str, &Topic>, topic_id: &str) -> String {
    let mut parts = Vec::new();
    let mut cur = by_id.get(topic_id).copied();
    while let Some(t) = cur {
        if parts.len() > 32 {
            break;
        }
        parts.push(t.title.as_str());
        cur = t.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    parts.reverse();
    parts.join(" / ")
}

/// Markdown to rough plain text: drops formatting characters, collapses whitespace.
pub fn plain(md: &str) -> String {
    let stripped: String = md
        .chars()
        .filter(|c| !matches!(c, '#' | '*' | '`' | '>' | '_' | '~'))
        .collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Char index of the first case-insensitive occurrence of `needle` in `hay`.
pub fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let hay: Vec<char> = hay.chars().collect();
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Some(0);
    }
    'outer: for start in 0..hay.len() {
        let mut lowered = hay[start..].iter().flat_map(|c| c.to_lowercase());
        for n in &needle {
            if lowered.next() != Some(*n) {
                continue 'outer;
            }
        }
        return Some(start);
    }
    None
}

/// Up to ~160 characters around the first match.
pub fn snippet(text: &str, needle: &str) -> Option<String> {
    let at = find_ci(text, needle)?;
    let chars: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(SNIPPET_BEFORE);
    let end = (start + SNIPPET_LEN).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    s = s.trim().to_string();
    if start > 0 {
        s = format!("…{s}");
    }
    if end < chars.len() {
        s.push('…');
    }
    Some(s)
}

fn truncate(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= SNIPPET_LEN {
        return text.to_string();
    }
    let mut s: String = chars[..SNIPPET_LEN].iter().collect();
    s = s.trim_end().to_string();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::OrgRole;

    #[test]
    fn find_is_case_insensitive_and_char_based() {
        assert_eq!(find_ci("Fourier Series", "series"), Some(8));
        assert_eq!(find_ci("Ünïcode ÉTÉ", "été"), Some(8));
        assert_eq!(find_ci("abc", "x"), None);
    }

    #[test]
    fn snippet_centers_on_match() {
        let text = format!("{} needle {}", "a ".repeat(100), "b ".repeat(100));
        let s = snippet(&text, "NEEDLE").unwrap();
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.contains("needle"));
        assert!(s.chars().count() <= SNIPPET_LEN + 2);
        assert_eq!(snippet("short needle", "needle").unwrap(), "short needle");
    }

    #[test]
    fn plain_strips_markdown() {
        assert_eq!(plain("## Title\n\n**bold** `code`"), "Title bold code");
    }

    async fn lesson(store: &Store, id: &str, title: &str, body: &str) {
        let now = chrono::Utc::now();
        let row = serde_json::json!({
            "id": id, "topic_id": "t", "title": title, "summary": "", "position": 0,
            "level": "intro", "minutes": 5, "body_md": body,
            "status": {"state": "ready"}, "created_at": now, "updated_at": now, "citations": [],
        });
        store.put(db::LESSON, id, &row).await.unwrap();
    }

    #[tokio::test]
    async fn the_index_finds_words_and_their_beginnings() {
        let s = Store::memory().await.unwrap();
        lesson(&s, "a", "Fourier series", "Sines **and** cosines.").await;
        lesson(
            &s,
            "b",
            "Laplace",
            "The FOURIER transform turns signals into frequencies.",
        )
        .await;
        lesson(&s, "c", "Bread", "Flour, water, salt.").await;
        let ids = |hits: Vec<SearchHit>| hits.into_iter().map(|h| h.id).collect::<Vec<_>>();
        // Rows from before L1 (no owner, no access) are the bootstrap owner's.
        let p = crate::state::test_principal(crate::principal::BOOTSTRAP_PERSON_ID, OrgRole::Owner);
        // Title matches first, then the body's; "four" doesn't find "Flour".
        assert_eq!(ids(find(&s, &p, "fourier").await.unwrap()), ["a", "b"]);
        assert_eq!(ids(find(&s, &p, "Fou").await.unwrap()), ["a", "b"]);
        // The whole query must match within one field.
        assert_eq!(ids(find(&s, &p, "fourier transform").await.unwrap()), ["b"]);
        assert!(find(&s, &p, "fourier water").await.unwrap().is_empty());
        assert!(find(&s, &p, "  ").await.unwrap().is_empty());
        // Someone else finds none of them.
        let bob = crate::state::test_principal("bob", OrgRole::Member);
        assert!(find(&s, &bob, "fourier").await.unwrap().is_empty());
        // The lessons were found through the index, not a scan.
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                "SELECT VALUE doc FROM lesson WHERE doc.title @0@ $q OR doc.body_md @1@ $q EXPLAIN",
                vec![("q", serde_json::json!("fourier"))],
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(
            plan.contains("lesson_title") && plan.contains("lesson_body"),
            "{plan}"
        );
    }

    /// A lesson and a note with an owner and access, as L1 writes them.
    async fn owned(store: &Store, table: &'static str, id: &str, owner: &str, title: &str) {
        let now = chrono::Utc::now();
        let row = serde_json::json!({
            "id": id, "topic_id": "t", "title": title, "summary": "", "position": 0,
            "level": "intro", "minutes": 5, "body_md": format!("{title} body"),
            "quote": null, "lesson_id": null,
            "status": {"state": "ready"}, "created_at": now, "updated_at": now, "citations": [],
            "owner": owner, "access": [format!("person/{owner}")], "deny": [],
        });
        store.put(table, id, &row).await.unwrap();
    }

    #[tokio::test]
    async fn search_finds_only_what_the_caller_may_read() {
        let s = Store::memory().await.unwrap();
        owned(&s, db::LESSON, "la", "alice", "Fourier for Alice").await;
        owned(&s, db::LESSON, "lb", "bob", "Fourier for Bob").await;
        owned(&s, db::NOTE, "na", "alice", "Fourier note of Alice").await;
        owned(&s, db::NOTE, "nb", "bob", "Fourier note of Bob").await;
        let topic = |id: &str, owner: &str, title: &str| {
            let now = chrono::Utc::now();
            serde_json::json!({
                "id": id, "parent_id": null, "title": title, "position": 0,
                "created_at": now, "updated_at": now,
                "owner": owner, "access": [format!("person/{owner}")], "deny": [],
            })
        };
        s.put(db::TOPIC, "ta", &topic("ta", "alice", "Fourier A"))
            .await
            .unwrap();
        s.put(db::TOPIC, "tb", &topic("tb", "bob", "Fourier B"))
            .await
            .unwrap();
        let ids = |hits: Vec<SearchHit>| {
            let mut v: Vec<String> = hits.into_iter().map(|h| h.id).collect();
            v.sort();
            v
        };
        let alice = crate::state::test_principal("alice", OrgRole::Owner);
        let bob = crate::state::test_principal("bob", OrgRole::Member);
        assert_eq!(
            ids(find(&s, &alice, "fourier").await.unwrap()),
            ["la", "na", "ta"]
        );
        assert_eq!(
            ids(find(&s, &bob, "fourier").await.unwrap()),
            ["lb", "nb", "tb"]
        );
        // A shared path reaches whoever holds it.
        let mut shared = topic("tc", "alice", "Fourier shared");
        shared["access"] = serde_json::json!(["company"]);
        s.put(db::TOPIC, "tc", &shared).await.unwrap();
        assert!(ids(find(&s, &bob, "fourier").await.unwrap()).contains(&"tc".to_string()));
    }

    /// L2 D5: ⌘K evaluates `deny` inside both FULLTEXT statements and the topics' list:
    /// a row shared with `company` that denies Bob (by name, or one of his groups) is
    /// missing from his hits, and still found by everyone else.
    #[tokio::test]
    async fn search_leaves_out_what_denies_the_caller() {
        let s = Store::memory().await.unwrap();
        let now = chrono::Utc::now();
        let put = |table: &'static str, id: &str, title: &str, deny: serde_json::Value| {
            let row = serde_json::json!({
                "id": id, "topic_id": "t", "parent_id": null, "title": title, "summary": "",
                "position": 0, "level": "intro", "minutes": 5,
                "body_md": format!("{title} body"), "quote": null, "lesson_id": null,
                "status": {"state": "ready"}, "created_at": now, "updated_at": now,
                "citations": [], "owner": "alice", "access": ["company"], "deny": deny,
            });
            let s = s.clone();
            async move {
                s.put(table, row["id"].as_str().unwrap(), &row)
                    .await
                    .unwrap()
            }
        };
        put(db::TOPIC, "t1", "Fourier open", serde_json::json!([])).await;
        put(
            db::TOPIC,
            "t2",
            "Fourier not bob",
            serde_json::json!(["person/bob"]),
        )
        .await;
        put(db::LESSON, "l1", "Fourier open", serde_json::json!([])).await;
        put(
            db::LESSON,
            "l2",
            "Fourier not bob",
            serde_json::json!(["person/bob"]),
        )
        .await;
        put(
            db::LESSON,
            "l3",
            "Fourier not sales",
            serde_json::json!(["company/sales"]),
        )
        .await;
        put(db::NOTE, "n1", "Fourier open", serde_json::json!([])).await;
        put(
            db::NOTE,
            "n2",
            "Fourier not bob",
            serde_json::json!(["person/bob"]),
        )
        .await;
        // A row from before `deny` existed denies no one.
        let mut old = serde_json::json!({
            "id": "l4", "topic_id": "t", "title": "Fourier old", "summary": "", "position": 0,
            "level": "intro", "minutes": 5, "body_md": "Fourier old body",
            "status": {"state": "ready"}, "created_at": now, "updated_at": now,
            "citations": [], "owner": "alice", "access": ["company"],
        });
        old.as_object_mut().unwrap().remove("deny");
        s.put(db::LESSON, "l4", &old).await.unwrap();
        let ids = |hits: Vec<SearchHit>| {
            let mut v: Vec<String> = hits.into_iter().map(|h| h.id).collect();
            v.sort();
            v
        };
        let mut bob = crate::state::test_principal("bob", OrgRole::Member);
        assert_eq!(
            ids(find(&s, &bob, "fourier").await.unwrap()),
            ["l1", "l3", "l4", "n1", "t1"]
        );
        // Bob joins sales: the group's denial reaches him through his expansion.
        bob.paths.push("company/sales".into());
        bob.deny_ids.push("company/sales".into());
        assert_eq!(
            ids(find(&s, &bob, "fourier").await.unwrap()),
            ["l1", "l4", "n1", "t1"]
        );
        let carol = crate::state::test_principal("carol", OrgRole::Member);
        assert_eq!(
            ids(find(&s, &carol, "fourier").await.unwrap()),
            ["l1", "l2", "l3", "l4", "n1", "n2", "t1", "t2"]
        );
        // The denial is inside the indexed statement: still a FULLTEXT lookup.
        let mut binds = access_binds(&bob);
        binds.push(("q", serde_json::json!("fourier")));
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                &format!(
                    "SELECT VALUE doc FROM lesson \
                     WHERE (doc.title @0@ $q OR doc.body_md @1@ $q) AND {ACCESS_WHERE} EXPLAIN"
                ),
                binds,
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(plan.contains("lesson_title"), "{plan}");
        assert!(plan.contains("CONTAINSNONE"), "{plan}");
    }
}
