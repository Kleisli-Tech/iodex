use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
// use codex_client::TransportError;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

pub(crate) fn spawn_anthropic_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        process_anthropic_sse(stream_response.bytes, tx_event, idle_timeout, telemetry).await;
    });
    ResponseStream { rx_event }
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    total_tokens: Option<i64>,
}

impl From<AnthropicUsage> for TokenUsage {
    fn from(usage: AnthropicUsage) -> Self {
        let input_tokens = usage.input_tokens.unwrap_or(0);
        let cached_input_tokens = usage
            .cache_creation_input_tokens
            .unwrap_or(0)
            .saturating_add(usage.cache_read_input_tokens.unwrap_or(0));
        let output_tokens = usage.output_tokens.unwrap_or(0);
        let total_tokens = usage.total_tokens.unwrap_or(
            input_tokens
                .saturating_add(cached_input_tokens)
                .saturating_add(output_tokens),
        );

        TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_output_tokens: 0,
            total_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    id: String,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicSseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    message: Option<AnthropicMessage>,
    #[serde(default)]
    content_block: Option<AnthropicContentBlock>,
    #[serde(default)]
    delta: Option<AnthropicDelta>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    error: Option<AnthropicErrorPayload>,
}

#[derive(Default)]
struct ToolUseState {
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    partial_json: String,
}

pub async fn process_anthropic_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    let mut stream = stream.eventsource();

