//! Mock LLM Provider for Testing
//!
//! This provider simulates LLM responses without making actual API calls.
//! Useful for:
//! - Unit and integration testing without API keys
//! - Workflow validation and debugging
//! - Performance testing and benchmarking
//! - Development without network connectivity

use super::{ContentType, LLMProvider, ProviderRequest, ProviderResponse, TokenUsage};
use crate::client::streaming::{StreamChunk, StreamingResponse, ToolCallDelta};
use crate::tool_calling::{StopReason, ToolCallRequest};
use crate::{LLMError, Result};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Mock LLM provider for testing
#[derive(Debug, Clone)]
pub struct MockProvider {
  /// Pre-configured response text
  response_text: Option<String>,
  /// Optional response queue consumed one item per request.
  response_queue: Arc<Mutex<VecDeque<String>>>,
  /// Response delay in milliseconds (simulates network latency)
  delay_ms: u64,
  /// Whether to simulate an error
  simulate_error: bool,
  /// Tool calls to surface on the next response (consumed FIFO).
  ///
  /// Each entry is a vector of tool calls returned for a single request.
  /// When this queue is non-empty, `stop_reason` is set to `ToolCalls`,
  /// matching native-provider behaviour. Used by ReAct/Plan-Execute
  /// fallback tests to drive the typed tool-calling path without a real
  /// network round-trip.
  tool_call_queue: Arc<Mutex<VecDeque<Vec<ToolCallRequest>>>>,
  /// V1.5: errors to return (FIFO, one per call) before falling through
  /// to the normal response/tool-call path. Lets tests simulate a
  /// provider that fails transiently (429/5xx) before succeeding,
  /// exercising `LLMClient`'s retry-with-backoff path end to end.
  error_queue: Arc<Mutex<VecDeque<LLMError>>>,
}

