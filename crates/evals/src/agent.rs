//! The agent suite (§3.4 `run --suite agent`): each golden chat question
//! (`golden/agent.jsonl`) runs through Margin's real agent loop (`margin_agent::
//! run_turn`, the chat model, full replay) with one tool, `search`, answered from the
//! eval index in the shape the context API returns (`n`-numbered passages, text cut to
//! `TOOL_OUTPUT_PASSAGE_CHARS`, at most `TOOL_SEARCH_K_MAX`). The answer's citations
//! are checked against the passages the tool actually returned, and the turn's steps,
//! tokens and tool output sizes are recorded. The unanswerable task scores whether the
//! agent declined to cite anything.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use margin_agent::{
    BoxFuture, ItemRecord, ModelClient, NoDevices, RetryPolicy, SinkError, Tool, ToolContext,
    ToolKind, ToolOutput, ToolRegistry, TurnDeps, TurnInput, TurnOutcome, TurnSink, run_turn,
    user_message_input,
};
use margin_agent::MessageContext;
use margin_shared::context::Scope;
use margin_shared::routes::TOOL_SEARCH_K_MAX;
use margin_shared::{ChatItem, ChatItemKind, Decision, DeviceRef, ItemStatus, ReasoningEffort, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::citations::{CitationReport, SentPassage, check};
use crate::golden::{AgentTask, matches_anchor};
use crate::index::{EvalIndex, scope_of};
use crate::packing::context_hit;

/// The `search` tool over the eval index. Passages are numbered across the turn, so
/// `[^n]` in the answer names one passage whatever call returned it.
pub struct SearchTool {
    pub index: Arc<EvalIndex<'static>>,
    pub scope: Scope,
    /// Every passage returned this turn, in `n` order.
    pub sent: Arc<Mutex<Vec<SentPassage>>>,
    /// Bytes of each output.
    pub outputs: Arc<Mutex<Vec<usize>>>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    k: Option<u32>,
}

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search the learner's documents for passages relevant to a query. Returns numbered \
         passages with their document and locator; cite passage n as [^n] in your answer. \
         Search again with other words when the first results don't answer the question."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query", "k"],
            "properties": {
                "query": {"type": "string", "description": "What to look for, in your own words."},
                "k": {"type": ["integer", "null"], "description": "How many passages (at most 12)."}
            }
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn label(&self, args: &Value) -> String {
        match args.get("query").and_then(Value::as_str) {
            Some(q) => format!("Searching for “{q}”"),
            None => "Searching".into(),
        }
    }
    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let args: SearchArgs = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("The arguments don't fit search: {e}")),
            };
            let k = args
                .k
                .map_or(8, |k| k as usize)
                .clamp(1, TOOL_SEARCH_K_MAX);
            let ranked = match self.index.search(std::slice::from_ref(&args.query), &self.scope).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("Search failed: {e}")),
            };
            let mut sent = self.sent.lock().unwrap();
            let passages: Vec<Value> = ranked
                .hits
                .iter()
                .take(k)
                .map(|h| {
                    let hit = context_hit(h);
                    sent.push(SentPassage {
                        document: hit.document_id.clone(),
                        text: hit.text.clone(),
                    });
                    let mut v = serde_json::to_value(&hit).unwrap_or(Value::Null);
                    v["n"] = json!(sent.len());
                    v
                })
                .collect();
            let output = json!({ "passages": passages }).to_string();
            self.outputs.lock().unwrap().push(output.len());
            let n = passages.len();
            ToolOutput::ok(output, format!("{n} passages"))
        })
    }
}

