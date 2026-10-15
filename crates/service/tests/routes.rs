//! Model routes (`pipeline.routes`): chat turns honour the chat route, titles and the
//! loop's condense calls go to the small-tasks route; and the pipeline variants Settings
//! chooses from (`GET /api/pipeline/variants`).

mod common;

use std::sync::Arc;

use common::*;
use margin_agent::scripted::text_reply;
use margin_agent::{ModelClient, Scripted, ScriptedModel};
use margin_ai::{CallOptions, ModelRequest, RouteModels, ToolChoice, ToolDef, WebTools};
use margin_service::settings::PipelineVariants;
use margin_service::turns::{SmallRoute, TracedModel, TurnTrace};
use margin_shared::ReasoningEffort;
use margin_shared::pipeline::PipelineConfig;
use margin_shared::pipeline::Route;
use margin_shared::{ChatDetail, ChatState};
use serde_json::json;

async fn set_routes(h: &Harness, t: &str, chat: Option<&str>, small: Option<&str>) {
    let mut cfg = PipelineConfig::default();
    cfg.routes.chat = chat.map(Into::into);
    cfg.routes.small = small.map(Into::into);
    let r = h
        .patch(t, "/api/settings")
        .json(&json!({ "pipeline": cfg }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn turns_use_the_chat_route_and_titles_the_small_one() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    set_routes(&h, &t, Some("chat-model"), Some("small-model")).await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hello there."])));
    h.title_reply("Greetings");
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "hi"}}))
        .send()
        .await
        .unwrap();
    let chat: ChatDetail = r.json().await.unwrap();
    eventually(|| async {
        let d: ChatDetail = h
            .get(&t, &format!("/api/chats/{}", chat.chat.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (d.chat.state == ChatState::Idle && d.chat.title == "Greetings").then_some(())
    })
    .await;
    let turn = h.generator.agent.requests();
    assert_eq!(turn[0].model.as_deref(), Some("chat-model"));
    // The title is a plain text call on the small route.
    let title = h.generator.text_calls.lock().unwrap().clone();
    assert_eq!(title[0].model, "small-model");
}

#[tokio::test(flavor = "multi_thread")]
async fn condense_calls_go_to_the_small_route() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let chat = Arc::new(ScriptedModel::default());
    let small = Arc::new(ScriptedModel::default());
    chat.push(Scripted::Events(text_reply(&["step"])));
    small.push(Scripted::Events(text_reply(&["summary"])));
    let model = TracedModel::new(
        chat.clone(),
        TurnTrace::new(&h.state, "c", "turn", None, "person/bootstrap"),
        "chat-model",
        "openai",
        None,
    )
    .with_small(SmallRoute {
        client: small.on_models(RouteModels {
            chat: "chat-model".into(),
            write: "write-model".into(),
            small: "small-model".into(),
        }),
        model: "small-model".into(),
        provider: "openai",
    });
    let request = |tools: Vec<ToolDef>, route| ModelRequest {
        opts: CallOptions::new(route).with_cache_key("c"),
        system: "condense".into(),
        history: vec![json!({"role": "user", "content": "hi"})],
        tools,
        tool_choice: ToolChoice::Auto,
        web: WebTools::default(),
        effort: ReasoningEffort::Low,
        compaction: None,
        parallel_tool_calls: true,
    };
    // A step (with tools) stays on the chat model…
    let search = ToolDef {
        name: "search".into(),
        description: "Search.".into(),
        parameters: json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    };
    drop(
        model
            .stream(request(vec![search], Route::Chat))
            .await
            .unwrap(),
    );
    // …a condense call (no tools) goes to the small one, on its small slot.
    drop(model.stream(request(vec![], Route::Small)).await.unwrap());
    assert_eq!(chat.requests().len(), 1);
    let sent = small.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].model.as_deref(), Some("small-model"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pipeline_variants_are_listed() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let r = h
        .get(&t, margin_shared::routes::PIPELINE_VARIANTS)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: PipelineVariants = r.json().await.unwrap();
    for kind in [
        "chunker",
        "rewriter",
        "retriever",
        "fusion",
        "reranker",
        "packer",
        "prompts",
    ] {
        assert!(!v.stages[kind].is_empty(), "no {kind}");
    }
    assert!(v.stages["retriever"].contains(&"hybrid-linear".to_string()));
    let hybrid = v.variants.iter().find(|n| n.name == "hybrid").unwrap();
    assert_eq!(hybrid.config.retriever.name, "hybrid-linear");
    // R2's model-backed variants are listed: the small-tasks ones run on the service's
    // models; the cross-encoder ones are unavailable until the reranker model is
    // downloaded, and say so.
    for n in &v.variants {
        if n.config.reranker.name == "onnx-crossencoder" {
            let why = n.unavailable.as_deref().unwrap_or_default();
            assert!(
                why.contains("download the reranker model"),
                "{}: {why}",
                n.name
            );
        } else {
            assert_eq!(n.unavailable, None, "{}", n.name);
        }
    }
    for name in ["rerank-small", "rewrite-small", "r2-full", "rerank-xenc"] {
        assert!(v.variants.iter().any(|n| n.name == name), "{name} missing");
    }
}
