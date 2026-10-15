//! Where groups and memberships come from (L1-groups, `docs/l1-brief.md` D5). L1 has
//! one source, `Manual`: what the API wrote into the `group` and `membership` tables.
//! L6 adds a GitHub organisation source and the job that syncs it; a write through the
//! API only touches rows whose `source` is `manual` (400 `managed_by_source`).

use futures_util::future::BoxFuture;

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

impl AccessSource for Manual {
    fn name(&self) -> &'static str {
        margin_shared::SOURCE_MANUAL
    }

    fn groups(&self) -> BoxFuture<'_, anyhow::Result<Vec<(String, String)>>> {
        unimplemented!("L1, groups")
    }

    fn members<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
        unimplemented!("L1, groups")
    }
}
