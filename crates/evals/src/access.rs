//! The access suite's fixture (roadmap L2, `docs/l2-brief.md` D8): principals and
//! audiences over the fixture corpus (`evals/access.toml`), the readers they search as
//! (expanded by the service's own `groups::expand` and `agents::agent_principal`), and
//! the oracle that says, in Rust and independent of the SQL, which documents each one
//! may read.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use chrono::Utc;
use margin_service::pipeline::Reader;
use margin_shared::{
    Agent, AgentKind, Audience, OrgRole, Person, PersonSettings, PersonState, agent_id,
};
use serde::Deserialize;

use crate::index::EvalIndex;

/// The fixture's file in `evals/`.
pub const ACCESS_FILE: &str = "access.toml";

/// The reader the retrieval suites search as: their index keeps every passage at
/// `company` (only the access suite stamps the fixture's audiences), so this reader reads
/// all of it, as every run before L2 did.
pub fn company_reader() -> Reader {
    Reader::sorted(vec![margin_shared::ROOT_GROUP.to_string()], Vec::new())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupsFx {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PersonFx {
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentFx {
    pub owner: String,
    pub ceiling: Vec<String>,
}

/// `evals/access.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessFixture {
    #[serde(default)]
    pub groups: GroupsFx,
    #[serde(default)]
    pub people: BTreeMap<String, PersonFx>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentFx>,
    /// By exact corpus path, `dir/*` or `dir/**`.
    #[serde(default)]
    pub documents: BTreeMap<String, Audience>,
    /// Everything `documents` doesn't name.
    pub default: Audience,
}

impl AccessFixture {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        let fx: Self =
            toml::from_str(&text).with_context(|| format!("{} is malformed", path.display()))?;
        fx.check()?;
        Ok(fx)
    }

    /// Every group named exists, every agent's owner is a person, every audience is in
    /// the alphabet.
    fn check(&self) -> anyhow::Result<()> {
        let known = |g: &String| g == margin_shared::ROOT_GROUP || self.groups.paths.contains(g);
        for (name, p) in &self.people {
            if let Some(g) = p.groups.iter().find(|g| !known(g)) {
                anyhow::bail!("{name} is in {g}, which isn't in [groups]");
            }
        }
        for (name, a) in &self.agents {
            anyhow::ensure!(
                self.people.contains_key(&a.owner),
                "agent {name}'s owner {} isn't a person",
                a.owner
            );
        }
        for a in self.documents.values().chain([&self.default]) {
            margin_shared::validate(a).map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(())
    }

    /// The audience the fixture gives a corpus document: its exact path, else the
    /// longest pattern that matches, else `default`.
    pub fn audience_of(&self, path: &str) -> Audience {
        if let Some(a) = self.documents.get(path) {
            return a.clone();
        }
        self.documents
            .iter()
            .filter(|(pattern, _)| matches(pattern, path))
            .max_by_key(|(pattern, _)| pattern.len())
            .map_or_else(|| self.default.clone(), |(_, a)| a.clone())
    }

    /// The people and agents, by name.
    pub fn principals(&self) -> Vec<String> {
        self.people
            .keys()
            .chain(self.agents.keys())
            .cloned()
            .collect()
    }

    /// The reader a person or agent of the fixture searches as, expanded by the
    /// service's own functions.
    pub fn reader(&self, name: &str) -> anyhow::Result<Reader> {
        if let Some(p) = self.people.get(name) {
            let e = margin_service::groups::expand(name, OrgRole::Member, &p.groups);
            return Ok(Reader::sorted(e.paths, e.deny_ids));
        }
        let a = self
            .agents
            .get(name)
            .with_context(|| format!("no principal {name} in {ACCESS_FILE}"))?;
        let owner = self
            .people
            .get(&a.owner)
            .with_context(|| format!("agent {name}'s owner {} isn't a person", a.owner))?;
        let e = margin_service::groups::expand(&a.owner, OrgRole::Member, &owner.groups);
        let agent = Agent {
            id: name.to_string(),
            owner_id: a.owner.clone(),
            name: name.to_string(),
            kind: AgentKind::External,
            ceiling: a.ceiling.clone(),
            disabled: false,
            token: None,
            created_at: Utc::now(),
        };
        let person = Person {
            id: a.owner.clone(),
            github_login: a.owner.clone(),
            github_id: None,
            handle: a.owner.clone(),
            name: a.owner.clone(),
            avatar_url: None,
            org_role: OrgRole::Member,
            state: PersonState::Active,
            groups: owner.groups.clone(),
            settings: PersonSettings::default(),
            created_at: Utc::now(),
        };
        let p = margin_service::agents::agent_principal(&agent, &e, person);
        debug_assert_eq!(p.id, agent_id(name));
        Ok(Reader::of(&p))
    }

    /// The oracle: whether `reader` may read corpus document `path` (exact membership,
    /// denials), from this file alone.
    pub fn may_read(&self, reader: &Reader, path: &str) -> bool {
        margin_shared::reads(&reader.paths, &reader.deny_ids, &self.audience_of(path))
    }

    /// Stamps every indexed document's passages with its audience, through the
    /// service's own writer (`Store::set_document_audience`).
    pub async fn stamp(&self, index: &EvalIndex<'_>) -> anyhow::Result<()> {
        for path in index.document_paths() {
            let a = self.audience_of(&path);
            index
                .store
                .set_document_audience(&path, &a.access, &a.deny)
                .await
                .with_context(|| format!("couldn't stamp {path}"))?;
        }
        Ok(())
    }
}

/// `pattern` matches `path`: equal, `dir/*` (one more segment) or `dir/**` (any depth).
fn matches(pattern: &str, path: &str) -> bool {
    if let Some(dir) = pattern.strip_suffix("/**") {
        return path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'));
    }
    if let Some(dir) = pattern.strip_suffix("/*") {
        return path
            .strip_prefix(dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
    }
    pattern == path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;

    #[test]
    fn the_fixture_reads_and_expands() {
        let fx = AccessFixture::load(&evals_dir().join(ACCESS_FILE)).unwrap();
        assert_eq!(
            fx.audience_of("fourier-series.md").access,
            ["company/engineering"]
        );
        assert_eq!(
            fx.audience_of("pdf/lens-optics.pdf").access,
            ["company/engineering/platform"]
        );
        assert_eq!(
            fx.audience_of("folder/notes/a/b.md").access,
            ["person/alice"]
        );
        assert_eq!(fx.audience_of("tcp-congestion.md").access, ["company"]);
        assert!(matches("pdf/*", "pdf/a.pdf") && !matches("pdf/*", "pdf/x/a.pdf"));
        assert!(!matches("folder/**", "folderx/a") && !matches("pdf/*", "pdf/"));

        let alice = fx.reader("alice").unwrap();
        assert!(alice.paths.contains(&"company/engineering".to_string()));
        assert!(fx.may_read(&alice, "fourier-series.md") && fx.may_read(&alice, "bayes.md"));
        // Exact membership: carol (company only) doesn't read engineering's documents.
        let carol = fx.reader("carol").unwrap();
        assert!(!fx.may_read(&carol, "fourier-series.md"));
        assert!(fx.may_read(&carol, "heat-engines.rst"));
        let bob = fx.reader("bob").unwrap();
        assert!(!fx.may_read(&bob, "heat-engines.rst") && fx.may_read(&bob, "photosynthesis.md"));
        // An agent reads its owner's access within its ceiling, and its own denials.
        let bot = fx.reader("eng-bot").unwrap();
        assert!(bot.paths.contains(&"agent/eng-bot".to_string()));
        assert!(fx.may_read(&bot, "fourier-series.md"));
        assert!(!fx.may_read(&bot, "bayes.md") && !fx.may_read(&bot, "pdf/lens-optics.pdf"));
        assert!(!fx.may_read(&bot, "sorting.html"));
        let all = fx.reader("alice-all").unwrap();
        assert!(fx.may_read(&all, "bayes.md") && fx.may_read(&all, "sorting.html"));
        assert!(fx.reader("nobody").is_err());
    }
}
