use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::sandbox::{
  NoopSandboxBackend, SandboxBackend, SandboxPolicy, SandboxScope, SandboxStatus,
};
use crate::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput};

/// How a [`ShellTool`] interprets the `command` parameter.
///
/// `Argv` (the default after Q1.1.1) parses the command into an argv vector,
/// rejecting unquoted shell metacharacters (`|`, `;`, `&`, `$`, `` ` ``, `>`,
/// `<`, parentheses, newlines). This eliminates the `sh -c` bypass where
/// `echo; rm -rf /` would pass the in-process `allowed_commands` check.
///
/// `Shell` re-enables `sh -c` interpretation but only when an enforcing OS
/// sandbox backend is wired in. Callers MUST `.with_os_sandbox()` (or attach
/// an enforcing backend manually) before constructing in `Shell` mode; the
/// tool fails closed at execute time otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellInterpretation {
  /// argv-only spawn, no shell metacharacters (default).
  #[default]
  Argv,
  /// `sh -c <command>` spawn; requires an enforcing OS sandbox backend.
  Shell,
}

/// Execute a shell command. By default the command is parsed into an argv
/// vector (no shell interpretation); opt in to `sh -c` semantics via
/// [`ShellTool::with_shell_interpretation`].
pub struct ShellTool {
  policy: Arc<SandboxPolicy>,
  backend: Arc<dyn SandboxBackend>,
  mode: ShellInterpretation,
}

impl ShellTool {
  /// Create a `ShellTool` in [`ShellInterpretation::Argv`] mode. The OS
  /// sandbox is a no-op until [`Self::with_os_sandbox`] is called.
  /// Capability and policy gating still apply via [`crate::ToolRegistry::execute`].
  pub fn new(policy: Arc<SandboxPolicy>) -> Self {
    Self {
      policy,
      backend: Arc::new(NoopSandboxBackend::new(
        "ShellTool default backend; opt in via with_os_sandbox()",
      )),
      mode: ShellInterpretation::Argv,
    }
  }

  /// Convenience: create with the default (restrictive) policy and no OS sandbox.
  pub fn default_policy() -> Self {
    Self::new(Arc::new(SandboxPolicy::default()))
  }

  /// Wrap subsequent invocations in the platform's enforcing sandbox backend
  /// ([`crate::sandbox::default_backend`]). On macOS this is `sandbox-exec`; on
  /// Linux this is a seccomp BPF filter installed via `pre_exec`. On other
  /// platforms the wrap returns an error and the call fails before spawn.
  pub fn with_os_sandbox(mut self) -> Self {
    self.backend = crate::sandbox::default_backend();
    self
  }

  /// Inject a custom backend (e.g. for tests).
  pub fn with_backend(mut self, backend: Arc<dyn SandboxBackend>) -> Self {
    self.backend = backend;
    self
  }

  /// Switch into `sh -c` interpretation. The tool will refuse to spawn at
  /// execute time unless the active backend reports `is_enforcing() == true`.
  pub fn with_shell_interpretation(mut self) -> Self {
    self.mode = ShellInterpretation::Shell;
    self
  }

  /// Returns the configured interpretation mode (for tests / inspection).
  pub fn interpretation(&self) -> ShellInterpretation {
    self.mode
  }
}

#[async_trait]
impl Tool for ShellTool {
  fn name(&self) -> &str {
    "shell"
  }

  fn description(&self) -> &str {
    "Execute a shell command and return its stdout/stderr. \
        Use for running system commands, inspecting files, or performing \
        OS-level operations."
  }

