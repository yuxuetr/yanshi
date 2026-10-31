//! Client session management
//!
//! This module handles the MCP session lifecycle including initialization,
//! connection state tracking, and message correlation.

use crate::error::{JsonRpcErrorCode, MCPError, MCPResult, ResultExt};
use crate::protocol::modern::{
  KNOWN_PROTOCOL_VERSIONS, MCP_PROTOCOL_VERSION_2026_07_28, McpEra,
  as_unsupported_protocol_version_error, inject_modern_meta_into_request, is_input_required_result,
  pick_mutually_supported_modern_version,
};
#[cfg(test)]
use crate::protocol::types::ClientCapabilities;
use crate::protocol::types::{
  Implementation, InitializeParams, InitializeResult, JsonRpcRequest, JsonRpcResponse,
  MCP_PROTOCOL_VERSION, RequestId, ServerCapabilities,
};
use crate::transport::Transport;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::builder::ClientConfig;
use super::era::era_for_transport;

/// MCP client with session management
///
/// This client handles the complete MCP session lifecycle including:
/// - Connection and initialization
/// - Request/response correlation
/// - Session state tracking
/// - Graceful disconnection
///
/// # Example
///
/// ```no_run
/// use yanshi_mcp::client::ClientBuilder;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut client = ClientBuilder::new()
///   .with_stdio(vec!["node".to_string(), "server.js".to_string()])
///   .build()
///   .await?;
///
/// client.connect().await?;
/// // Use client...
/// client.disconnect().await?;
/// # Ok(())
/// # }
/// ```
pub struct MCPClient {
  /// Transport for communication.
  ///
  /// Q3.2.2: pre-fix this was `Arc<Mutex<Box<dyn Transport>>>` — every
  /// outbound JSON-RPC call took the outer mutex, serializing all
  /// requests across the agent's parallel-tool-call dispatcher.
  /// `Transport` now provides per-request interior mutability
  /// (oneshot demux for stdio, internal `Arc<Mutex>` for mock), so
  /// the client owns the transport directly with no outer lock and
  /// `send_request` / `send_notification` are `&self`. The
  /// `connect` / `disconnect` paths still need `&mut self` to
  /// invoke the transport's `&mut` lifecycle methods. Upstream
  /// callers that want to fan out parallel requests wrap the
  /// connected `MCPClient` in `Arc<MCPClient>` (see
  /// `yanshi-skills::McpClientPool`).
  transport: Box<dyn Transport>,
  /// Client configuration
  config: ClientConfig,
  /// Session ID
  session_id: String,
  /// Connection state
  connected: Arc<Mutex<bool>>,
  /// Server capabilities (after initialization). W5.6: stored typed
  /// (was `Option<Value>`, round-tripped through `serde_json::to_value`
  /// for no reason — `InitializeResult::capabilities` is already typed)
  /// so `require_server_capability` can consult `supports_*()` directly.
  server_capabilities: Arc<Mutex<Option<ServerCapabilities>>>,
  /// Server info (after initialization)
  server_info: Arc<Mutex<Option<Implementation>>>,
  /// Request ID counter
  request_counter: Arc<AtomicU64>,
  /// W5.8-4: which protocol era this connection speaks, set by
  /// `connect()` from the transport type (see
  /// [`super::era::era_for_transport`]). Defaults to `Legacy` before
  /// the first successful `connect()` — matches every pre-W5.8 code
  /// path's behavior, since nothing meaningfully reads this before a
  /// connection exists.
  era: Arc<Mutex<McpEra>>,
}

impl std::fmt::Debug for MCPClient {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MCPClient")
      .field("session_id", &self.session_id)
      .field("connected", &"<Mutex>")
      .field("server_capabilities", &"<Mutex>")
      .field("server_info", &"<Mutex>")
      .finish_non_exhaustive()
  }
}

/// Session state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
  /// Not connected
  Disconnected,
  /// Connected but not initialized
  Connected,
  /// Fully initialized and ready
  Ready,
}

