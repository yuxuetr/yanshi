use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::sandbox::{
  NoopSandboxBackend, SandboxBackend, SandboxPolicy, SandboxScope, SandboxStatus,
};
use crate::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput};

/// Execute a named script from the skill's `scripts/` directory.
///
/// The agent passes:
/// - `script`: filename relative to the scripts directory (e.g. `"check_syntax.py"`)
/// - `args`: optional JSON object forwarded to the script as JSON on stdin
///
/// The interpreter is inferred from the file extension:
/// | Extension | Interpreter  |
/// |-----------|-------------|
/// | `.py`     | `python3`   |
/// | `.sh`     | `bash`      |
/// | `.js`     | `node`      |
///
/// Arguments are serialised to JSON and piped to the script on **stdin**.
/// The script's **stdout** is returned as the tool output.
pub struct ScriptTool {
  /// Absolute path to the `scripts/` directory for the current skill.
  scripts_dir: PathBuf,
  policy: Arc<SandboxPolicy>,
  /// Optional JSON schema for validating input parameters.
  parameters_schema: Option<Value>,
  backend: Arc<dyn SandboxBackend>,
  /// S1.2: filename → expected lowercase-hex sha256, the execution-time
  /// half of the skill's `[[scripts]]` integrity manifest (S1.1). `None`
  /// (the default) means no integrity enforcement is configured — the
  /// historical, permissive behaviour, used when a caller builds a
  /// `ScriptTool` outside the skill-manifest path. `Some(map)` — even an
  /// empty one — makes every execution fail-closed: a script whose name
  /// isn't a key in `map`, or whose content doesn't match the recorded
  /// hash, is refused. See docs/RFC_CODE_EXECUTION_TRUST.md.
  script_hashes: Option<HashMap<String, String>>,
  /// S2.3: absolute path to a `.py`-specific interpreter (a per-skill
  /// isolated venv's `python3`, built per docs/RFC_CODE_EXECUTION_TRUST.md
  /// S2). `None` (the default) means `.py` scripts run against the
  /// global `python3` on `PATH`, exactly as before this field existed.
  /// This only changes *which binary gets spawned* — the sandbox
  /// command-allowlist check still gates on the logical name `"python3"`
  /// regardless, so policy authoring is unaffected by whether a venv is
  /// configured.
  python_interpreter: Option<PathBuf>,
}

impl ScriptTool {
  pub fn new(scripts_dir: PathBuf, policy: Arc<SandboxPolicy>) -> Self {
    Self {
      scripts_dir,
      policy,
      parameters_schema: None,
      backend: Arc::new(NoopSandboxBackend::new(
        "ScriptTool default backend; opt in via with_os_sandbox()",
      )),
      script_hashes: None,
      python_interpreter: None,
    }
  }

  /// Convenience constructor with the default (restrictive) sandbox policy,
  /// pre-populated so the scripts directory itself is reachable.
  pub fn with_default_policy(scripts_dir: PathBuf) -> Self {
    let policy = SandboxPolicy {
      // Q1.2.1: empty `allowed_paths` is now "deny all", so we must seed
      // the scripts directory here or the tool can't even locate its
      // own scripts.
      allowed_paths: vec![scripts_dir.clone()],
      ..SandboxPolicy::default()
    };
    Self::new(scripts_dir, Arc::new(policy))
  }

  /// Sets the parameters schema for validation.
  pub fn with_parameters_schema(mut self, schema: Value) -> Self {
    self.parameters_schema = Some(schema);
    self
  }

  /// Configure execution-time integrity enforcement (S1.2): `hashes` maps
  /// script filename → expected lowercase-hex sha256. Once set, every
  /// `execute()` call is fail-closed — an unlisted script name, a missing
  /// file, or a content mismatch all refuse to run rather than warn.
  pub fn with_script_hashes(mut self, hashes: HashMap<String, String>) -> Self {
    self.script_hashes = Some(hashes);
    self
  }

