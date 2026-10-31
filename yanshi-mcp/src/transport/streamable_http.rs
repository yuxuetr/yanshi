//! Streamable HTTP transport (client side) — Modern-era (`2026-07-28`)
//! Streamable HTTP v2.
//!
//! W5.8-3 (`docs/RFC_MCP_PROTOCOL_MODERNIZATION.md` Phase 2). Per the
//! RFC's "Transport trait fit" finding, this is implemented directly
//! against the existing [`Transport`] trait — no redesign needed. The
//! Modern spec's SSE response is scoped to exactly one request (no
//! persistent GET stream, no session, no resumability) and closes on its
//! own once the final JSON-RPC response event arrives, so unlike
//! [`crate::transport::StdioTransport`]'s indefinite background reader
//! task, this transport can simply read the whole HTTP response (single
//! JSON object or a bounded SSE stream) per call and hand any
//! `notifications/*` events it finds off to the same
//! [`Transport::receive_message`] queue pattern.
//!
//! ## Header/body mirroring
//!
//! Per the RFC, every POST carries `MCP-Protocol-Version` / `Mcp-Method`
//! / (for `tools/call`/`resources/read`/`prompts/get`) `Mcp-Name` headers
//! mirroring the JSON-RPC body. `Mcp-Name` is read from whichever params
//! field that method actually carries the entity name/URI under in this
//! crate's existing client code (`client/tools.rs`: `tools/call` uses
//! `params.name`; `client/resources.rs`: `resources/read` uses
//! `params.uri`; `client/prompts.rs`: `prompts/get` uses `params.name`).
//!
//! ## Response-body handling / era-probe support
//!
//! [`Transport::send_message`] returns `Ok(value)` for **any** body that
//! parses as JSON, regardless of HTTP status (`200`, `400`, `404`, ...)
//! — including a `400` body shaped like `UnsupportedProtocolVersionError`
//! or `HeaderMismatch`. Only an empty or non-JSON body is surfaced as an
//! `Err`. This split matters for W5.8-4's era-probe, which per the RFC
//! distinguishes "a `400`/`404`/`405` with an unrecognized or empty body
//! means Legacy" from "a recognized modern error body means Modern" —
//! this transport does the body-shape half of that job; interpreting the
//! JSON-RPC error code is the client layer's job
//! ([`crate::protocol::modern::is_recognized_modern_error`]).

use crate::error::{JsonRpcErrorCode, MCPError, MCPResult};
use crate::protocol::modern::{
  MCP_PROTOCOL_VERSION_2026_07_28, MODERN_PROTOCOL_VERSION_META_FIELD,
};
use crate::transport::traits::{Transport, TransportConfig, TransportType};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// HTTP header carrying the protocol version, mirroring the JSON-RPC
/// body's `_meta` field.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
/// HTTP header carrying the JSON-RPC method name.
pub const MCP_METHOD_HEADER: &str = "Mcp-Method";
/// HTTP header carrying the target entity name/URI, for the three
/// methods below.
pub const MCP_NAME_HEADER: &str = "Mcp-Name";

/// Methods whose request carries an entity name/URI mirrored into the
/// `Mcp-Name` header (RFC: "for `tools/call`/`resources/read`/`prompts/get`").
const NAMED_METHODS: &[&str] = &["tools/call", "resources/read", "prompts/get"];

/// Streamable HTTP transport (client side).
pub struct StreamableHttpTransport {
  base_url: String,
  client: reqwest::Client,
  connected: Arc<AtomicBool>,
  notifications_tx: mpsc::Sender<Value>,
  notifications_rx: Arc<AsyncMutex<mpsc::Receiver<Value>>>,
  timeout: Duration,
  max_message_size: Arc<AtomicUsize>,
}

impl StreamableHttpTransport {
  /// Default timeout for HTTP operations (30 seconds).
  pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
  /// Default maximum response body size (10 MB), matching
  /// [`crate::transport::StdioTransport::DEFAULT_MAX_MESSAGE_SIZE`].
  pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;
  /// Default notifications channel capacity, matching
  /// [`crate::transport::StdioTransport::DEFAULT_NOTIFICATION_CHANNEL_CAPACITY`].
  pub const DEFAULT_NOTIFICATION_CHANNEL_CAPACITY: usize = 1024;

