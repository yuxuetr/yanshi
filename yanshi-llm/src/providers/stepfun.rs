use super::line_buffer::{LineBuffer, is_sse_done};
use crate::{
  LLMError, Result,
  client::streaming::{StreamChunk, StreamingResponse, TokenUsage},
  providers::{
    ContentType, LLMProvider, ProviderRequest, ProviderResponse,
    openai::{
      OpenAIStreamingToolCall, openai_streaming_tool_call_deltas, parse_openai_tool_calls,
      response_format_to_openai_value, tool_choice_to_openai_value, tool_spec_to_openai_value,
    },
  },
  thinking::ThinkingConfig,
  tool_calling::StopReason,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::pin::Pin;
use tokio_stream::Stream;

/// Model types supported by StepFun
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
#[allow(dead_code)] // Some variants like VoiceClone are defined but not yet used in routing logic
enum ModelType {
  Text,
  ImageUnderstand,
  Multimodal,
  TTS,        // Text-to-Speech: input text, output audio
  ASR,        // Automatic Speech Recognition: input audio, output text
  VoiceClone, // Voice Cloning: input audio sample, output voice profile
  GenerateImage,
  EditImage,
}

/// StepFun provider implementation
///
/// Handles chat-compatible models (Text, ImageUnderstand, Multimodal) via /chat/completions endpoint.
/// For specialized APIs (TTS, ASR, Image Generation, etc.), use StepFunSpecializedClient instead.
///
/// Supported endpoints:
/// - Text Models: /chat/completions (streaming + non-streaming)
/// - Image Understanding: /chat/completions with multimodal content
/// - Multimodal: /chat/completions with enhanced capabilities
pub struct StepFunProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl StepFunProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`]. See
  /// [`crate::providers::OpenAIProvider::with_client`] for the rationale.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "stepfun".to_string(),
      });
    }

    let base_url = base_url.unwrap_or_else(|| "https://api.stepfun.com/v1".to_string());

    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_headers(&self) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Q2.5.3: invalid API key → ConfigurationError, not panic.
    headers.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|err| {
        LLMError::ConfigurationError {
          message: format!("StepFun API key contains invalid characters: {err}"),
        }
      })?,
    );
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  fn build_request_body(&self, request: &ProviderRequest) -> Result<Value> {
    let mut body = json!({
      "model": request.model,
      "messages": request.messages,
      "stream": request.stream
    });

    // Add additional parameters
    for (key, value) in &request.parameters {
      body[key] = value.clone();
    }

    // StepFun is OpenAI-compatible — pass tools / tool_choice / response_format
    // through unchanged.
    if let Some(tools) = &request.tools {
      body["tools"] = Value::Array(tools.iter().map(tool_spec_to_openai_value).collect());
    }
    if let Some(choice) = &request.tool_choice {
      body["tool_choice"] = tool_choice_to_openai_value(choice);
    }
    if let Some(format) = &request.response_format {
      body["response_format"] = response_format_to_openai_value(format);
    }
    if let Some(thinking) = &request.thinking {
      if thinking.is_disabled() {
        // StepFun documents no way to turn reasoning off.
        return Err(LLMError::ConfigurationError {
          message: format!(
            "StepFun model '{}' always reasons; use ThinkingConfig::Low for the lightest effort",
            request.model
          ),
        });
      }
      if let Some(effort) = stepfun_reasoning_effort(&request.model, thinking) {
        body["reasoning_effort"] = Value::String(effort.to_string());
      }
    }

    Ok(body)
  }

  fn get_model_type(&self, model: &str) -> ModelType {
    match model {
      // Text models. The whole step-1-*/step-2-* lineage was retired from
      // StepFun's `/v1/models` (confirmed 404 `model_invalid`); step-3.5-flash
      // and step-3.5-flash-2603 are the current text-reasoning models.
      "step-3.5-flash" | "step-3.5-flash-2603" => ModelType::Text,

      // Image understanding models. step-1o-vision-32k / step-1v-8k /
      // step-1v-32k are no longer listed by the live API — step-1o-turbo-vision
      // is the only current vision-understanding entry point.
      "step-1o-turbo-vision" => ModelType::ImageUnderstand,

      // Multimodal models. step-3 (bare) was retired; step-3.7-flash and
      // step-5-preview are the current text + image + video reasoning models.
      "step-3.7-flash" | "step-5-preview" => ModelType::Multimodal,

      // Text-to-Speech models
      "step-tts-vivid" | "step-tts-mini" => ModelType::TTS,

      // Automatic Speech Recognition models
      "step-asr" => ModelType::ASR,

      // StepFun retired its image models (step-2x-large,
      // step-image-edit-2) and stopped its image APIs on 2026-10-10.

      // Default to text for unknown models
      _ => ModelType::Text,
    }
  }

  async fn execute_chat_completion(&self, url: String, body: Value) -> Result<ProviderResponse> {
    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let headers = response.headers().clone();
      let error_text = response.text().await.unwrap_or_default();
      return Err(super::chat_http_error(status_code, error_text, &headers));
    }

    let stepfun_response: StepFunResponse = response.json().await?;

    // Handle both string and array content formats (StepFun supports multimodal)
    let content_text = if let Some(first_choice) = stepfun_response.choices.first() {
      match &first_choice.message.content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(_)) => {
          // For multimodal responses that return structured content,
          // extract text parts or convert to string representation
          first_choice
            .message
            .content
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default()
        }
        _ => String::new(),
      }
    } else {
      String::new()
    };

    // Convert to ContentType
    let content = ContentType::Text(content_text);

    let usage = stepfun_response
      .usage
      .clone()
      .map(|u| crate::providers::TokenUsage {
        prompt_tokens: Some(u.prompt_tokens),
        completion_tokens: Some(u.completion_tokens),
        total_tokens: Some(u.total_tokens),
      });

    let first_choice = stepfun_response.choices.first();
    // StepFun's tool_calls field is `Vec<Value>`; wrap to a JSON array so we
    // can re-use the OpenAI parser unchanged.
    let tool_calls = first_choice
      .and_then(|c| c.message.tool_calls.as_ref())
      .map(|calls| parse_openai_tool_calls(&Value::Array(calls.clone())))
      .unwrap_or_default();
    // StepFun returns the reasoning text as `reasoning`, and also as the
    // DeepSeek-compatible `reasoning_content`.
    let thinking = first_choice.and_then(|c| {
      c.message
        .reasoning
        .clone()
        .or_else(|| c.message.reasoning_content.clone())
    });
    let stop_reason = if tool_calls.is_empty() {
      first_choice
        .and_then(|c| c.finish_reason.as_deref())
        .map(StopReason::from_openai_finish_reason)
    } else {
      Some(StopReason::ToolCalls)
    };

    Ok(ProviderResponse {
      content,
      usage,
      metadata: Some(serde_json::to_value(&stepfun_response)?),
      tool_calls,
      stop_reason,
      thinking,
    })
  }

  async fn execute_streaming_chat(
    &self,
    url: String,
    body: Value,
  ) -> Result<Box<dyn StreamingResponse>> {
    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let headers = response.headers().clone();
      let error_text = response.text().await.unwrap_or_default();
      return Err(super::chat_http_error(status_code, error_text, &headers));
    }

    Ok(Box::new(StepFunStreamingResponse::new(response)))
  }
}

#[async_trait]
impl LLMProvider for StepFunProvider {
  fn name(&self) -> &str {
    "stepfun"
  }

  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse> {
    if request.stream {
      return Err(LLMError::InternalError {
        message: "Use execute_streaming for streaming requests".to_string(),
      });
    }

    // Route to appropriate API based on model type
    match self.get_model_type(&request.model) {
      ModelType::Text | ModelType::ImageUnderstand | ModelType::Multimodal => {
        // Use chat completions for text, image understanding, and multimodal models
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request_body(request)?;
        self.execute_chat_completion(url, body).await
      }
      ModelType::TTS
      | ModelType::ASR
      | ModelType::VoiceClone
      | ModelType::GenerateImage
      | ModelType::EditImage => {
        // These model types require specialized APIs that are not suitable for streaming chat interface
        return Err(LLMError::InternalError {
          message: format!(
            "Model '{}' requires specialized API. Use StepFunSpecializedClient instead.",
            request.model
          ),
        });
      }
    }
  }

  async fn execute_streaming(
    &self,
    request: &ProviderRequest,
  ) -> Result<Box<dyn StreamingResponse>> {
    if !request.stream {
      return Err(LLMError::InternalError {
        message: "Streaming not enabled in request".to_string(),
      });
    }

    // Check if model supports streaming
    match self.get_model_type(&request.model) {
      ModelType::Text | ModelType::ImageUnderstand | ModelType::Multimodal => {
        // These models support streaming via chat completions
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request_body(request)?;
        self.execute_streaming_chat(url, body).await
      }
      ModelType::TTS
      | ModelType::ASR
      | ModelType::VoiceClone
      | ModelType::GenerateImage
      | ModelType::EditImage => {
        // These model types don't support streaming
        return Err(LLMError::InternalError {
          message: format!("Model '{}' does not support streaming", request.model),
        });
      }
    }
  }

  async fn validate_config(&self) -> Result<()> {
    // Simple health check - try to make a minimal request
    let url = format!("{}/chat/completions", self.base_url);
    let test_body = json!({
      "model": "step-3.5-flash",
      "messages": [{"role": "user", "content": "test"}],
      "max_tokens": 1
    });

    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&test_body)
      .send()
      .await?;

    if !response.status().is_success() {
      return Err(LLMError::AuthenticationError {
        provider: "stepfun".to_string(),
        message: "Failed to authenticate with StepFun API".to_string(),
      });
    }

    Ok(())
  }

  fn base_url(&self) -> &str {
    &self.base_url
  }

  fn supported_models(&self) -> Vec<String> {
    vec![
      // Text models
      "step-3.5-flash".to_string(),
      "step-3.5-flash-2603".to_string(),
      // Image understanding models
      "step-1o-turbo-vision".to_string(),
      // Multimodal models
      "step-3.7-flash".to_string(),
      "step-5-preview".to_string(),
      // Audio models (for chat completions interface)
      "step-tts-vivid".to_string(),
      "step-tts-mini".to_string(),
      "step-asr".to_string(),
    ]
  }
}

// StepFun API response structures (similar to OpenAI but with StepFun specifics)
/// StepFun's `reasoning_effort` for `thinking`. Most reasoning models take
/// `low` / `medium` / `high`; `step-3.5-flash-2603` takes only `low` and
/// `high`, so `medium` rounds up there. `Auto` sends nothing, leaving the
/// model's default.
fn stepfun_reasoning_effort(model: &str, thinking: &ThinkingConfig) -> Option<&'static str> {
  if matches!(thinking, ThinkingConfig::Auto) {
    return None;
  }
  let effort = match thinking.to_openai_effort()? {
    "minimal" | "low" => "low",
    "high" => "high",
    _ => "medium",
  };
  if model == "step-3.5-flash-2603" && effort == "medium" {
    return Some("high");
  }
  Some(effort)
}

#[derive(Debug, Deserialize, Serialize)]
struct StepFunResponse {
  id: String,
  object: String,
  created: u64,
  model: String,
  choices: Vec<StepFunChoice>,
  usage: Option<StepFunUsage>,
  #[serde(skip_serializing_if = "Option::is_none")]
  service_tier: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  system_fingerprint: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StepFunChoice {
  index: u32,
  message: StepFunMessage,
  finish_reason: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  logprobs: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StepFunMessage {
  role: String,
  content: Option<serde_json::Value>, // Can be string or array for multimodal
  #[serde(skip_serializing_if = "Option::is_none")]
  refusal: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  function_call: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  tool_calls: Option<Vec<Value>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  reasoning: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StepFunUsage {
  prompt_tokens: u32,
  completion_tokens: u32,
  total_tokens: u32,
  #[serde(skip_serializing_if = "Option::is_none")]
  completion_tokens_details: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  cached_tokens: Option<u32>,
}

// Streaming response structures
#[derive(Debug, Deserialize, Serialize)]
struct StepFunStreamingChunk {
  id: String,
  object: String,
  created: u64,
  model: String,
  choices: Vec<StepFunStreamingChoice>,
  usage: Option<StepFunUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StepFunStreamingChoice {
  index: u32,
  delta: StepFunStreamingDelta,
  finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StepFunStreamingDelta {
  role: Option<String>,
  content: Option<serde_json::Value>, // Can be string or array for multimodal
  /// P-LLM2.5: StepFun speaks the identical OpenAI-compatible
  /// `delta.tool_calls[]` streaming shape — see
  /// `openai::OpenAIStreamingToolCall`'s doc comment.
  #[serde(default)]
  tool_calls: Option<Vec<OpenAIStreamingToolCall>>,
}

pub struct StepFunStreamingResponse {
  stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
  buffer: LineBuffer,
  finished: bool,
}

// Q2.5.4: `unsafe impl Send + Sync` removed (trait no longer needs Sync).

impl StepFunStreamingResponse {
  fn new(response: reqwest::Response) -> Self {
    let byte_stream = response.bytes_stream();
    let string_stream = byte_stream.map(|chunk_result| {
      chunk_result
        .map_err(|e| LLMError::StreamingError {
          message: e.to_string(),
        })
        .map(|chunk| chunk.to_vec())
    });

    Self {
      stream: Box::pin(string_stream),
      buffer: LineBuffer::default(),
      finished: false,
    }
  }

  fn parse_sse_chunk(line: &str) -> Option<StreamChunk> {
    if !line.starts_with("data: ") {
      return None;
    }

    let data = &line[6..]; // Remove "data: " prefix

    if data.trim() == "[DONE]" {
      return Some(StreamChunk {
        media: Vec::new(),
        content: String::new(),
        is_final: true,
        metadata: None,
        usage: None,
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      });
    }

    if let Ok(chunk) = serde_json::from_str::<StepFunStreamingChunk>(data)
      && let Some(choice) = chunk.choices.first()
    {
      // Handle both string and array content in streaming
      let content_text = match &choice.delta.content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(), // Convert other types to string
        None => String::new(),
      };
      let tool_call_deltas = openai_streaming_tool_call_deltas(choice.delta.tool_calls.as_deref());

      // P-LLM2.5: previously this whole branch required `delta.content` to
      // be present, so a tool-call-only delta (no text) never even reached
      // the (also previously hardcoded-empty) `tool_call_deltas` — it fell
      // all the way through to `None` and was silently dropped. Emit a
      // chunk when there is *any* signal, mirroring the same fix in
      // `openai.rs`.
      let has_signal = !content_text.is_empty()
        || !tool_call_deltas.is_empty()
        || choice.finish_reason.is_some()
        || chunk.usage.is_some();
      if !has_signal {
        return None;
      }

      return Some(StreamChunk {
        media: Vec::new(),
        content: content_text,
        is_final: choice.finish_reason.is_some(),
        metadata: Some(serde_json::to_value(&chunk).ok()?),
        usage: chunk.usage.map(|u| TokenUsage {
          prompt_tokens: Some(u.prompt_tokens),
          completion_tokens: Some(u.completion_tokens),
          total_tokens: Some(u.total_tokens),
        }),
        content_type: Some("text".to_string()),
        tool_call_deltas,
      });
    }

    None
  }
}

#[async_trait]
impl StreamingResponse for StepFunStreamingResponse {
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
    if self.finished {
      return Ok(None);
    }

    loop {
      // Drain any complete lines already sitting in the buffer *before*
      // pulling more bytes off the network. A single network read can
      // contain multiple SSE `data:` lines — e.g. a burst of reasoning-
      // delta tokens from a thinking model, which stream densely with no
      // natural pacing between them. Without this, whatever's left in
      // the buffer after returning the first parsed line is silently
      // dropped the moment the underlying stream ends (or stalls) before
      // another network read arrives, because the old loop only ever
      // inspected the buffer immediately after a fresh `stream.next()`.
      {
        while let Some(line) = self.buffer.next_line() {
          if is_sse_done(&line) {
            self.finished = true;
          }
          if !line.is_empty()
            && let Some(chunk) = Self::parse_sse_chunk(&line)
          {
            return Ok(Some(chunk));
          }
        }
      }

      // Buffer has no complete parseable line left — pull more from the
      // network.
      match self.stream.next().await {
        Some(Ok(data)) => {
          self.buffer.push(&data);
        }
        Some(Err(e)) => return Err(e),
        None => {
          self.finished = true;
          return Ok(None);
        }
      }
    }
  }
}

// =============================================================================
// STEPFUN SPECIALIZED APIS
// =============================================================================
//
// This section provides specialized support for StepFun's audio synthesis,
// speech recognition and voice processing APIs beyond standard chat completions.

/// Text-to-speech parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TTSRequest {
  pub model: String, // "step-tts-mini" or "step-tts-vivid"
  pub input: String, // Max 1000 characters
  pub voice: String, // Voice ID
  #[serde(skip_serializing_if = "Option::is_none")]
  pub response_format: Option<String>, // "wav", "mp3", "flac", "opus"
  #[serde(skip_serializing_if = "Option::is_none")]
  pub speed: Option<f32>, // 0.5-2.0
  #[serde(skip_serializing_if = "Option::is_none")]
  pub volume: Option<f32>, // 0.1-2.0
  #[serde(skip_serializing_if = "Option::is_none")]
  pub voice_label: Option<VoiceLabel>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub sample_rate: Option<u32>, // 8000, 16000, 22050, 24000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceLabel {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub language: Option<String>, // 粤语, 四川话, 日语
  #[serde(skip_serializing_if = "Option::is_none")]
  pub emotion: Option<String>, // 高兴, 非常高兴, 生气, etc.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub style: Option<String>, // 慢速, 极慢, 快速, 极快
}

/// Voice cloning parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceCloningRequest {
  pub model: String,
  pub text: String,
  pub file_id: String, // From file upload
  #[serde(skip_serializing_if = "Option::is_none")]
  pub sample_text: Option<String>, // Max 300 characters
}

/// ASR (Automatic Speech Recognition) parameters
#[derive(Debug, Clone)]
pub struct ASRRequest {
  pub model: String,           // "step-asr"
  pub response_format: String, // "json", "text", "srt", "vtt"
  pub audio_data: Vec<u8>,
  pub filename: String,
}

/// Voice cloning response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceCloningResponse {
  pub id: String,     // Voice ID for future use
  pub object: String, // "audio.voice"
  #[serde(skip_serializing_if = "Option::is_none")]
  pub duplicated: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub sample_text: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub sample_audio: Option<String>, // Base64 encoded wav
}

/// Voice list response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceListResponse {
  pub object: String, // "list"
  pub data: Vec<VoiceInfo>,
  pub has_more: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub first_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub last_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceInfo {
  pub id: String,
  pub file_id: String,
  pub created_at: u64,
}

/// StepFun specialized API client
pub struct StepFunSpecializedClient {
  client: Client,
  api_key: String,
  base_url: String,
}

impl StepFunSpecializedClient {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`].
  ///
  /// This mirrors the chat providers and lets tests or embedded callers disable
  /// system proxy discovery in constrained environments.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "stepfun".to_string(),
      });
    }

    let base_url = base_url.unwrap_or_else(|| "https://api.stepfun.com/v1".to_string());

    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_auth_headers(&self) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    // Q2.5.3: invalid API key → ConfigurationError, not panic.
    headers.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|err| {
        LLMError::ConfigurationError {
          message: format!("StepFun API key contains invalid characters: {err}"),
        }
      })?,
    );
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  /// Convert text to speech
  pub async fn text_to_speech(&self, request: TTSRequest) -> Result<Vec<u8>> {
    use reqwest::header::{CONTENT_TYPE, HeaderValue};

    let url = format!("{}/audio/speech", self.base_url);

    let mut headers = self.build_auth_headers()?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let response = self
      .client
      .post(&url)
      .headers(headers)
      .json(&request)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    let audio_data = response.bytes().await?;
    Ok(audio_data.to_vec())
  }

  /// Create voice clone from audio sample
  pub async fn clone_voice(&self, request: VoiceCloningRequest) -> Result<VoiceCloningResponse> {
    use reqwest::header::{CONTENT_TYPE, HeaderValue};

    let url = format!("{}/audio/voices", self.base_url);

    let mut headers = self.build_auth_headers()?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let response = self
      .client
      .post(&url)
      .headers(headers)
      .json(&request)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    let result: VoiceCloningResponse = response.json().await?;
    Ok(result)
  }

  /// List available voices
  pub async fn list_voices(
    &self,
    limit: Option<u32>,
    order: Option<String>,
    before: Option<String>,
    after: Option<String>,
  ) -> Result<VoiceListResponse> {
    let mut url = format!("{}/audio/voices", self.base_url);
    let mut params = Vec::new();

    if let Some(limit) = limit {
      params.push(format!("limit={}", limit));
    }
    if let Some(order) = order {
      params.push(format!("order={}", order));
    }
    if let Some(before) = before {
      params.push(format!("before={}", before));
    }
    if let Some(after) = after {
      params.push(format!("after={}", after));
    }

    if !params.is_empty() {
      url.push('?');
      url.push_str(&params.join("&"));
    }

    let response = self
      .client
      .get(&url)
      .headers(self.build_auth_headers()?)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    let result: VoiceListResponse = response.json().await?;
    Ok(result)
  }

  /// Transcribe audio to text
  pub async fn speech_to_text(&self, request: ASRRequest) -> Result<String> {
    let url = format!("{}/audio/transcriptions", self.base_url);

    let form = reqwest::multipart::Form::new()
      .text("model", request.model.clone())
      .text("response_format", request.response_format.clone())
      .part(
        "file",
        reqwest::multipart::Part::bytes(request.audio_data)
          .file_name(request.filename)
          .mime_str("audio/mpeg")?,
      );

    let response = self
      .client
      .post(&url)
      .headers(self.build_auth_headers()?)
      .multipart(form)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    // Handle different response formats
    match request.response_format.as_str() {
      "json" => {
        #[derive(Deserialize)]
        struct JsonResponse {
          text: String,
        }
        let json_result: JsonResponse = response.json().await?;
        Ok(json_result.text)
      }
      "text" | "srt" | "vtt" => {
        let text_result = response.text().await?;
        Ok(text_result)
      }
      _ => {
        let text_result = response.text().await?;
        Ok(text_result)
      }
    }
  }
}

