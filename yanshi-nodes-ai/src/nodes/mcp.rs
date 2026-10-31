//! MCP (Model Context Protocol) integration node
//!
//! This node enables Yanshi workflows to call tools exposed by MCP servers.
//!
//! # Example Usage
//!
//! ```yaml
//! nodes:
//!   - id: read_file
//!     type: mcp
//!     parameters:
//!       server_command: ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//!       tool_name: read_file
//!       tool_params:
//!         path: "{{file_path}}"
//!       timeout_ms: 30000
//!       max_retries: 3
//! ```

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_mcp::client::ClientBuilder;

/// MCP Node for calling tools from MCP servers
#[derive(Debug, Clone)]
pub struct MCPNode {
  /// Server command to execute (e.g., `["npx", "-y", "@modelcontextprotocol/server-filesystem"]`)
  pub server_command: Vec<String>,

  /// Tool name to call
  pub tool_name: String,

  /// Tool parameters (JSON object)
  pub tool_params: Value,

  /// Timeout in milliseconds (default: 30000)
  pub timeout_ms: Option<u64>,

  /// Maximum retry attempts for the connection handshake (default: 3).
  /// The `tools/call` itself is sent at most once — a retry after a
  /// timeout could run the tool twice.
  pub max_retries: Option<u32>,

  /// Whether to cache the client connection (future feature)
  pub cache_connection: bool,
}

impl Default for MCPNode {
  fn default() -> Self {
    Self {
      server_command: vec![],
      tool_name: String::new(),
      tool_params: json!({}),
      timeout_ms: Some(30_000),
      max_retries: Some(3),
      cache_connection: false,
    }
  }
}

impl MCPNode {
  /// Create a new MCP node with the specified server command and tool
  pub fn new(server_command: Vec<String>, tool_name: String) -> Self {
    Self {
      server_command,
      tool_name,
      ..Default::default()
    }
  }

  /// Set tool parameters
  pub fn with_params(mut self, params: Value) -> Self {
    self.tool_params = params;
    self
  }

  /// Set timeout in milliseconds
  pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
    self.timeout_ms = Some(timeout_ms);
    self
  }

  /// Set maximum retry attempts
  pub fn with_max_retries(mut self, max_retries: u32) -> Self {
    self.max_retries = Some(max_retries);
    self
  }
}

#[async_trait]
impl AsyncNode for MCPNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    // 1. Extract and resolve parameters from inputs
    let server_command = if self.server_command.is_empty() {
      get_vec_string_input(inputs, "server_command")?
    } else {
      self.server_command.clone()
    };

    let tool_name = if self.tool_name.is_empty() {
      get_string_input(inputs, "tool_name")?.to_string()
    } else {
      self.tool_name.clone()
    };

    // Resolve tool parameters - allow runtime substitution
    let tool_params = match inputs.get("tool_params") {
      Some(FlowValue::Json(v)) => v.clone(),
      _ => self.tool_params.clone(),
    };

    tracing::debug!(server_command = ?server_command, "connecting to MCP server");

    // 2. Build MCP client with configuration
    let mut client_builder = ClientBuilder::new().with_stdio(server_command);

    if let Some(timeout_ms) = self
      .timeout_ms
      .or_else(|| get_optional_u64_input(inputs, "timeout_ms").ok().flatten())
    {
      client_builder = client_builder.with_timeout(Duration::from_millis(timeout_ms));
    }

    if let Some(max_retries) = self.max_retries.or_else(|| {
      get_optional_u64_input(inputs, "max_retries")
        .ok()
        .flatten()
        .map(|v| v as u32)
    }) {
      client_builder = client_builder.with_max_retries(max_retries);
    }

    let mut client = client_builder
      .build()
      .await
      .map_err(|e| YanshiError::ConfigurationError {
        message: format!("Failed to build MCP client: {}", e),
      })?;

    // 3. Connect and initialize
    client
      .connect()
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to MCP server: {}", e),
      })?;

    tracing::debug!("connected to MCP server");

    // 4. Call the tool
    tracing::debug!(tool = %tool_name, "calling MCP tool");

    let result = client
      .call_tool(&tool_name, tool_params)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("MCP tool call failed: {}", e),
      })?;

    tracing::debug!("MCP tool call completed");

    // 5. Disconnect gracefully
    client
      .disconnect()
      .await
      .map_err(|e| {
        tracing::warn!(error = %e, "failed to disconnect MCP client");
      })
      .ok();

    // 6. Convert result to JSON
    let result_json =
      serde_json::to_value(&result).map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to serialize MCP result: {}", e),
      })?;

    // 7. Return result
    let mut outputs = HashMap::new();
    outputs.insert("output".to_string(), FlowValue::Json(result_json));

    Ok(outputs)
  }
}

