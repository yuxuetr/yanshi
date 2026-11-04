use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::sandbox::SandboxStatus;

/// Result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
  /// The output content as a string (stdout, response body, etc.)
  pub content: String,
  /// Whether this output represents an error condition
  pub is_error: bool,
  /// Structured content parts returned by tools that support typed output.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub parts: Vec<ToolOutputPart>,
}

/// Typed output content returned by tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutputPart {
  Text {
    text: String,
  },
  Image {
    data: String,
    mime_type: String,
  },
  Resource {
    uri: String,
    mime_type: Option<String>,
    text: Option<String>,
  },
}

impl ToolOutput {
  pub fn success(content: impl Into<String>) -> Self {
    Self {
      content: content.into(),
      is_error: false,
      parts: Vec::new(),
    }
  }

  pub fn error(content: impl Into<String>) -> Self {
    Self {
      content: content.into(),
      is_error: true,
      parts: Vec::new(),
    }
  }

  pub fn success_parts(content: impl Into<String>, parts: Vec<ToolOutputPart>) -> Self {
    Self {
      content: content.into(),
      is_error: false,
      parts,
    }
  }

  pub fn error_parts(content: impl Into<String>, parts: Vec<ToolOutputPart>) -> Self {
    Self {
      content: content.into(),
      is_error: true,
      parts,
    }
  }

  pub fn with_parts(mut self, parts: Vec<ToolOutputPart>) -> Self {
    self.parts = parts;
    self
  }
}

/// A parsed tool call from an LLM response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
  /// The tool name to invoke
  pub tool: String,
  /// Parameters for the tool (matches the tool's JSON schema)
  pub params: Value,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn string_output_constructors_have_no_parts_by_default() {
    let output = ToolOutput::success("ok");

    assert_eq!(output.content, "ok");
    assert!(!output.is_error);
    assert!(output.parts.is_empty());
  }

  #[test]
  fn typed_output_preserves_parts_and_compatible_content() {
    let output = ToolOutput::success_parts(
      "hello",
      vec![ToolOutputPart::Text {
        text: "hello".to_string(),
      }],
    );

    assert_eq!(output.content, "hello");
    assert_eq!(
      output.parts,
      vec![ToolOutputPart::Text {
        text: "hello".to_string(),
      }]
    );
  }

  #[test]
  fn metadata_builder_records_idempotency() {
    let metadata =
      ToolMetadata::builtin_named("file").with_idempotency(ToolIdempotency::Idempotent);

    assert_eq!(metadata.idempotency, ToolIdempotency::Idempotent);
  }
}

/// OpenAI-compatible function definition for use in prompts or API calls
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
  pub name: String,
  pub description: String,
  pub parameters: Value,
  #[serde(default)]
  pub metadata: ToolMetadata,
}

/// Stable permission categories used to govern tool access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermission {
  /// Read local filesystem state.
  FilesystemRead,
  /// Write or mutate local filesystem state.
  FilesystemWrite,
  /// Execute local commands or scripts.
  ProcessExec,
  /// Make outbound network requests.
  Network,
  /// Connect to or invoke MCP servers.
  Mcp,
  /// Execute nested Yanshi workflows.
  Workflow,
}

impl ToolPermission {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::FilesystemRead => "filesystem_read",
      Self::FilesystemWrite => "filesystem_write",
      Self::ProcessExec => "process_exec",
      Self::Network => "network",
      Self::Mcp => "mcp",
      Self::Workflow => "workflow",
    }
  }
}

/// Permission set attached to tool metadata for inspection and governance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolPermissionSet {
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub permissions: Vec<ToolPermission>,
}

impl ToolPermissionSet {
  pub fn new(permissions: impl IntoIterator<Item = ToolPermission>) -> Self {
    let mut permissions: Vec<_> = permissions.into_iter().collect();
    permissions.sort_by_key(|permission| permission.as_str());
    permissions.dedup();
    Self { permissions }
  }

  pub fn empty() -> Self {
    Self::default()
  }

  pub fn allows(&self, permission: &ToolPermission) -> bool {
    self.permissions.contains(permission)
  }