  /// Create a new transport targeting `base_url`, with a default
  /// production HTTP client (no proxy override — see
  /// [`Self::with_client`] for tests).
  pub fn new(base_url: impl Into<String>) -> MCPResult<Self> {
    let client = reqwest::Client::builder()
      .build()
      .map_err(|e| MCPError::configuration(format!("failed to build HTTP client: {e}")))?;
    Ok(Self::with_client(base_url, client))
  }

  /// Create a new transport with an explicit [`reqwest::Client`]. Tests
  /// that spin up a local loopback server **must** pass a client built
  /// with `.no_proxy()` — a system HTTP proxy (common on macOS/Windows
  /// dev machines) otherwise routes loopback requests through it and
  /// they black-hole, misreporting as a generic connection error.
  /// Production callers don't need `.no_proxy()`.
  pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
    let (notifications_tx, notifications_rx) =
      mpsc::channel(Self::DEFAULT_NOTIFICATION_CHANNEL_CAPACITY);
    Self {
      base_url: base_url.into(),
      client,
      connected: Arc::new(AtomicBool::new(false)),
      notifications_tx,
      notifications_rx: Arc::new(AsyncMutex::new(notifications_rx)),
      timeout: Duration::from_millis(Self::DEFAULT_TIMEOUT_MS),
      max_message_size: Arc::new(AtomicUsize::new(Self::DEFAULT_MAX_MESSAGE_SIZE)),
    }
  }

  /// Set the request timeout.
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  /// Set the maximum response body size.
  pub fn with_max_message_size(self, size: usize) -> Self {
    self.max_message_size.store(size, Ordering::SeqCst);
    self
  }

  async fn read_body_capped(&self, response: reqwest::Response) -> MCPResult<String> {
    let max_size = self.max_message_size.load(Ordering::SeqCst);
    if let Some(len) = response.content_length()
      && len as usize > max_size
    {
      return Err(MCPError::transport(format!(
        "HTTP response Content-Length {len} exceeds max_message_size {max_size}"
      )));
    }
    let bytes = response
      .bytes()
      .await
      .map_err(|e| MCPError::transport(format!("failed to read HTTP response body: {e}")))?;
    if bytes.len() > max_size {
      return Err(MCPError::transport(format!(
        "HTTP response body ({} bytes) exceeds max_message_size {max_size}",
        bytes.len()
      )));
    }
    String::from_utf8(bytes.to_vec())
      .map_err(|e| MCPError::transport(format!("HTTP response body was not valid UTF-8: {e}")))
  }

  async fn handle_sse_body(&self, body: &str, request_id: Option<&Value>) -> MCPResult<Value> {
    let events = parse_sse_events(body);
    let mut final_response: Option<Value> = None;
    for event in events {
      let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        // Not JSON — skip. `:`-prefixed keep-alive comments never reach
        // here (filtered by parse_sse_events); this only catches a
        // malformed `data:` payload.
        continue;
      };
      if is_final_response(&value, request_id) {
        final_response = Some(value);
      } else {
        // Server-initiated message scoped to this request (e.g.
        // `notifications/progress`). Forward to the same queue
        // `receive_message` drains for every other transport.
        crate::transport::queue_notification(&self.notifications_tx, value, "streamable_http");
      }
    }
    final_response.ok_or_else(|| {
      MCPError::protocol(
        "SSE stream ended without a matching final JSON-RPC response",
        JsonRpcErrorCode::InternalError,
      )
    })
  }
}

/// One parsed SSE frame's `data:` payload (concatenated across multiple
/// `data:` lines within the same frame, per the SSE spec). `event:` is
/// parsed but unused today — nothing in this crate branches on it yet;
/// every frame's `data:` is parsed as a JSON-RPC message directly.
struct SseEvent {
  data: String,
}

/// Minimal hand-rolled SSE frame parser for the per-request-scoped
/// stream shape: `event:`/`data:` lines terminated by a blank line, plus
/// `:`-prefixed keep-alive comments (ignored). No `id:`/`retry:` support
/// — not needed for a stream that's scoped to one request and never
/// resumed (Modern-era Streamable HTTP dropped resumability entirely).
fn parse_sse_events(body: &str) -> Vec<SseEvent> {
  fn flush(data_lines: &mut Vec<String>, events: &mut Vec<SseEvent>) {
    if !data_lines.is_empty() {
      events.push(SseEvent {
        data: data_lines.join("\n"),
      });
      data_lines.clear();
    }
  }

  let mut events = Vec::new();
  let mut data_lines: Vec<String> = Vec::new();

  for line in body.lines() {
    if line.is_empty() {
      flush(&mut data_lines, &mut events);
      continue;
    }
    if line.starts_with(':') {
      continue; // keep-alive comment
    }
    if let Some(value) = line.strip_prefix("data:") {
      data_lines.push(value.trim_start().to_string());
    }
    // Other fields (event:/id:/retry:) are intentionally not tracked —
    // see the SseEvent doc comment.
  }
  flush(&mut data_lines, &mut events);
  events
}

