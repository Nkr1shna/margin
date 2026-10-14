//! Tools: what the model can call.

use std::sync::Arc;

use margin_shared::{Card, DeviceRef, Id, Workspace};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{BoxFuture, DeviceBridge};

/// What a tool does, which decides how the loop treats it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads Margin's data. Runs concurrently with the step's other Read and Device
    /// calls.
    Read,
    /// Changes Margin's data. Runs after the step's Read and Device calls, one at a time
    /// in the model's order. Asks first when [`Tool::confirmation`] says so (overwriting
    /// the owner's own writing, owner decision Q2).
    Write,
    /// Deletes. Always asks first ([`Tool::confirmation`] must return a prompt).
    Destructive,
    /// Runs on a device through the [`DeviceBridge`] (folder and app-control tools).
    /// Read-only; runs concurrently like Read.
    Device,
}

/// The question a confirmation card asks. The loop turns it into a
/// `Card::Confirmation` with `Decision::Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationPrompt {
    /// "Delete the topic “Fourier series”?"
    pub title: String,
    /// "3 lessons, 2 notes and 1 video go with it."
    pub detail: String,
    /// "Delete", "Replace".
    pub approve_label: String,
}

/// What a tool call is running in.
#[derive(Clone)]
pub struct ToolContext {
    pub chat_id: Id,
    pub turn_id: Id,
    /// The model's `call_id` for this call.
    pub call_id: String,
    /// The device whose message started the turn: app-control tools go to it.
    pub origin_device: DeviceRef,
    /// The folders attached to the chat.
    pub workspaces: Vec<Workspace>,
    pub devices: Arc<dyn DeviceBridge>,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// False when the call failed; `output` then says why (see [`error_output`]).
    pub ok: bool,
    /// What the model sees as the `function_call_output`. Plain text or JSON text. The
    /// loop cuts it to `TOOL_OUTPUT_MAX_BYTES`.
    pub output: String,
    /// One line for the tool row, e.g. "4 matches in 2 lessons".
    pub summary: String,
    /// Inline cards to show after the result (an outline, results, passages).
    pub cards: Vec<Card>,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            summary: summary.into(),
            cards: Vec::new(),
        }
    }

    /// A failed call: the model gets [`error_output`]`(message)`, the row shows `message`.
    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            ok: false,
            output: error_output(&message),
            summary: message,
            cards: Vec::new(),
        }
    }

    pub fn with_card(mut self, card: Card) -> Self {
        self.cards.push(card);
        self
    }
}

/// The output the model gets for a failed, denied or cancelled call:
/// `{"error":"<message>"}` as JSON text.
pub fn error_output(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Cuts `output` to at most `max` bytes on a UTF-8 boundary, ending it with
/// "\n[… cut: N more bytes]" (the marker counts toward `max`). Unchanged when it fits.
pub fn truncate_output(output: &str, max: usize) -> String {
    if output.len() <= max {
        return output.to_string();
    }
    // The marker's length depends on how much is cut; sizing it for the whole output
    // (the most digits it can need) keeps the result within `max`.
    let widest = cut_marker(output.len()).len();
    if widest > max {
        return output[..floor_char_boundary(output, max)].to_string();
    }
    let keep = floor_char_boundary(output, max - widest);
    let mut out = output[..keep].to_string();
    out.push_str(&cut_marker(output.len() - keep));
    out
}

fn cut_marker(cut: usize) -> String {
    format!("\n[… cut: {cut} more bytes]")
}

/// The largest index `<= at` that falls on a character boundary of `s`.
pub(crate) fn floor_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    let mut i = at;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A tool the model can call. Implementations must not panic: every failure is a
/// [`ToolOutput::error`] the model reads and works around.
pub trait Tool: Send + Sync + 'static {
    /// Unique, snake_case, e.g. "search_library".
    fn name(&self) -> &str;
    /// What the model reads to decide when to call it.
    fn description(&self) -> &str;
    /// The JSON schema of the arguments, valid for `strict: true` function calling (see
    /// [`strict_schema`]).
    fn parameters(&self) -> Value;
    fn kind(&self) -> ToolKind;
    /// One line for the tool row while it runs, e.g. "Searching the library for
    /// “fourier”". `args` may be malformed; fall back to a generic line.
    fn label(&self, args: &Value) -> String;
    /// For `ToolKind::Device` tools: the device the call goes to (shown on the tool row
    /// as `ToolCall.device`); `None` when `args` don't say.
    fn device(&self, ctx: &ToolContext, args: &Value) -> Option<DeviceRef> {
        let _ = (ctx, args);
        None
    }
    /// Whether this call needs the owner's OK before it runs. Called for Write and
    /// Destructive tools only; a Destructive tool must return `Ok(Some(..))`. `Err` fails
    /// the call with that message (e.g. the topic doesn't exist).
    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        let _ = (ctx, args);
        Box::pin(async { Ok(None) })
    }
    /// Runs the call. Cancel-safe: Stop drops the future.
    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput>;
}

