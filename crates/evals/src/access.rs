//! The access suite's fixture (roadmap L2, `docs/l2-brief.md` D8): principals and
//! audiences over the fixture corpus (`evals/access.toml`), the readers they search as
//! (expanded by the service's own `groups::expand` and `agents::agent_principal`), and
//! the oracle that says, in Rust and independent of the SQL, which documents each one
//! may read.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use chrono::Utc;
use margin_service::pipeline::Hit;
use margin_service::pipeline::Reader;
use margin_service::retrieval::{Selector, document_outline, read_passages};
use margin_shared::{
    Agent, AgentKind, Audience, OrgRole, Person, PersonSettings, PersonState, agent_id,
};
use serde::{Deserialize, Serialize};

use crate::AccessGates;
use crate::golden::RetrievalQuery;
use crate::index::{EvalIndex, Ranked, scope_of};
use crate::metrics::percentile;
use crate::retrieval::judge;
use crate::unfiltered::UnfilteredRetriever;

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
    /// The organisation's owner (a person below): as a member of every group, the reader
    /// of everything, whose ranks `owner_rank_identical` compares with the unfiltered run.
    pub owner: String,
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
        anyhow::ensure!(
            self.people.contains_key(&self.owner),
            "the owner {} isn't a person",
            self.owner
        );
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

    /// The owner as a member of every group, expanded by the service's own function:
    /// `[company, every group, person/<owner>]`.
    pub fn owner_reader(&self) -> Reader {
        let e = margin_service::groups::expand(&self.owner, OrgRole::Owner, &self.groups.paths);
        Reader::sorted(e.paths, e.deny_ids)
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

/// How many times each of the owner's searches is timed for the latency ratio (through
/// the reader and unfiltered, interleaved, the order swapped every round; each query's
/// fastest round counts).
pub const LATENCY_ROUNDS: usize = 3;

/// One principal's searches over the golden set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PrincipalSummary {
    pub name: String,
    /// Corpus documents the oracle lets it read.
    pub reads: usize,
    /// Hits over every golden query.
    pub hits: usize,
    /// Hits of the unfiltered run it may not read: what the conjuncts removed.
    pub removed: usize,
    /// Hits it may not read but got.
    pub leaks: usize,
}

/// `run --suite access` (L2 D8).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccessReport {
    /// The vectors searched with (the recording's embedder, or `fake-bag-of-words`).
    pub embedder: String,
    pub documents: usize,
    pub queries: usize,
    pub principals: Vec<PrincipalSummary>,
    /// "<principal> found <document> for <query>".
    pub leaks: Vec<String>,
    /// (principal, document) pairs the oracle forbids, each read by id.
    pub by_id_checked: usize,
    /// "<principal> read <document> by id".
    pub by_id_leaks: Vec<String>,
    /// Allowed (principal, document) pairs whose by-id read came back empty: the
    /// predicate refusing too much (not a leak, but the by-id gate would test nothing).
    pub by_id_refused: Vec<String>,
    pub owner: String,
    /// Golden queries whose owner ranking differs from the unfiltered run's.
    pub owner_mismatches: Vec<String>,
    /// (principal, query) pairs where the principal reads every relevant document.
    pub rank_checked: usize,
    /// Of those, the ones whose relevant hits came in another order than the owner's.
    pub rank_inconsistencies: Vec<String>,
    pub owner_p95_ms: f64,
    pub unfiltered_p95_ms: f64,
}

impl AccessReport {
    pub fn owner_rank_identical(&self) -> bool {
        self.owner_mismatches.is_empty()
    }

    pub fn rank_consistent(&self) -> bool {
        self.rank_inconsistencies.is_empty()
    }

    /// The owner's p95 over the unfiltered run's.
    pub fn latency_ratio(&self) -> f64 {
        if self.unfiltered_p95_ms > 0.0 {
            self.owner_p95_ms / self.unfiltered_p95_ms
        } else {
            1.0
        }
    }

    /// Hits the conjuncts removed, over every principal.
    pub fn removed(&self) -> usize {
        self.principals.iter().map(|p| p.removed).sum()
    }

