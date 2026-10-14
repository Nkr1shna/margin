//! The tools offered to the model.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::{Tool, ToolKind};

/// Every tool the agent can call, in a fixed order. The service builds one at startup
/// (service tools plus `device_tools()`); the same `tools` array goes with every model
/// call (so the prompt cache holds) and each step narrows what may be called with
/// `tool_choice: allowed_tools`.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool. Panics on a duplicate name (a programming error).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        assert!(
            self.get(tool.name()).is_none(),
            "two tools are named {}",
            tool.name()
        );
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Names of the tools of `kind`.
    pub fn names_of(&self, kind: ToolKind) -> Vec<String> {
        self.tools
            .iter()
            .filter(|t| t.kind() == kind)
            .map(|t| t.name().to_string())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The Responses API `tools` array: one
    /// `{"type":"function","name","description","parameters","strict":true}` per tool.
    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                    "strict": true,
                })
            })
            .collect()
    }
}