/// Builder for TTS requests
pub struct TTSBuilder {
  request: TTSRequest,
}

impl TTSBuilder {
  pub fn new(model: &str, input: &str, voice: &str) -> Self {
    Self {
      request: TTSRequest {
        model: model.to_string(),
        input: input.to_string(),
        voice: voice.to_string(),
        response_format: None,
        speed: None,
        volume: None,
        voice_label: None,
        sample_rate: None,
      },
    }
  }

  pub fn response_format(mut self, format: &str) -> Self {
    self.request.response_format = Some(format.to_string());
    self
  }

  pub fn speed(mut self, speed: f32) -> Self {
    self.request.speed = Some(speed);
    self
  }

  pub fn volume(mut self, volume: f32) -> Self {
    self.request.volume = Some(volume);
    self
  }

  pub fn language(mut self, language: &str) -> Self {
    self
      .request
      .voice_label
      .get_or_insert_with(VoiceLabel::default)
      .language = Some(language.to_string());
    self
  }

  pub fn emotion(mut self, emotion: &str) -> Self {
    self
      .request
      .voice_label
      .get_or_insert_with(VoiceLabel::default)
      .emotion = Some(emotion.to_string());
    self
  }

  pub fn style(mut self, style: &str) -> Self {
    self
      .request
      .voice_label
      .get_or_insert_with(VoiceLabel::default)
      .style = Some(style.to_string());
    self
  }

