//! Client builder with fluent API
//!
//! This module provides a builder pattern for constructing MCP clients with
//! various configuration options.

use crate::error::{MCPError, MCPResult};
use crate::protocol::types::{ClientCapabilities, Implementation};
use crate::transport::{StdioTransport, Transport};
use std::collections::HashMap;
use std::time::Duration;

/// Builder for creating MCP clients with custom configuration
///
/// # Example
///
/// ```no_run
/// use yanshi_mcp::client::ClientBuilder;
/// use std::time::Duration;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ClientBuilder::new()
///   .with_stdio(vec!["npx".to_string(), "-y".to_string(),
///     "@modelcontextprotocol/server-everything".to_string()])
///   .with_timeout(Duration::from_secs(60))
///   .with_max_retries(3)
///   .build()
///   .await?;
/// # Ok(())
/// # }
/// ```
pub struct ClientBuilder {
  /// Transport to use for communication
  transport: Option<Box<dyn Transport>>,
  /// Client capabilities to advertise
  capabilities: ClientCapabilities,
  /// Client implementation info
  client_info: Implementation,
  /// Timeout for operations
  timeout: Duration,
  /// Maximum retry attempts
  max_retries: u32,
  /// Retry backoff base (milliseconds)
  retry_backoff_ms: u64,
}

impl ClientBuilder {
  /// Default timeout for operations (30 seconds)
  pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

  /// Default maximum retries
  pub const DEFAULT_MAX_RETRIES: u32 = 3;

  /// Default retry backoff base (100ms)
  pub const DEFAULT_RETRY_BACKOFF_MS: u64 = 100;

  /// Create a new client builder with default configuration
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// let builder = ClientBuilder::new();
  /// ```
  pub fn new() -> Self {
    Self {
      transport: None,
      capabilities: ClientCapabilities::default(),
      client_info: Implementation::yanshi(),
      timeout: Duration::from_secs(Self::DEFAULT_TIMEOUT_SECS),
      max_retries: Self::DEFAULT_MAX_RETRIES,
      retry_backoff_ms: Self::DEFAULT_RETRY_BACKOFF_MS,
    }
  }

  /// Set the transport to use
  ///
  /// # Arguments
  ///
  /// * `transport` - The transport implementation
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  /// use yanshi_mcp::transport::StdioTransport;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_transport(StdioTransport::new(vec!["node".into(), "server.js".into()]));
  /// ```
  pub fn with_transport<T: Transport + 'static>(mut self, transport: T) -> Self {
    self.transport = Some(Box::new(transport));
    self
  }

  /// Configure stdio transport
  ///
  /// # Arguments
  ///
  /// * `command` - Command and arguments to spawn
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_stdio(vec!["npx".to_string(), "-y".to_string(),
  ///     "@modelcontextprotocol/server-everything".to_string()]);
  /// ```
  pub fn with_stdio(mut self, command: Vec<String>) -> Self {
    self.transport = Some(Box::new(StdioTransport::new(command)));
    self
  }

  /// Configure stdio transport with environment variables.
  pub fn with_stdio_env(mut self, command: Vec<String>, env: HashMap<String, String>) -> Self {
    self.transport = Some(Box::new(StdioTransport::new(command).with_env(env)));
    self
  }

  /// Set client capabilities
  ///
  /// # Arguments
  ///
  /// * `capabilities` - Client capabilities to advertise
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  /// use yanshi_mcp::protocol::types::ClientCapabilities;
  ///
  /// let caps = ClientCapabilities::with_sampling();
  /// let builder = ClientBuilder::new()
  ///   .with_capabilities(caps);
  /// ```
  pub fn with_capabilities(mut self, capabilities: ClientCapabilities) -> Self {
    self.capabilities = capabilities;
    self
  }

  /// Set client implementation info
  ///
  /// # Arguments
  ///
  /// * `name` - Client name
  /// * `version` - Client version
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_client_info("my-client", "1.0.0");
  /// ```
  pub fn with_client_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
    self.client_info = Implementation {
      name: name.into(),
      version: version.into(),
    };
    self
  }

  /// Set timeout for operations
  ///
  /// # Arguments
  ///
  /// * `timeout` - Timeout duration
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  /// use std::time::Duration;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_timeout(Duration::from_secs(60));
  /// ```
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  /// Set maximum retry attempts for transient failures. `tools/call` is
  /// never retried: a timeout can land after the server ran the tool.
  ///
  /// # Arguments
  ///
  /// * `max_retries` - Maximum number of retries (0 = no retries)
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_max_retries(5);
  /// ```
  pub fn with_max_retries(mut self, max_retries: u32) -> Self {
    self.max_retries = max_retries;
    self
  }

  /// Set retry backoff base duration
  ///
  /// Actual backoff is calculated as: `base * 2^attempt`
  ///
  /// # Arguments
  ///
  /// * `backoff_ms` - Base backoff in milliseconds
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// let builder = ClientBuilder::new()
  ///   .with_retry_backoff_ms(200); // 200ms, 400ms, 800ms, ...
  /// ```
  pub fn with_retry_backoff_ms(mut self, backoff_ms: u64) -> Self {
    self.retry_backoff_ms = backoff_ms;
    self
  }

  /// Build the MCP client
  ///
  /// This validates the configuration and creates the client instance.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// - No transport is configured
  /// - Transport configuration is invalid
  ///
  /// # Example
  ///
  /// ```no_run
  /// use yanshi_mcp::client::ClientBuilder;
  ///
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// let client = ClientBuilder::new()
  ///   .with_stdio(vec!["node".to_string(), "server.js".to_string()])
  ///   .build()
  ///   .await?;
  /// # Ok(())
  /// # }
  /// ```
  pub async fn build(self) -> MCPResult<super::MCPClient> {
    // Validate transport
    let transport = self
      .transport
      .ok_or_else(|| MCPError::configuration("No transport configured"))?;

    // Create client config
    let config = ClientConfig {
      capabilities: self.capabilities,
      client_info: self.client_info,
      timeout: self.timeout,
      max_retries: self.max_retries,
      retry_backoff_ms: self.retry_backoff_ms,
    };

    // Build client
    Ok(super::MCPClient::new(transport, config))
  }
}

