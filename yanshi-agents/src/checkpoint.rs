//! File-based implementation of the agent-loop checkpoint contract (V2.4).
//!
//! `yanshi-agent-spi::checkpoint` defines the contract
//! ([`AgentLoopCheckpoint`]/[`AgentLoopCheckpointer`]); this module provides
//! the concrete on-disk storage `ReActAgent`/`PlanExecuteAgent` write to and
//! `yanshi-cli`/`yanshi-harness` wire in. Deliberately does not reuse
//! `yanshi-core::checkpoint::CheckpointManager` — that type's
//! per-DAG-node `Checkpoint`/`WorkflowStatus` shape and retention policy are
//! a different concern (an audit-trail-style history of node completions)
//! from a single-file "latest loop position" snapshot.

use std::path::{Path, PathBuf};

// Re-exported (not just `use`d) so downstream crates that depend on
// `yanshi-agents` but not directly on `yanshi-agent-spi` (e.g.
// `yanshi-cli`) can reach the contract through this module too,
// alongside the concrete `FileLoopCheckpointer` below.
use async_trait::async_trait;
pub use yanshi_agent_spi::checkpoint::{
  AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION, AgentLoopCheckpoint, AgentLoopCheckpointError,
  AgentLoopCheckpointer,
};
use yanshi_agent_spi::runtime::AgentStopReason;

/// V2.4: whether a loop checkpoint should be cleared once the loop stops
/// for `reason`. Shared by `ReActAgent` and `PlanExecuteAgent` so the two
/// runtimes agree on which stop reasons represent a genuine completion
/// (checkpoint no longer useful, clear it so a later reused `session_id`
/// doesn't get confused by stale progress) versus a legitimately
/// resumable interruption (keep it).
///
/// Exhaustive over every [`AgentStopReason`] variant — the enum is
/// deliberately NOT `#[non_exhaustive]` so every consumer (e.g.
/// `yanshi-harness`'s `StoppedPayload` translation) is forced to
/// handle new variants explicitly; this follows the same convention
/// rather than risking a new variant silently falling into the wrong
/// bucket via a wildcard.
///
/// For the crash-simulation scenario this feature targets — the process
/// dies with no stop reason produced at all — this function never even
/// runs; the last-written checkpoint simply survives untouched on disk.
pub(crate) fn should_clear_checkpoint(reason: &AgentStopReason) -> bool {
  match reason {
    AgentStopReason::FinalAnswer => true,
    AgentStopReason::StopCondition { .. } => true,
    AgentStopReason::Error { .. } => true,
    AgentStopReason::MaxSteps { .. } => false,
    AgentStopReason::MaxToolCalls { .. } => false,
    AgentStopReason::Timeout { .. } => false,
    AgentStopReason::Cancelled { .. } => false,
    AgentStopReason::TokenBudgetExceeded { .. } => false,
    AgentStopReason::CostLimitExceeded { .. } => false,
    AgentStopReason::LoopDetected { .. } => false,
    // V2.3: the single most resumable interruption there is — the
    // checkpoint's `pending_question` is exactly what a resume-with-
    // answer call needs.
    AgentStopReason::AwaitingInput { .. } => false,
    // W0.5: a deliberate policy decision, not a pause — resuming this
    // checkpoint would either replay straight into the same denial
    // (session/run-scoped cache) or silently bypass what the approver
    // explicitly refused. Same bucket as `Error`: definitively over.
    AgentStopReason::ApprovalDenied { .. } => true,
  }
}

/// `session_id` is externally supplied (CLI `--session`, server-generated
/// UUID) and becomes part of a file path — reject anything that could
/// escape `dir` or collide with reserved names. Mirrors
/// `yanshi_core::checkpoint::is_safe_path_component` (that predicate is
/// `pub(crate)` there, not importable, so this is a deliberate duplicate).
fn is_safe_path_component(id: &str) -> bool {
  !id.is_empty() && !id.contains('/') && !id.contains('\\') && id != "." && id != ".."
}

