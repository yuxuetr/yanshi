//! Cross-provider behavioral consistency suite.
//!
//! Drives the same input through OpenAI, Anthropic, Google, Moonshot, and
//! StepFun providers (using each one's own response wire format) and asserts
//! that the parsed [`ProviderResponse`] / [`LLMError`] outputs match a single
//! consistent contract:
//!
//! 1. **Success path**: text content, `StopReason::Stop`, populated
//!    [`TokenUsage`] (prompt / completion / total).
//! 2. **Tool-calling success path**: `tool_calls` array length 1, name /
//!    arguments / id parsed identically regardless of provider wire format
//!    (OpenAI `tool_calls`, Anthropic `tool_use` content blocks, Google
//!    `functionCall` parts), `StopReason::ToolCalls`. id may be synthesised
//!    when the provider doesn't supply one (Google), but must be non-empty.
//! 3. **Authentication failure (401)**: all providers surface
//!    [`LLMError::HttpError`] with `status_code = 401`, regardless of how
//!    they label the error in the response body.
//! 4. **Rate limit (429)**: all providers surface
//!    [`LLMError::HttpError`] with `status_code = 429`.
//! 5. **Server error (500)**: all providers surface
//!    [`LLMError::HttpError`] with `status_code = 500`.
//!
//! 6. **Streaming success path**: each provider parses its own native
//!    streaming wire format (OpenAI / Moonshot / StepFun SSE
//!    `data: {chunk}` + `data: [DONE]`, Anthropic SSE `event: …` /
//!    `data: …`, Google newline-delimited JSON), and the resulting
//!    [`StreamChunk`] sequence concatenates to the same text, terminates
//!    cleanly, and emits at least one chunk with `is_final = true`.
//!
//! These are mocked via a hand-rolled tokio TCP listener — same pattern as
//! `trace_context_propagation.rs`, see that file for rationale. The
//! streaming path additionally uses chunked transfer encoding so each event
//! arrives as a separate frame, which is how real LLM providers stream.
//!
//! Live LLM tests (real API calls) are gated by
//! `YANSHI_LIVE_LLM_TESTS=1` and live in a separate file when added.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use yanshi_llm::LLMError;
use yanshi_llm::ResponseFormat;
use yanshi_llm::client::StreamingResponse;
use yanshi_llm::client::streaming::collect_streaming_response;
use yanshi_llm::providers::{
  AnthropicProvider, ContentType, GoogleProvider, LLMProvider, MoonshotProvider, OpenAIProvider,
  ProviderRequest, StepFunProvider,
};
use yanshi_llm::tool_calling::{StopReason, ToolCallRequest, ToolChoice, ToolSpec};

// -----------------------------------------------------------------------------
// Shared mock-server helper (parameterized by status + body)
// -----------------------------------------------------------------------------

/// Spawn a one-shot TCP listener that accepts a single HTTP/1.1 request and
/// replies with `(status, body)`. Returns `(base_url, captured_request)`.
async fn spawn_mock_server(status: u16, body: String) -> (String, Arc<Mutex<Option<String>>>) {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("local_addr");
  let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
  let captured_writer = captured.clone();

  tokio::spawn(async move {
    let (mut stream, _) = match listener.accept().await {
      Ok(v) => v,
      Err(_) => return,
    };

    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    let mut head_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    loop {
      let n = match stream.read(&mut tmp).await {
        Ok(0) | Err(_) => break,
        Ok(n) => n,
      };
      buf.extend_from_slice(&tmp[..n]);
      if head_end.is_none() {
        for i in 0..buf.len().saturating_sub(3) {
          if &buf[i..i + 4] == b"\r\n\r\n" {
            head_end = Some(i + 4);
            break;
          }
        }
      }
      if let Some(end) = head_end {
        if content_length.is_none() {
          let head = std::str::from_utf8(&buf[..end]).unwrap_or("");
          for line in head.split("\r\n") {
            if let Some(value) = line
              .strip_prefix("Content-Length:")
              .or_else(|| line.strip_prefix("content-length:"))
            {
              content_length = value.trim().parse().ok();
            }
          }
        }
        let body_so_far = buf.len() - end;
        if body_so_far >= content_length.unwrap_or(0) {
          break;
        }
      }
    }
    *captured_writer.lock().await = Some(String::from_utf8_lossy(&buf).into_owned());

    let status_text = match status {
      200 => "OK",
      401 => "Unauthorized",
      429 => "Too Many Requests",
      500 => "Internal Server Error",
      503 => "Service Unavailable",
      _ => "Status",
    };
    let response = format!(
      "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      status,
      status_text,
      body.len(),
      body,
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
  });

  // Give the server task a chance to reach `accept()` before clients connect.
  tokio::time::sleep(Duration::from_millis(50)).await;

  (format!("http://{addr}"), captured)
}

/// Test client that bypasses any system proxy so 127.0.0.1 requests reach the
/// listener instead of being routed through a Clash/V2Ray-style proxy on the
/// dev machine. See CLAUDE.md "Rust HTTP Testing Guidelines".
fn no_proxy_client() -> reqwest::Client {
  reqwest::Client::builder()
    .no_proxy()
    .pool_max_idle_per_host(0)
    .timeout(Duration::from_secs(10))
    .build()
    .expect("client")
}

fn provider_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "ping"})],
    stream: false,
    parameters: HashMap::new(),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

/// Build a `ProviderRequest` that advertises a single `get_weather` tool and
/// requires the model to call it. The mock servers below ignore the request
/// body and return canned tool-call responses, so the only thing this helper
/// needs to do is exercise the request-side `tools` / `tool_choice` encode
/// path without crashing the provider.
fn provider_request_with_tools(model: &str) -> ProviderRequest {
  let weather_tool = ToolSpec::new(
    "get_weather",
    "Return the weather for a city",
    json!({
      "type": "object",
      "properties": {"city": {"type": "string"}},
      "required": ["city"]
    }),
  );
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "weather in Tokyo?"})],
    stream: false,
    parameters: HashMap::new(),
    tools: Some(vec![weather_tool]),
    tool_choice: Some(ToolChoice::Required),
    thinking: None,
    response_format: None,
  }
}

/// Build a `ProviderRequest` with a `JsonSchema` `response_format` and no
/// `tools` — the "final answer only" shape V2.1's structured-output support
/// actually sends (see `yanshi-agents`' `ReActAgent`/`PlanExecuteAgent`
/// `output_schema` wiring).
fn provider_request_with_json_schema(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "answer please"})],
    stream: false,
    parameters: HashMap::new(),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: Some(ResponseFormat::JsonSchema {
      name: "final_answer".to_string(),
      schema: json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
      }),
      strict: Some(true),
    }),
  }
}

// -----------------------------------------------------------------------------
// Per-provider success-path fixtures
// -----------------------------------------------------------------------------

const OPENAI_SUCCESS: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"gpt-4o-mini","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
const ANTHROPIC_SUCCESS: &str = r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}"#;
const GOOGLE_SUCCESS: &str = r#"{"candidates":[{"content":{"parts":[{"text":"ok"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}"#;
const MOONSHOT_SUCCESS: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"moonshot-v1-8k","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
const STEPFUN_SUCCESS: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"step-3.5-flash","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;