impl MCPClient {
  /// Create a new MCP client (internal constructor)
  pub(super) fn new(transport: Box<dyn Transport>, config: ClientConfig) -> Self {
    Self {
      transport,
      config,
      session_id: Uuid::new_v4().to_string(),
      connected: Arc::new(Mutex::new(false)),
      server_capabilities: Arc::new(Mutex::new(None)),
      server_info: Arc::new(Mutex::new(None)),
      request_counter: Arc::new(AtomicU64::new(1)),
      era: Arc::new(Mutex::new(McpEra::Legacy)),
    }
  }

  /// Connect to the MCP server and initialize the session
  ///
  /// This performs the complete initialization handshake:
  /// 1. Connect transport
  /// 2. Send initialize request
  /// 3. Receive server capabilities
  /// 4. Send initialized notification
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// - Transport connection fails
  /// - Initialization handshake fails
  /// - Server rejects initialization
  ///
  /// # Example
  ///
  /// ```no_run
  /// # use yanshi_mcp::client::ClientBuilder;
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// let mut client = ClientBuilder::new()
  ///   .with_stdio(vec!["node".to_string(), "server.js".to_string()])
  ///   .build()
  ///   .await?;
  ///
  /// client.connect().await?;
  /// # Ok(())
  /// # }
  /// ```
  pub async fn connect(&mut self) -> MCPResult<()> {
    // Check if already connected
    let is_connected = *self.connected.lock().await;
    if is_connected {
      return Ok(());
    }

    // Connect transport with timeout
    let timeout = self.config.timeout;
    let connect_result = tokio::time::timeout(timeout, self.transport.connect()).await;

    match connect_result {
      Ok(Ok(())) => {
        // Update connection state
        *self.connected.lock().await = true;
      }
      Ok(Err(e)) => return Err(e.context("Failed to connect transport")),
      Err(_) => {
        return Err(MCPError::timeout(
          format!("Connection timeout after {:?}", timeout),
          Some(timeout.as_millis() as u64),
        ));
      }
    }

    // W5.8-4: era is a property of the transport (see
    // `era::era_for_transport`'s doc comment for why this is
    // transport-type-based rather than a runtime wire probe). Modern
    // has no handshake — stateless, per-request `_meta` instead — so
    // only Legacy runs `initialize()` below.
    let era = era_for_transport(self.transport.transport_type());
    *self.era.lock().await = era;
    if era == McpEra::Modern {
      return Ok(());
    }

    // Initialize session (already has retry + timeout via send_request)
    if let Err(err) = self.initialize().await {
      // W5.6: pre-existing gap, newly exercised by the protocol-version
      // check above — `connected` was flipped `true` right after the
      // transport connected, before `initialize()` ran, and nothing
      // rolled it back on failure. A caller could get an `Err` from
      // `connect()` yet see `is_connected() == true` afterward. Tear
      // down the transport (best-effort; we're already returning the
      // real error) and reset state so a failed handshake leaves the
      // client genuinely disconnected.
      let _ = self.transport.disconnect().await;
      *self.connected.lock().await = false;
      return Err(err.context("Failed to initialize MCP session"));
    }

    Ok(())
  }

  /// Initialize the MCP session
  async fn initialize(&mut self) -> MCPResult<()> {
    // Build initialize request
    let params = InitializeParams::new(
      self.config.capabilities.clone(),
      self.config.client_info.clone(),
    );

    let request = JsonRpcRequest::new(
      self.next_request_id(),
      "initialize",
      Some(
        serde_json::to_value(&params)
          .map_err(|e| MCPError::from(e).context("Failed to serialize initialize params"))?,
      ),
    );

    // Send request
    let response = self
      .send_request(request)
      .await
      .context("Failed to send initialize request")?;

    // Parse response
    let response: JsonRpcResponse = serde_json::from_value(response)
      .map_err(|e| MCPError::from(e).context("Failed to parse initialize response"))?;

    // Check for errors
    if let Some(error) = response.error {
      return Err(MCPError::protocol(
        format!("Initialization failed: {} - {}", error.code, error.message),
        JsonRpcErrorCode::InternalError,
      ));
    }

    // Parse result
    let result = response.result.ok_or_else(|| {
      MCPError::protocol(
        "Missing result in initialize response",
        JsonRpcErrorCode::InvalidRequest,
      )
    })?;

    let init_result: InitializeResult = serde_json::from_value(result)
      .map_err(|e| MCPError::from(e).context("Failed to parse initialize result"))?;

    // W5.6: verify the server actually returned the protocol version we
    // sent, instead of silently accepting whatever came back. This crate
    // implements exactly one protocol version's wire format today, so
    // there's nothing to negotiate down/up to yet — a mismatch means the
    // session never really started, checked before any state is stored.
    if init_result.protocol_version != MCP_PROTOCOL_VERSION {
      return Err(MCPError::protocol(
        format!(
          "server returned protocol version '{}', client requires '{}'",
          init_result.protocol_version, MCP_PROTOCOL_VERSION
        ),
        JsonRpcErrorCode::InvalidRequest,
      ));
    }

    // Store server info and capabilities
    *self.server_capabilities.lock().await = Some(init_result.capabilities);
    *self.server_info.lock().await = Some(init_result.server_info);

    // Send initialized notification
    let notification = JsonRpcRequest::notification("notifications/initialized", None);
    self
      .send_notification(notification)
      .await
      .context("Failed to send initialized notification")?;

    Ok(())
  }

