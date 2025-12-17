#![allow(clippy::expect_used)]

use std::sync::Arc;

use codex_app_server_protocol::AuthMode;
use codex_core::ContentItem;
use codex_core::ModelClient;
use codex_core::ModelProviderInfo;
use codex_core::Prompt;
use codex_core::ResponseItem;
use codex_core::WireApi;
use codex_core::openai_models::models_manager::ModelsManager;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::models::FunctionCallOutputPayload;
use core_test_support::load_default_config_for_test;
use core_test_support::skip_if_no_network;
use futures::StreamExt;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn anthropic_sse(events: Vec<serde_json::Value>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for ev in events {
        writeln!(&mut out, "data: {ev}").unwrap();
        out.push('\n');
    }
    out
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_use_roundtrip_emits_two_requests() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;

    let call_id = "update-plan-call";
    let args = json!({
        "explanation": "test plan",
        "plan": [{ "step": "do thing", "status": "pending" }],
    });

    // Responses don't matter for this test beyond completing the stream, so we
    // reuse a simple message_start/message_stop sequence for both calls.
    let sse_body = anthropic_sse(vec![
        json!({"type":"message_start","message":{"id":"msg-1"}}),
        json!({"type":"message_stop","message":{"id":"msg-1","usage":{
            "input_tokens":1,
            "output_tokens":1,
            "cache_creation_input_tokens":0,
            "cache_read_input_tokens":0
        }}}),
    ]);

    let template = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_body.clone(), "text/event-stream");

    // Serve the same SSE body for each POST to /v1/messages.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(&server)
        .await;

    // Configure a ModelClient that talks to the mock Anthropic Messages endpoint.
    let provider = ModelProviderInfo {
        name: "mock-anthropic".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::AnthropicMessages,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
    };

    let codex_home = TempDir::new().expect("tempdir");
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let conversation_id = ConversationId::new();
    let model_family = ModelsManager::construct_model_family_offline(model.as_str(), &config);
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        model.as_str(),
        model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(AuthMode::ApiKey),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        model_family,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    // First request: user message + tool_use (FunctionCall) only.
    let mut prompt = Prompt::default();
    prompt
        .input
        .push(user_message("update the plan and continue"));
    prompt.input.push(ResponseItem::FunctionCall {
        id: None,
        name: "update_plan".to_string(),
        arguments: args.to_string(),
        call_id: call_id.to_string(),
    });

    let mut stream = client.stream(&prompt).await.expect("stream anthropic 1");
    while let Some(event) = stream.next().await {
        event.expect("stream event 1");
    }

    // Second request: same history plus a tool_result (FunctionCallOutput).
    prompt.input.push(ResponseItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            content: "ok".to_string(),
            content_items: None,
            success: Some(true),
        },
    });

    let mut stream = client.stream(&prompt).await.expect("stream anthropic 2");
    while let Some(event) = stream.next().await {
        event.expect("stream event 2");
    }

    let all_requests = server
        .received_requests()
        .await
        .expect("mock server should not fail");
    let anthropic_requests: Vec<_> = all_requests
        .into_iter()
        .filter(|req| req.method == "POST" && req.url.path() == "/v1/messages")
        .collect();

    assert_eq!(
        anthropic_requests.len(),
        2,
        "expected exactly two Anthropic messages requests"
    );

    let first_body: serde_json::Value = anthropic_requests[0]
        .body_json()
        .expect("first request body json");
    let second_body: serde_json::Value = anthropic_requests[1]
        .body_json()
        .expect("second request body json");

    // First request: tool_use is present, but no tool_result.
    let first_messages = first_body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut saw_tool_use = false;
    let mut saw_tool_result = false;
    for msg in &first_messages {
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        if block.get("id").and_then(|v| v.as_str()) == Some(call_id)
                            && block.get("name").and_then(|v| v.as_str()) == Some("update_plan")
                        {
                            saw_tool_use = true;
                        }
                    }
                    Some("tool_result") => {
                        saw_tool_result = true;
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(saw_tool_use, "expected tool_use in first request");
    assert!(
        !saw_tool_result,
        "first request should not contain tool_result blocks"
    );

    // Second request: should contain a tool_result referencing the same call_id.
    let second_messages = second_body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut tool_result_count = 0;
    for msg in &second_messages {
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && block.get("tool_use_id").and_then(|v| v.as_str()) == Some(call_id)
                {
                    tool_result_count += 1;
                }
            }
        }
    }

    assert_eq!(
        tool_result_count, 1,
        "expected exactly one tool_result referencing {call_id}"
    );

    Ok(())
}

