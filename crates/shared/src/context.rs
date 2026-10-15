//! The context API's wire types (`docs/agent-rag-architecture.md` §2.11, §2.12): the
//! documents everything is indexed from, retrieval queries and hits, projects, and the
//! trace rows the service records for every model call, retrieval, tool call and job
//! step. Agents and the eval runner read these over HTTP (`routes::CONTEXT_*`,
//! `routes::TRACES*`); Margin's own agent uses the same retrieval code path.
//!
//! Additive only: every field added later is `#[serde(default)]`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pipeline::{Budget, Route};
use crate::{Id, SourceKind};

/// A corpus: what a retrieval may search lives inside one project. Today exactly one,
/// "My library", created at migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    #[serde(default = "epoch")]
    pub created_at: DateTime<Utc>,
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH
}

/// Something indexed into passages: an uploaded source, a file of an attached folder, a
/// lesson/note/chat of the library, or a document an agent ingested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: Id,
    pub project_id: Id,
    pub origin: Origin,
    /// File name, folder path, or title.
    pub name: String,
    pub kind: SourceKind,
    pub status: DocumentStatus,
    /// Bumped every time the document's passages change; retrieval caches key on it and
    /// `routes::CONTEXT_CHANGES` lists documents whose generation passed a value.
    pub generation: u64,
    pub bytes: u64,
    /// `owner` or `agent:<name>`: who wrote it.
    #[serde(default = "owner")]
    pub actor: String,
    /// Who may see it, as hierarchical access paths: "company", "company/engineering",
    /// "company/engineering/platform". A reader whose paths include one of these, or a
    /// prefix of one, may see the document. Copied onto every passage at ingest;
    /// retrieval filters by it inside the index query. Not enforced yet (the seam only).
    #[serde(default = "company")]
    pub access: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn owner() -> String {
    "owner".into()
}

/// The access path everything belongs to unless said otherwise.
pub const ACCESS_COMPANY: &str = "company";

fn company() -> Vec<String> {
    vec![ACCESS_COMPANY.into()]
}

/// Where a document comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Origin {
    Source {
        source_id: Id,
    },
    FolderFile {
        workspace_id: Id,
        /// Relative to the folder's root, `/`-separated.
        path: String,
    },
    /// A lesson, note or chat transcript: what humans write in the GUI is context.
    Library {
        kind: LibraryKind,
        id: Id,
    },
    /// Ingested through `routes::CONTEXT_INGEST` by an external agent.
    Agent {
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Lesson,
    Note,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DocumentStatus {
    /// Waiting to be read, or being read; `done` of `total` passages embedded.
    Indexing {
        done: u32,
        total: u32,
    },
    Ready {
        passages: u32,
    },
    Failed {
        error: String,
    },
}

/// What a passage is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    /// An image, or a rendered PDF page.
    Picture,
    /// A video frame.
    Frame,
    /// A 10-second window of audio.
    Sound,
}

impl Modality {
    pub const ALL: [Modality; 4] = [
        Modality::Text,
        Modality::Picture,
        Modality::Frame,
        Modality::Sound,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Picture => "picture",
            Modality::Frame => "frame",
            Modality::Sound => "sound",
        }
    }
}

/// What a retrieval may search. Every scope resolves inside one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scope {
    Sources {
        ids: Vec<Id>,
    },
    Topic {
        id: Id,
    },
    Chat {
        id: Id,
    },
    Workspace {
        id: Id,
    },
    /// Every document of the project.
    Library,
    Union {
        scopes: Vec<Scope>,
    },
}

/// `POST routes::CONTEXT_SEARCH`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextQuery {
    pub text: String,
    pub scope: Scope,
    /// Empty means every modality.
    #[serde(default)]
    pub modalities: Vec<Modality>,
    /// Most hits returned.
    pub k: u32,
    /// The budget the hits are packed to (against the chat route's window), at most `k`
    /// of them; `None` packs to the pipeline's `PipelineConfig.budget`.
    #[serde(default)]
    pub budget: Option<Budget>,
    /// The caller's access paths: only passages whose `access` the caller may see are
    /// searched (a pre-filter in the index query, never a post-filter). `None` = the
    /// caller's own grants (today everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Vec<String>>,
}

