use super::line_buffer::LineBuffer;
use crate::{
  LLMError, ResponseFormat, Result,
  client::streaming::{StreamChunk, StreamingResponse, ToolCallDelta},
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

pub struct AnthropicProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl AnthropicProvider {
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
        provider: "anthropic".to_string(),
      });
    }

    let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());

    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_headers(&self) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Q2.5.3: pre-fix this called `.expect("API key contains invalid
    // characters")` and panicked the entire runtime if a `.env` file
    // ended the value with a stray `\n`. Surface as
    // `ConfigurationError` so callers can degrade gracefully.
    headers.insert(
      "x-api-key",
      HeaderValue::from_str(&self.api_key).map_err(|err| LLMError::ConfigurationError {
        message: format!("Anthropic API key contains invalid characters: {err}"),
      })?,
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  fn build_request_body(&self, request: &ProviderRequest) -> Value {
    // Convert OpenAI-style messages to Anthropic format
    let mut system_message = None;
    let mut anthropic_messages = Vec::new();

    for message in &request.messages {
      if let Some(msg_obj) = message.as_object()
        && let (Some(role), Some(content)) = (msg_obj.get("role"), msg_obj.get("content"))
      {
        match role.as_str() {
          Some("system") => {
            // System messages stay text-only on Anthropic (mirrors Google's
            // `systemInstruction`); flatten any array parts to their
            // concatenated text rather than dropping non-string content
            // silently (P-LLM2.4 — pre-fix this was `content.as_str()`,
            // which yielded `None` — the whole system prompt vanishing —
            // for any multimodal system message, since `content` is an
            // array in that case, not a string).
            let text = super::openai_content_to_text(content);
            if !text.is_empty() {
              system_message = Some(text);
            }
          }
          Some("user") | Some("assistant") => {
            anthropic_messages.push(json!({
              "role": role,
              "content": openai_content_to_anthropic_content(content)
            }));
          }
          _ => {}
        }
      }
    }

    let mut body = json!({
      "model": request.model,
      "messages": anthropic_messages,
      "stream": request.stream
    });

    if let Some(system) = system_message {
      body["system"] = json!(system);
    }

    // Add additional parameters
    for (key, value) in &request.parameters {
      match key.as_str() {
        "max_tokens" => body["max_tokens"] = value.clone(),
        "temperature" => body["temperature"] = value.clone(),
        "top_p" => body["top_p"] = value.clone(),
        "top_k" => body["top_k"] = value.clone(),
        _ => {
          // Store other parameters in metadata for now
        }
      }
    }

    // Anthropic requires max_tokens to be specified. `body` is always a JSON
    // object (constructed from `json!({...})` above), so `body.get(...)` is
    // a safe, infallible alternative to `as_object().unwrap()` and avoids a
    // clippy `unwrap_used` deny under Q5.1.
    if body.get("max_tokens").is_none() {
      body["max_tokens"] = json!(4096);
    }

    if let Some(tools) = &request.tools {
      body["tools"] = Value::Array(tools.iter().map(tool_spec_to_anthropic_value).collect());
    }
    if let Some(choice) = &request.tool_choice {
      body["tool_choice"] = tool_choice_to_anthropic_value(choice);
    }

    if let Some(thinking) = &request.thinking
      && let Some(block) = thinking_config_to_anthropic_value(thinking)
    {
      body["thinking"] = block;
    }

    // V2.1: Anthropic has no native `response_format` field. Emulate a
    // `JsonSchema` constraint by forcing a single synthetic tool call whose
    // `input_schema` is the requested schema — `execute()` unwraps that
    // tool call's arguments back into plain-text `content` so callers see
    // schema-conformant JSON exactly as if it had come back as normal text.
    //
    // Only when no real `tools`/`tool_choice` were requested and the call
    // isn't streaming: forcing a tool choice here would hijack the
    // caller's own tool-calling turn (they'd never be able to call their
    // real tools), and unwrapping a forced tool call out of a streamed
    // response isn't implemented.
    if !request.stream
      && request.tools.is_none()
      && request.tool_choice.is_none()
      && let Some(ResponseFormat::JsonSchema { name, schema, .. }) = &request.response_format
    {
      body["tools"] = json!([{
        "name": name,
        "description": "Return the final structured response.",
        "input_schema": schema,
      }]);
      body["tool_choice"] = json!({ "type": "tool", "name": name });
    }

    body
  }
}

