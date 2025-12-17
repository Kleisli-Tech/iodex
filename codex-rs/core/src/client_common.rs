use crate::client_common::tools::ToolSpec;
use crate::error::Result;
use crate::openai_models::model_family::ModelFamily;
pub use codex_api::common::ResponseEvent;
use codex_apply_patch::APPLY_PATCH_TOOL_INSTRUCTIONS;
use codex_protocol::models::ResponseItem;
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Deref;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;

/// Review thread system prompt. Edit `core/src/review_prompt.md` to customize.
pub const REVIEW_PROMPT: &str = include_str!("../review_prompt.md");

// Centralized templates for review-related user messages
pub const REVIEW_EXIT_SUCCESS_TMPL: &str = include_str!("../templates/review/exit_success.xml");
pub const REVIEW_EXIT_INTERRUPTED_TMPL: &str =
    include_str!("../templates/review/exit_interrupted.xml");

/// API request payload for a single model turn
#[derive(Default, Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Vec<ResponseItem>,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub(crate) tools: Vec<ToolSpec>,

    /// Whether parallel tool calls are permitted for this prompt.
    pub(crate) parallel_tool_calls: bool,

    /// Optional override for the built-in BASE_INSTRUCTIONS.
    pub base_instructions_override: Option<String>,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,
}

impl Prompt {
    pub(crate) fn get_full_instructions<'a>(&'a self, model: &'a ModelFamily) -> Cow<'a, str> {
        let base = self
            .base_instructions_override
            .as_deref()
            .unwrap_or(model.base_instructions.deref());
        // When there are no custom instructions, add apply_patch_tool_instructions if:
        // - the model needs special instructions (4.1)
        // AND
        // - there is no apply_patch tool present
        let is_apply_patch_tool_present = self.tools.iter().any(|tool| match tool {
            ToolSpec::Function(f) => f.name == "apply_patch",
            ToolSpec::Freeform(f) => f.name == "apply_patch",
            _ => false,
        });
        if self.base_instructions_override.is_none()
            && model.needs_special_apply_patch_instructions
            && !is_apply_patch_tool_present
        {
            Cow::Owned(format!("{base}\n{APPLY_PATCH_TOOL_INSTRUCTIONS}"))
        } else {
            Cow::Borrowed(base)
        }
    }

    pub(crate) fn get_formatted_input(&self) -> Vec<ResponseItem> {
        let mut input = self.input.clone();

        // when using the *Freeform* apply_patch tool specifically, tool outputs
        // should be structured text, not json. Do NOT reserialize when using
        // the Function tool - note that this differs from the check above for
        // instructions. We declare the result as a named variable for clarity.
        let is_freeform_apply_patch_tool_present = self.tools.iter().any(|tool| match tool {
            ToolSpec::Freeform(f) => f.name == "apply_patch",
            _ => false,
        });
        if is_freeform_apply_patch_tool_present {
            reserialize_shell_outputs(&mut input);
        }

        input
    }
}

fn reserialize_shell_outputs(items: &mut [ResponseItem]) {
    let mut shell_call_ids: HashSet<String> = HashSet::new();

    items.iter_mut().for_each(|item| match item {
        ResponseItem::LocalShellCall { call_id, id, .. } => {
            if let Some(identifier) = call_id.clone().or_else(|| id.clone()) {
                shell_call_ids.insert(identifier);
            }
        }
        ResponseItem::CustomToolCall {
            id: _,
            status: _,
            call_id,
            name,
            input: _,
        } => {
            if name == "apply_patch" {
                shell_call_ids.insert(call_id.clone());
            }
        }
        ResponseItem::CustomToolCallOutput { call_id, output } => {
            if shell_call_ids.remove(call_id)
                && let Some(structured) = parse_structured_shell_output(output)
            {
                *output = structured
            }
        }
        ResponseItem::FunctionCall { name, call_id, .. }
            if is_shell_tool_name(name) || name == "apply_patch" =>
        {
            shell_call_ids.insert(call_id.clone());
        }
        ResponseItem::FunctionCallOutput { call_id, output } => {
            if shell_call_ids.remove(call_id)
                && let Some(structured) = parse_structured_shell_output(&output.content)
            {
                output.content = structured
            }
        }
        _ => {}
    })
}

