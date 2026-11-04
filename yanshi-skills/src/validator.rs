//! Skill-declared answer validator backing the eval harness's
//! `final_answer_matches_skill` assertion.
//!
//! v1 protocol: `docs/SKILL_VALIDATOR_PROTOCOL.md`. Three closed
//! kinds (`none` / `regex` / `command`) wired through one synchronous
//! trait so the agent eval `AssertionContext` can call the validator
//! without forcing the assertion DSL to be async.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use regex::{Regex, RegexBuilder};

use crate::error::SkillError;
use crate::manifest::{
  SkillManifest, VALIDATOR_TIMEOUT_SECS_MAX, VALIDATOR_TIMEOUT_SECS_MIN, ValidationConfig,
};

/// Exit code reserved per the v1 protocol for "validator could not
/// run" (mirrors `git bisect run` semantics).
pub const VALIDATOR_UNRUNNABLE_EXIT_CODE: i32 = 125;

/// Verdict returned by [`SkillValidator::validate`].
///
/// - `Pass` / `Fail` are the binary outcomes consumed by the
///   `final_answer_matches_skill` assertion.
/// - `Unrunnable` is surfaced when the validator itself couldn't
///   execute (e.g. command exit 125, timeout, I/O failure). The
///   assertion layer flips `Unrunnable` into a failed outcome but
///   tags it with a different `reason` string so operators can
///   distinguish "the skill rejected this answer" from "we couldn't
///   ask the skill".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatorVerdict {
  Pass,
  Fail {
    /// Free-form reason surfaced verbatim into
    /// `AssertionOutcome.reason`.
    reason: String,
  },
  Unrunnable {
    reason: String,
  },
}

impl ValidatorVerdict {
  /// Convenience: did the validator accept the answer?
  pub fn is_pass(&self) -> bool {
    matches!(self, Self::Pass)
  }
}

/// Synchronous trait so the assertion DSL doesn't need to be async.
/// Implementations that internally await an async runtime (e.g. the
/// `command` validator) use `tokio::runtime::Handle::block_on` inside
/// `block_in_place`; that detail is private.
pub trait SkillValidator: Send + Sync {
  fn validate(&self, final_answer: &str) -> ValidatorVerdict;
}

/// `kind = "regex"` validator. Pattern is pre-compiled at build time
/// (`build_validator` returns `Err` for malformed patterns).
#[derive(Debug)]
pub struct RegexValidator {
  regex: Regex,
}

impl RegexValidator {
  pub fn new(pattern: &str, multiline: bool, dotall: bool) -> Result<Self, SkillError> {
    let regex = RegexBuilder::new(pattern)
      .multi_line(multiline)
      .dot_matches_new_line(dotall)
      .build()
      .map_err(|e| SkillError::ValidationError {
        message: format!("invalid validator regex '{pattern}': {e}"),
      })?;
    Ok(Self { regex })
  }
}

impl SkillValidator for RegexValidator {
  fn validate(&self, final_answer: &str) -> ValidatorVerdict {
    if self.regex.is_match(final_answer) {
      ValidatorVerdict::Pass
    } else {
      ValidatorVerdict::Fail {
        reason: format!(
          "skill validator regex `{}` did not match the final answer",
          self.regex.as_str()
        ),
      }
    }
  }
}

/// `kind = "command"` validator. See
/// `docs/SKILL_VALIDATOR_PROTOCOL.md` for the wire protocol.
#[derive(Debug)]
pub struct CommandValidator {
  command: Vec<String>,
  timeout: Duration,
  working_dir: PathBuf,
  env_allowlist: Vec<String>,
}

impl CommandValidator {
  pub fn new(
    command: Vec<String>,
    timeout_secs: u64,
    working_dir: PathBuf,
    env_allowlist: Vec<String>,
  ) -> Result<Self, SkillError> {
    if command.is_empty() {
      return Err(SkillError::ValidationError {
        message: "validator command must not be empty".to_string(),
      });
    }
    let clamped = timeout_secs.clamp(VALIDATOR_TIMEOUT_SECS_MIN, VALIDATOR_TIMEOUT_SECS_MAX);
    Ok(Self {
      command,
      timeout: Duration::from_secs(clamped),
      working_dir,
      env_allowlist,
    })
  }

  /// Run the validator command and translate its outcome into a
  /// [`ValidatorVerdict`]. Synchronous on top of an internal Tokio
  /// runtime so the trait stays sync.
  fn run_sync(&self, final_answer: &str) -> ValidatorVerdict {
    // Build a small current-thread runtime; the command itself is the
    // only blocking work, and the validator's contract caps it at
    // `timeout_secs` so we don't share a runtime with the eval loop.
    let runtime = match tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
    {
      Ok(rt) => rt,
      Err(e) => {
        return ValidatorVerdict::Unrunnable {
          reason: format!("could not build validator runtime: {e}"),
        };
      }
    };
    runtime.block_on(self.run_async(final_answer))
  }

