//! Storage on embedded SurrealDB.
//!
//! Every record is stored as `{ doc: <the record as JSON> }` under the record id
//! `table:⟨id⟩`. Keeping the record nested under `doc` lets the wire types (whose own
//! `id` field would clash with SurrealDB's record id) round-trip unchanged through
//! serde. Libraries are personal-sized, so most filtering happens in Rust.
//!
//! The retrieval tables are different (`docs/agent-rag-architecture.md` §2.2, §2.11):
//! `passage` and `trace` are SCHEMAFULL with their fields at the top level, so the
//! HNSW, FULLTEXT and plain indexes can name them, and `trace` rows have array ids
//! `trace:[kind, at, ulid]` so a kind and time window is a record-range scan.
//! `trace_hourly`/`trace_daily` are table views SurrealDB keeps current on every write.

use std::future::IntoFuture as _;
use std::path::Path;

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use surrealdb::types::RecordId;

pub const TOPIC: &str = "topic";
pub const LESSON: &str = "lesson";
pub const NOTE: &str = "note";
pub const VIDEO: &str = "video";
pub const PROPOSAL: &str = "proposal";
pub const DEVICE: &str = "device";
pub const SETTINGS: &str = "settings";
pub const SOURCE: &str = "source";
/// Embedded pieces of sources (`sources::ChunkRow`).
pub const CHUNK: &str = "chunk";
/// Embedded pictures and sound of sources (`sources::MediaRow`).
pub const MEDIA: &str = "media";
/// Chats with the agent (`margin_shared::Chat`).
pub const CHAT: &str = "chat";
/// Chat transcripts (`chats::StoredChatItem`), with `chat_id`.
pub const CHAT_ITEM: &str = "chat_item";
/// Attached folders (`margin_shared::Workspace`).
pub const WORKSPACE: &str = "workspace";
/// What the service knows about each path of an attached folder
/// (`folders::FolderFileRow`), keyed by `folders::row_key`.
pub const FOLDER_FILE: &str = "folder_file";
/// Corpora (`margin_shared::context::Project`), `{ doc }` like the tables above.
pub const PROJECT: &str = "project";
/// Indexed documents (`margin_shared::context::Document`), `{ doc }`.
pub const DOCUMENT: &str = "document";
/// Tombstones of deleted documents (`ingest::Tombstone`), `{ doc }`, for the context
/// API's change feed.
pub const DOCUMENT_DELETED: &str = "document_deleted";
/// Retrievable passages (`PassageRow`): SCHEMAFULL, top-level fields, indexed.
pub const PASSAGE: &str = "passage";
/// Trace rows (`margin_shared::context::TraceRow`): SCHEMAFULL, ids `[kind, at, ulid]`.
pub const TRACE: &str = "trace";
/// Hourly and daily rollups of `trace`, as table views.
pub const TRACE_HOURLY: &str = "trace_hourly";
pub const TRACE_DAILY: &str = "trace_daily";
/// The version of the stored data's shape this build writes (`SettingsRow.schema_version`):
/// bumped when a `DEFINE` or a one-pass migration changes. A store with a higher one
/// refuses to serve; a lower one is backed up, then migrated (`docs/phase5-brief.md` D5).
pub const SCHEMA_VERSION: u32 = 2;

/// Bookkeeping of this module: the fingerprint of the index definitions last applied.
const SCHEMA_META: &str = "schema_meta";

/// The tables of sources' vectors before the `passage` table: defined until the one-pass
/// migration (`ingest::migrate`) removes them for good (`Store::remove_table`).
const OLD_TABLES: [&str; 2] = [CHUNK, MEDIA];

/// Rollup sums `trace::apply_retention` copies out of the views before it deletes raw
/// rows, so the hourly and daily numbers outlive them (R1-trace).
pub const TRACE_HOURLY_KEPT: &str = "trace_hourly_kept";
pub const TRACE_DAILY_KEPT: &str = "trace_daily_kept";
/// Everyone who may sign in (`people::PersonRow`, L1).
pub const PERSON: &str = "person";
/// Groups (`groups::GroupRow`), keyed by path.
pub const GROUP: &str = "group";
/// Direct memberships (`groups::MembershipRow`): `person_id`, `group_path`, `source`.
pub const MEMBERSHIP: &str = "membership";
/// Agents (`agents::AgentRow`) and their hashed tokens.
pub const AGENT: &str = "agent";
/// Invitations by GitHub login (`invitations::InvitationRow`).
pub const INVITATION: &str = "invitation";

const TABLES: [&str; 22] = [
    TOPIC,
    LESSON,
    NOTE,
    VIDEO,
    PROPOSAL,
    DEVICE,
    SETTINGS,
    SOURCE,
    CHAT,
    CHAT_ITEM,
    WORKSPACE,
    FOLDER_FILE,
    PROJECT,
    DOCUMENT,
    DOCUMENT_DELETED,
    TRACE_HOURLY_KEPT,
    TRACE_DAILY_KEPT,
    PERSON,
    GROUP,
    MEMBERSHIP,
    AGENT,
    INVITATION,
];

/// The `passage` and `trace` tables and their plain indexes. Idempotent (`IF NOT
/// EXISTS`); a field added later is one more line here.
const SCHEMA: &str = r#"
DEFINE TABLE IF NOT EXISTS schema_meta SCHEMALESS;
DEFINE INDEX IF NOT EXISTS document_project ON document FIELDS doc.project_id;
DEFINE INDEX IF NOT EXISTS document_access  ON document FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS document_deny    ON document FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS document_generation ON document FIELDS doc.generation;
DEFINE INDEX IF NOT EXISTS document_deleted_generation ON document_deleted FIELDS doc.generation;
DEFINE INDEX IF NOT EXISTS folder_file_workspace ON folder_file FIELDS doc.workspace_id;
DEFINE INDEX IF NOT EXISTS source_workspace ON source FIELDS doc.workspace_id;

DEFINE INDEX IF NOT EXISTS person_login      ON person FIELDS doc.login_key UNIQUE;
DEFINE INDEX IF NOT EXISTS person_handle     ON person FIELDS doc.handle UNIQUE;
DEFINE INDEX IF NOT EXISTS agent_token       ON agent FIELDS doc.token_hash;
DEFINE INDEX IF NOT EXISTS agent_owner       ON agent FIELDS doc.owner_id;
DEFINE INDEX IF NOT EXISTS device_person     ON device FIELDS doc.person_id;
DEFINE INDEX IF NOT EXISTS membership_person ON membership FIELDS doc.person_id;
DEFINE INDEX IF NOT EXISTS membership_group  ON membership FIELDS doc.group_path;
DEFINE INDEX IF NOT EXISTS invitation_login  ON invitation FIELDS doc.login_key;
DEFINE INDEX IF NOT EXISTS topic_access      ON topic FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS lesson_access     ON lesson FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS note_access       ON note FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS chat_access       ON chat FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS source_access     ON source FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS workspace_access  ON workspace FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS proposal_access   ON proposal FIELDS doc.access.*;
DEFINE INDEX IF NOT EXISTS topic_deny        ON topic FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS lesson_deny       ON lesson FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS note_deny         ON note FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS chat_deny         ON chat FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS source_deny       ON source FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS workspace_deny    ON workspace FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS proposal_deny     ON proposal FIELDS doc.deny.*;
DEFINE INDEX IF NOT EXISTS document_owner    ON document FIELDS doc.owner;

DEFINE TABLE IF NOT EXISTS passage SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS project_id   ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS document_id  ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS origin       ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS scope_ids    ON passage TYPE array<string>;
DEFINE FIELD IF NOT EXISTS access       ON passage TYPE array<string> DEFAULT [];
DEFINE FIELD IF NOT EXISTS deny         ON passage TYPE array<string> DEFAULT [];
DEFINE FIELD IF NOT EXISTS unit_id      ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS modality     ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS ord          ON passage TYPE int;
DEFINE FIELD IF NOT EXISTS page         ON passage TYPE option<int>;
DEFINE FIELD IF NOT EXISTS start_line   ON passage TYPE option<int>;
DEFINE FIELD IF NOT EXISTS end_line     ON passage TYPE option<int>;
DEFINE FIELD IF NOT EXISTS start_ms     ON passage TYPE option<int>;
DEFINE FIELD IF NOT EXISTS end_ms       ON passage TYPE option<int>;
DEFINE FIELD IF NOT EXISTS section_path ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS text         ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS file         ON passage TYPE option<string>;
DEFINE FIELD IF NOT EXISTS chunk_hash   ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS embedder     ON passage TYPE string;
DEFINE FIELD IF NOT EXISTS vector       ON passage TYPE array<float>;
DEFINE INDEX IF NOT EXISTS passage_document ON passage FIELDS document_id;
DEFINE INDEX IF NOT EXISTS passage_scope    ON passage FIELDS scope_ids.*;
DEFINE INDEX IF NOT EXISTS passage_access   ON passage FIELDS access.*;
DEFINE INDEX IF NOT EXISTS passage_deny     ON passage FIELDS deny.*;
DEFINE INDEX IF NOT EXISTS passage_hash     ON passage FIELDS document_id, chunk_hash UNIQUE;

