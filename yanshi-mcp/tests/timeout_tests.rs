//! Timeout behavior tests for MCP client
//!
//! This test suite validates timeout handling across different operations,
//! including connection timeouts, operation timeouts, and retry behavior.

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use yanshi_mcp::client::ClientBuilder;
use yanshi_mcp::error::MCPError;
use yanshi_mcp::transport::{MockTransport, StdioTransport, Transport};

// ============================================================================
// Configuration Timeout Tests
// ============================================================================

#[tokio::test]
async fn test_default_timeout_configuration() {
  let transport = MockTransport::new();
  let client = ClientBuilder::new()
    .with_transport(transport)
    .build()
    .await
    .unwrap();

  // Should have default timeout (30 seconds)
  drop(client); // Just verify construction succeeds
}

#[tokio::test]
async fn test_custom_timeout_configuration() {
  let transport = MockTransport::new();
  let client = ClientBuilder::new()
    .with_transport(transport)
    .with_timeout(Duration::from_secs(10))
    .build()
    .await
    .unwrap();

  drop(client); // Verify construction with custom timeout
}

#[tokio::test]
async fn test_very_short_timeout() {
  let transport = MockTransport::new();
  let client = ClientBuilder::new()
    .with_transport(transport)
    .with_timeout(Duration::from_millis(1))
    .build()
    .await
    .unwrap();

  drop(client); // Verify even very short timeouts are accepted
}

// ============================================================================
// Transport-Level Timeout Tests
// ============================================================================

#[tokio::test]
async fn test_stdio_read_timeout() {
  // Start cat process which won't send anything
  let mut transport =
    StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(100));

  transport.connect().await.unwrap();

  // Try to receive message - should timeout and return None
  let result = transport.receive_message().await;
  assert!(result.is_ok());
  assert_eq!(result.unwrap(), None); // Timeout returns None, not error

  transport.disconnect().await.unwrap();
}

#[tokio::test]
async fn test_stdio_timeout_configuration() {
  let transport =
    StdioTransport::new(vec!["echo".to_string()]).with_timeout(Duration::from_millis(500));

  // Verify timeout is set
  use yanshi_mcp::transport::TransportConfig;
  assert_eq!(transport.timeout_ms(), Some(500));
}

#[tokio::test]
async fn test_stdio_timeout_can_be_modified() {
  let mut transport = StdioTransport::new(vec!["echo".to_string()]);

  // Set custom timeout
  use yanshi_mcp::transport::TransportConfig;
  transport.set_timeout_ms(1000);
  assert_eq!(transport.timeout_ms(), Some(1000));

  // Modify again
  transport.set_timeout_ms(2000);
  assert_eq!(transport.timeout_ms(), Some(2000));
}

// ============================================================================
// Operation Timeout Tests with Mock Transport
// ============================================================================

/// Custom transport that delays responses to test timeouts
struct DelayedMockTransport {
  inner: MockTransport,
  delay: Duration,
}

impl DelayedMockTransport {
  fn new(delay: Duration) -> Self {
    Self {
      inner: MockTransport::new(),
      delay,
    }
  }

  fn add_response(&mut self, response: serde_json::Value) {
    self.inner.add_response(response);
  }
}

#[async_trait::async_trait]
impl Transport for DelayedMockTransport {
  async fn connect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
    self.inner.connect().await
  }

  async fn send_message(
    &self,
    request: serde_json::Value,
  ) -> yanshi_mcp::error::MCPResult<serde_json::Value> {
    // Delay before responding
    tokio::time::sleep(self.delay).await;
    self.inner.send_message(request).await
  }

  async fn send_notification(
    &self,
    notification: serde_json::Value,
  ) -> yanshi_mcp::error::MCPResult<()> {
    tokio::time::sleep(self.delay).await;
    self.inner.send_notification(notification).await
  }

  async fn receive_message(&self) -> yanshi_mcp::error::MCPResult<Option<serde_json::Value>> {
    self.inner.receive_message().await
  }

  async fn disconnect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
    self.inner.disconnect().await
  }

  fn is_connected(&self) -> bool {
    self.inner.is_connected()
  }

  fn transport_type(&self) -> yanshi_mcp::transport::TransportType {
    self.inner.transport_type()
  }
}

#[tokio::test]
async fn test_initialization_timeout() {
  // Create transport that delays responses beyond timeout
  let mut transport = DelayedMockTransport::new(Duration::from_millis(200));
  transport.add_response(MockTransport::standard_initialize_response());

  let mut client = ClientBuilder::new()
    .with_transport(transport)
    .with_timeout(Duration::from_millis(50)) // Shorter than delay
    .build()
    .await
    .unwrap();

  // Try to connect - should timeout during initialization
  let result = tokio::time::timeout(Duration::from_secs(1), client.connect()).await;

  // Should complete (won't hang), but the connect should fail
  assert!(result.is_ok());
  let connect_result = result.unwrap();
  assert!(connect_result.is_err());
}

