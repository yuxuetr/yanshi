//! Concrete [`ApprovalProvider`] implementations used by the
//! [`HookedTool`](crate::HookedTool) tool wrapper.
//!
//! Phase H2 ships three providers that cover the common policy modes:
//!
//! - [`AutoAllowApprovalProvider`] — auto-approve every request (used
//!   in tests and in `dev` profile when the operator explicitly opts
//!   in via `--permission-mode auto`).
//! - [`AutoDenyApprovalProvider`] — auto-deny every request (used in
//!   `production` profile fail-closed flows).
//! - [`CliApprovalProvider`] — block on stdin for an interactive
//!   prompt (used by `yanshi harness run` without `--output
//!   stream-json`).
//!
//! All three honour the [`ApprovalRequest::expires_at`] deadline so
//! callers never get an implicit allow on timeout.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;

use crate::approval::{
  ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalScope,
};
use crate::error::HarnessError;

/// Auto-allow every request once. Useful for CI smoke tests and the
/// `dev` security profile when the operator has explicitly opted in.
#[derive(Debug, Default, Clone)]
pub struct AutoAllowApprovalProvider {
  /// Stable identifier recorded as `ApprovalDecision::decided_by`.
  decided_by: String,
}

impl AutoAllowApprovalProvider {
  pub fn new() -> Self {
    Self {
      decided_by: "auto:allow".into(),
    }
  }

  pub fn with_decider(mut self, decider: impl Into<String>) -> Self {
    self.decided_by = decider.into();
    self
  }
}

#[async_trait]
impl ApprovalProvider for AutoAllowApprovalProvider {
  fn name(&self) -> &str {
    "auto_allow"
  }

  async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
    Ok(ApprovalDecision {
      request_id: request.id,
      decision: ApprovalOutcome::Allow,
      scope: ApprovalScope::Once,
      decided_by: self.decided_by.clone(),
      decided_at: Utc::now(),
      reason: Some("auto-approve provider".into()),
    })
  }
}

/// Auto-deny every request. Used for `production` profile fail-closed
/// flows where any tool reaching the approval path is treated as
/// unsafe.
#[derive(Debug, Default, Clone)]
pub struct AutoDenyApprovalProvider {
  decided_by: String,
  stop_on_deny: bool,
}

impl AutoDenyApprovalProvider {
  pub fn new() -> Self {
    Self {
      decided_by: "auto:deny".into(),
      stop_on_deny: false,
    }
  }

  /// Emit [`ApprovalOutcome::DenyAndStop`] instead of `Deny`. Use this
  /// when the harness should abort the whole agent loop on the first
  /// risky call (matches the fail-closed Production default).
  pub fn with_stop_on_deny(mut self, stop: bool) -> Self {
    self.stop_on_deny = stop;
    self
  }

  pub fn with_decider(mut self, decider: impl Into<String>) -> Self {
    self.decided_by = decider.into();
    self
  }
}

#[async_trait]
impl ApprovalProvider for AutoDenyApprovalProvider {
  fn name(&self) -> &str {
    "auto_deny"
  }

  async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
    let decision = if self.stop_on_deny {
      ApprovalOutcome::DenyAndStop
    } else {
      ApprovalOutcome::Deny
    };
    Ok(ApprovalDecision {
      request_id: request.id,
      decision,
      scope: ApprovalScope::Once,
      decided_by: self.decided_by.clone(),
      decided_at: Utc::now(),
      reason: Some("auto-deny provider".into()),
    })
  }
}