DEFINE TABLE IF NOT EXISTS trace SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS kind           ON trace TYPE string;
DEFINE FIELD IF NOT EXISTS at             ON trace TYPE datetime;
DEFINE FIELD IF NOT EXISTS duration_ms    ON trace TYPE number DEFAULT 0;
DEFINE FIELD IF NOT EXISTS ok             ON trace TYPE bool DEFAULT true;
DEFINE FIELD IF NOT EXISTS error_code     ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS trace_id       ON trace TYPE string DEFAULT '';
DEFINE FIELD IF NOT EXISTS parent_id      ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS project_id     ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS chat_id        ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS turn_id        ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS job_id         ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS document_id    ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS actor          ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS principal      ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS pipeline_hash  ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS prompt_version ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS route          ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS provider       ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS model          ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS usage          ON trace TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS cost_usd       ON trace TYPE option<number>;
DEFINE FIELD IF NOT EXISTS stop_reason    ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS step           ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS query_hash     ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS scope          ON trace TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS legs           ON trace TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS hit_ids        ON trace TYPE array<string> DEFAULT [];
DEFINE FIELD IF NOT EXISTS scores         ON trace TYPE array<number> DEFAULT [];
DEFINE FIELD IF NOT EXISTS packed_ids     ON trace TYPE array<string> DEFAULT [];
DEFINE FIELD IF NOT EXISTS packed_tokens  ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS images         ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS cache_hit      ON trace TYPE option<bool>;
DEFINE FIELD IF NOT EXISTS tool           ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS output_bytes   ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS device_id      ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS chunks_new     ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS chunks_kept    ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS chunks_deleted ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS embed_ms       ON trace TYPE option<number>;
DEFINE FIELD IF NOT EXISTS via            ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS query          ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS read_range     ON trace TYPE option<array<int>>;
DEFINE FIELD IF NOT EXISTS queries        ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS reranked       ON trace TYPE option<int>;
DEFINE FIELD IF NOT EXISTS reranker       ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS compaction     ON trace TYPE option<string>;
DEFINE FIELD IF NOT EXISTS provider_ms    ON trace TYPE option<number>;
DEFINE INDEX IF NOT EXISTS trace_trace_id ON trace FIELDS trace_id;
"#;

/// The analyzer and the FULLTEXT indexes ⌘K and `search_library` run on (lessons' and
/// notes' title, body and quote; analyzer `library`, every word's beginnings). Applied
/// at every open; `IF NOT EXISTS` makes that a catalog lookup once they exist.
pub const LIBRARY_INDEXES: &str = r#"
DEFINE ANALYZER IF NOT EXISTS library TOKENIZERS blank, class FILTERS lowercase, ascii, edgengram(1, 12);
DEFINE INDEX IF NOT EXISTS lesson_title ON lesson FIELDS doc.title FULLTEXT ANALYZER library BM25;
DEFINE INDEX IF NOT EXISTS lesson_body  ON lesson FIELDS doc.body_md FULLTEXT ANALYZER library BM25;
DEFINE INDEX IF NOT EXISTS note_title   ON note FIELDS doc.title FULLTEXT ANALYZER library BM25;
DEFINE INDEX IF NOT EXISTS note_body    ON note FIELDS doc.body_md FULLTEXT ANALYZER library BM25;
DEFINE INDEX IF NOT EXISTS note_quote   ON note FIELDS doc.quote FULLTEXT ANALYZER library BM25;
"#;

/// The analyzer, the vector and full-text indexes, and the rollup views: defined with
/// `OVERWRITE`, and applied only when this text changes (an HNSW or analyzer change
/// rebuilds the index once, at the next start).
const INDEXES: &str = r#"
DEFINE ANALYZER OVERWRITE prose TOKENIZERS blank, class FILTERS lowercase, ascii, snowball(english);
DEFINE INDEX OVERWRITE passage_text ON passage FIELDS text FULLTEXT ANALYZER prose BM25 HIGHLIGHTS;
DEFINE INDEX OVERWRITE passage_vector ON passage FIELDS vector HNSW DIMENSION 768 DIST COSINE TYPE F32 EFC 200 M 16;
DEFINE TABLE OVERWRITE trace_hourly TYPE NORMAL AS
  SELECT kind, route, provider, model, time::floor(at, 1h) AS bucket, count() AS calls,
    math::sum(usage.input ?? 0) AS input, math::sum(usage.cached ?? 0) AS cached,
    math::sum(usage.output ?? 0) AS output, math::sum(cost_usd ?? 0) AS cost_usd,
    math::mean(duration_ms) AS mean_ms
  FROM trace GROUP BY kind, route, provider, model, bucket;
DEFINE TABLE OVERWRITE trace_daily TYPE NORMAL AS
  SELECT kind, route, provider, model, time::floor(at, 1d) AS bucket, count() AS calls,
    math::sum(usage.input ?? 0) AS input, math::sum(usage.cached ?? 0) AS cached,
    math::sum(usage.output ?? 0) AS output, math::sum(cost_usd ?? 0) AS cost_usd,
    math::mean(duration_ms) AS mean_ms
  FROM trace GROUP BY kind, route, provider, model, bucket;
"#;

/// Field definitions that change on an existing store (`OVERWRITE`), applied only when
/// this text changes, like `INDEXES` but apart from it so a field's default never
/// rebuilds the vector or full-text index. L2: a passage written without an audience
/// is visible to nobody (`[]`), not to everyone (`['company']`, before L2).
const FIELDS: &str = r#"
DEFINE FIELD OVERWRITE access ON passage TYPE array<string> DEFAULT [];
"#;

/// How often `Store::query_rows` retries a statement SurrealDB says can be retried.
const CONFLICT_RETRIES: u32 = 5;

/// A transaction conflict SurrealDB says can be retried.
fn is_retryable(e: &surrealdb::Error) -> bool {
    e.to_string().contains("can be retried")
}

/// A record that knows its own key (for `Store::put_many`).
pub trait HasKey {
    fn key(&self) -> String;
}

/// The most rows one `insert_many` statement writes.
pub const INSERT_MANY_MAX: usize = 64;

/// One retrievable piece of a document: a text chunk, a picture (an image, a PDF page),
/// a video frame or a window of sound. Stored in `passage` with these fields at the top
/// level; `id` is the record key. `None` fields are left out when written (SurrealDB
/// tells `NONE` from JSON `null`, and `option<…>` fields take only `NONE`).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct PassageRow {
    pub id: String,
    pub project_id: String,
    pub document_id: String,
    /// "source", "folder", "library" or "agent".
    pub origin: String,
    /// What may search it: topic id, chat ids, workspace id.
    pub scope_ids: Vec<String>,
    /// The document's access paths ("company/engineering", "person/<id>"), copied at
    /// ingest; retrieval pre-filters on them inside the index query
    /// (`access CONTAINSANY $paths`, exact membership). Empty: visible to nobody.
    #[serde(default)]
    pub access: Vec<String>,
    /// The document's denials, copied with `access`; retrieval requires
    /// `deny CONTAINSNONE $deny` in the same scan.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Shared by a page's text and its picture.
    pub unit_id: String,
    /// `margin_shared::context::Modality` as text.
    pub modality: String,
    pub ord: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
    pub section_path: String,
    /// The chunk's text; a picture's OCR or page text; what was said around a frame.
    pub text: String,
    /// A picture's media file name ("page-3.png"), rendered on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub chunk_hash: String,
    /// Model and prefix version; rows with another version are re-embedded.
    pub embedder: String,
    pub vector: Vec<f32>,
}

/// One element of an array record id, for `Store::range`.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyPart {
    Str(String),
    Int(i64),
    Time(chrono::DateTime<chrono::Utc>),
}

#[derive(Clone)]
pub struct Store {
    db: Surreal<Db>,
}

