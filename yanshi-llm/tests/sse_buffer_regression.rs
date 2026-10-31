//! W0.3 regression: a single network read that contains multiple SSE
//! lines — the last real content delta immediately followed by the
//! stream's terminal marker — used to lose the trailing content (and
//! never observe `is_final`) on every provider except StepFun.
//!
//! `next_chunk()` used to drain the buffer only inside the branch that
//! just pulled fresh network bytes, returning on the *first* parsed
//! line and leaving any further already-buffered lines stranded until
//! another network read happened to re-trigger the drain. If the
//! underlying stream had already ended (as it always has by the time a
//! small canned test response finishes arriving over loopback), those
//! stranded lines were never drained — silently dropping content and
//! skipping the real terminal chunk.
//!
//! Each provider's server here writes its entire canned SSE body in one
//! `write_all` call and closes the connection immediately after, which
//! is what makes the whole body arrive as a single `bytes_stream()` item
//! in practice on loopback (same technique `trace_context_propagation.rs`
//! uses for byte-level control over the response).

use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use yanshi_llm::StreamingResponse;
use yanshi_llm::providers::{
  AnthropicProvider, GoogleProvider, LLMProvider, MoonshotProvider, OpenAIProvider, ProviderRequest,
};

/// Spawn a one-shot TCP listener that accepts a single HTTP/1.1 request,
/// drains it fully (so reqwest never sees a reset mid-request-write),
/// then replies with `body` as one `write_all` + immediate close.
async fn spawn_sse_server(body: &'static str) -> String {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("local_addr");

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

    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
      body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
  });

  tokio::time::sleep(Duration::from_millis(50)).await;
  format!("http://{addr}")
}

fn no_proxy_client() -> reqwest::Client {
  reqwest::Client::builder()
    .no_proxy()
    .pool_max_idle_per_host(0)
    .timeout(Duration::from_secs(10))
    .build()
    .expect("client")
}

fn streaming_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({"role": "user", "content": "ping"})],
    stream: true,
    parameters: std::collections::HashMap::new(),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

/// Drive `next_chunk()` to completion, returning (concatenated content,
/// whether any returned chunk had `is_final == true`).
async fn drain(mut response: Box<dyn StreamingResponse>) -> (String, bool) {
  let mut content = String::new();
  let mut saw_final = false;
  while let Some(chunk) = response.next_chunk().await.expect("next_chunk") {
    content.push_str(&chunk.content);
    if chunk.is_final {
      saw_final = true;
    }
  }
  (content, saw_final)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_single_read_trailing_content_and_done_are_not_dropped() {
  const BODY: &str = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"foo\"},\"finish_reason\":null}],\"usage\":null}\n\ndata: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"bar\"},\"finish_reason\":\"stop\"}],\"usage\":null}\n\ndata: [DONE]\n\n";
  let base_url = spawn_sse_server(BODY).await;
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let request = streaming_request("gpt-4o-mini");

  let response = provider.execute_streaming(&request).await.expect("stream");
  let (content, saw_final) = drain(response).await;

  assert_eq!(
    content, "foobar",
    "trailing content chunk must not be dropped"
  );
  assert!(saw_final, "the [DONE]-driven final chunk must be observed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_single_read_trailing_content_and_message_stop_are_not_dropped() {
  const BODY: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"foo\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"bar\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
  let base_url = spawn_sse_server(BODY).await;
  let provider = AnthropicProvider::with_client(no_proxy_client(), "test-key", Some(base_url))
    .expect("provider");
  let request = streaming_request("claude-3-5-sonnet-latest");

  let response = provider.execute_streaming(&request).await.expect("stream");
  let (content, saw_final) = drain(response).await;

  assert_eq!(
    content, "foobar",
    "trailing content chunk must not be dropped"
  );
  assert!(
    saw_final,
    "the message_stop-driven final chunk must be observed"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_single_read_trailing_content_and_finish_reason_are_not_dropped() {
  // The real `alt=sse` wire shape: `data: {json}` events, CRLF-framed.
  const BODY: &str = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"foo\"}]},\"finishReason\":null,\"index\":0}]}\r\n\r\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"bar\"}]},\"finishReason\":\"STOP\",\"index\":0}]}\r\n\r\n";
  let base_url = spawn_sse_server(BODY).await;
  let provider =
    GoogleProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let request = streaming_request("gemini-2.5-pro");

  let response = provider.execute_streaming(&request).await.expect("stream");
  let (content, saw_final) = drain(response).await;

  assert_eq!(
    content, "foobar",
    "trailing content chunk must not be dropped"
  );
  assert!(
    saw_final,
    "the finishReason-driven final chunk must be observed"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_single_read_trailing_content_and_done_are_not_dropped() {
  const BODY: &str = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"moonshot-v1-auto\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"foo\"},\"finish_reason\":null}],\"usage\":null}\n\ndata: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"moonshot-v1-auto\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"bar\"},\"finish_reason\":\"stop\"}],\"usage\":null}\n\ndata: [DONE]\n\n";
  let base_url = spawn_sse_server(BODY).await;
  let provider =
    MoonshotProvider::with_client(no_proxy_client(), "test-key", Some(base_url)).expect("provider");
  let request = streaming_request("moonshot-v1-auto");

  let response = provider.execute_streaming(&request).await.expect("stream");
  let (content, saw_final) = drain(response).await;

  assert_eq!(
    content, "foobar",
    "trailing content chunk must not be dropped"
  );
  assert!(saw_final, "the [DONE]-driven final chunk must be observed");
}