    let mut assistant_started = false;
    let mut assistant_text = String::new();
    let mut response_id: Option<String> = None;
    let mut token_usage: Option<TokenUsage> = None;
    let mut completed_sent = false;
    let mut tool_use_blocks: HashMap<usize, ToolUseState> = HashMap::new();

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }

        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("Anthropic SSE error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                if !completed_sent {
                    let error = ApiError::Stream("stream closed before message_stop".to_string());
                    let _ = tx_event.send(Err(error)).await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "idle timeout waiting for Anthropic SSE".to_string(),
                    )))
                    .await;
                return;
            }
        };

        let raw = sse.data.clone();
        trace!("Anthropic SSE event: {raw}");

        if sse.data.trim().is_empty() {
            continue;
        }

        let event: AnthropicSseEvent = match serde_json::from_str(&sse.data) {
            Ok(event) => event,
            Err(err) => {
                debug!(
                    "Failed to parse Anthropic SSE event: {err}, data: {}",
                    &sse.data
                );
                continue;
            }
        };

        match event.kind.as_str() {
            "message_start" => {
                if let Some(msg) = event.message {
                    response_id.get_or_insert(msg.id);
                    if let Some(usage) = msg.usage {
                        token_usage = Some(usage.into());
                    }
                }
            }
            "message_delta" => {
                if let Some(msg) = event.message {
                    if let Some(usage) = msg.usage {
                        token_usage = Some(usage.into());
                    }
                }
                if let Some(usage) = event.usage {
                    token_usage = Some(usage.into());
                }
            }
            "content_block_start" => {
                if let (Some(index), Some(block)) = (event.index, event.content_block) {
                    if block.kind == "tool_use" {
                        let state = tool_use_blocks.entry(index).or_default();
                        state.id = block.id;
                        state.name = block.name;
                        state.input = block.input;
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    if let Some(text) = delta.text {
                        if !text.is_empty() {
                            if !assistant_started {
                                assistant_started = true;
                                let item = ResponseItem::Message {
                                    id: None,
                                    role: "assistant".to_string(),
                                    content: Vec::new(),
                                };
                                if tx_event
                                    .send(Ok(ResponseEvent::OutputItemAdded(item)))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            assistant_text.push_str(&text);
                            if tx_event
                                .send(Ok(ResponseEvent::OutputTextDelta(text)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }

                    if let (Some(index), Some(kind)) = (event.index, delta.kind.as_deref()) {
                        if kind == "input_json_delta" {
                            if let Some(part) = delta.partial_json {
                                let state = tool_use_blocks.entry(index).or_default();
                                state.partial_json.push_str(&part);
                            }
                        }
                    }
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.index {
                    if let Some(state) = tool_use_blocks.remove(&index) {
                        if state.name.is_some() || state.id.is_some() {
                            let mut input_value = state.input.unwrap_or(Value::Null);
                            if !state.partial_json.is_empty() {
                                match serde_json::from_str::<Value>(&state.partial_json) {
                                    Ok(val) => {
                                        input_value = val;
                                    }
                                    Err(err) => {
                                        let _ = tx_event
                                            .send(Err(ApiError::Stream(format!(
                                                "failed to parse tool_use input_json_delta: {err}"
                                            ))))
                                            .await;
                                        return;
                                    }
                                }
                            }

                            let arguments = match serde_json::to_string(&input_value) {
                                Ok(s) => s,
                                Err(err) => {
                                    let _ = tx_event
                                        .send(Err(ApiError::Stream(format!(
                                            "failed to serialize tool_use arguments: {err}"
                                        ))))
                                        .await;
                                    return;
                                }
                            };

                            let name = state.name.unwrap_or_default();
                            let call_id = state.id.unwrap_or_else(|| format!("tool-call-{index}"));

                            let item = ResponseItem::FunctionCall {
                                id: None,
                                name,
                                arguments,
                                call_id,
                            };
                            if tx_event
                                .send(Ok(ResponseEvent::OutputItemDone(item)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
            "message_stop" => {
                if let Some(msg) = event.message {
                    response_id.get_or_insert(msg.id);
                    if let Some(usage) = msg.usage {
                        token_usage = Some(usage.into());
                    }
                }
                if let Some(usage) = event.usage {
                    token_usage = Some(usage.into());
                }

                if assistant_started {
                    let item = ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: assistant_text.clone(),
                        }],
                    };
                    assistant_started = false;
                    assistant_text.clear();
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                let completed = ResponseEvent::Completed {
                    response_id: response_id.clone().unwrap_or_default(),
                    token_usage: token_usage.clone(),
                };
                completed_sent = true;
                if tx_event.send(Ok(completed)).await.is_err() {
                    return;
                }
            }
            "error" => {
                let payload = event.error;
                let message = payload
                    .as_ref()
                    .and_then(|e| e.message.as_deref())
                    .unwrap_or("unknown Anthropic error");
                let code = payload
                    .as_ref()
                    .and_then(|e| e.code.as_deref())
                    .unwrap_or_default();
                let kind = payload
                    .as_ref()
                    .and_then(|e| e.kind.as_deref())
                    .unwrap_or_default();

                let description = if code.is_empty() && kind.is_empty() {
                    format!("Anthropic error: {message}")
                } else {
                    format!("Anthropic error ({kind} {code}): {message}")
                };

                let _ = tx_event.send(Err(ApiError::Stream(description))).await;
                return;
            }
            _ => {
                // Ignore other event types for now (e.g., ping, message_delta without usage).
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ResponseEvent;
    use crate::error::ApiError;
    use bytes::Bytes;
    use futures::StreamExt;
    use futures::TryStreamExt;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::io::ReaderStream;

    fn anthropic_sse(events: Vec<Value>) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for ev in events {
            writeln!(&mut out, "data: {ev}").unwrap();
            out.push('\n');
        }
        out
    }

    async fn collect_events(body: String) -> Vec<Result<ResponseEvent, ApiError>> {
        let reader = std::io::Cursor::new(body);
        let stream =
            ReaderStream::new(reader).map_err(|err| TransportError::Network(err.to_string()));
        let stream: ByteStream = Box::pin(stream.map_ok(Bytes::from));
        let (tx, rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
        process_anthropic_sse(stream, tx, Duration::from_secs(5), None).await;
        let mut events = Vec::new();
        let mut rx = rx;
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn text_only_stream_emits_item_added_delta_done_completed() {
        let body = anthropic_sse(vec![
            json!({"type":"message_start","message":{"id":"msg-1"}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop","message":{"id":"msg-1"},"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}),
        ]);

        let events = collect_events(body).await;
        let mut saw_item_added = false;
        let mut saw_delta = false;
        let mut saw_done = false;
        let mut saw_completed = false;

        for ev in &events {
            match ev {
                Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. })) => {
                    assert_eq!(role, "assistant");
                    saw_item_added = true;
                }
                Ok(ResponseEvent::OutputTextDelta(delta)) => {
                    assert_eq!(delta, "Hello");
                    saw_delta = true;
                }
                Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                    role, content, ..
                })) => {
                    assert_eq!(role, "assistant");
                    assert_eq!(content.len(), 1);
                    if let ContentItem::OutputText { text } = &content[0] {
                        assert_eq!(text, "Hello");
                    } else {
                        panic!("expected output text content");
                    }
                    saw_done = true;
                }
                Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                }) => {
                    assert_eq!(response_id, "msg-1");
                    let usage = token_usage.as_ref().expect("token usage");
                    assert_eq!(usage.input_tokens, 10);
                    assert_eq!(usage.output_tokens, 5);
                    assert_eq!(usage.total_tokens, 15);
                    saw_completed = true;
                }
                _ => {}
            }
        }

        assert!(saw_item_added);
        assert!(saw_delta);
        assert!(saw_done);
        assert!(saw_completed);
    }

    #[tokio::test]
    async fn tool_use_with_full_input_emits_function_call() {
        let call_id = "call-1";
        let arguments = json!({
            "explanation": "test plan",
            "plan": [{ "step": "do thing", "status": "pending" }],
        });

        let body = anthropic_sse(vec![
            json!({"type":"message_start","message":{"id":"msg-1"}}),
            json!({"type":"content_block_start","index":0,"content_block":{
                "type":"tool_use",
                "id": call_id,
                "name": "update_plan",
                "input": arguments,
            }}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop","message":{"id":"msg-1"}}),
        ]);

        let events = collect_events(body).await;
        let mut saw_call = false;

        for ev in &events {
            if let Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                call_id: cid,
                arguments,
                ..
            })) = ev
            {
                assert_eq!(name, "update_plan");
                assert_eq!(cid, "call-1");
                let parsed: Value = serde_json::from_str(arguments).expect("arguments json");
                assert_eq!(
                    parsed,
                    json!({
                        "explanation": "test plan",
                        "plan": [{ "step": "do thing", "status": "pending" }],
                    })
                );
                saw_call = true;
            }
        }

        assert!(saw_call);
    }

    #[tokio::test]
    async fn tool_use_with_input_json_delta_is_assembled() {
        let call_id = "call-1";
        let body = anthropic_sse(vec![
            json!({"type":"message_start","message":{"id":"msg-1"}}),
            json!({"type":"content_block_start","index":0,"content_block":{
                "type":"tool_use",
                "id": call_id,
                "name": "update_plan",
            }}),
            json!({"type":"content_block_delta","index":0,"delta":{
                "type":"input_json_delta",
                "partial_json":"{\"explanation\":\"test\",\"plan\":[{\"step\":\"do thing\",\"status\":\"pending\"}]}"
            }}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop","message":{"id":"msg-1"}}),
        ]);

        let events = collect_events(body).await;
        let mut saw_call = false;

        for ev in &events {
            if let Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                call_id: cid,
                arguments,
                ..
            })) = ev
            {
                assert_eq!(name, "update_plan");
                assert_eq!(cid, "call-1");
                let parsed: Value = serde_json::from_str(arguments).expect("arguments json");
                assert_eq!(
                    parsed,
                    json!({
                        "explanation": "test",
                        "plan": [{ "step": "do thing", "status": "pending" }],
                    })
                );
                saw_call = true;
            }
        }

        assert!(saw_call);
    }

    #[tokio::test]
    async fn error_event_yields_stream_error() {
        let body = anthropic_sse(vec![json!({
            "type":"error",
            "error": {
                "type":"invalid_request_error",
                "code":"bad_request",
                "message":"something went wrong"
            }
        })]);

        let events = collect_events(body).await;
        assert!(
            matches!(
                events.first(),
                Some(Err(ApiError::Stream(msg))) if msg.contains("Anthropic error")
            ),
            "expected Anthropic stream error, got {events:?}",
        );
    }
}
