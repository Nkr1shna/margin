//! Storage on embedded SurrealDB.
//!
//! Every record is stored as `{ doc: <the record as JSON> }` under the record id
//! `table:⟨id⟩`. Keeping the record nested under `doc` lets the wire types (whose own
//! `id` field would clash with SurrealDB's record id) round-trip unchanged through
//! serde. Libraries are personal-sized, so most filtering happens in Rust.

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

const TABLES: [&str; 13] = [
    TOPIC, LESSON, NOTE, VIDEO, PROPOSAL, DEVICE, SETTINGS, SOURCE, CHUNK, MEDIA, CHAT, CHAT_ITEM,
    WORKSPACE,
];

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
        db.query(defs)
            .await
            .context("couldn't define tables")?
            .check()
            .context("couldn't define tables")?;
        Ok(Self { db })
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
        self.db
            .query("UPSERT $rid CONTENT { doc: $doc } RETURN NONE")
            .bind(("rid", RecordId::new(table, id.to_string())))
            .bind(("doc", doc))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn delete(&self, table: &'static str, id: &str) -> anyhow::Result<()> {
        self.db
            .query("DELETE $rid")
            .bind(("rid", RecordId::new(table, id.to_string())))
            .await?
            .check()?;
        Ok(())
    }

    /// Deletes every record whose top-level JSON field `field` equals `value`.
    pub async fn delete_where(
        &self,
        table: &'static str,
        field: &'static str,
        value: &str,
    ) -> anyhow::Result<()> {
        self.db
            .query(format!(
                "DELETE {table} WHERE doc.{field} = $value RETURN NONE"
            ))
            .bind(("value", value.to_string()))
            .await?
            .check()?;
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
        self.db
            .query(format!(
                "UPDATE {table} SET doc.{set} = $to WHERE doc.{field} = $value RETURN NONE"
            ))
            .bind(("value", value.to_string()))
            .bind(("to", to))
            .await?
            .check()?;
        Ok(())
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
}
