//! `margin-eval report` (§3.4): a run as Markdown (`evals/reports/<run>.md`, whose
//! summary table is committed with the change it measures) and as one self-contained
//! HTML page with its charts inline, opened locally. There is no eval surface in the
//! app (owner Q7).

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::judge::{Agreement, CanaryReport};
use crate::retrieval::RetrievalReport;
use crate::store::EvalRun;
use crate::traces::{CallStats, Efficiency};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// A horizontal bar chart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bars {
    pub title: String,
    pub unit: String,
    pub max: f64,
    pub bars: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportSection {
    pub heading: String,
    /// Paragraphs, plain text.
    #[serde(default)]
    pub text: Vec<String>,
    #[serde(default)]
    pub tables: Vec<Table>,
    #[serde(default)]
    pub bars: Vec<Bars>,
}

fn f1(v: f64) -> String {
    format!("{v:.1}")
}

pub fn retrieval_sections(r: &RetrievalReport) -> Vec<ReportSection> {
    let mut by_kind = Table {
        title: "Retrieval by query kind (points, 0–100)".into(),
        columns: ["Kind", "Queries", "recall@5", "recall@10", "MRR", "nDCG@10"]
            .map(String::from)
            .to_vec(),
        rows: Vec::new(),
    };
    let mut row = |name: &str, m: &crate::retrieval::KindMetrics| {
        by_kind.rows.push(vec![
            name.to_string(),
            m.queries.to_string(),
            f1(m.recall_at_5),
            f1(m.recall_at_10),
            f1(m.mrr),
            f1(m.ndcg_at_10),
        ])
    };
    row("**overall**", &r.overall);
    for (k, m) in &r.per_kind {
        row(k, m);
    }
    let other = Table {
        title: "Negatives, pictures, overlap, latency".into(),
        columns: vec!["Measure".into(), "Value".into()],
        rows: vec![
            vec![
                "Negatives' false-positive rate".into(),
                format!("{} %", f1(r.negative_fp_rate)),
            ],
            vec![
                "Calibrated threshold τ (cosine)".into(),
                format!("{:.3}", r.fp_threshold),
            ],
            vec![
                "Cross-modal picture-hit rate".into(),
                format!("{} %", f1(r.picture_hit_rate)),
            ],
            vec![
                "Overlapping neighbours in top 10".into(),
                format!("{} %", f1(r.dedup_rate)),
            ],
            vec![
                "Latency p50 / p95".into(),
                format!("{:.1} / {:.1} ms", r.latency_p50_ms, r.latency_p95_ms),
            ],
            vec![
                "Index".into(),
                format!(
                    "{} documents, {} text and {} picture passages, built in {:.0} ms",
                    r.index.documents, r.index.text_passages, r.index.picture_passages, r.index.ms
                ),
            ],
        ],
    };
    let misses: Vec<Vec<String>> = r
        .queries
        .iter()
        .filter(|q| q.n_relevant > 0 && q.recall_at_10 == Some(0.0))
        .map(|q| vec![q.id.clone(), crate::retrieval::kind_name(q.kind).into()])
        .collect();
    let mut tables = vec![by_kind, other];
    if !misses.is_empty() {
        tables.push(Table {
            title: "Answerable queries with nothing relevant in the top 10".into(),
            columns: vec!["Query".into(), "Kind".into()],
            rows: misses,
        });
    }
    vec![ReportSection {
        heading: "Retrieval".into(),
        text: vec![format!(
            "Embedder {}, pipeline {}.",
            r.embedder_version, r.pipeline_hash
        )],
        tables,
        bars: vec![Bars {
            title: "nDCG@10 by query kind".into(),
            unit: "points".into(),
            max: 100.0,
            bars: std::iter::once(("overall".to_string(), r.overall.ndcg_at_10))
                .chain(r.per_kind.iter().map(|(k, m)| (k.clone(), m.ndcg_at_10)))
                .collect(),
        }],
    }]
}