/// `true` when `value` is the final JSON-RPC response for the request
/// that produced `request_id` (matching `id`), as opposed to a
/// server-initiated message (no `id`, or an unrelated one) that should
/// be queued as a notification instead.
fn is_final_response(value: &Value, request_id: Option<&Value>) -> bool {
  match (value.get("id"), request_id) {
    (Some(id), Some(expected)) => id == expected,
    _ => false,
  }
}

fn request_id_value(request: &Value) -> Option<&Value> {
  request.get("id").filter(|id| !id.is_null())
}

fn request_method(request: &Value) -> String {
  request
    .get("method")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string()
}

/// Read `params._meta.io.modelcontextprotocol/protocolVersion` off a
/// request, falling back to `2026-07-28` (the only version this
/// transport variant implements) when absent — e.g. for a Legacy-shaped
/// request that hasn't gone through
/// [`crate::protocol::modern::inject_modern_meta_into_request`] yet.
fn modern_protocol_version_from_request(request: &Value) -> String {
  request
    .get("params")
    .and_then(|p| p.get("_meta"))
    .and_then(|m| m.get(MODERN_PROTOCOL_VERSION_META_FIELD))
    .and_then(Value::as_str)
    .map(str::to_string)
    .unwrap_or_else(|| MCP_PROTOCOL_VERSION_2026_07_28.to_string())
}

/// Compute the `Mcp-Name` header value for methods that carry one. See
/// the module doc comment for why the params field differs per method.
fn mcp_name_header_value(method: &str, request: &Value) -> Option<String> {
  if !NAMED_METHODS.contains(&method) {
    return None;
  }
  let params = request.get("params")?;
  let field = if method == "resources/read" {
    "uri"
  } else {
    "name"
  };
  params.get(field)?.as_str().map(str::to_string)
}

#[async_trait]
impl Transport for StreamableHttpTransport {
  async fn connect(&mut self) -> MCPResult<()> {
    // Stateless per spec — no handshake, no session to open. Just mark
    // the transport ready to send.
    self.connected.store(true, Ordering::SeqCst);
    Ok(())
  }

  async fn send_message(&self, request: Value) -> MCPResult<Value> {
    if !self.connected.load(Ordering::SeqCst) {
      return Err(MCPError::connection("Transport not connected"));
    }
    let request_id = request_id_value(&request).cloned();
    if request_id.is_none() {
      return Err(MCPError::transport(
        "send_message called with a request that has no JSON-RPC `id` field; \
         use send_notification for fire-and-forget messages",
      ));
    }
    let method = request_method(&request);
    let protocol_version = modern_protocol_version_from_request(&request);
    let mcp_name = mcp_name_header_value(&method, &request);

    let mut req_builder = self
      .client
      .post(&self.base_url)
      .header(
        reqwest::header::ACCEPT,
        "application/json, text/event-stream",
      )
      .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
      .header(MCP_METHOD_HEADER, method);
    if let Some(name) = mcp_name {
      req_builder = req_builder.header(MCP_NAME_HEADER, name);
    }
    let req_builder = req_builder.json(&request);

    let response = tokio::time::timeout(self.timeout, req_builder.send())
      .await
      .map_err(|_| {
        MCPError::timeout(
          format!("Request timeout after {:?}", self.timeout),
          Some(self.timeout.as_millis() as u64),
        )
      })?
      .map_err(|e| MCPError::transport(format!("HTTP request failed: {e}")))?;

    let status = response.status();
    let content_type = response
      .headers()
      .get(reqwest::header::CONTENT_TYPE)
      .and_then(|v| v.to_str().ok())
      .unwrap_or("")
      .to_string();
    let is_sse = content_type.contains("text/event-stream");

    let body = self.read_body_capped(response).await?;

    if is_sse {
      return self.handle_sse_body(&body, request_id.as_ref()).await;
    }

    if body.trim().is_empty() {
      return Err(MCPError::transport(format!(
        "HTTP {status} response had an empty body (unrecognized Streamable HTTP shape)"
      )));
    }

    serde_json::from_str::<Value>(&body).map_err(|e| {
      MCPError::transport(format!(
        "HTTP {status} response body was not valid JSON (unrecognized Streamable HTTP shape): {e}"
      ))
    })
  }