/// Translate an OpenAI-shaped `content` field (string, or an array of typed
/// parts as produced by [`crate::multimodal::MultimodalMessage::to_openai_format`])
/// into Anthropic's native content-block shape.
///
/// Plain string content passes through unchanged — Anthropic accepts a bare
/// string for text-only turns, same as OpenAI, so this is a no-op for the
/// (overwhelmingly common) text-only case. Array content is translated
/// block-by-block:
///  - `text` → `{"type":"text","text":...}`
///  - `image_url` whose URL is a `data:<mime>;base64,<payload>` URI →
///    `{"type":"image","source":{"type":"base64","media_type":...,"data":...}}`
///  - `image_url` whose URL is a remote `http(s)` reference →
///    `{"type":"image","source":{"type":"url","url":...}}`
///  - `document_url` (PDF), translated the same way as `image_url` above but
///    into `{"type":"document",...}` blocks — see the Anthropic PDF-support
///    docs (`platform.claude.com/docs/en/build-with-claude/pdf-support`):
///    all active Claude models accept `document` blocks, no beta header
///    required.
///
/// P-LLM2.4: pre-fix, `build_request_body` forwarded the OpenAI-shaped array
/// verbatim as Anthropic `content` — Anthropic's real API recognizes neither
/// `"type":"image_url"` nor `"type":"image_data"`, so every multimodal
/// request built through `MultimodalMessage`/`.multimodal_prompt()` against a
/// Claude model was rejected, despite the model registry declaring
/// `accepts: [text, image]`.
///
/// Unrecognized block kinds are dropped rather than erroring here — the
/// only kinds `MultimodalMessage` can produce that this function doesn't
/// handle are `video_url`/`video_data` (Google-only; see `google.rs`), and
/// `validate_request` already rejects video input for any model — including
/// every Anthropic model — whose `accepts:` doesn't declare `video`, before
/// a request is ever built.
fn openai_content_to_anthropic_content(content: &Value) -> Value {
  let Some(items) = content.as_array() else {
    // Plain string (or any other non-array shape) — pass through unchanged.
    return content.clone();
  };
  let blocks: Vec<Value> = items
    .iter()
    .filter_map(|item| {
      let obj = item.as_object()?;
      match obj.get("type").and_then(Value::as_str)? {
        "text" => {
          let text = obj.get("text").and_then(Value::as_str)?;
          Some(json!({ "type": "text", "text": text }))
        }
        "image_url" => {
          let url = obj.get("image_url").and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(map) => map.get("url").and_then(Value::as_str),
            _ => None,
          })?;
          if let Some((media_type, data)) = super::parse_data_url(url) {
            Some(json!({
              "type": "image",
              "source": { "type": "base64", "media_type": media_type, "data": data }
            }))
          } else {
            Some(json!({
              "type": "image",
              "source": { "type": "url", "url": url }
            }))
          }
        }
        // P-LLM2.4 follow-up: PDF/document input. `MultimodalMessage::
        // to_openai_format` collapses `DocumentData` (base64) into the same
        // `document_url` kind with a `data:` URI, same as image_url/image_data
        // — see `multimodal.rs`'s `MessageContent::DocumentData` handling.
        "document_url" => {
          let url = obj.get("document_url").and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(map) => map.get("url").and_then(Value::as_str),
            _ => None,
          })?;
          if let Some((media_type, data)) = super::parse_data_url(url) {
            Some(json!({
              "type": "document",
              "source": { "type": "base64", "media_type": media_type, "data": data }
            }))
          } else {
            Some(json!({
              "type": "document",
              "source": { "type": "url", "url": url }
            }))
          }
        }
        _ => None,
      }
    })
    .collect();
  Value::Array(blocks)
}