  fn parameters_schema(&self) -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to execute (argv-only by default; shell interpretation requires explicit opt-in)"
            }
        },
        "required": ["command"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin_named(self.name())
  }

  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    ToolIdempotency::NonIdempotent
  }

  fn sandbox_status(&self) -> Option<SandboxStatus> {
    Some(SandboxStatus::from_backend(self.backend.as_ref()))
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let command = params["command"]
      .as_str()
      .ok_or_else(|| ToolError::InvalidParams {
        message: "Missing required parameter 'command'".to_string(),
      })?;

    let mut cmd = match self.mode {
      ShellInterpretation::Argv => self.prepare_argv(command)?,
      ShellInterpretation::Shell => self.prepare_shell(command)?,
    };

    let timeout = Duration::from_secs(self.policy.max_exec_time_secs);

    let scope = build_scope_from_policy(&self.policy);
    let caps = self.requires_capabilities();
    self
      .backend
      .wrap_command(&mut cmd, &caps, &scope)
      .map_err(|err| ToolError::SandboxViolation {
        message: format!("OS sandbox preparation failed: {err}"),
      })?;
    scrub_inherited_env(&mut cmd);

    let output = tokio::time::timeout(timeout, cmd.output())
      .await
      .map_err(|_| ToolError::ExecutionFailed {
        message: format!(
          "Command timed out after {} seconds",
          self.policy.max_exec_time_secs
        ),
      })?
      .map_err(ToolError::IoError)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
      let result = if stdout.trim().is_empty() {
        "(no output)".to_string()
      } else {
        stdout.trim().to_string()
      };
      Ok(ToolOutput::success(result))
    } else {
      let msg = if stderr.trim().is_empty() {
        stdout.trim().to_string()
      } else {
        stderr.trim().to_string()
      };
      Ok(ToolOutput::error(format!(
        "Exit code {}: {}",
        output.status.code().unwrap_or(-1),
        msg
      )))
    }
  }
}

impl ShellTool {
  fn prepare_argv(&self, command: &str) -> Result<tokio::process::Command, ToolError> {
    let argv = parse_argv_safe(command).map_err(|message| ToolError::SandboxViolation {
      message: format!(
        "argv parse failure (shell interpretation is disabled by default): {message}"
      ),
    })?;

    let base_cmd = argv
      .first()
      .ok_or_else(|| ToolError::InvalidParams {
        message: "command parsed into empty argv".to_string(),
      })?
      .as_str();
    if !self.policy.is_command_allowed(base_cmd) {
      return Err(ToolError::SandboxViolation {
        message: format!("Command '{}' is not in the allowed-commands list", base_cmd),
      });
    }

    let mut cmd = tokio::process::Command::new(base_cmd);
    cmd.args(&argv[1..]);
    Ok(cmd)
  }

  fn prepare_shell(&self, command: &str) -> Result<tokio::process::Command, ToolError> {
    if !self.backend.is_enforcing() {
      return Err(ToolError::SandboxViolation {
        message:
          "shell interpretation requires an enforcing OS sandbox backend (call .with_os_sandbox())"
            .to_string(),
      });
    }

    // Best-effort: split on shell separators and validate each segment's
    // leading word. Full POSIX-shell parsing is out of scope; operators
    // opting into shell interpretation accept this is best-effort and
    // rely on the OS sandbox as the real boundary.
    for segment in extract_shell_segments(command) {
      let leading = segment
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '/' && c != '-');
      if leading.is_empty() {
        continue;
      }
      if !self.policy.is_command_allowed(leading) {
        return Err(ToolError::SandboxViolation {
          message: format!(
            "Command '{}' (in shell segment '{}') is not in the allowed-commands list",
            leading,
            segment.trim()
          ),
        });
      }
    }

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    Ok(cmd)
  }
}

#[derive(Debug, Clone, Copy)]
enum ParseState {
  Plain,
  Single,
  Double,
}

