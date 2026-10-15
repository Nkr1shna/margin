//! Model providers (`docs/agent-providers-brief.md`): which API a chat and the job
//! routes call, the model each route uses per provider, the models a provider offers,
//! the ChatGPT sign-in state, and the web results and citations a hosted web tool
//! produces.
//!
//! A chat is created on one provider and stays on it (`Chat.provider`); lessons, videos,
//! outlines and the small calls (titles, condensing) follow Settings' default provider.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pipeline::Route;

/// The API a model call goes to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// OpenAI's Responses API with an API key (or an OpenAI-compatible base URL).
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    /// OpenAI's Responses API with the owner's ChatGPT plan ("Sign in with ChatGPT";
    /// the sign-in spike passed, `docs/agent-siwc-spike.md`).
    #[serde(rename = "chatgpt")]
    ChatGpt,
    /// Anthropic's Messages API with an API key.
    Claude,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::OpenAi, Provider::ChatGpt, Provider::Claude];

    /// The wire name, also the trace's `provider` field: "openai", "chatgpt", "claude".
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::ChatGpt => "chatgpt",
            Provider::Claude => "claude",
        }
    }

    /// The name shown in the app.
    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::ChatGpt => "ChatGPT",
            Provider::Claude => "Claude",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The model id each route uses on one provider: Settings' three slots ("Model for
/// chats", "for writing", "for small tasks").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteModels {
    pub chat: String,
    pub write: String,
    pub small: String,
}

impl RouteModels {
    pub fn get(&self, route: Route) -> &str {
        match route {
            Route::Chat => &self.chat,
            Route::Write => &self.write,
            Route::Small => &self.small,
        }
    }

    pub fn set(&mut self, route: Route, model: impl Into<String>) {
        let slot = match route {
            Route::Chat => &mut self.chat,
            Route::Write => &mut self.write,
            Route::Small => &mut self.small,
        };
        *slot = model.into();
    }

    /// Claude's shipped slots: Opus 5.5 for chats and writing, Haiku 5.5 for small tasks.
    pub fn claude_default() -> Self {
        Self {
            chat: CLAUDE_DEFAULT_MODEL.into(),
            write: CLAUDE_DEFAULT_MODEL.into(),
            small: CLAUDE_SMALL_MODEL.into(),
        }
    }

    /// OpenAI's slots for settings that predate them: chats on `agent_model` (else
    /// `openai_model`), writing on `openai_model`, small tasks on `OPENAI_SMALL_MODEL`.
    pub fn openai_from(openai_model: &str, agent_model: Option<&str>) -> Self {
        let agent = agent_model.map(str::trim).filter(|m| !m.is_empty());
        Self {
            chat: agent.unwrap_or(openai_model).to_string(),
            write: openai_model.to_string(),
            small: OPENAI_SMALL_MODEL.into(),
        }
    }

    /// ChatGPT's slots before the owner picks from the account's list.
    pub fn chatgpt_default() -> Self {
        Self {
            chat: CHATGPT_DEFAULT_MODEL.into(),
            write: CHATGPT_DEFAULT_MODEL.into(),
            small: CHATGPT_DEFAULT_MODEL.into(),
        }
    }
}

/// Claude's default model for chats and writing.
pub const CLAUDE_DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Claude's default model for small tasks (titles, condensing, query rewrite, rerank).
pub const CLAUDE_SMALL_MODEL: &str = "claude-haiku-5-5";
/// The Claude models Settings lists first, in order.
pub const CLAUDE_LISTED_MODELS: [&str; 3] =
    ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];
/// OpenAI's default small-tasks model (the rag doc's Small default).
pub const OPENAI_SMALL_MODEL: &str = "gpt-6-luna";
/// ChatGPT's default model until the owner picks from the account's list.
pub const CHATGPT_DEFAULT_MODEL: &str = "gpt-6.1-sol";

/// A model a provider offers (`GET routes::providers_models(provider)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Most input tokens (the context window), when the provider says or Margin's table
    /// knows it.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Most output tokens of one call, when known.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// The ChatGPT account Margin is signed in with. No token ever crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptAccount {
    pub email: String,
    pub signed_in_at: DateTime<Utc>,
    /// When the refresh token runs out (the owner signs in again after).
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// `GET routes::PROVIDERS_CHATGPT`: where the ChatGPT sign-in is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChatGptStatus {
    SignedOut,
    /// A sign-in was started; `authorize_url` must be opened on the Mac that runs the
    /// service (the OAuth callback is a loopback URL there).
    Waiting {
        authorize_url: String,
    },
    SignedIn {
        account: ChatGptAccount,
        /// Why the latest sign-in attempt failed, when it failed while this account's
        /// sign-in stayed valid.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_error: Option<String>,
    },
    Failed {
        message: String,
    },
}

/// `POST routes::PROVIDERS_CHATGPT_START`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptStart {
    pub authorize_url: String,
    pub state: String,
}

/// A page a hosted web tool cited in an answer (`AssistantText.citations`). Claude's
/// `web_search_result_location`/`char_location` and OpenAI's `url_citation` both map to
/// it. `start`/`end` are byte offsets into the answer's text when the provider gave them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebCitation {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    /// The quoted passage (Claude: at most 150 characters).
    #[serde(default)]
    pub cited_text: Option<String>,
    #[serde(default)]
    pub start: Option<u32>,
    #[serde(default)]
    pub end: Option<u32>,
}

/// One result of a web search (`Card::WebResults`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebResult {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    /// "April 30, 2025", as the provider wrote it.
    #[serde(default)]
    pub page_age: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_round_trip_by_wire_name() {
        for p in Provider::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<Provider>(&json).unwrap(), p);
        }
        assert_eq!(Provider::default(), Provider::OpenAi);
        assert_eq!(Provider::ChatGpt.label(), "ChatGPT");
    }

    #[test]
    fn route_models_by_route() {
        let mut m = RouteModels::claude_default();
        assert_eq!(m.get(Route::Chat), "claude-opus-5-5");
        assert_eq!(m.get(Route::Write), "claude-opus-5-5");
        assert_eq!(m.get(Route::Small), "claude-haiku-5-5");
        m.set(Route::Write, "claude-sonnet-5-5");
        assert_eq!(m.write, "claude-sonnet-5-5");
        let o = RouteModels::openai_from("gpt-w", Some(" gpt-c "));
        assert_eq!((o.chat.as_str(), o.write.as_str()), ("gpt-c", "gpt-w"));
        assert_eq!(RouteModels::openai_from("gpt-w", Some("")).chat, "gpt-w");
    }

    #[test]
    fn chatgpt_status_round_trips() {
        for s in [
            ChatGptStatus::SignedOut,
            ChatGptStatus::Waiting {
                authorize_url: "https://auth.example/a".into(),
            },
            ChatGptStatus::SignedIn {
                account: ChatGptAccount {
                    email: "a@b.c".into(),
                    signed_in_at: Utc::now(),
                    expires_at: None,
                },
                last_error: None,
            },
            ChatGptStatus::SignedIn {
                account: ChatGptAccount {
                    email: "a@b.c".into(),
                    signed_in_at: Utc::now(),
                    expires_at: None,
                },
                last_error: Some("The owner declined.".into()),
            },
            ChatGptStatus::Failed {
                message: "no".into(),
            },
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<ChatGptStatus>(&json).unwrap(), s);
        }
        let m: ModelInfo = serde_json::from_str(r#"{"id":"m"}"#).unwrap();
        assert_eq!(m.context_window, None);
    }
}