    /// What breaks `[access]`, one line each (empty = passed). A run that tests nothing
    /// (nobody refused anything, a principal with no hits, no forbidden by-id pair, an
    /// allowed by-id read refused) fails too, so the gate never passes vacuously.
    pub fn violations(&self, g: &AccessGates) -> Vec<String> {
        let list = |items: &[String]| {
            let mut s: Vec<&str> = items.iter().take(10).map(String::as_str).collect();
            if items.len() > 10 {
                s.push("…");
            }
            s.join("; ")
        };
        let mut v = Vec::new();
        if self.leaks.len() as u64 > u64::from(g.leaks_max) {
            v.push(format!(
                "leaks: {} hits a principal can't read (max {}): {}",
                self.leaks.len(),
                g.leaks_max,
                list(&self.leaks)
            ));
        }
        if self.by_id_leaks.len() as u64 > u64::from(g.by_id_leaks_max) {
            v.push(format!(
                "by_id_leaks: {} (max {}): {}",
                self.by_id_leaks.len(),
                g.by_id_leaks_max,
                list(&self.by_id_leaks)
            ));
        }
        if g.owner_rank_identical && !self.owner_rank_identical() {
            v.push(format!(
                "owner_rank_identical: the owner's ranked ids differ from the unfiltered run's \
                 on {} of {} queries: {}",
                self.owner_mismatches.len(),
                self.queries,
                list(&self.owner_mismatches)
            ));
        }
        if g.rank_consistent && !self.rank_consistent() {
            v.push(format!(
                "rank_consistent: the filter reordered relevant hits in {} of {} searches: {}",
                self.rank_inconsistencies.len(),
                self.rank_checked,
                list(&self.rank_inconsistencies)
            ));
        }
        if self.latency_ratio() > g.p95_latency_max_ratio {
            v.push(format!(
                "p95_latency_max_ratio: the owner's p95 {:.2} ms is {:.2}x the unfiltered \
                 {:.2} ms (max {:.2}x)",
                self.owner_p95_ms,
                self.latency_ratio(),
                self.unfiltered_p95_ms,
                g.p95_latency_max_ratio
            ));
        }
        if self.removed() == 0 {
            v.push(
                "the fixture's audiences removed nothing from anyone: the gate tests nothing"
                    .into(),
            );
        }
        for p in self.principals.iter().filter(|p| p.hits == 0) {
            v.push(format!(
                "{} found nothing at all: the gate tests nothing",
                p.name
            ));
        }
        if self.by_id_checked == 0 {
            v.push("no by-id read was forbidden: the by-id gate tests nothing".into());
        }
        if !self.by_id_refused.is_empty() {
            v.push(format!(
                "{} allowed by-id reads came back empty: {}",
                self.by_id_refused.len(),
                list(&self.by_id_refused)
            ));
        }
        v
    }
}

/// The ids of the hits relevant to `q`, in rank order.
fn relevant_ids(hits: &[Hit], q: &RetrievalQuery) -> Vec<String> {
    hits.iter()
        .filter(|h| !judge(h, &q.relevant).is_empty())
        .map(|h| h.passage.id.clone())
        .collect()
}

/// The ranked ids.
fn ids(r: &Ranked) -> Vec<String> {
    r.hits.iter().map(|h| h.passage.id.clone()).collect()
}

/// The access suite over a freshly built `index` (every passage still `company`). Stamps
/// the fixture's audiences through the service's writer, then:
///
/// - searches as the owner through the `Reader` and through the unfiltered replay
///   (`UnfilteredRetriever`: the same statements without the conjuncts), same vectors,
///   same process: the ranked ids must be equal (`owner_rank_identical`); the p95s are
///   compared over `LATENCY_ROUNDS` interleaved rounds;
/// - searches every golden query as every principal and checks each hit with the oracle
///   (`leaks`), counting what the conjuncts removed from the unfiltered run;
/// - for every principal who reads every relevant document of a query, checks the
///   relevant hits come in the owner's relative order (`rank_consistent`);
/// - reads every (principal, document) pair by id: forbidden ones must come back empty
///   (`by_id_leaks`), allowed ones must not (`by_id_refused`).
pub async fn run(
    index: &EvalIndex<'_>,
    fx: &AccessFixture,
    golden: &[RetrievalQuery],
) -> anyhow::Result<AccessReport> {
    run_timed(index, fx, golden, LATENCY_ROUNDS).await
}

