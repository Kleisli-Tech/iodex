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
