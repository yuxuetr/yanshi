//! `code_exec`: run LLM-generated Python inside a strongly-isolated
//! [`ContainerBackend`](crate::sandbox::container::ContainerBackend) (S4.2, `docs/RFC_LLM_CODE_EXECUTION.md`).
//!
//! Unlike [`crate::builtin::ScriptTool`] (which only ever runs author-signed,
//! hash-pinned files under a skill's `scripts/` directory), `code_exec` runs
//! content the model wrote *this turn* — never vetted by anyone. Per the
//! RFC's five binding design constraints, this tool:
//!
//! 1. Is wholly independent of `ScriptTool`'s trust model — no shared
//!    `SandboxPolicy`, no shared execution boundary.
//! 2. Gives every call a fresh, ephemeral working directory
//!    ([`tempfile::TempDir`]) torn down when `execute()` returns.
//! 3. Never grants network access — hardcoded, not policy-configurable —
//!    until the RFC's egress allowlist proxy exists.
//! 4. Returns results only through [`ToolOutput`] — the model gets no
//!    ambient filesystem access back to whatever the code produced beyond
//!    stdout/stderr.
//! 5. Registers as [`ToolIdempotency::NonIdempotent`] so
//!    `yanshi-harness`'s production-profile approval escalation applies
//!    automatically.
//!
//! Resource limits (memory / CPU-seconds / process count) are hardcoded
//! rather than manifest-configurable in this v1 — see the constants below
//! for the chosen values and rationale.
//!
//! **Mandatory strong isolation**: unlike `ScriptTool`/`ShellTool`'s opt-in
//! `os_sandbox`, `code_exec` refuses to run at all
//! ([`ToolError::SandboxViolation`]) when [`ContainerBackend::is_enforcing`](crate::sandbox::container::ContainerBackend::is_enforcing)
//! is false — there is no safe degraded mode for content that was never
//! vetted (see `sandbox::container`'s module docs).

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::sandbox::{SandboxBackend, SandboxScope, SandboxStatus, code_exec_backend};
use crate::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput};

/// Resident memory cap. Must stay at or above
/// [`crate::sandbox::container`]'s empirically-verified 200 MiB VM floor
/// (each container invocation is a full microVM on the Apple `container`
/// engine, not a lightweight cgroup) — 256 MiB leaves headroom above that
/// floor while still catching a script that allocates hundreds of MiB more
/// than any reasonable data-munging task needs.
const MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
/// Cumulative CPU-seconds before the container is killed (`RLIMIT_CPU`-exact
/// semantics via `--ulimit cpu=`, see `sandbox::container`'s module docs).
/// Generous enough for real computation, tight enough to catch a runaway
/// loop rather than let it run indefinitely.
const MAX_CPU_SECS: u64 = 30;
/// Process-count cap (`--ulimit nproc=` / Podman `--pids-limit`) — the
/// primary defense against a fork bomb. 32 comfortably covers a Python
/// process needing a handful of helper threads/subprocesses without leaving
/// room for a bomb to make meaningful progress.
const MAX_PIDS: u32 = 32;
/// Wall-clock spawn timeout — the backstop for a process that mostly
/// sleeps/blocks rather than burns CPU (e.g. waiting on a blocked network
/// call), which the CPU-seconds ulimit alone wouldn't catch. Deliberately
/// **not** equal to [`MAX_CPU_SECS`]: empirically (this session, a genuine
/// CPU-bound busy loop under `--ulimit cpu=30:30`) container boot overhead
/// and vCPU scheduling jitter meant accumulating 30 CPU-seconds took
/// slightly *more* than 30 wall-clock seconds, so an equal outer timeout
/// raced with — and sometimes beat — the inner ulimit, surfacing as a
/// generic "timed out" error instead of the ulimit's own kill. This buffer
/// gives the ulimit room to fire first, so its distinct failure mode stays
/// observable.
const SPAWN_TIMEOUT_SECS: u64 = MAX_CPU_SECS + 15;
/// Cap on captured stdout/stderr, independent of [`MAX_MEMORY_BYTES`]
/// above — that bounds the *guest* container's memory, not this host
/// process's own buffers. A payload that writes continuously (not
/// sleeping, so the wall-clock timeout backstop is what would eventually
/// catch it, not immediately) could otherwise grow an unbounded `Vec` on
/// the host before any other limit fires — found via adversarial code
/// review this session. Matches the `yanshi-harness::tasks`
/// `DEFAULT_MAX_OUTPUT_BYTES` convention for the same class of problem.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct CodeExecTool {
  backend: std::sync::Arc<dyn SandboxBackend>,
}

impl CodeExecTool {
  pub fn new() -> Self {
    Self {
      backend: std::sync::Arc::new(code_exec_backend()),
    }
  }

  /// Inject a custom backend (tests only — production always uses
  /// [`code_exec_backend`]).
  pub fn with_backend(mut self, backend: std::sync::Arc<dyn SandboxBackend>) -> Self {
    self.backend = backend;
    self
  }
}

impl Default for CodeExecTool {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl Tool for CodeExecTool {
  fn name(&self) -> &str {
    "code_exec"
  }