impl Store {
    /// Opens (or creates) the on-disk database in `dir`.
    pub async fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("couldn't create the database folder {}", dir.display()))?;
        let db = Surreal::new::<SurrealKv>(dir.to_path_buf())
            .await
            .context("couldn't open the database")?;
        Self::init(db).await
    }

    /// An in-memory database, for tests.
    pub async fn memory() -> anyhow::Result<Self> {
        let db = Surreal::new::<Mem>(())
            .await
            .context("couldn't start the in-memory database")?;
        Self::init(db).await
    }

    /// Opens the on-disk database in `dir` as it is: no `DEFINE`, no index or view
    /// change, no row written. For reading the stored schema version before this build
    /// decides whether it may migrate the data (`restore::open_guarded`), and for
    /// checking a copy of a backup (`restore::check_data`). `None` when `dir` holds no
    /// Margin database (no namespace and database `margin`).
    pub async fn open_existing(dir: &Path) -> anyhow::Result<Option<Self>> {
        let db = Surreal::new::<SurrealKv>(dir.to_path_buf())
            .await
            .context("couldn't open the database")?;
        let mut res = db
            .query("INFO FOR ROOT")
            .await
            .context("couldn't read the database's namespaces")?
            .check()
            .context("couldn't read the database's namespaces")?;
        let root: surrealdb::types::Value = res.take(0)?;
        if !has_key(&root.into_json_value(), "namespaces", "margin") {
            return Ok(None);
        }
        db.use_ns("margin")
            .await
            .context("couldn't select the namespace")?;
        let mut res = db
            .query("INFO FOR NS")
            .await
            .context("couldn't read the namespace")?
            .check()
            .context("couldn't read the namespace")?;
        let ns: surrealdb::types::Value = res.take(0)?;
        if !has_key(&ns.into_json_value(), "databases", "margin") {
            return Ok(None);
        }
        db.use_ns("margin")
            .use_db("margin")
            .await
            .context("couldn't select the database")?;
        Ok(Some(Self { db }))
    }

    /// The tables defined in the database.
    pub async fn table_names(&self) -> anyhow::Result<Vec<String>> {
        let info: Vec<serde_json::Value> = self.query_rows("INFO FOR DB", vec![]).await?;
        let mut names: Vec<String> = info
            .first()
            .and_then(|i| i.get("tables"))
            .and_then(|t| t.as_object())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    /// The settings row's `schema_version` as stored: `None` without a settings row (a
    /// new store), 0 for a row written before the field existed.
    pub async fn stored_schema_version(&self) -> anyhow::Result<Option<u32>> {
        if !self.table_names().await?.iter().any(|t| t == SETTINGS) {
            return Ok(None);
        }
        let Some(found) = self
            .get::<serde_json::Value>(SETTINGS, crate::state::SETTINGS_ID)
            .await?
        else {
            return Ok(None);
        };
        let version = found
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(Some(u32::try_from(version).unwrap_or(u32::MAX)))
    }

    /// Stamps `SCHEMA_VERSION` on the settings row when there is one (a new store gets
    /// it with its first settings row, `SettingsRow::default`).
    pub async fn record_schema_version(&self) -> anyhow::Result<()> {
        if self
            .get::<serde_json::Value>(SETTINGS, crate::state::SETTINGS_ID)
            .await?
            .is_none()
        {
            return Ok(());
        }
        self.retrying(|| {
            self.db
                .query("UPDATE $rid SET doc.schema_version = $v RETURN NONE")
                .bind((
                    "rid",
                    RecordId::new(SETTINGS, crate::state::SETTINGS_ID.to_string()),
                ))
                .bind(("v", SCHEMA_VERSION))
                .into_future()
        })
        .await
        .context("couldn't record the schema version")?;
        Ok(())
    }

    async fn init(db: Surreal<Db>) -> anyhow::Result<Self> {
        db.query("DEFINE NAMESPACE IF NOT EXISTS margin; USE NS margin; DEFINE DATABASE IF NOT EXISTS margin;")
            .await
            .context("couldn't define the namespace")?
            .check()
            .context("couldn't define the namespace")?;
        db.use_ns("margin")
            .use_db("margin")
            .await
            .context("couldn't select the database")?;
        let mut defs = String::new();
        for t in TABLES {
            defs.push_str(&format!("DEFINE TABLE IF NOT EXISTS {t} SCHEMALESS;"));
        }
        defs.push_str(SCHEMA);
        db.query(defs)
            .await
            .context("couldn't define tables")?
            .check()
            .context("couldn't define tables")?;
        let store = Self { db };
        store.define_old_tables().await?;
        store.apply_fields().await?;
        store.apply_indexes().await?;
        store
            .db
            .query(LIBRARY_INDEXES)
            .await
            .context("couldn't define the library indexes")?
            .check()
            .context("couldn't define the library indexes")?;
        Ok(store)
    }

    /// Defines `chunk` and `media` unless `remove_table` retired them: a store from
    /// before the `passage` table, or a new one, has them until the migration is done
    /// (reading a table that doesn't exist is an error).
    async fn define_old_tables(&self) -> anyhow::Result<()> {
        let retired = self.retired().await?;
        for t in OLD_TABLES {
            if !retired.iter().any(|r| r == t) {
                self.db
                    .query(format!("DEFINE TABLE IF NOT EXISTS {t} SCHEMALESS"))
                    .await?
                    .check()
                    .with_context(|| format!("couldn't define {t}"))?;
            }
        }
        Ok(())
    }

    /// Tables `remove_table` removed for good.
    async fn retired(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .get::<Vec<String>>(SCHEMA_META, "retired")
            .await?
            .unwrap_or_default())
    }

    /// Runs `FIELDS` when its text differs from the last one applied.
    async fn apply_fields(&self) -> anyhow::Result<()> {
        let fingerprint = crate::state::sha256_hex(FIELDS);
        let applied: Option<String> = self.get(SCHEMA_META, "fields").await?;
        if applied.as_deref() == Some(fingerprint.as_str()) {
            return Ok(());
        }
        self.db
            .query(FIELDS)
            .await
            .context("couldn't define the fields")?
            .check()
            .context("couldn't define the fields")?;
        self.put(SCHEMA_META, "fields", &fingerprint).await
    }

    /// Runs `INDEXES` when its text differs from the last one applied.
    async fn apply_indexes(&self) -> anyhow::Result<()> {
        let fingerprint = crate::state::sha256_hex(INDEXES);
        let applied: Option<String> = self.get(SCHEMA_META, "indexes").await?;
        if applied.as_deref() == Some(fingerprint.as_str()) {
            return Ok(());
        }
        self.db
            .query(INDEXES)
            .await
            .context("couldn't define the indexes")?
            .check()
            .context("couldn't define the indexes")?;
        self.put(SCHEMA_META, "indexes", &fingerprint).await
    }

    /// Writes up to `INSERT_MANY_MAX` rows in one statement (`INSERT INTO table $rows`);
    /// each row's `id` field, when present, is its record key. More rows are an error:
    /// callers batch, taking the write lock per statement.
    pub async fn insert_many<T: Serialize>(
        &self,
        table: &'static str,
        rows: &[T],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            rows.len() <= INSERT_MANY_MAX,
            "insert_many takes at most {INSERT_MANY_MAX} rows, not {}",
            rows.len()
        );
        if rows.is_empty() {
            return Ok(());
        }
        let rows = serde_json::to_value(rows)?;
        self.retrying(|| {
            self.db
                .query(format!("INSERT INTO {table} $rows RETURN NONE"))
                .bind(("rows", rows.clone()))
                .into_future()
        })
        .await
        .with_context(|| format!("couldn't insert into {table}"))?;
        Ok(())
    }

    /// Records of `table` whose array ids lie between `from` and `to` (both inclusive),
    /// in id order: a record-range scan, no index needed. Each row's `id` is the record
    /// id as SurrealQL text; `key` is the id's array.
    pub async fn range<T: DeserializeOwned>(
        &self,
        table: &'static str,
        from: &[KeyPart],
        to: &[KeyPart],
    ) -> anyhow::Result<Vec<T>> {
        let (range, binds) = range_expr(table, from, to);
        let mut q = self
            .db
            .query(format!("SELECT *, record::id(id) AS key FROM {range}"));
        for (name, part) in binds {
            q = bind_part(q, name, part);
        }
        let mut res = q.await?.check()?;
        let rows: Vec<serde_json::Value> = res.take(0)?;
        rows.into_iter()
            .map(|v| serde_json::from_value(v).with_context(|| format!("bad {table} record")))
            .collect()
    }

    /// Deletes the records of `table` whose array ids lie between `from` and `to` (both
    /// inclusive).
    pub async fn delete_range(
        &self,
        table: &'static str,
        from: &[KeyPart],
        to: &[KeyPart],
    ) -> anyhow::Result<()> {
        let (range, binds) = range_expr(table, from, to);
        self.retrying(|| {
            let mut q = self.db.query(format!("DELETE {range} RETURN NONE"));
            for (name, part) in &binds {
                q = bind_part(q, name.clone(), part);
            }
            q.into_future()
        })
        .await?;
        Ok(())
    }

    /// Runs a query built by `build` (again for each attempt), retrying a conflict
    /// SurrealDB says can be retried. The passage indexes (FULLTEXT, HNSW) keep shared
    /// keys that SurrealDB's own background index work also writes, so any statement,
    /// a plain `DELETE` of a chat or topic included, can collide with it. Every write
    /// here holds the service's write lock, so retrying is safe.
    async fn retrying<F, Fut>(&self, mut build: F) -> surrealdb::Result<surrealdb::IndexedResults>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = surrealdb::Result<surrealdb::IndexedResults>>,
    {
        let mut attempt = 0;
        loop {
            match build().await.and_then(|r| r.check()) {
                Ok(res) => return Ok(res),
                Err(e) if attempt < CONFLICT_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    tracing::debug!("retrying after a transaction conflict ({attempt}): {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(10 * attempt as u64)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Runs SurrealQL with bound parameters and returns the rows of its last statement.
    /// Table names in `sql` must be this module's constants, never input.
    pub async fn query_rows<T: DeserializeOwned>(
        &self,
        sql: &str,
        binds: Vec<(&'static str, serde_json::Value)>,
    ) -> anyhow::Result<Vec<T>> {
        let mut res = self
            .retrying(|| {
                let mut q = self.db.query(sql.to_string());
                for (name, value) in &binds {
                    q = q.bind((*name, value.clone()));
                }
                q.into_future()
            })
            .await?;
        let last = res.num_statements().saturating_sub(1);
        let value: surrealdb::types::Value = res.take(last)?;
        let rows = match value.into_json_value() {
            serde_json::Value::Array(rows) => rows,
            serde_json::Value::Null => Vec::new(),
            one => vec![one],
        };
        rows.into_iter()
            .map(|v| serde_json::from_value(v).context("bad query row"))
            .collect()
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        table: &'static str,
        id: &str,
    ) -> anyhow::Result<Option<T>> {
        let mut res = self
            .db
            .query("SELECT VALUE doc FROM $rid")
            .bind(("rid", RecordId::new(table, id.to_string())))
            .await?
            .check()?;
        let rows: Vec<serde_json::Value> = res.take(0)?;
        match rows.into_iter().next() {
            Some(v) if !v.is_null() => Ok(Some(
                serde_json::from_value(v).with_context(|| format!("bad {table} record"))?,
            )),
            _ => Ok(None),
        }
    }

    pub async fn list<T: DeserializeOwned>(&self, table: &'static str) -> anyhow::Result<Vec<T>> {
        // `table` is always one of the constants above, never user input.
        let mut res = self
            .db
            .query(format!("SELECT VALUE doc FROM {table}"))
            .await?
            .check()?;
        let rows: Vec<serde_json::Value> = res.take(0)?;
        rows.into_iter()
            .map(|v| serde_json::from_value(v).with_context(|| format!("bad {table} record")))
            .collect()
    }

    /// Records whose top-level JSON field `field` equals `value`.
    pub async fn list_where<T: DeserializeOwned>(
        &self,
        table: &'static str,
        field: &'static str,
        value: &str,
    ) -> anyhow::Result<Vec<T>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT VALUE doc FROM {table} WHERE doc.{field} = $value"
            ))
            .bind(("value", value.to_string()))
            .await?
            .check()?;
        let rows: Vec<serde_json::Value> = res.take(0)?;
        rows.into_iter()
            .map(|v| serde_json::from_value(v).with_context(|| format!("bad {table} record")))
            .collect()
    }

    /// Inserts or replaces a record.
    pub async fn put<T: Serialize>(
        &self,
        table: &'static str,
        id: &str,
        value: &T,
    ) -> anyhow::Result<()> {
        let doc = serde_json::to_value(value)?;
        self.retrying(|| {
            self.db
                .query("UPSERT $rid CONTENT { doc: $doc } RETURN NONE")
                .bind(("rid", RecordId::new(table, id.to_string())))
                .bind(("doc", doc.clone()))
                .into_future()
        })
        .await?;
        Ok(())
    }

    /// Inserts or replaces records `{ doc }` keyed by `key(row)`, at most
    /// `INSERT_MANY_MAX` per statement. Call with the write lock held.
    pub async fn put_many<T: Serialize + HasKey>(
        &self,
        table: &'static str,
        rows: &[T],
    ) -> anyhow::Result<()> {
        for batch in rows.chunks(INSERT_MANY_MAX) {
            let records = batch
                .iter()
                .map(|r| Ok(serde_json::json!({ "id": r.key(), "doc": serde_json::to_value(r)? })))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let records = serde_json::Value::Array(records);
            self.retrying(|| {
                self.db
                    .query(format!(
                        "INSERT INTO {table} $rows ON DUPLICATE KEY UPDATE doc = $input.doc RETURN NONE"
                    ))
                    .bind(("rows", records.clone()))
                    .into_future()
            })
            .await
            .with_context(|| format!("couldn't write {table}"))?;
        }
        Ok(())
    }

    pub async fn delete(&self, table: &'static str, id: &str) -> anyhow::Result<()> {
        self.retrying(|| {
            self.db
                .query("DELETE $rid")
                .bind(("rid", RecordId::new(table, id.to_string())))
                .into_future()
        })
        .await?;
        Ok(())
    }

    /// Deletes every record whose top-level JSON field `field` equals `value`.
    pub async fn delete_where(
        &self,
        table: &'static str,
        field: &'static str,
        value: &str,
    ) -> anyhow::Result<()> {
        self.retrying(|| {
            self.db
                .query(format!(
                    "DELETE {table} WHERE doc.{field} = $value RETURN NONE"
                ))
                .bind(("value", value.to_string()))
                .into_future()
        })
        .await?;
        Ok(())
    }

    /// Sets the top-level JSON field `set` to `to` on every record whose field `field`
    /// equals `value`.
    pub async fn set_where<T: Serialize>(
        &self,
        table: &'static str,
        field: &'static str,
        value: &str,
        set: &'static str,
        to: &T,
    ) -> anyhow::Result<()> {
        let to = serde_json::to_value(to)?;
        self.retrying(|| {
            self.db
                .query(format!(
                    "UPDATE {table} SET doc.{set} = $to WHERE doc.{field} = $value RETURN NONE"
                ))
                .bind(("value", value.to_string()))
                .bind(("to", to.clone()))
                .into_future()
        })
        .await?;
        Ok(())
    }

    // -- passages and documents (R1-store) ---------------------------------

    /// The next value of the store-wide generation counter (1, 2, …). Documents take
    /// one each time their passages change, and the context API's change feed lists what
    /// passed a value, so one counter orders every document.
    pub async fn next_generation(&self) -> anyhow::Result<u64> {
        let n: Vec<u64> = self
            .query_rows(
                // Never at or below a generation a document or tombstone already has
                // (rows written before the counter, or restored from a backup).
                "UPSERT schema_meta:generation SET n = math::max([n ?? 0, \
                   math::max(SELECT VALUE doc.generation FROM document) ?? 0, \
                   math::max(SELECT VALUE doc.generation FROM document_deleted) ?? 0]) + 1 \
                 RETURN VALUE n",
                vec![],
            )
            .await?;
        n.into_iter()
            .next()
            .context("the generation counter returned nothing")
    }

    /// The generation counter as it is (0 before the first document).
    pub async fn generation(&self) -> anyhow::Result<u64> {
        let n: Vec<Option<u64>> = self
            .query_rows("SELECT VALUE n FROM schema_meta:generation", vec![])
            .await?;
        Ok(n.into_iter().next().flatten().unwrap_or(0))
    }

    /// Every passage of a document, by `ord` (the `passage_document` index).
    pub async fn passages_of(&self, document_id: &str) -> anyhow::Result<Vec<PassageRow>> {
        self.query_rows(
            "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d ORDER BY ord",
            vec![("d", serde_json::json!(document_id))],
        )
        .await
    }

    /// Whether a document has any passage.
    pub async fn has_passages(&self, document_id: &str) -> anyhow::Result<bool> {
        let rows: Vec<serde_json::Value> = self
            .query_rows(
                "SELECT id FROM passage WHERE document_id = $d LIMIT 1",
                vec![("d", serde_json::json!(document_id))],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Deletes up to `INSERT_MANY_MAX` passages of a document by their ids.
    pub async fn delete_passages(&self, document_id: &str, ids: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(
            ids.len() <= INSERT_MANY_MAX,
            "delete_passages takes at most {INSERT_MANY_MAX} ids, not {}",
            ids.len()
        );
        if ids.is_empty() {
            return Ok(());
        }
        self.query_rows::<serde_json::Value>(
            "DELETE passage WHERE document_id = $d AND record::id(id) IN $ids RETURN NONE",
            vec![
                ("d", serde_json::json!(document_id)),
                ("ids", serde_json::json!(ids)),
            ],
        )
        .await?;
        Ok(())
    }

    /// Deletes every passage of a document.
    pub async fn delete_document_passages(&self, document_id: &str) -> anyhow::Result<()> {
        self.query_rows::<serde_json::Value>(
            "DELETE passage WHERE document_id = $d RETURN NONE",
            vec![("d", serde_json::json!(document_id))],
        )
        .await?;
        Ok(())
    }

    /// Rewrites everything but the text, hash, embedder and vector of up to
    /// `INSERT_MANY_MAX` stored passages (by `id`): their place in the document, scope
    /// and access, for passages an ingest keeps.
    pub async fn update_passages(&self, rows: &[PassageRow]) -> anyhow::Result<()> {
        anyhow::ensure!(
            rows.len() <= INSERT_MANY_MAX,
            "update_passages takes at most {INSERT_MANY_MAX} rows, not {}",
            rows.len()
        );
        if rows.is_empty() {
            return Ok(());
        }
        // Without the vectors: only the locator fields travel.
        let rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r)?;
                if let Some(o) = v.as_object_mut() {
                    o.remove("vector");
                }
                Ok(v)
            })
            .collect::<anyhow::Result<_>>()?;
        self.query_rows::<serde_json::Value>(
            "FOR $r IN $rows { UPDATE type::record('passage', $r.id) SET \
               project_id = $r.project_id, origin = $r.origin, scope_ids = $r.scope_ids, \
               access = $r.access, deny = $r.deny, unit_id = $r.unit_id, modality = $r.modality, ord = $r.ord, \
               page = $r.page, start_line = $r.start_line, end_line = $r.end_line, \
               start_ms = $r.start_ms, end_ms = $r.end_ms, section_path = $r.section_path, \
               file = $r.file RETURN NONE; };",
            vec![("rows", serde_json::Value::Array(rows))],
        )
        .await?;
        Ok(())
    }

    /// The inverse of `update_passages`: rewrites the vector and embedder (and the place
    /// in the document) of up to `INSERT_MANY_MAX` stored passages (by `id`) whose text
    /// and hash are unchanged, for a re-embed with another embedder version. The
    /// passages keep their ids, so they never leave the index while it runs.
    pub async fn rewrite_vectors(&self, rows: &[PassageRow]) -> anyhow::Result<()> {
        anyhow::ensure!(
            rows.len() <= INSERT_MANY_MAX,
            "rewrite_vectors takes at most {INSERT_MANY_MAX} rows, not {}",
            rows.len()
        );
        if rows.is_empty() {
            return Ok(());
        }
        self.query_rows::<serde_json::Value>(
            "FOR $r IN $rows { UPDATE type::record('passage', $r.id) SET \
               vector = $r.vector, embedder = $r.embedder, \
               project_id = $r.project_id, origin = $r.origin, scope_ids = $r.scope_ids, \
               access = $r.access, deny = $r.deny, unit_id = $r.unit_id, modality = $r.modality, ord = $r.ord, \
               page = $r.page, start_line = $r.start_line, end_line = $r.end_line, \
               start_ms = $r.start_ms, end_ms = $r.end_ms, section_path = $r.section_path, \
               file = $r.file RETURN NONE; };",
            vec![("rows", serde_json::to_value(rows)?)],
        )
        .await?;
        Ok(())
    }

    /// Re-stamps a document's audience: every passage of it (one indexed `UPDATE` on
    /// `passage_document`) and the `document` row, which takes a new generation so
    /// caches and the change feed move. A writer by document id: call under the write
    /// lock, for a document the caller already passed the check for. Returns the new
    /// generation, or `None` when there is no such document row (the passages are
    /// re-stamped all the same).
    pub async fn set_document_audience(
        &self,
        document_id: &str,
        access: &[String],
        deny: &[String],
    ) -> anyhow::Result<Option<u64>> {
        self.query_rows::<serde_json::Value>(
            "UPDATE passage SET access = $a, deny = $d WHERE document_id = $id RETURN NONE",
            vec![
                ("id", serde_json::json!(document_id)),
                ("a", serde_json::json!(access)),
                ("d", serde_json::json!(deny)),
            ],
        )
        .await?;
        if self
            .get::<serde_json::Value>(DOCUMENT, document_id)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let generation = self.next_generation().await?;
        self.retrying(|| {
            self.db
                .query(
                    "UPDATE $rid SET doc.access = $a, doc.deny = $d, doc.generation = $g \
                     RETURN NONE",
                )
                .bind(("rid", RecordId::new(DOCUMENT, document_id.to_string())))
                .bind(("a", access.to_vec()))
                .bind(("d", deny.to_vec()))
                .bind(("g", generation))
                .into_future()
        })
        .await
        .context("couldn't re-stamp the document")?;
        Ok(Some(generation))
    }

    /// The documents with a passage whose `embedder` isn't `version`.
    pub async fn documents_with_other_embedder(
        &self,
        version: &str,
    ) -> anyhow::Result<std::collections::BTreeSet<String>> {
        #[derive(serde::Deserialize)]
        struct Row {
            document_id: String,
        }
        let rows: Vec<Row> = self
            .query_rows(
                "SELECT document_id FROM passage WHERE embedder != $e GROUP BY document_id",
                vec![("e", serde_json::json!(version))],
            )
            .await?;
        Ok(rows.into_iter().map(|r| r.document_id).collect())
    }

    /// Adds `scope_id` to the `scope_ids` of every passage of a document.
    pub async fn add_passage_scope(&self, document_id: &str, scope_id: &str) -> anyhow::Result<()> {
        self.query_rows::<serde_json::Value>(
            "UPDATE passage SET scope_ids = array::union(scope_ids, [$s]) \
             WHERE document_id = $d RETURN NONE",
            vec![
                ("d", serde_json::json!(document_id)),
                ("s", serde_json::json!(scope_id)),
            ],
        )
        .await?;
        Ok(())
    }

    /// Removes `scope_id` from the `scope_ids` of every passage of a document.
    pub async fn remove_passage_scope(
        &self,
        document_id: &str,
        scope_id: &str,
    ) -> anyhow::Result<()> {
        self.query_rows::<serde_json::Value>(
            "UPDATE passage SET scope_ids = array::complement(scope_ids, [$s]) \
             WHERE document_id = $d RETURN NONE",
            vec![
                ("d", serde_json::json!(document_id)),
                ("s", serde_json::json!(scope_id)),
            ],
        )
        .await?;
        Ok(())
    }

    /// Whether `table` is defined in the database.
    pub async fn table_exists(&self, table: &'static str) -> anyhow::Result<bool> {
        let info: Vec<serde_json::Value> = self.query_rows("INFO FOR DB", vec![]).await?;
        Ok(info
            .first()
            .and_then(|i| i.get("tables"))
            .and_then(|t| t.get(table))
            .is_some())
    }

    /// Removes a table and its records for good: an old table (`chunk`, `media`) isn't
    /// defined again at the next start. Reading a removed table is an error; callers
    /// check `table_exists` first.
    pub async fn remove_table(&self, table: &'static str) -> anyhow::Result<()> {
        let mut retired = self.retired().await?;
        if !retired.iter().any(|r| r == table) {
            retired.push(table.to_string());
            self.put(SCHEMA_META, "retired", &retired).await?;
        }
        self.db
            .query(format!("REMOVE TABLE IF EXISTS {table}"))
            .await?
            .check()
            .with_context(|| format!("couldn't remove the table {table}"))?;
        Ok(())
    }
}