/// Single-file-per-session [`AgentLoopCheckpointer`]: only the latest loop
/// position matters for resume, unlike the DAG checkpoint's timestamped
/// audit trail, so each `save` atomically overwrites `<dir>/<session_id>.json`.
pub struct FileLoopCheckpointer {
  dir: PathBuf,
}

impl FileLoopCheckpointer {
  /// Create a checkpointer rooted at `dir`, creating the directory if it
  /// doesn't exist.
  pub fn new(dir: impl Into<PathBuf>) -> Result<Self, AgentLoopCheckpointError> {
    let dir = dir.into();
    std::fs::create_dir_all(&dir).map_err(|e| AgentLoopCheckpointError::Io {
      message: format!(
        "failed to create loop checkpoint dir {}: {e}",
        dir.display()
      ),
    })?;
    Ok(Self { dir })
  }

  /// Conventional loop-checkpoint directory under a run root, sibling to
  /// `yanshi_harness::default_session_dir`'s `<root>/harness/sessions`.
  pub fn default_dir(run_root: &Path) -> PathBuf {
    run_root.join("harness").join("loop_checkpoints")
  }

  fn path_for(&self, session_id: &str) -> Result<PathBuf, AgentLoopCheckpointError> {
    if !is_safe_path_component(session_id) {
      return Err(AgentLoopCheckpointError::InvalidSessionId {
        session_id: session_id.to_string(),
      });
    }
    Ok(self.dir.join(format!("{session_id}.json")))
  }
}

#[async_trait]
impl AgentLoopCheckpointer for FileLoopCheckpointer {
  async fn save(&self, checkpoint: &AgentLoopCheckpoint) -> Result<(), AgentLoopCheckpointError> {
    let path = self.path_for(&checkpoint.session_id)?;
    let tmp_path = path.with_extension("json.tmp");
    let json =
      serde_json::to_string_pretty(checkpoint).map_err(|e| AgentLoopCheckpointError::Io {
        message: format!("failed to serialize loop checkpoint: {e}"),
      })?;
    tokio::fs::write(&tmp_path, json)
      .await
      .map_err(|e| AgentLoopCheckpointError::Io {
        message: format!("failed to write {}: {e}", tmp_path.display()),
      })?;
    tokio::fs::rename(&tmp_path, &path)
      .await
      .map_err(|e| AgentLoopCheckpointError::Io {
        message: format!(
          "failed to rename {} to {}: {e}",
          tmp_path.display(),
          path.display()
        ),
      })?;
    Ok(())
  }

  async fn load(
    &self,
    session_id: &str,
  ) -> Result<Option<AgentLoopCheckpoint>, AgentLoopCheckpointError> {
    let path = self.path_for(session_id)?;
    let bytes = match tokio::fs::read(&path).await {
      Ok(bytes) => bytes,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(e) => {
        return Err(AgentLoopCheckpointError::Io {
          message: format!("failed to read {}: {e}", path.display()),
        });
      }
    };
    let checkpoint: AgentLoopCheckpoint =
      serde_json::from_slice(&bytes).map_err(|e| AgentLoopCheckpointError::Io {
        message: format!("failed to parse loop checkpoint {}: {e}", path.display()),
      })?;
    if checkpoint.schema_version > AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION {
      return Err(AgentLoopCheckpointError::UnsupportedSchemaVersion {
        found: checkpoint.schema_version,
        supported: AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      });
    }
    Ok(Some(checkpoint))
  }