/// Parse `command` into an argv vector, rejecting any unquoted shell
/// metacharacter. The parser honors single- and double-quoted strings,
/// treats `\\` as an escape outside single quotes, and rejects command
/// substitution (`$(...)`, backticks) inside double quotes as well.
///
/// Returns `Err(reason)` for the first metacharacter found or when a
/// quote is never terminated.
fn parse_argv_safe(command: &str) -> Result<Vec<String>, String> {
  let mut tokens: Vec<String> = Vec::new();
  let mut current = String::new();
  let mut has_token_started = false;
  let mut state = ParseState::Plain;
  let mut chars = command.chars().peekable();

  while let Some(c) = chars.next() {
    match state {
      ParseState::Plain => match c {
        '\'' => {
          state = ParseState::Single;
          has_token_started = true;
        }
        '"' => {
          state = ParseState::Double;
          has_token_started = true;
        }
        '\\' => {
          // Backslash escapes the next char (POSIX-shell semantics outside quotes).
          if let Some(next) = chars.next() {
            current.push(next);
            has_token_started = true;
          } else {
            return Err("trailing backslash with no escape target".to_string());
          }
        }
        c if c.is_whitespace() => {
          if has_token_started {
            tokens.push(std::mem::take(&mut current));
            has_token_started = false;
          }
        }
        '|' | ';' | '&' | '$' | '`' | '>' | '<' | '(' | ')' | '\n' => {
          return Err(format!(
            "unquoted shell metacharacter '{c}' is not allowed in argv mode"
          ));
        }
        other => {
          current.push(other);
          has_token_started = true;
        }
      },
      ParseState::Single => match c {
        '\'' => state = ParseState::Plain,
        other => current.push(other),
      },
      ParseState::Double => match c {
        '"' => state = ParseState::Plain,
        '\\' => {
          // Inside double quotes, backslash only escapes `\"`, `\\`, `\$`, ``\` ``, `\n`.
          if let Some(&next) = chars.peek() {
            if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
              current.push(next);
              chars.next();
            } else {
              current.push('\\');
            }
          } else {
            return Err("trailing backslash inside double quotes".to_string());
          }
        }
        '$' | '`' => {
          return Err(format!(
            "shell command substitution character '{c}' is not allowed inside double quotes in argv mode"
          ));
        }
        other => current.push(other),
      },
    }
  }

  if !matches!(state, ParseState::Plain) {
    return Err("unterminated quoted string".to_string());
  }
  if has_token_started {
    tokens.push(current);
  }
  if tokens.is_empty() {
    return Err("command string is empty".to_string());
  }
  Ok(tokens)
}

/// Split a shell command on top-level separators (`;`, `&&`, `||`, `|`, newline).
/// Best-effort: respects single/double quote boundaries to avoid splitting
/// inside `awk 'BEGIN { ... }'` style arguments.
fn extract_shell_segments(command: &str) -> Vec<String> {
  let mut segments = Vec::new();
  let mut current = String::new();
  let mut state = ParseState::Plain;
  let mut chars = command.chars().peekable();
  while let Some(c) = chars.next() {
    match state {
      ParseState::Plain => match c {
        '\'' => {
          current.push(c);
          state = ParseState::Single;
        }
        '"' => {
          current.push(c);
          state = ParseState::Double;
        }
        '\\' => {
          current.push(c);
          if let Some(next) = chars.next() {
            current.push(next);
          }
        }
        ';' | '\n' => {
          segments.push(std::mem::take(&mut current));
        }
        '&' | '|' => {
          // Detect `&&` and `||` as separators; single `|` and single `&`
          // also split (pipe and background).
          if matches!(chars.peek(), Some(&next) if next == c) {
            chars.next();
          }
          segments.push(std::mem::take(&mut current));
        }
        other => current.push(other),
      },
      ParseState::Single => {
        current.push(c);
        if c == '\'' {
          state = ParseState::Plain;
        }
      }
      ParseState::Double => {
        current.push(c);
        if c == '"' {
          state = ParseState::Plain;
        } else if c == '\\'
          && let Some(next) = chars.next()
        {
          current.push(next);
        }
      }
    }
  }
  segments.push(current);
  segments
}

/// Variables a sandboxed child may inherit from the host. Everything else
/// (API keys, `DATABASE_URL`, gateway tokens, ...) is dropped: the child's
/// output flows back to the LLM vendor and into traces, so an inherited
/// secret is an exfiltrated secret. Same stance as the MCP stdio and skill
/// validator env allowlists.
pub(crate) const CHILD_ENV_ALLOWLIST: &[&str] = &[
  "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "TERM",
];

/// Drop the host environment from `cmd`, keeping only [`CHILD_ENV_ALLOWLIST`]
/// plus any variable already set explicitly on `cmd`. Call it *after*
/// `SandboxBackend::wrap_command`: the macOS backend rebuilds the command
/// and only carries explicit vars over, so an earlier `env_clear()` would be
/// silently lost and the wrapper would inherit everything again.
pub(crate) fn scrub_inherited_env(cmd: &mut tokio::process::Command) {
  let explicit: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = cmd
    .as_std()
    .get_envs()
    .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
    .collect();
  cmd.env_clear();
  for key in CHILD_ENV_ALLOWLIST {
    if let Some(value) = std::env::var_os(key) {
      cmd.env(key, value);
    }
  }
  for (key, value) in explicit {
    match value {
      Some(value) => cmd.env(key, value),
      None => cmd.env_remove(key),
    };
  }
}

