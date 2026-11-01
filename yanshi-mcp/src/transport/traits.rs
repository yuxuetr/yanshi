//! Transport trait abstraction for MCP communication
//!
//! This module defines the core Transport trait that all MCP transports
//! must implement, providing a uniform interface for stdio, HTTP, and
//! future transport mechanisms.

use crate::error::MCPResult;
use async_trait::async_trait;
use serde_json::Value;

/// Transport type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
  /// Standard I/O transport (local process)
  Stdio,
  /// HTTP transport (remote server). Reflects the old, deprecated
  /// `2024-11-05` dual-endpoint HTTP+SSE shape's plain-POST half —
  /// unimplemented as an actual transport in this crate (stdio is the
  /// only real transport prior to W5.8-3).
  Http,
  /// HTTP with Server-Sent Events for bidirectional communication.
  /// Reflects the Legacy `2025-03-26`–`2025-11-25` session-based
  /// Streamable HTTP v1 shape (`Mcp-Session-Id`, persistent GET SSE
  /// stream, resumable). Also unimplemented as an actual transport.
  HttpWithSSE,
  /// Modern-era (`2026-07-28`) Streamable HTTP v2: stateless, POST-only,
  /// response is either a single JSON object or an SSE stream scoped to
  /// exactly that one request (no session, no persistent GET stream, no
  /// resumability). See [`crate::transport::StreamableHttpTransport`]
  /// (W5.8-3) and `docs/RFC_MCP_PROTOCOL_MODERNIZATION.md`.
  StreamableHttp,
}

impl std::fmt::Display for TransportType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Stdio => write!(f, "stdio"),
      Self::Http => write!(f, "http"),
      Self::HttpWithSSE => write!(f, "http+sse"),
      Self::StreamableHttp => write!(f, "streamable-http"),
    }
  }
}

/// Transport trait for MCP communication
///
/// All MCP transports (stdio, HTTP, etc.) must implement this trait to provide
/// a uniform interface for sending and receiving JSON-RPC messages.
///
/// # Example Implementation
///
/// ```ignore
/// use yanshi_mcp::transport::Transport;
/// use async_trait::async_trait;
///
/// struct MyTransport;
///
/// #[async_trait]
/// impl Transport for MyTransport {
///   async fn connect(&mut self) -> MCPResult<()> {
///     // Connection logic
///     Ok(())
///   }
///
///   async fn send_message(&mut self, request: Value) -> MCPResult<Value> {
///     // Send logic
///     Ok(serde_json::json!({}))
///   }
///
///   // ... other methods
/// }
/// ```
#[async_trait]
pub trait Transport: Send + Sync {
  /// Connect to the MCP server/endpoint.
  ///
  /// `connect` / `disconnect` keep their `&mut self` signature because
  /// they own the underlying I/O resource (e.g. for stdio: spawn the
  /// child process and start the demux reader task). Once connected,
  /// every per-request method below uses interior mutability so the
  /// client can fire concurrent `send_message` calls over the same
  /// connection (Q3.2.2) — pre-fix `MCPClient` wrapped the whole
  /// transport in `Arc<Mutex<Box<dyn Transport>>>` and serialized
  /// every RPC behind that lock.
  async fn connect(&mut self) -> MCPResult<()>;

  /// Send a request and wait for the matching response. Q3.2.2:
  /// `&self` so multiple concurrent calls can be in flight at once
  /// over the same transport. Implementations correlate responses
  /// back to the right caller via the JSON-RPC `id` field — see
  /// [`StdioTransport`](super::StdioTransport)'s reader-task + per-request oneshot
  /// demultiplexer.
  async fn send_message(&self, request: Value) -> MCPResult<Value>;

  /// Send a notification (no response expected). Q3.2.2: `&self`
  /// so notifications don't queue behind in-flight `send_message`
  /// calls.
  async fn send_notification(&self, notification: Value) -> MCPResult<()>;

  /// Receive a server-initiated message (e.g. notifications,
  /// progress updates). Q3.2.2: `&self` — the underlying reader
  /// task feeds notifications into an internal queue that multiple
  /// callers can drain.
  ///
  /// - `Ok(Some(message))` if a message was received
  /// - `Ok(None)` if no message is available (timeout / non-blocking)
  /// - `Err(...)` if an error occurred
  async fn receive_message(&self) -> MCPResult<Option<Value>>;

  /// Close the connection. See [`Self::connect`] for why this stays
  /// on `&mut self`.
  async fn disconnect(&mut self) -> MCPResult<()>;

  /// Check if the transport is currently connected
  ///
  /// # Returns
  ///
  /// `true` if connected and ready to send/receive, `false` otherwise.
  fn is_connected(&self) -> bool;

  /// Get the transport type
  ///
  /// # Returns
  ///
  /// The transport type identifier.
  fn transport_type(&self) -> TransportType;

  /// Check if this transport supports server-initiated messages
  ///
  /// # Returns
  ///
  /// `true` if the transport can receive server-initiated messages (e.g., stdio, HTTP+SSE).
  /// `false` for pure request-response transports (e.g., basic HTTP).
  fn supports_server_messages(&self) -> bool {
    matches!(
      self.transport_type(),
      TransportType::Stdio | TransportType::HttpWithSSE | TransportType::StreamableHttp
    )
  }
}

/// Transport configuration trait
///
/// Optional trait for transports that support runtime configuration.
pub trait TransportConfig {
  /// Get the connection timeout in milliseconds
  fn timeout_ms(&self) -> Option<u64> {
    None
  }

  /// Set the connection timeout in milliseconds
  fn set_timeout_ms(&mut self, timeout: u64);

  /// Get the maximum message size in bytes
  fn max_message_size(&self) -> Option<usize> {
    None
  }

  /// Set the maximum message size in bytes
  fn set_max_message_size(&mut self, size: usize);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_transport_type_display() {
    assert_eq!(TransportType::Stdio.to_string(), "stdio");
    assert_eq!(TransportType::Http.to_string(), "http");
    assert_eq!(TransportType::HttpWithSSE.to_string(), "http+sse");
    assert_eq!(TransportType::StreamableHttp.to_string(), "streamable-http");
  }

  #[test]
  fn test_transport_type_equality() {
    assert_eq!(TransportType::Stdio, TransportType::Stdio);
    assert_ne!(TransportType::Stdio, TransportType::Http);
    assert_ne!(TransportType::StreamableHttp, TransportType::HttpWithSSE);
  }

  // Note: Actual transport implementations will have their own tests
}