/// Tests that rate limit headers from Anthropic API are emitted as RateLimits events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_rate_limit_headers_emit_event() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;

    let sse_body = anthropic_sse(vec![
        json!({"type":"message_start","message":{"id":"msg-1"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5}}),
        json!({"type":"message_stop","message":{"id":"msg-1"}}),
    ]);

    // Include Anthropic rate limit headers in response
    let template = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("anthropic-ratelimit-requests-limit", "1000")
        .insert_header("anthropic-ratelimit-requests-remaining", "750")
        .insert_header("anthropic-ratelimit-tokens-limit", "100000")
        .insert_header("anthropic-ratelimit-tokens-remaining", "80000")
        .set_body_raw(sse_body, "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "mock-anthropic".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::AnthropicMessages,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
    };

    let codex_home = TempDir::new().expect("tempdir");
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let conversation_id = ConversationId::new();
    let model_family = ModelsManager::construct_model_family_offline(model.as_str(), &config);
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        model.as_str(),
        model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(AuthMode::ApiKey),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        model_family,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    let mut prompt = Prompt::default();
    prompt.input.push(user_message("hello"));

    let mut stream = client.stream(&prompt).await.expect("stream anthropic");

    let mut saw_rate_limits = false;
    while let Some(event) = stream.next().await {
        if let Ok(codex_core::ResponseEvent::RateLimits(snapshot)) = event {
            // Verify the rate limit values were parsed correctly
            // Requests: 25% used (1000 - 750) / 1000
            if let Some(primary) = &snapshot.primary {
                assert!(
                    (primary.used_percent - 25.0).abs() < 0.1,
                    "expected ~25% requests used, got {}",
                    primary.used_percent
                );
            }
            // Tokens: 20% used (100000 - 80000) / 100000
            if let Some(secondary) = &snapshot.secondary {
                assert!(
                    (secondary.used_percent - 20.0).abs() < 0.1,
                    "expected ~20% tokens used, got {}",
                    secondary.used_percent
                );
            }
            saw_rate_limits = true;
        }
    }

    assert!(
        saw_rate_limits,
        "expected RateLimits event from response headers"
    );

    Ok(())
}

/// Tests that x-api-key header is included in Anthropic requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_auth_header_included() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;

    let sse_body = anthropic_sse(vec![
        json!({"type":"message_start","message":{"id":"msg-1"}}),
        json!({"type":"message_stop","message":{"id":"msg-1","usage":{
            "input_tokens":1,
            "output_tokens":1
        }}}),
    ]);

    let template = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_body, "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "mock-anthropic".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: Some("test-api-key-12345".to_string()),
        wire_api: WireApi::AnthropicMessages,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
    };

    let codex_home = TempDir::new().expect("tempdir");
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let conversation_id = ConversationId::new();
    let model_family = ModelsManager::construct_model_family_offline(model.as_str(), &config);
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        model.as_str(),
        model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(AuthMode::ApiKey),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        model_family,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    let mut prompt = Prompt::default();
    prompt.input.push(user_message("hello"));

    let mut stream = client.stream(&prompt).await.expect("stream anthropic");
    while let Some(event) = stream.next().await {
        event.expect("stream event");
    }

    let all_requests = server
        .received_requests()
        .await
        .expect("mock server should not fail");
    let anthropic_request = all_requests
        .iter()
        .find(|req| req.method == "POST" && req.url.path() == "/v1/messages")
        .expect("expected at least one Anthropic request");

    // Verify the Authorization header contains the bearer token
    let auth_header = anthropic_request
        .headers
        .get("authorization")
        .map(|v| v.to_str().unwrap_or(""));

    assert!(
        auth_header.is_some(),
        "expected Authorization header in request"
    );
    assert!(
        auth_header.unwrap().contains("test-api-key-12345"),
        "expected bearer token in Authorization header, got {:?}",
        auth_header
    );

    Ok(())
}

/// Tests that retry logic works for transient 5xx errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_retry_on_transient_error() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::Respond;

    let server = MockServer::start().await;

    let success_body = anthropic_sse(vec![
        json!({"type":"message_start","message":{"id":"msg-1"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Success!"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5}}),
        json!({"type":"message_stop","message":{"id":"msg-1"}}),
    ]);

    // Custom responder that fails on first request, succeeds on second
    struct RetryResponder {
        call_count: AtomicUsize,
        success_body: String,
    }

    impl Respond for RetryResponder {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                // First request: return 503 Service Unavailable
                ResponseTemplate::new(503)
                    .insert_header("content-type", "application/json")
                    .set_body_json(
                        json!({"error": {"type": "overloaded_error", "message": "Overloaded"}}),
                    )
            } else {
                // Subsequent requests: succeed
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(self.success_body.clone())
            }
        }
    }

    let responder = RetryResponder {
        call_count: AtomicUsize::new(0),
        success_body: success_body.clone(),
    };

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "mock-anthropic".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::AnthropicMessages,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(3), // Allow retries
        stream_max_retries: Some(3),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
    };

    let codex_home = TempDir::new().expect("tempdir");
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);

    let conversation_id = ConversationId::new();
    let model_family = ModelsManager::construct_model_family_offline(model.as_str(), &config);
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        model.as_str(),
        model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(AuthMode::ApiKey),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        model_family,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    let mut prompt = Prompt::default();
    prompt.input.push(user_message("hello"));

    let mut stream = client.stream(&prompt).await.expect("stream anthropic");

    let mut saw_success_text = false;
    while let Some(event) = stream.next().await {
        if let Ok(codex_core::ResponseEvent::OutputTextDelta(text)) = event {
            if text.contains("Success!") {
                saw_success_text = true;
            }
        }
    }

    assert!(saw_success_text, "expected successful response after retry");

    // Verify that multiple requests were made (retry happened)
    let all_requests = server
        .received_requests()
        .await
        .expect("mock server should not fail");
    let anthropic_requests: Vec<_> = all_requests
        .iter()
        .filter(|req| req.method == "POST" && req.url.path() == "/v1/messages")
        .collect();

    assert!(
        anthropic_requests.len() >= 2,
        "expected at least 2 requests (1 failed + 1 retry), got {}",
        anthropic_requests.len()
    );

    Ok(())
}