impl MockProvider {
  /// Create a new mock provider with default settings
  pub fn new(_api_key: &str, _base_url: Option<String>) -> Result<Self> {
    Ok(Self {
      response_text: std::env::var("YANSHI_MOCK_RESPONSE").ok(),
      response_queue: Arc::new(Mutex::new(load_response_queue_from_env())),
      // `YANSHI_MOCK_DELAY_MS` lets env-driven tests (which build the mock
      // through the model registry, not the `with_delay` builder) simulate a
      // slow round-trip — used to characterize the runtime's timeout /
      // cancellation racing paths deterministically.
      delay_ms: std::env::var("YANSHI_MOCK_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0),
      simulate_error: false,
      tool_call_queue: Arc::new(Mutex::new(load_tool_call_queue_from_env())),
      error_queue: Arc::new(Mutex::new(load_error_queue_from_env())),
    })
  }

  /// Queue a single batch of tool calls to be surfaced on the next request.
  ///
  /// Tests use this to drive the native tool-calling code path through the
  /// Mock provider without making a real network call. Calling this multiple
  /// times queues additional batches in FIFO order.
  pub fn with_tool_calls(self, calls: Vec<ToolCallRequest>) -> Self {
    if let Ok(mut queue) = self.tool_call_queue.lock() {
      queue.push_back(calls);
    }
    self
  }

  /// Create a mock provider with custom response
  pub fn with_response(mut self, text: impl Into<String>) -> Self {
    self.response_text = Some(text.into());
    self
  }

  /// Set response delay in milliseconds
  pub fn with_delay(mut self, delay_ms: u64) -> Self {
    self.delay_ms = delay_ms;
    self
  }

  /// Configure to simulate an error
  pub fn with_error(mut self) -> Self {
    self.simulate_error = true;
    self
  }

  /// V1.5: queue errors to return (FIFO, one per call) before falling
  /// through to a normal response. Used by tests to simulate a
  /// provider recovering from a transient failure (e.g. a 429
  /// rate-limit) so the client-level retry path can be exercised
  /// end to end.
  pub fn with_queued_errors(self, errors: Vec<LLMError>) -> Self {
    if let Ok(mut queue) = self.error_queue.lock() {
      queue.extend(errors);
    }
    self
  }

  /// Generate a default response based on the request
  fn generate_default_response(&self, request: &ProviderRequest) -> String {
    let first_message = request
      .messages
      .first()
      .and_then(|m| m.get("content"))
      .and_then(|c| c.as_str())
      .unwrap_or("unknown");

    format!(
      "Mock response for: '{}'... (model: {})",
      first_message.chars().take(50).collect::<String>(),
      request.model
    )
  }

  fn next_response(&self, request: &ProviderRequest) -> String {
    if let Ok(mut queue) = self.response_queue.lock()
      && let Some(response) = queue.pop_front()
    {
      return response;
    }

    self
      .response_text
      .clone()
      .unwrap_or_else(|| self.generate_default_response(request))
  }
}

/// Load `YANSHI_MOCK_ERROR_STATUS_CODES` as a queue of `HttpError`s.
///
/// Format: JSON array of HTTP status codes, e.g. `[429, 429]` — each
/// consumed FIFO on subsequent requests before the mock falls through to
/// its normal response. Mirrors `YANSHI_MOCK_RESPONSES`'s env-driven
/// pattern so registry-constructed providers (which only see the
/// constructor, not the builder methods) can also be seeded, e.g. by an
/// end-to-end retry test.
fn load_error_queue_from_env() -> VecDeque<LLMError> {
  let Ok(raw) = std::env::var("YANSHI_MOCK_ERROR_STATUS_CODES") else {
    return VecDeque::new();
  };
  match serde_json::from_str::<Vec<u16>>(&raw) {
    Ok(codes) => codes
      .into_iter()
      .map(|status_code| LLMError::HttpError {
        status_code,
        message: "mock simulated transient error".to_string(),
      })
      .collect(),
    Err(_) => VecDeque::new(),
  }
}

fn load_response_queue_from_env() -> VecDeque<String> {
  let Ok(raw) = std::env::var("YANSHI_MOCK_RESPONSES") else {
    return VecDeque::new();
  };

  match serde_json::from_str::<Vec<String>>(&raw) {
    Ok(responses) => responses.into(),
    Err(_) => VecDeque::new(),
  }
}

/// Load `YANSHI_MOCK_TOOL_CALLS` as a queue of tool-call batches.
///
/// Format: JSON `Vec<Vec<ToolCallRequest>>`. Each outer entry is consumed
/// FIFO on subsequent requests; an empty inner vec means "no tool calls
/// for this request" (the model emits plain text instead). Matches the
/// `YANSHI_MOCK_RESPONSES` pattern for consistency in agent tests.
fn load_tool_call_queue_from_env() -> VecDeque<Vec<ToolCallRequest>> {
  let Ok(raw) = std::env::var("YANSHI_MOCK_TOOL_CALLS") else {
    return VecDeque::new();
  };
  match serde_json::from_str::<Vec<Vec<ToolCallRequest>>>(&raw) {
    Ok(batches) => batches.into(),
    Err(_) => VecDeque::new(),
  }
}

/// Mock streaming response.
///
/// V2.2: splits `content` into multiple small chunks instead of a single
/// `is_final: true` chunk, so tests exercise a genuine multi-delta
/// sequence (the whole point of streaming) rather than a degenerate
/// one-chunk stream. Also surfaces any queued tool calls (mirroring
/// `MockProvider::execute`'s `tool_call_queue` consumption) — required
/// once a caller's only LLM entry point is the streaming path.
pub struct MockStreamingResponse {
  chunks: VecDeque<String>,
  tool_calls: Vec<ToolCallRequest>,
  usage: TokenUsage,
  final_sent: bool,
}

impl MockStreamingResponse {
  /// Chunk granularity, in characters (not bytes, so multi-byte UTF-8
  /// content is never split mid-codepoint). Concatenating every chunk's
  /// `content` in order reconstructs the original string exactly — unlike
  /// a naive `split_whitespace` scheme, no whitespace is lost or
  /// normalized.
  const CHARS_PER_CHUNK: usize = 4;

  fn new(content: String, tool_calls: Vec<ToolCallRequest>, usage: TokenUsage) -> Self {
    let chars: Vec<char> = content.chars().collect();
    let chunks = chars
      .chunks(Self::CHARS_PER_CHUNK)
      .map(|piece| piece.iter().collect::<String>())
      .collect();
    Self {
      chunks,
      tool_calls,
      usage,
      final_sent: false,
    }
  }
}

#[async_trait]
impl StreamingResponse for MockStreamingResponse {
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
    if let Some(piece) = self.chunks.pop_front() {
      return Ok(Some(StreamChunk {
        media: Vec::new(),
        content: piece,
        is_final: false,
        metadata: None,
        usage: None,
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      }));
    }

    if self.final_sent {
      return Ok(None);
    }
    self.final_sent = true;

    let tool_call_deltas = self
      .tool_calls
      .iter()
      .enumerate()
      .map(|(index, call)| ToolCallDelta {
        index: index as u32,
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        arguments_delta: Some(call.arguments.to_string()),
      })
      .collect();