/// One retrieved passage with its locator. Also the element of
/// `routes::context_document_passages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub document_id: Id,
    /// Shared by a page's text and its picture, so a hit on either finds the other.
    pub unit_id: Id,
    pub passage_id: Id,
    pub modality: Modality,
    /// "Chapter 2 > Fourier series"; empty when the document has no headings.
    #[serde(default)]
    pub section_path: String,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub start_ms: Option<u64>,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
    pub score: f32,
    /// The matching words marked, from the full-text index.
    #[serde(default)]
    pub highlight: Option<String>,
}

/// `GET routes::context_document_outline(id)`: the document's sections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentOutline {
    pub document_id: Id,
    pub sections: Vec<OutlineSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlineSection {
    pub section_path: String,
    /// The section's first and last passage (`ord`), for `context_document_passages`.
    pub from_ord: u32,
    pub to_ord: u32,
    #[serde(default)]
    pub first_page: Option<u32>,
    #[serde(default)]
    pub last_page: Option<u32>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// `POST routes::CONTEXT_INGEST`: a text document an agent adds to the project. The
/// service sets `actor` to `agent:<name>` from the origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextIngest {
    pub name: String,
    pub origin: Origin,
    #[serde(default = "text_kind")]
    pub kind: SourceKind,
    pub text: String,
}

fn text_kind() -> SourceKind {
    SourceKind::Text
}

/// `GET routes::CONTEXT_CHANGES?since=<generation>&limit=<n>`: one page of the
/// changes after `since`, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextChanges {
    /// Pass it as `since` next time: the highest generation in this page when `more`,
    /// else the highest generation now.
    pub generation: u64,
    /// Documents whose generation is above `since`, by generation.
    pub changed: Vec<Document>,
    /// Documents deleted since then.
    #[serde(default)]
    pub deleted: Vec<Id>,
    /// More changes follow this page: ask again from `generation`.
    #[serde(default)]
    pub more: bool,
}

/// Changes one page of `CONTEXT_CHANGES` lists when `limit` isn't given.
pub const CHANGES_PAGE_DEFAULT: usize = 100;
/// The most one page lists.
pub const CHANGES_PAGE_MAX: usize = 500;

// ---------------------------------------------------------------------------
// Traces (§2.11)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    ModelCall,
    Retrieval,
    ToolCall,
    Ingest,
    JobStep,
    Turn,
    /// An alert rising or clearing (`docs/phase5-brief.md` D6): `error_code` is the
    /// alert's name, `ok: false` when it rose and `true` when it cleared,
    /// `duration_ms` the value that crossed the threshold.
    Alert,
}

impl TraceKind {
    pub const ALL: [TraceKind; 7] = [
        TraceKind::ModelCall,
        TraceKind::Retrieval,
        TraceKind::ToolCall,
        TraceKind::Ingest,
        TraceKind::JobStep,
        TraceKind::Turn,
        TraceKind::Alert,
    ];

    /// The first element of a trace row's array id.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceKind::ModelCall => "model_call",
            TraceKind::Retrieval => "retrieval",
            TraceKind::ToolCall => "tool_call",
            TraceKind::Ingest => "ingest",
            TraceKind::JobStep => "job_step",
            TraceKind::Turn => "turn",
            TraceKind::Alert => "alert",
        }
    }
}

/// How a model call's history was shortened (`TraceRow.compaction`): the provider
/// compacted it server-side (its output carried a compaction item), or Margin condensed
/// it locally before the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compaction {
    Server,
    Condensed,
}

/// Token counts of one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceUsage {
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub web_searches: u64,
    /// Hosted web page fetches (Claude's `web_fetch_requests`).
    pub web_fetches: u64,
}

/// Per-leg timings of a retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalLegs {
    pub dense_ms: f64,
    pub lex_ms: f64,
    pub fuse_ms: f64,
    pub rerank_ms: f64,
}

