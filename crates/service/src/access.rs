//! Where groups and memberships come from (L1-groups, `docs/l1-brief.md` D5). L1 has
//! one source, `Manual`: what the API wrote into the `group` and `membership` tables.
//! L6 adds a GitHub organisation source and the job that syncs it; a write through the
//! API only touches rows whose `source` is `manual` (400 `managed_by_source`).

use futures_util::future::BoxFuture;
use margin_shared::SOURCE_MANUAL;

use crate::db::{self, Store};
use crate::groups::{GroupRow, MembershipRow};
use crate::people::PersonRow;

/// A source of groups and memberships.
pub trait AccessSource: Send + Sync {
    /// Stored in `group.source` and `membership.source`.
    fn name(&self) -> &'static str;
    /// Every group it knows: `(path, name)`.
    fn groups(&self) -> BoxFuture<'_, anyhow::Result<Vec<(String, String)>>>;
    /// The GitHub logins of a group's direct members.
    fn members<'a>(&'a self, path: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<String>>>;
}

/// Groups and memberships made through the API: what the tables of this store hold.
pub struct Manual(pub crate::db::Store);

impl Manual {
    async fn manual_groups(store: &Store) -> anyhow::Result<Vec<(String, String)>> {
        let mut rows: Vec<GroupRow> = store.list(db::GROUP).await?;
        rows.retain(|r| r.source == SOURCE_MANUAL);
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(rows.into_iter().map(|r| (r.path, r.name)).collect())
    }

    async fn manual_members(store: &Store, path: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<MembershipRow> = store.list_where(db::MEMBERSHIP, "group_path", path).await?;
        let mut logins = Vec::new();
        for row in rows.into_iter().filter(|r| r.source == SOURCE_MANUAL) {
            if let Some(person) = store.get::<PersonRow>(db::PERSON, &row.person_id).await? {
                logins.push(person.person.github_login);
            }
        }
        logins.sort_by_key(|l| l.to_ascii_lowercase());
        Ok(logins)
    }
}

impl AccessSource for Manual {
    fn name(&self) -> &'static str {
        SOURCE_MANUAL
    }

    fn groups(&self) -> BoxFuture<'_, anyhow::Result<Vec<(String, String)>>> {
        Box::pin(Self::manual_groups(&self.0))
    }

    fn members<'a>(&'a self, path: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
        Box::pin(Self::manual_members(&self.0, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use margin_shared::{OrgRole, Person, PersonSettings, PersonState};

    #[tokio::test]
    async fn manual_reads_what_the_tables_hold() {
        let store = Store::memory().await.unwrap();
        for (path, source) in [("company/eng", SOURCE_MANUAL), ("company/gh", "github-org")] {
            let row = GroupRow {
                path: path.into(),
                name: format!("{path} name"),
                source: source.into(),
                created_at: Utc::now(),
            };
            store.put(db::GROUP, path, &row).await.unwrap();
        }
        let person = PersonRow {
            person: Person {
                id: "p1".into(),
                github_login: "Bob-Dev".into(),
                github_id: None,
                handle: "bob-dev".into(),
                name: "Bob".into(),
                avatar_url: None,
                org_role: OrgRole::Member,
                state: PersonState::Active,
                groups: vec![],
                settings: PersonSettings::default(),
                created_at: Utc::now(),
            },
            login_key: "bob-dev".into(),
        };
        store.put(db::PERSON, "p1", &person).await.unwrap();
        let m = MembershipRow {
            id: MembershipRow::key("p1", "company/eng"),
            person_id: "p1".into(),
            group_path: "company/eng".into(),
            source: SOURCE_MANUAL.into(),
        };
        store.put(db::MEMBERSHIP, &m.id, &m).await.unwrap();

        let manual = Manual(store);
        assert_eq!(manual.name(), "manual");
        assert_eq!(
            manual.groups().await.unwrap(),
            vec![("company/eng".to_string(), "company/eng name".to_string())]
        );
        assert_eq!(
            manual.members("company/eng").await.unwrap(),
            vec!["Bob-Dev"]
        );
        assert!(manual.members("company/gh").await.unwrap().is_empty());
    }
}