  /// Disconnect from the server
  ///
  /// This gracefully closes the connection and cleans up resources.
  ///
  /// # Example
  ///
  /// ```no_run
  /// # use yanshi_mcp::client::ClientBuilder;
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// # let mut client = ClientBuilder::new()
  /// #   .with_stdio(vec!["node".to_string(), "server.js".to_string()])
  /// #   .build().await?;
  /// client.disconnect().await?;
  /// # Ok(())
  /// # }
  /// ```
  pub async fn disconnect(&mut self) -> MCPResult<()> {
    // Disconnect transport
    self
      .transport
      .disconnect()
      .await
      .context("Failed to disconnect transport")?;

    // Update state
    *self.connected.lock().await = false;
    *self.server_capabilities.lock().await = None;
    *self.server_info.lock().await = None;

    Ok(())
  }

  /// Check if client is connected
  ///
  /// # Example
  ///
  /// ```no_run
  /// # use yanshi_mcp::client::ClientBuilder;
  /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
  /// # let client = ClientBuilder::new()
  /// #   .with_stdio(vec!["node".to_string(), "server.js".to_string()])
  /// #   .build().await?;
  /// if client.is_connected().await {
  ///   println!("Client is connected");
  /// }
  /// # Ok(())
  /// # }
  /// ```
  pub async fn is_connected(&self) -> bool {
    *self.connected.lock().await
  }

  /// Get current session state
  pub async fn session_state(&self) -> SessionState {
    let is_connected = *self.connected.lock().await;
    let has_capabilities = self.server_capabilities.lock().await.is_some();

    if !is_connected {
      SessionState::Disconnected
    } else if has_capabilities {
      SessionState::Ready
    } else {
      SessionState::Connected
    }
  }

  /// Get server capabilities (if initialized)
  pub async fn server_capabilities(&self) -> Option<ServerCapabilities> {
    self.server_capabilities.lock().await.clone()
  }

  /// W5.6: fail fast, client-side, when a caller tries to invoke a
  /// method the server never advertised support for during `initialize`
  /// — instead of relying on the server to reject it (typically with a
  /// JSON-RPC "method not found").
  ///
  /// W5.8-4: Modern era has no `initialize`/`ServerCapabilities`
  /// handshake to consult (the RFC: the server "accepts or rejects each
  /// request independently") — bypassed unconditionally for a Modern
  /// connection; the server rejects unsupported methods per-request
  /// instead of the client gating on a stale up-front capability set.
  pub(crate) async fn require_server_capability(
    &self,
    check: impl Fn(&ServerCapabilities) -> bool,
    capability_name: &str,
  ) -> MCPResult<()> {
    if *self.era.lock().await == McpEra::Modern {
      return Ok(());
    }
    match self.server_capabilities.lock().await.as_ref() {
      Some(caps) if check(caps) => Ok(()),
      Some(_) => Err(MCPError::protocol(
        format!("server did not advertise the '{capability_name}' capability during initialize"),
        JsonRpcErrorCode::MethodNotFound,
      )),
      None => Err(MCPError::connection(
        "client is not initialized (call connect() first)",
      )),
    }
  }