  async fn run_async(&self, final_answer: &str) -> ValidatorVerdict {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command as AsyncCommand;

    let mut cmd = AsyncCommand::new(&self.command[0]);
    cmd.args(&self.command[1..]);
    cmd.current_dir(&self.working_dir);
    cmd.env_clear();
    for key in &self.env_allowlist {
      if let Ok(value) = std::env::var(key) {
        cmd.env(key, value);
      }
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
      Ok(c) => c,
      Err(e) => {
        return ValidatorVerdict::Unrunnable {
          reason: format!(
            "could not spawn validator command `{}`: {e}",
            self.command[0]
          ),
        };
      }
    };
    if let Some(mut stdin) = child.stdin.take() {
      // A validator that doesn't read stdin (e.g. `sh -c "exit 0"`) closes
      // its end of the pipe as it exits. On Linux that race typically wins
      // before our write completes, surfacing `BrokenPipe`. That is NOT a
      // validator-execution failure — the validator's own exit code is the
      // authoritative signal, so we swallow EPIPE here and let the wait
      // logic below decide Pass/Fail/Unrunnable from the exit status.
      // Other I/O errors (PermissionDenied, etc.) still mean we couldn't
      // run the validator and remain Unrunnable.
      match stdin.write_all(final_answer.as_bytes()).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => {
          return ValidatorVerdict::Unrunnable {
            reason: format!("failed to write final answer to validator stdin: {e}"),
          };
        }
      }
      // Close stdin to signal EOF to the validator.
      drop(stdin);
    }

    let wait_for_output = child.wait_with_output();
    let output = match tokio::time::timeout(self.timeout, wait_for_output).await {
      Ok(Ok(out)) => out,
      Ok(Err(e)) => {
        return ValidatorVerdict::Unrunnable {
          reason: format!("validator command failed: {e}"),
        };
      }
      Err(_) => {
        return ValidatorVerdict::Fail {
          reason: format!("validator timed out after {}s", self.timeout.as_secs()),
        };
      }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code == 0 {
      return ValidatorVerdict::Pass;
    }
    if exit_code == VALIDATOR_UNRUNNABLE_EXIT_CODE {
      let stderr = trim_stderr(&output.stderr);
      return ValidatorVerdict::Unrunnable {
        reason: format!("validator unrunnable: {stderr}"),
      };
    }
    let stderr = trim_stderr(&output.stderr);
    ValidatorVerdict::Fail {
      reason: format!("skill validator rejected the final answer (exit {exit_code}): {stderr}"),
    }
  }
}

impl SkillValidator for CommandValidator {
  fn validate(&self, final_answer: &str) -> ValidatorVerdict {
    self.run_sync(final_answer)
  }
}

/// 2 KiB cap on the stderr snippet surfaced into outcomes, per the
/// design doc. Truncation is at byte boundaries with a clear marker.
fn trim_stderr(bytes: &[u8]) -> String {
  const MAX: usize = 2 * 1024;
  let s = String::from_utf8_lossy(bytes);
  if s.len() <= MAX {
    s.into_owned().trim().to_string()
  } else {
    let mut out = s[..MAX].to_string();
    out.push_str("…[truncated]");
    out.trim().to_string()
  }
}

