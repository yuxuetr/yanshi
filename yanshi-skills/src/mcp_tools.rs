use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};
use yanshi_mcp::client::{
  ClientBuilder, Content, MCPClient, Tool as McpTool, validate_tool_arguments,
};
use yanshi_tools::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput, ToolOutputPart};

use crate::{error::SkillError, manifest::McpServerConfig};

/// Shared MCP client handle for all tools exposed by one configured server.
///
/// The handle lazily reconnects when needed and serializes access through a
/// mutex because the MCP client API requires mutable access for requests.
#[derive(Debug)]
pub struct McpClientPool {
  config: McpServerConfig,
  client: Mutex<Option<MCPClient>>,
  call_slots: Semaphore,
}

impl McpClientPool {
  pub fn new(config: McpServerConfig) -> Self {
    let max_calls = config.resolved_max_concurrent_calls();
    Self {
      config,
      client: Mutex::new(None),
      call_slots: Semaphore::new(max_calls),
    }
  }

  pub fn server_name(&self) -> &str {
    &self.config.name
  }

  pub async fn list_tools(&self) -> Result<Vec<McpTool>, SkillError> {
    info!(
      event = "mcp_tools_list_started",
      server = %self.config.name,
      "Listing MCP tools"
    );
    let mut guard = self.client.lock().await;
    let client = ensure_client(&self.config, &mut guard).await?;
    let tools = match client.list_tools().await {
      Ok(tools) => tools,
      Err(e) => {
        warn!(
          event = "mcp_tools_list_failed",
          server = %self.config.name,
          error = %e,
          "Failed to list MCP tools"
        );
        // W5.8-1: a transient (connection/transport/timeout) error means
        // the cached client is dead — clear the slot so the next call
        // reconnects instead of repeatedly hitting the same broken
        // client. Mirrors the timeout-path slot-clearing in call_tool.
        if e.is_transient() {
          *guard = None;
        }
        return Err(SkillError::McpError(format!("{}: {}", self.config.name, e)));
      }
    };
    info!(
      event = "mcp_tools_list_succeeded",
      server = %self.config.name,
      tool_count = tools.len(),
      "Listed MCP tools"
    );
    Ok(tools)
  }

  pub async fn disconnect(&self) -> Result<(), SkillError> {
    let mut guard = self.client.lock().await;
    if let Some(client) = guard.as_mut() {
      client
        .disconnect()
        .await
        .map_err(|e| SkillError::McpError(format!("{}: {}", self.config.name, e)))?;
    }
    *guard = None;
    Ok(())
  }

  async fn call_tool(&self, tool_name: &str, params: Value) -> Result<ToolOutput, ToolError> {
    let _permit = self
      .call_slots
      .acquire()
      .await
      .map_err(|_| ToolError::ExecutionFailed {
        message: format!(
          "MCP server '{}' concurrency limiter is closed",
          self.config.name
        ),
      })?;
    let mut guard = self.client.lock().await;
    let client = ensure_client_for_tool(&self.config, &mut guard).await?;
    let timeout = self.config.resolved_timeout();
    info!(
      event = "mcp_tool_call_started",
      server = %self.config.name,
      tool = %tool_name,
      timeout_ms = timeout.as_millis() as u64,
      max_concurrent_calls = self.config.resolved_max_concurrent_calls(),
      "Calling MCP tool"
    );
    let result = match tokio::time::timeout(timeout, client.call_tool(tool_name, params)).await {
      Ok(Ok(result)) => result,
      // The client's own per-request timeout (same duration) can win the
      // race against the outer one; report both the same way.
      Ok(Err(e)) if !matches!(e, yanshi_mcp::error::MCPError::Timeout { .. }) => {
        let message = format!(
          "MCP server '{}' tool '{}' failed: {}",
          self.config.name, tool_name, e
        );
        warn!(
          event = "mcp_tool_call_failed",
          server = %self.config.name,
          tool = %tool_name,
          error = %e,
          "MCP tool call failed"
        );
        // W5.8-1: only the timeout branch below used to clear the
        // cached slot. A non-timeout connection error (e.g. the child
        // process died and the next write/read errors immediately)
        // fell through here and left a known-dead client cached for
        // every subsequent call.
        if e.is_transient() {
          *guard = None;
        }
        return Err(ToolError::ExecutionFailed { message });
      }
      Ok(Err(_)) | Err(_) => {
        if let Some(client) = guard.as_mut() {
          let _ = client.disconnect().await;
        }
        *guard = None;
        warn!(
          event = "mcp_tool_call_timeout",
          server = %self.config.name,
          tool = %tool_name,
          timeout_ms = timeout.as_millis() as u64,
          "MCP tool call timed out"
        );
        return Err(ToolError::ExecutionFailed {
          message: format!(
            "MCP server '{}' tool '{}' timed out after {:?}",
            self.config.name, tool_name, timeout
          ),
        });
      }
    };

    let (content, parts) = convert_mcp_result_content(&result.content);
    if result.is_error() {
      let content = format!(
        "MCP server '{}' tool '{}' returned error: {}",
        self.config.name, tool_name, content
      );
      warn!(
        event = "mcp_tool_call_result_error",
        server = %self.config.name,
        tool = %tool_name,
        "MCP tool returned an error result"
      );
      Ok(ToolOutput::error_parts(content, parts))
    } else {
      info!(
        event = "mcp_tool_call_succeeded",
        server = %self.config.name,
        tool = %tool_name,
        "MCP tool call succeeded"
      );
      Ok(ToolOutput::success_parts(content, parts))
    }
  }
}