  /// Configure a per-skill isolated Python interpreter (S2.3): `.py`
  /// scripts spawn `interpreter_path` instead of the global `python3`.
  /// `.sh`/`.js` scripts are unaffected.
  pub fn with_python_interpreter(mut self, interpreter_path: PathBuf) -> Self {
    self.python_interpreter = Some(interpreter_path);
    self
  }

  /// Wrap subsequent invocations in the platform's enforcing sandbox backend.
  /// On macOS this is `sandbox-exec`; on Linux this is a seccomp BPF filter.
  pub fn with_os_sandbox(mut self) -> Self {
    self.backend = crate::sandbox::default_backend();
    self
  }

  /// Inject a custom backend (e.g. for tests).
  pub fn with_backend(mut self, backend: Arc<dyn SandboxBackend>) -> Self {
    self.backend = backend;
    self
  }
}

#[async_trait]
impl Tool for ScriptTool {
  fn name(&self) -> &str {
    "script"
  }

  fn description(&self) -> &str {
    "Execute a script from the skill's scripts/ directory. \
        Pass the script filename and optional arguments as JSON. \
        Supported languages: Python (.py), Bash (.sh), JavaScript (.js)."
  }

  fn parameters_schema(&self) -> Value {
    self
      .parameters_schema
      .clone()
      .unwrap_or_else(default_script_parameters_schema)
  }

  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::script()
  }

  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    ToolIdempotency::NonIdempotent
  }

  fn sandbox_status(&self) -> Option<SandboxStatus> {
    Some(SandboxStatus::from_backend(self.backend.as_ref()))
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    // ── Schema validation ────────────────────────────────────────────────
    let schema = self.parameters_schema();
    let compiled_schema = jsonschema::JSONSchema::options()
      .compile(&schema)
      .map_err(|e| ToolError::InvalidParams {
        message: format!("Invalid script tool JSON schema: {}", e),
      })?;
    if let Err(errors) = compiled_schema.validate(&params) {
      let error_messages = errors.map(|error| error.to_string()).collect::<Vec<_>>();
      return Err(ToolError::InvalidParams {
        message: format!(
          "Parameters failed schema validation: {}",
          error_messages.join(", ")
        ),
      });
    }

    // ── Parameter extraction ─────────────────────────────────────────────
    let script_name = params["script"]
      .as_str()
      .ok_or_else(|| ToolError::InvalidParams {
        message: "Missing required parameter 'script'".to_string(),
      })?;

    // ── Path resolution + sandbox check ──────────────────────────────────
    // Reject any path traversal attempts (e.g. "../../../etc/passwd")
    if script_name.contains("..") || script_name.contains('/') || script_name.contains('\\') {
      return Err(ToolError::SandboxViolation {
        message: format!(
          "Script name '{}' must be a plain filename, not a path",
          script_name
        ),
      });
    }

    let script_path = self.scripts_dir.join(script_name);
    if !script_path.exists() {
      return Err(ToolError::ExecutionFailed {
        message: format!(
          "Script '{}' not found in scripts directory '{}'",
          script_name,
          self.scripts_dir.display()
        ),
      });
    }

    let canonical_scripts_dir =
      self
        .scripts_dir
        .canonicalize()
        .map_err(|e| ToolError::ExecutionFailed {
          message: format!(
            "Failed to canonicalize scripts directory '{}': {}",
            self.scripts_dir.display(),
            e
          ),
        })?;
    let canonical_script_path =
      script_path
        .canonicalize()
        .map_err(|e| ToolError::ExecutionFailed {
          message: format!("Failed to canonicalize script '{}': {}", script_name, e),
        })?;
    if !canonical_script_path.starts_with(&canonical_scripts_dir) {
      return Err(ToolError::SandboxViolation {
        message: format!(
          "Script '{}' resolves outside scripts directory '{}'",
          script_name,
          self.scripts_dir.display()
        ),
      });
    }

    if !self.policy.is_path_allowed(&canonical_script_path) {
      return Err(ToolError::SandboxViolation {
        message: format!(
          "Script '{}' is outside allowed path prefixes",
          canonical_script_path.display()
        ),
      });
    }

    // ── Integrity verification (S1.2) ──────────────────────────────────────
    // Only engages when the caller configured `script_hashes` (the skill
    // builder does this from the manifest's `[[scripts]]` list, S1.3).
    // Fail-closed: an unlisted or mismatched script never reaches spawn.
    if let Some(expected_hashes) = &self.script_hashes {
      let expected =
        expected_hashes
          .get(script_name)
          .ok_or_else(|| ToolError::SandboxViolation {
            message: format!(
              "Script '{}' is not listed in the skill's script integrity manifest",
              script_name
            ),
          })?;
      let bytes = tokio::fs::read(&canonical_script_path)
        .await
        .map_err(ToolError::IoError)?;
      let actual = sha256_hex(&bytes);
      if &actual != expected {
        return Err(ToolError::SandboxViolation {
          message: format!(
            "Script '{}' failed integrity verification (expected sha256 {}, found {}) \
             — refusing to execute a script whose content changed after install",
            script_name, expected, actual
          ),
        });
      }
      tracing::info!(
        event = "script_integrity_verified",
        script = script_name,
        sha256 = %actual,
        "Script integrity check passed"
      );
    }

    // ── Interpreter selection ────────────────────────────────────────────
    let ext = canonical_script_path
      .extension()
      .and_then(|e| e.to_str())
      .unwrap_or("");
    let interpreter = interpreter_for(ext).ok_or_else(|| ToolError::ExecutionFailed {
      message: format!(
        "Unsupported script extension '.{}'. Supported: .py, .sh, .js",
        ext
      ),
    })?;

    // Check that the interpreter is allowed by the sandbox policy. This is
    // always keyed on the *logical* command name ("python3"/"bash"/"node"),
    // never on a resolved venv path — S2.3's `python_interpreter` only
    // changes which physical binary gets spawned below, not the sandbox
    // policy question of whether that language is allowed to run at all.
    if !self.policy.is_command_allowed(interpreter) {
      return Err(ToolError::SandboxViolation {
        message: format!(
          "Interpreter '{}' is not in the allowed-commands list",
          interpreter
        ),
      });
    }

    // S2.3: `.py` scripts spawn the skill's own isolated venv interpreter
    // when one is configured, instead of the global `python3`.
    let spawn_interpreter = match (ext, &self.python_interpreter) {
      ("py", Some(venv_python)) => venv_python.as_os_str(),
      _ => std::ffi::OsStr::new(interpreter),
    };

    // S3.3: resolve the *real* (symlink-followed) install location of
    // whichever interpreter is about to be spawned. On macOS an
    // interpreter that lives outside the sandbox baseline (Homebrew,
    // pyenv, or a per-skill venv — whose `bin/python3` is itself
    // typically a symlink into a Homebrew Cellar path) fails to load its
    // own runtime under `os_sandbox: true` unless that real location is
    // explicitly granted read access — see docs/RFC_CODE_EXECUTION_TRUST.md
    // S3.3. Best-effort: `None` on failure just means no extra grant, same
    // as before this existed.
    let resolved_interpreter = resolve_interpreter_real_path(&spawn_interpreter.to_string_lossy());

    // ── Serialise args as JSON for stdin ─────────────────────────────────
    let stdin_json = match params.get("args") {
      None | Some(Value::Null) => String::new(),
      Some(value) => serde_json::to_string(value).unwrap_or_default(),
    };

    // ── Execution ────────────────────────────────────────────────────────
    let timeout = Duration::from_secs(self.policy.max_exec_time_secs);

    let mut cmd = tokio::process::Command::new(spawn_interpreter);
    cmd
      .arg(&canonical_script_path)
      .current_dir(&canonical_scripts_dir);

    let scope = build_script_scope(
      &canonical_scripts_dir,
      &self.policy,
      self.python_interpreter.as_deref(),
      resolved_interpreter.as_ref(),
    );
    let caps = self.requires_capabilities();
    self
      .backend
      .wrap_command(&mut cmd, &caps, &scope)
      .map_err(|err| ToolError::SandboxViolation {
        message: format!("OS sandbox preparation failed: {err}"),
      })?;
    super::shell::scrub_inherited_env(&mut cmd);

    // Stdio must be configured *after* `wrap_command`: an enforcing
    // backend (e.g. macOS `sandbox-exec`) may rebuild the underlying
    // `Command` wholesale to re-point it at a wrapper binary, which would
    // silently discard any stdio configuration set beforehand.
    cmd
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| ToolError::ExecutionFailed {
      message: format!("Failed to spawn '{}': {}", interpreter, e),
    })?;

    // Write args to stdin if present.
    if !stdin_json.is_empty()
      && let Some(mut stdin) = child.stdin.take()
    {
      use tokio::io::AsyncWriteExt;
      stdin
        .write_all(stdin_json.as_bytes())
        .await
        .map_err(ToolError::IoError)?;
      // stdin is dropped here, signalling EOF to the child.
    }

    let output = tokio::time::timeout(timeout, child.wait_with_output())
      .await
      .map_err(|_| ToolError::ExecutionFailed {
        message: format!(
          "Script '{}' timed out after {} seconds",
          script_name, self.policy.max_exec_time_secs
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
        "Script exited with code {}: {}",
        output.status.code().unwrap_or(-1),
        msg
      )))
    }
  }
}