  fn description(&self) -> &str {
    "Run Python code in a strongly-isolated, ephemeral sandbox (no network access, no \
     access to any files outside this single call). Use for computation, data \
     transformation, or validating output — not for interacting with the skill's own \
     files or the network (use the file/http tools for those)."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "code": {
          "type": "string",
          "description": "Python source code to run. Runs in its own throwaway directory with no arguments and no stdin; print() whatever result you need back."
        }
      },
      "required": ["code"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin_named("code_exec")
  }

  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    ToolIdempotency::NonIdempotent
  }

  fn sandbox_status(&self) -> Option<SandboxStatus> {
    Some(SandboxStatus::from_backend(self.backend.as_ref()))
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    // ── Mandatory strong isolation ────────────────────────────────────────
    // Checked explicitly, not just trusted to `wrap_command`'s error path:
    // `NoopSandboxBackend::wrap_command` succeeds and does nothing by
    // design (its contract puts the "refuse if not enforcing" decision on
    // the caller, e.g. `ShellTool`'s opt-in `os_sandbox`) — a caller that
    // injected it here (or any other non-enforcing backend) must never
    // silently run llm-generated code unsandboxed on the host.
    if !self.backend.is_enforcing() {
      return Err(ToolError::SandboxViolation {
        message: format!(
          "code_exec requires an enforcing isolation backend (got '{}', not enforcing) and \
           refuses to run unsandboxed",
          self.backend.name()
        ),
      });
    }

    // ── Schema validation ────────────────────────────────────────────────
    let schema = self.parameters_schema();
    let compiled_schema = jsonschema::JSONSchema::options()
      .compile(&schema)
      .map_err(|e| ToolError::InvalidParams {
        message: format!("Invalid code_exec tool JSON schema: {}", e),
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
    let code = params["code"]
      .as_str()
      .ok_or_else(|| ToolError::InvalidParams {
        message: "Missing required parameter 'code'".to_string(),
      })?;

    // ── Ephemeral workdir (RFC constraint #2) ────────────────────────────
    // Distinguishable prefix so tests can identify exactly this call's
    // workdir among unrelated entries transient system temp-dir churn
    // (e.g. the container engine's own bookkeeping files) may also create.
    let workdir = tempfile::Builder::new()
      .prefix("yanshi-code-exec-")
      .tempdir()
      .map_err(ToolError::IoError)?;
    let script_path = workdir.path().join("main.py");
    tokio::fs::write(&script_path, code)
      .await
      .map_err(ToolError::IoError)?;
    let canonical_workdir = workdir.path().canonicalize().map_err(ToolError::IoError)?;

    // ── Sandbox scope + spawn ─────────────────────────────────────────────
    // Stable name so `SandboxBackend::terminate` can address this specific
    // container later — see that call below for why this is required, not
    // optional (a killed `container`/`podman` *client* process does not
    // stop the container it launched; confirmed empirically, S4.2
    // follow-up).
    let container_name = format!(
      "yanshi-code-exec-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
    );
    let scope = SandboxScope::new()
      .with_read_paths([canonical_workdir.clone()])
      .with_write_paths([canonical_workdir.clone()])
      .with_working_directory(canonical_workdir)
      .with_max_memory_bytes(MAX_MEMORY_BYTES)
      .with_max_cpu_secs(MAX_CPU_SECS)
      .with_max_pids(MAX_PIDS)
      .with_container_name(container_name);
    let caps = self.requires_capabilities();

    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(Path::new("main.py")).current_dir(workdir.path());

    self
      .backend
      .wrap_command(&mut cmd, &caps, &scope)
      .map_err(|err| ToolError::SandboxViolation {
        message: format!(
          "code_exec requires a working isolation backend and refuses to run without one: {err}"
        ),
      })?;

    // Stdio must be configured *after* `wrap_command` — see the trait's
    // caller contract (`SandboxBackend::wrap_command` doc comment).
    // `kill_on_drop` is defense in depth for the client process itself;
    // it does NOT stop the container (see `TerminateOnDrop` below).
    cmd
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| ToolError::ExecutionFailed {
      message: format!("Failed to spawn code_exec container: {e}"),
    })?;
    // `.stdout(Stdio::piped())`/`.stderr(Stdio::piped())` above guarantee
    // `Child::stdout`/`stderr` are `Some` on a successful spawn — this is a
    // build-time invariant of `std::process::Command`, not a runtime risk on
    // untrusted input (the code being sandboxed never touches this).
    #[allow(
      clippy::expect_used,
      reason = "stdio configured as piped a few lines above; Some is guaranteed on successful spawn"
    )]
    let mut stdout_pipe = child.stdout.take().expect("stdout was configured as piped");
    #[allow(
      clippy::expect_used,
      reason = "stdio configured as piped a few lines above; Some is guaranteed on successful spawn"
    )]
    let mut stderr_pipe = child.stderr.take().expect("stderr was configured as piped");

    // Guarantees `self.backend.terminate(&scope)` runs on every exit path
    // from here on *except* a clean `wait_with_output()` completion (armed
    // = false below) — a container that actually finished is already torn
    // down by its own `--rm`. Every other path (timeout, an I/O error
    // losing track of the child, a future early return this function
    // doesn't have yet) leaves the guard armed, so `Drop` fires it: without
    // this, a code_exec call whose payload merely sleeps/blocks (burning
    // no CPU, so RLIMIT_CPU never fires, and no memory, so the memory cap
    // never fires) would outlive this function's own timeout indefinitely,
    // orphaning a live container/VM on the host — confirmed as a real gap
    // via adversarial code review this session, not hypothetical.
    struct TerminateOnDrop<'a> {
      backend: &'a dyn SandboxBackend,
      scope: &'a SandboxScope,
      armed: bool,
    }
    impl Drop for TerminateOnDrop<'_> {
      fn drop(&mut self) {
        if self.armed {
          self.backend.terminate(self.scope);
        }
      }
    }
    let mut cleanup_guard = TerminateOnDrop {
      backend: self.backend.as_ref(),
      scope: &scope,
      armed: true,
    };

    // Bounded, concurrent reads (not `wait_with_output()`, which
    // accumulates each stream into an unbounded `Vec`) — see
    // `MAX_OUTPUT_BYTES` and `read_capped`. `read_capped` drains past the
    // cap rather than stopping, so a chatty child never blocks on a full
    // pipe waiting for a reader that gave up on it; `child.wait()` runs
    // concurrently with both via `tokio::join!`, not after, for the same
    // reason.
    let wait_result =
      tokio::time::timeout(std::time::Duration::from_secs(SPAWN_TIMEOUT_SECS), async {
        let (stdout_bytes, stderr_bytes, status) = tokio::join!(
          read_capped(&mut stdout_pipe, MAX_OUTPUT_BYTES),
          read_capped(&mut stderr_pipe, MAX_OUTPUT_BYTES),
          child.wait(),
        );
        status.map(|status| (stdout_bytes, stderr_bytes, status))
      })
      .await;
    let (stdout_bytes, stderr_bytes, status) = match wait_result {
      Ok(Ok(triple)) => {
        cleanup_guard.armed = false;
        triple
      }
      Ok(Err(io_err)) => return Err(ToolError::IoError(io_err)),
      Err(_) => {
        return Err(ToolError::ExecutionFailed {
          message: format!("code_exec timed out after {SPAWN_TIMEOUT_SECS} seconds"),
        });
      }
    };

    // `workdir` is dropped (and its temp directory removed) once this
    // function returns — nothing here needs to reach back into it after
    // this point (RFC constraint #4: results only via `ToolOutput`).
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

    if status.success() {
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
        "code_exec exited with code {}: {}",
        status.code().unwrap_or(-1),
        msg
      )))
    }
  }
}