/// Tool adapter registered in Yanshi's local ToolRegistry.
pub struct McpToolAdapter {
  public_name: String,
  remote_name: String,
  description: String,
  input_schema: Value,
  pool: Arc<McpClientPool>,
}

impl McpToolAdapter {
  pub fn new(pool: Arc<McpClientPool>, tool: McpTool) -> Self {
    let public_name = public_tool_name(pool.server_name(), &tool.name);
    let description = tool.description.unwrap_or_else(|| {
      format!(
        "MCP tool '{}' exposed by server '{}'",
        tool.name,
        pool.server_name()
      )
    });

    Self {
      public_name,
      remote_name: tool.name,
      description,
      input_schema: tool.input_schema,
      pool,
    }
  }
}

#[async_trait]
impl Tool for McpToolAdapter {
  fn name(&self) -> &str {
    &self.public_name
  }

  fn description(&self) -> &str {
    &self.description
  }

  fn parameters_schema(&self) -> Value {
    self.input_schema.clone()
  }

  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::mcp(self.pool.server_name(), &self.remote_name)
      .with_idempotency(mcp_tool_idempotency(&self.description, &self.input_schema))
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    validate_tool_arguments(&self.remote_name, &self.input_schema, &params).map_err(|e| {
      ToolError::InvalidParams {
        message: format!(
          "MCP tool '{}' parameter validation failed: {}",
          self.remote_name, e
        ),
      }
    })?;

    self.pool.call_tool(&self.remote_name, params).await
  }
}

fn mcp_tool_idempotency(description: &str, input_schema: &Value) -> ToolIdempotency {
  if has_idempotency_hint(input_schema, "idempotent")
    || description_contains(description, "[idempotent]")
  {
    return ToolIdempotency::Idempotent;
  }
  if has_idempotency_hint(input_schema, "non_idempotent")
    || has_idempotency_hint(input_schema, "non-idempotent")
    || description_contains(description, "[non_idempotent]")
    || description_contains(description, "[non-idempotent]")
  {
    return ToolIdempotency::NonIdempotent;
  }
  ToolIdempotency::Unknown
}

fn description_contains(description: &str, needle: &str) -> bool {
  description.to_ascii_lowercase().contains(needle)
}

fn has_idempotency_hint(input_schema: &Value, expected: &str) -> bool {
  input_schema
    .get("x-yanshi-idempotency")
    .or_else(|| input_schema.get("x_idempotency"))
    .or_else(|| input_schema.get("idempotency"))
    .and_then(Value::as_str)
    .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

pub fn public_tool_name(server_name: &str, tool_name: &str) -> String {
  format!(
    "mcp_{}_{}",
    sanitize_tool_name(server_name),
    sanitize_tool_name(tool_name)
  )
}

fn sanitize_tool_name(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    if ch.is_ascii_alphanumeric() || ch == '_' {
      out.push(ch.to_ascii_lowercase());
    } else {
      out.push('_');
    }
  }
  let trimmed = out.trim_matches('_').to_string();
  if trimmed.is_empty() {
    "tool".to_string()
  } else {
    trimmed
  }
}