  pub fn builtin(tool_name: &str) -> Self {
    match tool_name {
      "shell" => Self::new([ToolPermission::ProcessExec]),
      "file" => Self::new([
        ToolPermission::FilesystemRead,
        ToolPermission::FilesystemWrite,
      ]),
      "http" => Self::new([ToolPermission::Network]),
      // S4.2: code_exec runs llm-generated code inside a ContainerBackend
      // (never the host process directly), so it needs ProcessExec for
      // admission purposes even though its filesystem access is confined
      // entirely to its own ephemeral in-container workdir.
      "code_exec" => Self::new([ToolPermission::ProcessExec]),
      _ => Self::empty(),
    }
  }

  pub fn script() -> Self {
    Self::new([ToolPermission::ProcessExec, ToolPermission::FilesystemRead])
  }

  pub fn mcp() -> Self {
    Self::new([ToolPermission::Mcp, ToolPermission::Network])
  }

  pub fn workflow() -> Self {
    Self::new([ToolPermission::Workflow])
  }
}

/// Stable source classification for tools registered in Yanshi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
  Builtin,
  Script,
  Mcp,
  Workflow,
}

impl ToolSource {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Builtin => "builtin",
      Self::Script => "script",
      Self::Mcp => "mcp",
      Self::Workflow => "workflow",
    }
  }
}

/// Replay safety classification for tool calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdempotency {
  /// Safe to repeat with the same parameters.
  Idempotent,
  /// Not safe to repeat automatically because it mutates state or triggers side effects.
  NonIdempotent,
  /// The tool has not declared replay semantics.
  #[default]
  Unknown,
}

/// Y7.2: what a call does to the world beyond its own result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
  /// Stays within this system and its vendors (files, API calls, media
  /// generation): the default.
  #[default]
  Local,
  /// Puts content in front of an audience outside the system (a blog
  /// post, a social or podcast platform). It cannot be taken back, so
  /// the production profile approves every such call, every time.
  Publish,
}

impl ToolEffect {
  pub fn is_local(&self) -> bool {
    matches!(self, Self::Local)
  }
}

/// Metadata used for inspection, tracing, and tool registry diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
  pub source: ToolSource,
  #[serde(default)]
  pub permissions: ToolPermissionSet,
  #[serde(default)]
  pub idempotency: ToolIdempotency,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mcp_server_name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mcp_tool_name: Option<String>,
}

impl ToolMetadata {
  pub fn builtin() -> Self {
    Self::builtin_named("builtin")
  }

  pub fn builtin_named(tool_name: impl AsRef<str>) -> Self {
    Self {
      source: ToolSource::Builtin,
      permissions: ToolPermissionSet::builtin(tool_name.as_ref()),
      idempotency: builtin_tool_idempotency(tool_name.as_ref()),
      mcp_server_name: None,
      mcp_tool_name: None,
    }
  }

  pub fn script() -> Self {
    Self {
      source: ToolSource::Script,
      permissions: ToolPermissionSet::script(),
      idempotency: ToolIdempotency::NonIdempotent,
      mcp_server_name: None,
      mcp_tool_name: None,
    }
  }

  pub fn mcp(server_name: impl Into<String>, tool_name: impl Into<String>) -> Self {
    Self {
      source: ToolSource::Mcp,
      permissions: ToolPermissionSet::mcp(),
      idempotency: ToolIdempotency::Unknown,
      mcp_server_name: Some(server_name.into()),
      mcp_tool_name: Some(tool_name.into()),
    }
  }

  pub fn with_idempotency(mut self, idempotency: ToolIdempotency) -> Self {
    self.idempotency = idempotency;
    self
  }

  pub fn workflow() -> Self {
    Self {
      source: ToolSource::Workflow,
      permissions: ToolPermissionSet::workflow(),
      idempotency: ToolIdempotency::Unknown,
      mcp_server_name: None,
      mcp_tool_name: None,
    }
  }
}

impl Default for ToolMetadata {
  fn default() -> Self {
    Self::builtin()
  }
}

fn builtin_tool_idempotency(tool_name: &str) -> ToolIdempotency {
  match tool_name {
    "shell" | "code_exec" => ToolIdempotency::NonIdempotent,
    "file" | "http" => ToolIdempotency::Unknown,
    _ => ToolIdempotency::Unknown,
  }
}

/// Core trait that every tool must implement
#[async_trait]
pub trait Tool: Send + Sync {
  /// Unique, machine-readable name (e.g. "shell", "file", "http")
  fn name(&self) -> &str;