fn is_shell_tool_name(name: &str) -> bool {
    matches!(name, "shell" | "container.exec")
}

#[derive(Deserialize)]
struct ExecOutputJson {
    output: String,
    metadata: ExecOutputMetadataJson,
}

#[derive(Deserialize)]
struct ExecOutputMetadataJson {
    exit_code: i32,
    duration_seconds: f32,
}

fn parse_structured_shell_output(raw: &str) -> Option<String> {
    let parsed: ExecOutputJson = serde_json::from_str(raw).ok()?;
    Some(build_structured_output(&parsed))
}

fn build_structured_output(parsed: &ExecOutputJson) -> String {
    let mut sections = Vec::new();
    sections.push(format!("Exit code: {}", parsed.metadata.exit_code));
    sections.push(format!(
        "Wall time: {} seconds",
        parsed.metadata.duration_seconds
    ));

    let mut output = parsed.output.clone();
    if let Some((stripped, total_lines)) = strip_total_output_header(&parsed.output) {
        sections.push(format!("Total output lines: {total_lines}"));
        output = stripped.to_string();
    }

    sections.push("Output:".to_string());
    sections.push(output);

    sections.join("\n")
}

fn strip_total_output_header(output: &str) -> Option<(&str, u32)> {
    let after_prefix = output.strip_prefix("Total output lines: ")?;
    let (total_segment, remainder) = after_prefix.split_once('\n')?;
    let total_lines = total_segment.parse::<u32>().ok()?;
    let remainder = remainder.strip_prefix('\n').unwrap_or(remainder);
    Some((remainder, total_lines))
}

pub(crate) mod tools {
    use crate::tools::spec::JsonSchema;
    use serde::Deserialize;
    use serde::Serialize;

    /// When serialized as JSON, this produces a valid "Tool" in the OpenAI
    /// Responses API.
    #[derive(Debug, Clone, Serialize, PartialEq)]
    #[serde(tag = "type")]
    pub(crate) enum ToolSpec {
        #[serde(rename = "function")]
        Function(ResponsesApiTool),
        #[serde(rename = "local_shell")]
        LocalShell {},
        // TODO: Understand why we get an error on web_search although the API docs say it's supported.
        // https://platform.openai.com/docs/guides/tools-web-search?api-mode=responses#:~:text=%7B%20type%3A%20%22web_search%22%20%7D%2C
        #[serde(rename = "web_search")]
        WebSearch {},
        #[serde(rename = "custom")]
        Freeform(FreeformTool),
    }