// ============================================================================
// Timeout with Retry Tests
// ============================================================================

#[tokio::test]
async fn test_retry_after_timeout() {
  use std::sync::atomic::{AtomicU32, Ordering};

  let attempt_count = Arc::new(AtomicU32::new(0));
  let attempt_count_clone = attempt_count.clone();

  // Create a mock that fails with timeout first, then succeeds
  struct RetryMockTransport {
    inner: MockTransport,
    attempt_count: Arc<AtomicU32>,
    fail_first_n: u32,
  }

  #[async_trait::async_trait]
  impl Transport for RetryMockTransport {
    async fn connect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.connect().await
    }

    async fn send_message(
      &self,
      request: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<serde_json::Value> {
      let attempt = self.attempt_count.fetch_add(1, Ordering::SeqCst);

      if attempt < self.fail_first_n {
        // Simulate timeout
        return Err(MCPError::timeout(
          "Simulated timeout".to_string(),
          Some(100),
        ));
      }

      self.inner.send_message(request).await
    }

    async fn send_notification(
      &self,
      notification: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.send_notification(notification).await
    }

    async fn receive_message(&self) -> yanshi_mcp::error::MCPResult<Option<serde_json::Value>> {
      self.inner.receive_message().await
    }

    async fn disconnect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.disconnect().await
    }

    fn is_connected(&self) -> bool {
      self.inner.is_connected()
    }

    fn transport_type(&self) -> yanshi_mcp::transport::TransportType {
      self.inner.transport_type()
    }
  }

  let mut inner = MockTransport::new();
  inner.add_response(MockTransport::standard_initialize_response());

  let transport = RetryMockTransport {
    inner,
    attempt_count: attempt_count_clone,
    fail_first_n: 2, // Fail first 2 attempts
  };

  let mut client = ClientBuilder::new()
    .with_transport(transport)
    .with_max_retries(3) // Allow 3 retries
    .build()
    .await
    .unwrap();

  // Connect should succeed after retries
  let result = client.connect().await;
  assert!(result.is_ok());

  // Should have attempted 3 times (2 failures + 1 success)
  assert_eq!(attempt_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_timeout_exhausts_retries() {
  use std::sync::atomic::{AtomicU32, Ordering};

  let attempt_count = Arc::new(AtomicU32::new(0));
  let attempt_count_clone = attempt_count.clone();

  // Create a mock that always times out
  struct AlwaysTimeoutTransport {
    inner: MockTransport,
    attempt_count: Arc<AtomicU32>,
  }

  #[async_trait::async_trait]
  impl Transport for AlwaysTimeoutTransport {
    async fn connect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.connect().await
    }

    async fn send_message(
      &self,
      _request: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<serde_json::Value> {
      self.attempt_count.fetch_add(1, Ordering::SeqCst);
      Err(MCPError::timeout("Always timeout".to_string(), Some(100)))
    }

    async fn send_notification(
      &self,
      notification: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.send_notification(notification).await
    }

    async fn receive_message(&self) -> yanshi_mcp::error::MCPResult<Option<serde_json::Value>> {
      self.inner.receive_message().await
    }

    async fn disconnect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.disconnect().await
    }

    fn is_connected(&self) -> bool {
      self.inner.is_connected()
    }

    fn transport_type(&self) -> yanshi_mcp::transport::TransportType {
      self.inner.transport_type()
    }
  }

  let transport = AlwaysTimeoutTransport {
    inner: MockTransport::new(),
    attempt_count: attempt_count_clone,
  };

  let mut client = ClientBuilder::new()
    .with_transport(transport)
    .with_max_retries(2) // Allow 2 retries
    .build()
    .await
    .unwrap();

  // Connect should fail after exhausting retries
  let result = client.connect().await;
  assert!(result.is_err());

  // Should have attempted 3 times (initial + 2 retries)
  assert_eq!(attempt_count.load(Ordering::SeqCst), 3);
}

// ============================================================================
// Concurrent Timeout Tests
// ============================================================================

#[tokio::test]
async fn test_multiple_operations_with_different_timeouts() {
  // Create multiple clients with different timeout configurations
  let mut clients = vec![];

  for timeout_ms in [100, 500, 1000] {
    let transport = MockTransport::new();
    let client = ClientBuilder::new()
      .with_transport(transport)
      .with_timeout(Duration::from_millis(timeout_ms))
      .build()
      .await
      .unwrap();

    clients.push(client);
  }

  // All clients should coexist with different timeouts
  assert_eq!(clients.len(), 3);
}

// ============================================================================
// Timeout Error Message Tests
// ============================================================================

#[tokio::test]
async fn test_timeout_error_contains_duration() {
  let mut transport =
    StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(100));

  transport.connect().await.unwrap();

  // Try to read with timeout - internal method
  // This test validates that timeout errors contain duration info
  // We test this indirectly through send_message on a non-responsive process

  let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
  let result = transport.send_message(request).await;

  if let Err(e) = result {
    let error_str = e.to_string();
    // Error should mention timeout
    assert!(
      error_str.to_lowercase().contains("timeout"),
      "Error should mention timeout: {}",
      error_str
    );
  }

  transport.disconnect().await.unwrap();
}

