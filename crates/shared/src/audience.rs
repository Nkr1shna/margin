//! Audiences (roadmap L2, `docs/l2-brief.md` D3, D4): who may read a row, as the wire
//! body of `routes::audience`, and the one function that computes an audience from
//! others (`derive`).
//!
//! An audience entry is a `principal::PrincipalId`: a group path, `person/<id>` or
//! `agent/<id>`. A reader with expanded paths `P` (own id, every group and ancestor,
//! `company`; an agent: its owner's paths within its ceiling, and its own id) reads an
//! audience `A` iff `P ∩ A.access ≠ ∅ ∧ P ∩ A.deny = ∅`: exact membership, the
//! hierarchy being in the reader's expansion, never in the comparison.

use serde::{Deserialize, Serialize};

use crate::principal::{AGENT_PREFIX, PERSON_PREFIX, PrincipalId, is_group_path};

/// Most entries in an audience's `access`, and in its `deny`.
pub const AUDIENCE_MAX: usize = 64;

/// `PUT`/`GET routes::audience(unit, id)`: who may read a row (`access`) and who may
/// not even when `access` admits them (`deny`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Audience {
    #[serde(default)]
    pub access: Vec<PrincipalId>,
    #[serde(default)]
    pub deny: Vec<PrincipalId>,
}

/// What can have its audience set by hand in L2. Lessons and notes follow their topic;
/// chats stay private to their owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudienceUnit {
    /// Cascades to its sub-topics, lessons, notes, videos and sources.
    Topic,
    /// An attached folder: cascades to its files.
    Workspace,
    /// A source without a topic.
    Source,
    /// A document an agent ingested.
    Document,
}

impl AudienceUnit {
    pub const ALL: [AudienceUnit; 4] = [
        AudienceUnit::Topic,
        AudienceUnit::Workspace,
        AudienceUnit::Source,
        AudienceUnit::Document,
    ];

    /// The path segment of `routes::audience`.
    pub fn as_str(self) -> &'static str {
        match self {
            AudienceUnit::Topic => "topic",
            AudienceUnit::Workspace => "workspace",
            AudienceUnit::Source => "source",
            AudienceUnit::Document => "document",
        }
    }

    /// The unit a path segment names; `None` for anything else (a lesson, a note, a
    /// chat: 400 `access_code::NOT_A_UNIT`).
    pub fn parse(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.as_str() == segment)
    }
}

/// `GET routes::audience(unit, id)`: the stored audience, and where a row that follows
/// another (a lesson its topic) takes it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudienceView {
    pub audience: Audience,
    #[serde(default)]
    pub inherited_from: Option<(AudienceUnit, crate::Id)>,
}

/// Whether `entry` is in the alphabet of an audience: a group path, `person/<id>` or
/// `agent/<id>` (an id of letters, digits, `-` and `_`).
pub fn is_principal_id(entry: &str) -> bool {
    if is_group_path(entry) {
        return true;
    }
    let id = entry
        .strip_prefix(PERSON_PREFIX)
        .or_else(|| entry.strip_prefix(AGENT_PREFIX));
    id.is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Checks an audience's shape (not whether its groups, people and agents exist: the
/// service does that). The error is the sentence to show.
pub fn validate(a: &Audience) -> Result<(), String> {
    for (list, what) in [(&a.access, "readers"), (&a.deny, "denials")] {
        if list.len() > AUDIENCE_MAX {
            return Err(format!(
                "An audience can name at most {AUDIENCE_MAX} {what}; this one names {}. Use a group instead.",
                list.len()
            ));
        }
        for (i, entry) in list.iter().enumerate() {
            if !is_principal_id(entry) {
                return Err(format!(
                    "“{entry}” isn't a group, a person or an agent. Pick one from the list."
                ));
            }
            if list[..i].contains(entry) {
                return Err(format!("“{entry}” is named twice. Name it once."));
            }
        }
    }
    Ok(())
}

/// Whether a reader with expanded `paths` (and the ids a denial can name, `deny_ids`)
/// reads `a`: exact membership.
pub fn reads(paths: &[PrincipalId], deny_ids: &[PrincipalId], a: &Audience) -> bool {
    a.access.iter().any(|x| paths.contains(x)) && !a.deny.iter().any(|x| deny_ids.contains(x))
}

/// `a ⊑ p`: `a == p`, or `a` is a group path and an ancestor of the group path `p`.
/// `person/*` and `agent/*` are only equal to themselves.
fn covers(a: &str, p: &str) -> bool {
    a == p
        || (is_group_path(a)
            && is_group_path(p)
            && p.len() > a.len()
            && p.starts_with(a)
            && p.as_bytes()[a.len()] == b'/')
}

/// What `derive` computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub audience: Audience,
    /// The result is private to the creator: the sources' audiences share no reader
    /// the alphabet can name. The app shows a notice with "Publish…".
    pub narrowed: bool,
}