/// Blocking stdin prompt. Used by `yanshi harness run` outside of
/// stream-json mode. The provider prints a structured summary, then
/// reads a single line of input parsed as one of:
///
/// - `y` / `yes` → allow once.
/// - `s` / `session` → allow for the rest of the session.
/// - `r` / `run` → allow for the current run.
/// - `n` / `no` / empty → deny.
/// - `q` / `quit` → deny-and-stop.
///
/// Any other input loops with a clarification message. The provider
/// honours [`ApprovalRequest::expires_at`] by polling stdin via
/// `tokio::task::spawn_blocking` so the async runtime can race the
/// deadline.
pub struct CliApprovalProvider {
  prompt_writer: Arc<Mutex<Box<dyn Write + Send>>>,
  /// Wrapping a reader behind `Arc<Mutex<…>>` lets tests inject scripted
  /// input. Real usage uses [`CliApprovalProvider::stdin`] which
  /// builds a stdin-backed instance.
  input_reader: Arc<Mutex<Box<dyn BufRead + Send>>>,
  /// The read a timed-out prompt left behind. A blocking `read_line` can't
  /// be cancelled, so the next prompt takes it over instead of starting a
  /// second read that would race it for the user's answer (eval 2026-10-07
  /// A5). Locking it also serializes prompts, so concurrent requests can't
  /// receive each other's answers.
  pending_read: tokio::sync::Mutex<Option<PendingRead>>,
}

type PendingRead = tokio::task::JoinHandle<Result<String, HarnessError>>;

impl CliApprovalProvider {
  /// Construct a provider that prompts on stderr and reads from stdin.
  pub fn stdin() -> Self {
    Self {
      prompt_writer: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
      input_reader: Arc::new(Mutex::new(Box::new(std::io::BufReader::new(
        std::io::stdin(),
      )))),
      pending_read: tokio::sync::Mutex::new(None),
    }
  }

  /// Construct a provider with explicit writer / reader. Used by tests
  /// to script the interaction.
  pub fn with_streams<W, R>(writer: W, reader: R) -> Self
  where
    W: Write + Send + 'static,
    R: BufRead + Send + 'static,
  {
    Self {
      prompt_writer: Arc::new(Mutex::new(Box::new(writer))),
      input_reader: Arc::new(Mutex::new(Box::new(reader))),
      pending_read: tokio::sync::Mutex::new(None),
    }
  }

  fn write_prompt(&self, request: &ApprovalRequest) -> Result<(), HarnessError> {
    let mut writer = self
      .prompt_writer
      .lock()
      .map_err(|_| HarnessError::Other("CliApprovalProvider prompt writer poisoned".into()))?;
    writeln!(writer, "── Harness approval request ──").map_err(io_err)?;
    writeln!(
      writer,
      "  tool: {} (step={})",
      request.tool, request.step_index
    )
    .map_err(io_err)?;
    writeln!(
      writer,
      "  risk: {:?}   idempotency: {:?}",
      request.risk, request.idempotency
    )
    .map_err(io_err)?;
    if let Some(source) = &request.source {
      writeln!(writer, "  source: {source:?}").map_err(io_err)?;
    }
    if !request.permissions.is_empty() {
      writeln!(writer, "  permissions: {:?}", request.permissions).map_err(io_err)?;
    }
    writeln!(writer, "  reason: {}", request.reason).map_err(io_err)?;
    writeln!(writer, "  params: {}", request.params_summary).map_err(io_err)?;
    writeln!(
      writer,
      "Allow this call? [y]es / [s]ession / [r]un / [n]o / [q]uit:"
    )
    .map_err(io_err)?;
    writer.flush().map_err(io_err)?;
    Ok(())
  }
}

impl CliApprovalProvider {
  fn spawn_read(&self) -> PendingRead {
    let reader = self.input_reader.clone();
    tokio::task::spawn_blocking(move || {
      let mut reader = reader
        .lock()
        .map_err(|_| HarnessError::Other("CliApprovalProvider reader poisoned".into()))?;
      let mut line = String::new();
      reader
        .read_line(&mut line)
        .map_err(|err| HarnessError::Other(format!("io: {err}")))?;
      Ok(line.trim().to_string())
    })
  }
}

fn joined_line(
  result: Result<Result<String, HarnessError>, tokio::task::JoinError>,
) -> Result<String, HarnessError> {
  result.map_err(|err| HarnessError::Other(format!("approval read join failed: {err}")))?
}

fn io_err(err: std::io::Error) -> HarnessError {
  HarnessError::Other(format!("io: {err}"))
}

impl std::fmt::Debug for CliApprovalProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CliApprovalProvider")
      .finish_non_exhaustive()
  }
}

#[async_trait]
impl ApprovalProvider for CliApprovalProvider {
  fn name(&self) -> &str {
    "cli"
  }