/// Whether an `INFO FOR …` result lists `name` under `section`.
fn has_key(info: &serde_json::Value, section: &str, name: &str) -> bool {
    info.get(section)
        .and_then(|s| s.as_object())
        .is_some_and(|s| s.contains_key(name))
}

/// `table:[$f0, …]..=[$t0, …]` and the parameters it names.
fn range_expr<'a>(
    table: &str,
    from: &'a [KeyPart],
    to: &'a [KeyPart],
) -> (String, Vec<(String, &'a KeyPart)>) {
    let mut binds = Vec::new();
    let mut side = |prefix: &str, parts: &'a [KeyPart]| {
        let names: Vec<String> = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let name = format!("{prefix}{i}");
                binds.push((name.clone(), part));
                format!("${name}")
            })
            .collect();
        format!("[{}]", names.join(", "))
    };
    let lo = side("from_", from);
    let hi = side("to_", to);
    (format!("{table}:{lo}..={hi}"), binds)
}

fn bind_part<'q>(
    q: surrealdb::method::Query<'q, Db>,
    name: String,
    part: &KeyPart,
) -> surrealdb::method::Query<'q, Db> {
    match part {
        KeyPart::Str(s) => q.bind((name, s.clone())),
        KeyPart::Int(i) => q.bind((name, *i)),
        KeyPart::Time(t) => q.bind((name, *t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        id: String,
        parent_id: Option<String>,
        n: i64,
        f: Option<f32>,
        status: serde_json::Value,
    }

    /// Every write retries a conflict SurrealDB says can be retried (a chat or topic
    /// `DELETE` used not to, and failed with a 500 under load); other errors don't.
    #[tokio::test]
    async fn writes_retry_a_retryable_conflict_and_nothing_else() {
        use std::pin::Pin;
        type Fut<'a> = Pin<
            Box<
                dyn std::future::Future<Output = surrealdb::Result<surrealdb::IndexedResults>> + 'a,
            >,
        >;
        let s = Store::memory().await.unwrap();
        let mut calls = 0;
        let res = s
            .retrying(|| -> Fut<'_> {
                calls += 1;
                if calls < 3 {
                    Box::pin(async {
                        Err(surrealdb::Error::internal(
                            "Transaction conflict: Write conflict. This transaction can be retried"
                                .into(),
                        ))
                    })
                } else {
                    Box::pin(s.db.query("RETURN 1").into_future())
                }
            })
            .await;
        assert!(res.is_ok());
        assert_eq!(calls, 3);

        let mut calls = 0;
        let res = s
            .retrying(|| -> Fut<'_> {
                calls += 1;
                Box::pin(async { Err(surrealdb::Error::internal("Something else broke".into())) })
            })
            .await;
        assert!(res.is_err());
        assert_eq!(calls, 1);
    }

    /// The L1 tables (`group` is a SurrealQL keyword: `docs/l1-brief.md` §6) are
    /// defined, written, listed and filtered like the others, and the unique login
    /// index refuses a second person with the same login.
    #[tokio::test]
    async fn principal_tables_round_trip() {
        let s = Store::memory().await.unwrap();
        for table in [PERSON, GROUP, MEMBERSHIP, AGENT, INVITATION] {
            assert!(s.list::<serde_json::Value>(table).await.unwrap().is_empty());
        }
        let group = serde_json::json!({"path": "company/eng", "name": "Engineering"});
        s.put(GROUP, "company/eng", &group).await.unwrap();
        assert_eq!(
            s.get::<serde_json::Value>(GROUP, "company/eng")
                .await
                .unwrap(),
            Some(group.clone())
        );
        assert_eq!(
            s.list::<serde_json::Value>(GROUP).await.unwrap(),
            vec![group.clone()]
        );
        assert_eq!(
            s.list_where::<serde_json::Value>(GROUP, "name", "Engineering")
                .await
                .unwrap(),
            vec![group]
        );
        s.delete(GROUP, "company/eng").await.unwrap();
        assert!(s.list::<serde_json::Value>(GROUP).await.unwrap().is_empty());

        let p1 = serde_json::json!({"id": "p1", "login_key": "bob", "handle": "bob"});
        let p2 = serde_json::json!({"id": "p2", "login_key": "bob", "handle": "bob-2"});
        s.put(PERSON, "p1", &p1).await.unwrap();
        assert!(s.put(PERSON, "p2", &p2).await.is_err());
        assert_eq!(s.list::<serde_json::Value>(PERSON).await.unwrap(), vec![p1]);
    }

    #[tokio::test]
    async fn round_trips_records() {
        let s = Store::memory().await.unwrap();
        assert!(s.get::<Row>(TOPIC, "a").await.unwrap().is_none());
        assert!(s.list::<Row>(TOPIC).await.unwrap().is_empty());
        let a = Row {
            id: "a".into(),
            parent_id: None,
            n: 3,
            f: Some(0.5),
            status: serde_json::json!({"state": "failed", "error": "x"}),
        };
        let b = Row {
            id: "b".into(),
            parent_id: Some("a".into()),
            n: -1,
            f: None,
            status: serde_json::json!({"state": "ready"}),
        };
        s.put(TOPIC, "a", &a).await.unwrap();
        s.put(TOPIC, "b", &b).await.unwrap();
        assert_eq!(s.get::<Row>(TOPIC, "a").await.unwrap().unwrap(), a);
        assert_eq!(s.list::<Row>(TOPIC).await.unwrap().len(), 2);
        assert_eq!(
            s.list_where::<Row>(TOPIC, "parent_id", "a").await.unwrap(),
            vec![b]
        );
        let a2 = Row { n: 9, ..a };
        s.put(TOPIC, "a", &a2).await.unwrap();
        assert_eq!(s.get::<Row>(TOPIC, "a").await.unwrap().unwrap().n, 9);
        s.delete(TOPIC, "a").await.unwrap();
        assert!(s.get::<Row>(TOPIC, "a").await.unwrap().is_none());
        assert_eq!(s.list::<Row>(TOPIC).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn updates_and_deletes_by_field() {
        let s = Store::memory().await.unwrap();
        let row = |id: &str, parent: &str| Row {
            id: id.into(),
            parent_id: Some(parent.into()),
            n: 0,
            f: Some(0.25),
            status: serde_json::json!(null),
        };
        s.put(CHUNK, "a", &row("a", "x")).await.unwrap();
        s.put(CHUNK, "b", &row("b", "x")).await.unwrap();
        s.put(CHUNK, "c", &row("c", "y")).await.unwrap();
        s.set_where(CHUNK, "parent_id", "x", "status", &"moved")
            .await
            .unwrap();
        let moved: Vec<Row> = s.list_where(CHUNK, "parent_id", "x").await.unwrap();
        assert_eq!(moved.len(), 2);
        assert!(
            moved
                .iter()
                .all(|r| r.status == "moved" && r.f == Some(0.25))
        );
        let c: Row = s.get(CHUNK, "c").await.unwrap().unwrap();
        assert!(c.status.is_null());
        s.delete_where(CHUNK, "parent_id", "x").await.unwrap();
        let left: Vec<Row> = s.list(CHUNK).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "c");
    }

    // -- the retrieval schema on the embedded engine (kv-mem) ------------------

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0f32; 768];
        v[i] = 1.0;
        v
    }

    fn passage(
        id: &str,
        doc: &str,
        scope: &[&str],
        modality: &str,
        text: &str,
        v: Vec<f32>,
    ) -> PassageRow {
        PassageRow {
            id: id.into(),
            project_id: "p".into(),
            document_id: doc.into(),
            origin: "source".into(),
            scope_ids: scope.iter().map(|s| s.to_string()).collect(),
            access: vec!["company".into()],
            deny: Vec::new(),
            unit_id: format!("{doc}/{id}"),
            modality: modality.into(),
            ord: 0,
            page: None,
            start_line: Some(1),
            end_line: Some(3),
            start_ms: None,
            end_ms: None,
            section_path: "Intro".into(),
            text: text.into(),
            file: None,
            chunk_hash: format!("h-{id}"),
            embedder: "test".into(),
            vector: v,
        }
    }

    async fn seeded() -> Store {
        let s = Store::memory().await.unwrap();
        let mut near = unit(0);
        near[1] = 0.5;
        s.insert_many(
            PASSAGE,
            &[
                passage(
                    "a",
                    "d1",
                    &["t1"],
                    "text",
                    "The Carnot engine sets the efficiency limit of heat engines",
                    unit(0),
                ),
                passage(
                    "b",
                    "d1",
                    &["t1", "c1"],
                    "text",
                    "Photosynthesis stores light energy in sugar",
                    unit(1),
                ),
                passage(
                    "c",
                    "d2",
                    &["t2"],
                    "picture",
                    "pressure volume diagram of a heat engine cycle",
                    near,
                ),
            ],
        )
        .await
        .unwrap();
        s
    }

    #[derive(Debug, serde::Deserialize)]
    struct Ranked {
        id: String,
        #[serde(default)]
        d: Option<f64>,
        #[serde(default)]
        s: Option<f64>,
        #[serde(default)]
        h: Option<String>,
    }

    #[tokio::test]
    async fn schema_applies_once_and_passages_round_trip() {
        let s = seeded().await;
        // A second init over the same store re-applies nothing that fails.
        s.apply_indexes().await.unwrap();
        let rows: Vec<PassageRow> = s
            .query_rows(
                "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d ORDER BY id",
                vec![("d", serde_json::json!("d1"))],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            passage(
                "a",
                "d1",
                &["t1"],
                "text",
                "The Carnot engine sets the efficiency limit of heat engines",
                unit(0)
            )
        );
        // (document_id, chunk_hash) is unique.
        let dup = passage("z", "d1", &[], "text", "again", unit(5));
        let dup = PassageRow {
            chunk_hash: "h-a".into(),
            ..dup
        };
        assert!(s.insert_many(PASSAGE, &[dup]).await.is_err());
        // Batches are bounded.
        let many: Vec<PassageRow> = (0..=INSERT_MANY_MAX)
            .map(|i| passage(&format!("m{i}"), "d9", &[], "text", "x", unit(i % 768)))
            .collect();
        assert!(s.insert_many(PASSAGE, &many).await.is_err());
        // A vector of the wrong type is refused by the schema.
        let bad: anyhow::Result<()> = s
            .query_rows::<serde_json::Value>(
                "CREATE passage:bad CONTENT { document_id: 'd', vector: 'no' }",
                vec![],
            )
            .await
            .map(|_| ());
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn hnsw_finds_the_nearest_within_a_scope() {
        let s = seeded().await;
        let mut q = unit(0);
        q[1] = 0.05;
        let hits: Vec<Ranked> = s
            .query_rows(
                "SELECT record::id(id) AS id, vector::distance::knn() AS d FROM passage \
                 WHERE vector <|2, 128|> $q ORDER BY d",
                vec![("q", serde_json::json!(q))],
            )
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
        assert!(hits[0].d.unwrap() < hits[1].d.unwrap());
        // Pre-filtered by scope and modality: c is outside t1.
        let hits: Vec<Ranked> = s
            .query_rows(
                "SELECT record::id(id) AS id, vector::distance::knn() AS d FROM passage \
                 WHERE vector <|2, 128|> $q AND $scope IN scope_ids AND modality IN $modalities ORDER BY d",
                vec![
                    ("q", serde_json::json!(q)),
                    ("scope", serde_json::json!("t1")),
                    ("modalities", serde_json::json!(["text", "picture"])),
                ],
            )
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        // Access paths pre-filter inside the KNN scan, like scope. (`$access` is a
        // protected parameter name in SurrealDB 3.3.4, so the binding is `$paths`.)
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                "SELECT id FROM passage WHERE vector <|2, 128|> $q AND access CONTAINSANY $paths EXPLAIN",
                vec![
                    ("q", serde_json::json!(q)),
                    ("paths", serde_json::json!(["company"])),
                ],
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(
            plan.contains("KnnScan") && plan.contains("CONTAINSANY"),
            "{plan}"
        );
        let none: Vec<Ranked> = s
            .query_rows(
                "SELECT record::id(id) AS id FROM passage WHERE vector <|2, 128|> $q AND access CONTAINSANY $paths",
                vec![
                    ("q", serde_json::json!(q)),
                    ("paths", serde_json::json!(["company/finance"])),
                ],
            )
            .await
            .unwrap();
        assert!(none.is_empty());
        // The planner uses the HNSW index, and the scope index on its own.
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                "SELECT id FROM passage WHERE vector <|2, 128|> $q AND $scope IN scope_ids EXPLAIN",
                vec![
                    ("q", serde_json::json!(q)),
                    ("scope", serde_json::json!("t1")),
                ],
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(
            plan.contains("KnnScan") && plan.contains("passage_vector"),
            "{plan}"
        );
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                "SELECT id FROM passage WHERE $scope IN scope_ids EXPLAIN",
                vec![("scope", serde_json::json!("c1"))],
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(plan.contains("passage_scope"), "{plan}");
    }

    #[tokio::test]
    async fn fulltext_scores_and_highlights() {
        let s = seeded().await;
        let hits: Vec<Ranked> = s
            .query_rows(
                "SELECT record::id(id) AS id, search::score(1) AS s, \
                 search::highlight('<b>', '</b>', 1) AS h FROM passage \
                 WHERE text @1@ $q ORDER BY s DESC",
                vec![("q", serde_json::json!("photosynthesis"))],
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b");
        assert!(hits[0].s.unwrap() > 0.0);
        assert_eq!(
            hits[0].h.as_deref(),
            Some("<b>Photosynthesis</b> stores light energy in sugar")
        );
        // Stemming: "engines" finds "engine".
        let hits: Vec<Ranked> = s
            .query_rows(
                "SELECT record::id(id) AS id, search::score(1) AS s FROM passage \
                 WHERE text @1@ $q ORDER BY s DESC",
                vec![("q", serde_json::json!("engines"))],
            )
            .await
            .unwrap();
        let mut ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["a", "c"]);
    }

    #[tokio::test]
    async fn rrf_fuses_the_two_legs() {
        let s = seeded().await;
        #[derive(Debug, serde::Deserialize)]
        struct Fused {
            id: String,
            rrf_score: f64,
        }
        let mut q = unit(1);
        q[0] = 0.05;
        let fused: Vec<Fused> = s
            .query_rows(
                "LET $dense = SELECT id, vector::distance::knn() AS d FROM passage WHERE vector <|3, 128|> $q; \
                 LET $lex = SELECT id, search::score(1) AS s FROM passage WHERE text @1@ $text ORDER BY s DESC LIMIT 40; \
                 RETURN search::rrf([$dense, $lex], 24, 60);",
                vec![
                    ("q", serde_json::json!(q)),
                    ("text", serde_json::json!("heat engine")),
                ],
            )
            .await
            .unwrap();
        assert_eq!(fused.len(), 3);
        assert!(fused.windows(2).all(|w| w[0].rrf_score >= w[1].rrf_score));
        // b is first in the dense leg only; a and c are in both legs and outrank it.
        assert_eq!(fused[2].id, "passage:b");
        assert!((fused[2].rrf_score - 1.0 / 61.0).abs() < 1e-9);
    }

    async fn trace(s: &Store, kind: &str, at: &str, ulid: &str, extra: serde_json::Value) {
        s.query_rows::<serde_json::Value>(
            "LET $at = <datetime>$at_text; \
             CREATE type::record('trace', [$kind, $at, $ulid]) \
             CONTENT object::extend($extra, { kind: $kind, at: $at }) RETURN NONE;",
            vec![
                ("kind", serde_json::json!(kind)),
                ("at_text", serde_json::json!(at)),
                ("ulid", serde_json::json!(ulid)),
                ("extra", extra),
            ],
        )
        .await
        .unwrap();
    }

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        text.parse().unwrap()
    }

    #[tokio::test]
    async fn traces_by_kind_and_window_and_their_rollups() {
        let s = Store::memory().await.unwrap();
        let call = serde_json::json!({
            "duration_ms": 100, "route": "small", "provider": "openai", "model": "m",
            "cost_usd": 0.25, "usage": {"input": 100, "cached": 40, "output": 5}
        });
        trace(
            &s,
            "model_call",
            "2026-10-14T09:59:59Z",
            "01A",
            call.clone(),
        )
        .await;
        trace(
            &s,
            "model_call",
            "2026-10-14T10:03:21.412Z",
            "01B",
            call.clone(),
        )
        .await;
        trace(
            &s,
            "model_call",
            "2026-10-14T10:40:00Z",
            "01C",
            call.clone(),
        )
        .await;
        trace(
            &s,
            "retrieval",
            "2026-10-14T10:05:00Z",
            "01D",
            serde_json::json!({"duration_ms": 8, "hit_ids": ["a"]}),
        )
        .await;

        #[derive(Debug, serde::Deserialize)]
        struct Row {
            kind: String,
            key: (String, String, String),
        }
        let window = |kind: &str, from: &str, to: &str| {
            (
                vec![
                    KeyPart::Str(kind.into()),
                    KeyPart::Time(at(from)),
                    KeyPart::Str(String::new()),
                ],
                vec![
                    KeyPart::Str(kind.into()),
                    KeyPart::Time(at(to)),
                    KeyPart::Str("~".into()),
                ],
            )
        };
        let (from, to) = window("model_call", "2026-10-14T10:00:00Z", "2026-10-14T11:00:00Z");
        let rows: Vec<Row> = s.range(TRACE, &from, &to).await.unwrap();
        let keys: Vec<&str> = rows.iter().map(|r| r.key.2.as_str()).collect();
        assert_eq!(keys, ["01B", "01C"]);
        assert!(
            rows.iter()
                .all(|r| r.kind == "model_call" && r.key.0 == "model_call")
        );

        #[derive(Debug, serde::Deserialize)]
        struct Rollup {
            kind: String,
            bucket: chrono::DateTime<chrono::Utc>,
            calls: u64,
            input: f64,
            cached: f64,
            cost_usd: f64,
            mean_ms: f64,
        }
        let hourly = |s: Store| async move {
            s.query_rows::<Rollup>(
                "SELECT * FROM trace_hourly WHERE kind = 'model_call' ORDER BY bucket",
                vec![],
            )
            .await
            .unwrap()
        };
        let rows = hourly(s.clone()).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].bucket, at("2026-10-14T10:00:00Z"));
        assert_eq!(
            (rows[1].calls, rows[1].input, rows[1].cached),
            (2, 200.0, 80.0)
        );
        assert!((rows[1].cost_usd - 0.5).abs() < 1e-9 && rows[1].mean_ms == 100.0);
        // The view follows a new row…
        trace(
            &s,
            "model_call",
            "2026-10-14T10:59:00Z",
            "01E",
            call.clone(),
        )
        .await;
        assert_eq!(hourly(s.clone()).await[1].calls, 3);
        // …and rows without usage or cost (a retrieval) count in their own group.
        let daily: Vec<Rollup> = s
            .query_rows("SELECT * FROM trace_daily ORDER BY kind", vec![])
            .await
            .unwrap();
        assert_eq!(
            daily
                .iter()
                .map(|r| (r.kind.as_str(), r.calls))
                .collect::<Vec<_>>(),
            [("model_call", 4), ("retrieval", 1)]
        );

        // Retention deletes by range and leaves other kinds alone.
        let (from, to) = window("model_call", "1970-01-01T00:00:00Z", "2026-10-14T10:30:00Z");
        s.delete_range(TRACE, &from, &to).await.unwrap();
        let (from, to) = window("model_call", "1970-01-01T00:00:00Z", "2100-01-01T00:00:00Z");
        let left: Vec<Row> = s.range(TRACE, &from, &to).await.unwrap();
        assert_eq!(
            left.iter().map(|r| r.key.2.as_str()).collect::<Vec<_>>(),
            ["01C", "01E"]
        );
        let (from, to) = window("retrieval", "1970-01-01T00:00:00Z", "2100-01-01T00:00:00Z");
        assert_eq!(s.range::<Row>(TRACE, &from, &to).await.unwrap().len(), 1);
    }

    // -- R1-store helpers ------------------------------------------------------

    #[tokio::test]
    async fn the_generation_counter_counts_up_from_one() {
        let s = Store::memory().await.unwrap();
        assert_eq!(s.generation().await.unwrap(), 0);
        assert_eq!(s.next_generation().await.unwrap(), 1);
        assert_eq!(s.next_generation().await.unwrap(), 2);
        assert_eq!(s.generation().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn passages_of_a_document_are_read_updated_scoped_and_deleted() {
        let s = seeded().await;
        let rows = s.passages_of("d1").await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(s.has_passages("d2").await.unwrap());
        assert!(!s.has_passages("d9").await.unwrap());
        // The document index serves the read.
        let plan: Vec<serde_json::Value> = s
            .query_rows(
                "SELECT * FROM passage WHERE document_id = $d EXPLAIN",
                vec![("d", serde_json::json!("d1"))],
            )
            .await
            .unwrap();
        let plan = serde_json::to_string(&plan).unwrap();
        assert!(plan.contains("passage_document"), "{plan}");

        // Locators change; the vector and the text stay.
        let mut a = rows[0].clone();
        a.ord = 7;
        a.page = Some(3);
        a.start_line = None;
        a.section_path = "Moved".into();
        a.access = vec!["company/engineering".into()];
        s.update_passages(std::slice::from_ref(&a)).await.unwrap();
        let back = s.passages_of("d1").await.unwrap();
        assert_eq!(back.iter().find(|r| r.id == "a").unwrap(), &a);

        s.add_passage_scope("d1", "chat9").await.unwrap();
        s.add_passage_scope("d1", "chat9").await.unwrap();
        let back = s.passages_of("d1").await.unwrap();
        assert!(
            back.iter()
                .all(|r| r.scope_ids.iter().filter(|x| *x == "chat9").count() == 1)
        );
        s.remove_passage_scope("d1", "t1").await.unwrap();
        let back = s.passages_of("d1").await.unwrap();
        assert!(back.iter().all(|r| !r.scope_ids.iter().any(|x| x == "t1")));

        s.delete_passages("d1", &["a".to_string()]).await.unwrap();
        assert_eq!(s.passages_of("d1").await.unwrap().len(), 1);
        // Only the named document's passages go.
        s.delete_passages("d1", &["c".to_string()]).await.unwrap();
        assert!(s.has_passages("d2").await.unwrap());
        s.delete_document_passages("d1").await.unwrap();
        assert!(s.passages_of("d1").await.unwrap().is_empty());
        assert!(s.has_passages("d2").await.unwrap());
    }

    /// The first node of an `EXPLAIN` plan whose operator is `op` (depth first).
    fn plan_node<'a>(v: &'a serde_json::Value, op: &str) -> Option<&'a serde_json::Value> {
        if v.get("operator").and_then(|o| o.as_str()) == Some(op) {
            return Some(v);
        }
        let children = v
            .as_array()
            .or_else(|| v.get("children").and_then(|c| c.as_array()))?;
        children.iter().find_map(|c| plan_node(c, op))
    }

    /// The plan as text, for `EXPLAIN` assertions.
    async fn plan(s: &Store, sql: &str, binds: Vec<(&'static str, serde_json::Value)>) -> String {
        let plan: Vec<serde_json::Value> = s.query_rows(sql, binds).await.unwrap();
        serde_json::to_string(&plan).unwrap()
    }

    /// L2 D1: the reader's two conjuncts (`access CONTAINSANY $paths AND deny
    /// CONTAINSNONE $deny`) plan inside both legs of the hybrid query: the HNSW scan
    /// (`KnnScan`, a pre-filter, not a post-filter) and the FULLTEXT scan on
    /// `passage_text`. And they select by exact membership with denials.
    #[tokio::test]
    async fn the_readers_two_conjuncts_plan_inside_both_legs() {
        let s = seeded().await;
        // d2's picture is denied to bob; d1's rows are `company`, deny none.
        s.set_document_audience("d2", &["company".into()], &["person/bob".into()])
            .await
            .unwrap();
        let mut q = unit(0);
        q[1] = 0.05;
        let reader = |paths: &[&str], deny: &[&str]| {
            vec![
                ("q", serde_json::json!(q)),
                ("paths", serde_json::json!(paths)),
                ("deny", serde_json::json!(deny)),
            ]
        };

        // The dense leg: the KnnScan's own predicate holds both conjuncts (a pre-filter:
        // the k nearest among the rows the reader may read). The planner re-checks the
        // same predicate in a `Filter` above it; the scan already applied it.
        let dense = "SELECT id FROM passage WHERE vector <|3, 128|> $q \
                     AND access CONTAINSANY $paths AND deny CONTAINSNONE $deny EXPLAIN";
        let rows: Vec<serde_json::Value> = s
            .query_rows(dense, reader(&["company"], &["person/bob"]))
            .await
            .unwrap();
        let p = serde_json::Value::Array(rows);
        let knn = plan_node(&p, "KnnScan").unwrap_or_else(|| panic!("no KnnScan: {p}"));
        assert_eq!(knn["attributes"]["index"], "passage_vector", "{p}");
        assert_eq!(
            knn["attributes"]["predicate"],
            "access CONTAINSANY ['company'] AND deny CONTAINSNONE ['person/bob']",
            "{p}"
        );

        // The lexical leg: the FULLTEXT scan on `passage_text` intersected with the
        // `passage_access` index (`BitmapAnd`), and the statement's predicate (both
        // conjuncts) on the rows it resolves, before any `ORDER BY … LIMIT`.
        let lexical = "SELECT id, search::score(0) AS s FROM passage WHERE text @0@ $text \
                       AND access CONTAINSANY $paths AND deny CONTAINSNONE $deny EXPLAIN";
        let mut binds = reader(&["company"], &["person/bob"]);
        binds.push(("text", serde_json::json!("engine")));
        let rows: Vec<serde_json::Value> = s.query_rows(lexical, binds).await.unwrap();
        let p = serde_json::Value::Array(rows);
        let fts =
            plan_node(&p, "BitmapFullTextScan").unwrap_or_else(|| panic!("no FULLTEXT scan: {p}"));
        assert_eq!(fts["attributes"]["index"], "passage_text", "{p}");
        let access =
            plan_node(&p, "BitmapIndexScan").unwrap_or_else(|| panic!("no access index scan: {p}"));
        assert_eq!(access["attributes"]["index"], "passage_access", "{p}");
        assert!(plan_node(&p, "BitmapAnd").is_some(), "{p}");
        let filter = plan_node(&p, "Filter").unwrap_or_else(|| panic!("no predicate: {p}"));
        assert_eq!(
            filter["attributes"]["predicate"],
            "text @0@ 'engine' AND access CONTAINSANY ['company'] \
             AND deny CONTAINSNONE ['person/bob']",
            "{p}"
        );

        let ids = |rows: Vec<Ranked>| {
            let mut ids: Vec<String> = rows.into_iter().map(|r| r.id).collect();
            ids.sort();
            ids
        };
        // k = 2: the nearest are a, then c, then b.
        let dense_ids = "SELECT record::id(id) AS id FROM passage WHERE vector <|2, 128|> $q \
                         AND access CONTAINSANY $paths AND deny CONTAINSNONE $deny";
        // An empty deny on both sides matches (`[] CONTAINSNONE []`).
        let all: Vec<Ranked> = s
            .query_rows(dense_ids, reader(&["company"], &[]))
            .await
            .unwrap();
        assert_eq!(ids(all), ["a", "c"]);
        // Bob is denied d2's picture, inside the scan: the 2 nearest he may read are a
        // and b (a post-filter would have found a and c, then dropped c).
        let bob: Vec<Ranked> = s
            .query_rows(
                dense_ids,
                reader(
                    &["company", "company/sales", "person/bob"],
                    &["person/bob", "company", "company/sales"],
                ),
            )
            .await
            .unwrap();
        assert_eq!(ids(bob), ["a", "b"]);
        // Exact membership: a reader of `company/engineering` alone reads nothing
        // `company` holds (the hierarchy is in the reader's expansion, not here).
        let eng: Vec<Ranked> = s
            .query_rows(dense_ids, reader(&["company/engineering"], &[]))
            .await
            .unwrap();
        assert!(eng.is_empty());
        // The lexical leg drops the denied row too.
        let lex_ids = "SELECT record::id(id) AS id FROM passage WHERE text @0@ $text \
                       AND access CONTAINSANY $paths AND deny CONTAINSNONE $deny";
        let mut binds = reader(&["company", "person/bob"], &["person/bob"]);
        binds.push(("text", serde_json::json!("engine")));
        let hits: Vec<Ranked> = s.query_rows(lex_ids, binds).await.unwrap();
        assert_eq!(ids(hits), ["a"]);
    }

    /// L2: `passage.access` defaults to `[]` (visible to nobody) through the
    /// fingerprinted `FIELDS` block, which changes the default and leaves stored rows
    /// as they are; `set_document_audience` re-stamps a document's passages and row.
    #[tokio::test]
    async fn the_access_default_is_nobody_and_a_document_is_restamped_whole() {
        let s = seeded().await;
        // Re-applying the block over populated rows (as an upgraded store does once)
        // rewrites no row.
        s.put(SCHEMA_META, "fields", &"old").await.unwrap();
        s.apply_fields().await.unwrap();
        let rows = s.passages_of("d1").await.unwrap();
        assert!(
            rows.iter()
                .all(|r| r.access == ["company"] && r.deny.is_empty())
        );
        s.query_rows::<serde_json::Value>(
            "CREATE passage:bare CONTENT { project_id: 'p', document_id: 'd3', origin: 'agent', \
             scope_ids: [], unit_id: 'u', modality: 'text', ord: 0, section_path: '', \
             text: 'bare', chunk_hash: 'h', embedder: 'e', vector: $v }",
            vec![("v", serde_json::json!(unit(5)))],
        )
        .await
        .unwrap();
        let bare = s.passages_of("d3").await.unwrap();
        assert!(bare[0].access.is_empty() && bare[0].deny.is_empty());

        s.put(
            DOCUMENT,
            "d1",
            &serde_json::json!({ "id": "d1", "access": ["company"], "generation": 1 }),
        )
        .await
        .unwrap();
        let g = s
            .set_document_audience(
                "d1",
                &["person/alice".into(), "company/engineering".into()],
                &["agent/bot".into()],
            )
            .await
            .unwrap()
            .unwrap();
        assert!(g > 1);
        let rows = s.passages_of("d1").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(
            |r| r.access == ["person/alice", "company/engineering"] && r.deny == ["agent/bot"]
        ));
        // Other documents' rows are untouched.
        assert!(s.passages_of("d2").await.unwrap()[0].deny.is_empty());
        let doc: serde_json::Value = s.get(DOCUMENT, "d1").await.unwrap().unwrap();
        assert_eq!(doc["deny"], serde_json::json!(["agent/bot"]));
        assert_eq!(doc["generation"], serde_json::json!(g));
        // The document's deny index plans.
        let p = plan(
            &s,
            "SELECT id FROM document WHERE doc.access CONTAINSANY $paths \
             AND doc.deny CONTAINSNONE $deny EXPLAIN",
            vec![
                ("paths", serde_json::json!(["company"])),
                ("deny", serde_json::json!(["person/bob"])),
            ],
        )
        .await;
        assert!(p.contains("document_access"), "{p}");
        // No document row: the passages are re-stamped, no generation is taken.
        assert_eq!(s.set_document_audience("d2", &[], &[]).await.unwrap(), None);
        assert!(s.passages_of("d2").await.unwrap()[0].access.is_empty());
    }

    #[tokio::test]
    async fn old_tables_stay_removed_once_removed() {
        let s = Store::memory().await.unwrap();
        assert!(s.table_exists(CHUNK).await.unwrap() && s.table_exists(MEDIA).await.unwrap());
        s.put(CHUNK, "x", &serde_json::json!({"source_id": "s"}))
            .await
            .unwrap();
        s.remove_table(CHUNK).await.unwrap();
        assert!(!s.table_exists(CHUNK).await.unwrap());
        // Reading it now is an error, which is why callers ask first.
        assert!(
            s.list_where::<serde_json::Value>(CHUNK, "source_id", "s")
                .await
                .is_err()
        );
        // The next start doesn't define it again; the other old table is still there.
        s.define_old_tables().await.unwrap();
        assert!(!s.table_exists(CHUNK).await.unwrap());
        assert!(s.table_exists(MEDIA).await.unwrap());
        s.remove_table(CHUNK).await.unwrap();
        assert_eq!(s.retired().await.unwrap(), [CHUNK]);
    }
}