/// The audience of something written from `sources` (a lesson from its topic and the
/// passages it drew on): the widest audience whose every reader reads every source.
///
/// `access` = the minimal elements (ancestors before descendants) of `{ p ∈ ⋃ Aᵢ.access
/// : ∀ i, ∃ a ∈ Aᵢ.access, a ⊑ p }`, then `creator`; `deny` = `⋃ Aᵢ.deny − {creator}`.
/// Expansions are ancestor-closed, so a reader of `p` holds every `a ⊑ p`. Never given
/// a reader's paths: that would widen the result to the reader.
pub fn derive(creator: &PrincipalId, sources: &[Audience]) -> Derived {
    let mut union: Vec<&PrincipalId> = Vec::new();
    for a in sources {
        for p in &a.access {
            if !union.contains(&p) {
                union.push(p);
            }
        }
    }
    let common: Vec<&PrincipalId> = union
        .into_iter()
        .filter(|p| {
            sources
                .iter()
                .all(|a| a.access.iter().any(|x| covers(x, p)))
        })
        .collect();
    let minimal: Vec<PrincipalId> = common
        .iter()
        .filter(|p| !common.iter().any(|q| q != *p && covers(q, p)))
        .map(|p| (*p).clone())
        .collect();
    let narrowed = minimal.iter().all(|p| p == creator);
    let mut access = vec![creator.clone()];
    access.extend(minimal.into_iter().filter(|p| p != creator));
    let mut deny: Vec<PrincipalId> = Vec::new();
    for a in sources {
        for d in &a.deny {
            if d != creator && !deny.contains(d) {
                deny.push(d.clone());
            }
        }
    }
    Derived {
        audience: Audience { access, deny },
        narrowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::ancestors;

    fn a(access: &[&str]) -> Audience {
        Audience {
            access: access.iter().map(|s| s.to_string()).collect(),
            deny: Vec::new(),
        }
    }

    const ALICE: &str = "person/alice";

    #[test]
    fn worked_examples() {
        let d = derive(
            &ALICE.into(),
            &[
                a(&["company/engineering"]),
                a(&["company/engineering/platform"]),
            ],
        );
        assert_eq!(d.audience.access, [ALICE, "company/engineering/platform"]);
        assert!(!d.narrowed);
        let d = derive(
            &ALICE.into(),
            &[a(&["company"]), a(&["company/engineering"])],
        );
        assert_eq!(d.audience.access, [ALICE, "company/engineering"]);
        let d = derive(
            &ALICE.into(),
            &[a(&["company/sales"]), a(&["company/engineering"])],
        );
        assert_eq!(d.audience.access, [ALICE]);
        assert!(d.narrowed);
        let d = derive(
            &ALICE.into(),
            &[
                a(&["person/bob", "company/eng"]),
                a(&["person/bob", "company/hr"]),
            ],
        );
        assert_eq!(d.audience.access, [ALICE, "person/bob"]);
        assert!(!d.narrowed);
        let same = a(&["company/engineering", "person/bob"]);
        let d = derive(&ALICE.into(), &[same.clone(), same.clone()]);
        assert_eq!(
            d.audience.access,
            [ALICE, "company/engineering", "person/bob"]
        );
        // The creator alone is narrowed; denials add up, never the creator's own.
        let d = derive(
            &ALICE.into(),
            &[
                Audience {
                    access: vec!["company".into(), ALICE.into()],
                    deny: vec!["person/bob".into(), ALICE.into()],
                },
                Audience {
                    access: vec![ALICE.into()],
                    deny: vec!["agent/x".into()],
                },
            ],
        );
        assert_eq!(d.audience.access, [ALICE]);
        assert_eq!(d.audience.deny, ["person/bob", "agent/x"]);
        assert!(d.narrowed);
        // `company/eng` is not an ancestor of `company/engineering`.
        let d = derive(
            &ALICE.into(),
            &[a(&["company/eng"]), a(&["company/engineering"])],
        );
        assert!(d.narrowed);
    }

    #[test]
    fn validation() {
        assert!(validate(&a(&["company", "company/a-1", "person/01J", "agent/x_y"])).is_ok());
        assert!(validate(&a(&["engineering"])).is_err());
        assert!(validate(&a(&["person/"])).is_err());
        assert!(validate(&a(&["person/a/b"])).is_err());
        assert!(validate(&a(&["company", "company"])).is_err());
        let many: Vec<String> = (0..=AUDIENCE_MAX).map(|i| format!("person/p{i}")).collect();
        assert!(
            validate(&Audience {
                access: many.clone(),
                deny: vec![]
            })
            .is_err()
        );
        assert!(
            validate(&Audience {
                access: vec![],
                deny: many
            })
            .is_err()
        );
        assert_eq!(AudienceUnit::parse("topic"), Some(AudienceUnit::Topic));
        assert_eq!(AudienceUnit::parse("lesson"), None);
        for u in AudienceUnit::ALL {
            assert_eq!(AudienceUnit::parse(u.as_str()), Some(u));
            let json = serde_json::to_value(u).unwrap();
            assert_eq!(json, u.as_str());
        }
        let view = AudienceView {
            audience: a(&["company"]),
            inherited_from: Some((AudienceUnit::Topic, "t".into())),
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(serde_json::from_value::<AudienceView>(json).unwrap(), view);
        let empty: Audience = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Audience::default());
    }

    /// A small deterministic generator (no dependency for one test).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn subset(&mut self, of: &[String], max: usize) -> Vec<String> {
            let n = 1 + self.below(max);
            let mut out = Vec::new();
            for _ in 0..n {
                let p = &of[self.below(of.len())];
                if !out.contains(p) {
                    out.push(p.clone());
                }
            }
            out
        }
    }

    /// Over random trees, audiences and readers: every reader of `derive(..)` reads
    /// every source, and no single entry the result lacks would add a reader who reads
    /// every source (maximality, on the minimal reader of each entry).
    #[test]
    fn derived_audiences_are_safe_and_maximal() {
        let mut rng = Lcg(42);
        let creator: PrincipalId = "person/creator".into();
        for _ in 0..300 {
            // A tree of up to 8 groups under `company`.
            let mut groups = vec!["company".to_string()];
            for i in 0..(1 + rng.below(8)) {
                let parent = groups[rng.below(groups.len())].clone();
                groups.push(format!("{parent}/g{i}"));
            }
            let people: Vec<String> = (0..3).map(|i| format!("person/p{i}")).collect();
            let agents: Vec<String> = (0..2).map(|i| format!("agent/a{i}")).collect();
            let alphabet: Vec<String> = groups
                .iter()
                .chain(&people)
                .chain(&agents)
                .cloned()
                .collect();
            let sources: Vec<Audience> = (0..(1 + rng.below(3)))
                .map(|_| Audience {
                    access: rng.subset(&alphabet, 3),
                    deny: if rng.below(3) == 0 {
                        rng.subset(&alphabet, 1)
                    } else {
                        Vec::new()
                    },
                })
                .collect();
            let d = derive(&creator, &sources);
            assert_eq!(d.audience.access[0], creator);
            assert_eq!(
                d.narrowed,
                d.audience.access.len() == 1,
                "{sources:?} → {d:?}"
            );
            // Random readers: some groups (ancestor-closed), maybe a person, maybe an
            // agent's own id.
            for _ in 0..20 {
                let mut paths: Vec<String> = Vec::new();
                for g in rng.subset(&groups, 3) {
                    for anc in ancestors(&g) {
                        if !paths.contains(&anc) {
                            paths.push(anc);
                        }
                    }
                }
                match rng.below(3) {
                    0 => paths.push(people[rng.below(people.len())].clone()),
                    1 => paths.push(agents[rng.below(agents.len())].clone()),
                    _ => {}
                }
                if reads(&paths, &paths, &d.audience) {
                    for s in &sources {
                        assert!(
                            reads(&paths, &paths, s),
                            "{paths:?} reads {d:?} but not {s:?}"
                        );
                    }
                }
            }
            // Maximality: the minimal reader of any entry outside the result that reads
            // every source reads the result already.
            for p in &alphabet {
                let paths = if is_group_path(p) {
                    ancestors(p)
                } else {
                    vec![p.clone()]
                };
                if sources.iter().all(|s| reads(&paths, &paths, s)) {
                    assert!(
                        reads(&paths, &paths, &d.audience),
                        "{p} reads every source of {sources:?} but not {d:?}"
                    );
                }
            }
        }
    }
}
