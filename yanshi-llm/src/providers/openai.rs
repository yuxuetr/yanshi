use super::line_buffer::{LineBuffer, is_sse_done};
use crate::{
  LLMError, ResponseFormat, Result,
  client::streaming::{StreamChunk, StreamingResponse, TokenUsage, ToolCallDelta},
  providers::{ContentType, LLMProvider, ProviderRequest, ProviderResponse},
  thinking::ThinkingConfig,
  tool_calling::{StopReason, ToolCallRequest, ToolChoice, ToolSpec},
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::pin::Pin;
use tokio_stream::Stream;

pub struct OpenAIProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl OpenAIProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`].
  ///
  /// Useful when callers need a non-default client — for example tests that
  /// must disable the system proxy (`.no_proxy()` on the builder) to reach a
  /// localhost mock, or production deployments that share one HTTPS-pinned
  /// client across providers.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "openai".to_string(),
      });
    }

    let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());

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
    // Q2.5.3: surface invalid-character API keys as
    // `ConfigurationError` instead of panicking the runtime.
    headers.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|err| {
        LLMError::ConfigurationError {
          message: format!("OpenAI API key contains invalid characters: {err}"),
        }
      })?,
    );
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  fn build_request_body(&self, request: &ProviderRequest) -> Value {
    let mut body = json!({
      "model": request.model,
      "messages": request.messages,
      "stream": request.stream
    });
    // Y2.1 (COR-1): without this an OpenAI-compatible stream carries no
    // usage at all, so every streamed call (all ReAct turns) reported
    // `total_tokens: None` and token / cost budgets never accrued.
    // Before `parameters` so a caller can still override it.
    if request.stream && accepts_stream_options(&request.model) {
      body["stream_options"] = json!({ "include_usage": true });
    }

    // Add additional parameters
    for (key, value) in &request.parameters {
      body[key] = value.clone();
    }

    if let Some(tools) = &request.tools {
      body["tools"] = Value::Array(tools.iter().map(tool_spec_to_openai_value).collect());
    }
    if let Some(choice) = &request.tool_choice {
      body["tool_choice"] = tool_choice_to_openai_value(choice);
    }

    // DeepSeek's V4 API diverges from real OpenAI here despite otherwise
    // routing through this "OpenAI-compatible" provider — see
    // `thinking_config_to_deepseek_value`'s doc comment. Dispatched on the
    // model name (no per-vendor field exists on this struct — `OpenAIProvider`
    // is shared verbatim by DashScope/GLM/DeepSeek/MiniMax), mirroring the
    // same `starts_with("deepseek-")` idiom `tokenizer.rs` already uses to
    // pick a per-vendor token-counting heuristic.
    if let Some(thinking) = &request.thinking {
      if is_deepseek_model(&request.model) {
        body["thinking"] = thinking_config_to_deepseek_value(thinking);
      } else if let Some(effort) = thinking_config_to_openai_effort(thinking) {
        body["reasoning_effort"] = Value::String(effort.to_string());
      }
    }

    if let Some(format) = &request.response_format {
      body["response_format"] = response_format_to_openai_value(format);
    }

    body
  }
}

/// Whether a model served through this provider takes
/// `stream_options.include_usage`. OpenAI, DeepSeek and DashScope
/// document it. MiniMax (`api.minimaxi.com`) has been reported to reject
/// it as an invalid chat setting (error 2013), and GLM documents no such
/// field, so both are skipped by model name; the stream parser still
/// picks up whatever usage they send on their own final chunk.
pub(crate) fn accepts_stream_options(model: &str) -> bool {
  let model = model.to_ascii_lowercase();
  !(model.starts_with("glm-") || model.starts_with("minimax-"))
}

/// Encode a [`ResponseFormat`] as OpenAI's `response_format` request field.
/// Shared verbatim by Moonshot/StepFun, which speak the identical
/// OpenAI-compatible wire shape and support it alongside `tools` in the
/// same request (unlike Anthropic/Google — see `ProviderRequest::
/// response_format`'s doc comment).
pub(crate) fn response_format_to_openai_value(format: &ResponseFormat) -> Value {
  match format {
    ResponseFormat::Text => json!({ "type": "text" }),
    ResponseFormat::JsonObject => json!({ "type": "json_object" }),
    ResponseFormat::JsonSchema {
      name,
      schema,
      strict,
    } => {
      let mut json_schema = json!({ "name": name, "schema": schema });
      if let Some(strict_val) = strict {
        json_schema["strict"] = Value::Bool(*strict_val);
      }
      json!({ "type": "json_schema", "json_schema": json_schema })
    }
  }
}

/// Encode a [`ThinkingConfig`] as OpenAI's `reasoning_effort` request field.
///
/// Returns `None` for [`ThinkingConfig::Disabled`] — caller omits the field
/// entirely so the model uses its default behaviour. The four accepted
/// values are `minimal`, `low`, `medium`, `high`; unknown caller-supplied
/// strings get normalised in [`ThinkingConfig::to_openai_effort`].
pub(crate) fn thinking_config_to_openai_effort(config: &ThinkingConfig) -> Option<&'static str> {
  config.to_openai_effort()
}

