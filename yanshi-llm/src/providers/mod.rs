use crate::{
  LLMError, ResponseFormat, Result, StreamingResponse,
  thinking::ThinkingConfig,
  tool_calling::{StopReason, ToolCallRequest, ToolChoice, ToolSpec},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub mod anthropic;
pub mod dashscope_media;
pub mod glm_images;
pub mod google;
pub mod google_media;
pub mod google_veo;
pub mod kling_video;
mod line_buffer;
pub mod minimax_tts;
pub mod minimax_video;
pub mod mock;
pub mod mock_media;
pub mod modality;
pub mod moonshot;
pub mod openai;
pub mod openai_asr;
pub mod openai_images;
pub mod openai_tts;
pub mod stepfun;

pub use anthropic::AnthropicProvider;
pub use google::GoogleProvider;
pub use mock::MockProvider;
pub use mock_media::{MockMediaProvider, text2image_calls as mock_text2image_calls};
pub use moonshot::MoonshotProvider;
pub use openai::OpenAIProvider;
pub use stepfun::StepFunProvider;

pub(crate) fn build_http_client(
  builder: impl Fn(bool) -> reqwest::ClientBuilder,
) -> Result<reqwest::Client> {
  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder(false).build())) {
    Ok(result) => result.map_err(Into::into),
    Err(_) => builder(true).build().map_err(Into::into),
  }
}

/// Default connect timeout for LLM provider HTTP calls. Mirrors
/// `LLMError`'s `From<reqwest::Error>` synthetic value so the timeout
/// reported to callers actually matches what `reqwest` enforces
/// (Q1.8.2 — pre-fix the builder had no timeout at all, yet
/// `LLMError::from(reqwest::Error)` reported `timeout_ms: 30000`).
pub(crate) const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Default request-level timeout for LLM provider HTTP calls. Larger
/// than the connect timeout because LLM responses (especially with
/// generated content > 1 K tokens) routinely take 20–60 s on the
/// wire. The Anthropic / OpenAI clients themselves enforce this at
/// 600 s in their canonical SDKs.
pub(crate) const DEFAULT_HTTP_REQUEST_TIMEOUT_SECS: u64 = 600;

pub(crate) fn default_http_client() -> Result<reqwest::Client> {
  build_http_client(|disable_proxy| {
    let builder = reqwest::Client::builder()
      // Q1.8.2: pin timeouts. Without these the client waits
      // indefinitely on a dropped connection / hung-but-not-closed
      // socket. Aligned with the timeout `LLMError::from` already
      // reports.
      .connect_timeout(std::time::Duration::from_secs(
        DEFAULT_HTTP_CONNECT_TIMEOUT_SECS,
      ))
      .timeout(std::time::Duration::from_secs(
        DEFAULT_HTTP_REQUEST_TIMEOUT_SECS,
      ));
    if disable_proxy {
      builder.no_proxy()
    } else {
      builder
    }
  })
}

/// Request structure for LLM providers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
  pub model: String,
  pub messages: Vec<Value>,
  pub stream: bool,
  pub parameters: HashMap<String, Value>,
  /// Native tool / function-calling specification.
  ///
  /// When `Some`, providers that support native tool calling map this to their
  /// own wire format (OpenAI `tools`, Anthropic `tools` block, Google
  /// `function_declarations`). Providers without native support either pass it
  /// through (OpenAI-compatible) or ignore it and rely on prompt-based ReAct.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tools: Option<Vec<ToolSpec>>,
  /// Selection strategy for `tools`. Ignored when `tools` is `None`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_choice: Option<ToolChoice>,
  /// Extended-reasoning / "thinking" configuration.
  ///
  /// Travels as a typed field rather than via `parameters` because
  /// providers like Anthropic and Google whitelist keys in their
  /// `parameters` map and would otherwise silently drop a `thinking` key.
  /// Each provider adapter is responsible for serialising this into its
  /// native wire shape (Anthropic `thinking` block, OpenAI
  /// `reasoning_effort`, Google `thinkingConfig`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub thinking: Option<ThinkingConfig>,
  /// Structured-output constraint (V2.1).
  ///
  /// Travels as a typed field rather than via `parameters` for the same
  /// reason `thinking` does above: Anthropic and Google whitelist keys in
  /// their `parameters` map and would otherwise silently drop a
  /// `response_format` key with no error at all — the exact "I set X but
  /// didn't get X" bug class `thinking` was already promoted to fix.
  ///
  /// Native mapping per provider (each adapter owns its own encoding):
  ///  - **OpenAI / Moonshot / StepFun** (OpenAI-compatible): `response_format`
  ///    request field, unconditionally — these APIs support it alongside
  ///    `tools` in the same request.
  ///  - **Anthropic**: no native `response_format` field exists. When no
  ///    real `tools` are requested (and the request isn't streaming), a
  ///    `JsonSchema` format is emulated by forcing a single synthetic tool
  ///    call whose `input_schema` is the requested schema, then unwrapping
  ///    that tool call's arguments back into `ProviderResponse::content` as
  ///    plain JSON text — transparent to callers. When real `tools` are
  ///    also requested, forcing a tool choice would break the caller's own
  ///    tool-calling turn, so this is skipped and the request falls back to
  ///    prompt-only constraint.
  ///  - **Google**: `generationConfig.responseMimeType` +
  ///    `.responseSchema`, only when no real `tools` are requested (Gemini's
  ///    function-calling and structured-output modes are not verified to
  ///    compose reliably; same conservative skip-when-tools-present rule as
  ///    Anthropic).
  ///  - **Mock**: ignored (scripted responses don't consult the request).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_format: Option<ResponseFormat>,
}

