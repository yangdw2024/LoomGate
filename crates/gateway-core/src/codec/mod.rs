pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_responses;

use crate::config::UpstreamProtocol;
use crate::ir::{CanonicalRequest, CanonicalResponse, CanonicalStreamEvent};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("invalid request payload: {0}")]
    InvalidPayload(String),
    #[error("json serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Lossless bidirectional codec between a wire protocol and the Canonical IR
/// covering requests, non-streaming responses, streaming SSE frames, and protocol-native errors.
pub trait ProtocolCodec: Send + Sync {
    fn protocol(&self) -> UpstreamProtocol;
    fn decode_request(
        &self,
        payload: &Value,
        session_id: Option<String>,
        task_stage: Option<String>,
    ) -> Result<CanonicalRequest, CodecError>;
    fn encode_request(
        &self,
        req: &CanonicalRequest,
        upstream_model: &str,
        same_protocol: bool,
    ) -> Result<Value, CodecError>;
    fn decode_response(&self, body: &Value) -> Result<CanonicalResponse, CodecError>;
    fn encode_response(&self, resp: &CanonicalResponse) -> Value;
    fn decode_sse_frame(&self, event_name: Option<&str>, data: &str) -> Vec<CanonicalStreamEvent>;
    fn encode_sse_event(&self, event: &CanonicalStreamEvent) -> Vec<String>;
    fn encode_error(&self, error_type: &str, message: &str) -> Value;
}

pub fn codec_for(protocol: UpstreamProtocol) -> Box<dyn ProtocolCodec> {
    match protocol {
        UpstreamProtocol::Anthropic => Box::new(anthropic::AnthropicCodec),
        UpstreamProtocol::OpenAi => Box::new(openai::OpenAiCodec),
        UpstreamProtocol::OpenAiResponses => Box::new(openai_responses::OpenAiResponsesCodec),
        UpstreamProtocol::Gemini => Box::new(gemini::GeminiCodec),
    }
}
