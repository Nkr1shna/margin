//! The Claude prompt-prefix gate (`docs/agent-providers-brief.md` D12, §5 P-evals): the
//! CI check that the Claude client's prompt caching sits on R1's constant-first prompts
//! the way the brief says, read from the request bodies a fake Messages server
//! recorded.
//!
//! What a recorded body is: the JSON body of one `POST /v1/messages`, exactly as the
//! client sent it (the fake keeps them in order, the way the fake Responses server keeps
//! its requests for R1's OpenAI gate). For a plain text call (`TextModel::stream_parts`
//! with `CallOptions.cache_prefix_len = Some(prompt.constant.len())`) the brief's shape is
//!
//! ```json
//! "system": [
//!   {"type": "text", "text": "<the prompt's constant block>", "cache_control": {"type": "ephemeral"}},
//!   {"type": "text", "text": "<the rest of the instructions: the variable part>"}
//! ]
//! ```
//!
//! with the second block left out when the prompt has no variable part (the API rejects
//! an empty text block), and no other `cache_control` anywhere in the body (one
//! breakpoint on a text call).
//!
//! The invariant: for one `prompt_version`, the bytes before the breakpoint are the
//! same on every call. So `system[0].text` must be byte-identical to that version's
//! constant block and carry the marker; the marker never sits after variable text; the
//! variable part is `system[1]`, unmarked; and the constant block is sent nowhere else.

use std::collections::BTreeMap;

use margin_ai::prompts::{self, Prompt};
use serde_json::{Value, json};

/// The constant-first prompts whose calls run on the Claude client's `TextModel`
/// (lessons, scenes, the planner, the reranker, the query rewriter), by version.
pub fn known_constants() -> Vec<(&'static str, &'static str)> {
    vec![
        (prompts::LESSON_VERSION, prompts::LESSON_CONSTANT),
        (prompts::SCENE_VERSION, prompts::SCENE_CONSTANT.as_str()),
        (prompts::PLAN_VERSION, prompts::PLAN_CONSTANT),
        (prompts::RERANK_VERSION, prompts::RERANK_CONSTANT),
        (prompts::REWRITE_VERSION, prompts::REWRITE_CONSTANT),
    ]
}

/// What the gate found over a set of recorded bodies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrefixReport {
    /// Bodies checked.
    pub calls: usize,
    /// Bodies per prompt version whose `system[0]` is that version's constant block.
    pub per_version: BTreeMap<String, usize>,
    /// Every way a body breaks the invariant; empty = pass.
    pub violations: Vec<String>,
}

/// The ephemeral cache marker the client puts on the end of the constant block.
fn is_marker(v: Option<&Value>) -> bool {
    v == Some(&json!({"type": "ephemeral"}))
}

/// The `cache_control` markers anywhere in `v`.
fn breakpoints(v: &Value) -> usize {
    match v {
        Value::Object(m) => {
            usize::from(m.contains_key("cache_control"))
                + m.values().map(breakpoints).sum::<usize>()
        }
        Value::Array(a) => a.iter().map(breakpoints).sum(),
        _ => 0,
    }
}

/// The text of a `system` block that is `{"type":"text","text":…}`.
fn block_text(b: &Value) -> Option<&str> {
    (b.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| b.get("text").and_then(Value::as_str))
        .flatten()
}

/// Checks recorded bodies against the prompts in `known` (version, constant block).
/// Each body's `system[0]` must be one known constant, byte for byte, with the marker.
pub fn check_recorded(bodies: &[Value], known: &[(&str, &str)]) -> PrefixReport {
    let mut report = PrefixReport {
        calls: bodies.len(),
        ..Default::default()
    };
    for (i, body) in bodies.iter().enumerate() {
        let mut v = Vec::new();
        if let Some(version) = check_body(body, known, None, &mut v) {
            *report.per_version.entry(version).or_default() += 1;
        }
        report
            .violations
            .extend(v.into_iter().map(|m| format!("request {i}: {m}")));
    }
    report
}