/// Whether `model` is a DeepSeek model, by name prefix — the only signal
/// available inside `build_request_body`, since `OpenAIProvider` carries no
/// per-vendor tag (DashScope/GLM/DeepSeek/MiniMax all share this one struct).
/// Same idiom `tokenizer.rs` already uses to pick a per-vendor token-counting
/// heuristic.
pub(crate) fn is_deepseek_model(model: &str) -> bool {
  model.to_ascii_lowercase().starts_with("deepseek-")
}

/// Encode a [`ThinkingConfig`] as DeepSeek's `thinking` request block
/// (`api-docs.deepseek.com/zh-cn/api/create-chat-completion`, verified
/// 2026-08-13).
///
/// Despite `deepseek-v4-*` routing through this "OpenAI-compatible"
/// provider, DeepSeek's actual wire shape diverges from real OpenAI's flat
/// `reasoning_effort` string in two ways this function has to account for:
///  - it's a **nested object**: `{"type": "enabled"|"disabled",
///    "reasoning_effort": "low"|"high"|"max"}`, not a top-level field — the
///    flat `reasoning_effort` this crate previously sent for every
///    OpenAI-compatible vendor is simply an unrecognized field DeepSeek's API
///    ignores, so `.thinking(...)` against a DeepSeek model was a silent
///    no-op before this fix, despite `ThinkingConfig`'s own module doc
///    (written for the older DeepSeek-R1 API) claiming DeepSeek has "no
///    input knob";
///  - thinking **defaults to `enabled`** (unlike OpenAI o-series, where
///    omitting `reasoning_effort` doesn't turn reasoning off, just leaves
///    the level unspecified) — so [`ThinkingConfig::Disabled`] must
///    *actively* send `{"type": "disabled"}`, not omit the field entirely
///    the way [`thinking_config_to_openai_effort`] does.
///
/// Effort levels: DeepSeek only accepts `low` / `high` (default) / `max` —
/// `medium` and `xhigh` alias to `high` server-side per the docs' own
/// compatibility note; this function maps directly to `high` for those
/// instead of relying on that server-side aliasing.
pub(crate) fn thinking_config_to_deepseek_value(config: &ThinkingConfig) -> Value {
  if config.is_disabled() {
    return json!({ "type": "disabled" });
  }
  let effort = match config {
    // Auto: omit `reasoning_effort` and let DeepSeek apply its own default
    // (`high`) rather than force a value, mirroring how Anthropic/Google
    // handle `Auto` elsewhere in this crate.
    ThinkingConfig::Auto => None,
    ThinkingConfig::Low => Some("low"),
    ThinkingConfig::Medium | ThinkingConfig::High => Some("high"),
    ThinkingConfig::Budget(n) => Some(deepseek_budget_to_effort(*n)),
    ThinkingConfig::Effort(s) => Some(deepseek_normalise_effort(s)),
    ThinkingConfig::Disabled => unreachable!("handled by the is_disabled() check above"),
  };
  match effort {
    Some(effort) => json!({ "type": "enabled", "reasoning_effort": effort }),
    None => json!({ "type": "enabled" }),
  }
}

/// Bucket an explicit token budget into DeepSeek's three effort levels.
/// DeepSeek doesn't accept a raw token budget, so this reuses the same
/// split philosophy as `ThinkingConfig`'s own OpenAI four-level bucketing
/// (`thinking.rs`'s private `budget_to_effort`), extended with a `max` tier
/// for very large budgets DeepSeek can express but OpenAI's scheme can't.
fn deepseek_budget_to_effort(n: u32) -> &'static str {
  if n <= 2048 {
    "low"
  } else if n <= 16384 {
    "high"
  } else {
    "max"
  }
}