/// Encode a [`ThinkingConfig`] as Anthropic's `thinking` request block.
///
/// Anthropic only accepts `{ type: "enabled", budget_tokens: N }` or
/// `{ type: "disabled" }`. `Auto` maps to `enabled` with the model's
/// default budget — Anthropic uses 5000 tokens when none is specified
/// for Claude 3.7+, but we send an explicit baseline of 4096 so behaviour
/// is reproducible across `Auto` and `Medium`.
pub(crate) fn thinking_config_to_anthropic_value(config: &ThinkingConfig) -> Option<Value> {
  if config.is_disabled() {
    return Some(json!({ "type": "disabled" }));
  }
  let budget = config.to_token_budget().unwrap_or(4096);
  Some(json!({
    "type": "enabled",
    "budget_tokens": budget,
  }))
}

/// Encode a `ToolSpec` as Anthropic's `{ name, description, input_schema }`.
pub(crate) fn tool_spec_to_anthropic_value(spec: &ToolSpec) -> Value {
  json!({
    "name": spec.name,
    "description": spec.description,
    "input_schema": spec.parameters,
  })
}

/// Encode `ToolChoice` as Anthropic's `tool_choice` object.
pub(crate) fn tool_choice_to_anthropic_value(choice: &ToolChoice) -> Value {
  match choice {
    ToolChoice::Auto => json!({"type": "auto"}),
    // Anthropic uses `none` since 2024-11; older revisions reject it. The
    // value is sent verbatim because callers opting into ToolChoice::None
    // know what they're doing.
    ToolChoice::None => json!({"type": "none"}),
    // Anthropic spells this `any` (model must call at least one tool).
    ToolChoice::Required => json!({"type": "any"}),
    ToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
  }
}

/// Pull `tool_use` content blocks out of an Anthropic response and convert
/// them to typed `ToolCallRequest`s.
pub(crate) fn parse_anthropic_tool_use_blocks(
  content: &[AnthropicContent],
) -> Vec<ToolCallRequest> {
  content
    .iter()
    .filter_map(|block| match block {
      AnthropicContent::ToolUse { id, name, input } => Some(ToolCallRequest {
        id: id.clone(),
        name: name.clone(),
        arguments: input.clone(),
      }),
      AnthropicContent::Text { .. } | AnthropicContent::Thinking { .. } => None,
    })
    .collect()
}

/// Concatenate the text from every `thinking` block in an Anthropic
/// response. Returns `None` when no thinking blocks were emitted (the
/// caller's thinking config may have been disabled, the model may not
/// have used the budget, or thinking simply wasn't requested).
pub(crate) fn parse_anthropic_thinking_blocks(content: &[AnthropicContent]) -> Option<String> {
  let joined: String = content
    .iter()
    .filter_map(|block| match block {
      AnthropicContent::Thinking { thinking, .. } => Some(thinking.as_str()),
      AnthropicContent::Text { .. } | AnthropicContent::ToolUse { .. } => None,
    })
    .collect::<Vec<_>>()
    .join("");
  if joined.is_empty() {
    None
  } else {
    Some(joined)
  }
}