impl ProviderRequest {
  /// Construct a minimal `ProviderRequest` with no tool calling configured.
  pub fn new(model: impl Into<String>, messages: Vec<Value>, stream: bool) -> Self {
    Self {
      model: model.into(),
      messages,
      stream,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    }
  }
}

/// Parse a `data:<mime>;base64,<payload>` URL into its `(mime_type, payload)`
/// parts. Returns `None` for anything else (e.g. a remote `http(s)` URL),
/// which callers treat as "not embedded data — reference it by URL instead".
///
/// Shared by every provider adapter that must distinguish a base64-embedded
/// image (translated into that provider's native inline/base64 content
/// block) from a remote URL reference (passed through as a URL reference) —
/// [`crate::multimodal::MultimodalMessage::to_openai_format`] represents both
/// as an `image_url` block, so this is the one place that tells them apart.
pub(crate) fn parse_data_url(url: &str) -> Option<(String, String)> {
  let rest = url.strip_prefix("data:")?;
  let (meta, data) = rest.split_once(',')?;
  let meta = meta.strip_suffix(";base64")?;
  let mime = if meta.is_empty() {
    "application/octet-stream".to_string()
  } else {
    meta.to_string()
  };
  Some((mime, data.to_string()))
}

/// Build an [`LLMError`] from a non-2xx chat-completion HTTP response,
/// honoring a server-supplied `Retry-After` header when present.
///
/// Only the delta-seconds form of `Retry-After` (RFC 7231 §7.1.3, e.g.
/// `Retry-After: 30`) is parsed — every LLM vendor observed sends this
/// form, not the HTTP-date alternative; an absent or unparseable header
/// falls back to `LLMError::HttpError`, same as before this existed, so
/// `retry_transient` computes its own exponential+jitter delay.
///
/// Used by the chat `execute`/`execute_streaming` methods that route
/// through `retry_transient` (OpenAI, Anthropic, Google, Moonshot,
/// StepFun, and — since they share `OpenAIProvider` — DashScope/GLM/
/// DeepSeek/MiniMax too). One-shot modality endpoints (TTS/ASR/image/
/// video) that never retry keep constructing `LLMError::HttpError`
/// directly — a `Retry-After` value is meaningless without a retry loop
/// to consume it.
pub(crate) fn chat_http_error(
  status_code: u16,
  message: String,
  headers: &reqwest::header::HeaderMap,
) -> LLMError {
  let is_retryable_status = status_code == 429 || (500..=599).contains(&status_code);
  if is_retryable_status
    && let Some(retry_after_ms) = headers
      .get("retry-after")
      .and_then(|v| v.to_str().ok())
      .and_then(|s| s.trim().parse::<u64>().ok())
      .map(|secs| secs.saturating_mul(1000))
  {
    return LLMError::RateLimitedWithRetryAfter {
      status_code,
      message,
      retry_after_ms,
    };
  }
  LLMError::HttpError {
    status_code,
    message,
  }
}

/// Concatenate the textual portion of an OpenAI-shaped `content` value
/// (a plain string, or an array of typed blocks). Used wherever a provider's
/// native system-message field is text-only (Google's `systemInstruction`,
/// Anthropic's `system`) so a multimodal system message's text survives
/// instead of being silently dropped just because `content` is an array
/// rather than a string.
pub(crate) fn openai_content_to_text(content: &Value) -> String {
  if let Some(text) = content.as_str() {
    return text.to_string();
  }
  let Some(items) = content.as_array() else {
    return String::new();
  };
  let mut out = String::new();
  for item in items {
    if let Some(obj) = item.as_object()
      && obj.get("type").and_then(Value::as_str) == Some("text")
      && let Some(text) = obj.get("text").and_then(Value::as_str)
    {
      if !out.is_empty() {
        out.push(' ');
      }
      out.push_str(text);
    }
  }
  out
}