/// Project a [`SandboxPolicy`] into the [`SandboxScope`] consumed by OS-level
/// backends. We treat `allowed_paths` as both read- and write-allowed because
/// the in-process layer does not split read vs. write at policy level — that
/// distinction is enforced by `Capability::FsRead` vs. `Capability::FsWrite`.
///
/// When the policy is permissive (empty allowlist) we fall back to a
/// conservative default: `/tmp` plus the current working directory. This
/// keeps shell builtins working without granting the entire filesystem.
pub(crate) fn build_scope_from_policy(policy: &SandboxPolicy) -> SandboxScope {
  let mut scope = SandboxScope::new();
  if policy.allowed_paths.is_empty() {
    scope.read_paths.push(PathBuf::from("/tmp"));
    if let Ok(cwd) = std::env::current_dir() {
      scope.read_paths.push(cwd.clone());
      scope.write_paths.push(cwd.clone());
      scope.working_directory = Some(cwd);
    }
  } else {
    for path in &policy.allowed_paths {
      scope.read_paths.push(path.clone());
      scope.write_paths.push(path.clone());
    }
  }
  scope.max_memory_bytes = policy.max_memory_bytes;
  scope.max_pids = policy.max_pids;
  scope.max_cpu_secs = policy.max_cpu_secs;
  scope
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn default_argv_mode_runs_simple_command() {
    let tool = ShellTool::default_policy();
    let result = tool
      .execute(json!({"command": "echo hello"}))
      .await
      .unwrap();
    assert!(result.content.contains("hello"));
  }

  #[tokio::test]
  async fn child_does_not_inherit_host_secrets() {
    // `cargo test` exports CARGO_MANIFEST_DIR into this process; it stands
    // in for an API key or DATABASE_URL a gateway host would carry.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let policy = SandboxPolicy {
      allowed_commands: vec!["env".to_string()],
      ..SandboxPolicy::default()
    };
    let tool = ShellTool::new(Arc::new(policy)).with_os_sandbox();
    let result = tool.execute(json!({"command": "env"})).await.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(
      !result.content.contains("CARGO_MANIFEST_DIR="),
      "host env leaked into child: {}",
      result.content
    );
    assert!(result.content.contains("PATH="), "{}", result.content);
  }

  #[tokio::test]
  async fn unknown_command_is_rejected_before_backend_runs() {
    let tool = ShellTool::default_policy();
    let result = tool.execute(json!({"command": "rm -rf /"})).await;
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
  }

  #[tokio::test]
  async fn semicolon_bypass_is_rejected_in_argv_mode() {
    let tool = ShellTool::default_policy();
    let result = tool
      .execute(json!({"command": "echo hello; rm -rf /tmp/yanshi-should-not-run"}))
      .await;
    let err = result.expect_err("semicolon bypass must be rejected");
    match err {
      ToolError::SandboxViolation { message } => {
        assert!(
          message.contains("';'") || message.contains("metacharacter"),
          "expected metacharacter rejection, got: {message}"
        );
      }
      other => panic!("expected SandboxViolation, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn double_ampersand_bypass_is_rejected_in_argv_mode() {
    let tool = ShellTool::default_policy();
    let result = tool
      .execute(json!({"command": "echo hello && rm -rf /tmp/yanshi-should-not-run"}))
      .await;
    let err = result.expect_err("&& bypass must be rejected");
    match err {
      ToolError::SandboxViolation { message } => {
        assert!(
          message.contains("'&'") || message.contains("metacharacter"),
          "expected metacharacter rejection, got: {message}"
        );
      }
      other => panic!("expected SandboxViolation, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn command_substitution_bypass_is_rejected_in_argv_mode() {
    let tool = ShellTool::default_policy();
    let result = tool
      .execute(json!({"command": "echo $(rm -rf /tmp/yanshi-should-not-run)"}))
      .await;
    let err = result.expect_err("$() bypass must be rejected");
    match err {
      ToolError::SandboxViolation { message } => {
        assert!(
          message.contains("'$'") || message.contains("'('") || message.contains("metacharacter"),
          "expected metacharacter rejection, got: {message}"
        );
      }
      other => panic!("expected SandboxViolation, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn pipe_bypass_is_rejected_in_argv_mode() {
    let tool = ShellTool::default_policy();
    let result = tool.execute(json!({"command": "echo hello | sh"})).await;
    let err = result.expect_err("| bypass must be rejected");
    assert!(matches!(err, ToolError::SandboxViolation { .. }));
  }

  #[tokio::test]
  async fn backtick_bypass_is_rejected_in_argv_mode() {
    let tool = ShellTool::default_policy();
    let result = tool
      .execute(json!({"command": "echo `rm -rf /tmp/yanshi-should-not-run`"}))
      .await;
    let err = result.expect_err("backtick bypass must be rejected");
    assert!(matches!(err, ToolError::SandboxViolation { .. }));
  }

  #[tokio::test]
  async fn quoted_arguments_are_preserved_in_argv_mode() {
    let policy = Arc::new(SandboxPolicy {
      allowed_commands: vec!["awk".to_string()],
      max_exec_time_secs: 5,
      ..SandboxPolicy::default()
    });
    let tool = ShellTool::new(policy);
    // Single-quoted argument with shell metacharacters inside; they should be literal.
    let result = tool
      .execute(json!({"command": "awk 'BEGIN { print \"yanshi\" }'"}))
      .await
      .expect("quoted args must parse cleanly");
    assert!(result.content.contains("yanshi"));
  }

  #[tokio::test]
  async fn shell_mode_without_enforcing_backend_is_rejected() {
    let tool = ShellTool::default_policy().with_shell_interpretation();
    let result = tool.execute(json!({"command": "echo hello"})).await;
    let err = result.expect_err("shell mode without OS sandbox must fail closed");
    match err {
      ToolError::SandboxViolation { message } => {
        assert!(
          message.contains("enforcing OS sandbox"),
          "expected enforcing-OS-sandbox message, got: {message}"
        );
      }
      other => panic!("expected SandboxViolation, got {other:?}"),
    }
  }

  #[test]
  fn argv_parser_splits_on_whitespace_respecting_quotes() {
    let argv = parse_argv_safe(r#"git commit -m "fix: a thing""#).unwrap();
    assert_eq!(argv, vec!["git", "commit", "-m", "fix: a thing"]);
  }

  #[test]
  fn argv_parser_rejects_unterminated_quote() {
    assert!(parse_argv_safe("echo 'hello").is_err());
    assert!(parse_argv_safe("echo \"hello").is_err());
  }

  #[test]
  fn argv_parser_handles_escaped_metacharacter() {
    // A backslash-escaped `$` outside quotes should become a literal `$`.
    let argv = parse_argv_safe(r#"echo \$HOME"#).unwrap();
    assert_eq!(argv, vec!["echo", "$HOME"]);
  }

  #[test]
  fn extract_shell_segments_splits_on_top_level_operators() {
    let segs = extract_shell_segments("echo a; echo b && echo c | echo d");
    let trimmed: Vec<String> = segs.into_iter().map(|s| s.trim().to_string()).collect();
    assert_eq!(trimmed, vec!["echo a", "echo b", "echo c", "echo d"]);
  }

  #[test]
  fn extract_shell_segments_respects_quotes() {
    let segs = extract_shell_segments("awk 'BEGIN { print 1; print 2 }'");
    let trimmed: Vec<String> = segs.into_iter().map(|s| s.trim().to_string()).collect();
    // Should NOT split on the `;` inside the single-quoted block.
    assert_eq!(trimmed.len(), 1);
    assert!(trimmed[0].contains("BEGIN"));
  }

  #[test]
  fn permissive_policy_falls_back_to_tmp_plus_cwd() {
    let policy = SandboxPolicy::permissive();
    let scope = build_scope_from_policy(&policy);

    assert!(
      scope
        .read_paths
        .iter()
        .any(|p| p == std::path::Path::new("/tmp"))
    );
    assert!(scope.working_directory.is_some());
  }

  #[test]
  fn restrictive_policy_uses_only_allowed_paths() {
    let policy = SandboxPolicy {
      allowed_paths: vec![PathBuf::from("/var/yanshi")],
      ..SandboxPolicy::default()
    };
    let scope = build_scope_from_policy(&policy);

    assert_eq!(scope.read_paths, vec![PathBuf::from("/var/yanshi")]);
    assert_eq!(scope.write_paths, vec![PathBuf::from("/var/yanshi")]);
    assert!(scope.working_directory.is_none());
  }
}