  async fn clear(&self, session_id: &str) -> Result<(), AgentLoopCheckpointError> {
    let path = self.path_for(session_id)?;
    match tokio::fs::remove_file(&path).await {
      Ok(()) => Ok(()),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(e) => Err(AgentLoopCheckpointError::Io {
        message: format!("failed to remove {}: {e}", path.display()),
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Utc;
  use std::collections::VecDeque;
  use yanshi_agent_spi::checkpoint::LoopRuntimeKind;

  fn sample(session_id: &str) -> AgentLoopCheckpoint {
    AgentLoopCheckpoint {
      schema_version: AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      session_id: session_id.to_string(),
      runtime_kind: LoopRuntimeKind::React,
      created_at: Utc::now(),
      steps: vec![],
      events: vec![],
      step_index: 2,
      iteration: 2,
      tool_calls: 1,
      verification_attempts: 0,
      schema_correction_attempts: 0,
      last_tool_call: None,
      recent_tool_calls: VecDeque::new(),
      cumulative_cost_usd: 0.0,
      cumulative_tokens: 0,
      system_prompt: "be helpful".into(),
      user_input: "do the thing".into(),
      trace_context: None,
      plan_steps: serde_json::Value::Null,
      plan_position: 0,
      observations: vec![],
      pending_question: None,
    }
  }

  #[tokio::test]
  async fn save_then_load_round_trips_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    let checkpoint = sample("sess-1");
    checkpointer.save(&checkpoint).await.unwrap();
    let loaded = checkpointer.load("sess-1").await.unwrap();
    assert_eq!(loaded, Some(checkpoint));
  }

  #[tokio::test]
  async fn save_is_atomic_no_tmp_file_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    checkpointer.save(&sample("sess-1")).await.unwrap();
    let entries: Vec<_> = std::fs::read_dir(dir.path())
      .unwrap()
      .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
      .collect();
    assert_eq!(entries, vec!["sess-1.json".to_string()]);
  }

  #[tokio::test]
  async fn load_on_missing_session_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    assert_eq!(checkpointer.load("never-saved").await.unwrap(), None);
  }

  #[tokio::test]
  async fn save_then_load_a_second_session_overwrites_only_its_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    checkpointer.save(&sample("sess-1")).await.unwrap();
    let mut second = sample("sess-1");
    second.step_index = 9;
    checkpointer.save(&second).await.unwrap();
    let loaded = checkpointer.load("sess-1").await.unwrap().unwrap();
    assert_eq!(loaded.step_index, 9);
  }

  #[tokio::test]
  async fn clear_removes_the_checkpoint_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    checkpointer.save(&sample("sess-1")).await.unwrap();
    checkpointer.clear("sess-1").await.unwrap();
    assert_eq!(checkpointer.load("sess-1").await.unwrap(), None);
    // Clearing an already-clear session must not error.
    checkpointer.clear("sess-1").await.unwrap();
  }

  #[tokio::test]
  async fn rejects_unsafe_session_ids() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    for bad in ["", ".", "..", "../escape", "nested/path", "nested\\path"] {
      let mut checkpoint = sample(bad);
      checkpoint.session_id = bad.to_string();
      let result = checkpointer.save(&checkpoint).await;
      assert!(
        matches!(
          result,
          Err(AgentLoopCheckpointError::InvalidSessionId { .. })
        ),
        "expected InvalidSessionId for {bad:?}, got {result:?}"
      );
      let result = checkpointer.load(bad).await;
      assert!(
        matches!(
          result,
          Err(AgentLoopCheckpointError::InvalidSessionId { .. })
        ),
        "expected InvalidSessionId for {bad:?}, got {result:?}"
      );
    }
  }

  #[tokio::test]
  async fn load_rejects_a_newer_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let checkpointer = FileLoopCheckpointer::new(dir.path()).unwrap();
    let mut checkpoint = sample("sess-1");
    checkpoint.schema_version = AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION + 1;
    checkpointer.save(&checkpoint).await.unwrap();
    let result = checkpointer.load("sess-1").await;
    assert!(matches!(
      result,
      Err(AgentLoopCheckpointError::UnsupportedSchemaVersion { .. })
    ));
  }

  #[test]
  fn default_dir_is_a_sibling_of_the_harness_sessions_dir() {
    let root = Path::new("/tmp/run-root");
    assert_eq!(
      FileLoopCheckpointer::default_dir(root),
      root.join("harness").join("loop_checkpoints")
    );
  }
}