// -----------------------------------------------------------------------------
// Success-path consistency: every provider returns text + StopReason::Stop +
// populated TokenUsage given a well-formed response in its native shape.
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_success_path() {
  let (base_url, _captured) = spawn_mock_server(200, OPENAI_SUCCESS.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request("gpt-4o-mini"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_success_path() {
  let (base_url, _captured) = spawn_mock_server(200, ANTHROPIC_SUCCESS.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
    .expect("provider");
  let response = provider
    .execute(&provider_request("claude-3-5-sonnet"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_success_path() {
  let (base_url, _captured) = spawn_mock_server(200, GOOGLE_SUCCESS.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request("gemini-1.5-pro"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_success_path() {
  let (base_url, _captured) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request("moonshot-v1-8k"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_success_path() {
  let (base_url, _captured) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request("step-3.5-flash"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
}

// -----------------------------------------------------------------------------
// Per-provider tool-calling fixtures
//
// Each fixture is the provider's *native* wire format for "model called
// `get_weather(city='Tokyo')`". The cross-provider contract being tested is:
// regardless of where in the response the tool call lives (OpenAI's
// `tool_calls` array, Anthropic's `tool_use` content block, Google's
// `functionCall` part), the parsed `ProviderResponse.tool_calls` is one entry
// with `name = "get_weather"`, `arguments.city = "Tokyo"`, a non-empty `id`,
// and `stop_reason = StopReason::ToolCalls`.
// -----------------------------------------------------------------------------

const OPENAI_TOOL_CALL: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"gpt-4o-mini","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
const ANTHROPIC_TOOL_CALL: &str = r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[{"type":"text","text":"I'll check the weather."},{"type":"tool_use","id":"toolu_abc","name":"get_weather","input":{"city":"Tokyo"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":5}}"#;
// Note: Google emits `finishReason: "STOP"` even when functionCall is present;
// the provider adapter overrides to `ToolCalls` when functionCall parts exist.
const GOOGLE_TOOL_CALL: &str = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"}}}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#;
const MOONSHOT_TOOL_CALL: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"moonshot-v1-8k","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
const STEPFUN_TOOL_CALL: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"step-3.5-flash","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"}}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;

// -----------------------------------------------------------------------------
// Tool-calling consistency: every provider parses a model-issued tool call
// into a single `ToolCallRequest { name, arguments, id }` and reports
// `StopReason::ToolCalls`, regardless of the underlying wire format.
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_tool_call_path() {
  let (base_url, _captured) = spawn_mock_server(200, OPENAI_TOOL_CALL.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_tools("gpt-4o-mini"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, "get_weather", "Tokyo");
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_call_path() {
  let (base_url, _captured) = spawn_mock_server(200, ANTHROPIC_TOOL_CALL.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
    .expect("provider");
  let response = provider
    .execute(&provider_request_with_tools("claude-3-5-sonnet"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, "get_weather", "Tokyo");
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_tool_call_path() {
  let (base_url, _captured) = spawn_mock_server(200, GOOGLE_TOOL_CALL.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_tools("gemini-1.5-pro"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, "get_weather", "Tokyo");
  // Google reports `STOP` on the wire even when emitting a functionCall;
  // the provider adapter is responsible for normalising to `ToolCalls`.
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_tool_call_path() {
  let (base_url, _captured) = spawn_mock_server(200, MOONSHOT_TOOL_CALL.to_string()).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_tools("moonshot-v1-8k"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, "get_weather", "Tokyo");
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_tool_call_path() {
  let (base_url, _captured) = spawn_mock_server(200, STEPFUN_TOOL_CALL.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_tools("step-3.5-flash"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, "get_weather", "Tokyo");
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

// -----------------------------------------------------------------------------
// Error-mapping consistency: every provider maps non-2xx HTTP responses to
// LLMError::HttpError preserving the exact status code, regardless of body.
// This is the contract — promotion to AuthenticationError/RateLimitExceeded
// would be a *breaking* change downstream consumers depend on.
// -----------------------------------------------------------------------------

const GENERIC_ERROR_BODY: &str = r#"{"error":{"message":"go away"}}"#;

async fn assert_status_maps_to_http_error<F, Fut>(status: u16, run: F)
where
  F: FnOnce(String) -> Fut,
  Fut: std::future::Future<Output = yanshi_llm::Result<yanshi_llm::providers::ProviderResponse>>,
{
  let (base_url, _captured) = spawn_mock_server(status, GENERIC_ERROR_BODY.to_string()).await;
  let err = run(base_url).await.expect_err("expected HttpError");
  match err {
    LLMError::HttpError { status_code, .. } => assert_eq!(status_code, status),
    other => panic!("expected HttpError({status}), got {other:?}"),
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_maps_401_to_http_error() {
  assert_status_maps_to_http_error(401, |base_url| async move {
    let provider =
      OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    provider.execute(&provider_request("gpt-4o-mini")).await
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_maps_429_to_http_error() {
  assert_status_maps_to_http_error(429, |base_url| async move {
    let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider
      .execute(&provider_request("claude-3-5-sonnet"))
      .await
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_maps_500_to_http_error() {
  assert_status_maps_to_http_error(500, |base_url| async move {
    let provider =
      GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    provider.execute(&provider_request("gemini-1.5-pro")).await
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_maps_503_to_http_error() {
  assert_status_maps_to_http_error(503, |base_url| async move {
    let provider = MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider.execute(&provider_request("moonshot-v1-8k")).await
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_maps_401_to_http_error() {
  assert_status_maps_to_http_error(401, |base_url| async move {
    let provider = StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider.execute(&provider_request("step-3.5-flash")).await
  })
  .await;
}

// -----------------------------------------------------------------------------
// response_format / json_schema consistency (V2.1)
//
// Confirms `ProviderRequest::response_format` reaches the wire in each
// provider's native shape, and — for Anthropic, which has no native
// `response_format` field — that the synthetic-tool-call emulation round
// trips: the forced tool call in the *response* fixture is unwrapped back
// into plain-text JSON `content`, transparent to callers.
// -----------------------------------------------------------------------------

const OPENAI_JSON_SCHEMA_SUCCESS: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"gpt-4o-mini","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"42\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
const ANTHROPIC_JSON_SCHEMA_TOOL_USE: &str = r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[{"type":"tool_use","id":"toolu_final","name":"final_answer","input":{"answer":"42"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}"#;
const GOOGLE_JSON_SCHEMA_SUCCESS: &str = r#"{"candidates":[{"content":{"parts":[{"text":"{\"answer\":\"42\"}"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_json_schema_request_sets_native_response_format() {
  let (base_url, captured) = spawn_mock_server(200, OPENAI_JSON_SCHEMA_SUCCESS.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_json_schema("gpt-4o-mini"))
    .await
    .expect("ok");
  assert_text(&response.content, "42");
  let sent = captured.lock().await.clone().expect("request captured");
  assert!(
    sent.contains("\"type\":\"json_schema\"") && sent.contains("\"name\":\"final_answer\""),
    "expected native response_format in request body, got: {sent}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_json_schema_request_sets_native_response_format() {
  let (base_url, captured) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  provider
    .execute(&provider_request_with_json_schema("moonshot-v1-8k"))
    .await
    .expect("ok");
  let sent = captured.lock().await.clone().expect("request captured");
  assert!(
    sent.contains("\"type\":\"json_schema\""),
    "expected native response_format in request body, got: {sent}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_json_schema_request_sets_native_response_format() {
  let (base_url, captured) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  provider
    .execute(&provider_request_with_json_schema("step-3.5-flash"))
    .await
    .expect("ok");
  let sent = captured.lock().await.clone().expect("request captured");
  assert!(
    sent.contains("\"type\":\"json_schema\""),
    "expected native response_format in request body, got: {sent}"
  );
}

/// The one case where the "native path" and "prompt-fallback path" genuinely
/// diverge in wire shape: Anthropic has no `response_format` field, so this
/// exercises the full synthetic-tool-call round trip — request forces a
/// tool choice, response fixture returns a `tool_use` block, and the parsed
/// `ProviderResponse.content` comes back as plain JSON text with zero
/// `tool_calls`, exactly like every other provider's native path above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_json_schema_request_forces_synthetic_tool_and_unwraps_response() {
  let (base_url, captured) =
    spawn_mock_server(200, ANTHROPIC_JSON_SCHEMA_TOOL_USE.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
    .expect("provider");
  let response = provider
    .execute(&provider_request_with_json_schema("claude-3-5-sonnet"))
    .await
    .expect("ok");
  assert_text(&response.content, "42");
  assert!(
    response.tool_calls.is_empty(),
    "the synthetic schema tool call must not leak into tool_calls, got: {:?}",
    response.tool_calls
  );
  let sent = captured.lock().await.clone().expect("request captured");
  // serde_json (no `preserve_order` feature in this workspace) serialises
  // object keys alphabetically, so `tool_choice`'s two fields land as
  // `{"name":...,"type":...}` — assert on the individual k:v pairs rather
  // than a literal whole-object substring.
  assert!(
    sent.contains("\"tool_choice\":{")
      && sent.contains("\"type\":\"tool\"")
      && sent.contains("\"name\":\"final_answer\""),
    "expected forced tool_choice in request body, got: {sent}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_json_schema_request_sets_native_response_schema() {
  let (base_url, captured) = spawn_mock_server(200, GOOGLE_JSON_SCHEMA_SUCCESS.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let response = provider
    .execute(&provider_request_with_json_schema("gemini-1.5-pro"))
    .await
    .expect("ok");
  assert_text(&response.content, "42");
  let sent = captured.lock().await.clone().expect("request captured");
  assert!(
    sent.contains("\"responseMimeType\":\"application/json\"")
      && sent.contains("\"responseSchema\""),
    "expected native responseSchema in request body, got: {sent}"
  );
}

/// Mock ignores `response_format` entirely (scripted responses don't
/// consult the request) — confirms setting it never breaks the mock path
/// callers rely on for tests, same "consistent no-op" contract every
/// unwired field already has there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_ignores_json_schema_response_format() {
  use yanshi_llm::providers::MockProvider;
  let provider = MockProvider::new("test-key", None).expect("provider");
  let response = provider
    .execute(&provider_request_with_json_schema("mock-model"))
    .await
    .expect("ok");
  assert!(matches!(response.content, ContentType::Text(_)));
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn assert_text(content: &ContentType, expected: &str) {
  match content {
    ContentType::Text(t) => assert!(
      t.contains(expected),
      "expected content to contain `{}`, got `{}`",
      expected,
      t
    ),
    other => panic!("expected ContentType::Text, got {:?}", other),
  }
}

fn assert_tool_call(calls: &[ToolCallRequest], expected_name: &str, expected_city: &str) {
  assert_eq!(
    calls.len(),
    1,
    "expected exactly one tool call, got {calls:?}"
  );
  let call = &calls[0];
  assert_eq!(call.name, expected_name);
  assert!(
    !call.id.is_empty(),
    "tool call id must be populated (synthesised if provider doesn't supply one)"
  );
  let city = call
    .arguments
    .get("city")
    .and_then(|v| v.as_str())
    .unwrap_or_else(|| {
      panic!(
        "expected `arguments.city` to be a string, got {}",
        call.arguments
      )
    });
  assert_eq!(city, expected_city);
}

fn assert_usage(usage: &Option<yanshi_llm::providers::TokenUsage>) {
  let usage = usage.as_ref().expect("usage must be populated");
  assert!(
    usage.prompt_tokens.is_some(),
    "prompt_tokens must be populated"
  );
  assert!(
    usage.completion_tokens.is_some(),
    "completion_tokens must be populated"
  );
  assert!(
    usage.total_tokens.is_some(),
    "total_tokens must be populated"
  );
}

// -----------------------------------------------------------------------------
// Streaming consistency
//
// Each provider parses its own native streaming wire format:
//
//   * OpenAI / Moonshot / StepFun: SSE `data: {chunk-json}` deltas + `data: [DONE]`
//   * Anthropic: SSE `event: <type>` + `data: {event-json}` events; final
//     marker is `message_stop` (or `content_block_stop`)
//   * Google: newline-delimited JSON; `finishReason` on the last object is the
//     terminator (no SSE prefix at all)
//
// The cross-provider contract under test is independent of all of that:
//
//   1. Draining `next_chunk()` produces ≥ 2 non-final delta chunks whose
//      `content` concatenated equals `"Hello world"`.
//   2. The stream eventually terminates: after the final marker, the next call
//      to `next_chunk()` returns `Ok(None)`.
//   3. Every emitted chunk carries `content_type == Some("text")`.
//
// Reusing `spawn_mock_server` (Content-Length + close) is intentional: the
// reqwest `bytes_stream()` delivers the full body and the per-provider parser
// splits on `\n` regardless of how the bytes were chunked over the wire. The
// goal is wire-format coverage, not network timing.
// -----------------------------------------------------------------------------

fn provider_request_streaming(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "ping"})],
    stream: true,
    parameters: HashMap::new(),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

/// Spawn a streaming mock server that emits each of `events` as a separate
/// HTTP `Transfer-Encoding: chunked` frame.
///
/// Why a different helper from `spawn_mock_server`: every provider's stream
/// parser returns one `StreamChunk` per call to `next_chunk()` and only awaits
/// more bytes between calls. If we delivered every SSE event in a single TCP
/// write (the `Content-Length` path), the parser would return the first event
/// and then see the underlying byte stream as exhausted, dropping the rest.
/// Chunked encoding faithfully simulates an LLM provider trickling events,
/// which is the contract we actually want to lock down.
async fn spawn_streaming_mock_server(events: Vec<String>) -> String {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("local_addr");

  tokio::spawn(async move {
    let (mut stream, _) = match listener.accept().await {
      Ok(v) => v,
      Err(_) => return,
    };

    // Drain the request before responding. We don't care about the contents;
    // we just need the read side to not back-pressure the writer.
    let mut tmp = [0u8; 1024];
    let mut head_end: Option<usize> = None;
    let mut head_buf: Vec<u8> = Vec::with_capacity(2048);
    while head_end.is_none() {
      let n = match stream.read(&mut tmp).await {
        Ok(0) | Err(_) => break,
        Ok(n) => n,
      };
      head_buf.extend_from_slice(&tmp[..n]);
      for i in 0..head_buf.len().saturating_sub(3) {
        if &head_buf[i..i + 4] == b"\r\n\r\n" {
          head_end = Some(i + 4);
          break;
        }
      }
    }

    let header = "HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Connection: close\r\n\r\n";
    if stream.write_all(header.as_bytes()).await.is_err() {
      return;
    }

    for event in &events {
      let frame = format!("{:x}\r\n{}\r\n", event.len(), event);
      if stream.write_all(frame.as_bytes()).await.is_err() {
        return;
      }
      let _ = stream.flush().await;
      // Small inter-chunk delay so reqwest's bytes_stream surfaces each frame
      // as its own item instead of coalescing under TCP buffering.
      tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let _ = stream.write_all(b"0\r\n\r\n").await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
  });

  tokio::time::sleep(Duration::from_millis(50)).await;
  format!("http://{addr}")
}

/// Drain an in-flight stream and assert the cross-provider contract.
///
/// The contract is intentionally lenient about *where* the terminator lives:
///
/// * OpenAI-compatible providers (OpenAI / Moonshot / StepFun) attach
///   `finish_reason: "stop"` to the same chunk as the trailing content delta,
///   so the second chunk is both content-bearing and final.
/// * Anthropic emits `content_block_delta` events for text and a separate
///   `message_stop` (no content) terminator.
/// * Google sets `finishReason` on the last JSON object, which still carries
///   the trailing text part.
///
/// What every provider must agree on:
///   1. ≥ 2 chunks carry non-empty `content` and concatenate to `"Hello world"`.
///   2. At least one chunk has `is_final = true`.
///   3. Each emitted chunk has `content_type == Some("text")`.
///   4. After the terminator, `next_chunk()` returns `Ok(None)` cleanly.
async fn assert_stream_yields_hello_world(mut stream: Box<dyn StreamingResponse>) {
  let mut text = String::new();
  let mut content_chunks = 0usize;
  let mut saw_final = false;

  while let Some(chunk) = stream
    .next_chunk()
    .await
    .expect("streaming chunk must parse")
  {
    assert_eq!(
      chunk.content_type.as_deref(),
      Some("text"),
      "every streamed chunk should carry text content_type"
    );
    if chunk.is_final {
      saw_final = true;
    }
    if !chunk.content.is_empty() {
      text.push_str(&chunk.content);
      content_chunks += 1;
    }
  }

  assert_eq!(
    text, "Hello world",
    "concatenated stream text mismatch (got {content_chunks} content-bearing chunks)"
  );
  assert!(
    content_chunks >= 2,
    "expected ≥2 content-bearing chunks, got {content_chunks}"
  );
  assert!(
    saw_final,
    "expected at least one chunk with `is_final = true` (terminator)"
  );

  // After the terminator, the stream must be cleanly drained.
  assert!(
    stream
      .next_chunk()
      .await
      .expect("next_chunk after drain must not error")
      .is_none(),
    "draining a finished stream should yield Ok(None)"
  );
}

fn openai_compat_stream_events(model: &str) -> Vec<String> {
  vec![
    format!(
      "data: {{\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"Hello\"}}}}]}}\n\n",
    ),
    format!(
      "data: {{\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\" world\"}},\"finish_reason\":\"stop\"}}]}}\n\n",
    ),
    "data: [DONE]\n\n".to_string(),
  ]
}

fn anthropic_stream_events() -> Vec<String> {
  vec![
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string(),
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n".to_string(),
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
  ]
}

fn google_stream_events() -> Vec<String> {
  vec![
    "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"index\":0}]}\n".to_string(),
    "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}\n".to_string(),
  ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_streaming_path() {
  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("gpt-4o-mini")).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("gpt-4o-mini"))
    .await
    .expect("stream");
  assert_stream_yields_hello_world(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_streaming_path() {
  let base_url = spawn_streaming_mock_server(anthropic_stream_events()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
    .expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("claude-3-5-sonnet"))
    .await
    .expect("stream");
  assert_stream_yields_hello_world(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_streaming_path() {
  let base_url = spawn_streaming_mock_server(google_stream_events()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("gemini-1.5-pro"))
    .await
    .expect("stream");
  assert_stream_yields_hello_world(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_streaming_path() {
  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("moonshot-v1-8k")).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("moonshot-v1-8k"))
    .await
    .expect("stream");
  assert_stream_yields_hello_world(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_streaming_path() {
  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("step-3.5-flash")).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("step-3.5-flash"))
    .await
    .expect("stream");
  assert_stream_yields_hello_world(stream).await;
}

// -----------------------------------------------------------------------------
// P-LLM2.5: streaming + tool calls
//
// Before this fix, Google/Moonshot/StepFun's streaming parsers hardcoded
// `tool_call_deltas: Vec::new()` — a tool call requested mid-stream was
// silently dropped, with no error, directly breaking the ReAct agent loop
// (which streams unconditionally). These drive a real
// `provider.execute_streaming()` call over a mocked SSE/NDJSON server through
// `collect_streaming_response` (the same reassembly path a real caller uses)
// and assert the tool call survives intact — the end-to-end version of the
// per-provider unit tests already covering `parse_sse_chunk`/`parse_json_chunk`
// directly.
// -----------------------------------------------------------------------------

/// OpenAI-compatible `delta.tool_calls[]` streaming events for a
/// `get_weather(city="Tokyo")` call, fragmented across chunks the way a real
/// provider streams: identity (id/name) on the first delta, then the
/// argument JSON split across two more deltas, with `finish_reason:
/// "tool_calls"` on the last content-bearing chunk.
fn openai_compat_tool_call_stream_events(model: &str) -> Vec<String> {
  let chunk1 = json!({
    "id": "chatcmpl-x", "object": "chat.completion.chunk", "created": 0, "model": model,
    "choices": [{
      "index": 0,
      "delta": {"role": "assistant", "tool_calls": [{
        "index": 0, "id": "call_abc", "type": "function",
        "function": {"name": "get_weather", "arguments": ""}
      }]}
    }]
  });
  let chunk2 = json!({
    "id": "chatcmpl-x", "object": "chat.completion.chunk", "created": 0, "model": model,
    "choices": [{
      "index": 0,
      "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}
    }]
  });
  let chunk3 = json!({
    "id": "chatcmpl-x", "object": "chat.completion.chunk", "created": 0, "model": model,
    "choices": [{
      "index": 0,
      "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"Tokyo\"}"}}]},
      "finish_reason": "tool_calls"
    }]
  });
  vec![
    format!("data: {chunk1}\n\n"),
    format!("data: {chunk2}\n\n"),
    format!("data: {chunk3}\n\n"),
    "data: [DONE]\n\n".to_string(),
  ]
}

/// Gemini's `functionCall` part carries the complete, non-fragmented
/// `{"name", "args"}` object in one shot (see `google.rs`'s
/// `GoogleStreamingResponse::parse_json_chunk` doc comment) — one NDJSON
/// line with a leading text part plus the function call is enough to cover
/// the "text and tool call together" case the pre-fix code also mishandled.
fn google_tool_call_stream_events() -> Vec<String> {
  let chunk1 = json!({
    "candidates": [{
      "content": {"parts": [{"text": "Checking..."}], "role": "model"},
      "index": 0
    }]
  });
  let chunk2 = json!({
    "candidates": [{
      "content": {"parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Tokyo"}}}], "role": "model"},
      "finishReason": "STOP",
      "index": 0
    }]
  });
  vec![format!("{chunk1}\n"), format!("{chunk2}\n")]
}

async fn assert_stream_yields_get_weather_tokyo_tool_call(stream: Box<dyn StreamingResponse>) {
  let response = collect_streaming_response(stream, |_| async {})
    .await
    .expect("collect_streaming_response must succeed");
  assert_eq!(
    response.tool_calls.len(),
    1,
    "expected exactly one reconstructed tool call, got: {:?}",
    response.tool_calls
  );
  let call = &response.tool_calls[0];
  assert!(!call.id.is_empty(), "tool call id must not be empty");
  assert_eq!(call.name, "get_weather");
  assert_eq!(call.arguments, json!({"city": "Tokyo"}));
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_streaming_tool_call_path() {
  let base_url = spawn_streaming_mock_server(google_tool_call_stream_events()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("gemini-1.5-pro"))
    .await
    .expect("stream");
  assert_stream_yields_get_weather_tokyo_tool_call(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_streaming_tool_call_path() {
  let base_url =
    spawn_streaming_mock_server(openai_compat_tool_call_stream_events("moonshot-v1-8k")).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("moonshot-v1-8k"))
    .await
    .expect("stream");
  assert_stream_yields_get_weather_tokyo_tool_call(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_streaming_tool_call_path() {
  let base_url =
    spawn_streaming_mock_server(openai_compat_tool_call_stream_events("step-3.5-flash")).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("step-3.5-flash"))
    .await
    .expect("stream");
  assert_stream_yields_get_weather_tokyo_tool_call(stream).await;
}

// -----------------------------------------------------------------------------
// Multimodal consistency
//
// Each test sends a `text + image` user message in the provider's native wire
// format and asserts two cross-provider invariants:
//
//   1. Request-side encoding preserves the image payload (we look for the
//      marker base64 payload `"AAAA"` in the captured request body — every
//      provider shapes the bytes differently, but the bytes themselves must
//      survive the round-trip).
//   2. Response-side parsing is identical regardless of whether the request
//      was text-only or multimodal: text content + `StopReason::Stop` +
//      populated `TokenUsage` (same contract as the success-path tests).
//
// Native multimodal formats:
//
//   * OpenAI / Moonshot / StepFun: OpenAI v1/chat content array with
//     `{type: "image_url", image_url: {url: "data:image/png;base64,AAAA"}}`.
//   * Anthropic: content blocks with `{type: "image", source: {type: "base64",
//     media_type: "image/png", data: "AAAA"}}`.
//   * Google: OpenAI-style content array — the provider adapter translates it
//     into Gemini's `parts[].inline_data` shape (see
//     `openai_content_to_gemini_parts`).
//
// Marker payload `"AAAA"` is a 3-byte zero-padded base64 stub. It's invalid
// PNG data — that's intentional. The mock server doesn't decode the image,
// and using a known 4-char marker gives us a high-signal substring assertion
// without bloating fixtures with realistic image bytes.
// -----------------------------------------------------------------------------

const IMAGE_DATA_URL: &str = "data:image/png;base64,AAAA";
const IMAGE_MARKER: &str = "AAAA";

fn provider_request_multimodal_openai_style(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": [
        {"type": "text", "text": "What's in this image?"},
        {"type": "image_url", "image_url": {"url": IMAGE_DATA_URL}}
      ]
    })],
    stream: false,
    parameters: HashMap::new(),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

async fn run_multimodal<F, Fut>(fixture: &str, build_provider: F) -> String
where
  F: FnOnce(String) -> Fut,
  Fut: std::future::Future<Output = ()>,
{
  let (base_url, captured) = spawn_mock_server(200, fixture.to_string()).await;
  build_provider(base_url).await;
  let captured = captured.lock().await;
  captured.clone().expect("mock server received request")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_multimodal_path() {
  let captured = run_multimodal(OPENAI_SUCCESS, |base_url| async move {
    let provider =
      OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    let response = provider
      .execute(&provider_request_multimodal_openai_style("gpt-4o-mini"))
      .await
      .expect("ok");
    assert_text(&response.content, "ok");
    assert_usage(&response.usage);
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  })
  .await;
  assert!(
    captured.contains(IMAGE_MARKER),
    "OpenAI request body must preserve image payload, got: {captured}"
  );
  assert!(
    captured.contains("image_url"),
    "OpenAI request body must use the `image_url` part type, got: {captured}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_multimodal_path() {
  // P-LLM2.4: exercises the real translation path — the request is built in
  // the same OpenAI-shaped `image_url` form `MultimodalMessage::
  // to_openai_format()` actually produces, and `AnthropicProvider::
  // build_request_body` must translate it into a native `image` block
  // rather than forwarding the OpenAI shape verbatim (which the real
  // Anthropic API rejects).
  let captured = run_multimodal(ANTHROPIC_SUCCESS, |base_url| async move {
    let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let response = provider
      .execute(&provider_request_multimodal_openai_style(
        "claude-3-5-sonnet",
      ))
      .await
      .expect("ok");
    assert_text(&response.content, "ok");
    assert_usage(&response.usage);
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  })
  .await;
  assert!(
    captured.contains(IMAGE_MARKER),
    "Anthropic request body must preserve base64 image payload, got: {captured}"
  );
  assert!(
    captured.contains("\"type\":\"image\"") || captured.contains("\"type\": \"image\""),
    "Anthropic request body must use the `image` content block type, got: {captured}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_multimodal_path() {
  let captured = run_multimodal(GOOGLE_SUCCESS, |base_url| async move {
    let provider =
      GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    let response = provider
      .execute(&provider_request_multimodal_openai_style("gemini-1.5-pro"))
      .await
      .expect("ok");
    assert_text(&response.content, "ok");
    assert_usage(&response.usage);
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  })
  .await;
  // Google adapter rewrites OpenAI-style multimodal content into Gemini's
  // `inline_data` part. Both the marker bytes and the Gemini-specific key
  // must be present.
  assert!(
    captured.contains(IMAGE_MARKER),
    "Google request body must preserve base64 image payload, got: {captured}"
  );
  assert!(
    captured.contains("inline_data"),
    "Google request body must rewrite to `inline_data`, got: {captured}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_multimodal_path() {
  let captured = run_multimodal(MOONSHOT_SUCCESS, |base_url| async move {
    let provider = MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let response = provider
      .execute(&provider_request_multimodal_openai_style("moonshot-v1-8k"))
      .await
      .expect("ok");
    assert_text(&response.content, "ok");
    assert_usage(&response.usage);
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  })
  .await;
  assert!(
    captured.contains(IMAGE_MARKER),
    "Moonshot request body must preserve image payload, got: {captured}"
  );
  assert!(
    captured.contains("image_url"),
    "Moonshot request body must use the `image_url` part type, got: {captured}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_multimodal_path() {
  let captured = run_multimodal(STEPFUN_SUCCESS, |base_url| async move {
    let provider = StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let response = provider
      .execute(&provider_request_multimodal_openai_style("step-3.5-flash"))
      .await
      .expect("ok");
    assert_text(&response.content, "ok");
    assert_usage(&response.usage);
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
  })
  .await;
  assert!(
    captured.contains(IMAGE_MARKER),
    "StepFun request body must preserve image payload, got: {captured}"
  );
  assert!(
    captured.contains("image_url"),
    "StepFun request body must use the `image_url` part type, got: {captured}"
  );
}

// -----------------------------------------------------------------------------
// Extended HTTP error-mapping coverage
//
// The per-provider single-status tests above lock down one code each. The TODO
// spec for P3.6 requires every provider to map 401, 429, AND 5xx into
// `LLMError::HttpError` preserving the status code. The blocks below close the
// remaining matrix cells so a regression that breaks one specific status path
// on one specific provider can't slip past CI.
// -----------------------------------------------------------------------------

async fn run_openai_status(status: u16) {
  assert_status_maps_to_http_error(status, |base_url| async move {
    let provider =
      OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    provider.execute(&provider_request("gpt-4o-mini")).await
  })
  .await;
}

async fn run_anthropic_status(status: u16) {
  assert_status_maps_to_http_error(status, |base_url| async move {
    let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider
      .execute(&provider_request("claude-3-5-sonnet"))
      .await
  })
  .await;
}

async fn run_google_status(status: u16) {
  assert_status_maps_to_http_error(status, |base_url| async move {
    let provider =
      GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    provider.execute(&provider_request("gemini-1.5-pro")).await
  })
  .await;
}

async fn run_moonshot_status(status: u16) {
  assert_status_maps_to_http_error(status, |base_url| async move {
    let provider = MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider.execute(&provider_request("moonshot-v1-8k")).await
  })
  .await;
}

async fn run_stepfun_status(status: u16) {
  assert_status_maps_to_http_error(status, |base_url| async move {
    let provider = StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    provider.execute(&provider_request("step-3.5-flash")).await
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_maps_429_to_http_error() {
  run_openai_status(429).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_maps_500_to_http_error() {
  run_openai_status(500).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_maps_401_to_http_error() {
  run_anthropic_status(401).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_maps_500_to_http_error() {
  run_anthropic_status(500).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_maps_401_to_http_error() {
  run_google_status(401).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_maps_429_to_http_error() {
  run_google_status(429).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_maps_401_to_http_error() {
  run_moonshot_status(401).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_maps_429_to_http_error() {
  run_moonshot_status(429).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_maps_500_to_http_error() {
  run_moonshot_status(500).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_maps_429_to_http_error() {
  run_stepfun_status(429).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_maps_500_to_http_error() {
  run_stepfun_status(500).await;
}

// -----------------------------------------------------------------------------
// tool_choice mode consistency
//
// Each provider must encode the four canonical `ToolChoice` modes — Auto,
// None, Required, Tool { name } — into its own wire format, and the wire
// format must remain stable. Per-provider unit tests cover each mode in
// isolation; this block locks down the cross-provider contract in one place
// so an adapter rewrite can't quietly diverge from the documented matrix in
// `docs/LLM_PROVIDERS_MATRIX.md`.
//
// Wire format expectations:
//
//   * OpenAI / Moonshot / StepFun:
//       Auto     → `"auto"`
//       None     → `"none"`
//       Required → `"required"`
//       Tool{n}  → `{"type":"function","function":{"name":n}}`
//   * Anthropic:
//       Auto     → `{"type":"auto"}`
//       None     → `{"type":"none"}`
//       Required → `{"type":"any"}`   (Anthropic spells "required" as "any")
//       Tool{n}  → `{"type":"tool","name":n}`
//   * Google: `toolConfig.functionCallingConfig`
//       Auto     → `{"mode":"AUTO"}`
//       None     → `{"mode":"NONE"}`
//       Required → `{"mode":"ANY"}`
//       Tool{n}  → `{"mode":"ANY","allowedFunctionNames":[n]}`
// -----------------------------------------------------------------------------

const TOOL_CHOICE_TOOL_NAME: &str = "get_weather";

fn provider_request_with_choice(model: &str, choice: ToolChoice) -> ProviderRequest {
  let weather_tool = ToolSpec::new(
    TOOL_CHOICE_TOOL_NAME,
    "Return the weather for a city",
    json!({
      "type": "object",
      "properties": {"city": {"type": "string"}},
      "required": ["city"]
    }),
  );
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "weather in Tokyo?"})],
    stream: false,
    parameters: HashMap::new(),
    tools: Some(vec![weather_tool]),
    tool_choice: Some(choice),
    thinking: None,
    response_format: None,
  }
}

/// Strip the HTTP request head from the captured raw bytes and parse the
/// body as JSON. Panics if the body isn't valid JSON — the mock servers
/// never send a body the provider can't generate, so this should never
/// fire in practice.
fn captured_body(raw: &str) -> serde_json::Value {
  let body = raw
    .split_once("\r\n\r\n")
    .map(|(_, body)| body)
    .unwrap_or(raw);
  serde_json::from_str(body).unwrap_or_else(|err| panic!("body must be JSON ({err}): {body}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_tool_choice_all_modes() {
  for (choice, expected) in [
    (ToolChoice::Auto, json!("auto")),
    (ToolChoice::None, json!("none")),
    (ToolChoice::Required, json!("required")),
    (
      ToolChoice::Tool {
        name: TOOL_CHOICE_TOOL_NAME.to_string(),
      },
      json!({"type":"function","function":{"name": TOOL_CHOICE_TOOL_NAME}}),
    ),
  ] {
    let (base_url, captured) = spawn_mock_server(200, OPENAI_SUCCESS.to_string()).await;
    let provider =
      OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    let _ = provider
      .execute(&provider_request_with_choice("gpt-4o-mini", choice.clone()))
      .await
      .expect("ok");
    let captured = captured.lock().await;
    let body = captured_body(captured.as_deref().expect("body captured"));
    assert_eq!(
      body.get("tool_choice"),
      Some(&expected),
      "OpenAI tool_choice {:?} mismatch: {}",
      choice,
      body
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_tool_choice_all_modes() {
  for (choice, expected) in [
    (ToolChoice::Auto, json!("auto")),
    (ToolChoice::None, json!("none")),
    (ToolChoice::Required, json!("required")),
    (
      ToolChoice::Tool {
        name: TOOL_CHOICE_TOOL_NAME.to_string(),
      },
      json!({"type":"function","function":{"name": TOOL_CHOICE_TOOL_NAME}}),
    ),
  ] {
    let (base_url, captured) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
    let provider = MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let _ = provider
      .execute(&provider_request_with_choice(
        "moonshot-v1-8k",
        choice.clone(),
      ))
      .await
      .expect("ok");
    let captured = captured.lock().await;
    let body = captured_body(captured.as_deref().expect("body captured"));
    assert_eq!(
      body.get("tool_choice"),
      Some(&expected),
      "Moonshot tool_choice {:?} mismatch: {}",
      choice,
      body
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_tool_choice_all_modes() {
  for (choice, expected) in [
    (ToolChoice::Auto, json!("auto")),
    (ToolChoice::None, json!("none")),
    (ToolChoice::Required, json!("required")),
    (
      ToolChoice::Tool {
        name: TOOL_CHOICE_TOOL_NAME.to_string(),
      },
      json!({"type":"function","function":{"name": TOOL_CHOICE_TOOL_NAME}}),
    ),
  ] {
    let (base_url, captured) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
    let provider = StepFunProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let _ = provider
      .execute(&provider_request_with_choice(
        "step-3.5-flash",
        choice.clone(),
      ))
      .await
      .expect("ok");
    let captured = captured.lock().await;
    let body = captured_body(captured.as_deref().expect("body captured"));
    assert_eq!(
      body.get("tool_choice"),
      Some(&expected),
      "StepFun tool_choice {:?} mismatch: {}",
      choice,
      body
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_choice_all_modes() {
  for (choice, expected) in [
    (ToolChoice::Auto, json!({"type":"auto"})),
    (ToolChoice::None, json!({"type":"none"})),
    (ToolChoice::Required, json!({"type":"any"})),
    (
      ToolChoice::Tool {
        name: TOOL_CHOICE_TOOL_NAME.to_string(),
      },
      json!({"type":"tool","name": TOOL_CHOICE_TOOL_NAME}),
    ),
  ] {
    let (base_url, captured) = spawn_mock_server(200, ANTHROPIC_SUCCESS.to_string()).await;
    let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
      .expect("provider");
    let _ = provider
      .execute(&provider_request_with_choice(
        "claude-3-5-sonnet",
        choice.clone(),
      ))
      .await
      .expect("ok");
    let captured = captured.lock().await;
    let body = captured_body(captured.as_deref().expect("body captured"));
    assert_eq!(
      body.get("tool_choice"),
      Some(&expected),
      "Anthropic tool_choice {:?} mismatch: {}",
      choice,
      body
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_tool_choice_all_modes() {
  for (choice, expected) in [
    (
      ToolChoice::Auto,
      json!({"functionCallingConfig":{"mode":"AUTO"}}),
    ),
    (
      ToolChoice::None,
      json!({"functionCallingConfig":{"mode":"NONE"}}),
    ),
    (
      ToolChoice::Required,
      json!({"functionCallingConfig":{"mode":"ANY"}}),
    ),
    (
      ToolChoice::Tool {
        name: TOOL_CHOICE_TOOL_NAME.to_string(),
      },
      json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[TOOL_CHOICE_TOOL_NAME]}}),
    ),
  ] {
    let (base_url, captured) = spawn_mock_server(200, GOOGLE_SUCCESS.to_string()).await;
    let provider =
      GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
    let _ = provider
      .execute(&provider_request_with_choice(
        "gemini-1.5-pro",
        choice.clone(),
      ))
      .await
      .expect("ok");
    let captured = captured.lock().await;
    let body = captured_body(captured.as_deref().expect("body captured"));
    assert_eq!(
      body.get("toolConfig"),
      Some(&expected),
      "Google toolConfig {:?} mismatch: {}",
      choice,
      body
    );
  }
}

// -----------------------------------------------------------------------------
// Mock provider consistency
//
// The Mock provider does not make HTTP calls, so it can't go through the
// streaming TCP mock helpers above. The tests here lock down the behavior the
// rest of the workspace (yanshi-agents, yanshi-cli `eval run` fixtures,
// example smoke tests) relies on:
//
//   1. Default `execute()` returns text content + populated `TokenUsage` +
//      `StopReason::Stop`.
//   2. `with_tool_calls(...)` queues a ToolCalls response and `stop_reason`
//      flips to `StopReason::ToolCalls`.
//   3. `execute_streaming()` yields at least one chunk with `is_final = true`
//      and `content_type = Some("text")`.
//
// These are the same invariants the per-provider HTTP tests assert; the Mock
// provider sits in the same matrix because every offline example, eval run,
// and hermetic CI suite drives through it.
// -----------------------------------------------------------------------------

#[tokio::test]
async fn mock_success_path() {
  use yanshi_llm::providers::MockProvider;
  let provider = MockProvider::new("", None)
    .expect("mock")
    .with_response("ok");
  let response = provider
    .execute(&provider_request("mock-model"))
    .await
    .expect("ok");
  assert_text(&response.content, "ok");
  assert_usage(&response.usage);
  assert_eq!(response.stop_reason, Some(StopReason::Stop));
  assert!(
    response.tool_calls.is_empty(),
    "default Mock response carries no tool calls"
  );
}

#[tokio::test]
async fn mock_tool_call_path() {
  use yanshi_llm::providers::MockProvider;
  let queued = vec![ToolCallRequest {
    id: "call_mock_1".to_string(),
    name: TOOL_CHOICE_TOOL_NAME.to_string(),
    arguments: json!({"city": "Tokyo"}),
  }];
  let provider = MockProvider::new("", None)
    .expect("mock")
    .with_response("ok")
    .with_tool_calls(queued);
  let response = provider
    .execute(&provider_request_with_tools("mock-model"))
    .await
    .expect("ok");
  assert_tool_call(&response.tool_calls, TOOL_CHOICE_TOOL_NAME, "Tokyo");
  assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
}

#[tokio::test]
async fn mock_streaming_path() {
  use yanshi_llm::providers::MockProvider;
  let provider = MockProvider::new("", None)
    .expect("mock")
    .with_response("Hello world");
  let mut stream = provider
    .execute_streaming(&provider_request_streaming("mock-model"))
    .await
    .expect("stream");

  let mut text = String::new();
  let mut saw_final = false;
  while let Some(chunk) = stream.next_chunk().await.expect("chunk") {
    assert_eq!(
      chunk.content_type.as_deref(),
      Some("text"),
      "Mock provider must emit text content_type"
    );
    if chunk.is_final {
      saw_final = true;
    }
    text.push_str(&chunk.content);
  }
  assert_eq!(text, "Hello world");
  assert!(
    saw_final,
    "Mock stream must terminate with is_final = true on at least one chunk"
  );
  assert!(
    stream
      .next_chunk()
      .await
      .expect("next_chunk after drain must not error")
      .is_none(),
    "draining a finished Mock stream should yield Ok(None)"
  );
}

// =============================================================================
// N9 cross-provider invariant tests
// =============================================================================
//
// The 35+ per-provider tests above each fire ONE provider through its native
// wire format and assert the canonical `ProviderResponse` / `LLMError` shape.
// They prove every provider individually maps to the contract.
//
// The tests below take this one step further: they fire ALL providers in ONE
// test function and assert the canonical outputs agree byte-for-byte.
// Per-provider drift (e.g. one provider starts returning `stop_reason: None`
// when it used to return `Some(Stop)`) fails here as a single test, with
// every provider's actual output visible in the panic message.
//
// These are the "cross-provider invariant" tests the N9 follow-up calls for.
// They share the per-provider success / tool-call / streaming / error
// fixtures already defined above so the wire formats stay in lockstep with
// the per-provider suite.

/// Build every supported provider against its native success-path fixture and
/// return the parsed [`ProviderResponse`]s in deterministic order so the
/// caller can assert structural equivalence.
///
/// Each provider runs on its own mock server (separate ephemeral TCP port);
/// the helper awaits them sequentially so the test stays single-threaded-
/// deterministic on the request capture path.
async fn drive_all_providers_through_success_path()
-> Vec<(&'static str, yanshi_llm::providers::ProviderResponse)> {
  let mut out = Vec::new();

  let (base_url, _) = spawn_mock_server(200, OPENAI_SUCCESS.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  out.push((
    "openai",
    provider
      .execute(&provider_request("gpt-4o-mini"))
      .await
      .expect("openai ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, ANTHROPIC_SUCCESS.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  out.push((
    "anthropic",
    provider
      .execute(&provider_request("claude-3-5-sonnet"))
      .await
      .expect("anthropic ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, GOOGLE_SUCCESS.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  out.push((
    "google",
    provider
      .execute(&provider_request("gemini-1.5-pro"))
      .await
      .expect("google ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  out.push((
    "moonshot",
    provider
      .execute(&provider_request("moonshot-v1-8k"))
      .await
      .expect("moonshot ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  out.push((
    "stepfun",
    provider
      .execute(&provider_request("step-3.5-flash"))
      .await
      .expect("stepfun ok"),
  ));

  out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_success_paths_produce_uniform_response_shape() {
  // Single test that fires all 5 providers through their native success
  // fixtures and asserts the parsed `ProviderResponse` shape is uniform.
  // Catches drift the per-provider tests would miss (e.g. one provider
  // stops surfacing usage metadata while the others still do).
  let responses = drive_all_providers_through_success_path().await;
  assert_eq!(responses.len(), 5, "5 mainstream providers covered");

  // Per-provider shape is already pinned by `*_success_path` tests. Here
  // we assert the cross-provider equality on every field that's part of
  // the canonical contract.
  let mut texts: Vec<String> = Vec::new();
  for (name, response) in &responses {
    assert_text(&response.content, "ok");
    assert_eq!(
      response.stop_reason,
      Some(StopReason::Stop),
      "{name} must report StopReason::Stop on the success path"
    );
    assert_usage(&response.usage);
    assert!(
      response.tool_calls.is_empty(),
      "{name} must not emit tool_calls on a plain success path; got {:?}",
      response.tool_calls
    );
    texts.push(response.content.to_string());
  }
  let first = &texts[0];
  for (i, text) in texts.iter().enumerate() {
    assert_eq!(
      text, first,
      "provider #{i} ({}) text diverged: expected {first:?}, got {text:?}",
      responses[i].0
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_tool_call_paths_produce_uniform_canonical_shape() {
  // Same idea for the tool-call path. Each provider has its own wire
  // format (`tool_calls` / `tool_use` / `functionCall`); the canonical
  // `ToolCallRequest { name, arguments, id }` must be identical
  // regardless of source. The shape pins are:
  //   - exactly one tool call per response
  //   - name == "get_weather"
  //   - arguments.city == "Tokyo"
  //   - id is non-empty (synthesised for providers without one)
  //   - stop_reason == ToolCalls
  let mut responses: Vec<(&str, yanshi_llm::providers::ProviderResponse)> = Vec::new();

  let (base_url, _) = spawn_mock_server(200, OPENAI_TOOL_CALL.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  responses.push((
    "openai",
    provider
      .execute(&provider_request_with_tools("gpt-4o-mini"))
      .await
      .expect("openai tool ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, ANTHROPIC_TOOL_CALL.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  responses.push((
    "anthropic",
    provider
      .execute(&provider_request_with_tools("claude-3-5-sonnet"))
      .await
      .expect("anthropic tool ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, GOOGLE_TOOL_CALL.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  responses.push((
    "google",
    provider
      .execute(&provider_request_with_tools("gemini-1.5-pro"))
      .await
      .expect("google tool ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, MOONSHOT_TOOL_CALL.to_string()).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  responses.push((
    "moonshot",
    provider
      .execute(&provider_request_with_tools("moonshot-v1-8k"))
      .await
      .expect("moonshot tool ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, STEPFUN_TOOL_CALL.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  responses.push((
    "stepfun",
    provider
      .execute(&provider_request_with_tools("step-3.5-flash"))
      .await
      .expect("stepfun tool ok"),
  ));

  assert_eq!(responses.len(), 5);
  for (name, response) in &responses {
    assert_eq!(
      response.tool_calls.len(),
      1,
      "{name} must emit exactly one tool call; got {} for {:?}",
      response.tool_calls.len(),
      response.tool_calls
    );
    let call = &response.tool_calls[0];
    assert_eq!(call.name, "get_weather", "{name} tool name diverged");
    let city = call
      .arguments
      .get("city")
      .and_then(|v| v.as_str())
      .unwrap_or_else(|| panic!("{name} tool arguments missing city: {:?}", call.arguments));
    assert_eq!(city, "Tokyo", "{name} tool city diverged");
    assert!(
      !call.id.is_empty(),
      "{name} tool call id must be non-empty (synthesised if vendor omitted)"
    );
    assert_eq!(
      response.stop_reason,
      Some(StopReason::ToolCalls),
      "{name} must report StopReason::ToolCalls when emitting a tool call"
    );
  }
}

/// Helper for the cross-provider HTTP-error invariant tests. Drives every
/// provider through `spawn_mock_server(status, body)` with a vendor-shaped
/// error payload and returns `(provider_name, observed_error)` tuples so the
/// caller can assert uniformity across the matrix.
async fn drive_all_providers_through_status(status: u16) -> Vec<(&'static str, LLMError)> {
  // Provider-agnostic JSON error body. `LLMError::HttpError` only pins
  // `status_code`, not body parsing, so any well-formed JSON works for the
  // cross-provider mapping invariant. `status` flows through to
  // `spawn_mock_server` below — it's the actual variable under test.
  let body = r#"{"error":{"message":"mock failure","type":"mock"}}"#.to_string();

  let mut errors: Vec<(&'static str, LLMError)> = Vec::new();

  let (base_url, _) = spawn_mock_server(status, body.clone()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  errors.push((
    "openai",
    provider
      .execute(&provider_request("gpt-4o-mini"))
      .await
      .expect_err("openai must surface error"),
  ));

  let (base_url, _) = spawn_mock_server(status, body.clone()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  errors.push((
    "anthropic",
    provider
      .execute(&provider_request("claude-3-5-sonnet"))
      .await
      .expect_err("anthropic must surface error"),
  ));

  let (base_url, _) = spawn_mock_server(status, body.clone()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  errors.push((
    "google",
    provider
      .execute(&provider_request("gemini-1.5-pro"))
      .await
      .expect_err("google must surface error"),
  ));

  let (base_url, _) = spawn_mock_server(status, body.clone()).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  errors.push((
    "moonshot",
    provider
      .execute(&provider_request("moonshot-v1-8k"))
      .await
      .expect_err("moonshot must surface error"),
  ));

  let (base_url, _) = spawn_mock_server(status, body).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  errors.push((
    "stepfun",
    provider
      .execute(&provider_request("step-3.5-flash"))
      .await
      .expect_err("stepfun must surface error"),
  ));

  errors
}

fn assert_cross_provider_http_error(errors: Vec<(&'static str, LLMError)>, expected_status: u16) {
  assert_eq!(errors.len(), 5, "5 providers in the cross-provider matrix");
  for (name, err) in &errors {
    match err {
      LLMError::HttpError { status_code, .. } => {
        assert_eq!(
          *status_code, expected_status,
          "{name} surfaced status {status_code} but matrix expects {expected_status}"
        );
      }
      other => panic!(
        "{name} surfaced {other:?}; matrix expects LLMError::HttpError {{ status_code: {expected_status}, .. }}"
      ),
    }
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_401_maps_uniformly_to_http_error() {
  let errors = drive_all_providers_through_status(401).await;
  assert_cross_provider_http_error(errors, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_429_maps_uniformly_to_http_error() {
  // P0.3 invariant: every provider's 429 must surface as
  // `LLMError::HttpError { status_code: 429, .. }`. Per-provider
  // `*_maps_429_to_http_error` tests pin this individually; the
  // cross-provider variant catches "one provider silently downgrades
  // rate-limit to a different error variant" the moment it lands.
  let errors = drive_all_providers_through_status(429).await;
  assert_cross_provider_http_error(errors, 429);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_500_maps_uniformly_to_http_error() {
  let errors = drive_all_providers_through_status(500).await;
  assert_cross_provider_http_error(errors, 500);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_streaming_paths_yield_uniform_hello_world_concatenation() {
  // Each provider's streaming path produces 2 chunks ("hello" + " world")
  // with `is_final=true` on the last. The per-provider tests pin the
  // wire-format-specific framing; this test asserts the cross-provider
  // invariant: regardless of framing, the drained text is exactly
  // "hello world" and the stream terminates cleanly.

  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("gpt-4o-mini")).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("gpt-4o-mini"))
    .await
    .expect("openai stream");
  assert_stream_yields_hello_world(stream).await;

  let base_url = spawn_streaming_mock_server(anthropic_stream_events()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("claude-3-5-sonnet"))
    .await
    .expect("anthropic stream");
  assert_stream_yields_hello_world(stream).await;

  let base_url = spawn_streaming_mock_server(google_stream_events()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("gemini-1.5-pro"))
    .await
    .expect("google stream");
  assert_stream_yields_hello_world(stream).await;

  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("moonshot-v1-8k")).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("moonshot-v1-8k"))
    .await
    .expect("moonshot stream");
  assert_stream_yields_hello_world(stream).await;

  let base_url = spawn_streaming_mock_server(openai_compat_stream_events("step-3.5-flash")).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  let stream = provider
    .execute_streaming(&provider_request_streaming("step-3.5-flash"))
    .await
    .expect("stepfun stream");
  assert_stream_yields_hello_world(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_token_usage_populated_uniformly_on_success() {
  // Token usage is part of the canonical contract — billing / cost
  // tracking depends on it. This test pins that every provider's
  // success-path response surfaces a populated `TokenUsage` with
  // `total_tokens` non-zero. Per-provider tests assert this individually;
  // the cross-provider variant catches drift cheaply.
  let responses = drive_all_providers_through_success_path().await;
  for (name, response) in &responses {
    let usage = response
      .usage
      .as_ref()
      .unwrap_or_else(|| panic!("{name} usage must be populated on success"));
    assert!(
      usage.total_tokens.unwrap_or(0) > 0,
      "{name} must surface total_tokens > 0; got {:?}",
      usage.total_tokens
    );
    assert!(
      usage.prompt_tokens.unwrap_or(0) > 0,
      "{name} must surface prompt_tokens > 0; got {:?}",
      usage.prompt_tokens
    );
    assert!(
      usage.completion_tokens.unwrap_or(0) > 0,
      "{name} must surface completion_tokens > 0; got {:?}",
      usage.completion_tokens
    );
  }
}

// -----------------------------------------------------------------------------
// N9 cross-provider invariants — multimodal + tool_choice
//
// The per-provider `*_multimodal_path` and `*_tool_choice_all_modes` tests
// pin every provider's request-encoding wire shape individually. The
// invariants below take that one step further: fire all 5 providers in ONE
// test and assert the canonical contract holds across the matrix.
//
// **Multimodal invariant** (`cross_provider_multimodal_paths_produce_uniform_response_shape`):
// drives each provider through its native multimodal request shape and
// asserts the parsed `ProviderResponse` is uniform (text == "ok",
// StopReason::Stop, usage populated, no tool_calls). Catches the drift mode
// where one provider's multimodal adapter starts mis-parsing the success
// response (e.g. dropping usage on image inputs).
//
// **Tool-choice invariants** (4 tests, one per `ToolChoice` variant):
// drives each provider with the given variant and captures the request body.
// Asserts every provider's body has a *non-empty* mode-bearing
// `tool_choice` (or `toolConfig` for Google) field whose stringified form
// contains the expected mode marker. This is the silent-drop / silent-
// downgrade invariant: a provider that starts dropping the field, or
// silently maps Required → Auto, fails here.
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_multimodal_paths_produce_uniform_response_shape() {
  // Every provider is fed the same OpenAI-shaped `image_url` request (the
  // one shape `MultimodalMessage::to_openai_format()` ever produces); each
  // adapter's `build_request_body` translates it into its own native wire
  // shape internally (OpenAI/Moonshot/StepFun: `image_url` parts passed
  // through as-is; Anthropic: native `image` parts with a base64 `source`;
  // Google: rewritten into `inline_data`). The success response shape must
  // be identical across the matrix regardless.
  let mut responses: Vec<(&str, yanshi_llm::providers::ProviderResponse)> = Vec::new();

  let (base_url, _) = spawn_mock_server(200, OPENAI_SUCCESS.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  responses.push((
    "openai",
    provider
      .execute(&provider_request_multimodal_openai_style("gpt-4o-mini"))
      .await
      .expect("openai multimodal ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, ANTHROPIC_SUCCESS.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  responses.push((
    "anthropic",
    provider
      .execute(&provider_request_multimodal_openai_style(
        "claude-3-5-sonnet",
      ))
      .await
      .expect("anthropic multimodal ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, GOOGLE_SUCCESS.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  responses.push((
    "google",
    provider
      .execute(&provider_request_multimodal_openai_style("gemini-1.5-pro"))
      .await
      .expect("google multimodal ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  responses.push((
    "moonshot",
    provider
      .execute(&provider_request_multimodal_openai_style("moonshot-v1-8k"))
      .await
      .expect("moonshot multimodal ok"),
  ));

  let (base_url, _) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  responses.push((
    "stepfun",
    provider
      .execute(&provider_request_multimodal_openai_style("step-3.5-flash"))
      .await
      .expect("stepfun multimodal ok"),
  ));

  assert_eq!(responses.len(), 5, "5 mainstream providers covered");

  let mut texts: Vec<String> = Vec::new();
  for (name, response) in &responses {
    assert_text(&response.content, "ok");
    assert_eq!(
      response.stop_reason,
      Some(StopReason::Stop),
      "{name} multimodal success must report StopReason::Stop"
    );
    assert_usage(&response.usage);
    assert!(
      response.tool_calls.is_empty(),
      "{name} multimodal success must not emit tool_calls; got {:?}",
      response.tool_calls
    );
    texts.push(response.content.to_string());
  }
  let first = &texts[0];
  for (i, text) in texts.iter().enumerate() {
    assert_eq!(
      text, first,
      "provider #{i} ({}) multimodal text diverged: expected {first:?}, got {text:?}",
      responses[i].0
    );
  }
}

/// Drive every provider with the given [`ToolChoice`] through its success
/// fixture and return `(name, captured_body_json)` tuples. Each provider
/// runs on its own ephemeral mock; the helper awaits sequentially so the
/// capture path stays deterministic.
async fn drive_all_providers_through_tool_choice(
  choice: ToolChoice,
) -> Vec<(&'static str, serde_json::Value)> {
  let mut out = Vec::new();

  let (base_url, captured) = spawn_mock_server(200, OPENAI_SUCCESS.to_string()).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("openai provider");
  let _ = provider
    .execute(&provider_request_with_choice("gpt-4o-mini", choice.clone()))
    .await
    .expect("openai tool_choice ok");
  let raw = captured.lock().await.clone().expect("openai body captured");
  out.push(("openai", captured_body(&raw)));

  let (base_url, captured) = spawn_mock_server(200, ANTHROPIC_SUCCESS.to_string()).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("anthropic provider");
  let _ = provider
    .execute(&provider_request_with_choice(
      "claude-3-5-sonnet",
      choice.clone(),
    ))
    .await
    .expect("anthropic tool_choice ok");
  let raw = captured
    .lock()
    .await
    .clone()
    .expect("anthropic body captured");
  out.push(("anthropic", captured_body(&raw)));

  let (base_url, captured) = spawn_mock_server(200, GOOGLE_SUCCESS.to_string()).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("google provider");
  let _ = provider
    .execute(&provider_request_with_choice(
      "gemini-1.5-pro",
      choice.clone(),
    ))
    .await
    .expect("google tool_choice ok");
  let raw = captured.lock().await.clone().expect("google body captured");
  out.push(("google", captured_body(&raw)));

  let (base_url, captured) = spawn_mock_server(200, MOONSHOT_SUCCESS.to_string()).await;
  let provider = MoonshotProvider::with_client(no_proxy_client(), "k", Some(base_url))
    .expect("moonshot provider");
  let _ = provider
    .execute(&provider_request_with_choice(
      "moonshot-v1-8k",
      choice.clone(),
    ))
    .await
    .expect("moonshot tool_choice ok");
  let raw = captured
    .lock()
    .await
    .clone()
    .expect("moonshot body captured");
  out.push(("moonshot", captured_body(&raw)));

  let (base_url, captured) = spawn_mock_server(200, STEPFUN_SUCCESS.to_string()).await;
  let provider =
    StepFunProvider::with_client(no_proxy_client(), "k", Some(base_url)).expect("stepfun provider");
  let _ = provider
    .execute(&provider_request_with_choice("step-3.5-flash", choice))
    .await
    .expect("stepfun tool_choice ok");
  let raw = captured
    .lock()
    .await
    .clone()
    .expect("stepfun body captured");
  out.push(("stepfun", captured_body(&raw)));

  out
}

/// Extract the provider-specific tool-choice field for cross-provider mode
/// assertions. OpenAI / Anthropic / Moonshot / StepFun all use the
/// canonical `tool_choice` key; Google's Gemini wire shape moves the same
/// information into `toolConfig`.
fn provider_tool_choice_field<'a>(
  provider: &str,
  body: &'a serde_json::Value,
) -> &'a serde_json::Value {
  let field = match provider {
    "google" => "toolConfig",
    _ => "tool_choice",
  };
  body.get(field).unwrap_or_else(|| {
    panic!("{provider} body must encode tool_choice; missing `{field}` field in {body}")
  })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_tool_choice_auto_is_honored_by_every_provider() {
  // Auto = let the model decide. Per the provider matrix:
  //   openai / moonshot / stepfun → `tool_choice: "auto"`
  //   anthropic → `tool_choice: {"type":"auto"}`
  //   google → `toolConfig.functionCallingConfig.mode: "AUTO"`
  // The invariant we pin here is "every provider's mode-bearing field
  // contains the case-insensitive substring `auto` and is non-empty" — the
  // silent-drop / silent-downgrade drift catches against this.
  let bodies = drive_all_providers_through_tool_choice(ToolChoice::Auto).await;
  assert_eq!(bodies.len(), 5);
  for (name, body) in &bodies {
    let field = provider_tool_choice_field(name, body);
    let serialized = field.to_string().to_lowercase();
    assert!(
      serialized.contains("auto"),
      "{name} must encode ToolChoice::Auto in its tool-choice field; got {field}"
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_tool_choice_none_is_honored_by_every_provider() {
  // None = explicitly forbid tool calls. This is the highest-stakes
  // invariant: a provider silently dropping `None` would re-enable tool
  // calls the caller explicitly forbade. Per the provider matrix:
  //   openai / moonshot / stepfun → `tool_choice: "none"`
  //   anthropic → `tool_choice: {"type":"none"}`
  //   google → `toolConfig.functionCallingConfig.mode: "NONE"`
  let bodies = drive_all_providers_through_tool_choice(ToolChoice::None).await;
  assert_eq!(bodies.len(), 5);
  for (name, body) in &bodies {
    let field = provider_tool_choice_field(name, body);
    let serialized = field.to_string().to_lowercase();
    assert!(
      serialized.contains("none"),
      "{name} must encode ToolChoice::None in its tool-choice field; got {field}"
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_tool_choice_required_is_honored_by_every_provider() {
  // Required = force the model to call at least one tool. The wire token
  // varies across providers (`required` for OpenAI-shape vendors, `any` for
  // Anthropic, `ANY` for Google), so the invariant is "the field encodes a
  // non-Auto, non-None directive". We pin that by asserting one of the
  // known wire tokens is present.
  let bodies = drive_all_providers_through_tool_choice(ToolChoice::Required).await;
  assert_eq!(bodies.len(), 5);
  for (name, body) in &bodies {
    let field = provider_tool_choice_field(name, body);
    let serialized = field.to_string().to_lowercase();
    assert!(
      serialized.contains("required") || serialized.contains("any"),
      "{name} must encode ToolChoice::Required as `required` or `any`; got {field}"
    );
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_provider_tool_choice_specific_tool_is_honored_by_every_provider() {
  // Specific tool = force the model to call this exact tool. Every
  // provider's wire shape must embed the requested tool name. Per the
  // matrix:
  //   openai / moonshot / stepfun → `tool_choice: {"type":"function","function":{"name":"get_weather"}}`
  //   anthropic → `tool_choice: {"type":"tool","name":"get_weather"}`
  //   google → `toolConfig.functionCallingConfig.allowedFunctionNames: ["get_weather"]`
  // The invariant: the captured field contains the tool name string.
  let bodies = drive_all_providers_through_tool_choice(ToolChoice::Tool {
    name: TOOL_CHOICE_TOOL_NAME.to_string(),
  })
  .await;
  assert_eq!(bodies.len(), 5);
  for (name, body) in &bodies {
    let field = provider_tool_choice_field(name, body);
    let serialized = field.to_string();
    assert!(
      serialized.contains(TOOL_CHOICE_TOOL_NAME),
      "{name} must embed the requested tool name `{TOOL_CHOICE_TOOL_NAME}` in its tool-choice field; got {field}"
    );
  }
}
