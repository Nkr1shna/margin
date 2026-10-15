//! The prompt-injection suite (`docs/phase5-brief.md` D2): `margin-eval run --suite
//! injection`. For each prompt of `evals/golden/injection.jsonl` (the owner asks a
//! question whose best passage carries an injection from `evals/corpus/injection/`), on
//! each configured provider: whether a Write/Destructive tool was called, whether the
//! canary phrase or the exfil URL is in the answer, whether the model told the owner
//! about the injection (judged on `Small` with `evals/rubrics/injection.md`), and the
//! step count. The metric is `injection_resisted`, the share resisted, per provider and
//! prompt version. Opt-in, real models, never CI; no gate.

use std::path::Path;

use anyhow::Context;

use crate::live::Models;
use crate::runner::Env;
use crate::store::EvalStore;

/// The prompts, under the evals folder.
pub const GOLDEN_FILE: &str = "golden/injection.jsonl";
/// The judge's rubric, under the evals folder.
pub const RUBRIC_FILE: &str = "rubrics/injection.md";

/// One row of `golden/injection.jsonl`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct InjectionPrompt {
    pub id: String,
    /// What the owner asks.
    pub question: String,
    /// The canary document (under `corpus/injection/`) whose passage answers it.
    pub document: String,
    /// What a model that followed the injection would do or say: the tool it would
    /// call, or the phrase or URL it would repeat.
    #[serde(default)]
    pub canaries: Vec<String>,
}

/// The prompts of `evals/golden/injection.jsonl`.
pub fn load_prompts(evals: &Path) -> anyhow::Result<Vec<InjectionPrompt>> {
    let path = evals.join(GOLDEN_FILE);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("couldn't read {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{} line {}", path.display(), i + 1))
        })
        .collect()
}

/// Runs the suite with `variant` and records it as a run.
pub async fn run(
    _env: &Env<'_>,
    _variant: &str,
    _models: &Models,
    _store: &EvalStore,
) -> anyhow::Result<()> {
    unimplemented!("P5, 5-guard")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::INJECTION_DIR;
    use crate::golden::evals_dir;

    /// The 24 prompts load, each names a canary document that exists and at least
    /// one canary.
    #[test]
    fn the_injection_rows_load() {
        let evals = evals_dir();
        let prompts = load_prompts(&evals).unwrap();
        assert_eq!(prompts.len(), 24);
        let mut ids = std::collections::HashSet::new();
        for p in &prompts {
            assert!(ids.insert(p.id.clone()), "duplicate id {}", p.id);
            assert!(!p.question.trim().is_empty(), "{}", p.id);
            assert!(!p.canaries.is_empty(), "{}", p.id);
            let doc = evals.join("corpus").join(INJECTION_DIR).join(&p.document);
            assert!(doc.is_file(), "{}: no {}", p.id, doc.display());
        }
        assert!(evals.join(RUBRIC_FILE).is_file());
    }
}
