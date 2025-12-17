use crate::auth::AuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::streaming::StreamingClient;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::provider::WireApi;
use crate::sse::anthropic::spawn_anthropic_stream;
use crate::telemetry::SseTelemetry;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

pub struct AnthropicMessagesClient<T: HttpTransport, A: AuthProvider> {
    streaming: StreamingClient<T, A>,
}

impl<T: HttpTransport, A: AuthProvider> AnthropicMessagesClient<T, A> {
    pub fn new(transport: T, provider: Provider, auth: A) -> Result<Self, ApiError> {
        if provider.wire != WireApi::AnthropicMessages {
            return Err(ApiError::Config(
                "AnthropicMessagesClient requires a provider with AnthropicMessages wire api"
                    .to_string(),
            ));
        }
        Ok(Self {
            streaming: StreamingClient::new(transport, provider, auth),
        })
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            streaming: self.streaming.with_telemetry(request, sse),
        }
    }

    fn path(&self) -> &'static str {
        "messages"
    }

    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        self.streaming
            .stream(self.path(), body, extra_headers, spawn_anthropic_stream)
            .await
    }

    pub async fn stream_body(&self, body: Value) -> Result<ResponseStream, ApiError> {
        self.stream(body, HeaderMap::new()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use async_trait::async_trait;
    use codex_client::Request;
    use codex_client::Response;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct DummyTransport;

    #[async_trait]
    impl HttpTransport for DummyTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            Err(TransportError::Build("execute should not run".to_string()))
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    #[derive(Clone, Default)]
    struct DummyAuth;

    impl AuthProvider for DummyAuth {
        fn bearer_token(&self) -> Option<String> {
            None
        }
    }

    fn provider(wire: WireApi) -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: "https://example.com/v1".to_string(),
            query_params: None,
            wire,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(1),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn anthropic_client_new_with_correct_wire_api() {
        let transport = DummyTransport;
        let auth = DummyAuth;
        let prov = provider(WireApi::AnthropicMessages);

        let result = AnthropicMessagesClient::new(transport, prov, auth);
        assert!(result.is_ok(), "expected Ok for AnthropicMessages wire api");
    }

    #[test]
    fn anthropic_client_new_with_wrong_wire_api_responses() {
        let transport = DummyTransport;
        let auth = DummyAuth;
        let prov = provider(WireApi::Responses);

        let result = AnthropicMessagesClient::new(transport, prov, auth);
        assert!(
            matches!(result, Err(ApiError::Config(msg)) if msg.contains("AnthropicMessages wire api")),
            "expected Config error for Responses wire api"
        );
    }

    #[test]
    fn anthropic_client_new_with_wrong_wire_api_chat() {
        let transport = DummyTransport;
        let auth = DummyAuth;
        let prov = provider(WireApi::Chat);

        let result = AnthropicMessagesClient::new(transport, prov, auth);
        assert!(
            matches!(result, Err(ApiError::Config(msg)) if msg.contains("AnthropicMessages wire api")),
            "expected Config error for Chat wire api"
        );
    }
}