fn default_script_parameters_schema() -> Value {
  json!({
      "type": "object",
      "additionalProperties": false,
      "properties": {
          "script": {
              "type": "string",
              "pattern": r"^[A-Za-z0-9._-]+\.(py|sh|js)$",
              "description": "Script filename (e.g. 'check_syntax.py'). Must be inside the skill scripts/ directory."
          },
          "args": {
              "description": "Optional arguments forwarded to the script as JSON on stdin. Can be any JSON value.",
              "default": null
          }
      },
      "required": ["script"]
  })
}

/// Build the OS-level sandbox scope for a script invocation.
///
/// The script directory is always read-allowed (the script and any sibling
/// resources it imports live there). When the policy declares additional
/// allowed paths we add them as both read- and write-targets so scripts can
/// produce outputs in skill-managed scratch dirs without escaping.
///
/// S2.3: when a per-skill venv interpreter is configured, its root
/// directory (`.venv/`, two levels up from `.venv/bin/python3`) is also
/// granted read+write — the interpreter needs to read its own standard
/// library / site-packages and may write `__pycache__` bytecode.
fn build_script_scope(
  scripts_dir: &std::path::Path,
  policy: &SandboxPolicy,
  python_interpreter: Option<&std::path::Path>,
  resolved_interpreter: Option<&ResolvedInterpreter>,
) -> SandboxScope {
  let mut scope = SandboxScope::new()
    .with_read_paths([scripts_dir.to_path_buf()])
    .with_working_directory(scripts_dir.to_path_buf());
  for path in &policy.allowed_paths {
    scope.read_paths.push(path.clone());
    scope.write_paths.push(path.clone());
  }
  if let Some(venv_root) = python_interpreter
    .and_then(|p| p.parent())
    .and_then(|p| p.parent())
  {
    scope.read_paths.push(venv_root.to_path_buf());
    scope.write_paths.push(venv_root.to_path_buf());
  }
  // S3.3: the interpreter's real (symlink-resolved) install prefix — e.g.
  // Homebrew's Cellar path a venv's `bin/python3` symlink ultimately points
  // to, or a Homebrew-installed `bash`/`node` — needs read access too, on
  // top of (not instead of) the venv directory itself: the venv's own
  // site-packages live under the venv root, while the interpreter's core
  // runtime (shared libraries, stdlib) lives under the real install prefix.
  if let Some(interpreter_prefix) = resolved_interpreter
    .and_then(|r| r.real.parent())
    .and_then(|p| p.parent())
  {
    scope.read_paths.push(interpreter_prefix.to_path_buf());
  }
  // S3.4: a package-manager-installed interpreter's own prefix isn't always
  // enough — Homebrew binaries link against *sibling* Cellar packages'
  // shared libraries through the `<prefix>/opt/<pkg>/...` symlink farm
  // (e.g. Homebrew bash needs `<prefix>/opt/readline/lib/libreadline.8.dylib`,
  // which lives under a completely different package's own prefix). When the
  // resolved path follows this layout, widen the read grant to the whole
  // package-manager root (still read-only) so dyld/ld.so can resolve
  // transitive runtime dependencies — see `package_manager_root`.
  if let Some(pm_root) = resolved_interpreter.and_then(|r| package_manager_root(&r.real)) {
    scope.read_paths.push(pm_root);
  }
  // S3.4: `command` as found on `PATH` may itself be a symlink into an
  // *ambient* venv (e.g. a shell with a venv activated so plain `python3`
  // resolves to `<venv>/bin/python3`, distinct from S2.3's explicit
  // `[dependencies]`-configured venv above) whose target is a completely
  // different, fully-canonicalized system install. Python's `site` module
  // looks for `pyvenv.cfg` next to the *original*, pre-canonicalize path,
  // not its resolved target — grant that original directory's parent too,
  // read-only, whenever it differs from the resolved target's own prefix.
  if let Some(ambient_venv_root) = resolved_interpreter.and_then(|r| {
    if r.original == r.real {
      None
    } else {
      r.original.parent().and_then(|p| p.parent())
    }
  }) {
    scope.read_paths.push(ambient_venv_root.to_path_buf());
  }
  if scope.write_paths.is_empty() {
    scope.write_paths.push(std::path::PathBuf::from("/tmp"));
  }
  scope.max_memory_bytes = policy.max_memory_bytes;
  scope.max_pids = policy.max_pids;
  scope.max_cpu_secs = policy.max_cpu_secs;
  scope
}