/// Content types that can be returned by LLM providers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentType {
  /// Plain text content
  Text(String),
  /// Image content with binary data and media type
  Image { data: Vec<u8>, media_type: String },
  /// Audio content with binary data and media type
  Audio { data: Vec<u8>, media_type: String },
  /// Mixed content containing multiple blocks
  Mixed(Vec<ContentBlock>),
}

/// Individual content blocks for mixed content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
  /// Text block
  Text(String),
  /// Image block
  Image { data: Vec<u8>, media_type: String },
  /// Audio block
  Audio { data: Vec<u8>, media_type: String },
}

impl ContentType {
  /// Convert content to a plain text string (for backward compatibility)
  #[allow(clippy::inherent_to_string)]
  pub fn to_string(&self) -> String {
    match self {
      ContentType::Text(text) => text.clone(),
      ContentType::Image { .. } => "[Image]".to_string(),
      ContentType::Audio { .. } => "[Audio]".to_string(),
      ContentType::Mixed(blocks) => blocks
        .iter()
        .map(|block| match block {
          ContentBlock::Text(text) => text.clone(),
          ContentBlock::Image { .. } => "[Image]".to_string(),
          ContentBlock::Audio { .. } => "[Audio]".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" "),
    }
  }

  /// Y6.6 (M7): the text (without `[Image]` placeholders) and the
  /// non-text blocks, in order.
  pub fn split_text_and_media(&self) -> (String, Vec<ContentBlock>) {
    match self {
      ContentType::Text(text) => (text.clone(), Vec::new()),
      ContentType::Image { data, media_type } => (
        String::new(),
        vec![ContentBlock::Image {
          data: data.clone(),
          media_type: media_type.clone(),
        }],
      ),
      ContentType::Audio { data, media_type } => (
        String::new(),
        vec![ContentBlock::Audio {
          data: data.clone(),
          media_type: media_type.clone(),
        }],
      ),
      ContentType::Mixed(blocks) => {
        let mut text = String::new();
        let mut media = Vec::new();
        for block in blocks {
          match block {
            ContentBlock::Text(part) => text.push_str(part),
            other => media.push(other.clone()),
          }
        }
        (text, media)
      }
    }
  }

  /// Check if content is purely text
  pub fn is_text(&self) -> bool {
    matches!(self, ContentType::Text(_))
  }

  /// Check if content contains images
  pub fn has_images(&self) -> bool {
    match self {
      ContentType::Image { .. } => true,
      ContentType::Mixed(blocks) => blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. })),
      _ => false,
    }
  }

  /// Check if content contains audio
  pub fn has_audio(&self) -> bool {
    match self {
      ContentType::Audio { .. } => true,
      ContentType::Mixed(blocks) => blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Audio { .. })),
      _ => false,
    }
  }

  /// Get the length of the content (for text, the string length; for binary, the data length)
  pub fn len(&self) -> usize {
    match self {
      ContentType::Text(text) => text.len(),
      ContentType::Image { data, .. } => data.len(),
      ContentType::Audio { data, .. } => data.len(),
      ContentType::Mixed(blocks) => blocks
        .iter()
        .map(|block| match block {
          ContentBlock::Text(text) => text.len(),
          ContentBlock::Image { data, .. } => data.len(),
          ContentBlock::Audio { data, .. } => data.len(),
        })
        .sum(),
    }
  }

  /// Check whether the content has zero bytes or characters.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl From<String> for ContentType {
  fn from(text: String) -> Self {
    ContentType::Text(text)
  }
}

impl From<&str> for ContentType {
  fn from(text: &str) -> Self {
    ContentType::Text(text.to_string())
  }
}

/// Response structure from LLM providers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
  pub content: ContentType,
  pub usage: Option<TokenUsage>,
  pub metadata: Option<Value>,
  /// Tool calls requested by the model in this response.
  ///
  /// Empty unless the provider parsed native tool calls from the response.
  /// Consumers should prefer this over re-parsing `content` whenever
  /// non-empty.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tool_calls: Vec<ToolCallRequest>,
  /// Reason the model stopped generating, normalised across providers.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stop_reason: Option<StopReason>,
  /// Model-emitted reasoning / "thinking" text, when the provider returns
  /// it. Anthropic emits `type: thinking` content blocks; DeepSeek-R1
  /// emits `reasoning_content` alongside `content`; OpenAI o-series may
  /// emit reasoning summaries on the new `output[*].type == "reasoning"`
  /// shape. Stays `None` when the model didn't return reasoning text or
  /// when thinking wasn't requested.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub thinking: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
  pub prompt_tokens: Option<u32>,
  pub completion_tokens: Option<u32>,
  pub total_tokens: Option<u32>,
}

/// Trait that all LLM providers must implement
#[async_trait]
pub trait LLMProvider: Send + Sync {
  /// Get the provider name (e.g., "openai", "anthropic", "google")
  fn name(&self) -> &str;

  /// Execute a non-streaming request
  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse>;