  pub fn sample_rate(mut self, rate: u32) -> Self {
    self.request.sample_rate = Some(rate);
    self
  }

  pub fn build(self) -> TTSRequest {
    self.request
  }
}

// ============================================================================
// P-LLM.1 modality trait adapters
// ----------------------------------------------------------------------------
// Wires `StepFunSpecializedClient` into the per-modality trait surface in
// `crate::providers::modality::*`. Each impl translates the modality-level
// request type into the StepFun-internal one, calls the existing client
// method, and translates the response back. No new wire behaviour — these
// are pure shape adapters.
// ============================================================================

use crate::providers::modality::{
  AsrProvider, AsrRequest, AsrResponse, TtsProvider, TtsRequest, TtsResponse,
};

fn tts_mime_type_for(response_format: Option<&str>) -> &'static str {
  match response_format {
    Some("mp3") => "audio/mpeg",
    Some("flac") => "audio/flac",
    Some("opus") => "audio/opus",
    Some("wav") | None | Some(_) => "audio/wav",
  }
}

#[async_trait]
impl AsrProvider for StepFunSpecializedClient {
  fn name(&self) -> &str {
    "stepfun"
  }

  async fn transcribe(&self, request: AsrRequest) -> Result<AsrResponse> {
    // StepFun's ASR endpoint ignores `language` / `temperature` today.
    // We accept them at the trait surface so the contract works for
    // Whisper (P-LLM.5) without breaking the call site.
    let text = self
      .speech_to_text(ASRRequest {
        model: request.model,
        response_format: request.response_format,
        audio_data: request.audio_data,
        filename: request.filename,
      })
      .await?;
    Ok(AsrResponse {
      text,
      metadata: None,
    })
  }
}