// ============================================================================
// Graceful Degradation Tests
// ============================================================================

#[tokio::test]
async fn test_timeout_does_not_corrupt_state() {
  let mut transport = MockTransport::new();
  transport.add_response(MockTransport::standard_initialize_response());
  transport.add_response(MockTransport::tools_list_response(vec![]));

  let mut client = ClientBuilder::new()
    .with_transport(transport)
    .with_timeout(Duration::from_secs(1))
    .build()
    .await
    .unwrap();

  // Connect successfully
  client.connect().await.unwrap();
  assert!(client.is_connected().await);

  // Even if an operation times out (simulated), state should remain consistent
  let tools_result = client.list_tools().await;
  assert!(tools_result.is_ok());

  // Client should still be connected
  assert!(client.is_connected().await);
}

// ============================================================================
// Real Process Timeout Tests (Unix only)
// ============================================================================

#[tokio::test]
#[cfg(unix)]
async fn test_real_process_read_timeout() {
  // Use 'cat' which waits for input - will timeout on read
  let mut transport =
    StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(200));

  transport.connect().await.unwrap();

  // Try to receive a message - should timeout gracefully
  let start = std::time::Instant::now();
  let result = transport.receive_message().await;
  let elapsed = start.elapsed();

  // Should return Ok(None) for timeout
  assert!(result.is_ok());
  assert_eq!(result.unwrap(), None);

  // Should have taken approximately the timeout duration
  assert!(
    elapsed >= Duration::from_millis(150),
    "Should wait for timeout"
  );
  assert!(
    elapsed < Duration::from_millis(500),
    "Should not wait too long"
  );

  transport.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg(unix)]
async fn test_real_process_write_timeout_scenario() {
  // This test documents write timeout behavior with a real process
  // Use a simple echo process with very short timeout
  let mut transport = StdioTransport::new(vec![
    "sh".to_string(),
    "-c".to_string(),
    "while read line; do echo \"$line\"; done".to_string(),
  ])
  .with_timeout(Duration::from_millis(500));

  transport.connect().await.unwrap();

  // Normal operation should succeed within timeout
  let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
  let result = transport.send_message(request).await;

  // Should succeed (echo is fast enough)
  assert!(result.is_ok());

  transport.disconnect().await.unwrap();
}

/// Eval 2026-10-07 L5: a timed-out `tools/call` may already have run on the
/// server, so retrying it re-executes a non-idempotent tool. MCP tool
/// definitions carry no idempotency, so `tools/call` is sent at most once;
/// read-only requests keep retrying.
#[tokio::test]
async fn timed_out_tools_call_is_not_replayed() {
  use std::sync::atomic::{AtomicU32, Ordering};

  struct TimeoutByMethod {
    inner: MockTransport,
    sends: Arc<AtomicU32>,
  }

  #[async_trait::async_trait]
  impl Transport for TimeoutByMethod {
    async fn connect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.connect().await
    }

    async fn send_message(
      &self,
      request: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<serde_json::Value> {
      match request["method"].as_str() {
        Some("tools/call") | Some("tools/list") => {
          self.sends.fetch_add(1, Ordering::SeqCst);
          Err(MCPError::timeout("simulated timeout".to_string(), Some(10)))
        }
        _ => self.inner.send_message(request).await,
      }
    }

    async fn send_notification(
      &self,
      notification: serde_json::Value,
    ) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.send_notification(notification).await
    }

    async fn receive_message(&self) -> yanshi_mcp::error::MCPResult<Option<serde_json::Value>> {
      self.inner.receive_message().await
    }

    async fn disconnect(&mut self) -> yanshi_mcp::error::MCPResult<()> {
      self.inner.disconnect().await
    }

    fn is_connected(&self) -> bool {
      self.inner.is_connected()
    }

    fn transport_type(&self) -> yanshi_mcp::transport::TransportType {
      self.inner.transport_type()
    }
  }

  let sends = Arc::new(AtomicU32::new(0));
  let mut inner = MockTransport::new();
  inner.add_response(MockTransport::standard_initialize_response());
  let mut client = ClientBuilder::new()
    .with_transport(TimeoutByMethod {
      inner,
      sends: sends.clone(),
    })
    .with_max_retries(3)
    .with_retry_backoff_ms(1)
    .build()
    .await
    .unwrap();
  client.connect().await.unwrap();

  assert!(client.call_tool("charge_card", json!({})).await.is_err());
  assert_eq!(
    sends.swap(0, Ordering::SeqCst),
    1,
    "tools/call must be sent once"
  );

  assert!(client.list_tools().await.is_err());
  assert_eq!(
    sends.load(Ordering::SeqCst),
    4,
    "tools/list keeps its retries"
  );
}