/// Checks one recorded body against the prompt that produced it: on top of
/// [`check_recorded`]'s rules, `system[1]` must be exactly the rest of
/// `prompt.instructions()` after its constant block.
pub fn check_call(body: &Value, prompt: &Prompt) -> Vec<String> {
    let instructions = prompt.instructions();
    let rest = &instructions[prompt.constant.len()..];
    let mut v = Vec::new();
    check_body(
        body,
        &[(prompt.version, prompt.constant)],
        Some(rest),
        &mut v,
    );
    v
}

/// The version `body`'s `system[0]` belongs to, when it is a known constant block;
/// pushes what is wrong onto `out`.
fn check_body(
    body: &Value,
    known: &[(&str, &str)],
    expected_rest: Option<&str>,
    out: &mut Vec<String>,
) -> Option<String> {
    let system = match body.get("system") {
        Some(Value::Array(blocks)) if !blocks.is_empty() => blocks,
        Some(Value::String(_)) => {
            out.push("`system` is a plain string: no cache breakpoint at all".into());
            return None;
        }
        _ => {
            out.push("no `system` blocks".into());
            return None;
        }
    };
    let Some(first) = block_text(&system[0]) else {
        out.push("`system[0]` isn't a text block".into());
        return None;
    };
    if !is_marker(system[0].get("cache_control")) {
        out.push("`system[0]` carries no `cache_control: {\"type\": \"ephemeral\"}` marker".into());
    }
    let version = match known.iter().find(|(_, c)| *c == first) {
        Some((version, _)) => Some(version.to_string()),
        None => {
            if let Some((version, constant)) = known.iter().find(|(_, c)| first.starts_with(c)) {
                out.push(format!(
                    "`system[0]` is the {version} constant block plus {} more bytes: the \
                     breakpoint sits after variable text",
                    first.len() - constant.len()
                ));
            } else if let Some((version, _)) = known.iter().find(|(_, c)| c.starts_with(first)) {
                out.push(format!(
                    "`system[0]` is the first {} bytes of the {version} constant block: the \
                     breakpoint cuts the constant block short",
                    first.len()
                ));
            } else {
                out.push(format!(
                    "`system[0]` ({} bytes) is no known prompt's constant block, byte for \
                     byte",
                    first.len()
                ));
            }
            None
        }
    };
    match system.len() {
        1 => {
            if let Some(rest) = expected_rest
                && !rest.is_empty()
            {
                out.push(format!(
                    "the variable part ({} bytes) is missing: no `system[1]`",
                    rest.len()
                ));
            }
        }
        2 => match block_text(&system[1]) {
            None => out.push("`system[1]` isn't a text block".into()),
            Some(second) => {
                if system[1].get("cache_control").is_some() {
                    out.push(
                        "`system[1]` (the variable part) carries a cache marker: a cache \
                         entry nobody reads"
                            .into(),
                    );
                }
                if second.is_empty() {
                    out.push("`system[1]` is an empty text block (the API rejects it)".into());
                }
                if let Some(rest) = expected_rest
                    && second != rest
                {
                    out.push(format!(
                        "`system[1]` isn't the rest of the instructions ({} bytes sent, {} \
                         expected)",
                        second.len(),
                        rest.len()
                    ));
                }
                if known.iter().any(|(_, c)| second.contains(c)) {
                    out.push("`system[1]` repeats a constant block".into());
                }
            }
        },
        n => out.push(format!(
            "{n} `system` blocks: a text call has the constant block and at most the \
             variable part"
        )),
    }
    let marks = breakpoints(body);
    if marks != 1 {
        out.push(format!(
            "{marks} cache breakpoints in the body: a text call has exactly one, at the end \
             of the constant block"
        ));
    }
    if let Some(messages) = body.get("messages") {
        let sent = messages.to_string();
        if let Some((version, _)) = known.iter().find(|(_, c)| {
            serde_json::to_string(c)
                .map(|quoted| sent.contains(quoted.trim_matches('"')))
                .unwrap_or(false)
        }) {
            out.push(format!(
                "the {version} constant block is also sent in `messages`"
            ));
        }
    }
    version
}