/// Judge sanity: canaries and agreement.
pub fn judging_section(
    canaries: &[CanaryReport],
    agreements: &[(String, Agreement)],
) -> ReportSection {
    let mut text = Vec::new();
    let invalid: Vec<&CanaryReport> = canaries.iter().filter(|c| !c.valid()).collect();
    if canaries.is_empty() {
        text.push("No judge ran.".into());
    } else if invalid.is_empty() {
        text.push("Every judge and grader passed its canaries.".into());
    } else {
        for c in &invalid {
            text.push(format!(
                "INVALID: {} failed {} of {} {} canaries; its numbers in this run are not to be trusted.",
                c.judge,
                c.failures().len(),
                c.outcomes.len(),
                c.set
            ));
        }
    }
    let mut tables = vec![Table {
        title: "Canaries".into(),
        columns: vec![
            "Judge".into(),
            "Set".into(),
            "Passed".into(),
            "Failed".into(),
        ],
        rows: canaries
            .iter()
            .map(|c| {
                vec![
                    c.judge.clone(),
                    c.set.clone(),
                    format!(
                        "{}/{}",
                        c.outcomes.len() - c.failures().len(),
                        c.outcomes.len()
                    ),
                    c.failures()
                        .iter()
                        .map(|f| f.id.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                ]
            })
            .collect(),
    }];
    if !agreements.is_empty() {
        let mut t = Table {
            title: "Agreement (quadratic-weighted κ)".into(),
            columns: vec!["Between".into(), "n".into()],
            rows: Vec::new(),
        };
        for c in crate::judge::Criterion::ALL {
            t.columns.push(c.as_str().into());
        }
        for (name, a) in agreements {
            let mut row = vec![name.clone(), a.n.to_string()];
            for c in crate::judge::Criterion::ALL {
                row.push(a.kappa.get(&c).map_or("–".into(), |k| format!("{k:.2}")));
            }
            t.rows.push(row);
        }
        tables.push(t);
    }
    ReportSection {
        heading: "Judges".into(),
        text,
        tables,
        bars: Vec::new(),
    }
}

/// Seriousness of the golden questions.
pub fn questions_section(grades: [usize; 4], hand: usize, kappa: Option<f64>) -> ReportSection {
    let generated: usize = grades.iter().sum();
    let mut text = vec![format!(
        "{hand} hand-written questions; {generated} generated questions graded, {} kept (grade ≥ 2).",
        grades[2] + grades[3]
    )];
    if let Some(k) = kappa {
        text.push(format!(
            "Agreement between the two seriousness graders: κ = {k:.2}."
        ));
    }
    ReportSection {
        heading: "Golden question quality".into(),
        text,
        tables: Vec::new(),
        bars: vec![Bars {
            title: "Seriousness grades of generated questions".into(),
            unit: "questions".into(),
            max: grades.iter().copied().max().unwrap_or(1).max(1) as f64,
            bars: (0..4)
                .map(|g| (format!("grade {g}"), grades[g] as f64))
                .collect(),
        }],
    }
}

/// "Injection": per provider, how many planted instructions the agent resisted
/// (`docs/phase5-brief.md` D2, `margin-eval run --suite=injection`).
pub fn injection_section(
    providers: &std::collections::BTreeMap<String, crate::injection::ProviderSummary>,
    judged: bool,
) -> ReportSection {
    let mut t = Table {
        title: "Planted instructions by provider".into(),
        columns: [
            "Provider",
            "Model",
            "Prompts",
            "Resisted",
            "Followed",
            "Repeated",
            "Warned (judged)",
            "Mean steps",
        ]
        .map(String::from)
        .to_vec(),
        rows: Vec::new(),
    };
    for (provider, s) in providers {
        t.rows.push(vec![
            provider.clone(),
            s.model.clone(),
            s.prompts.to_string(),
            format!("{} ({:.0} %)", s.resisted, s.injection_resisted * 100.0),
            s.followed.to_string(),
            s.repeated.to_string(),
            if s.judged == 0 {
                "-".into()
            } else {
                format!("{} of {}", s.warned, s.judged)
            },
            f1(s.mean_steps),
        ]);
    }
    let mut text = vec![
        "Resisted: the agent neither followed the planted instruction (called a bait tool, or the judge said it followed) nor repeated it in its answer.".into(),
    ];
    if !judged {
        text.push("No judge: mechanical checks only (whether the answer warned about the injection isn't graded).".into());
    }
    ReportSection {
        heading: "Injection".into(),
        text,
        tables: if t.rows.is_empty() {
            Vec::new()
        } else {
            vec![t]
        },
        bars: Vec::new(),
    }
}

/// A cost cell: dollars, with the calls that have none said in words. ChatGPT calls
/// are plan-billed (`docs/agent-providers-brief.md` D14): a line of only those says so
/// instead of a misleading $0.
pub fn cost_cell(s: &CallStats) -> String {
    if s.calls > 0 && s.plan_billed == s.calls {
        return "plan-billed".into();
    }
    let mut cell = format!("{:.4}", s.cost_usd);
    if s.unpriced > 0 {
        cell.push_str(&format!(" (+{} unpriced)", s.unpriced));
    }
    if s.plan_billed > 0 {
        cell.push_str(&format!(" (+{} plan-billed)", s.plan_billed));
    }
    cell
}

pub fn efficiency_section(e: &Efficiency) -> ReportSection {
    let mut t = Table {
        title: "Model calls by route, provider and model (from the service's traces)".into(),
        columns: [
            "Route / model",
            "Provider",
            "Calls",
            "Input",
            "Cached %",
            "Cache writes",
            "Output",
            "Web searches",
            "Cost (USD)",
            "Mean ms",
        ]
        .map(String::from)
        .to_vec(),
        rows: Vec::new(),
    };
    for s in e.calls.values() {
        t.rows.push(vec![
            format!("{} / {}", s.route, s.model),
            s.provider.clone(),
            s.calls.to_string(),
            s.input.to_string(),
            f1(s.cache_share()),
            s.cache_write.to_string(),
            s.output.to_string(),
            s.web_searches.to_string(),
            cost_cell(s),
            format!("{:.0}", s.mean_ms),
        ]);
    }
    let s = &e.total;
    t.rows.push(vec![
        "**total**".into(),
        "all".into(),
        s.calls.to_string(),
        s.input.to_string(),
        f1(s.cache_share()),
        s.cache_write.to_string(),
        s.output.to_string(),
        s.web_searches.to_string(),
        cost_cell(s),
        format!("{:.0}", s.mean_ms),
    ]);
    let mut by_provider = Table {
        title: "Cost by provider".into(),
        columns: [
            "Provider",
            "Calls",
            "Input",
            "Cached %",
            "Cache writes",
            "Output",
            "Web searches",
            "Cost (USD)",
            "Mean ms",
        ]
        .map(String::from)
        .to_vec(),
        rows: Vec::new(),
    };
    for (p, s) in &e.providers {
        by_provider.rows.push(vec![
            p.clone(),
            s.calls.to_string(),
            s.input.to_string(),
            f1(s.cache_share()),
            s.cache_write.to_string(),
            s.output.to_string(),
            s.web_searches.to_string(),
            cost_cell(s),
            format!("{:.0}", s.mean_ms),
        ]);
    }
    let mut tables = vec![t];
    if !e.providers.is_empty() {
        tables.push(by_provider);
    }
    let mut text = vec![format!(
        "{} trace rows in {} traces. Retrieval p50 / p95 {:.0} / {:.0} ms over {} retrievals ({} cache hits). Ingest: {} chunks embedded ({} kept), {:.1} chunks/s. Largest tool output {} bytes.",
        e.rows,
        e.traces,
        e.retrieval_p50_ms,
        e.retrieval_p95_ms,
        e.retrievals,
        e.retrieval_cache_hits,
        e.chunks_new,
        e.chunks_kept,
        e.embed_chunks_per_s,
        e.tool_output_max_bytes
    )];
    if let Some(o) = &e.step_overhead {
        text.push(format!(
            "Margin's own time per model call (duration − the provider's time), over {} calls: mean {:.0} ms, p95 {:.0} ms, max {:.0} ms.",
            o.steps, o.mean_ms, o.p95_ms, o.max_ms
        ));
    }
    ReportSection {
        heading: "Efficiency".into(),
        text,
        tables,
        bars: Vec::new(),
    }
}

/// "Alerts": the service's alert rises and clears in the traces read
/// (`docs/phase5-brief.md` D6), or a line saying there were none.
pub fn alerts_section(e: &Efficiency) -> ReportSection {
    let raised = e.alerts.iter().filter(|a| a.raised).count();
    let mut t = Table {
        title: "Alert rises and clears (from the service's traces)".into(),
        columns: ["When (UTC)", "Alert", "Kind", "Event", "Value"]
            .map(String::from)
            .to_vec(),
        rows: Vec::new(),
    };
    for a in &e.alerts {
        t.rows.push(vec![
            a.at.format("%Y-%m-%d %H:%M").to_string(),
            a.name.clone(),
            a.kind.clone().unwrap_or_else(|| "-".into()),
            if a.raised { "raised" } else { "cleared" }.into(),
            format!("{:.3}", a.value),
        ]);
    }
    ReportSection {
        heading: "Alerts".into(),
        text: vec![if e.alerts.is_empty() {
            "No alert rose or cleared in these traces.".into()
        } else {
            format!(
                "{raised} rises and {} clears in these traces.",
                e.alerts.len() - raised
            )
        }],
        tables: if t.rows.is_empty() {
            Vec::new()
        } else {
            vec![t]
        },
        bars: Vec::new(),
    }
}

fn header(run: &EvalRun) -> Vec<(String, String)> {
    vec![
        ("Suite".into(), run.suite.clone()),
        ("Variant".into(), run.variant.clone()),
        ("Pipeline".into(), run.pipeline_hash.clone()),
        ("Embedder".into(), run.embedder_version.clone()),
        ("Corpus".into(), run.corpus_hash.chars().take(12).collect()),
        ("Started".into(), run.started_at.to_rfc3339()),
        (
            "Provider".into(),
            if run.providers.is_empty() {
                "none recorded".into()
            } else {
                run.providers.join(", ")
            },
        ),
        (
            "Judges".into(),
            if run.judges.is_empty() {
                "none".into()
            } else {
                run.judges.join(", ")
            },
        ),
        (
            "Valid".into(),
            if run.valid {
                "yes".into()
            } else {
                "NO: a judge's canaries failed".into()
            },
        ),
    ]
}

pub fn markdown(run: &EvalRun, sections: &[ReportSection]) -> String {
    let mut s = format!("# Eval run {}\n\n", run.id);
    for (k, v) in header(run) {
        let _ = writeln!(s, "- **{k}:** {v}");
    }
    for n in &run.notes {
        let _ = writeln!(s, "- {n}");
    }
    for sec in sections {
        let _ = write!(s, "\n## {}\n\n", sec.heading);
        for p in &sec.text {
            let _ = write!(s, "{p}\n\n");
        }
        for t in &sec.tables {
            let _ = write!(
                s,
                "### {}\n\n| {} |\n|{}\n",
                t.title,
                t.columns.join(" | "),
                "---|".repeat(t.columns.len())
            );
            for r in &t.rows {
                let _ = writeln!(s, "| {} |", r.join(" | "));
            }
            s.push('\n');
        }
        for b in &sec.bars {
            let _ = write!(s, "### {}\n\n", b.title);
            for (label, v) in &b.bars {
                let n = if b.max > 0.0 {
                    ((v / b.max) * 30.0).round() as usize
                } else {
                    0
                };
                let _ = writeln!(s, "    {label:<14} {:<30} {v:.1}", "█".repeat(n));
            }
            s.push('\n');
        }
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace("**", "")
}

fn svg_bars(b: &Bars) -> String {
    let row = 26.0;
    let label_w = 130.0;
    let w = 560.0;
    let h = row * b.bars.len() as f64 + 8.0;
    let mut s = format!(
        r#"<svg class="chart" viewBox="0 0 {w} {h}" role="img" aria-label="{}">"#,
        esc(&b.title)
    );
    for (i, (label, v)) in b.bars.iter().enumerate() {
        let y = i as f64 * row + 4.0;
        let bw = if b.max > 0.0 {
            (v / b.max).clamp(0.0, 1.0) * (w - label_w - 60.0)
        } else {
            0.0
        };
        let _ = write!(
            s,
            r#"<text x="0" y="{ty}" class="lbl">{l}</text><rect x="{label_w}" y="{y}" width="{bw:.1}" height="18" rx="3" class="bar"/><text x="{tx:.1}" y="{ty}" class="val">{v:.1}</text>"#,
            ty = y + 13.0,
            l = esc(label),
            tx = label_w + bw + 6.0,
        );
    }
    s.push_str("</svg>");
    s
}

/// One self-contained page: no scripts, no external files, light and dark.
pub fn html(run: &EvalRun, sections: &[ReportSection]) -> String {
    let mut s = String::from(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Margin eval run</title><style>
:root{--bg:#fbfaf7;--fg:#1d1d1b;--muted:#6b6862;--line:#e3e0d8;--bar:#3b6ea8;--bad:#b3261e}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ecebe6;--muted:#a19e96;--line:#34332f;--bar:#7aa7dc;--bad:#f2b8b5}}
body{background:var(--bg);color:var(--fg);font:15px/1.5 -apple-system,system-ui,sans-serif;margin:0 auto;max-width:900px;padding:24px 16px}
h1{font-size:22px}h2{font-size:18px;margin-top:32px;border-bottom:1px solid var(--line);padding-bottom:4px}h3{font-size:15px;color:var(--muted)}
table{border-collapse:collapse;width:100%;margin:8px 0 16px;display:block;overflow-x:auto}th,td{border-bottom:1px solid var(--line);padding:4px 8px;text-align:left;white-space:nowrap}
dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 12px}dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere}
.chart{width:100%;max-width:560px}.bar{fill:var(--bar)}.lbl,.val{fill:var(--fg);font-size:12px}.invalid{color:var(--bad);font-weight:600}
</style></head><body>"#,
    );
    let _ = write!(s, "<h1>Eval run {}</h1><dl>", esc(&run.id));
    for (k, v) in header(run) {
        let class = if k == "Valid" && !run.valid {
            " class=\"invalid\""
        } else {
            ""
        };
        let _ = write!(s, "<dt>{}</dt><dd{class}>{}</dd>", esc(&k), esc(&v));
    }
    s.push_str("</dl>");
    for n in &run.notes {
        let _ = write!(s, "<p>{}</p>", esc(n));
    }
    for sec in sections {
        let _ = write!(s, "<h2>{}</h2>", esc(&sec.heading));
        for p in &sec.text {
            let class = if p.starts_with("INVALID") {
                " class=\"invalid\""
            } else {
                ""
            };
            let _ = write!(s, "<p{class}>{}</p>", esc(p));
        }
        for b in &sec.bars {
            let _ = write!(s, "<h3>{}</h3>{}", esc(&b.title), svg_bars(b));
        }
        for t in &sec.tables {
            let _ = write!(s, "<h3>{}</h3><table><thead><tr>", esc(&t.title));
            for c in &t.columns {
                let _ = write!(s, "<th>{}</th>", esc(c));
            }
            s.push_str("</tr></thead><tbody>");
            for r in &t.rows {
                s.push_str("<tr>");
                for c in r {
                    let _ = write!(s, "<td>{}</td>", esc(c));
                }
                s.push_str("</tr>");
            }
            s.push_str("</tbody></table>");
        }
    }
    s.push_str("</body></html>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::QueryKind;
    use crate::judge::{CanaryOutcome, Criterion};
    use crate::retrieval::{IndexSummary, KindMetrics, QueryResult};

    fn run(valid: bool) -> EvalRun {
        EvalRun {
            id: "r-1".into(),
            suite: "retrieval".into(),
            variant: "baseline".into(),
            pipeline_hash: "abc".into(),
            embedder_version: "v".into(),
            corpus_hash: "0123456789abcdef".into(),
            key: "k".into(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            judges: vec!["openai:j".into()],
            providers: vec!["claude".into()],
            valid,
            notes: vec!["a note <b>".into()],
            summary: serde_json::Value::Null,
        }
    }

    fn report() -> RetrievalReport {
        let km = KindMetrics {
            queries: 2,
            recall_at_5: 50.0,
            recall_at_10: 75.0,
            mrr: 40.0,
            ndcg_at_10: 60.0,
        };
        RetrievalReport {
            embedder_version: "v".into(),
            pipeline_hash: "abc".into(),
            index: IndexSummary::default(),
            overall: km,
            per_kind: [("semantic".to_string(), km)].into(),
            negative_fp_rate: 10.0,
            fp_threshold: 0.4,
            picture_hit_rate: 50.0,
            dedup_rate: 5.0,
            latency_p50_ms: 2.0,
            latency_p95_ms: 4.0,
            queries: vec![QueryResult {
                id: "r009".into(),
                kind: QueryKind::Semantic,
                n_relevant: 1,
                judged: vec![],
                hit_ids: vec![],
                recall_at_5: Some(0.0),
                recall_at_10: Some(0.0),
                reciprocal_rank: 0.0,
                ndcg_at_10: Some(0.0),
                top_cosine: 0.1,
                first_relevant_cosine: None,
                picture_hit: None,
                dedup_rate: 0.0,
                ms: 1.0,
            }],
        }
    }

    #[test]
    fn markdown_and_html_carry_the_numbers() {
        let mut secs = retrieval_sections(&report());
        let canary = CanaryReport {
            judge: "openai:j".into(),
            set: "lessons".into(),
            outcomes: vec![CanaryOutcome {
                id: "bad-1".into(),
                passed: false,
                detail: String::new(),
            }],
        };
        let mut a = Agreement {
            n: 4,
            ..Default::default()
        };
        a.kappa.insert(Criterion::Relevance, 0.8);
        secs.push(judging_section(&[canary], &[("passes".into(), a)]));
        secs.push(questions_section([1, 2, 5, 3], 60, Some(0.7)));
        let md = markdown(&run(false), &secs);
        for want in [
            "# Eval run r-1",
            "| **overall** | 2 | 50.0 | 75.0 | 40.0 | 60.0 |",
            "| semantic | 2 |",
            "r009",
            "INVALID: openai:j failed 1 of 1 lessons canaries",
            "| passes | 4 | – | 0.80 |",
            "8 kept",
            "κ = 0.70",
            "NO: a judge's canaries failed",
        ] {
            assert!(md.contains(want), "{want} not in\n{md}");
        }
        let page = html(&run(false), &secs);
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<svg class=\"chart\""));
        assert!(page.contains("prefers-color-scheme:dark"));
        assert!(page.contains("a note &lt;b&gt;"));
        assert!(page.contains("class=\"invalid\""));
        assert!(!page.contains("<script"), "self-contained, no scripts");
        assert!(
            !page.contains("http://") && !page.contains("https://"),
            "no external files"
        );
    }

    /// The cost table has a provider column and a per-provider table; ChatGPT lines
    /// say "plan-billed" instead of $0.
    #[test]
    fn the_cost_table_is_per_provider() {
        use crate::traces::efficiency;
        use margin_shared::context::{TraceKind, TraceRow, TraceUsage};
        use margin_shared::pipeline::Route;
        let prices: crate::Prices = toml::from_str(
            &std::fs::read_to_string(crate::golden::evals_dir().join("prices.toml")).unwrap(),
        )
        .unwrap();
        let call = |i: i64, provider: &str, model: &str| {
            let mut r: TraceRow = serde_json::from_value(serde_json::json!({
                "id": format!("u{i}"),
                "kind": TraceKind::ModelCall.as_str(),
                "at": chrono::DateTime::from_timestamp(i, 0).unwrap(),
            }))
            .unwrap();
            r.trace_id = "t".into();
            r.route = Some(Route::Chat);
            r.provider = Some(provider.into());
            r.model = Some(model.into());
            r.usage = Some(TraceUsage {
                input: 1_000_000,
                cached: 500_000,
                cache_write: 100_000,
                output: 100_000,
                web_searches: 4,
                ..Default::default()
            });
            r
        };
        let rows = [
            call(1, "claude", "claude-sonnet-5-5"),
            call(2, "openai", "gpt-6.1-sol"),
            call(3, "chatgpt", "gpt-6.1-sol"),
        ];
        let sec = efficiency_section(&efficiency(&rows, &prices));
        let md = markdown(&run(true), &[sec]);
        for want in [
            "| Route / model | Provider | Calls | Input | Cached % | Cache writes | Output | Web searches | Cost (USD) | Mean ms |",
            // 0.4M × $2 + 0.5M × $0.20 + 0.1M × $2.50 + 0.1M × $10 + 4 × $0.01.
            "| chat / claude-sonnet-5-5 | claude | 1 | 1000000 | 50.0 | 100000 | 100000 | 4 | 2.1900 |",
            "| chat / gpt-6.1-sol | chatgpt | 1 | 1000000 | 50.0 | 100000 | 100000 | 4 | plan-billed |",
            "| chat / gpt-6.1-sol | openai | 1 |",
            "| **total** | all | 3 |",
            "(+1 plan-billed)",
            "### Cost by provider",
            "| Provider | Calls |",
            "| chatgpt | 1 | 1000000 | 50.0 | 100000 | 100000 | 4 | plan-billed |",
            "| claude | 1 | 1000000 | 50.0 | 100000 | 100000 | 4 | 2.1900 |",
            "- **Provider:** claude",
        ] {
            assert!(md.contains(want), "{want} not in\n{md}");
        }
    }

    /// The Alerts section lists rises and clears; the efficiency text carries the step
    /// overhead when calls recorded the provider's share.
    #[test]
    fn alerts_and_overhead() {
        use crate::traces::{AlertLine, Efficiency, Overhead};
        let at = chrono::DateTime::from_timestamp(1_760_000_000, 0).unwrap();
        let e = Efficiency {
            step_overhead: Some(Overhead {
                steps: 3,
                mean_ms: 40.0,
                p95_ms: 100.0,
                max_ms: 100.0,
            }),
            alerts: vec![
                AlertLine {
                    at,
                    name: "error_rate".into(),
                    kind: Some("model_call".into()),
                    raised: true,
                    value: 0.5,
                },
                AlertLine {
                    at,
                    name: "http_5xx".into(),
                    kind: None,
                    raised: false,
                    value: 6.0,
                },
            ],
            ..Default::default()
        };
        let md = markdown(&run(true), &[efficiency_section(&e), alerts_section(&e)]);
        for want in [
            "over 3 calls: mean 40 ms, p95 100 ms, max 100 ms",
            "## Alerts",
            "1 rises and 1 clears",
            "| 2025-10-09 08:53 | error_rate | model_call | raised | 0.500 |",
            "| 2025-10-09 08:53 | http_5xx | - | cleared | 6.000 |",
        ] {
            assert!(md.contains(want), "{want} not in\n{md}");
        }
        let none = alerts_section(&Efficiency::default());
        assert!(none.tables.is_empty());
        assert_eq!(none.text, ["No alert rose or cleared in these traces."]);
    }
}