  /// Get server info (if initialized)
  pub async fn server_info(&self) -> Option<Implementation> {
    self.server_info.lock().await.clone()
  }

  /// Which protocol era this connection speaks (W5.8-4). `Legacy` before
  /// the first successful `connect()`.
  pub async fn era(&self) -> McpEra {
    *self.era.lock().await
  }

  /// Get session ID
  pub fn session_id(&self) -> &str {
    &self.session_id
  }

  /// Send a JSON-RPC request and wait for response.
  ///
  /// Q3.2.2: `&self` (was `&mut self`) so an `Arc<MCPClient>` can
  /// fan out concurrent requests across the shared transport. The
  /// underlying [`Transport`] now exposes per-request demux via
  /// internal oneshot channels, so dispatching N requests
  /// simultaneously no longer serializes behind a single outer
  /// mutex.
  ///
  /// Applies retry-with-backoff for transient errors and the
  /// per-call timeout configured on the client.
  pub(super) async fn send_request(&self, request: JsonRpcRequest) -> MCPResult<Value> {
    use crate::client::retry::{RetryConfig, retry_with_backoff};

    // P3.8: traceparent injection for cross-hop OTel.
    let mut request = request;
    crate::protocol::traceparent::inject_traceparent_into_request(&mut request);

    // W5.8-4: Modern era carries protocol version + client identity
    // per-request instead of via a one-time `initialize()` handshake.
    let era = *self.era.lock().await;
    let sent_protocol_version = MCP_PROTOCOL_VERSION_2026_07_28;
    if era == McpEra::Modern {
      inject_modern_meta_into_request(
        &mut request,
        sent_protocol_version,
        &self.config.client_info,
        &self.config.capabilities,
      );
    }
    let request_value = serde_json::to_value(&request)
      .map_err(|e| MCPError::from(e).context("Failed to serialize request"))?;

    // A timeout or transport error can land after the server received the
    // request, so a retried `tools/call` may run the tool twice. MCP tool
    // definitions carry no idempotency, so tool calls are sent at most once.
    let max_retries = if request.method == "tools/call" {
      0
    } else {
      self.config.max_retries
    };
    let retry_config = RetryConfig::new(max_retries, self.config.retry_backoff_ms);
    let timeout = self.config.timeout;
    // Borrow rather than clone — the closure runs sequentially
    // inside `retry_with_backoff` so the borrow lasts only across
    // each await before the next retry iteration.
    let transport = &*self.transport;

    let response = retry_with_backoff(&retry_config, || {
      let request_value = request_value.clone();
      async move {
        let result = tokio::time::timeout(timeout, transport.send_message(request_value)).await;
        match result {
          Ok(Ok(response)) => Ok(response),
          Ok(Err(e)) => Err(e.context("Failed to send message")),
          Err(_) => Err(MCPError::timeout(
            format!("Request timeout after {:?}", timeout),
            Some(timeout.as_millis() as u64),
          )),
        }
      }
    })
    .await?;

    if era != McpEra::Modern {
      return Ok(response);
    }

    // Modern-only post-processing: retry once on a recognized version
    // mismatch, and refuse to silently misinterpret an MRTR
    // `InputRequiredResult` as an ordinary result.
    let Ok(parsed) = serde_json::from_value::<JsonRpcResponse>(response.clone()) else {
      // Not a well-formed JSON-RPC response envelope — nothing
      // Modern-specific to do; let the caller's own parsing surface
      // whatever's wrong.
      return Ok(response);
    };

    if let Some(error) = parsed.error.as_ref()
      && let Some(data) = as_unsupported_protocol_version_error(error)
      && let Some(retry_version) = pick_mutually_supported_modern_version(
        &data.supported,
        KNOWN_PROTOCOL_VERSIONS,
        sent_protocol_version,
      )
    {
      inject_modern_meta_into_request(
        &mut request,
        retry_version,
        &self.config.client_info,
        &self.config.capabilities,
      );
      let retry_value = serde_json::to_value(&request)
        .map_err(|e| MCPError::from(e).context("Failed to serialize version-retry request"))?;
      let retried = tokio::time::timeout(timeout, transport.send_message(retry_value))
        .await
        .map_err(|_| {
          MCPError::timeout(
            format!("Request timeout after {:?}", timeout),
            Some(timeout.as_millis() as u64),
          )
        })?
        .map_err(|e| e.context("Failed to send version-retry request"))?;
      return Ok(retried);
    }

    if let Some(result) = parsed.result.as_ref()
      && is_input_required_result(result)
    {
      return Err(MCPError::protocol(
        "server requested MRTR input (InputRequiredResult) but this client has no \
         sampling/elicitation/roots input-response handler wired up yet",
        JsonRpcErrorCode::InternalError,
      ));
    }

    Ok(response)
  }