    impl ToolSpec {
        pub(crate) fn name(&self) -> &str {
            match self {
                ToolSpec::Function(tool) => tool.name.as_str(),
                ToolSpec::LocalShell {} => "local_shell",
                ToolSpec::WebSearch {} => "web_search",
                ToolSpec::Freeform(tool) => tool.name.as_str(),
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformTool {
        pub(crate) name: String,
        pub(crate) description: String,
        pub(crate) format: FreeformToolFormat,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformToolFormat {
        pub(crate) r#type: String,
        pub(crate) syntax: String,
        pub(crate) definition: String,
    }

    #[derive(Debug, Clone, Serialize, PartialEq)]
    pub struct ResponsesApiTool {
        pub(crate) name: String,
        pub(crate) description: String,
        /// TODO: Validation. When strict is set to true, the JSON schema,
        /// `required` and `additional_properties` must be present. All fields in
        /// `properties` must be present in `required`.
        pub(crate) strict: bool,
        pub(crate) parameters: JsonSchema,
    }
}

pub struct ResponseStream {
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

pub(crate) mod anthropic {
    use super::Prompt;
    use super::Result;
    use crate::openai_models::model_family::ModelFamily;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use serde_json::Value;
    use serde_json::json;
    use std::collections::HashMap;
    use std::collections::HashSet;

    pub(crate) fn build_anthropic_messages_body(
        prompt: &Prompt,
        model_family: &ModelFamily,
        instructions: String,
        tools_json: Vec<Value>,
    ) -> Result<Value> {
        let model = model_family.get_model_slug().to_string();
        let system = instructions;
        let messages = response_items_to_anthropic_messages(prompt);

        let tool_choice = if !tools_json.is_empty() && model_family.supports_parallel_tool_calls {
            Some(json!({ "type": "auto" }))
        } else {
            None
        };

        let max_tokens = compute_max_tokens(model_family);

        let mut body = json!({
            "model": model,
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });

        if !tools_json.is_empty() {
            body.as_object_mut()
                .expect("anthropic body is object")
                .insert("tools".to_string(), Value::Array(tools_json));
        }

        if let Some(choice) = tool_choice {
            body.as_object_mut()
                .expect("anthropic body is object")
                .insert("tool_choice".to_string(), choice);
        }

        Ok(body)
    }

    fn compute_max_tokens(model_family: &ModelFamily) -> i64 {
        let context_window = model_family.context_window.unwrap_or(200_000);
        let effective_limit = model_family
            .auto_compact_token_limit()
            .unwrap_or(context_window);
        let fraction = effective_limit.saturating_mul(25).saturating_div(100);
        let hard_cap = 64_000;
        let capped = fraction.min(hard_cap);
        capped.max(1)
    }

    /// Parse a data URL into (media_type, base64_data).
    /// Expected format: data:image/png;base64,<data>
    fn parse_data_url(url: &str) -> Option<(String, String)> {
        let url = url.strip_prefix("data:")?;
        let (meta, data) = url.split_once(',')?;
        let media_type = meta.strip_suffix(";base64")?.to_string();
        Some((media_type, data.to_string()))
    }

    fn response_items_to_anthropic_messages(prompt: &Prompt) -> Vec<Value> {
        let input = prompt.get_formatted_input();
        let mut messages: Vec<Value> = Vec::new();

        // Build a lookup from call_id -> first FunctionCallOutput index so we can
        // emit tool_use and tool_result as adjacent messages, as required by
        // the Anthropic Messages tools protocol.
        let mut first_output_index_by_call_id: HashMap<&str, usize> = HashMap::new();
        for (idx, item) in input.iter().enumerate() {
            if let ResponseItem::FunctionCallOutput { call_id, .. } = item {
                first_output_index_by_call_id
                    .entry(call_id.as_str())
                    .or_insert(idx);
            }
        }
        let mut consumed_output_indices: HashSet<usize> = HashSet::new();

        let mut index = 0;
        while index < input.len() {
            match &input[index] {
                ResponseItem::Message { role, content, .. } => {
                    let mut blocks: Vec<Value> = Vec::new();
                    for c in content {
                        match c {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                if !text.trim().is_empty() {
                                    blocks.push(json!({ "type": "text", "text": text }));
                                }
                            }
                            ContentItem::InputImage { image_url } => {
                                // Anthropic accepts both URL and base64 formats
                                if image_url.starts_with("data:") {
                                    // Parse data URL: data:image/png;base64,<data>
                                    if let Some((media_type, data)) = parse_data_url(image_url) {
                                        blocks.push(json!({
                                            "type": "image",
                                            "source": {
                                                "type": "base64",
                                                "media_type": media_type,
                                                "data": data,
                                            }
                                        }));
                                    }
                                } else {
                                    // External URL
                                    blocks.push(json!({
                                        "type": "image",
                                        "source": {
                                            "type": "url",
                                            "url": image_url,
                                        }
                                    }));
                                }
                            }
                        }
                    }
                    if !blocks.is_empty() {
                        messages.push(json!({
                            "role": role,
                            "content": blocks,
                        }));
                    }
                }
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    let input_json: Value =
                        serde_json::from_str(&arguments).unwrap_or_else(|_| json!(arguments));
                    messages.push(json!({
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": input_json,
                        }],
                    }));

                    // If we have a corresponding FunctionCallOutput later in the
                    // history, emit its tool_result immediately after this tool_use
                    // so that Anthropic sees the required adjacency.
                    if let Some(output_index) =
                        first_output_index_by_call_id.get(call_id.as_str()).copied()
                    {
                        if output_index > index
                            && !consumed_output_indices.contains(&output_index)
                            && matches!(
                                input.get(output_index),
                                Some(ResponseItem::FunctionCallOutput { .. })
                            )
                        {
                            if let Some(ResponseItem::FunctionCallOutput { output, .. }) =
                                input.get(output_index)
                            {
                                let content_value = function_output_to_tool_result_content(output);
                                messages.push(json!({
                                    "role": "user",
                                    "content": [{
                                        "type": "tool_result",
                                        "tool_use_id": call_id,
                                        "content": content_value,
                                    }],
                                }));
                                consumed_output_indices.insert(output_index);
                            }
                        }
                    }
                }
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    // If this output was not already paired with its call above
                    // (for example, if the call is missing or out of order), emit
                    // a standalone tool_result message to preserve the transcript.
                    if !consumed_output_indices.contains(&index) {
                        let content_value = function_output_to_tool_result_content(output);
                        messages.push(json!({
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": content_value,
                            }],
                        }));
                    }
                }
                ResponseItem::Reasoning { .. }
                | ResponseItem::LocalShellCall { .. }
                | ResponseItem::CustomToolCall { .. }
                | ResponseItem::CustomToolCallOutput { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::GhostSnapshot { .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::Other => {}
            }
            index += 1;
        }