#[async_trait]
impl LLMProvider for AnthropicProvider {
  fn name(&self) -> &str {
    "anthropic"
  }

  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse> {
    if request.stream {
      return Err(LLMError::InternalError {
        message: "Use execute_streaming for streaming requests".to_string(),
      });
    }

    let url = format!("{}/v1/messages", self.base_url);
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

    let anthropic_response: AnthropicResponse = response.json().await?;

    // Concatenate all text blocks; tool_use blocks are surfaced via
    // `tool_calls` instead of being stringified into content.
    let content_text = anthropic_response
      .content
      .iter()
      .filter_map(|block| match block {
        AnthropicContent::Text { text } => Some(text.as_str()),
        AnthropicContent::ToolUse { .. } | AnthropicContent::Thinking { .. } => None,
      })
      .collect::<Vec<_>>()
      .join("");

    let mut content = ContentType::Text(content_text);

    let usage = anthropic_response
      .usage
      .clone()
      .map(|u| crate::providers::TokenUsage {
        prompt_tokens: Some(u.input_tokens),
        completion_tokens: Some(u.output_tokens),
        total_tokens: Some(u.input_tokens + u.output_tokens),
      });

    let mut tool_calls = parse_anthropic_tool_use_blocks(&anthropic_response.content);

    // V2.1: unwind `build_request_body`'s synthetic-tool-call schema
    // emulation — if this request forced the schema tool (no real tools
    // requested, non-streaming, JsonSchema format), pull its arguments
    // back out as plain-text JSON content instead of surfacing it as a
    // tool call the caller never asked for.
    if request.tools.is_none()
      && let Some(ResponseFormat::JsonSchema {
        name: schema_name, ..
      }) = &request.response_format
      && let Some(pos) = tool_calls.iter().position(|call| &call.name == schema_name)
    {
      let call = tool_calls.remove(pos);
      content = ContentType::Text(serde_json::to_string(&call.arguments).unwrap_or_default());
    }

    let stop_reason = anthropic_response
      .stop_reason
      .as_deref()
      .map(StopReason::from_anthropic_stop_reason);
    // Anthropic claude-3.7+ emits `type: "thinking"` content blocks
    // alongside `text` blocks when extended thinking is enabled. Surface
    // their concatenated text on the typed channel.
    let thinking = parse_anthropic_thinking_blocks(&anthropic_response.content);

    Ok(ProviderResponse {
      content,
      usage,
      metadata: Some(serde_json::to_value(&anthropic_response)?),
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

    let url = format!("{}/v1/messages", self.base_url);
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

    Ok(Box::new(AnthropicStreamingResponse::new(response)))
  }

  async fn validate_config(&self) -> Result<()> {
    // Simple validation by trying to create a minimal request
    let test_body = json!({
      "model": "claude-3-haiku-20240307",
      "messages": [{"role": "user", "content": "Hi"}],
      "max_tokens": 1
    });

    let url = format!("{}/v1/messages", self.base_url);
    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&test_body)
      .send()
      .await?;

    if response.status().as_u16() == 401 {
      return Err(LLMError::AuthenticationError {
        provider: "anthropic".to_string(),
        message: "Invalid API key".to_string(),
      });
    }

    // Any other error is likely a configuration issue, but auth is valid
    Ok(())
  }

  fn base_url(&self) -> &str {
    &self.base_url
  }

  fn supported_models(&self) -> Vec<String> {
    vec![
      "claude-3-5-sonnet-20241022".to_string(),
      "claude-3-5-sonnet-20240620".to_string(),
      "claude-3-5-haiku-20241022".to_string(),
      "claude-3-opus-20240229".to_string(),
      "claude-3-sonnet-20240229".to_string(),
      "claude-3-haiku-20240307".to_string(),
    ]
  }
}

// Anthropic API response structures
#[derive(Debug, Deserialize, Serialize)]
struct AnthropicResponse {
  id: String,
  #[serde(rename = "type")]
  type_field: String,
  role: String,
  content: Vec<AnthropicContent>,
  model: String,
  stop_reason: Option<String>,
  stop_sequence: Option<String>,
  usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum AnthropicContent {
  #[serde(rename = "text")]
  Text { text: String },
  #[serde(rename = "tool_use")]
  ToolUse {
    id: String,
    name: String,
    input: Value,
  },
  /// Extended-thinking block emitted by Claude 3.7+ when `thinking: {
  /// type: "enabled" }` is set on the request. Carries the model's chain
  /// of thought; the `signature` is opaque and used by Anthropic for
  /// integrity verification on multi-turn replays. We don't act on the
  /// signature today but accept the field so deserialisation succeeds.
  #[serde(rename = "thinking")]
  Thinking {
    thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
  },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AnthropicUsage {
  input_tokens: u32,
  output_tokens: u32,
}

// Streaming response structures
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AnthropicStreamingEvent {
  #[serde(rename = "type")]
  event_type: String,
  data: Option<Value>,
}

pub struct AnthropicStreamingResponse {
  stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
  buffer: LineBuffer,
  finished: bool,
}

// Make it Send + Sync
// Q2.5.4: `unsafe impl Send + Sync` removed (trait no longer needs Sync).

impl AnthropicStreamingResponse {
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

  fn parse_sse_event(line: &str) -> Option<StreamChunk> {
    if line.starts_with("event: ") {
      return None; // Event type line, not data
    }

    if !line.starts_with("data: ") {
      return None;
    }

    let data = &line[6..]; // Remove "data: " prefix

    if let Ok(event) = serde_json::from_str::<Value>(data)
      && let Some(event_type) = event.get("type").and_then(|t| t.as_str())
    {
      match event_type {
        "content_block_start" => {
          // Q2.5.2: tool_use blocks emit their `id` and `name` here, before any
          // `input_json_delta`. Surface them as a ToolCallDelta with empty args
          // so downstream consumers learn the tool_call exists.
          if let Some(block) = event.get("content_block")
            && block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
          {
            let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let id = block.get("id").and_then(|i| i.as_str()).map(String::from);
            let name = block.get("name").and_then(|n| n.as_str()).map(String::from);
            return Some(StreamChunk {
              media: Vec::new(),
              content: String::new(),
              is_final: false,
              metadata: Some(event.clone()),
              usage: None,
              content_type: Some("tool_use".to_string()),
              tool_call_deltas: vec![ToolCallDelta {
                index,
                id,
                name,
                arguments_delta: None,
              }],
            });
          }
        }
        "content_block_delta" => {
          if let Some(delta) = event.get("delta") {
            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            // Q2.5.2: `input_json_delta` carries partial JSON for a tool_use
            // block's `input` field. Concatenate per index downstream.
            if delta_type == "input_json_delta"
              && let Some(partial) = delta.get("partial_json").and_then(|p| p.as_str())
            {
              let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
              return Some(StreamChunk {
                media: Vec::new(),
                content: String::new(),
                is_final: false,
                metadata: Some(event.clone()),
                usage: None,
                content_type: Some("tool_use".to_string()),
                tool_call_deltas: vec![ToolCallDelta {
                  index,
                  id: None,
                  name: None,
                  arguments_delta: Some(partial.to_string()),
                }],
              });
            }
            // Existing path: text_delta on a text block.
            if let Some(text) = delta.get("text")
              && let Some(text_str) = text.as_str()
            {
              return Some(StreamChunk {
                media: Vec::new(),
                content: text_str.to_string(),
                is_final: false,
                metadata: Some(event.clone()),
                usage: None,
                content_type: Some("text".to_string()),
                tool_call_deltas: Vec::new(),
              });
            }
          }
        }
        "message_stop" => {
          // Q2.5.1: Anthropic emits `message_stop` exactly once at the end of the
          // entire response. `content_block_stop` fires after each block (text,
          // tool_use, …) and previously terminated the stream early, dropping
          // every block after the first one. Now only `message_stop` ends it.
          return Some(StreamChunk {
            media: Vec::new(),
            content: String::new(),
            is_final: true,
            metadata: Some(event.clone()),
            usage: None,
            content_type: Some("text".to_string()),
            tool_call_deltas: Vec::new(),
          });
        }
        _ => {}
      }
    }

    None
  }
}

#[async_trait]
impl StreamingResponse for AnthropicStreamingResponse {
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
    if self.finished {
      return Ok(None);
    }

    loop {
      // W0.3: drain any complete lines already sitting in the buffer
      // *before* pulling more bytes off the network — see `openai.rs`'s
      // `OpenAIStreamingResponse::next_chunk` (same bug, same fix) and
      // `stepfun.rs` for the original reference fix. Draining only
      // inside the `Some(Ok(data))` arm and returning on the first
      // parsed line stranded every subsequent buffered line until
      // another network read happened to re-trigger the drain; if the
      // stream had already ended, that line (e.g. the terminal
      // `message_stop` event) was silently dropped.
      {
        while let Some(line) = self.buffer.next_line() {
          if !line.is_empty()
            && let Some(chunk) = Self::parse_sse_event(&line)
          {
            if chunk.is_final {
              self.finished = true;
            }
            return Ok(Some(chunk));
          }
        }
      }

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

  #[test]
  fn test_anthropic_provider_creation() {
    let provider = AnthropicProvider::new("test-key", None);
    assert!(provider.is_ok());

    let provider = AnthropicProvider::new("", None);
    assert!(provider.is_err());
  }

  #[tokio::test]
  async fn build_headers_injects_traceparent_when_scope_active() {
    use crate::trace_context::{LlmTraceContext, scope};

    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { provider.build_headers() })
      .await
      .expect("headers must build under a well-formed ASCII API key");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
  }

  /// Q2.5.3 regression: an API key with a stray non-ASCII byte
  /// (typical `.env` mistake — trailing `\n` from copy-paste)
  /// surfaces as `ConfigurationError` instead of panicking.
  #[test]
  fn build_headers_returns_err_on_invalid_api_key() {
    let provider = AnthropicProvider::new("bad\nkey", None).unwrap();
    let err = provider
      .build_headers()
      .expect_err("newline-in-key must fail closed");
    match err {
      LLMError::ConfigurationError { message } => {
        assert!(
          message.contains("invalid characters") || message.contains("API key"),
          "expected configuration error, got: {message}"
        );
      }
      other => panic!("expected ConfigurationError, got {other:?}"),
    }
  }

  #[test]
  fn test_build_request_body() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("temperature".to_string(), json!(0.7));
    params.insert("max_tokens".to_string(), json!(100));

    let request = ProviderRequest {
      model: "claude-3-sonnet-20240229".to_string(),
      messages: vec![
        json!({"role": "system", "content": "You are helpful"}),
        json!({"role": "user", "content": "test"}),
      ],
      stream: false,
      parameters: params,
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert_eq!(body["model"], "claude-3-sonnet-20240229");
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["system"], "You are helpful");
    assert_eq!(body["messages"].as_array().unwrap().len(), 1); // Only user message
    assert!(body.get("tools").is_none());
  }

  /// P-LLM2.4 regression: a real `MultimodalMessage` (via
  /// `to_openai_format()`) built through the public builder API must
  /// translate into Anthropic-native `image` content blocks, not pass
  /// through the OpenAI-shaped `image_url`/`image_data` tags verbatim
  /// (which the real Anthropic API rejects outright).
  #[test]
  fn build_request_body_translates_multimodal_content_to_anthropic_blocks() {
    use crate::multimodal::MultimodalMessage;

    let provider = AnthropicProvider::new("test-key", None).unwrap();

    let remote = MultimodalMessage::user()
      .add_text("what is this")
      .add_image_url("https://example.com/cat.jpg")
      .build()
      .to_openai_format();
    let base64 = MultimodalMessage::user()
      .add_image_data("aGVsbG8=", "image/png")
      .build()
      .to_openai_format();

    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![remote, base64],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);

    let remote_blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(remote_blocks[0]["type"], "text");
    assert_eq!(remote_blocks[0]["text"], "what is this");
    assert_eq!(remote_blocks[1]["type"], "image");
    assert_eq!(remote_blocks[1]["source"]["type"], "url");
    assert_eq!(
      remote_blocks[1]["source"]["url"],
      "https://example.com/cat.jpg"
    );
    assert!(remote_blocks[1].get("image_url").is_none());

    let base64_blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(base64_blocks[0]["type"], "image");
    assert_eq!(base64_blocks[0]["source"]["type"], "base64");
    assert_eq!(base64_blocks[0]["source"]["media_type"], "image/png");
    assert_eq!(base64_blocks[0]["source"]["data"], "aGVsbG8=");
  }

