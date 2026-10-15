//! Rebuilding a chat's model input from its stored items, per provider
//! (`docs/agent-providers-brief.md` D4). A chat stays on one provider, and its
//! `model_items` hold that provider's wire items verbatim: Responses items for OpenAI
//! and ChatGPT ([`openai`]), Messages content blocks with their role for Claude
//! ([`claude`]). Nothing stored is ever rewritten.

pub mod claude;
pub mod openai;

pub use openai::{
    CONDENSED_HISTORY_PREFIX, ReplayItem, TOOL_PICTURES_PREFIX, action_input,
    cancelled_call_output, function_call_output, replay_input, tool_pictures_input,
    unanswered_calls,
};
pub(crate) use openai::{
    PICTURE_REMOVED, compaction_start, condensed_history_input, is_condensed_history, item_type,
    tool_pictures_call_id,
};
