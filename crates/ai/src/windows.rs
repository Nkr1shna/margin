//! Context windows of OpenAI models by id (D13). OpenAI's `/models` carries no window,
//! so the Responses client fills `ModelInfo` from this table; ChatGPT uses it too. An id
//! the table doesn't know gets `None`, and the service then runs it on
//! `MODEL_WINDOW_FALLBACK` and says so in Settings — never a silent guess.
//!
//! Checked against OpenAI's models page on 2026-10-14: the GPT-6 family (`gpt-6-astra`,
//! `gpt-6.1-sol`, `gpt-6-luna`) has a 1 050 000-token context window and 128 000 output
//! tokens.

use crate::model::ModelWindow;

/// Known windows, by id prefix: an id matches the longest prefix it starts with.
const TABLE: &[(&str, ModelWindow)] = &[(
    "gpt-6",
    ModelWindow {
        input: 1_050_000,
        output: 128_000,
    },
)];

/// The window of an OpenAI (or ChatGPT) model id, when the table knows it.
pub fn openai_window(model: &str) -> Option<ModelWindow> {
    TABLE
        .iter()
        .filter(|(prefix, _)| model.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, w)| *w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_6_models_have_their_window_and_others_none() {
        for id in ["gpt-6-luna", "gpt-6.1-sol", "gpt-6-astra"] {
            let w = openai_window(id).unwrap();
            assert_eq!((w.input, w.output), (1_050_000, 128_000), "{id}");
        }
        assert_eq!(openai_window("gpt-4o"), None);
        assert_eq!(openai_window("my-local-model"), None);
    }
}