/// One trace row: ids, numbers and hashes only. Never prompt text, output text, tool
/// output, a key or a token. Stored as `trace:[kind, at, ulid]`; every field but the
/// first five is optional so a row carries only what its kind has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceRow {
    /// The row's ulid (the last element of its record id).
    pub id: String,
    pub kind: TraceKind,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub duration_ms: f64,
    #[serde(default = "yes")]
    pub ok: bool,
    #[serde(default)]
    pub error_code: Option<String>,

    // Correlation.
    /// One per turn or job.
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<Id>,
    #[serde(default)]
    pub chat_id: Option<Id>,
    #[serde(default)]
    pub turn_id: Option<Id>,
    /// Lesson, video or proposal id.
    #[serde(default)]
    pub job_id: Option<Id>,
    #[serde(default)]
    pub document_id: Option<Id>,
    /// `owner` or `agent:<name>`.
    #[serde(default)]
    pub actor: Option<String>,
    /// The way in when it isn't the app: `mcp` for the context API as `margin-mcp`
    /// calls it (dogfood plan §6). `route` stays the model slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,

    // Configuration.
    #[serde(default)]
    pub pipeline_hash: Option<String>,
    #[serde(default)]
    pub prompt_version: Option<String>,
    #[serde(default)]
    pub route: Option<Route>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,

    // Model calls.
    #[serde(default)]
    pub usage: Option<TraceUsage>,
    /// Priced at write time from `evals/prices.toml`, so a later price change doesn't
    /// rewrite history.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub step: Option<u32>,
    /// Set on the step whose output carried a compaction item, or that followed a
    /// local condense.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,
    /// The provider's own time within `duration_ms` (first byte to last event), so
    /// `duration_ms − provider_ms` is Margin's overhead per step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_ms: Option<f64>,

    // Retrievals.
    #[serde(default)]
    pub query_hash: Option<String>,
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub legs: Option<RetrievalLegs>,
    #[serde(default)]
    pub hit_ids: Vec<String>,
    #[serde(default)]
    pub scores: Vec<f32>,
    #[serde(default)]
    pub packed_ids: Vec<String>,
    #[serde(default)]
    pub packed_tokens: Option<u64>,
    #[serde(default)]
    pub images: Option<u32>,
    #[serde(default)]
    pub cache_hit: Option<bool>,
    /// The query as asked, kept only for an agent's context-API search (`via` set,
    /// `actor` `agent:*`), at most `TRACE_QUERY_MAX` characters: what
    /// `margin-eval harvest` turns into eval candidates. App retrievals keep the hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The passages a `read_passages` call read (`[from, to]` ords, inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_range: Option<[u32; 2]>,

    // Tool calls.
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    #[serde(default)]
    pub device_id: Option<Id>,

    // Ingest.
    #[serde(default)]
    pub chunks_new: Option<u32>,
    #[serde(default)]
    pub chunks_kept: Option<u32>,
    #[serde(default)]
    pub chunks_deleted: Option<u32>,
    #[serde(default)]
    pub embed_ms: Option<f64>,
}

fn yes() -> bool {
    true
}

impl TraceRow {
    /// A row of `kind` at `at` with every optional field empty.
    pub fn new(kind: TraceKind, id: impl Into<String>, at: DateTime<Utc>) -> Self {
        Self {
            id: id.into(),
            kind,
            at,
            duration_ms: 0.0,
            ok: true,
            error_code: None,
            trace_id: String::new(),
            parent_id: None,
            project_id: None,
            chat_id: None,
            turn_id: None,
            job_id: None,
            document_id: None,
            actor: None,
            via: None,
            pipeline_hash: None,
            prompt_version: None,
            route: None,
            provider: None,
            model: None,
            usage: None,
            cost_usd: None,
            stop_reason: None,
            step: None,
            compaction: None,
            provider_ms: None,
            query_hash: None,
            scope: None,
            legs: None,
            hit_ids: Vec::new(),
            scores: Vec::new(),
            packed_ids: Vec::new(),
            packed_tokens: None,
            images: None,
            cache_hit: None,
            query: None,
            read_range: None,
            tool: None,
            output_bytes: None,
            device_id: None,
            chunks_new: None,
            chunks_kept: None,
            chunks_deleted: None,
            embed_ms: None,
        }
    }
}

/// The longest query a trace row keeps (`TraceRow::query`).
pub const TRACE_QUERY_MAX: usize = 500;