/// Detect a Homebrew/Linuxbrew-style package layout in `resolved_path`
/// (`<prefix>/Cellar/<pkg>/<version>/...`) and return `<prefix>` if found.
/// Package managers using this layout resolve a package's transitive shared
/// library dependencies through a separate `<prefix>/opt/<pkg>/...` symlink
/// farm rather than anything under the dependent package's own `Cellar`
/// subtree, so granting only the interpreter's own resolved prefix (as S3.3
/// originally did) misses them entirely. `None` for any other layout (plain
/// system binaries, pyenv, a skill's own venv, ...) — those already get
/// read access via the venv-root / interpreter-prefix grants above.
fn package_manager_root(resolved_path: &std::path::Path) -> Option<std::path::PathBuf> {
  let mut prefix = std::path::PathBuf::new();
  for component in resolved_path.components() {
    if component.as_os_str() == "Cellar" {
      return Some(prefix);
    }
    prefix.push(component);
  }
  None
}

/// The interpreter path as found on `PATH` (or given verbatim), alongside
/// its fully symlink-resolved target. Kept separate (S3.4) because they
/// answer different sandbox-grant questions: `original` may itself be a
/// venv shim whose *own* directory holds files (`pyvenv.cfg`) the
/// interpreter needs read access to, while `real` is the actual install
/// prefix whose runtime files (shared libs, stdlib) need it.
struct ResolvedInterpreter {
  original: std::path::PathBuf,
  real: std::path::PathBuf,
}