/// `run` with `rounds` (≥ 1) timings of the owner's searches.
pub async fn run_timed(
    index: &EvalIndex<'_>,
    fx: &AccessFixture,
    golden: &[RetrievalQuery],
    rounds: usize,
) -> anyhow::Result<AccessReport> {
    fx.stamp(index).await?;
    let paths = index.document_paths();
    let owner = fx.owner_reader();
    if let Some(d) = paths.iter().find(|d| !fx.may_read(&owner, d)) {
        anyhow::bail!(
            "the owner {} (in every group) can't read {d} by {ACCESS_FILE}: \
             owner_rank_identical needs an owner who reads every document",
            fx.owner
        );
    }
    let unfiltered =
        UnfilteredRetriever::new(index.store.clone(), &index.pipeline.config.retriever)?;
    let purpose = index.retrieval_purpose();
    let mut report = AccessReport {
        embedder: index.vectors.version().to_string(),
        documents: paths.len(),
        queries: golden.len(),
        owner: fx.owner.clone(),
        ..AccessReport::default()
    };

    // The owner, through the reader and unfiltered: ranks and latency.
    let mut owner_hits: Vec<Vec<Hit>> = Vec::with_capacity(golden.len());
    let mut open_hits: Vec<Vec<Hit>> = Vec::with_capacity(golden.len());
    let mut owner_ms = vec![f64::MAX; golden.len()];
    let mut open_ms = vec![f64::MAX; golden.len()];
    for round in 0..rounds.max(1) {
        for (i, q) in golden.iter().enumerate() {
            let plan = std::slice::from_ref(&q.query);
            let scope = scope_of(q.scope);
            let filtered = async || index.search_as(&owner, plan, &scope, purpose).await;
            let open = async || {
                index
                    .search_with(&unfiltered, &owner, plan, &scope, purpose)
                    .await
            };
            let (a, b) = if round % 2 == 0 {
                let a = filtered().await?;
                (a, open().await?)
            } else {
                let b = open().await?;
                (filtered().await?, b)
            };
            owner_ms[i] = owner_ms[i].min(a.ms);
            open_ms[i] = open_ms[i].min(b.ms);
            if round == 0 {
                if ids(&a) != ids(&b) {
                    report.owner_mismatches.push(q.id.clone());
                }
                owner_hits.push(a.hits);
                open_hits.push(b.hits);
            }
        }
    }
    report.owner_p95_ms = percentile(&owner_ms, 95.0);
    report.unfiltered_p95_ms = percentile(&open_ms, 95.0);

    // Every principal: leaks, what was removed, the relative order of relevant hits.
    for name in fx.principals() {
        let reader = fx.reader(&name)?;
        let mut s = PrincipalSummary {
            name: name.clone(),
            reads: paths.iter().filter(|d| fx.may_read(&reader, d)).count(),
            ..PrincipalSummary::default()
        };
        for (i, q) in golden.iter().enumerate() {
            let plan = std::slice::from_ref(&q.query);
            let ranked = index
                .search_as(&reader, plan, &scope_of(q.scope), purpose)
                .await?;
            s.hits += ranked.hits.len();
            for h in &ranked.hits {
                if !fx.may_read(&reader, &h.passage.document_id) {
                    s.leaks += 1;
                    report.leaks.push(format!(
                        "{name} found {} for {}",
                        h.passage.document_id, q.id
                    ));
                }
            }
            s.removed += open_hits[i]
                .iter()
                .filter(|h| !fx.may_read(&reader, &h.passage.document_id))
                .count();
            let reads_all = !q.relevant.is_empty()
                && q.relevant.iter().all(|r| fx.may_read(&reader, &r.document));
            if reads_all {
                report.rank_checked += 1;
                let mine = relevant_ids(&ranked.hits, q);
                let theirs = relevant_ids(&owner_hits[i], q);
                let in_both = |a: &[String], b: &[String]| -> Vec<String> {
                    a.iter().filter(|id| b.contains(id)).cloned().collect()
                };
                if in_both(&mine, &theirs) != in_both(&theirs, &mine) {
                    report
                        .rank_inconsistencies
                        .push(format!("{name} on {}", q.id));
                }
            }
        }
        report.principals.push(s);
    }

    // By id: every forbidden pair empty, every allowed pair with text not.
    let mut has_text = Vec::with_capacity(paths.len());
    for path in &paths {
        let rows = index.store.passages_of(path).await?;
        has_text.push(rows.iter().any(|r| r.modality == "text"));
    }
    for name in fx.principals() {
        let reader = fx.reader(&name)?;
        for (path, has_text) in paths.iter().zip(&has_text) {
            let outline = document_outline(&index.store, &reader, path).await?;
            let rows = read_passages(
                &index.store,
                &reader,
                path,
                Selector::Ords { from: 0, to: 9999 },
                10_000,
            )
            .await?;
            if !fx.may_read(&reader, path) {
                report.by_id_checked += 1;
                if !outline.is_empty() || !rows.is_empty() {
                    report.by_id_leaks.push(format!("{name} read {path} by id"));
                }
            } else if *has_text && rows.is_empty() {
                report
                    .by_id_refused
                    .push(format!("{name} couldn't read {path} by id"));
            }
        }
    }
    Ok(report)
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
        // The owner, in every group, reads every document.
        let owner = fx.owner_reader();
        assert!(owner.paths.contains(&"company/sales".to_string()));
        assert!(owner.paths.contains(&"person/alice".to_string()));
        for d in [
            "bayes.md",
            "photosynthesis.md",
            "folder/notes/a/b.md",
            "sorting.html",
        ] {
            assert!(fx.may_read(&owner, d), "{d}");
        }
    }

    #[test]
    fn a_report_that_tests_nothing_fails() {
        let v = AccessReport::default().violations(&AccessGates::default());
        assert!(v.iter().any(|l| l.contains("removed nothing")), "{v:?}");
        assert!(v.iter().any(|l| l.contains("no by-id read")), "{v:?}");
        let r = AccessReport {
            principals: vec![PrincipalSummary {
                name: "bob".into(),
                reads: 3,
                hits: 10,
                removed: 4,
                leaks: 0,
            }],
            by_id_checked: 2,
            owner_mismatches: vec!["q1".into()],
            rank_checked: 1,
            rank_inconsistencies: vec!["bob on q2".into()],
            owner_p95_ms: 3.0,
            unfiltered_p95_ms: 1.0,
            ..AccessReport::default()
        };
        let v = r.violations(&AccessGates::default());
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v[0].starts_with("owner_rank_identical") && v[0].contains("q1"));
        assert!(v[1].starts_with("rank_consistent"));
        assert!(v[2].starts_with("p95_latency_max_ratio"));
        // Gates switched off don't fire.
        let off = AccessGates {
            owner_rank_identical: false,
            rank_consistent: false,
            p95_latency_max_ratio: 4.0,
            ..AccessGates::default()
        };
        assert!(r.violations(&off).is_empty());
    }

    /// The suite catches a retriever that ignores the reader: the pipeline's
    /// `hybrid-rrf` replaced by the unfiltered replay leaks, and only the leak gate
    /// fires (the by-id reads still carry the conjuncts; the owner's ranks still match).
    #[tokio::test]
    async fn a_retriever_without_the_reader_leaks() {
        use std::sync::Arc;

        use margin_service::db::Store;
        use margin_service::pipeline::StageCtor;
        use margin_shared::pipeline::PipelineConfig;

        use crate::corpus::{Extraction, documents};
        use crate::recorded::FakeVectors;
        use crate::runner::Paths;

        let evals = evals_dir();
        let corpus = evals.join("corpus");
        let docs = documents(&corpus).unwrap();
        let extraction = Extraction::fixture(&corpus, &docs).unwrap();
        let vectors = FakeVectors;
        let index = EvalIndex::build(
            Store::memory().await.unwrap(),
            &PipelineConfig::default(),
            |reg| {
                reg.register(
                    "hybrid-rrf",
                    StageCtor::Retriever(Arc::new(|cfg, deps| {
                        Ok(Arc::new(UnfilteredRetriever::new(deps.store.clone(), cfg)?))
                    })),
                )
            },
            &vectors,
            &corpus,
            &docs,
            &extraction,
        )
        .await
        .unwrap();
        let fx = AccessFixture::load(&evals.join(ACCESS_FILE)).unwrap();
        let golden = Paths::same(&evals).retrieval_golden().unwrap();
        let r = run_timed(&index, &fx, &golden, 1).await.unwrap();
        assert!(!r.leaks.is_empty());
        assert!(
            r.leaks
                .iter()
                .any(|l| l.starts_with("carol found fourier-series.md"))
        );
        assert!(r.by_id_leaks.is_empty() && r.by_id_refused.is_empty());
        assert!(r.owner_rank_identical());
        let v = r.violations(&AccessGates {
            p95_latency_max_ratio: f64::MAX,
            ..AccessGates::default()
        });
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].starts_with("leaks: "), "{v:?}");
    }
}