/// Normalise an arbitrary caller-supplied effort string to DeepSeek's three
/// accepted values.
fn deepseek_normalise_effort(s: &str) -> &'static str {
  match s.to_ascii_lowercase().as_str() {
    "minimal" | "low" => "low",
    "max" | "xhigh" => "max",
    // "medium", "high", and anything unrecognized — matches DeepSeek's own
    // server-side aliasing of medium/xhigh, so an unknown value degrades to
    // the same effort level medium would.
    _ => "high",
  }
}

/// Encode a `ToolSpec` as the OpenAI `{ "type": "function", "function": ... }`
/// wire format.
pub(crate) fn tool_spec_to_openai_value(spec: &ToolSpec) -> Value {
  json!({
    "type": "function",
    "function": {
      "name": spec.name,
      "description": spec.description,
      "parameters": spec.parameters,
    }
  })
}

/// Encode a `ToolChoice` as the OpenAI `tool_choice` wire format.
pub(crate) fn tool_choice_to_openai_value(choice: &ToolChoice) -> Value {
  match choice {
    ToolChoice::Auto => Value::String("auto".to_string()),
    ToolChoice::None => Value::String("none".to_string()),
    ToolChoice::Required => Value::String("required".to_string()),
    ToolChoice::Tool { name } => json!({
      "type": "function",
      "function": { "name": name }
    }),
  }
}