/// Resolve `command` — a bare name (`"python3"`, `"bash"`, `"node"`) or an
/// already-concrete path (a venv's interpreter) — to its real,
/// symlink-followed absolute path: the actual install location whose
/// contents the interpreter needs to read at startup (S3.3). Best-effort;
/// `None` if it can't be found or resolved, same as before this existed.
fn resolve_interpreter_real_path(command: &str) -> Option<ResolvedInterpreter> {
  let original = if command.contains(std::path::MAIN_SEPARATOR) {
    std::path::PathBuf::from(command)
  } else {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
      .map(|dir| dir.join(command))
      .find(|candidate| candidate.is_file())?
  };
  let real = original.canonicalize().ok()?;
  Some(ResolvedInterpreter { original, real })
}

/// Lowercase hex-encoded SHA-256 of `bytes` (S1.2 integrity check).
fn sha256_hex(bytes: &[u8]) -> String {
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  format!("{:x}", hasher.finalize())
}

/// Map a file extension to a known interpreter binary name.
fn interpreter_for(ext: &str) -> Option<&'static str> {
  match ext {
    "py" => Some("python3"),
    "sh" => Some("bash"),
    "js" => Some("node"),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;
  use tempfile::TempDir;

  fn make_tool(dir: &std::path::Path) -> ScriptTool {
    let policy = SandboxPolicy {
      allowed_commands: vec![
        "python3".to_string(),
        "bash".to_string(),
        "node".to_string(),
      ],
      // Q1.2.1: scripts_dir must be in allowed_paths now that an empty
      // allow-list means "deny all" instead of "allow all".
      allowed_paths: vec![dir.to_path_buf()],
      ..Default::default()
    };
    ScriptTool::new(dir.to_path_buf(), Arc::new(policy))
  }

  #[tokio::test]
  async fn executes_bash_script() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("hello.sh");
    let mut f = std::fs::File::create(&script).unwrap();
    writeln!(f, "#!/bin/bash\necho 'hello from script'").unwrap();

    let tool = make_tool(dir.path());
    let result = tool.execute(json!({"script": "hello.sh"})).await.unwrap();
    assert!(result.content.contains("hello from script"));
  }

  #[tokio::test]
  async fn rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(dir.path());
    let result = tool.execute(json!({"script": "../etc/passwd"})).await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[tokio::test]
  async fn rejects_unknown_extension() {
    let dir = TempDir::new().unwrap();
    // Create a dummy .rb file
    std::fs::File::create(dir.path().join("run.rb")).unwrap();
    let tool = make_tool(dir.path());
    let result = tool.execute(json!({"script": "run.rb"})).await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[tokio::test]
  async fn rejects_extra_top_level_params_by_default() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("hello.sh");
    std::fs::write(&script, "echo ok").unwrap();
    let tool = make_tool(dir.path());

    let result = tool
      .execute(json!({"script": "hello.sh", "unexpected": true}))
      .await;

    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[tokio::test]
  async fn custom_schema_validation_rejects_bad_args() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("hello.sh");
    std::fs::write(&script, "echo ok").unwrap();
    let tool = make_tool(dir.path()).with_parameters_schema(json!({
      "type": "object",
      "required": ["script", "args"],
      "properties": {
        "script": {"type": "string"},
        "args": {
          "type": "object",
          "required": ["count"],
          "properties": {"count": {"type": "integer"}}
        }
      }
    }));

    let result = tool
      .execute(json!({"script": "hello.sh", "args": {"count": "bad"}}))
      .await;

    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn rejects_symlink_that_escapes_scripts_dir() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("escape.sh");
    std::fs::write(&target, "echo escaped").unwrap();
    symlink(&target, dir.path().join("escape.sh")).unwrap();
    let tool = make_tool(dir.path());

    let result = tool.execute(json!({"script": "escape.sh"})).await;

    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
  }

  // ── S1.2: execute-time integrity verification ──────────────────────────

  #[tokio::test]
  async fn executes_when_hash_matches() {
    let dir = TempDir::new().unwrap();
    let content = "#!/bin/bash\necho hello from script";
    let script = dir.path().join("hello.sh");
    std::fs::write(&script, content).unwrap();

    let tool = make_tool(dir.path()).with_script_hashes(HashMap::from([(
      "hello.sh".to_string(),
      sha256_hex(content.as_bytes()),
    )]));

    let result = tool.execute(json!({"script": "hello.sh"})).await.unwrap();
    assert!(result.content.contains("hello from script"));
  }

  /// S1.2 regression: a script whose content was modified after its hash
  /// was recorded must be refused, not silently executed.
  #[tokio::test]
  async fn rejects_tampered_script_content() {
    let dir = TempDir::new().unwrap();
    let original = "#!/bin/bash\necho original";
    let script = dir.path().join("hello.sh");
    std::fs::write(&script, original).unwrap();

    let tool = make_tool(dir.path()).with_script_hashes(HashMap::from([(
      "hello.sh".to_string(),
      sha256_hex(original.as_bytes()),
    )]));

    // Tamper with the file after the hash was recorded.
    std::fs::write(&script, "#!/bin/bash\necho tampered").unwrap();

    let result = tool.execute(json!({"script": "hello.sh"})).await;
    assert!(
      matches!(result, Err(ToolError::SandboxViolation { .. })),
      "expected a SandboxViolation for tampered content, got {result:?}"
    );
  }

  /// A script that exists on disk but was never listed in `script_hashes`
  /// must also be refused — fail-closed on omission, not just mismatch.
  #[tokio::test]
  async fn rejects_unlisted_script_when_hashes_configured() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("hello.sh"), "echo ok").unwrap();
    std::fs::write(dir.path().join("other.sh"), "echo other").unwrap();

    // Only "other.sh" is listed — "hello.sh" is not, even though it exists.
    let tool = make_tool(dir.path()).with_script_hashes(HashMap::from([(
      "other.sh".to_string(),
      sha256_hex(b"echo other"),
    )]));

    let result = tool.execute(json!({"script": "hello.sh"})).await;
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
  }

  /// Without `with_script_hashes`, behaviour is unchanged (back-compat).
  #[tokio::test]
  async fn executes_without_integrity_check_when_hashes_not_configured() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("hello.sh"), "echo ok").unwrap();

    let tool = make_tool(dir.path());
    let result = tool.execute(json!({"script": "hello.sh"})).await;
    assert!(result.is_ok());
  }

  // ── S2.3: per-skill python interpreter ──────────────────────────────────

  /// `.py` scripts must spawn the configured venv interpreter, not the
  /// global `python3` — proven with a stand-in executable so the test
  /// doesn't depend on a real venv existing.
  #[cfg(unix)]
  #[tokio::test]
  async fn python_scripts_spawn_the_configured_interpreter() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("run.py"), "print('should not run')").unwrap();

    let fake_interpreter = dir.path().join("fake_python3");
    std::fs::write(
      &fake_interpreter,
      "#!/bin/bash\necho from-venv-interpreter\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake_interpreter).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_interpreter, perms).unwrap();

    let tool = make_tool(dir.path()).with_python_interpreter(fake_interpreter);
    let result = tool.execute(json!({"script": "run.py"})).await.unwrap();

    assert!(result.content.contains("from-venv-interpreter"));
  }

  /// `.sh`/`.js` scripts are unaffected by a configured python interpreter.
  #[cfg(unix)]
  #[tokio::test]
  async fn python_interpreter_override_does_not_affect_other_extensions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("run.sh"), "#!/bin/bash\necho real-bash").unwrap();

    let fake_interpreter = dir.path().join("fake_python3");
    std::fs::write(
      &fake_interpreter,
      "#!/bin/bash\necho from-venv-interpreter\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake_interpreter).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_interpreter, perms).unwrap();

    let tool = make_tool(dir.path()).with_python_interpreter(fake_interpreter);
    let result = tool.execute(json!({"script": "run.sh"})).await.unwrap();

    assert!(result.content.contains("real-bash"));
  }

  /// Without `with_python_interpreter`, `.py` scripts are still gated by
  /// the sandbox policy's `"python3"` allow-list entry exactly as before
  /// this field existed — a policy that never allowed python3 still
  /// refuses `.py` execution regardless of a venv being configured.
  #[tokio::test]
  async fn without_python_interpreter_override_py_scripts_still_need_python3_allowed() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("run.py"), "print('hi')").unwrap();
    let policy = SandboxPolicy {
      allowed_commands: vec!["bash".to_string(), "node".to_string()], // no python3
      allowed_paths: vec![dir.path().to_path_buf()],
      ..Default::default()
    };
    let tool = ScriptTool::new(dir.path().to_path_buf(), Arc::new(policy));

    let result = tool.execute(json!({"script": "run.py"})).await;
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
  }
}