    // `StreamChunk::usage` and `providers::TokenUsage` (what `self.usage`
    // and `execute()`'s `ProviderResponse::usage` both use) are distinct
    // types with the same shape — convert.
    let usage = crate::client::streaming::TokenUsage {
      prompt_tokens: self.usage.prompt_tokens,
      completion_tokens: self.usage.completion_tokens,
      total_tokens: self.usage.total_tokens,
    };

    Ok(Some(StreamChunk {
      media: Vec::new(),
      content: String::new(),
      is_final: true,
      metadata: None,
      usage: Some(usage),
      content_type: Some("text".to_string()),
      tool_call_deltas,
    }))
  }
}

#[async_trait]
impl LLMProvider for MockProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse> {
    // Simulate network delay
    if self.delay_ms > 0 {
      tokio::time::sleep(tokio::time::Duration::from_millis(self.delay_ms)).await;
    }

    // V1.5: a queued transient error takes priority over the normal
    // response path, one per call, so tests can simulate "fails N
    // times then succeeds".
    if let Some(err) = self.error_queue.lock().ok().and_then(|mut q| q.pop_front()) {
      return Err(err);
    }

    // Simulate error if configured
    if self.simulate_error {
      return Err(LLMError::ModelExecutionError {
        message: "Mock provider simulated error".to_string(),
      });
    }

    // Generate response
    let content_text = self.next_response(request);

    let word_count = content_text.split_whitespace().count() as u32;

    let tool_calls = self
      .tool_call_queue
      .lock()
      .ok()
      .and_then(|mut q| q.pop_front())
      .unwrap_or_default();
    let stop_reason = if tool_calls.is_empty() {
      Some(StopReason::Stop)
    } else {
      Some(StopReason::ToolCalls)
    };