/// `via` of the context API's rows when `margin-mcp` calls it.
pub const VIA_MCP: &str = "mcp";

/// The header `margin-mcp` names its agent with on context-API calls (`claude-code`),
/// so the service's trace rows say `actor: agent:<name>`, `via: mcp`.
pub const AGENT_HEADER: &str = "x-margin-agent";

/// Whether `name` is a usable agent name: 1–40 of `[a-z0-9_-]`.
pub fn is_agent_name(name: &str) -> bool {
    (1..=40).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// The bucket of `GET routes::TRACES_ROLLUP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupBucket {
    Hour,
    Day,
}

/// A row of the `trace_hourly` / `trace_daily` views.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceRollup {
    pub kind: TraceKind,
    #[serde(default)]
    pub route: Option<Route>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Start of the hour or day.
    pub bucket: DateTime<Utc>,
    pub calls: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cached: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub mean_ms: f64,
    /// The bucket's 95th percentile of `duration_ms`; `None` where the store can't
    /// compute it (rollups kept from deleted rows, or before phase 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Alerts (`docs/phase5-brief.md` D6)
// ---------------------------------------------------------------------------

/// A threshold from `[alerts]` in `service.toml` that the last window crossed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// `error_rate`, `p95_ms`, `rate_limited_share`, `ingest_backlog`, `rss_bytes`,
    /// `http_5xx`.
    pub name: String,
    /// The trace kind it's about; `None` for the service-wide ones (backlog, memory,
    /// HTTP errors).
    #[serde(default)]
    pub kind: Option<TraceKind>,
    pub value: f64,
    pub threshold: f64,
    /// When it rose.
    pub since: DateTime<Utc>,
    /// The last tick that still saw it crossed.
    pub last_seen: DateTime<Utc>,
}

/// An alert rising (`raised: true`) or clearing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertEvent {
    pub alert: Alert,
    pub raised: bool,
    pub at: DateTime<Utc>,
}