async fn ensure_client<'a>(
  config: &McpServerConfig,
  slot: &'a mut Option<MCPClient>,
) -> Result<&'a mut MCPClient, SkillError> {
  if slot.is_none() {
    info!(
      event = "mcp_server_connect_started",
      server = %config.name,
      command = %config.command,
      timeout_ms = config.resolved_timeout().as_millis() as u64,
      "Connecting MCP server"
    );
    let mut client = build_client(config).await.map_err(|e| {
      warn!(
        event = "mcp_server_client_build_failed",
        server = %config.name,
        error = %e,
        "Failed to build MCP client"
      );
      SkillError::McpError(format!(
        "Failed to build MCP client '{}': {}",
        config.name, e
      ))
    })?;
    client.connect().await.map_err(|e| {
      warn!(
        event = "mcp_server_connect_failed",
        server = %config.name,
        error = %e,
        "Failed to connect MCP server"
      );
      SkillError::McpError(format!(
        "Failed to connect MCP server '{}': {}",
        config.name, e
      ))
    })?;
    info!(
      event = "mcp_server_connected",
      server = %config.name,
      "Connected MCP server"
    );
    *slot = Some(client);
  }

  slot.as_mut().ok_or_else(|| {
    SkillError::McpError(format!(
      "MCP client '{}' was not available after initialization",
      config.name
    ))
  })
}

async fn ensure_client_for_tool<'a>(
  config: &McpServerConfig,
  slot: &'a mut Option<MCPClient>,
) -> Result<&'a mut MCPClient, ToolError> {
  ensure_client(config, slot)
    .await
    .map_err(|e| ToolError::ExecutionFailed {
      message: e.to_string(),
    })
}

async fn build_client(config: &McpServerConfig) -> yanshi_mcp::MCPResult<MCPClient> {
  let mut command = Vec::with_capacity(1 + config.args.len());
  command.push(config.command.clone());
  command.extend(config.args.clone());

  let builder = if config.env.is_empty() {
    ClientBuilder::new().with_stdio(command)
  } else {
    ClientBuilder::new().with_stdio_env(command, config.env.clone())
  };

  builder
    .with_timeout(config.resolved_timeout())
    .build()
    .await
}