  /// Execute a streaming request
  async fn execute_streaming(
    &self,
    request: &ProviderRequest,
  ) -> Result<Box<dyn StreamingResponse>>;

  /// Validate that the provider is properly configured
  async fn validate_config(&self) -> Result<()>;

  /// Get the base URL for this provider
  fn base_url(&self) -> &str;

  /// Get supported model names for this provider
  fn supported_models(&self) -> Vec<String>;
}

/// Factory function to create providers by name
pub fn create_provider(
  provider_name: &str,
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn LLMProvider>> {
  match provider_name.to_lowercase().as_str() {
    "mock" => Ok(Box::new(MockProvider::new(api_key, base_url)?)), // Mock provider for testing
    "openai" => Ok(Box::new(OpenAIProvider::new(api_key, base_url)?)),
    "anthropic" => Ok(Box::new(AnthropicProvider::new(api_key, base_url)?)),
    "google" | "gemini" => Ok(Box::new(GoogleProvider::new(api_key, base_url)?)),
    "moonshot" => Ok(Box::new(MoonshotProvider::new(api_key, base_url)?)),
    "stepfun" | "step" => Ok(Box::new(StepFunProvider::new(api_key, base_url)?)), // Use dedicated StepFun provider
    "dashscope" => Ok(Box::new(OpenAIProvider::new(api_key, base_url)?)), // Dashscope is OpenAI-compatible
    "glm" | "bigmodel" | "zhipu" => Ok(Box::new(OpenAIProvider::new(api_key, base_url)?)), // BigModel GLM is OpenAI-compatible
    "deepseek" => Ok(Box::new(OpenAIProvider::new(api_key, base_url)?)), // DeepSeek is OpenAI-compatible
    "minimax" => Ok(Box::new(OpenAIProvider::new(api_key, base_url)?)), // MiniMax is OpenAI-compatible (host: api.minimaxi.com)
    _ => Err(LLMError::UnsupportedProvider {
      provider: provider_name.to_string(),
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use reqwest::header::HeaderMap;

  fn headers_with_retry_after(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", value.parse().unwrap());
    headers
  }

  /// P-LLM2.7: a 429 with a valid delta-seconds `Retry-After` must
  /// produce `RateLimitedWithRetryAfter`, converted to milliseconds.
  #[test]
  fn chat_http_error_honors_retry_after_on_429() {
    let headers = headers_with_retry_after("30");
    let err = chat_http_error(429, "rate limited".to_string(), &headers);
    match err {
      LLMError::RateLimitedWithRetryAfter {
        status_code,
        retry_after_ms,
        ..
      } => {
        assert_eq!(status_code, 429);
        assert_eq!(retry_after_ms, 30_000);
      }
      other => panic!("expected RateLimitedWithRetryAfter, got {other:?}"),
    }
  }

  /// Same, for a 503 — any retryable 5xx should honor the header, not
  /// just 429.
  #[test]
  fn chat_http_error_honors_retry_after_on_503() {
    let headers = headers_with_retry_after("5");
    let err = chat_http_error(503, "unavailable".to_string(), &headers);
    assert!(matches!(
      err,
      LLMError::RateLimitedWithRetryAfter {
        retry_after_ms: 5_000,
        ..
      }
    ));
  }

  /// A non-retryable status (e.g. 400) must stay a plain `HttpError` even
  /// if a `Retry-After` header is somehow present — retrying a caller
  /// mistake makes no sense regardless of what the header says.
  #[test]
  fn chat_http_error_ignores_retry_after_on_non_retryable_status() {
    let headers = headers_with_retry_after("30");
    let err = chat_http_error(400, "bad request".to_string(), &headers);
    assert!(matches!(
      err,
      LLMError::HttpError {
        status_code: 400,
        ..
      }
    ));
  }

  /// No `Retry-After` header at all → plain `HttpError`, so
  /// `retry_transient` falls back to its own computed exponential+jitter
  /// delay, same as before this existed.
  #[test]
  fn chat_http_error_falls_back_to_http_error_when_header_absent() {
    let err = chat_http_error(429, "rate limited".to_string(), &HeaderMap::new());
    assert!(matches!(
      err,
      LLMError::HttpError {
        status_code: 429,
        ..
      }
    ));
  }

  /// An unparseable `Retry-After` value (e.g. the HTTP-date form this
  /// function deliberately doesn't parse) must not panic — fall back to
  /// `HttpError`, same as an absent header.
  #[test]
  fn chat_http_error_falls_back_when_retry_after_unparseable() {
    let headers = headers_with_retry_after("Fri, 31 Dec 1999 23:59:59 GMT");
    let err = chat_http_error(429, "rate limited".to_string(), &headers);
    assert!(matches!(
      err,
      LLMError::HttpError {
        status_code: 429,
        ..
      }
    ));
  }
}