/// A tool with typed arguments; wrap it in [`Typed`] to register it. Arguments that
/// don't parse become an error output naming the problem, without running the tool.
pub trait TypedTool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send + Sync;
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const KIND: ToolKind;

    fn label(&self, args: &Self::Args) -> String;

    /// See [`Tool::device`].
    fn device(&self, ctx: &ToolContext, args: &Self::Args) -> Option<DeviceRef> {
        let _ = (ctx, args);
        None
    }

    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        let _ = (ctx, args);
        Box::pin(async { Ok(None) })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput>;
}

/// Adapts a [`TypedTool`] to [`Tool`]: schema from `schemars` through
/// [`strict_schema`], arguments parsed with serde.
pub struct Typed<T>(pub T);

impl<T: TypedTool> Tool for Typed<T> {
    fn name(&self) -> &str {
        T::NAME
    }

    fn description(&self) -> &str {
        T::DESCRIPTION
    }

    fn parameters(&self) -> Value {
        strict_schema::<T::Args>()
    }

    fn kind(&self) -> ToolKind {
        T::KIND
    }

    fn label(&self, args: &Value) -> String {
        match T::Args::deserialize(args) {
            Ok(args) => self.0.label(&args),
            Err(_) => generic_label(T::NAME),
        }
    }

    fn device(&self, ctx: &ToolContext, args: &Value) -> Option<DeviceRef> {
        let args = T::Args::deserialize(args).ok()?;
        self.0.device(ctx, &args)
    }

    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let args = parse_args::<T>(args)?;
            self.0.confirmation(ctx, &args).await
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match parse_args::<T>(&args) {
                Ok(args) => self.0.run(ctx, args).await,
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

fn parse_args<T: TypedTool>(args: &Value) -> Result<T::Args, String> {
    T::Args::deserialize(args).map_err(|e| format!("The arguments don't fit {}: {e}", T::NAME))
}

/// "search_library" → "Search library": the tool row's line when the arguments can't
/// be read.
pub(crate) fn generic_label(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The JSON schema of `T` as strict function calling requires it: every object has
/// `"additionalProperties": false` and lists every property in `required`; an
/// `Option<_>` field is required with a `null`-able type; no `$ref`s to definitions the
/// API rejects (definitions are inlined or kept under `$defs`), no `format`s the API
/// doesn't support, no `default`s. Doc comments on fields become `description`s.
pub fn strict_schema<T: JsonSchema>() -> Value {
    // `Option<T>` comes out as `T` or null (`"type": [.., "null"]` or an `anyOf` with
    // `{"type": "null"}`), which strict mode accepts once the field is required.
    let settings = SchemaSettings::draft2020_12().with(|s| s.inline_subschemas = true);
    let mut schema = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    if let Value::Object(root) = &mut schema {
        root.remove("$schema");
        root.remove("title");
    }
    make_strict(&mut schema);
    schema
}

/// Applies the strict-mode rules to a schema and every schema inside it.
fn make_strict(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            for key in [
                "format",
                "default",
                "uniqueItems",
                "examples",
                "deprecated",
                "readOnly",
                "writeOnly",
                "title",
            ] {
                // Only schema keywords get here: property names live in `properties`,
                // whose map is walked below without this pass.
                map.remove(key);
            }
            // Strict mode takes `anyOf`, not `oneOf` (serde enums are generated as
            // `oneOf`; their variants never overlap, so the meaning is the same).
            if let Some(one_of) = map.remove("oneOf") {
                map.insert("anyOf".into(), one_of);
            }
            if let Some(constant) = map.remove("const") {
                map.insert("enum".into(), Value::Array(vec![constant]));
            }
            let is_object = map.get("type").is_some_and(|t| {
                t == "object"
                    || t.as_array()
                        .is_some_and(|ts| ts.iter().any(|t| t == "object"))
            }) || map.contains_key("properties");
            if is_object {
                let names: Vec<Value> = map
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|props| props.keys().cloned().map(Value::String).collect())
                    .unwrap_or_default();
                map.entry("properties")
                    .or_insert_with(|| Value::Object(Default::default()));
                map.insert("required".into(), Value::Array(names));
                map.insert("additionalProperties".into(), Value::Bool(false));
            }
            for (key, value) in map.iter_mut() {
                match key.as_str() {
                    // Maps of names to schemas.
                    "properties" | "$defs" | "definitions" => {
                        if let Value::Object(props) = value {
                            props.values_mut().for_each(make_strict);
                        }
                    }
                    // Doc comments wrap lines; the model reads one paragraph.
                    "description" => {
                        if let Value::String(text) = value {
                            *text = text.replace('\n', " ");
                        }
                    }
                    // Literal values, not schemas.
                    "enum" | "required" => {}
                    _ => make_strict(value),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(make_strict),
        _ => {}
    }
}

/// Checks that `schema` is valid for strict function calling, as the API enforces it:
/// the root is an object; every object lists all its properties in `required` and has
/// `"additionalProperties": false`; no `oneOf`, `$ref`, `format`, `default` or
/// `uniqueItems`. `Err` names the first problem and where. The strict-schema test runs
/// it over every registered tool.
pub fn check_strict_schema(schema: &Value) -> Result<(), String> {
    if schema.get("type") != Some(&Value::String("object".into())) {
        return Err("the root must be an object".into());
    }
    check_strict_at(schema, "$")
}

fn check_strict_at(schema: &Value, at: &str) -> Result<(), String> {
    let map = match schema {
        Value::Object(map) => map,
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                check_strict_at(item, &format!("{at}[{i}]"))?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    for key in [
        "oneOf",
        "$ref",
        "format",
        "default",
        "uniqueItems",
        "allOf",
        "not",
    ] {
        if map.contains_key(key) {
            return Err(format!("{at}: `{key}` isn't allowed"));
        }
    }
    let is_object = map.get("type").is_some_and(|t| {
        t == "object"
            || t.as_array()
                .is_some_and(|ts| ts.iter().any(|t| t == "object"))
    });
    if is_object {
        let props = map
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{at}: an object needs `properties`"))?;
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(format!("{at}: `additionalProperties` must be false"));
        }
        let required: Vec<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for name in props.keys() {
            if !required.contains(&name.as_str()) {
                return Err(format!("{at}: `{name}` must be required"));
            }
        }
    }
    for (key, value) in map {
        match key.as_str() {
            "properties" | "$defs" | "definitions" => {
                if let Value::Object(props) = value {
                    for (name, prop) in props {
                        check_strict_at(prop, &format!("{at}.{name}"))?;
                    }
                }
            }
            "enum" | "required" | "description" => {}
            _ => check_strict_at(value, &format!("{at}.{key}"))?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoDevices;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn short_outputs_are_unchanged() {
        assert_eq!(truncate_output("hello", 5), "hello");
        assert_eq!(truncate_output("", 0), "");
    }

    #[test]
    fn long_outputs_are_cut_with_a_marker_within_max() {
        let text = "a".repeat(1000);
        let cut = truncate_output(&text, 100);
        assert!(cut.len() <= 100, "{}", cut.len());
        assert!(cut.starts_with("aaaa"));
        let kept = cut.find('\n').unwrap();
        assert!(cut.ends_with(&format!("[… cut: {} more bytes]", 1000 - kept)));
    }

    #[test]
    fn cuts_fall_on_char_boundaries() {
        // "é" is two bytes, "🦀" four: every possible cut must stay valid UTF-8.
        let text = "é🦀".repeat(300);
        for max in 30..200 {
            let cut = truncate_output(&text, max);
            assert!(cut.len() <= max);
            let kept = cut.find('\n').unwrap();
            assert!(text.is_char_boundary(kept));
            assert_eq!(&cut[..kept], &text[..kept]);
        }
        // Too small for the marker: just the start that fits.
        assert_eq!(truncate_output(&text, 3), "é");
    }

    #[test]
    fn generic_labels() {
        assert_eq!(generic_label("search_library"), "Search library");
        assert_eq!(generic_label(""), "");
    }

    /// Where to look.
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// What to look for.
        query: String,
        limit: Option<u32>,
        tags: Vec<String>,
        place: Option<Place>,
        mode: Mode,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Place {
        page: u32,
        note: Option<String>,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Mode {
        Fast,
        Deep { depth: Option<u8> },
    }

    #[test]
    fn strict_schemas_follow_the_rules() {
        let schema = strict_schema::<Args>();
        check_strict_schema(&schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        // serde_json keeps object keys sorted.
        assert_eq!(
            schema["required"],
            json!(["limit", "mode", "place", "query", "tags"])
        );
        assert_eq!(
            schema["properties"]["place"]["required"],
            json!(["note", "page"])
        );
        assert_eq!(
            schema["properties"]["query"]["description"],
            "What to look for."
        );
        assert_eq!(
            schema["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        assert!(schema["properties"]["limit"].get("format").is_none());
        assert!(schema.get("$schema").is_none());
        let text = schema.to_string();
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("oneOf"), "{text}");
    }

    #[test]
    fn the_strict_checker_catches_loose_schemas() {
        assert!(
            check_strict_schema(
                &json!({"type": "object", "properties": {"a": {"type": "string"}}})
            )
            .is_err()
        );
        assert!(
            check_strict_schema(&json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": [], "additionalProperties": false})).is_err()
        );
        assert!(
            check_strict_schema(&json!({"type": "object", "properties": {"a": {"type": "string", "format": "uint"}}, "required": ["a"], "additionalProperties": false})).is_err()
        );
        assert!(
            check_strict_schema(&json!({"type": "object", "properties": {"a": {"oneOf": []}}, "required": ["a"], "additionalProperties": false})).is_err()
        );
    }

    struct Echo;

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
    }

    impl TypedTool for Echo {
        type Args = EchoArgs;
        const NAME: &'static str = "echo_text";
        const DESCRIPTION: &'static str = "Echoes.";
        const KIND: ToolKind = ToolKind::Write;

        fn label(&self, args: &EchoArgs) -> String {
            format!("Echoing “{}”", args.text)
        }

        fn confirmation<'a>(
            &'a self,
            _ctx: &'a ToolContext,
            args: &'a EchoArgs,
        ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
            Box::pin(async move {
                if args.text == "missing" {
                    Err("No such thing.".into())
                } else {
                    Ok(None)
                }
            })
        }

        fn run<'a>(&'a self, _ctx: &'a ToolContext, args: EchoArgs) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move { ToolOutput::ok(args.text.clone(), "echoed") })
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            chat_id: "c".into(),
            turn_id: "t".into(),
            call_id: "call".into(),
            origin_device: DeviceRef {
                device_id: "d".into(),
                name: "Mac".into(),
            },
            workspaces: vec![],
            devices: Arc::new(NoDevices),
        }
    }

    #[tokio::test]
    async fn typed_tools_parse_their_arguments() {
        let tool = Typed(Echo);
        let ctx = ctx();
        assert_eq!(tool.label(&json!({"text": "hi"})), "Echoing “hi”");
        assert_eq!(tool.label(&json!({"nope": 1})), "Echo text");
        let out = tool.run(&ctx, json!({"text": "hi"})).await;
        assert!(out.ok);
        assert_eq!(out.output, "hi");
        let out = tool.run(&ctx, json!({"text": 3})).await;
        assert!(!out.ok);
        assert!(out.summary.starts_with("The arguments don't fit echo_text"));
        assert!(out.output.starts_with("{\"error\":"));
        assert_eq!(
            tool.confirmation(&ctx, &json!({"text": "missing"})).await,
            Err("No such thing.".into())
        );
        assert!(tool.confirmation(&ctx, &json!({})).await.is_err());
        assert_eq!(
            tool.confirmation(&ctx, &json!({"text": "x"})).await,
            Ok(None)
        );
    }
}