/// `GET routes::ALERTS`: the alerts now active and the last rises and clears, newest
/// first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertList {
    #[serde(default)]
    pub active: Vec<Alert>,
    #[serde(default)]
    pub recent: Vec<AlertEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap()
    }

    fn round_trip<T>(value: &T) -> serde_json::Value
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, value);
        json
    }

    #[test]
    fn documents_and_origins_round_trip() {
        for origin in [
            Origin::Source {
                source_id: "s".into(),
            },
            Origin::FolderFile {
                workspace_id: "w".into(),
                path: "notes/a.md".into(),
            },
            Origin::Library {
                kind: LibraryKind::Lesson,
                id: "l".into(),
            },
            Origin::Agent {
                name: "codex".into(),
            },
        ] {
            let doc = Document {
                id: "d".into(),
                project_id: "p".into(),
                origin,
                name: "a.md".into(),
                kind: SourceKind::Text,
                status: DocumentStatus::Ready { passages: 3 },
                generation: 2,
                bytes: 10,
                actor: "owner".into(),
                access: vec!["company/engineering".into()],
                created_at: at(),
                updated_at: at(),
            };
            round_trip(&doc);
        }
        let old: Document = serde_json::from_value(json!({
            "id": "d", "project_id": "p", "origin": {"origin": "agent", "name": "x"},
            "name": "n", "kind": "text", "status": {"state": "ready", "passages": 1},
            "generation": 1, "bytes": 1,
            "created_at": "2025-10-09T08:53:20Z", "updated_at": "2025-10-09T08:53:20Z"
        }))
        .unwrap();
        assert_eq!(old.access, vec![ACCESS_COMPANY.to_string()]);
        assert_eq!(old.actor, "owner");
        let json = round_trip(&Origin::FolderFile {
            workspace_id: "w".into(),
            path: "a".into(),
        });
        assert_eq!(
            json,
            json!({"origin": "folder_file", "workspace_id": "w", "path": "a"})
        );
    }

    #[test]
    fn queries_scopes_and_hits_round_trip() {
        let q = ContextQuery {
            text: "carnot efficiency".into(),
            scope: Scope::Union {
                scopes: vec![
                    Scope::Chat { id: "c".into() },
                    Scope::Topic { id: "t".into() },
                ],
            },
            modalities: vec![Modality::Text, Modality::Picture],
            k: 12,
            budget: Some(Budget::default()),
            access: Some(vec!["company/engineering".into()]),
        };
        let json = round_trip(&q);
        assert_eq!(json["scope"]["scope"], "union");
        let minimal: ContextQuery =
            serde_json::from_value(json!({"text": "x", "scope": {"scope": "library"}, "k": 5}))
                .unwrap();
        assert!(minimal.modalities.is_empty());
        assert_eq!(minimal.budget, None);
        assert_eq!(minimal.access, None);
        let hit: Hit = serde_json::from_value(json!({
            "document_id": "d", "unit_id": "u", "passage_id": "p", "modality": "text",
            "text": "t", "score": 0.5
        }))
        .unwrap();
        assert_eq!(hit.page, None);
        round_trip(&hit);
    }

    #[test]
    fn trace_rows_are_additive() {
        let mut row = TraceRow::new(TraceKind::ModelCall, "01J0", at());
        row.route = Some(Route::Small);
        row.usage = Some(TraceUsage {
            input: 100,
            cached: 60,
            output: 5,
            ..Default::default()
        });
        row.cost_usd = Some(0.0001);
        let json = round_trip(&row);
        assert_eq!(json["kind"], "model_call");
        // A row with only the required fields parses.
        let bare: TraceRow = serde_json::from_value(
            json!({"id": "x", "kind": "retrieval", "at": "2025-10-09T08:53:20Z"}),
        )
        .unwrap();
        assert!(bare.ok);
        assert!(bare.hit_ids.is_empty());
        assert_eq!(TraceKind::ALL.map(TraceKind::as_str)[0], "model_call");
        round_trip(&TraceRollup {
            kind: TraceKind::ModelCall,
            route: Some(Route::Chat),
            provider: Some("openai".into()),
            model: Some("m".into()),
            bucket: at(),
            calls: 3,
            input: 10,
            cached: 2,
            output: 1,
            cost_usd: 0.5,
            mean_ms: 12.5,
            p95_ms: Some(40.0),
        });
    }

    #[test]
    fn phase5_trace_fields_are_additive() {
        let mut row = TraceRow::new(TraceKind::ModelCall, "01J1", at());
        row.compaction = Some(Compaction::Condensed);
        row.provider_ms = Some(812.5);
        let json = round_trip(&row);
        assert_eq!(json["compaction"], "condensed");
        let plain = serde_json::to_value(TraceRow::new(TraceKind::Turn, "x", at())).unwrap();
        assert!(plain.get("compaction").is_none());
        assert!(plain.get("provider_ms").is_none());
        // A row written before phase 5 parses without them.
        let old: TraceRow = serde_json::from_value(json!({
            "id": "x", "kind": "model_call", "at": "2025-10-09T08:53:20Z",
            "duration_ms": 10.0, "step": 2
        }))
        .unwrap();
        assert_eq!((old.compaction, old.provider_ms), (None, None));
        assert_eq!(TraceKind::Alert.as_str(), "alert");
        assert_eq!(serde_json::to_value(TraceKind::Alert).unwrap(), "alert");
        assert!(TraceKind::ALL.contains(&TraceKind::Alert));
        let old_rollup: TraceRollup = serde_json::from_value(json!({
            "kind": "retrieval", "bucket": "2025-10-09T08:00:00Z", "calls": 2
        }))
        .unwrap();
        assert_eq!(old_rollup.p95_ms, None);
    }

    #[test]
    fn alerts_round_trip() {
        let alert = Alert {
            name: "error_rate".into(),
            kind: Some(TraceKind::ModelCall),
            value: 0.4,
            threshold: 0.2,
            since: at(),
            last_seen: at(),
        };
        let json = round_trip(&alert);
        assert_eq!(json["kind"], "model_call");
        round_trip(&AlertList {
            active: vec![alert.clone()],
            recent: vec![AlertEvent {
                alert: Alert {
                    kind: None,
                    ..alert
                },
                raised: true,
                at: at(),
            }],
        });
        let empty: AlertList = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, AlertList::default());
    }
}