/// Keeps a turn's items in memory.
#[derive(Default)]
pub struct MemorySink {
    pub items: Mutex<Vec<ChatItem>>,
    pub usage: Mutex<Usage>,
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

impl TurnSink for MemorySink {
    fn add_item(
        &self,
        status: ItemStatus,
        kind: ChatItemKind,
        _record: ItemRecord,
    ) -> BoxFuture<'_, Result<ChatItem, SinkError>> {
        let mut items = self.items.lock().unwrap();
        let seq = items.len() as u64 + 1;
        let item = ChatItem {
            id: format!("i{seq}"),
            chat_id: "eval".into(),
            turn_id: "turn".into(),
            seq,
            status,
            created_at: now(),
            updated_at: now(),
            kind,
        };
        items.push(item.clone());
        Box::pin(async move { Ok(item) })
    }
    fn delta(&self, _item: &ChatItem, _text: &str) {}
    fn update_item<'a>(
        &'a self,
        item: &'a ChatItem,
        _record: ItemRecord,
    ) -> BoxFuture<'a, Result<(), SinkError>> {
        if let Some(slot) = self.items.lock().unwrap().iter_mut().find(|i| i.id == item.id) {
            *slot = item.clone();
        }
        Box::pin(async { Ok(()) })
    }
    fn add_usage<'a>(&'a self, usage: &'a Usage) -> BoxFuture<'a, Result<(), SinkError>> {
        self.usage.lock().unwrap().add(usage);
        Box::pin(async { Ok(()) })
    }
    fn wait_for_decision<'a>(&'a self, _card: &'a ChatItem) -> BoxFuture<'a, Result<Decision, SinkError>> {
        Box::pin(async { Err(SinkError("the eval agent has no one to ask".into())) })
    }
}

/// One task's outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub finished: bool,
    pub error: Option<String>,
    pub steps: u32,
    pub searches: usize,
    pub answer: String,
    pub passages: Vec<SentPassage>,
    pub citations: CitationReport,
    /// Share of the task's anchors found in a passage the answer cites.
    pub anchors_cited: f64,
    /// For a task with nothing to cite: the answer cited nothing.
    pub abstained: Option<bool>,
    pub tool_output_max_bytes: usize,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