// Helper functions for extracting inputs

fn get_string_input<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Result<&'a str, YanshiError> {
  inputs
    .get(key)
    .and_then(|v| match v {
      FlowValue::Json(Value::String(s)) => Some(s.as_str()),
      _ => None,
    })
    .ok_or_else(|| YanshiError::NodeInputError {
      message: format!(
        "Required string input '{}' is missing or has wrong type",
        key
      ),
    })
}

fn get_vec_string_input(inputs: &AsyncNodeInputs, key: &str) -> Result<Vec<String>, YanshiError> {
  inputs
    .get(key)
    .and_then(|v| match v {
      FlowValue::Json(Value::Array(arr)) => arr
        .iter()
        .map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Option<Vec<String>>>(),
      _ => None,
    })
    .ok_or_else(|| YanshiError::NodeInputError {
      message: format!(
        "Required array of strings input '{}' is missing or has wrong type",
        key
      ),
    })
}

fn get_optional_u64_input(inputs: &AsyncNodeInputs, key: &str) -> Result<Option<u64>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::Number(n)) => Ok(n.as_u64()),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a number", key),
      }),
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_mcp_node_creation() {
    let node = MCPNode::new(
      vec!["npx".to_string(), "-y".to_string(), "server".to_string()],
      "test_tool".to_string(),
    );

    assert_eq!(node.server_command.len(), 3);
    assert_eq!(node.tool_name, "test_tool");
    assert_eq!(node.timeout_ms, Some(30_000));
    assert_eq!(node.max_retries, Some(3));
  }

  #[test]
  fn test_mcp_node_builder_pattern() {
    let node = MCPNode::new(vec!["test".to_string()], "tool".to_string())
      .with_params(json!({"key": "value"}))
      .with_timeout_ms(60_000)
      .with_max_retries(5);

    assert_eq!(node.timeout_ms, Some(60_000));
    assert_eq!(node.max_retries, Some(5));
    assert_eq!(node.tool_params, json!({"key": "value"}));
  }

  #[test]
  fn test_helper_get_string_input() {
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("test".to_string(), FlowValue::Json(json!("hello")));

    let result = get_string_input(&inputs, "test");
    assert_eq!(result.unwrap(), "hello");
  }

  #[test]
  fn test_helper_get_vec_string_input() {
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("test".to_string(), FlowValue::Json(json!(["a", "b", "c"])));

    let result = get_vec_string_input(&inputs, "test");
    assert_eq!(result.unwrap(), vec!["a", "b", "c"]);
  }

  #[test]
  fn test_helper_get_optional_u64_input() {
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("test".to_string(), FlowValue::Json(json!(123)));

    let result = get_optional_u64_input(&inputs, "test");
    assert_eq!(result.unwrap(), Some(123));

    let missing = get_optional_u64_input(&inputs, "missing");
    assert_eq!(missing.unwrap(), None);
  }

  // Integration test with mock MCP server would go here
  // Requires running MCP server, so marked as ignored by default
  #[tokio::test]
  #[ignore]
  async fn test_mcp_node_integration() {
    // This test requires a running MCP server
    // Example: npx -y @modelcontextprotocol/server-filesystem /tmp

    let node = MCPNode::new(
      vec![
        "npx".to_string(),
        "-y".to_string(),
        "@modelcontextprotocol/server-filesystem".to_string(),
        "/tmp".to_string(),
      ],
      "list_directory".to_string(),
    )
    .with_params(json!({"path": "/tmp"}));

    let inputs = AsyncNodeInputs::new();
    let result = node.execute(&inputs).await;

    assert!(result.is_ok(), "MCP node execution failed: {:?}", result);
  }
}