/// Decode OpenAI `tool_calls` array into typed `ToolCallRequest`s.
///
/// OpenAI returns `arguments` as a JSON-encoded string; we parse it eagerly so
/// callers don't need to know the wire format. Malformed JSON falls back to a
/// `Value::String` of the raw payload so the call can still surface in traces.
pub(crate) fn parse_openai_tool_calls(value: &Value) -> Vec<ToolCallRequest> {
  let Some(items) = value.as_array() else {
    return Vec::new();
  };
  items
    .iter()
    .enumerate()
    .filter_map(|(idx, item)| {
      let function = item.get("function")?;
      let name = function.get("name")?.as_str()?.to_string();
      let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{}", idx));
      let raw_args = function.get("arguments");
      let arguments = match raw_args {
        Some(Value::String(s)) => {
          serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Object(serde_json::Map::new()),
      };
      Some(ToolCallRequest {
        id,
        name,
        arguments,
      })
    })
    .collect()
}

#[async_trait]
impl LLMProvider for OpenAIProvider {
  fn name(&self) -> &str {
    "openai"
  }

  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse> {
    if request.stream {
      return Err(LLMError::InternalError {
        message: "Use execute_streaming for streaming requests".to_string(),
      });
    }

    let url = format!("{}/chat/completions", self.base_url);
    let body = self.build_request_body(request);

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

    let openai_response: OpenAIResponse = response.json().await?;

    // Handle both string and array content formats
    let content_text = if let Some(first_choice) = openai_response.choices.first() {
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

    // Convert to ContentType - OpenAI currently only returns text
    let content = ContentType::Text(content_text);

    let usage = openai_response
      .usage
      .clone()
      .map(|u| crate::providers::TokenUsage {
        prompt_tokens: Some(u.prompt_tokens),
        completion_tokens: Some(u.completion_tokens),
        total_tokens: Some(u.total_tokens),
      });

    let first_choice = openai_response.choices.first();
    let tool_calls = first_choice
      .and_then(|c| c.message.tool_calls.as_ref())
      .map(parse_openai_tool_calls)
      .unwrap_or_default();
    let stop_reason = if tool_calls.is_empty() {
      first_choice
        .and_then(|c| c.finish_reason.as_deref())
        .map(StopReason::from_openai_finish_reason)
    } else {
      Some(StopReason::ToolCalls)
    };

    // Reasoning text — for DeepSeek-R1 the provider returns
    // `reasoning_content` alongside `content`; for vanilla OpenAI chat
    // completions it's `None`. See `OpenAIMessage::reasoning_content`.
    let thinking = first_choice
      .and_then(|c| c.message.reasoning_content.clone())
      .filter(|s| !s.is_empty());

    Ok(ProviderResponse {
      content,
      usage,
      metadata: Some(serde_json::to_value(&openai_response)?),
      tool_calls,
      stop_reason,
      thinking,
    })
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

    let url = format!("{}/chat/completions", self.base_url);
    let body = self.build_request_body(request);

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

    Ok(Box::new(OpenAIStreamingResponse::new(response)))
  }

  async fn validate_config(&self) -> Result<()> {
    // Simple health check - try to list models
    let url = format!("{}/models", self.base_url);

    let response = self
      .client
      .get(&url)
      .headers(self.build_headers()?)
      .send()
      .await?;

    if !response.status().is_success() {
      return Err(LLMError::AuthenticationError {
        provider: "openai".to_string(),
        message: "Failed to authenticate with OpenAI API".to_string(),
      });
    }

    Ok(())
  }

  fn base_url(&self) -> &str {
    &self.base_url
  }

  fn supported_models(&self) -> Vec<String> {
    vec![
      "gpt-4o".to_string(),
      "gpt-4o-mini".to_string(),
      "gpt-4-turbo".to_string(),
      "gpt-4".to_string(),
      "gpt-3.5-turbo".to_string(),
    ]
  }
}

// OpenAI API response structures
#[derive(Debug, Deserialize, Serialize)]
struct OpenAIResponse {
  id: String,
  object: String,
  created: u64,
  model: String,
  choices: Vec<OpenAIChoice>,
  usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAIChoice {
  index: u32,
  message: OpenAIMessage,
  finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAIMessage {
  role: String,
  content: Option<serde_json::Value>, // Can be string or array of content objects
  #[serde(default, skip_serializing_if = "Option::is_none")]
  tool_calls: Option<serde_json::Value>,
  /// DeepSeek-R1 surfaces the model's chain-of-thought here, alongside
  /// `content`. Captured so the typed `ProviderResponse.thinking` channel
  /// can carry it rather than relying on the raw metadata blob. Vanilla
  /// OpenAI chat completions don't emit this field; the `default` lets
  /// deserialisation succeed in both cases.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OpenAIUsage {
  prompt_tokens: u32,
  completion_tokens: u32,
  total_tokens: u32,
}

// Streaming response structures
#[derive(Debug, Deserialize, Serialize)]
struct OpenAIStreamingChunk {
  // Defaulted: some compatible vendors omit these on the usage-only frame.
  #[serde(default)]
  id: String,
  #[serde(default)]
  object: String,
  #[serde(default)]
  created: u64,
  #[serde(default)]
  model: String,
  choices: Vec<OpenAIStreamingChoice>,
  usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAIStreamingChoice {
  index: u32,
  delta: OpenAIStreamingDelta,
  finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAIStreamingDelta {
  role: Option<String>,
  content: Option<serde_json::Value>, // Can be string or array of content objects
  /// Q2.5.2: streamed tool_call deltas. The provider sends partial JSON
  /// for `function.arguments`; consumers must concatenate per-index.
  #[serde(default)]
  tool_calls: Option<Vec<OpenAIStreamingToolCall>>,
  /// Y10.3: reasoning text streamed by OpenAI-compatible reasoning models
  /// (DeepSeek, Qwen, GLM, Kimi, StepFun call it `reasoning_content`;
  /// some vendors use `reasoning`).
  #[serde(default)]
  reasoning_content: Option<String>,
  #[serde(default)]
  reasoning: Option<String>,
}

/// Y10.3: the reasoning text of one OpenAI-compatible streaming delta.
pub(crate) fn streaming_reasoning(
  reasoning_content: Option<&str>,
  reasoning: Option<&str>,
) -> String {
  reasoning_content
    .or(reasoning)
    .unwrap_or_default()
    .to_string()
}

/// Q2.5.2: incremental tool_call entry inside a streaming delta.
///
/// `pub(crate)` (P-LLM2.5): Moonshot and StepFun speak this exact same
/// OpenAI-compatible `delta.tool_calls[]` streaming shape despite having
/// their own separate provider structs (they're not composed over
/// `OpenAIProvider`), so they deserialize into this type directly and
/// share [`openai_streaming_tool_call_deltas`] rather than each
/// reimplementing the same mapping.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct OpenAIStreamingToolCall {
  index: u32,
  #[serde(default)]
  id: Option<String>,
  #[serde(default, rename = "type")]
  _kind: Option<String>,
  #[serde(default)]
  function: Option<OpenAIStreamingToolCallFunction>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct OpenAIStreamingToolCallFunction {
  #[serde(default)]
  name: Option<String>,
  #[serde(default)]
  arguments: Option<String>,
}

/// Map a streamed `delta.tool_calls[]` array into [`ToolCallDelta`]s.
/// Shared by OpenAI, Moonshot, and StepFun — see
/// [`OpenAIStreamingToolCall`]'s doc comment for why.
pub(crate) fn openai_streaming_tool_call_deltas(
  tool_calls: Option<&[OpenAIStreamingToolCall]>,
) -> Vec<ToolCallDelta> {
  tool_calls
    .map(|calls| {
      calls
        .iter()
        .map(|call| ToolCallDelta {
          index: call.index,
          id: call.id.clone(),
          name: call.function.as_ref().and_then(|f| f.name.clone()),
          arguments_delta: call.function.as_ref().and_then(|f| f.arguments.clone()),
        })
        .collect::<Vec<_>>()
    })
    .unwrap_or_default()
}

pub struct OpenAIStreamingResponse {
  stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
  buffer: LineBuffer,
  finished: bool,
}

// Q2.5.4: removed `unsafe impl Send + Sync`. The inner pinned
// `dyn Stream + Send` is auto-Send already, and the trait no longer
// requires Sync (streams are sequentially consumed via &mut self).

impl OpenAIStreamingResponse {
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
        thinking: String::new(),
        media: Vec::new(),
        content: String::new(),
        is_final: true,
        metadata: None,
        usage: None,
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      });
    }

    let Ok(chunk) = serde_json::from_str::<OpenAIStreamingChunk>(data) else {
      return None;
    };
    // Y2.1: the `include_usage` frame has `choices: []` and only `usage`.
    // It used to fail the `choices.first()` check and vanish. Its
    // metadata is left out so the collector keeps the last content
    // frame's raw metadata.
    if chunk.choices.is_empty() {
      return chunk.usage.map(|u| StreamChunk {
        thinking: String::new(),
        media: Vec::new(),
        content: String::new(),
        is_final: false,
        metadata: None,
        usage: Some(TokenUsage {
          prompt_tokens: Some(u.prompt_tokens),
          completion_tokens: Some(u.completion_tokens),
          total_tokens: Some(u.total_tokens),
        }),
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      });
    }
    if let Some(choice) = chunk.choices.first() {
      let content_text = match &choice.delta.content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
      };

      let tool_call_deltas = openai_streaming_tool_call_deltas(choice.delta.tool_calls.as_deref());
      let thinking = streaming_reasoning(
        choice.delta.reasoning_content.as_deref(),
        choice.delta.reasoning.as_deref(),
      );

      // Q2.5.2: emit a chunk when there is *any* signal — text content,
      // tool_call delta, finish_reason, or usage — so tool-call-only
      // turns don't get silently dropped between [DONE] sentinels.
      let has_signal = !content_text.is_empty()
        || !thinking.is_empty()
        || !tool_call_deltas.is_empty()
        || choice.finish_reason.is_some()
        || chunk.usage.is_some();
      if !has_signal {
        return None;
      }

      return Some(StreamChunk {
        thinking,
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
impl StreamingResponse for OpenAIStreamingResponse {
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
    if self.finished {
      return Ok(None);
    }

    loop {
      // W0.3: drain any complete lines already sitting in the buffer
      // *before* pulling more bytes off the network. A single network
      // read can contain multiple SSE `data:` lines (e.g. the final
      // content delta immediately followed by `[DONE]` in the same
      // read) — draining only inside the `Some(Ok(data))` arm below and
      // returning on the first parsed line stranded every subsequent
      // line in the buffer until another network read arrived to
      // re-trigger the drain. If that never-drained line happened to be
      // the *last* one because the stream had already ended, it was
      // silently dropped and `is_final` was never observed from a real
      // `[DONE]`/finish_reason chunk. See `stepfun.rs`'s
      // `StepFunStreamingResponse::next_chunk` for the reference fix.
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

#[cfg(test)]
mod tests {
  use super::*;

  /// `ThinkingConfig::High` must emit `reasoning_effort: "high"` on the
  /// request body. OpenAI is pass-through but the field must still appear
  /// on the wire even when `parameters` is empty.
  #[test]
  fn build_request_body_emits_reasoning_effort_for_thinking_high() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "o3-mini".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::High),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(body["reasoning_effort"], "high");
  }

  #[test]
  fn build_request_body_omits_reasoning_effort_when_disabled() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "o3-mini".to_string(),
      messages: vec![json!({"role": "user", "content": "no reasoning"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Disabled),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert!(body.get("reasoning_effort").is_none());
  }

  /// DeepSeek's real wire shape for thinking is a nested object, not
  /// OpenAI's flat `reasoning_effort` string — despite `deepseek-v4-*`
  /// routing through this same `OpenAIProvider`. Regression test for the
  /// "DeepSeek `.thinking()` calls were a silent no-op" bug this fixes.
  #[test]
  fn build_request_body_emits_nested_thinking_block_for_deepseek_model() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "deepseek-v4-pro".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::High),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert!(
      body.get("reasoning_effort").is_none(),
      "must not send OpenAI's flat field for a DeepSeek model"
    );
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["reasoning_effort"], "high");
  }

  /// DeepSeek thinking defaults to *enabled* (unlike OpenAI o-series, which
  /// has no off switch) — `Disabled` must actively send
  /// `{"type": "disabled"}`, not omit the field the way the OpenAI path
  /// does above.
  #[test]
  fn build_request_body_actively_disables_thinking_for_deepseek_model() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "deepseek-v4-flash".to_string(),
      messages: vec![json!({"role": "user", "content": "no reasoning"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Disabled),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
  }

  #[test]
  fn build_request_body_deepseek_auto_omits_explicit_effort() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "deepseek-v4-pro".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Auto),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(body["thinking"].get("reasoning_effort").is_none());
  }

  #[test]
  fn is_deepseek_model_matches_by_prefix_case_insensitively() {
    assert!(is_deepseek_model("deepseek-v4-pro"));
    assert!(is_deepseek_model("DeepSeek-V4-Flash"));
    assert!(!is_deepseek_model("gpt-4o-mini"));
    assert!(!is_deepseek_model("qwen-deepseek-lookalike"));
  }

  #[test]
  fn thinking_config_to_deepseek_value_maps_every_qualitative_level() {
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Low),
      json!({"type": "enabled", "reasoning_effort": "low"})
    );
    // DeepSeek only accepts low/high/max server-side (medium aliases to
    // high) — map directly instead of relying on that server aliasing.
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Medium),
      json!({"type": "enabled", "reasoning_effort": "high"})
    );
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::High),
      json!({"type": "enabled", "reasoning_effort": "high"})
    );
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Budget(1000)),
      json!({"type": "enabled", "reasoning_effort": "low"})
    );
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Budget(30_000)),
      json!({"type": "enabled", "reasoning_effort": "max"})
    );
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Effort("xhigh".to_string())),
      json!({"type": "enabled", "reasoning_effort": "max"})
    );
    assert_eq!(
      thinking_config_to_deepseek_value(&ThinkingConfig::Disabled),
      json!({"type": "disabled"})
    );
  }

  /// DeepSeek-R1 routes through `OpenAIProvider` (per `providers/mod.rs::
  /// create_provider`) and returns `reasoning_content` alongside
  /// `content`. The deserialise path must capture it so the typed
  /// `ProviderResponse.thinking` channel carries the reasoning text.
  #[test]
  fn openai_message_deserialises_reasoning_content_for_deepseek_r1() {
    let raw = json!({
      "role": "assistant",
      "content": "The answer is 42.",
      "reasoning_content": "Let me think step by step about this..."
    });
    let msg: OpenAIMessage = serde_json::from_value(raw).unwrap();
    assert_eq!(
      msg.reasoning_content.as_deref(),
      Some("Let me think step by step about this...")
    );
  }

  /// Vanilla OpenAI chat completions do NOT emit `reasoning_content`.
  /// Deserialise must succeed and leave the field `None`.
  #[test]
  fn openai_message_reasoning_content_is_optional() {
    let raw = json!({
      "role": "assistant",
      "content": "hi"
    });
    let msg: OpenAIMessage = serde_json::from_value(raw).unwrap();
    assert!(msg.reasoning_content.is_none());
  }

  #[test]
  fn test_openai_provider_creation() {
    let provider = OpenAIProvider::new("test-key", None);
    assert!(provider.is_ok());

    let provider = OpenAIProvider::new("", None);
    assert!(provider.is_err());
  }

  #[test]
  fn with_client_validates_api_key_and_keeps_caller_supplied_client() {
    let custom = reqwest::Client::builder()
      .no_proxy()
      .build()
      .expect("custom client");

    assert!(
      OpenAIProvider::with_client(custom.clone(), "", Some("http://x".into())).is_err(),
      "empty api key must error",
    );
    let provider = OpenAIProvider::with_client(custom, "test-key", Some("http://example".into()))
      .expect("custom client provider");
    assert_eq!(provider.base_url, "http://example");
  }

  #[tokio::test]
  async fn build_headers_injects_traceparent_when_scope_active() {
    use crate::trace_context::{LlmTraceContext, scope};

    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { provider.build_headers() })
      .await
      .expect("headers must build under a well-formed ASCII API key");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
  }

  #[test]
  fn build_headers_omits_traceparent_when_no_scope_active() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let headers = provider.build_headers().expect("ASCII key builds cleanly");
    assert!(headers.get("traceparent").is_none());
  }

  /// Q2.5.3 regression: invalid-character API key surfaces as
  /// ConfigurationError instead of panicking the runtime.
  #[test]
  fn build_headers_returns_err_on_invalid_api_key() {
    let provider = OpenAIProvider::new("bad\nkey", None).unwrap();
    let err = provider
      .build_headers()
      .expect_err("newline-in-key must fail closed");
    assert!(matches!(err, LLMError::ConfigurationError { .. }));
  }

  // Q2.5.2: OpenAI streams tool_calls via `choices[0].delta.tool_calls`. The
  // first delta carries id/name; subsequent deltas append `function.arguments`
  // partial JSON. Concatenating all `arguments_delta` per index yields the
  // canonical argument JSON.
  #[test]
  fn streaming_tool_call_delta_carries_id_and_name() {
    let chunk = OpenAIStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 0);
    assert_eq!(delta.id.as_deref(), Some("call_abc"));
    assert_eq!(delta.name.as_deref(), Some("get_weather"));
    assert_eq!(delta.arguments_delta.as_deref(), Some(""));
    assert!(!chunk.is_final);
  }

  /// Y10.3: DeepSeek / Qwen / GLM stream `reasoning_content`, others
  /// `reasoning`; both become `StreamChunk.thinking`.
  #[test]
  fn streaming_reasoning_delta_is_surfaced_as_thinking() {
    for field in ["reasoning_content", "reasoning"] {
      let line = format!(
        "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"deepseek-v4-flash\",\"choices\":[{{\"index\":0,\"delta\":{{\"{field}\":\"step one\"}},\"finish_reason\":null}}]}}"
      );
      let chunk = OpenAIStreamingResponse::parse_sse_chunk(&line).expect("reasoning chunk");
      assert_eq!(chunk.thinking, "step one", "{field}");
      assert!(chunk.content.is_empty());
    }
  }

  #[test]
  fn streaming_tool_call_subsequent_delta_appends_arguments() {
    let chunk = OpenAIStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 0);
    assert!(delta.id.is_none());
    assert!(delta.name.is_none());
    assert_eq!(delta.arguments_delta.as_deref(), Some("{\"city\":"));
  }

  #[test]
  fn streaming_done_sentinel_finalizes() {
    let chunk = OpenAIStreamingResponse::parse_sse_chunk("data: [DONE]").unwrap();
    assert!(chunk.is_final);
    assert!(chunk.tool_call_deltas.is_empty());
  }

  fn stream_request(model: &str, stream: bool) -> ProviderRequest {
    ProviderRequest {
      model: model.to_string(),
      messages: vec![json!({"role": "user", "content": "test"})],
      stream,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    }
  }

  /// Y2.1: streamed requests ask for usage; non-streamed ones (usage is
  /// always in the body) and vendors that reject the field do not.
  #[test]
  fn build_request_body_requests_stream_usage_where_supported() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    for model in ["gpt-4o", "deepseek-v4-chat", "qwen-plus"] {
      let body = provider.build_request_body(&stream_request(model, true));
      assert_eq!(
        body["stream_options"],
        json!({ "include_usage": true }),
        "{model}"
      );
    }
    let body = provider.build_request_body(&stream_request("gpt-4o", false));
    assert!(body.get("stream_options").is_none());
    for model in ["glm-5.1", "MiniMax-M2.7"] {
      let body = provider.build_request_body(&stream_request(model, true));
      assert!(body.get("stream_options").is_none(), "{model}");
    }
  }

  #[test]
  fn build_request_body_lets_parameters_override_stream_options() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let mut request = stream_request("gpt-4o", true);
    request.parameters.insert(
      "stream_options".to_string(),
      json!({ "include_usage": false }),
    );
    let body = provider.build_request_body(&request);
    assert_eq!(body["stream_options"]["include_usage"], false);
  }

  #[test]
  fn usage_only_frame_parses_to_a_usage_chunk() {
    let chunk = OpenAIStreamingResponse::parse_sse_chunk(
      "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}",
    )
    .expect("usage frame must not be dropped");
    assert!(!chunk.is_final);
    assert!(chunk.metadata.is_none());
    assert_eq!(chunk.usage.expect("usage").total_tokens, Some(3));
  }

  #[test]
  fn test_build_request_body() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("temperature".to_string(), json!(0.7));
    params.insert("max_tokens".to_string(), json!(100));

    let request = ProviderRequest {
      model: "gpt-4o".to_string(),
      messages: vec![json!({"role": "user", "content": "test"})],
      stream: false,
      parameters: params,
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["stream"], false);
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
  }

  #[test]
  fn build_request_body_serialises_tools() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new(
      "get_weather",
      "Return the weather for a city",
      json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"]
      }),
    );
    let request = ProviderRequest {
      model: "gpt-4o".to_string(),
      messages: vec![json!({"role": "user", "content": "weather in Tokyo?"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: Some(ToolChoice::Required),
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
    assert_eq!(tools[0]["function"]["parameters"]["required"][0], "city");
    assert_eq!(body["tool_choice"], "required");
  }

  #[test]
  fn build_request_body_tool_choice_specific() {
    let provider = OpenAIProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gpt-4o".to_string(),
      messages: vec![],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![ToolSpec::new("x", "", json!({}))]),
      tool_choice: Some(ToolChoice::Tool {
        name: "x".to_string(),
      }),
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert_eq!(body["tool_choice"]["type"], "function");
    assert_eq!(body["tool_choice"]["function"]["name"], "x");
  }

  #[test]
  fn parse_openai_tool_calls_decodes_arguments_json() {
    let raw = json!([
      {
        "id": "call_abc",
        "type": "function",
        "function": {
          "name": "get_weather",
          "arguments": "{\"city\": \"Tokyo\"}"
        }
      },
      {
        "id": "call_def",
        "type": "function",
        "function": {
          "name": "search",
          "arguments": "not json"
        }
      }
    ]);
    let calls = parse_openai_tool_calls(&raw);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_abc");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments["city"], "Tokyo");
    // Unparseable arguments fall back to a string so traces still see them.
    assert_eq!(calls[1].arguments, Value::String("not json".to_string()));
  }

  #[test]
  fn parse_openai_tool_calls_synthesises_id_when_missing() {
    let raw = json!([
      { "type": "function", "function": { "name": "ping", "arguments": "{}" } }
    ]);
    let calls = parse_openai_tool_calls(&raw);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_0");
  }

  #[test]
  fn openai_message_deserialises_tool_calls() {
    let raw = json!({
      "id": "x",
      "object": "chat.completion",
      "created": 0,
      "model": "gpt-4o",
      "choices": [
        {
          "index": 0,
          "message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [
              {
                "id": "call_1",
                "type": "function",
                "function": {
                  "name": "get_weather",
                  "arguments": "{\"city\": \"NYC\"}"
                }
              }
            ]
          },
          "finish_reason": "tool_calls"
        }
      ],
      "usage": null
    });
    let parsed: OpenAIResponse = serde_json::from_value(raw).unwrap();
    let choice = &parsed.choices[0];
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    let tool_calls = parse_openai_tool_calls(choice.message.tool_calls.as_ref().unwrap());
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "get_weather");
    assert_eq!(tool_calls[0].arguments["city"], "NYC");
  }
}