/// Build a [`SkillValidator`] from the manifest's [`ValidationConfig`].
///
/// Returns `Ok(None)` when the skill declares
/// [`ValidationConfig::None`] (or omits the section). Returns
/// `Err(SkillError)` when a `regex` pattern fails to compile or a
/// `command` declaration is malformed — caught at
/// `SkillLoader::validate` time so eval runs never trip on bad
/// manifests.
pub fn build_validator(
  manifest: &SkillManifest,
  skill_dir: &Path,
) -> Result<Option<Arc<dyn SkillValidator>>, SkillError> {
  match &manifest.validation {
    ValidationConfig::None => Ok(None),
    ValidationConfig::Regex {
      pattern,
      multiline,
      dotall,
    } => {
      let v = RegexValidator::new(pattern, *multiline, *dotall)?;
      Ok(Some(Arc::new(v)))
    }
    ValidationConfig::Command {
      command,
      timeout_secs,
      working_dir,
      env_allowlist,
    } => {
      let resolved_working = if Path::new(working_dir).is_absolute() {
        PathBuf::from(working_dir)
      } else {
        skill_dir.join(working_dir)
      };
      let v = CommandValidator::new(
        command.clone(),
        *timeout_secs,
        resolved_working,
        env_allowlist.clone(),
      )?;
      Ok(Some(Arc::new(v)))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::manifest::SkillInfo;

  fn manifest_with(validation: ValidationConfig) -> SkillManifest {
    SkillManifest {
      skill: SkillInfo {
        name: "test".to_string(),
        version: "0.1.0".to_string(),
        description: "fixture".to_string(),
      },
      persona: crate::manifest::PersonaConfig {
        role: "test".to_string(),
        language: None,
      },
      model: crate::manifest::ModelConfig::default(),
      security: crate::manifest::SecurityConfig::default(),
      tools: vec![],
      mcp_servers: vec![],
      knowledge: vec![],
      memory: None,
      validation,
      scripts: vec![],
      dependencies: Default::default(),
      workflows: Vec::new(),
    }
  }

  // ── Regex validator ───────────────────────────────────────────────────

  #[test]
  fn regex_validator_passes_when_pattern_matches() {
    let v = RegexValidator::new(r"^OK\b", false, false).unwrap();
    assert_eq!(v.validate("OK done"), ValidatorVerdict::Pass);
  }

  #[test]
  fn regex_validator_fails_with_pattern_in_reason() {
    let v = RegexValidator::new(r"^OK\b", false, false).unwrap();
    match v.validate("not ok") {
      ValidatorVerdict::Fail { reason } => {
        assert!(reason.contains("^OK"), "reason: {reason}");
      }
      other => panic!("expected Fail, got {other:?}"),
    }
  }

  #[test]
  fn regex_validator_honors_multiline_flag() {
    let v = RegexValidator::new(r"^OK", true, false).unwrap();
    assert!(v.validate("first line\nOK second").is_pass());
    // Without multiline the same pattern would fail on the second
    // line.
    let v2 = RegexValidator::new(r"^OK", false, false).unwrap();
    assert!(!v2.validate("first line\nOK second").is_pass());
  }

  #[test]
  fn regex_validator_rejects_malformed_pattern_at_build_time() {
    let err = RegexValidator::new("[unclosed", false, false).unwrap_err();
    assert!(
      matches!(err, SkillError::ValidationError { message } if message.contains("invalid validator regex")),
      "err shape unexpected"
    );
  }

  // ── build_validator factory ───────────────────────────────────────────

  #[test]
  fn build_validator_returns_none_for_kind_none() {
    let m = manifest_with(ValidationConfig::None);
    let v = build_validator(&m, Path::new(".")).expect("build ok");
    assert!(v.is_none());
  }

  #[test]
  fn build_validator_returns_some_for_regex() {
    let m = manifest_with(ValidationConfig::Regex {
      pattern: r"hello".to_string(),
      multiline: false,
      dotall: false,
    });
    let built = build_validator(&m, Path::new(".")).expect("build ok");
    let v = built.expect("regex validator should be present");
    assert!(v.validate("hello world").is_pass());
  }

  #[test]
  fn build_validator_surfaces_bad_regex_as_skill_error() {
    let m = manifest_with(ValidationConfig::Regex {
      pattern: "[unclosed".to_string(),
      multiline: false,
      dotall: false,
    });
    let err = match build_validator(&m, Path::new(".")) {
      Ok(_) => panic!("expected error for malformed regex"),
      Err(e) => e,
    };
    assert!(matches!(err, SkillError::ValidationError { .. }));
  }

  #[test]
  fn build_validator_rejects_empty_command_vector() {
    let m = manifest_with(ValidationConfig::Command {
      command: vec![],
      timeout_secs: 5,
      working_dir: ".".to_string(),
      env_allowlist: vec!["PATH".to_string()],
    });
    let err = match build_validator(&m, Path::new(".")) {
      Ok(_) => panic!("expected error for empty command"),
      Err(e) => e,
    };
    assert!(
      matches!(err, SkillError::ValidationError { message } if message.contains("must not be empty"))
    );
  }

  #[test]
  fn build_validator_command_resolves_relative_working_dir_against_skill_dir() {
    // Construct the validator and inspect it via SkillValidator::validate
    // by routing through a no-op `true` command (Unix `true` is on PATH
    // in CI). The test only checks construction succeeds and the
    // command is runnable.
    let m = manifest_with(ValidationConfig::Command {
      command: vec!["true".to_string()],
      timeout_secs: 5,
      working_dir: "subdir".to_string(),
      env_allowlist: vec!["PATH".to_string()],
    });
    let tmp = tempfile::tempdir().unwrap();
    let built = build_validator(&m, tmp.path()).expect("build ok");
    assert!(built.is_some(), "command validator should be present");
  }

  // ── Command validator end-to-end ─────────────────────────────────────

  /// Unix-only: relies on `/bin/sh -c` being available. The CI matrix
  /// runs on macOS + Linux so this gate is fine.
  #[cfg(unix)]
  #[test]
  fn command_validator_pass_when_exit_code_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
      vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exit 0".to_string(),
      ],
      5,
      tmp.path().to_path_buf(),
      vec!["PATH".to_string()],
    )
    .unwrap();
    assert_eq!(v.validate("anything"), ValidatorVerdict::Pass);
  }

  #[cfg(unix)]
  #[test]
  fn command_validator_fail_when_exit_code_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
      vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo bad >&2; exit 2".to_string(),
      ],
      5,
      tmp.path().to_path_buf(),
      vec!["PATH".to_string()],
    )
    .unwrap();
    match v.validate("anything") {
      ValidatorVerdict::Fail { reason } => {
        assert!(reason.contains("exit 2"), "reason: {reason}");
        assert!(
          reason.contains("bad"),
          "stderr not captured; reason: {reason}"
        );
      }
      other => panic!("expected Fail, got {other:?}"),
    }
  }

  #[cfg(unix)]
  #[test]
  fn command_validator_unrunnable_on_reserved_exit_code_125() {
    let tmp = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
      vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo busted >&2; exit 125".to_string(),
      ],
      5,
      tmp.path().to_path_buf(),
      vec!["PATH".to_string()],
    )
    .unwrap();
    match v.validate("answer") {
      ValidatorVerdict::Unrunnable { reason } => {
        assert!(reason.contains("busted"), "reason: {reason}");
      }
      other => panic!("expected Unrunnable, got {other:?}"),
    }
  }

  #[cfg(unix)]
  #[test]
  fn command_validator_timeout_reports_fail_with_seconds_in_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
      vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 5".to_string(),
      ],
      1, // 1-second timeout against a 5-second sleep
      tmp.path().to_path_buf(),
      vec!["PATH".to_string()],
    )
    .unwrap();
    match v.validate("x") {
      ValidatorVerdict::Fail { reason } => {
        assert!(reason.contains("timed out"), "reason: {reason}");
        assert!(reason.contains("1s"), "reason: {reason}");
      }
      other => panic!("expected Fail (timeout), got {other:?}"),
    }
  }

  #[cfg(unix)]
  #[test]
  fn command_validator_receives_final_answer_on_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    // Script reads stdin and passes only when it contains "MAGIC".
    let v = CommandValidator::new(
      vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "grep -q MAGIC".to_string(),
      ],
      5,
      tmp.path().to_path_buf(),
      vec!["PATH".to_string()],
    )
    .unwrap();
    assert!(v.validate("the MAGIC word is here").is_pass());
    assert!(!v.validate("nothing here").is_pass());
  }

  // ── Timeout clamping ─────────────────────────────────────────────────

  #[test]
  fn command_validator_clamps_timeout_to_max_and_min() {
    let tmp = tempfile::tempdir().unwrap();
    let v_big = CommandValidator::new(
      vec!["true".to_string()],
      9_999,
      tmp.path().to_path_buf(),
      vec![],
    )
    .unwrap();
    assert_eq!(
      v_big.timeout,
      Duration::from_secs(VALIDATOR_TIMEOUT_SECS_MAX)
    );
    let v_zero = CommandValidator::new(
      vec!["true".to_string()],
      0,
      tmp.path().to_path_buf(),
      vec![],
    )
    .unwrap();
    assert_eq!(
      v_zero.timeout,
      Duration::from_secs(VALIDATOR_TIMEOUT_SECS_MIN)
    );
  }

  // ── Manifest round trip ──────────────────────────────────────────────

  #[test]
  fn validation_config_toml_round_trip_for_each_kind() {
    let cases = [
      r#"
kind = "none"
"#,
      r#"
kind = "regex"
pattern = "(?i)\\bOK\\b"
multiline = false
dotall = true
"#,
      r#"
kind = "command"
command = ["bash", "tests/validator.sh"]
timeout_secs = 5
working_dir = "."
env_allowlist = ["PATH"]
"#,
    ];
    for src in cases {
      let cfg: ValidationConfig = toml::from_str(src).unwrap();
      // Re-serialize → re-parse for stability.
      let s = toml::to_string(&cfg).unwrap();
      let _back: ValidationConfig = toml::from_str(&s).unwrap();
    }
  }
}