/// Runs one task through the agent loop.
pub async fn run_task(
    model: &dyn ModelClient,
    model_id: Option<String>,
    index: Arc<EvalIndex<'static>>,
    task: &AgentTask,
    retry: RetryPolicy,
) -> TaskResult {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SearchTool {
        index,
        scope: scope_of(task.scope),
        sent: sent.clone(),
        outputs: outputs.clone(),
    }));
    let sink = MemorySink::default();
    let device = DeviceRef {
        device_id: "margin-eval".into(),
        name: "margin-eval".into(),
    };
    let history = user_message_input(
        &task.question,
        &MessageContext {
            now: Utc::now(),
            device: device.clone(),
            workspaces: Vec::new(),
            sources: Vec::new(),
        },
    );
    let outcome = run_turn(
        TurnDeps {
            model,
            tools: &tools,
            sink: &sink,
            devices: Arc::new(NoDevices),
            retry,
        },
        TurnInput {
            chat_id: format!("eval-{}", task.id),
            turn_id: "turn".into(),
            origin_device: device,
            model: model_id,
            reasoning_effort: ReasoningEffort::Medium,
            history,
            workspaces: Vec::new(),
        },
    )
    .await;
    let items = sink.items.lock().unwrap().clone();
    let answer = items
        .iter()
        .filter_map(|i| match &i.kind {
            ChatItemKind::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let searches = items
        .iter()
        .filter(|i| matches!(&i.kind, ChatItemKind::ToolCall { name, .. } if name == "search"))
        .count();
    let passages = sent.lock().unwrap().clone();
    let citations = check(&answer, &passages, &task.must_cite);
    let cited: Vec<&SentPassage> = crate::citations::sentences(&answer)
        .iter()
        .flat_map(|s| s.markers.clone())
        .filter_map(|n| n.checked_sub(1).and_then(|i| passages.get(i)))
        .collect();
    let anchors_cited = if task.anchors.is_empty() {
        1.0
    } else {
        task.anchors
            .iter()
            .filter(|a| cited.iter().any(|p| matches_anchor(&p.text, a)))
            .count() as f64
            / task.anchors.len() as f64
    };
    let usage = *sink.usage.lock().unwrap();
    let (finished, steps, error) = match outcome {
        TurnOutcome::Finished { steps } => (true, steps, None),
        TurnOutcome::Failed { error, .. } => (false, 0, Some(error)),
    };
    TaskResult {
        id: task.id.clone(),
        finished,
        error,
        steps,
        searches,
        abstained: task.must_cite.is_empty().then_some(citations.markers == 0),
        answer,
        passages,
        citations,
        anchors_cited,
        tool_output_max_bytes: outputs.lock().unwrap().iter().copied().max().unwrap_or(0),
        input_tokens: usage.input_tokens,
        cached_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{Extraction, documents};
    use crate::golden::{GoldenScope, evals_dir, read_jsonl};
    use crate::index::testing::register_stand_ins;
    use crate::recorded::FakeVectors;
    use margin_agent::scripted::{Scripted, ScriptedModel, text_reply, tool_calls_reply};
    use margin_service::db::Store;
    use margin_shared::pipeline::PipelineConfig;

    static VECTORS: FakeVectors = FakeVectors;

    async fn index() -> Arc<EvalIndex<'static>> {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let ex = Extraction::fixture(&corpus, &docs).unwrap();
        Arc::new(
            EvalIndex::build(
                Store::memory().await.unwrap(),
                &PipelineConfig::default(),
                register_stand_ins,
                &VECTORS,
                &corpus,
                &docs,
                &ex,
            )
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn a_scripted_turn_searches_and_cites() {
        let index = index().await;
        let tasks: Vec<AgentTask> = read_jsonl(&evals_dir().join("golden/agent.jsonl")).unwrap();
        let task = tasks.iter().find(|t| t.id == "a01").unwrap();
        let model = ScriptedModel::new([
            Scripted::Events(tool_calls_reply(&[(
                "c1",
                "search",
                r#"{"query":"Gibbs overshoot adding more terms","k":5}"#,
            )])),
            Scripted::Events(text_reply(&[
                "No: adding more terms does not shrink the overshoot near the jump.[^1] ",
                "It stays near 9 percent of the jump.[^1]",
            ])),
        ]);
        let r = run_task(&model, None, index.clone(), task, RetryPolicy::immediate()).await;
        assert!(r.finished, "{:?}", r.error);
        assert_eq!(r.steps, 2);
        assert_eq!(r.searches, 1);
        assert_eq!(r.passages.len(), 5);
        assert!(r.tool_output_max_bytes > 0 && r.tool_output_max_bytes < 16 * 1024);
        assert_eq!(r.citations.resolution_pct(), 100.0);
        assert!(r.answer.contains("does not shrink"));
        assert_eq!(r.abstained, None);
        // The search tool was offered to the model with a strict schema.
        let req = &model.requests()[0];
        assert!(req.tools.iter().any(|t| t["name"] == "search"));
        // The second call carries the tool's output with numbered passages.
        let second = serde_json::to_string(&model.requests()[1].input).unwrap();
        assert!(second.contains("\\\"n\\\":1"), "{second}");
    }

    #[tokio::test]
    async fn the_unanswerable_task_scores_abstention() {
        let index = index().await;
        let task = AgentTask {
            id: "a10".into(),
            question: "Do my sources say anything about CRISPR guide RNA design?".into(),
            scope: GoldenScope::Corpus,
            must_cite: vec![],
            anchors: vec![],
        };
        let model = ScriptedModel::new([Scripted::Events(text_reply(&[
            "Your sources don't cover CRISPR.",
        ]))]);
        let r = run_task(&model, None, index, &task, RetryPolicy::immediate()).await;
        assert_eq!(r.abstained, Some(true));
        assert_eq!(r.anchors_cited, 1.0);
        let failing = ScriptedModel::default();
        let r = run_task(&failing, None, index_again().await, &task, RetryPolicy::immediate()).await;
        assert!(!r.finished);
        assert!(r.error.is_some());
    }

    async fn index_again() -> Arc<EvalIndex<'static>> {
        index().await
    }
}