/// The bodies' `system[0]` texts per version are all one value (byte-identical across
/// calls). Implied by [`check_recorded`] matching each against its constant; this is
/// the check without a list of constants, for bodies grouped by the caller.
pub fn identical_prefixes<'a>(bodies: impl IntoIterator<Item = &'a Value>) -> bool {
    let mut first: Option<&str> = None;
    for b in bodies {
        let text = b.get("system").and_then(|s| s.get(0)).and_then(block_text);
        match (first, text) {
            (_, None) => return false,
            (None, Some(t)) => first = Some(t),
            (Some(f), Some(t)) if f != t => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSTANT: &str = "You write one lesson. Constant rules.";

    fn known() -> Vec<(&'static str, &'static str)> {
        vec![("lesson-v9", CONSTANT), ("scene-v9", "Scene rules.")]
    }

    fn body(system: Value) -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 64000,
            "stream": true,
            "system": system,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Write it."}]}],
        })
    }

    fn good(rest: &str) -> Value {
        body(json!([
            {"type": "text", "text": CONSTANT, "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": rest},
        ]))
    }

    fn prompt(variable: &str) -> Prompt {
        Prompt {
            job: "lesson",
            version: "lesson-v9",
            constant: CONSTANT,
            variable: variable.into(),
            input: vec![],
            format: None,
        }
    }

    #[test]
    fn well_formed_bodies_pass() {
        let bodies = [
            good("\n\nThis lesson: intro."),
            good("\n\nThis lesson: advanced, with a quiz."),
            body(json!([
                {"type": "text", "text": "Scene rules.", "cache_control": {"type": "ephemeral"}},
            ])),
        ];
        let r = check_recorded(&bodies, &known());
        assert_eq!(r.violations, Vec::<String>::new());
        assert_eq!(r.calls, 3);
        assert_eq!(r.per_version["lesson-v9"], 2);
        assert_eq!(r.per_version["scene-v9"], 1);
        assert!(identical_prefixes(&bodies[..2]));
        assert!(!identical_prefixes(&bodies));
        assert!(check_call(&bodies[0], &prompt("This lesson: intro.")).is_empty());
        let scene = Prompt {
            version: "scene-v9",
            constant: "Scene rules.",
            ..prompt("")
        };
        assert!(check_call(&bodies[2], &scene).is_empty());
    }

    #[test]
    fn each_way_of_breaking_the_cache_prefix_is_caught() {
        let cases: Vec<(Value, &str)> = vec![
            (body(json!(CONSTANT)), "plain string"),
            (body(json!([])), "no `system` blocks"),
            (
                body(json!([{"type": "text", "text": CONSTANT}])),
                "no `cache_control",
            ),
            (
                body(json!([{"type": "text",
                    "text": format!("{CONSTANT}\n\nThis lesson: intro."),
                    "cache_control": {"type": "ephemeral"}}])),
                "breakpoint sits after variable text",
            ),
            (
                body(json!([
                    {"type": "text", "text": &CONSTANT[..10], "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": &CONSTANT[10..]},
                ])),
                "cuts the constant block short",
            ),
            (
                body(json!([{"type": "text", "text": "Something else.",
                    "cache_control": {"type": "ephemeral"}}])),
                "no known prompt's constant block",
            ),
            (
                body(json!([
                    {"type": "text", "text": CONSTANT, "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "\n\nx", "cache_control": {"type": "ephemeral"}},
                ])),
                "`system[1]` (the variable part) carries a cache marker",
            ),
            (
                body(json!([
                    {"type": "text", "text": CONSTANT, "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": ""},
                ])),
                "empty text block",
            ),
            (
                body(json!([
                    {"type": "text", "text": CONSTANT, "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "a"},
                    {"type": "text", "text": "b"},
                ])),
                "3 `system` blocks",
            ),
            (
                {
                    let mut b = good("\n\nx");
                    b["messages"][0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
                    b
                },
                "2 cache breakpoints",
            ),
            (
                {
                    let mut b = good("\n\nx");
                    b["messages"][0]["content"][0]["text"] = json!(format!("{CONSTANT} again"));
                    b
                },
                "also sent in `messages`",
            ),
        ];
        for (b, want) in cases {
            let r = check_recorded(std::slice::from_ref(&b), &known());
            let all = r.violations.join("\n");
            assert!(all.contains(want), "{want} not in:\n{all}");
            assert!(all.starts_with("request 0: "), "{all}");
        }
    }

    #[test]
    fn the_variable_part_must_be_the_rest_of_the_instructions() {
        let p = prompt("This lesson: intro.");
        let v = check_call(&good("\n\nThis lesson: other."), &p);
        assert!(v[0].contains("isn't the rest of the instructions"), "{v:?}");
        let v = check_call(
            &body(json!([{"type": "text", "text": CONSTANT,
                "cache_control": {"type": "ephemeral"}}])),
            &p,
        );
        assert!(
            v[0].contains("the variable part (21 bytes) is missing"),
            "{v:?}"
        );
    }

    #[test]
    fn the_real_prompts_are_known() {
        let k = known_constants();
        assert_eq!(k.len(), 5);
        assert!(k.iter().all(|(v, c)| !v.is_empty() && c.len() > 512));
        // Distinct, and none is a prefix of another (the gate tells them apart).
        for (i, (_, a)) in k.iter().enumerate() {
            for (j, (_, b)) in k.iter().enumerate() {
                assert!(i == j || !a.starts_with(b), "{i} {j}");
            }
        }
    }

    /// The two R2 prompts (rerank, rewrite) on bodies shaped as the Claude client sends
    /// a constant-only prompt: `system` is the constant block alone with the marker.
    /// They pass the gate, are told apart, and a body that caches the input with them
    /// is caught.
    #[test]
    fn the_rerank_and_rewrite_prompts_pass_the_gate() {
        use margin_ai::{RerankCandidate, RewriteInput};
        use margin_shared::pipeline::RetrievalPurpose;

        let candidates = [RerankCandidate::new("c1", "dsp.pdf", Some(12), "", "a_n.")];
        let mut sent = Vec::new();
        for (query, purpose) in [
            ("gibbs", RetrievalPurpose::Agent),
            ("Fourier coefficients", RetrievalPurpose::Lesson),
        ] {
            sent.push(prompts::rewrite(&RewriteInput {
                query: query.into(),
                purpose,
                scope_names: vec!["dsp.pdf".into()],
                max_added: 2,
            }));
            sent.push(prompts::rerank(
                &format!("{query}\nAlso asked: why"),
                &candidates,
            ));
        }
        let shaped = |p: &Prompt, cache_input: bool| {
            let text = if cache_input {
                format!("{}{}", p.instructions(), p.input_text())
            } else {
                p.instructions()
            };
            body(json!([{"type": "text", "text": text,
                "cache_control": {"type": "ephemeral"}}]))
        };
        let bodies: Vec<Value> = sent.iter().map(|p| shaped(p, false)).collect();
        let r = check_recorded(&bodies, &known_constants());
        assert_eq!(r.violations, Vec::<String>::new());
        assert_eq!(r.per_version[prompts::REWRITE_VERSION], 2);
        assert_eq!(r.per_version[prompts::RERANK_VERSION], 2);
        for (p, b) in sent.iter().zip(&bodies) {
            assert!(p.variable.is_empty(), "{}", p.version);
            assert!(check_call(b, p).is_empty(), "{}", p.version);
        }
        assert!(identical_prefixes([&bodies[0], &bodies[2]]));
        assert!(!identical_prefixes([&bodies[0], &bodies[1]]));
        let bad = shaped(&sent[0], true);
        let r = check_recorded(std::slice::from_ref(&bad), &known_constants());
        assert!(
            r.violations[0].contains("rewrite-v1 constant block plus"),
            "{:?}",
            r.violations
        );
    }
}