/// Read `reader` to EOF, keeping only the first `cap` bytes — see
/// [`MAX_OUTPUT_BYTES`] for why this exists instead of the unbounded
/// accumulation `Child::wait_with_output()` would otherwise do. Drains
/// past the cap rather than stopping once it's reached: a child whose
/// pipe fills up because nothing is reading from it anymore blocks on the
/// next write, which would otherwise turn "cap the output" into "hang the
/// container until the wall-clock timeout," defeating the point of a
/// resource limit.
async fn read_capped(mut reader: impl tokio::io::AsyncRead + Unpin, cap: usize) -> Vec<u8> {
  use tokio::io::AsyncReadExt;
  let mut buf = Vec::with_capacity(cap.min(8192));
  let mut chunk = [0u8; 8192];
  loop {
    match reader.read(&mut chunk).await {
      Ok(0) | Err(_) => break,
      Ok(n) => {
        if buf.len() < cap {
          let take = (cap - buf.len()).min(n);
          buf.extend_from_slice(&chunk[..take]);
        }
      }
    }
  }
  buf
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::sandbox::NoopSandboxBackend;

  #[tokio::test]
  async fn refuses_to_run_without_an_enforcing_backend() {
    let tool = CodeExecTool::new().with_backend(std::sync::Arc::new(NoopSandboxBackend::new(
      "test: no backend",
    )));
    let result = tool.execute(json!({"code": "print('hi')"})).await;
    assert!(
      matches!(result, Err(ToolError::SandboxViolation { .. })),
      "expected SandboxViolation, got {result:?}"
    );
  }

  #[tokio::test]
  async fn rejects_missing_code_parameter() {
    let tool = CodeExecTool::new();
    let result = tool.execute(json!({})).await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[tokio::test]
  async fn rejects_unknown_parameters() {
    let tool = CodeExecTool::new();
    let result = tool
      .execute(json!({"code": "print(1)", "extra": true}))
      .await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
  }

  #[test]
  fn metadata_declares_nonidempotent_and_process_exec() {
    let tool = CodeExecTool::new();
    let metadata = tool.metadata();
    assert_eq!(metadata.idempotency, ToolIdempotency::NonIdempotent);
    assert!(
      metadata
        .permissions
        .allows(&crate::ToolPermission::ProcessExec)
    );
  }
}
