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
  tool_calling::StopReason,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::pin::Pin;
use tokio_stream::Stream;

pub struct MoonshotProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl MoonshotProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`]. See
  /// [`crate::providers::OpenAIProvider::with_client`] for the rationale.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "moonshot".to_string(),
      });
    }

    let base_url = base_url.unwrap_or_else(|| "https://api.moonshot.cn/v1".to_string());

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
          message: format!("Moonshot API key contains invalid characters: {err}"),
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

    // Add additional parameters
    for (key, value) in &request.parameters {
      body[key] = value.clone();
    }

    // Moonshot speaks the OpenAI tools wire format directly.
    if let Some(tools) = &request.tools {
      body["tools"] = Value::Array(tools.iter().map(tool_spec_to_openai_value).collect());
    }
    if let Some(choice) = &request.tool_choice {
      body["tool_choice"] = tool_choice_to_openai_value(choice);
    }
    if let Some(format) = &request.response_format {
      body["response_format"] = response_format_to_openai_value(format);
    }

    body
  }
}

#[async_trait]
impl LLMProvider for MoonshotProvider {
  fn name(&self) -> &str {
    "moonshot"
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

    let moonshot_response: MoonshotResponse = response.json().await?;

    let content_text = moonshot_response
      .choices
      .first()
      .and_then(|choice| choice.message.content.as_ref())
      .unwrap_or(&String::new())
      .clone();

    // Convert to ContentType - Moonshot currently only returns text
    let content = ContentType::Text(content_text);

    let usage = moonshot_response
      .usage
      .clone()
      .map(|u| crate::providers::TokenUsage {
        prompt_tokens: Some(u.prompt_tokens),
        completion_tokens: Some(u.completion_tokens),
        total_tokens: Some(u.total_tokens),
      });

    let first_choice = moonshot_response.choices.first();
    let tool_calls = first_choice
      .and_then(|c| c.message.tool_calls.as_ref())
      .map(parse_openai_tool_calls)
      .unwrap_or_default();
    let stop_reason = first_choice
      .and_then(|c| c.finish_reason.as_deref())
      .map(StopReason::from_openai_finish_reason);

    Ok(ProviderResponse {
      content,
      usage,
      metadata: Some(serde_json::to_value(&moonshot_response)?),
      tool_calls,
      stop_reason,
      thinking: None,
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

    Ok(Box::new(MoonshotStreamingResponse::new(response)))
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
        provider: "moonshot".to_string(),
        message: "Failed to authenticate with Moonshot API".to_string(),
      });
    }

    Ok(())
  }

  fn base_url(&self) -> &str {
    &self.base_url
  }

  fn supported_models(&self) -> Vec<String> {
    vec!["kimi-k2.6".to_string()]
  }
}

// Moonshot API response structures (similar to OpenAI format)
#[derive(Debug, Deserialize, Serialize)]
struct MoonshotResponse {
  id: String,
  object: String,
  created: u64,
  model: String,
  choices: Vec<MoonshotChoice>,
  usage: Option<MoonshotUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MoonshotChoice {
  index: u32,
  message: MoonshotMessage,
  finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MoonshotMessage {
  role: String,
  content: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  tool_calls: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct MoonshotUsage {
  prompt_tokens: u32,
  completion_tokens: u32,
  total_tokens: u32,
}

// Streaming response structures
#[derive(Debug, Deserialize, Serialize)]
struct MoonshotStreamingChunk {
  id: String,
  object: String,
  created: u64,
  model: String,
  choices: Vec<MoonshotStreamingChoice>,
  usage: Option<MoonshotUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MoonshotStreamingChoice {
  index: u32,
  delta: MoonshotStreamingDelta,
  finish_reason: Option<String>,
  /// Y2.1: Kimi puts the stream's usage on the finish chunk's choice,
  /// not at the top level where the OpenAI shape has it.
  #[serde(default)]
  usage: Option<MoonshotUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MoonshotStreamingDelta {
  role: Option<String>,
  content: Option<String>,
  /// P-LLM2.5: Moonshot speaks the identical OpenAI-compatible
  /// `delta.tool_calls[]` streaming shape — see
  /// `openai::OpenAIStreamingToolCall`'s doc comment.
  #[serde(default)]
  tool_calls: Option<Vec<OpenAIStreamingToolCall>>,
  /// Y10.3: Kimi thinking models stream their reasoning here.
  #[serde(default)]
  reasoning_content: Option<String>,
}

pub struct MoonshotStreamingResponse {
  stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
  buffer: LineBuffer,
  finished: bool,
}

// Q2.5.4: `unsafe impl Send + Sync` removed (trait no longer needs Sync).

impl MoonshotStreamingResponse {
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

    if let Ok(chunk) = serde_json::from_str::<MoonshotStreamingChunk>(data)
      && let Some(choice) = chunk.choices.first()
    {
      let content_text = choice.delta.content.clone().unwrap_or_default();
      let tool_call_deltas = openai_streaming_tool_call_deltas(choice.delta.tool_calls.as_deref());
      let thinking = choice.delta.reasoning_content.clone().unwrap_or_default();

      // P-LLM2.5: previously this whole branch required `delta.content` to
      // be present (`&& let Some(content) = &choice.delta.content`), so a
      // tool-call-only delta (no text) never even reached the (also
      // previously hardcoded-empty) `tool_call_deltas` — it fell all the
      // way through to `None` and was silently dropped. Emit a chunk when
      // there is *any* signal, mirroring the same fix in `openai.rs`.
      let has_signal = !content_text.is_empty()
        || !thinking.is_empty()
        || !tool_call_deltas.is_empty()
        || choice.finish_reason.is_some()
        || chunk.usage.is_some()
        || choice.usage.is_some();
      if !has_signal {
        return None;
      }

      return Some(StreamChunk {
        thinking,
        media: Vec::new(),
        content: content_text,
        is_final: choice.finish_reason.is_some(),
        metadata: Some(serde_json::to_value(&chunk).ok()?),
        usage: chunk
          .usage
          .as_ref()
          .or(choice.usage.as_ref())
          .map(|u| TokenUsage {
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
impl StreamingResponse for MoonshotStreamingResponse {
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
      // `[DONE]`) was silently dropped and `is_final` was never
      // observed from a real sentinel.
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
  fn streaming_reasoning_content_is_surfaced_as_thinking() {
    let chunk = MoonshotStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"kimi-k2-thinking\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm\"},\"finish_reason\":null}]}",
    )
    .expect("reasoning chunk");
    assert_eq!(chunk.thinking, "hmm");
    assert!(chunk.content.is_empty());
  }

  #[test]
  fn test_moonshot_provider_creation() {
    let provider = MoonshotProvider::new("test-key", None);
    assert!(provider.is_ok());

    let provider = MoonshotProvider::new("", None);
    assert!(provider.is_err());
  }

  #[tokio::test]
  async fn build_headers_injects_traceparent_when_scope_active() {
    use crate::trace_context::{LlmTraceContext, scope};

    let provider = MoonshotProvider::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { provider.build_headers() })
      .await
      .expect("ASCII key builds cleanly");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
  }

  #[test]
  fn test_build_request_body() {
    let provider = MoonshotProvider::new("test-key", None).unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("temperature".to_string(), json!(0.7));
    params.insert("max_tokens".to_string(), json!(100));

    let request = ProviderRequest {
      model: "moonshot-v1-8k".to_string(),
      messages: vec![json!({"role": "user", "content": "test"})],
      stream: false,
      parameters: params,
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert_eq!(body["model"], "moonshot-v1-8k");
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["stream"], false);
    assert!(body.get("tools").is_none());
  }

  #[test]
  fn build_request_body_passes_tools_through_openai_format() {
    use crate::tool_calling::{ToolChoice, ToolSpec};
    let provider = MoonshotProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new("ping", "Ping a host", json!({"type": "object"}));
    let request = ProviderRequest {
      model: "moonshot-v1-8k".to_string(),
      messages: vec![],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: Some(ToolChoice::Auto),
      thinking: None,
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools[0]["function"]["name"], "ping");
    assert_eq!(body["tool_choice"], "auto");
  }

  #[test]
  fn test_supported_models() {
    let provider = MoonshotProvider::new("test-key", None).unwrap();
    let models = provider.supported_models();
    assert!(models.contains(&"kimi-k2.6".to_string()));
    assert!(!models.iter().any(|m| m.starts_with("moonshot-v1")));
  }

  /// P-LLM2.5 regression: Moonshot speaks the identical OpenAI-compatible
  /// `delta.tool_calls[]` streaming shape — mirrors
  /// `openai::tests::streaming_tool_call_delta_carries_id_and_name`.
  #[test]
  fn streaming_tool_call_delta_carries_id_and_name() {
    let chunk = MoonshotStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"moonshot-v1-8k\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
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
    let chunk = MoonshotStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"moonshot-v1-8k\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}",
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
    let chunk = MoonshotStreamingResponse::parse_sse_chunk(
      "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"moonshot-v1-8k\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"noop\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
    );
    assert!(
      chunk.is_some(),
      "a tool-call-only delta (no text content) must not be dropped"
    );
    assert_eq!(chunk.unwrap().content, "");
  }
}