  /// Send a JSON-RPC notification (no response expected).
  ///
  /// Q3.2.2: `&self` for the same parallel-fan-out reason as
  /// [`Self::send_request`].
  pub(super) async fn send_notification(&self, notification: JsonRpcRequest) -> MCPResult<()> {
    // P3.8: traceparent injection on notifications too. The MCP
    // server side may correlate notifications with their parent
    // span (e.g. `notifications/progress` emitted during a long
    // tool call); without the carrier those records would orphan.
    let mut notification = notification;
    crate::protocol::traceparent::inject_traceparent_into_request(&mut notification);
    let notification_value = serde_json::to_value(&notification)
      .map_err(|e| MCPError::from(e).context("Failed to serialize notification"))?;

    // Apply timeout to notification
    let timeout = self.config.timeout;
    let result = tokio::time::timeout(
      timeout,
      self.transport.send_notification(notification_value),
    )
    .await;

    match result {
      Ok(Ok(())) => Ok(()),
      Ok(Err(e)) => Err(e.context("Failed to send notification")),
      Err(_) => Err(MCPError::timeout(
        format!("Notification timeout after {:?}", timeout),
        Some(timeout.as_millis() as u64),
      )),
    }
  }

  /// Generate next request ID
  pub(super) fn next_request_id(&self) -> RequestId {
    let id = self.request_counter.fetch_add(1, Ordering::SeqCst);
    RequestId::Number(id as i64)
  }
}

impl Drop for MCPClient {
  fn drop(&mut self) {
    // Best-effort cleanup
    // Note: Can't use async in Drop, so transport cleanup happens in its own Drop
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::transport::StdioTransport;

  #[test]
  fn test_session_id_generated() {
    let transport = Box::new(StdioTransport::new(vec!["echo".to_string()]));
    let config = ClientConfig {
      capabilities: ClientCapabilities::default(),
      client_info: Implementation::yanshi(),
      timeout: std::time::Duration::from_secs(30),
      max_retries: 3,
      retry_backoff_ms: 100,
    };
    let client = MCPClient::new(transport, config);
    assert!(!client.session_id.is_empty());
  }

  #[tokio::test]
  async fn test_initial_state_disconnected() {
    let transport = Box::new(StdioTransport::new(vec!["echo".to_string()]));
    let config = ClientConfig {
      capabilities: ClientCapabilities::default(),
      client_info: Implementation::yanshi(),
      timeout: std::time::Duration::from_secs(30),
      max_retries: 3,
      retry_backoff_ms: 100,
    };
    let client = MCPClient::new(transport, config);
    assert_eq!(client.session_state().await, SessionState::Disconnected);
  }

  #[tokio::test]
  async fn test_request_id_increment() {
    let transport = Box::new(StdioTransport::new(vec!["echo".to_string()]));
    let config = ClientConfig {
      capabilities: ClientCapabilities::default(),
      client_info: Implementation::yanshi(),
      timeout: std::time::Duration::from_secs(30),
      max_retries: 3,
      retry_backoff_ms: 100,
    };
    let client = MCPClient::new(transport, config);

    let id1 = client.next_request_id();
    let id2 = client.next_request_id();

    match (id1, id2) {
      (RequestId::Number(n1), RequestId::Number(n2)) => {
        assert_eq!(n2, n1 + 1);
      }
      _ => panic!("Expected numeric request IDs"),
    }
  }
}