fn convert_mcp_result_content(content: &[Content]) -> (String, Vec<ToolOutputPart>) {
  if content.is_empty() {
    return (String::new(), Vec::new());
  }

  let mut text_parts = Vec::with_capacity(content.len());
  let mut output_parts = Vec::with_capacity(content.len());
  for item in content {
    match item {
      Content::Text { text } => {
        text_parts.push(text.clone());
        output_parts.push(ToolOutputPart::Text { text: text.clone() });
      }
      Content::Image { data, mime_type } => {
        text_parts.push(format!("[image:{};{} bytes]", mime_type, data.len()));
        output_parts.push(ToolOutputPart::Image {
          data: data.clone(),
          mime_type: mime_type.clone(),
        });
      }
      Content::Resource {
        uri,
        mime_type,
        text,
      } => {
        if let Some(text) = text {
          text_parts.push(text.clone());
        } else if let Some(mime_type) = mime_type {
          text_parts.push(format!("[resource:{};{}]", uri, mime_type));
        } else {
          text_parts.push(format!("[resource:{}]", uri));
        }
        output_parts.push(ToolOutputPart::Resource {
          uri: uri.clone(),
          mime_type: mime_type.clone(),
          text: text.clone(),
        });
      }
    }
  }
  (text_parts.join("\n"), output_parts)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn public_tool_names_are_stable_and_prefixed() {
    assert_eq!(
      public_tool_name("github-server", "search/repositories"),
      "mcp_github_server_search_repositories"
    );
  }

  #[test]
  fn empty_tool_name_segments_fall_back() {
    assert_eq!(public_tool_name("!!!", "???"), "mcp_tool_tool");
  }

  #[test]
  fn mcp_adapter_metadata_preserves_original_server_and_tool_names() {
    let pool = Arc::new(McpClientPool::new(McpServerConfig {
      name: "local-demo".to_string(),
      command: "python3".to_string(),
      args: vec![],
      env: Default::default(),
      timeout_secs: None,
      max_concurrent_calls: None,
    }));
    let adapter = McpToolAdapter::new(
      pool,
      McpTool {
        name: "echo/raw".to_string(),
        description: Some("Echo".to_string()),
        input_schema: serde_json::json!({"type": "object"}),
      },
    );

    let definition = adapter.definition();
    assert_eq!(definition.name, "mcp_local_demo_echo_raw");
    assert_eq!(definition.metadata.source, yanshi_tools::ToolSource::Mcp);
    assert_eq!(
      definition.metadata.mcp_server_name.as_deref(),
      Some("local-demo")
    );
    assert_eq!(
      definition.metadata.mcp_tool_name.as_deref(),
      Some("echo/raw")
    );
  }

  #[test]
  fn mcp_adapter_metadata_reads_idempotency_hints() {
    let pool = Arc::new(McpClientPool::new(McpServerConfig {
      name: "local-demo".to_string(),
      command: "python3".to_string(),
      args: vec![],
      env: Default::default(),
      timeout_secs: None,
      max_concurrent_calls: None,
    }));
    let adapter = McpToolAdapter::new(
      pool,
      McpTool {
        name: "search".to_string(),
        description: Some("Search docs [idempotent]".to_string()),
        input_schema: serde_json::json!({"type": "object"}),
      },
    );

    assert_eq!(
      adapter.definition().metadata.idempotency,
      ToolIdempotency::Idempotent
    );
  }

  #[tokio::test]
  async fn mcp_adapter_rejects_invalid_params_before_remote_call() {
    let pool = Arc::new(McpClientPool::new(McpServerConfig {
      name: "local-demo".to_string(),
      command: "python3".to_string(),
      args: vec![],
      env: Default::default(),
      timeout_secs: None,
      max_concurrent_calls: None,
    }));
    let adapter = McpToolAdapter::new(
      pool,
      McpTool {
        name: "search".to_string(),
        description: Some("Search".to_string()),
        input_schema: serde_json::json!({
          "type": "object",
          "required": ["query"],
          "properties": {
            "query": { "type": "string" }
          }
        }),
      },
    );

    let result = adapter.execute(serde_json::json!({"query": 42})).await;
    let error = result.unwrap_err().to_string();

    assert!(error.contains("Invalid parameters"));
    assert!(error.contains("search"));
    assert!(error.contains("query"));
  }

  // ============================================================================
  // W5.8-1: reconnect-on-error regression tests
  // ============================================================================
  //
  // Each fixture is a one-shot `sh -c` "server": it answers exactly one
  // initialize handshake + one tools/list (or tools/call), then the shell
  // script ends and the child process exits. A second call over the same
  // cached `MCPClient` then hits a connection error (broken pipe / closed
  // reader) rather than a timeout — the exact gap W5.8-1 closes. Before the
  // fix, the dead client stayed cached forever; after, the transient error
  // clears the slot so the next call reconnects (spawns a fresh process,
  // which — being `sh -c` — replays the same one-shot script and succeeds
  // again).

  #[cfg(unix)]
  fn one_shot_tools_list_config(name: &str) -> McpServerConfig {
    let script = r#"
read -r line1
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1.0"}}}'
read -r line2
read -r line3
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
"#;
    McpServerConfig {
      name: name.to_string(),
      command: "sh".to_string(),
      args: vec!["-c".to_string(), script.to_string()],
      env: Default::default(),
      timeout_secs: Some(5),
      max_concurrent_calls: None,
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn list_tools_reconnects_after_transient_error_instead_of_reusing_dead_client() {
    let pool = McpClientPool::new(one_shot_tools_list_config("flaky-list"));

    // First call: connects, completes the one-shot script, gets an empty
    // tools list back. The child process then exits.
    pool.list_tools().await.expect("first call succeeds");

    // Give the reader task time to observe EOF so the second call's
    // failure is a clean connection error, not a race.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Second call: the cached client's transport is dead. This must fail
    // (nothing left to serve the request) — that's expected either way,
    // fixed or not.
    pool
      .list_tools()
      .await
      .expect_err("cached client is dead, second call must fail");

    // Third call is the actual regression check: with the fix, the
    // transient error above cleared the cached slot, so this call
    // reconnects (spawns a fresh one-shot process) and succeeds. Without
    // the fix, it would keep reusing the same dead client and fail again.
    pool
      .list_tools()
      .await
      .expect("third call must reconnect instead of reusing the dead client");
  }

  #[cfg(unix)]
  fn one_shot_tool_call_config(name: &str) -> McpServerConfig {
    let script = r#"
read -r line1
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1.0"}}}'
read -r line2
read -r line3
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[]}}'
"#;
    McpServerConfig {
      name: name.to_string(),
      command: "sh".to_string(),
      args: vec!["-c".to_string(), script.to_string()],
      env: Default::default(),
      timeout_secs: Some(5),
      max_concurrent_calls: None,
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn call_tool_reconnects_after_transient_error_instead_of_reusing_dead_client() {
    let pool = Arc::new(McpClientPool::new(one_shot_tool_call_config("flaky-call")));
    let adapter = McpToolAdapter::new(
      pool.clone(),
      McpTool {
        name: "noop".to_string(),
        description: Some("Noop".to_string()),
        input_schema: serde_json::json!({"type": "object"}),
      },
    );

    adapter
      .execute(serde_json::json!({}))
      .await
      .expect("first call succeeds");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    adapter
      .execute(serde_json::json!({}))
      .await
      .expect_err("cached client is dead, second call must fail");

    adapter
      .execute(serde_json::json!({}))
      .await
      .expect("third call must reconnect instead of reusing the dead client");
  }
}