  async fn send_notification(&self, notification: Value) -> MCPResult<()> {
    if !self.connected.load(Ordering::SeqCst) {
      return Err(MCPError::connection("Transport not connected"));
    }
    let method = request_method(&notification);
    let protocol_version = modern_protocol_version_from_request(&notification);

    let response = tokio::time::timeout(
      self.timeout,
      self
        .client
        .post(&self.base_url)
        .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
        .header(MCP_METHOD_HEADER, method)
        .json(&notification)
        .send(),
    )
    .await
    .map_err(|_| {
      MCPError::timeout(
        format!("Notification timeout after {:?}", self.timeout),
        Some(self.timeout.as_millis() as u64),
      )
    })?
    .map_err(|e| MCPError::transport(format!("HTTP notification failed: {e}")))?;

    if !response.status().is_success() {
      return Err(MCPError::transport(format!(
        "HTTP notification got non-success status {}",
        response.status()
      )));
    }
    Ok(())
  }

  async fn receive_message(&self) -> MCPResult<Option<Value>> {
    let mut rx = self.notifications_rx.lock().await;
    match tokio::time::timeout(self.timeout, rx.recv()).await {
      Ok(Some(value)) => Ok(Some(value)),
      Ok(None) => Ok(None), // channel closed (disconnected)
      Err(_) => Ok(None),   // timeout — no message available
    }
  }

  async fn disconnect(&mut self) -> MCPResult<()> {
    self.connected.store(false, Ordering::SeqCst);
    Ok(())
  }

  fn is_connected(&self) -> bool {
    self.connected.load(Ordering::SeqCst)
  }

  fn transport_type(&self) -> TransportType {
    TransportType::StreamableHttp
  }
}

impl TransportConfig for StreamableHttpTransport {
  fn timeout_ms(&self) -> Option<u64> {
    Some(self.timeout.as_millis() as u64)
  }

  fn set_timeout_ms(&mut self, timeout: u64) {
    self.timeout = Duration::from_millis(timeout);
  }

  fn max_message_size(&self) -> Option<usize> {
    Some(self.max_message_size.load(Ordering::SeqCst))
  }

  fn set_max_message_size(&mut self, size: usize) {
    self.max_message_size.store(size, Ordering::SeqCst);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use std::io::{Read, Write};
  use std::net::TcpListener;

  fn test_client() -> reqwest::Client {
    // Rust-HTTP-testing rule: loopback requests must bypass any system
    // proxy (Clash/V2Ray etc. on 127.0.0.1) or they black-hole and
    // misreport as an opaque connection error.
    reqwest::Client::builder()
      .no_proxy()
      .build()
      .expect("test client")
  }

  /// Spawn a one-shot raw TCP HTTP/1.1 responder: accepts exactly one
  /// connection, reads the request (discarded), writes back
  /// `raw_response` verbatim, then the listener thread exits. Returns
  /// the `http://127.0.0.1:<port>` base URL.
  ///
  /// Hand-rolled rather than pulling in `hyper`/`wiremock` — this crate
  /// has no HTTP test-server dependency today (RFC: prefer hand-rolling
  /// over a new dependency where the wire format needed is narrow) and a
  /// fixed canned response is all these tests need.
  fn spawn_one_shot_http_server(raw_response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || {
      if let Ok((mut stream, _)) = listener.accept() {
        // Drain the request (headers + any body) before responding —
        // real clients expect the server to read the full request.
        let mut buf = [0u8; 8192];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(raw_response.as_bytes());
        let _ = stream.flush();
      }
    });
    format!("http://{addr}")
  }

  #[tokio::test]
  async fn send_message_parses_single_json_response() {
    let response_body = json!({
      "jsonrpc": "2.0",
      "id": 1,
      "result": { "tools": [] }
    })
    .to_string();
    let raw = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      response_body.len(),
      response_body
    );
    let raw: &'static str = Box::leak(raw.into_boxed_str());
    let base_url = spawn_one_shot_http_server(raw);