  /// P-LLM2.4 regression: a system message whose `content` is an array
  /// (because it went through `MultimodalMessage`) must still surface its
  /// text — pre-fix, `content.as_str()` returned `None` for array content
  /// and the entire system prompt vanished from the request.
  #[test]
  fn build_request_body_flattens_multimodal_system_message_text() {
    use crate::multimodal::MultimodalMessage;

    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let system = MultimodalMessage::system()
      .add_text("Be concise.")
      .build()
      .to_openai_format();

    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![system, json!({"role": "user", "content": "hi"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert_eq!(body["system"], "Be concise.");
  }

  /// P-LLM2.4 follow-up: a `MultimodalMessage` built with `.add_document_url()`
  /// / `.add_document_data()` must translate into Anthropic-native `document`
  /// content blocks, mirroring the `image` translation above.
  #[test]
  fn build_request_body_translates_document_content_to_anthropic_blocks() {
    use crate::multimodal::MultimodalMessage;

    let provider = AnthropicProvider::new("test-key", None).unwrap();

    let remote = MultimodalMessage::user()
      .add_text("what are the key findings")
      .add_document_url("https://example.com/report.pdf")
      .build()
      .to_openai_format();
    let base64 = MultimodalMessage::user()
      .add_document_data("aGVsbG8=", "application/pdf")
      .build()
      .to_openai_format();

    let request = ProviderRequest {
      model: "claude-opus-5".to_string(),
      messages: vec![remote, base64],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);

    let remote_blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(remote_blocks[0]["type"], "text");
    assert_eq!(remote_blocks[1]["type"], "document");
    assert_eq!(remote_blocks[1]["source"]["type"], "url");
    assert_eq!(
      remote_blocks[1]["source"]["url"],
      "https://example.com/report.pdf"
    );
    assert!(remote_blocks[1].get("document_url").is_none());

    let base64_blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(base64_blocks[0]["type"], "document");
    assert_eq!(base64_blocks[0]["source"]["type"], "base64");
    assert_eq!(base64_blocks[0]["source"]["media_type"], "application/pdf");
    assert_eq!(base64_blocks[0]["source"]["data"], "aGVsbG8=");
  }

  #[test]
  fn build_request_body_serialises_tools() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
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
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![json!({"role": "user", "content": "weather?"})],
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
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["input_schema"]["required"][0], "city");
    // Anthropic spells Required as "any".
    assert_eq!(body["tool_choice"]["type"], "any");
  }

  #[test]
  fn tool_choice_specific_uses_tool_type() {
    let body = tool_choice_to_anthropic_value(&ToolChoice::Tool {
      name: "x".to_string(),
    });
    assert_eq!(body["type"], "tool");
    assert_eq!(body["name"], "x");
  }

  #[test]
  fn parse_anthropic_tool_use_blocks_extracts_calls() {
    let raw = json!({
      "id": "msg_x",
      "type": "message",
      "role": "assistant",
      "model": "claude-3-5-sonnet-20241022",
      "content": [
        {"type": "text", "text": "I'll check the weather."},
        {
          "type": "tool_use",
          "id": "toolu_abc",
          "name": "get_weather",
          "input": {"city": "Tokyo"}
        }
      ],
      "stop_reason": "tool_use",
      "stop_sequence": null,
      "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let parsed: AnthropicResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.stop_reason.as_deref(), Some("tool_use"));
    let tool_calls = parse_anthropic_tool_use_blocks(&parsed.content);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "toolu_abc");
    assert_eq!(tool_calls[0].name, "get_weather");
    assert_eq!(tool_calls[0].arguments["city"], "Tokyo");
  }

  #[test]
  fn parse_anthropic_text_only_returns_no_tool_calls() {
    let content = vec![AnthropicContent::Text {
      text: "hello".to_string(),
    }];
    assert!(parse_anthropic_tool_use_blocks(&content).is_empty());
  }

  // Q2.5.1: `content_block_stop` must NOT terminate the stream; only
  // `message_stop` may. Multi-block responses (text + tool_use) were
  // truncated to the first block before the fix.
  #[test]
  fn streaming_content_block_stop_does_not_finalize() {
    let chunk = AnthropicStreamingResponse::parse_sse_event(
      "data: {\"type\":\"content_block_stop\",\"index\":0}",
    );
    assert!(
      chunk.is_none(),
      "content_block_stop should be ignored, got {chunk:?}"
    );
  }

  #[test]
  fn streaming_message_stop_does_finalize() {
    let chunk =
      AnthropicStreamingResponse::parse_sse_event("data: {\"type\":\"message_stop\"}").unwrap();
    assert!(chunk.is_final);
  }

  // Q2.5.2: tool_use blocks stream their identity via `content_block_start`
  // and their JSON arguments via `input_json_delta` chunks. The parser must
  // surface both as `ToolCallDelta` entries so concatenated `arguments_delta`
  // values reconstruct the full argument JSON.
  #[test]
  fn streaming_tool_use_block_start_emits_id_and_name() {
    let chunk = AnthropicStreamingResponse::parse_sse_event(
      "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"get_weather\",\"input\":{}}}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 1);
    assert_eq!(delta.id.as_deref(), Some("toolu_abc"));
    assert_eq!(delta.name.as_deref(), Some("get_weather"));
    assert!(delta.arguments_delta.is_none());
    assert!(!chunk.is_final);
  }

  #[test]
  fn streaming_input_json_delta_emits_arguments_fragment() {
    let chunk = AnthropicStreamingResponse::parse_sse_event(
      "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}",
    ).unwrap();
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 1);
    assert_eq!(delta.arguments_delta.as_deref(), Some("{\"city\":"));
    assert!(delta.id.is_none());
    assert!(delta.name.is_none());
  }

  // ----- Thinking serialisation + parse coverage -----

  /// `ThinkingConfig::Medium` must be emitted as Anthropic's typed
  /// `thinking: { type: "enabled", budget_tokens: 4096 }` block on the
  /// request body — bypassing the parameters whitelist that would
  /// otherwise silently drop it.
  #[test]
  fn build_request_body_serialises_thinking_medium() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "claude-3-7-sonnet-20250219".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Medium),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 4096);
  }