    Ok(ProviderResponse {
      content: ContentType::Text(content_text),
      usage: Some(TokenUsage {
        prompt_tokens: Some(50),
        completion_tokens: Some(word_count),
        total_tokens: Some(50 + word_count),
      }),
      metadata: Some(serde_json::json!({
          "model": request.model,
          "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool_calls" }
      })),
      tool_calls,
      stop_reason,
      thinking: None,
    })
  }

  async fn execute_streaming(
    &self,
    request: &ProviderRequest,
  ) -> Result<Box<dyn StreamingResponse>> {
    // Simulate network delay
    if self.delay_ms > 0 {
      tokio::time::sleep(tokio::time::Duration::from_millis(self.delay_ms)).await;
    }

    // V1.5: see the analogous check in `execute` above.
    if let Some(err) = self.error_queue.lock().ok().and_then(|mut q| q.pop_front()) {
      return Err(err);
    }

    // Simulate error if configured
    if self.simulate_error {
      return Err(LLMError::ModelExecutionError {
        message: "Mock provider simulated error".to_string(),
      });
    }

    let content = self.next_response(request);
    let word_count = content.split_whitespace().count() as u32;
    let tool_calls = self
      .tool_call_queue
      .lock()
      .ok()
      .and_then(|mut q| q.pop_front())
      .unwrap_or_default();
    let usage = TokenUsage {
      prompt_tokens: Some(50),
      completion_tokens: Some(word_count),
      total_tokens: Some(50 + word_count),
    };

    Ok(Box::new(MockStreamingResponse::new(
      content, tool_calls, usage,
    )))
  }

  async fn validate_config(&self) -> Result<()> {
    if self.simulate_error {
      Err(LLMError::ConfigurationError {
        message: "Mock provider configured to simulate error".to_string(),
      })
    } else {
      Ok(())
    }
  }

  fn base_url(&self) -> &str {
    "mock://localhost"
  }

  fn supported_models(&self) -> Vec<String> {
    vec![
      "mock-model".to_string(),
      "mock-fast".to_string(),
      "mock-slow".to_string(),
    ]
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  #[tokio::test]
  async fn test_mock_provider_default_response() {
    let provider = MockProvider::new("", None).unwrap();
    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({
          "role": "user",
          "content": "Hello, world!"
      })],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let response = provider.execute(&request).await.unwrap();
    assert!(response.content.to_string().contains("Mock response"));
  }

  #[tokio::test]
  async fn test_mock_provider_custom_response() {
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_response("Custom test response");

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({
          "role": "user",
          "content": "Test prompt"
      })],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let response = provider.execute(&request).await.unwrap();
    assert_eq!(response.content.to_string(), "Custom test response");
  }

  #[tokio::test]
  async fn test_mock_provider_error_simulation() {
    let provider = MockProvider::new("", None).unwrap().with_error();

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({
          "role": "user",
          "content": "Test prompt"
      })],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let result = provider.execute(&request).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_mock_provider_with_delay() {
    let provider = MockProvider::new("", None).unwrap().with_delay(50);

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({
          "role": "user",
          "content": "Test prompt"
      })],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let start = std::time::Instant::now();
    let _response = provider.execute(&request).await.unwrap();
    let duration = start.elapsed();

    assert!(duration.as_millis() >= 50);
  }

  #[tokio::test]
  async fn mock_provider_surfaces_queued_tool_calls() {
    let call = ToolCallRequest {
      id: "call_0".to_string(),
      name: "get_weather".to_string(),
      arguments: serde_json::json!({"city": "Tokyo"}),
    };
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_tool_calls(vec![call.clone()]);

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({"role": "user", "content": "weather?"})],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let response = provider.execute(&request).await.unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0], call);
    assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
  }

  #[tokio::test]
  async fn mock_provider_no_tool_calls_yields_stop() {
    let provider = MockProvider::new("", None).unwrap();
    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };
    let response = provider.execute(&request).await.unwrap();
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  }

  /// V1.5: queued errors are consumed FIFO, one per call, before the
  /// mock falls through to its normal response — this is the exact
  /// "fails N times then succeeds" shape the client-level retry path
  /// needs to prove out.
  #[tokio::test]
  async fn with_queued_errors_are_consumed_fifo_then_falls_through_to_response() {
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_queued_errors(vec![
        LLMError::HttpError {
          status_code: 429,
          message: "rate limited".to_string(),
        },
        LLMError::HttpError {
          status_code: 503,
          message: "unavailable".to_string(),
        },
      ])
      .with_response("recovered");

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let first = provider.execute(&request).await;
    assert!(matches!(
      first,
      Err(LLMError::HttpError {
        status_code: 429,
        ..
      })
    ));

    let second = provider.execute(&request).await;
    assert!(matches!(
      second,
      Err(LLMError::HttpError {
        status_code: 503,
        ..
      })
    ));

    let third = provider.execute(&request).await.unwrap();
    assert_eq!(third.content.to_string(), "recovered");
  }

  #[tokio::test]
  async fn test_mock_provider_streaming() {
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_response("Streaming test");

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({
          "role": "user",
          "content": "Test prompt"
      })],
      stream: true,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let _stream = provider.execute_streaming(&request).await.unwrap();
    // Note: Testing actual stream consumption would require more complex setup
  }

  /// V2.2: the mock streaming path must emit a genuine multi-chunk
  /// sequence (not a single `is_final: true` chunk) so tests that switch
  /// to streaming exercise real delta forwarding, and concatenating every
  /// chunk's `content` must reconstruct the original response exactly.
  #[tokio::test]
  async fn streaming_response_splits_into_multiple_chunks_that_concatenate_exactly() {
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_response("this is a longer streaming response for testing");

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
      stream: true,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let mut stream = provider.execute_streaming(&request).await.unwrap();
    let mut chunk_count = 0;
    let mut reconstructed = String::new();
    let mut saw_final = false;
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
      chunk_count += 1;
      reconstructed.push_str(&chunk.content);
      if chunk.is_final {
        saw_final = true;
        assert!(chunk.usage.is_some(), "final chunk must carry usage");
      }
    }

    assert!(
      chunk_count > 1,
      "expected more than one chunk, got {chunk_count}"
    );
    assert!(saw_final, "expected exactly one is_final chunk");
    assert_eq!(
      reconstructed,
      "this is a longer streaming response for testing"
    );
  }

  /// The streaming path must surface queued tool calls exactly like the
  /// non-streaming `execute()` path does — every ReAct/Plan-Execute test
  /// scripting `YANSHI_MOCK_TOOL_CALLS` depends on this.
  #[tokio::test]
  async fn streaming_response_surfaces_queued_tool_calls() {
    let call = ToolCallRequest {
      id: "call_0".to_string(),
      name: "get_weather".to_string(),
      arguments: serde_json::json!({"city": "Tokyo"}),
    };
    let provider = MockProvider::new("", None)
      .unwrap()
      .with_tool_calls(vec![call.clone()]);

    let request = ProviderRequest {
      model: "mock-model".to_string(),
      messages: vec![serde_json::json!({"role": "user", "content": "weather?"})],
      stream: true,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let mut stream = provider.execute_streaming(&request).await.unwrap();
    let mut deltas = Vec::new();
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
      deltas.extend(chunk.tool_call_deltas);
    }

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].id.as_deref(), Some("call_0"));
    assert_eq!(deltas[0].name.as_deref(), Some("get_weather"));
    let reassembled: serde_json::Value =
      serde_json::from_str(deltas[0].arguments_delta.as_deref().unwrap()).unwrap();
    assert_eq!(reassembled, serde_json::json!({"city": "Tokyo"}));
  }
}