  /// Human-readable description shown to the LLM
  fn description(&self) -> &str;

  /// JSON Schema for the tool's parameters object
  fn parameters_schema(&self) -> Value;

  /// Metadata for registry inspection and diagnostics.
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin()
  }

  /// Replay safety for this concrete invocation.
  ///
  /// Tools whose safety depends on parameters can override this method while
  /// still exposing a conservative default through metadata.
  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    self.metadata().idempotency
  }

  /// Y7.2: the call's [`ToolEffect`]. Per call, like
  /// [`Self::idempotency`], so one tool can publish for some parameters
  /// only (a `status: "published"` vs a draft).
  fn effect(&self, _params: &Value) -> ToolEffect {
    ToolEffect::Local
  }

  /// OS-level capabilities the tool needs to execute.
  ///
  /// The default decomposes the declared [`ToolPermission`] set into
  /// [`Capability`] values; tools can override to declare a tighter set.
  fn requires_capabilities(&self) -> Vec<Capability> {
    Capability::from_permissions(&self.metadata().permissions.permissions)
  }

  /// Snapshot of the OS sandbox backend this tool wraps a child process in,
  /// if any. Defaults to `None` because most tools run entirely in-process.
  ///
  /// Tools that spawn subprocesses (shell, script, plugin) should override
  /// this so the active backend name and enforcement level are visible in
  /// `ToolCapabilityDecision` events and `yanshi doctor` output. The
  /// snapshot must reflect the backend actually used at execution time, not
  /// the platform default — tests and offline runs may inject a no-op
  /// backend and the trace should reflect that.
  fn sandbox_status(&self) -> Option<SandboxStatus> {
    None
  }

  /// Execute the tool and return its output.
  ///
  /// # Cancellation contract (cooperative)
  ///
  /// Yanshi's runtimes (`ReActAgent`, `PlanExecuteAgent`, supervisors) race
  /// this future against the run's [`AgentCancellationToken`] via
  /// `tokio::select!`. When the token fires, the runtime **drops** this future
  /// — there is no explicit `cancel()` hook on the trait. As a result:
  ///
  /// - Futures composed entirely of cooperatively-cancellable awaits
  ///   (`tokio::time::sleep`, `tokio::process::Child::wait`, `reqwest`,
  ///   `tokio::io::*`, channel recv) are cancelled cleanly: any
  ///   `Drop` guards in the future's state run, in-flight `tokio` syscalls are
  ///   aborted, and child processes spawned via `tokio::process::Command` are
  ///   killed (when `Command::kill_on_drop(true)` is set).
  /// - Work that the tool has **detached** from this future will NOT be
  ///   aborted. Concretely: anything spawned via `tokio::spawn` /
  ///   `tokio::task::spawn_blocking` without joining inside the same future,
  ///   blocking syscalls run on a thread you don't own, FFI calls that do not
  ///   honour Drop, and `std::process::Command` (without `kill_on_drop`).
  ///
  /// Tool authors who need to abort detached work must wire up their own
  /// cancellation signal (e.g. an `Arc<AtomicBool>` checked by their worker,
  /// or `tokio_util::sync::CancellationToken` whose `cancel()` they call from
  /// a guard struct held by the returned future). The runtime cannot do this
  /// for you because the `Tool` trait is sync-cancellable only via Drop.
  ///
  /// See `yanshi-agents/src/runtime.rs` (`AgentCancellationToken` docs +
  /// `tool_future_drop_runs_when_token_is_cancelled` regression test) for the
  /// reference behaviour pinned by `Q3.12.1`.
  ///
  /// [`AgentCancellationToken`]: ../../yanshi_agents/runtime/struct.AgentCancellationToken.html
  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError>;

  /// Convenience: build the full tool definition
  fn definition(&self) -> ToolDefinition {
    ToolDefinition {
      name: self.name().to_string(),
      description: self.description().to_string(),
      parameters: self.parameters_schema(),
      metadata: self.metadata(),
    }
  }

  /// Format the definition as a compact string for LLM system prompts
  fn prompt_description(&self) -> String {
    format!(
      "- **{}**: {}\n  Parameters: {}",
      self.name(),
      self.description(),
      serde_json::to_string(&self.parameters_schema()).unwrap_or_else(|_| "{}".to_string())
    )
  }
}