    let mut transport = StreamableHttpTransport::with_client(base_url, test_client());
    transport.connect().await.unwrap();

    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let response = transport.send_message(request).await.unwrap();
    assert_eq!(response["result"]["tools"], json!([]));
  }

  #[tokio::test]
  async fn send_message_parses_sse_response_and_queues_notifications() {
    let final_response = json!({ "jsonrpc": "2.0", "id": 7, "result": { "ok": true } });
    let notification =
      json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": { "pct": 50 } });
    let sse_body = format!(
      ": keep-alive comment, ignored\n\ndata: {}\n\ndata: {}\n\n",
      notification, final_response
    );
    let raw = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      sse_body.len(),
      sse_body
    );
    let raw: &'static str = Box::leak(raw.into_boxed_str());
    let base_url = spawn_one_shot_http_server(raw);

    let mut transport = StreamableHttpTransport::with_client(base_url, test_client());
    transport.connect().await.unwrap();

    let request =
      json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "search" } });
    let response = transport.send_message(request).await.unwrap();
    assert_eq!(response, final_response);

    let queued = transport.receive_message().await.unwrap();
    assert_eq!(queued, Some(notification));
  }

  #[tokio::test]
  async fn send_message_returns_ok_for_recognized_modern_error_body_on_400() {
    let error_body = json!({
      "jsonrpc": "2.0",
      "id": 1,
      "error": {
        "code": -32022,
        "message": "unsupported protocol version",
        "data": { "supported": ["2026-07-28"] }
      }
    })
    .to_string();
    let raw = format!(
      "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      error_body.len(),
      error_body
    );
    let raw: &'static str = Box::leak(raw.into_boxed_str());
    let base_url = spawn_one_shot_http_server(raw);

    let mut transport = StreamableHttpTransport::with_client(base_url, test_client());
    transport.connect().await.unwrap();

    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let response = transport.send_message(request).await.unwrap();
    assert_eq!(response["error"]["code"], json!(-32022));
  }

  #[tokio::test]
  async fn send_message_errors_on_empty_body() {
    let raw = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let base_url = spawn_one_shot_http_server(raw);

    let mut transport = StreamableHttpTransport::with_client(base_url, test_client());
    transport.connect().await.unwrap();

    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let result = transport.send_message(request).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn send_message_not_connected() {
    let transport = StreamableHttpTransport::with_client("http://127.0.0.1:1", test_client());
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let result = transport.send_message(request).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn send_message_without_id_is_rejected() {
    let mut transport = StreamableHttpTransport::with_client("http://127.0.0.1:1", test_client());
    transport.connect().await.unwrap();
    let request = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let result = transport.send_message(request).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("no JSON-RPC"));
  }

  #[test]
  fn transport_type_is_streamable_http() {
    let transport = StreamableHttpTransport::with_client("http://127.0.0.1:1", test_client());
    assert_eq!(transport.transport_type(), TransportType::StreamableHttp);
    assert!(transport.supports_server_messages());
    assert!(!transport.is_connected());
  }

  #[test]
  fn mcp_name_header_uses_uri_for_resources_read_and_name_otherwise() {
    assert_eq!(
      mcp_name_header_value(
        "resources/read",
        &json!({ "params": { "uri": "file:///a" } })
      ),
      Some("file:///a".to_string())
    );
    assert_eq!(
      mcp_name_header_value("tools/call", &json!({ "params": { "name": "search" } })),
      Some("search".to_string())
    );
    assert_eq!(
      mcp_name_header_value("prompts/get", &json!({ "params": { "name": "greet" } })),
      Some("greet".to_string())
    );
    assert_eq!(
      mcp_name_header_value("tools/list", &json!({ "params": { "name": "irrelevant" } })),
      None
    );
  }

  #[test]
  fn modern_protocol_version_falls_back_when_meta_absent() {
    assert_eq!(
      modern_protocol_version_from_request(&json!({ "method": "tools/list" })),
      MCP_PROTOCOL_VERSION_2026_07_28
    );
    assert_eq!(
      modern_protocol_version_from_request(&json!({
        "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": "2099-01-01" } }
      })),
      "2099-01-01"
    );
  }

  #[test]
  fn parse_sse_events_splits_on_blank_lines_and_ignores_comments() {
    let body = ": comment\n\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
    let events = parse_sse_events(body);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data, "{\"a\":1}");
    assert_eq!(events[1].data, "{\"b\":2}");
  }

  #[test]
  fn parse_sse_events_joins_multi_line_data() {
    let body = "data: line1\ndata: line2\n\n";
    let events = parse_sse_events(body);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "line1\nline2");
  }
}