        messages
    }

    fn function_output_to_tool_result_content(output: &FunctionCallOutputPayload) -> Value {
        if let Some(items) = &output.content_items {
            let mapped: Vec<Value> = items
                .iter()
                .filter_map(|it| match it {
                    FunctionCallOutputContentItem::InputText { text } => {
                        if text.trim().is_empty() {
                            None
                        } else {
                            Some(json!({ "type": "text", "text": text }))
                        }
                    }
                    FunctionCallOutputContentItem::InputImage { image_url } => Some(json!({
                        "type": "image_url",
                        "image_url": { "url": image_url },
                    })),
                })
                .collect();
            if mapped.is_empty() && !output.content.trim().is_empty() {
                Value::Array(vec![json!({
                    "type": "text",
                    "text": output.content,
                })])
            } else {
                Value::Array(mapped)
            }
        } else {
            if output.content.trim().is_empty() {
                Value::Array(vec![])
            } else {
                Value::Array(vec![json!({
                    "type": "text",
                    "text": output.content,
                })])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_api::ResponsesApiRequest;
    use codex_api::common::OpenAiVerbosity;
    use codex_api::common::TextControls;
    use codex_api::create_text_param_for_request;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    use crate::config::test_config;
    use crate::openai_models::models_manager::ModelsManager;

    use super::*;

    struct InstructionsTestCase {
        pub slug: &'static str,
        pub expects_apply_patch_instructions: bool,
    }
    #[test]
    fn get_full_instructions_no_user_content() {
        let prompt = Prompt {
            ..Default::default()
        };
        let test_cases = vec![
            InstructionsTestCase {
                slug: "gpt-3.5",
                expects_apply_patch_instructions: true,
            },
            InstructionsTestCase {
                slug: "gpt-4.1",
                expects_apply_patch_instructions: true,
            },
            InstructionsTestCase {
                slug: "gpt-4o",
                expects_apply_patch_instructions: true,
            },
            InstructionsTestCase {
                slug: "gpt-5",
                expects_apply_patch_instructions: true,
            },
            InstructionsTestCase {
                slug: "gpt-5.1",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "codex-mini-latest",
                expects_apply_patch_instructions: true,
            },
            InstructionsTestCase {
                slug: "gpt-oss:120b",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex-max",
                expects_apply_patch_instructions: false,
            },
        ];
        for test_case in test_cases {
            let config = test_config();
            let model_family =
                ModelsManager::construct_model_family_offline(test_case.slug, &config);
            let expected = if test_case.expects_apply_patch_instructions {
                format!(
                    "{}\n{}",
                    model_family.clone().base_instructions,
                    APPLY_PATCH_TOOL_INSTRUCTIONS
                )
            } else {
                model_family.clone().base_instructions
            };

            let full = prompt.get_full_instructions(&model_family);
            assert_eq!(full, expected);
        }
    }

    #[test]
    fn serializes_text_verbosity_when_set() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: Some(TextControls {
                verbosity: Some(OpenAiVerbosity::Low),
                format: None,
            }),
        };

        let v = serde_json::to_value(&req).expect("json");
        assert_eq!(
            v.get("text")
                .and_then(|t| t.get("verbosity"))
                .and_then(|s| s.as_str()),
            Some("low")
        );
    }

    #[test]
    fn serializes_text_schema_with_strict_format() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"}
            },
            "required": ["answer"],
        });
        let text_controls =
            create_text_param_for_request(None, &Some(schema.clone())).expect("text controls");

        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: Some(text_controls),
        };

        let v = serde_json::to_value(&req).expect("json");
        let text = v.get("text").expect("text field");
        assert!(text.get("verbosity").is_none());
        let format = text.get("format").expect("format field");

        assert_eq!(
            format.get("name"),
            Some(&serde_json::Value::String("codex_output_schema".into()))
        );
        assert_eq!(
            format.get("type"),
            Some(&serde_json::Value::String("json_schema".into()))
        );
        assert_eq!(format.get("strict"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(format.get("schema"), Some(&schema));
    }

    #[test]
    fn omits_text_when_not_set() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: None,
        };

        let v = serde_json::to_value(&req).expect("json");
        assert!(v.get("text").is_none());
    }

    #[test]
    fn anthropic_body_includes_basic_fields() {
        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "Hello, Claude".to_string(),
            }],
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();

        let tools_json: Vec<serde_json::Value> = Vec::new();
        let body = crate::client_common::anthropic::build_anthropic_messages_body(
            &prompt,
            &model_family,
            instructions.clone(),
            tools_json,
        )
        .expect("anthropic body");

        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("claude-sonnet-4.5")
        );
        assert_eq!(
            body.get("system").and_then(|v| v.as_str()),
            Some(instructions.as_str())
        );
        assert_eq!(body.get("stream").and_then(|v| v.as_bool()), Some(true));

        let max_tokens = body.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        assert!(max_tokens > 0);
        assert!(max_tokens <= 64_000);

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 1);
        let first = messages.first().cloned().unwrap();
        assert_eq!(first.get("role").and_then(|v| v.as_str()), Some("user"));
    }

    #[test]
    fn anthropic_tool_use_and_result_are_adjacent() {
        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "Run a tool and continue".to_string(),
            }],
        });

        let call_id = "toolu_01ABC";
        prompt.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "update_plan".to_string(),
            arguments: r#"{"step":"one"}"#.to_string(),
            call_id: call_id.to_string(),
        });

        // In realistic history, assistant text may be recorded between the call and
        // its output; the Anthropic mapping must still emit tool_result immediately
        // after the corresponding tool_use.
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![codex_protocol::models::ContentItem::OutputText {
                text: "Running the plan tool...".to_string(),
            }],
        });

        prompt.input.push(ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload {
                content: "ok".to_string(),
                content_items: None,
                success: Some(true),
            },
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();

        let tools_json: Vec<serde_json::Value> = Vec::new();
        let body = crate::client_common::anthropic::build_anthropic_messages_body(
            &prompt,
            &model_family,
            instructions,
            tools_json,
        )
        .expect("anthropic body");

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        assert!(
            messages.len() >= 3,
            "expected at least user, tool_use, tool_result messages"
        );

        // messages[0] should be the initial user message.
        assert_eq!(
            messages[0].get("role").and_then(|v| v.as_str()),
            Some("user")
        );

        // messages[1] should contain the tool_use block.
        let second = &messages[1];
        assert_eq!(
            second.get("role").and_then(|v| v.as_str()),
            Some("assistant")
        );
        let second_blocks = second
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            second_blocks.iter().any(|block| {
                block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && block.get("id").and_then(|v| v.as_str()) == Some(call_id)
            }),
            "expected tool_use block with the call id in second message"
        );

        // messages[2] must immediately contain the corresponding tool_result block.
        let third = &messages[2];
        assert_eq!(third.get("role").and_then(|v| v.as_str()), Some("user"));
        let third_blocks = third
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            third_blocks.iter().any(|block| {
                block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && block.get("tool_use_id").and_then(|v| v.as_str()) == Some(call_id)
            }),
            "expected tool_result block referencing the same call id in third message"
        );
    }

    #[test]
    fn anthropic_omits_whitespace_only_message_text_blocks() {
        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                codex_protocol::models::ContentItem::InputText {
                    text: "   ".to_string(),
                },
                codex_protocol::models::ContentItem::InputText {
                    text: "kept".to_string(),
                },
            ],
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();

        let tools_json: Vec<serde_json::Value> = Vec::new();
        let body = crate::client_common::anthropic::build_anthropic_messages_body(
            &prompt,
            &model_family,
            instructions,
            tools_json,
        )
        .expect("anthropic body");

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 1);

        let content_blocks = messages[0]
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let texts: Vec<String> = content_blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(ToString::to_string)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(texts, vec!["kept".to_string()]);
    }

    #[test]
    fn anthropic_tool_result_does_not_emit_whitespace_only_text_blocks() {
        let mut prompt = Prompt::default();
        let call_id = "tool_01EMPTY";
        prompt.input.push(ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload {
                content: "   ".to_string(),
                content_items: None,
                success: Some(true),
            },
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();
        let tools_json: Vec<serde_json::Value> = Vec::new();
        let body = crate::client_common::anthropic::build_anthropic_messages_body(
            &prompt,
            &model_family,
            instructions,
            tools_json,
        )
        .expect("anthropic body");

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 1);

        let content_blocks = messages[0]
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(content_blocks.len(), 1);

        let tool_result = &content_blocks[0];
        assert_eq!(
            tool_result.get("type").and_then(|t| t.as_str()),
            Some("tool_result")
        );

        let tool_result_content = tool_result
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let texts: Vec<String> = tool_result_content
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(ToString::to_string)
                } else {
                    None
                }
            })
            .collect();

        assert!(
            texts.iter().all(|t| !t.trim().is_empty()),
            "expected no whitespace-only text blocks in tool_result content"
        );
    }

    #[test]
    fn anthropic_image_base64_format() {
        use super::anthropic::build_anthropic_messages_body;

        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
            }],
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();
        let tools_json: Vec<serde_json::Value> = Vec::new();

        let body = build_anthropic_messages_body(&prompt, &model_family, instructions, tools_json)
            .expect("anthropic body");

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        let content = messages[0]
            .get("content")
            .and_then(|v| v.as_array())
            .expect("content array");
        let image_block = &content[0];

        assert_eq!(
            image_block.get("type").and_then(|v| v.as_str()),
            Some("image")
        );
        let source = image_block.get("source").expect("source object");
        assert_eq!(source.get("type").and_then(|v| v.as_str()), Some("base64"));
        assert_eq!(
            source.get("media_type").and_then(|v| v.as_str()),
            Some("image/png")
        );
        assert_eq!(
            source.get("data").and_then(|v| v.as_str()),
            Some("iVBORw0KGgo=")
        );
    }

    #[test]
    fn anthropic_image_url_format() {
        use super::anthropic::build_anthropic_messages_body;

        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "https://example.com/image.png".to_string(),
            }],
        });

        let config = test_config();
        let model_family =
            ModelsManager::construct_model_family_offline("claude-sonnet-4.5", &config);
        let instructions = prompt.get_full_instructions(&model_family).into_owned();
        let tools_json: Vec<serde_json::Value> = Vec::new();

        let body = build_anthropic_messages_body(&prompt, &model_family, instructions, tools_json)
            .expect("anthropic body");

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        let content = messages[0]
            .get("content")
            .and_then(|v| v.as_array())
            .expect("content array");
        let image_block = &content[0];

        assert_eq!(
            image_block.get("type").and_then(|v| v.as_str()),
            Some("image")
        );
        let source = image_block.get("source").expect("source object");
        assert_eq!(source.get("type").and_then(|v| v.as_str()), Some("url"));
        assert_eq!(
            source.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/image.png")
        );
    }
}