impl Default for ClientBuilder {
  fn default() -> Self {
    Self::new()
  }
}

/// Client configuration
#[derive(Debug, Clone)]
pub(super) struct ClientConfig {
  pub capabilities: ClientCapabilities,
  pub client_info: Implementation,
  pub timeout: Duration,
  pub max_retries: u32,
  pub retry_backoff_ms: u64,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_builder_default() {
    let builder = ClientBuilder::new();
    assert_eq!(
      builder.timeout,
      Duration::from_secs(ClientBuilder::DEFAULT_TIMEOUT_SECS)
    );
    assert_eq!(builder.max_retries, ClientBuilder::DEFAULT_MAX_RETRIES);
    assert_eq!(
      builder.retry_backoff_ms,
      ClientBuilder::DEFAULT_RETRY_BACKOFF_MS
    );
  }

  #[test]
  fn test_builder_with_timeout() {
    let builder = ClientBuilder::new().with_timeout(Duration::from_secs(60));
    assert_eq!(builder.timeout, Duration::from_secs(60));
  }

  #[test]
  fn test_builder_with_retries() {
    let builder = ClientBuilder::new()
      .with_max_retries(5)
      .with_retry_backoff_ms(200);
    assert_eq!(builder.max_retries, 5);
    assert_eq!(builder.retry_backoff_ms, 200);
  }

  #[test]
  fn test_builder_with_client_info() {
    let builder = ClientBuilder::new().with_client_info("test-client", "2.0.0");
    assert_eq!(builder.client_info.name, "test-client");
    assert_eq!(builder.client_info.version, "2.0.0");
  }

  #[tokio::test]
  async fn test_builder_without_transport() {
    let result = ClientBuilder::new().build().await;
    assert!(result.is_err());
    assert!(matches!(
      result.unwrap_err(),
      MCPError::Configuration { .. }
    ));
  }

  #[test]
  fn test_builder_with_stdio() {
    let builder =
      ClientBuilder::new().with_stdio(vec!["node".to_string(), "server.js".to_string()]);
    assert!(builder.transport.is_some());
  }

  #[test]
  fn test_builder_default_trait() {
    let builder = ClientBuilder::default();
    assert_eq!(
      builder.timeout,
      Duration::from_secs(ClientBuilder::DEFAULT_TIMEOUT_SECS)
    );
  }
}