#[async_trait]
impl TtsProvider for StepFunSpecializedClient {
  fn name(&self) -> &str {
    "stepfun"
  }

  async fn synthesize(&self, request: TtsRequest) -> Result<TtsResponse> {
    let mime_type = tts_mime_type_for(request.response_format.as_deref()).to_string();
    let stepfun_request = TTSRequest {
      model: request.model,
      input: request.input,
      voice: request.voice,
      response_format: request.response_format,
      speed: request.speed,
      volume: request.volume,
      voice_label: None,
      sample_rate: request.sample_rate,
    };
    let audio = self.text_to_speech(stepfun_request).await?;
    Ok(TtsResponse { audio, mime_type })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_stepfun_provider_creation() {
    let provider = StepFunProvider::new("test-key", None);
    assert!(provider.is_ok());

    let provider = StepFunProvider::new("", None);
    assert!(provider.is_err());
  }

  #[tokio::test]
  async fn build_headers_injects_traceparent_when_scope_active() {
    use crate::trace_context::{LlmTraceContext, scope};

    let provider = StepFunProvider::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { provider.build_headers() })
      .await
      .expect("ASCII key builds cleanly");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
  }

  #[tokio::test]
  async fn specialized_client_build_auth_headers_injects_traceparent() {
    use crate::trace_context::{LlmTraceContext, scope};

    let client = StepFunSpecializedClient::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { client.build_auth_headers() })
      .await
      .expect("ASCII key builds cleanly");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
  }

  #[test]
  fn test_supported_models() {
    let provider = StepFunProvider::new("test-key", None).unwrap();
    let models = provider.supported_models();
    assert!(models.contains(&"step-1o-turbo-vision".to_string()));
    assert!(models.len() > 5);
  }

  #[test]
  fn build_request_body_passes_tools_through_openai_format() {
    use crate::tool_calling::{ToolChoice, ToolSpec};
    let provider = StepFunProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new("ping", "Ping a host", json!({"type": "object"}));
    let request = ProviderRequest {
      model: "step-3.5-flash".to_string(),
      messages: vec![],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: Some(ToolChoice::Required),
      thinking: None,
      response_format: None,
    };
    let body = provider.build_request_body(&request).expect("body");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools[0]["function"]["name"], "ping");
    assert_eq!(body["tool_choice"], "required");
  }

  fn thinking_request(model: &str, thinking: ThinkingConfig) -> ProviderRequest {
    ProviderRequest {
      model: model.to_string(),
      messages: vec![],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(thinking),
      response_format: None,
    }
  }

  #[test]
  fn thinking_maps_to_stepfun_reasoning_effort() {
    let provider = StepFunProvider::new("test-key", None).unwrap();
    let effort = |model: &str, thinking: ThinkingConfig| {
      provider
        .build_request_body(&thinking_request(model, thinking))
        .expect("body")
        .get("reasoning_effort")
        .cloned()
    };
    assert_eq!(
      effort("step-5-preview", ThinkingConfig::Medium),
      Some(json!("medium"))
    );
    assert_eq!(
      effort("step-3.7-flash", ThinkingConfig::High),
      Some(json!("high"))
    );
    assert_eq!(
      effort("step-5-preview", ThinkingConfig::Effort("minimal".into())),
      Some(json!("low"))
    );
    // 2603 has no `medium`.
    assert_eq!(
      effort("step-3.5-flash-2603", ThinkingConfig::Medium),
      Some(json!("high"))
    );
    assert_eq!(
      effort("step-3.5-flash-2603", ThinkingConfig::Low),
      Some(json!("low"))
    );
    assert_eq!(effort("step-5-preview", ThinkingConfig::Auto), None);
  }

  #[test]
  fn disabling_thinking_on_stepfun_is_an_error() {
    let provider = StepFunProvider::new("test-key", None).unwrap();
    let result = provider.build_request_body(&thinking_request(
      "step-5-preview",
      ThinkingConfig::Disabled,
    ));
    assert!(matches!(result, Err(LLMError::ConfigurationError { .. })));
  }

  #[test]
  fn response_reasoning_is_read_from_either_field() {
    let message: StepFunMessage = serde_json::from_value(json!({
      "role": "assistant",
      "content": "391",
      "reasoning_content": "17 * 23 = 391"
    }))
    .unwrap();
    assert_eq!(message.reasoning_content.as_deref(), Some("17 * 23 = 391"));
    let message: StepFunMessage = serde_json::from_value(json!({
      "role": "assistant",
      "content": "391",
      "reasoning": "17 * 23"
    }))
    .unwrap();
    assert_eq!(message.reasoning.as_deref(), Some("17 * 23"));
  }

  #[test]
  fn test_specialized_client_creation() {
    let client = StepFunSpecializedClient::new("test-key", None);
    assert!(client.is_ok());

    let client = StepFunSpecializedClient::new("", None);
    assert!(client.is_err());
  }

  #[test]
  fn test_tts_builder() {
    let request = TTSBuilder::new("step-tts-mini", "Hello world", "default_voice")
      .response_format("mp3")
      .speed(1.2)
      .emotion("高兴")
      .build();

    assert_eq!(request.model, "step-tts-mini");
    assert_eq!(request.input, "Hello world");
    assert_eq!(request.speed, Some(1.2));
    assert!(request.voice_label.is_some());
    assert_eq!(
      request.voice_label.unwrap().emotion,
      Some("高兴".to_string())
    );
  }

  #[test]
  fn stepfun_specialized_client_implements_all_modality_traits() {
    // Compile-time check: StepFunSpecializedClient implements every
    // trait in `providers::modality`, so the dispatcher (P-LLM.2) can
    // route any of the 5 modalities to it without per-trait casting.
    // This is a hermetic trait-object materialisation test — no
    // network calls.
    fn make() -> StepFunSpecializedClient {
      StepFunSpecializedClient::new("test-key", None).expect("specialized client creation")
    }
    let _: Box<dyn AsrProvider> = Box::new(make());
    let _: Box<dyn TtsProvider> = Box::new(make());

    // Also assert each trait reports the canonical provider name.
    let asr: Box<dyn AsrProvider> = Box::new(make());
    assert_eq!(asr.name(), "stepfun");
  }

  #[test]
  fn tts_mime_type_falls_back_to_wav_for_unknown_or_missing() {
    assert_eq!(tts_mime_type_for(Some("mp3")), "audio/mpeg");
    assert_eq!(tts_mime_type_for(Some("flac")), "audio/flac");
    assert_eq!(tts_mime_type_for(Some("opus")), "audio/opus");
    assert_eq!(tts_mime_type_for(Some("wav")), "audio/wav");
    assert_eq!(tts_mime_type_for(None), "audio/wav");
    // Unknown format ⇒ default to wav (matches the existing TTS node
    // fallback behaviour, so the post-P-LLM.3 routing change is a no-op).
    assert_eq!(tts_mime_type_for(Some("aac")), "audio/wav");
  }

  /// P-LLM2.5 regression: StepFun speaks the identical OpenAI-compatible
  /// `delta.tool_calls[]` streaming shape — mirrors
  /// `openai::tests::streaming_tool_call_delta_carries_id_and_name`.
  #[test]
  fn streaming_tool_call_delta_carries_id_and_name() {
    let chunk = StepFunStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"step-2-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 0);
    assert_eq!(delta.id.as_deref(), Some("call_abc"));
    assert_eq!(delta.name.as_deref(), Some("get_weather"));
    assert!(!chunk.is_final);
  }

  #[test]
  fn streaming_tool_call_subsequent_delta_appends_arguments() {
    let chunk = StepFunStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"step-2-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert!(delta.id.is_none());
    assert_eq!(delta.arguments_delta.as_deref(), Some("{\"city\":"));
  }

  /// A tool-call-only delta (no `content`) must not be silently dropped —
  /// pre-fix, the required-`content` guard on the whole branch meant this
  /// case never even reached the (also previously hardcoded-empty)
  /// `tool_call_deltas` field.
  #[test]
  fn streaming_tool_call_only_delta_is_not_dropped() {
    let chunk = StepFunStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"step-2-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"noop\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
    );
    assert!(
      chunk.is_some(),
      "a tool-call-only delta (no text content) must not be dropped"
    );
    assert_eq!(chunk.unwrap().content, "");
  }
}