  #[test]
  fn build_request_body_thinking_disabled_emits_disabled_type() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "claude-3-7-sonnet-20250219".to_string(),
      messages: vec![json!({"role": "user", "content": "no thinking"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Disabled),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body["thinking"].get("budget_tokens").is_none());
  }

  #[test]
  fn build_request_body_no_thinking_omits_the_block() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![json!({"role": "user", "content": "hi"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert!(
      body.get("thinking").is_none(),
      "no thinking config → no `thinking` key on the wire body"
    );
  }

  #[test]
  fn parse_anthropic_thinking_blocks_concatenates_when_present() {
    let raw = json!({
      "id": "msg_x",
      "type": "message",
      "role": "assistant",
      "model": "claude-sonnet-4-20250514",
      "content": [
        {"type": "thinking", "thinking": "Step 1: ", "signature": "sig_abc"},
        {"type": "thinking", "thinking": "Step 2 done."},
        {"type": "text", "text": "Final answer."}
      ],
      "stop_reason": "end_turn",
      "stop_sequence": null,
      "usage": {"input_tokens": 12, "output_tokens": 8}
    });
    let parsed: AnthropicResponse = serde_json::from_value(raw).unwrap();
    let thinking = parse_anthropic_thinking_blocks(&parsed.content);
    assert_eq!(thinking.as_deref(), Some("Step 1: Step 2 done."));
  }

  #[test]
  fn parse_anthropic_thinking_blocks_returns_none_when_absent() {
    let content = vec![AnthropicContent::Text {
      text: "no reasoning here".to_string(),
    }];
    assert!(parse_anthropic_thinking_blocks(&content).is_none());
  }

  fn json_schema_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
      name: "final_answer".to_string(),
      schema: json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
      }),
      strict: Some(true),
    }
  }

  /// V2.1: with no real tools requested, a `JsonSchema` format forces a
  /// single synthetic tool call whose `input_schema` is the requested
  /// schema — Anthropic's only way to constrain output shape natively.
  #[test]
  fn build_request_body_forces_synthetic_tool_for_json_schema_without_tools() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![json!({"role": "user", "content": "answer please"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: Some(json_schema_format()),
    };
    let body = provider.build_request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "final_answer");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    assert_eq!(
      body["tool_choice"],
      json!({"type": "tool", "name": "final_answer"})
    );
  }

  /// When real tools are already requested, forcing a tool choice would
  /// hijack the caller's own tool-calling turn — the schema emulation must
  /// stay out of the way and leave the real tools/tool_choice untouched.
  #[test]
  fn build_request_body_skips_synthetic_tool_when_real_tools_present() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new(
      "get_weather",
      "Return the weather",
      json!({"type": "object"}),
    );
    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![json!({"role": "user", "content": "weather?"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: Some(ToolChoice::Auto),
      thinking: None,
      response_format: Some(json_schema_format()),
    };
    let body = provider.build_request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(body["tool_choice"], json!({"type": "auto"}));
  }

  /// Streamed requests never get the synthetic-tool treatment — unwrapping
  /// a forced tool call out of an SSE stream isn't implemented, so a
  /// streaming `JsonSchema` request must fall back to prompt-only
  /// constraint rather than silently breaking the stream.
  #[test]
  fn build_request_body_skips_synthetic_tool_when_streaming() {
    let provider = AnthropicProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "claude-3-5-sonnet-20241022".to_string(),
      messages: vec![json!({"role": "user", "content": "answer please"})],
      stream: true,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: Some(json_schema_format()),
    };
    let body = provider.build_request_body(&request);
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
  }

  #[test]
  fn streaming_text_delta_remains_unaffected() {
    let chunk = AnthropicStreamingResponse::parse_sse_event(
      "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
    ).unwrap();
    assert_eq!(chunk.content, "hi");
    assert!(chunk.tool_call_deltas.is_empty());
  }
}