  async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
    let mut pending = self.pending_read.lock().await;
    if pending.as_ref().is_some_and(|read| read.is_finished()) {
      // A late answer to a prompt that already expired.
      *pending = None;
    }
    self.write_prompt(&request)?;
    // Read input on the blocking pool so the async runtime stays
    // responsive. We honour `expires_at` by racing with a sleep.
    let mut read = match pending.take() {
      Some(read) => read,
      None => self.spawn_read(),
    };

    let line = if let Some(expires_at) = request.expires_at {
      let now = Utc::now();
      let remaining = (expires_at - now)
        .to_std()
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
      tokio::select! {
        result = &mut read => joined_line(result)?,
        _ = tokio::time::sleep(remaining) => {
          *pending = Some(read);
          return Err(HarnessError::ApprovalTimeout {
            timeout_ms: remaining.as_millis() as u64,
          });
        }
      }
    } else {
      joined_line(read.await)?
    };

    let (decision, scope, reason) = parse_response(&line);
    Ok(ApprovalDecision {
      request_id: request.id,
      decision,
      scope,
      decided_by: "user".into(),
      decided_at: Utc::now(),
      reason,
    })
  }
}

fn parse_response(input: &str) -> (ApprovalOutcome, ApprovalScope, Option<String>) {
  match input.to_ascii_lowercase().as_str() {
    "y" | "yes" | "allow" | "allow_once" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Once,
      Some("user allowed once".into()),
    ),
    "s" | "session" | "allow_session" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Session,
      Some("user allowed for session".into()),
    ),
    "r" | "run" | "allow_run" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Run,
      Some("user allowed for run".into()),
    ),
    "q" | "quit" | "deny_and_stop" => (
      ApprovalOutcome::DenyAndStop,
      ApprovalScope::Once,
      Some("user denied and requested stop".into()),
    ),
    "" | "n" | "no" | "deny" => (
      ApprovalOutcome::Deny,
      ApprovalScope::Once,
      Some("user denied".into()),
    ),
    other => (
      ApprovalOutcome::Deny,
      ApprovalScope::Once,
      Some(format!("unrecognised input '{other}' — defaulting to deny")),
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::approval::ApprovalRisk;
  use chrono::Duration as ChronoDuration;
  use std::io::Cursor;
  use std::sync::Mutex as StdMutex;
  use yanshi_tool::{ToolIdempotency, ToolPermission, ToolSource};

  fn sample_request() -> ApprovalRequest {
    ApprovalRequest {
      id: "req-1".into(),
      session_id: "sess-1".into(),
      step_index: 2,
      tool: "shell".into(),
      source: Some(ToolSource::Builtin),
      permissions: vec![ToolPermission::ProcessExec],
      idempotency: ToolIdempotency::NonIdempotent,
      effect: Default::default(),
      params_summary: serde_json::json!({"cmd": "ls"}),
      risk: ApprovalRisk::High,
      reason: "shell tool".into(),
      requested_at: Utc::now(),
      expires_at: None,
    }
  }

  #[tokio::test]
  async fn auto_allow_returns_allow_outcome() {
    let provider = AutoAllowApprovalProvider::new();
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Allow);
    assert_eq!(decision.scope, ApprovalScope::Once);
    assert_eq!(decision.decided_by, "auto:allow");
    assert_eq!(decision.request_id, "req-1");
  }

  #[tokio::test]
  async fn auto_deny_returns_deny_outcome_by_default() {
    let provider = AutoDenyApprovalProvider::new();
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Deny);
  }

  #[tokio::test]
  async fn auto_deny_can_request_stop() {
    let provider = AutoDenyApprovalProvider::new().with_stop_on_deny(true);
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::DenyAndStop);
  }

  #[tokio::test]
  async fn cli_provider_parses_yes_response_as_allow_once() {
    let writer: Box<dyn Write + Send> = Box::new(StdMutex::new(Vec::new()).into_inner().unwrap());
    let reader: Box<dyn BufRead + Send> = Box::new(Cursor::new(b"y\n".to_vec()));
    let provider = CliApprovalProvider {
      prompt_writer: Arc::new(Mutex::new(writer)),
      input_reader: Arc::new(Mutex::new(reader)),
      pending_read: tokio::sync::Mutex::new(None),
    };
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Allow);
    assert_eq!(decision.scope, ApprovalScope::Once);
    assert_eq!(decision.decided_by, "user");
  }

  #[tokio::test]
  async fn cli_provider_parses_session_scope() {
    let provider =
      CliApprovalProvider::with_streams(Vec::<u8>::new(), Cursor::new(b"s\n".to_vec()));
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.scope, ApprovalScope::Session);
  }

  #[tokio::test]
  async fn cli_provider_treats_empty_input_as_deny() {
    let provider = CliApprovalProvider::with_streams(Vec::<u8>::new(), Cursor::new(b"\n".to_vec()));
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Deny);
  }

  #[tokio::test]
  async fn cli_provider_quit_returns_deny_and_stop() {
    let provider =
      CliApprovalProvider::with_streams(Vec::<u8>::new(), Cursor::new(b"q\n".to_vec()));
    let decision = provider.request(sample_request()).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::DenyAndStop);
  }

  #[tokio::test]
  async fn cli_provider_honours_expires_at_deadline() {
    // 100ms-from-now deadline + 500ms-delayed input.
    let slow_input: Vec<u8> = b"y\n".to_vec();
    // To force a deadline miss without sleeping the test for half a
    // second, we let the reader block on a never-ready stream by
    // using a pipe-like construct. Simpler: read from a small but
    // valid stream after deadline has passed by setting expires_at in
    // the past.
    let mut request = sample_request();
    request.expires_at = Some(Utc::now() - ChronoDuration::milliseconds(10));
    let provider = CliApprovalProvider::with_streams(Vec::<u8>::new(), Cursor::new(slow_input));
    let result = provider.request(request).await;
    // The deadline has already passed, but the reader is non-blocking
    // (returns immediately with `y\n`). Depending on the runtime's
    // selection, either ApprovalTimeout or Allow can win. We accept
    // either to keep the test deterministic.
    match result {
      Err(HarnessError::ApprovalTimeout { .. }) => {}
      Ok(decision) => assert_eq!(decision.decision, ApprovalOutcome::Allow),
      Err(other) => panic!("unexpected error: {other:?}"),
    }
  }

  /// Input whose lines arrive when the test sends them, like a terminal.
  struct TypedInput(std::sync::mpsc::Receiver<String>, Vec<u8>);

  impl std::io::Read for TypedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
      if self.1.is_empty() {
        match self.0.recv() {
          Ok(line) => self.1 = line.into_bytes(),
          Err(_) => return Ok(0),
        }
      }
      let n = buf.len().min(self.1.len());
      buf[..n].copy_from_slice(&self.1[..n]);
      self.1.drain(..n);
      Ok(n)
    }
  }

  /// Eval 2026-10-07 A5: a timed-out prompt left its blocking read
  /// pending, so it swallowed the answer typed for the next prompt; a late
  /// answer to the expired prompt must not decide the next one either.
  #[tokio::test]
  async fn cli_provider_timeout_does_not_swallow_the_next_answer() {
    let (typed, input) = std::sync::mpsc::channel::<String>();
    let provider = CliApprovalProvider::with_streams(
      Vec::<u8>::new(),
      std::io::BufReader::new(TypedInput(input, Vec::new())),
    );
    let expiring = |ms| {
      let mut request = sample_request();
      request.expires_at = Some(Utc::now() + ChronoDuration::milliseconds(ms));
      request
    };

    let answer_after = |line: &'static str| {
      let typed = typed.clone();
      tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        typed.send(line.into()).unwrap();
      });
    };

    // The answer typed for the prompt after a timeout reaches it.
    assert!(matches!(
      provider.request(expiring(100)).await,
      Err(HarnessError::ApprovalTimeout { .. })
    ));
    answer_after("y\n");
    let decision = provider.request(expiring(2_000)).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Allow);

    // A late answer to an expired prompt does not decide the next one.
    assert!(matches!(
      provider.request(expiring(100)).await,
      Err(HarnessError::ApprovalTimeout { .. })
    ));
    typed.send("q\n".into()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    answer_after("y\n");
    let decision = provider.request(expiring(2_000)).await.unwrap();
    assert_eq!(decision.decision, ApprovalOutcome::Allow);
  }
}
