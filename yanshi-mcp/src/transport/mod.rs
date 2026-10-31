//! MCP transport layer implementations
//!
//! This module provides transport implementations for communicating with MCP servers
//! using various mechanisms (stdio, HTTP, etc.).
//!
//! # Transports
//!
//! - **Stdio**: Communicates with local processes via stdin/stdout
//! - **HTTP**: Communicates with remote servers via HTTP (future)
//! - **HTTP+SSE**: HTTP with Server-Sent Events for bidirectional communication (future)
//!
//! # Example
//!
//! ```no_run
//! use yanshi_mcp::transport::{StdioTransport, Transport};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut transport = StdioTransport::new(vec![
//!   "npx".to_string(),
//!   "-y".to_string(),
//!   "@modelcontextprotocol/server-everything".to_string(),
//! ]);
//!
//! transport.connect().await?;
//! # Ok(())
//! # }
//! ```

pub mod mock;
pub mod stdio;
pub mod streamable_http;
pub mod traits;

// Re-export commonly used types
pub use mock::MockTransport;
pub use stdio::StdioTransport;
pub use streamable_http::StreamableHttpTransport;
pub use traits::{Transport, TransportConfig, TransportType};

/// Queues a server-initiated message for `receive_message` without
/// waiting. A transport's reader also delivers responses, so blocking on
/// a full queue would stall every in-flight request; overflow is dropped.
pub(crate) fn queue_notification(
  queue: &tokio::sync::mpsc::Sender<serde_json::Value>,
  message: serde_json::Value,
  transport: &'static str,
) {
  if let Err(tokio::sync::mpsc::error::TrySendError::Full(dropped)) = queue.try_send(message) {
    tracing::warn!(
      target = "yanshi_mcp::transport",
      transport,
      method = ?dropped.get("method"),
      "notification queue full; dropping server notification"
    );
  }
}
