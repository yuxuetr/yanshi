//! Workflow state checkpoint and recovery system
//!
//! This module provides persistent checkpoint capabilities for workflow execution,
//! enabling fault tolerance and resumable workflows.
//!
//! # Features
//!
//! - Incremental checkpointing after each node execution
//! - Atomic file operations (write-then-rename) — no cross-process file
//!   locking; concurrent writers to the same checkpoint key race at the
//!   OS rename level (last write to complete the rename wins), rather
//!   than being serialized by a lock
//! - Workflow recovery from last checkpoint
//! - TTL-based cleanup of old checkpoints
//!
//! # Example
//!
//! ```no_run
//! use yanshi_core::checkpoint::{CheckpointManager, CheckpointConfig};
//! use std::collections::HashMap;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = CheckpointConfig::default()
//!     .with_checkpoint_dir("./checkpoints")
//!     .with_retention_days(7);
//!
//! let manager = CheckpointManager::new(config)?;
//!
//! // Save checkpoint
//! let mut state = HashMap::new();
//! state.insert("node1".to_string(), serde_json::json!({"status": "completed"}));
//! manager.save_checkpoint("workflow_123", "node1", &state).await?;
//!
//! // Resume from checkpoint
//! if let Some(checkpoint) = manager.load_latest_checkpoint("workflow_123").await? {
//!     println!("Resuming from node: {}", checkpoint.last_completed_node);
//! }
//! # Ok(())
//! # }
//! ```

use crate::error::{Result, YanshiError};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

// `CheckpointConfig` (the data a Flow carries) moved to `yanshi-graph`
// (P-A1.3 step 2c) so the IR `Flow` can hold it. Re-export it here so
// `yanshi_core::checkpoint::CheckpointConfig` and the `CheckpointManager`
// below keep resolving unchanged. The manager (IO logic) stays in this crate.
pub use yanshi_graph::checkpoint::CheckpointConfig;

/// Workflow checkpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
  /// Workflow run ID
  pub workflow_id: String,

  /// Last successfully completed node ID
  pub last_completed_node: String,

  /// Workflow state (node ID -> output value)
  pub state: HashMap<String, serde_json::Value>,

  /// Checkpoint creation timestamp
  pub created_at: DateTime<Utc>,

  /// Workflow status
  pub status: WorkflowStatus,

  /// Additional metadata
  pub metadata: HashMap<String, String>,
}

/// Y2.4: `Checkpoint::metadata` keys the executor writes so a resume
/// can reproduce the original run: its inputs, how it was scheduled, its
/// cost limit and what it had already spent.
pub mod run_metadata {
  /// JSON object of the run's initial inputs (tagged `FlowValue`s).
  pub const INPUTS: &str = "yanshi.inputs";
  /// USD the run had recorded when the checkpoint was written.
  pub const COST_USD: &str = "yanshi.cost_usd";
  /// `serial` or `concurrent`.
  pub const EXECUTION_MODE: &str = "yanshi.execution_mode";
  pub const MAX_CONCURRENCY: &str = "yanshi.max_concurrency";
  pub const COST_LIMIT_USD: &str = "yanshi.cost_limit_usd";
  pub const RUN_BASE_DIR: &str = "yanshi.run_base_dir";
}

/// Y2.4: the metadata the executor stores with each checkpoint.
pub fn run_metadata_for(
  inputs: &crate::async_node::AsyncNodeInputs,
  config: &crate::FlowExecutionConfig,
  cost_usd: f64,
) -> HashMap<String, String> {
  let mut metadata = HashMap::new();
  if let Ok(json) = serde_json::to_string(inputs) {
    metadata.insert(run_metadata::INPUTS.to_string(), json);
  }
  metadata.insert(run_metadata::COST_USD.to_string(), cost_usd.to_string());
  let mode = match config.mode {
    crate::FlowExecutionMode::Serial => "serial",
    crate::FlowExecutionMode::Concurrent => "concurrent",
  };
  metadata.insert(run_metadata::EXECUTION_MODE.to_string(), mode.to_string());
  metadata.insert(
    run_metadata::MAX_CONCURRENCY.to_string(),
    config.max_concurrency.to_string(),
  );
  if let Some(limit) = config.cost_limit_usd {
    metadata.insert(run_metadata::COST_LIMIT_USD.to_string(), limit.to_string());
  }
  if let Some(dir) = &config.run_base_dir {
    metadata.insert(
      run_metadata::RUN_BASE_DIR.to_string(),
      dir.to_string_lossy().into_owned(),
    );
  }
  metadata
}

impl Checkpoint {
  /// Y2.4: the run's initial inputs. Empty for checkpoints written
  /// before Y2.4; an unreadable value is an error rather than a silent
  /// resume with missing inputs.
  pub fn recorded_inputs(&self) -> Result<crate::async_node::AsyncNodeInputs> {
    match self.metadata.get(run_metadata::INPUTS) {
      Some(json) => serde_json::from_str(json).map_err(|e| {
        YanshiError::SerializationError(format!("checkpoint inputs are unreadable: {e}"))
      }),
      None => Ok(HashMap::new()),
    }
  }

  /// Y2.4: USD the run had spent when this checkpoint was written.
  pub fn recorded_cost_usd(&self) -> f64 {
    self
      .metadata
      .get(run_metadata::COST_USD)
      .and_then(|v| v.parse::<f64>().ok())
      .filter(|v| v.is_finite() && *v > 0.0)
      .unwrap_or(0.0)
  }

  /// Y2.4: the original run's scheduling, cost limit and run base dir;
  /// serial with the defaults for checkpoints written before Y2.4.
  pub fn recorded_execution_config(&self) -> crate::FlowExecutionConfig {
    let get = |key: &str| self.metadata.get(key);
    let max_concurrency = get(run_metadata::MAX_CONCURRENCY)
      .and_then(|v| v.parse::<usize>().ok())
      .unwrap_or(1);
    let mut config = match get(run_metadata::EXECUTION_MODE).map(String::as_str) {
      Some("concurrent") => crate::FlowExecutionConfig::concurrent(max_concurrency),
      _ => crate::FlowExecutionConfig::serial(),
    };
    if let Some(limit) = get(run_metadata::COST_LIMIT_USD).and_then(|v| v.parse::<f64>().ok()) {
      config = config.with_cost_limit_usd(limit);
    }
    if let Some(dir) = get(run_metadata::RUN_BASE_DIR) {
      config = config.with_run_base_dir(PathBuf::from(dir));
    }
    config
  }
}

/// Workflow execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowStatus {
  /// Workflow is running
  Running,
  /// Workflow completed successfully
  Completed,
  /// Workflow failed
  Failed,
  /// Workflow was cancelled
  Cancelled,
}

/// V0.4: `true` when `id` is safe to embed as a single path component in a
/// checkpoint or run-directory path — used for both `workflow_id`
/// (this module) and `node_id` (`yanshi_core::flow`, which imports
/// this). Neither originates from this crate: `workflow_id` can be a
/// caller-supplied run id and `node_id` comes from a workflow author's
/// YAML `id:` field, so both must be treated as untrusted. Rejects
/// anything that could escape the intended directory once handed to a
/// real filesystem call: empty strings, `.` / `..`, and any path
/// separator (checked for both `/` and `\` regardless of host OS, since
/// a Linux-built checkpoint dir could still be inspected on Windows).
pub(crate) fn is_safe_path_component(id: &str) -> bool {
  !id.is_empty() && !id.contains('/') && !id.contains('\\') && id != "." && id != ".."
}

/// Checkpoint manager
pub struct CheckpointManager {
  config: CheckpointConfig,
}

impl CheckpointManager {
  /// Create a new checkpoint manager
  pub fn new(config: CheckpointConfig) -> Result<Self> {
    let manager = Self { config };

    // Create checkpoint directory if it doesn't exist
    std::fs::create_dir_all(&manager.config.checkpoint_dir).map_err(|e| {
      YanshiError::PersistenceError {
        message: format!("Failed to create checkpoint directory: {}", e),
      }
    })?;

    Ok(manager)
  }

  /// Save a checkpoint for a workflow
  pub async fn save_checkpoint(
    &self,
    workflow_id: &str,
    last_completed_node: &str,
    state: &HashMap<String, serde_json::Value>,
  ) -> Result<()> {
    self
      .save_checkpoint_with_status(
        workflow_id,
        last_completed_node,
        state,
        WorkflowStatus::Running,
      )
      .await
  }

  /// Save a checkpoint with specific status
  pub async fn save_checkpoint_with_status(
    &self,
    workflow_id: &str,
    last_completed_node: &str,
    state: &HashMap<String, serde_json::Value>,
    status: WorkflowStatus,
  ) -> Result<()> {
    let checkpoint = Checkpoint {
      workflow_id: workflow_id.to_string(),
      last_completed_node: last_completed_node.to_string(),
      state: state.clone(),
      created_at: Utc::now(),
      status,
      metadata: HashMap::new(),
    };

    self.save_checkpoint_struct(&checkpoint).await
  }

  /// Y2.4: save a checkpoint carrying the run's resume metadata (see
  /// [`run_metadata_for`]).
  pub async fn save_checkpoint_with_metadata(
    &self,
    workflow_id: &str,
    last_completed_node: &str,
    state: &HashMap<String, serde_json::Value>,
    status: WorkflowStatus,
    metadata: HashMap<String, String>,
  ) -> Result<()> {
    self
      .save_checkpoint_struct(&Checkpoint {
        workflow_id: workflow_id.to_string(),
        last_completed_node: last_completed_node.to_string(),
        state: state.clone(),
        created_at: Utc::now(),
        status,
        metadata,
      })
      .await
  }

  /// Save a checkpoint structure
  pub async fn save_checkpoint_struct(&self, checkpoint: &Checkpoint) -> Result<()> {
    let workflow_dir = self.get_workflow_dir(&checkpoint.workflow_id)?;
    fs::create_dir_all(&workflow_dir)
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to create workflow directory: {}", e),
      })?;

    // Generate checkpoint filename with timestamp
    let timestamp = checkpoint.created_at.format("%Y%m%d_%H%M%S_%3f");
    let filename = format!("checkpoint_{}.json", timestamp);
    let checkpoint_path = workflow_dir.join(&filename);
    let temp_path = workflow_dir.join(format!(".{}.tmp", filename));

    // Serialize checkpoint
    let json = serde_json::to_string_pretty(&checkpoint).map_err(|e| {
      YanshiError::SerializationError(format!("Failed to serialize checkpoint: {}", e))
    })?;

    // Write to temporary file (atomic operation)
    let mut file =
      fs::File::create(&temp_path)
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to create temp checkpoint file: {}", e),
        })?;

    file
      .write_all(json.as_bytes())
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to write checkpoint: {}", e),
      })?;

    file
      .sync_all()
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to sync checkpoint: {}", e),
      })?;

    drop(file);

    // Atomically rename temp file to final name
    fs::rename(&temp_path, &checkpoint_path)
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to rename checkpoint file: {}", e),
      })?;

    // Also save as "latest" for quick access
    let latest_path = workflow_dir.join("checkpoint_latest.json");
    fs::copy(&checkpoint_path, &latest_path)
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to save latest checkpoint: {}", e),
      })?;

    Ok(())
  }

  /// Load the latest checkpoint for a workflow
  pub async fn load_latest_checkpoint(&self, workflow_id: &str) -> Result<Option<Checkpoint>> {
    let latest_path = self
      .get_workflow_dir(workflow_id)?
      .join("checkpoint_latest.json");

    if !latest_path.exists() {
      return Ok(None);
    }

    self.load_checkpoint_from_path(&latest_path).await.map(Some)
  }

  /// Load all checkpoints for a workflow (sorted by timestamp, newest first)
  pub async fn load_all_checkpoints(&self, workflow_id: &str) -> Result<Vec<Checkpoint>> {
    let workflow_dir = self.get_workflow_dir(workflow_id)?;

    if !workflow_dir.exists() {
      return Ok(Vec::new());
    }

    let mut checkpoints = Vec::new();
    let mut entries =
      fs::read_dir(&workflow_dir)
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to read checkpoint directory: {}", e),
        })?;

    while let Some(entry) =
      entries
        .next_entry()
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to read directory entry: {}", e),
        })?
    {
      let path = entry.path();
      if path.is_file() && path.extension().map(|e| e == "json").unwrap_or(false) {
        let filename = path
          .file_name()
          .and_then(|n| n.to_str())
          .unwrap_or("")
          .to_string();

        // Skip latest symlink and temp files
        if filename == "checkpoint_latest.json" || filename.starts_with('.') {
          continue;
        }

        if let Ok(checkpoint) = self.load_checkpoint_from_path(&path).await {
          checkpoints.push(checkpoint);
        }
      }
    }

    // Sort by timestamp, newest first
    checkpoints.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.created_at));

    Ok(checkpoints)
  }

  /// Load checkpoint from file path
  async fn load_checkpoint_from_path(&self, path: &Path) -> Result<Checkpoint> {
    let contents = fs::read_to_string(path)
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to read checkpoint file: {}", e),
      })?;

    serde_json::from_str(&contents).map_err(|e| {
      YanshiError::SerializationError(format!("Failed to deserialize checkpoint: {}", e))
    })
  }

  /// Delete a specific checkpoint
  pub async fn delete_checkpoint(
    &self,
    workflow_id: &str,
    created_at: DateTime<Utc>,
  ) -> Result<()> {
    let timestamp = created_at.format("%Y%m%d_%H%M%S_%3f");
    let filename = format!("checkpoint_{}.json", timestamp);
    let path = self.get_workflow_dir(workflow_id)?.join(filename);

    if path.exists() {
      fs::remove_file(&path)
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to delete checkpoint: {}", e),
        })?;
    }

    // If we deleted the latest checkpoint, update the latest symlink
    let latest_path = self
      .get_workflow_dir(workflow_id)?
      .join("checkpoint_latest.json");
    if latest_path.exists() {
      // Load the latest checkpoint to check if it was the one we deleted
      if let Ok(checkpoint) = self.load_checkpoint_from_path(&latest_path).await
        && checkpoint.created_at == created_at
      {
        // The latest was deleted, find the next most recent one
        let checkpoints = self.load_all_checkpoints(workflow_id).await?;
        if let Some(newest) = checkpoints.first() {
          // Update latest symlink to point to the next checkpoint
          let timestamp = newest.created_at.format("%Y%m%d_%H%M%S_%3f");
          let filename = format!("checkpoint_{}.json", timestamp);
          let checkpoint_path = self.get_workflow_dir(workflow_id)?.join(filename);

          let _ = fs::remove_file(&latest_path).await;
          fs::copy(&checkpoint_path, &latest_path).await.ok();
        } else {
          // No more checkpoints, delete the latest symlink
          let _ = fs::remove_file(&latest_path).await;
        }
      }
    }

    Ok(())
  }

  /// Delete all checkpoints for a workflow
  pub async fn delete_all_checkpoints(&self, workflow_id: &str) -> Result<()> {
    let workflow_dir = self.get_workflow_dir(workflow_id)?;

    if workflow_dir.exists() {
      fs::remove_dir_all(&workflow_dir)
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to delete checkpoint directory: {}", e),
        })?;
    }

    Ok(())
  }

  /// Clean up old checkpoints based on retention policy
  pub async fn cleanup_old_checkpoints(&self) -> Result<usize> {
    let mut cleaned_count = 0;
    let mut entries = fs::read_dir(&self.config.checkpoint_dir)
      .await
      .map_err(|e| YanshiError::PersistenceError {
        message: format!("Failed to read checkpoint directory: {}", e),
      })?;

    while let Some(entry) =
      entries
        .next_entry()
        .await
        .map_err(|e| YanshiError::PersistenceError {
          message: format!("Failed to read directory entry: {}", e),
        })?
    {
      let path = entry.path();
      if path.is_dir() {
        let workflow_id = path
          .file_name()
          .and_then(|n| n.to_str())
          .unwrap_or("")
          .to_string();

        cleaned_count += self.cleanup_workflow_checkpoints(&workflow_id).await?;
      }
    }

    Ok(cleaned_count)
  }

  /// Clean up checkpoints for a specific workflow
  async fn cleanup_workflow_checkpoints(&self, workflow_id: &str) -> Result<usize> {
    let checkpoints = self.load_all_checkpoints(workflow_id).await?;
    let mut cleaned_count = 0;

    for checkpoint in checkpoints {
      let retention_days = match checkpoint.status {
        WorkflowStatus::Completed => self.config.success_retention_days,
        WorkflowStatus::Failed | WorkflowStatus::Cancelled => self.config.failure_retention_days,
        WorkflowStatus::Running => continue, // Keep running workflows
      };

      let age = Utc::now().signed_duration_since(checkpoint.created_at);
      if age > Duration::days(retention_days) {
        self
          .delete_checkpoint(workflow_id, checkpoint.created_at)
          .await?;
        cleaned_count += 1;
      }
    }

    Ok(cleaned_count)
  }

  /// Get workflow checkpoint directory.
  ///
  /// V0.4: fails closed if `workflow_id` isn't safe to use as a single
  /// path component. `workflow_id` is often a caller-supplied run id
  /// (e.g. from `Flow::execute_from_inputs_with_id_and_config`), not a
  /// value this crate generates — `PathBuf::join` treats an absolute
  /// `workflow_id` as replacing `checkpoint_dir` entirely, and `".."`
  /// resolves to `checkpoint_dir`'s *parent* once handed to a real
  /// filesystem call. `delete_all_checkpoints("..")` would otherwise
  /// `remove_dir_all` on that parent directory.
  fn get_workflow_dir(&self, workflow_id: &str) -> Result<PathBuf> {
    if !is_safe_path_component(workflow_id) {
      return Err(YanshiError::ConfigurationError {
        message: format!(
          "invalid workflow_id '{}': must be a single path segment (no '/', '\\', '.', or '..')",
          workflow_id
        ),
      });
    }
    Ok(self.config.checkpoint_dir.join(workflow_id))
  }

  /// Get configuration
  pub fn config(&self) -> &CheckpointConfig {
    &self.config
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  fn create_test_manager() -> (CheckpointManager, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);

    let manager = CheckpointManager::new(config).unwrap();
    (manager, temp_dir)
  }

  #[tokio::test]
  async fn test_save_and_load_checkpoint() {
    let (manager, _temp_dir) = create_test_manager();

    let mut state = HashMap::new();
    state.insert(
      "node1".to_string(),
      serde_json::json!({"result": "success"}),
    );

    manager
      .save_checkpoint("test_workflow", "node1", &state)
      .await
      .unwrap();

    let checkpoint = manager
      .load_latest_checkpoint("test_workflow")
      .await
      .unwrap()
      .unwrap();

    assert_eq!(checkpoint.workflow_id, "test_workflow");
    assert_eq!(checkpoint.last_completed_node, "node1");
    assert_eq!(checkpoint.state.len(), 1);
    assert_eq!(checkpoint.status, WorkflowStatus::Running);
  }

  #[tokio::test]
  async fn test_multiple_checkpoints() {
    let (manager, _temp_dir) = create_test_manager();

    // Create multiple checkpoints
    for i in 1..=3 {
      let mut state = HashMap::new();
      state.insert(format!("node{}", i), serde_json::json!(i));

      manager
        .save_checkpoint("test_workflow", &format!("node{}", i), &state)
        .await
        .unwrap();

      tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    let checkpoints = manager.load_all_checkpoints("test_workflow").await.unwrap();

    assert_eq!(checkpoints.len(), 3);
    // Should be sorted newest first
    assert_eq!(checkpoints[0].last_completed_node, "node3");
  }

  #[tokio::test]
  async fn test_delete_checkpoint() {
    let (manager, _temp_dir) = create_test_manager();

    let mut state = HashMap::new();
    state.insert("node1".to_string(), serde_json::json!(1));

    manager
      .save_checkpoint("test_workflow", "node1", &state)
      .await
      .unwrap();

    let checkpoint = manager
      .load_latest_checkpoint("test_workflow")
      .await
      .unwrap()
      .unwrap();

    manager
      .delete_checkpoint("test_workflow", checkpoint.created_at)
      .await
      .unwrap();

    let result = manager
      .load_latest_checkpoint("test_workflow")
      .await
      .unwrap();
    assert!(result.is_none());
  }

  #[tokio::test]
  async fn test_delete_all_checkpoints() {
    let (manager, _temp_dir) = create_test_manager();

    for i in 1..=3 {
      let mut state = HashMap::new();
      state.insert(format!("node{}", i), serde_json::json!(i));

      manager
        .save_checkpoint("test_workflow", &format!("node{}", i), &state)
        .await
        .unwrap();
    }

    manager
      .delete_all_checkpoints("test_workflow")
      .await
      .unwrap();

    let checkpoints = manager.load_all_checkpoints("test_workflow").await.unwrap();
    assert_eq!(checkpoints.len(), 0);
  }

  /// V0.4 regression: `is_safe_path_component` rejects everything that
  /// could escape a single path segment, and accepts ordinary ids.
  #[test]
  fn is_safe_path_component_rejects_traversal_and_separators() {
    assert!(is_safe_path_component("workflow-123"));
    assert!(is_safe_path_component("node_1"));
    assert!(!is_safe_path_component(""));
    assert!(!is_safe_path_component("."));
    assert!(!is_safe_path_component(".."));
    assert!(!is_safe_path_component("../escape"));
    assert!(!is_safe_path_component("nested/path"));
    assert!(!is_safe_path_component("nested\\path"));
    assert!(!is_safe_path_component("/etc/passwd"));
  }

  /// V0.4 regression: the original disaster scenario — a workflow id of
  /// `".."` must not let `delete_all_checkpoints` reach (and
  /// `remove_dir_all`) the checkpoint directory's *parent*. Before the
  /// fix, `get_workflow_dir("..")` returned `checkpoint_dir.join("..")`
  /// unchecked, which a real filesystem call resolves to the parent.
  #[tokio::test]
  async fn delete_all_checkpoints_rejects_parent_directory_traversal() {
    let (manager, temp_dir) = create_test_manager();

    // A sibling file in the checkpoint dir's parent that must survive.
    let sentinel = temp_dir.path().parent().unwrap().join(format!(
      "yanshi-checkpoint-test-sentinel-{}",
      uuid::Uuid::new_v4()
    ));
    std::fs::write(&sentinel, "must not be deleted").unwrap();

    let err = manager.delete_all_checkpoints("..").await.unwrap_err();
    assert!(
      err.to_string().contains("invalid workflow_id"),
      "expected an invalid-workflow_id error, got: {err}"
    );
    assert!(
      sentinel.exists(),
      "the checkpoint dir's parent must not have been touched"
    );
    assert!(
      temp_dir.path().exists(),
      "the checkpoint dir itself must still exist"
    );

    let _ = std::fs::remove_file(&sentinel);
  }

  /// V0.4 regression: every `CheckpointManager` entry point that takes a
  /// caller-supplied `workflow_id` fails closed on an unsafe one, rather
  /// than writing/reading/deleting outside `checkpoint_dir`.
  #[tokio::test]
  async fn checkpoint_operations_reject_unsafe_workflow_ids() {
    let (manager, _temp_dir) = create_test_manager();
    let state = HashMap::new();

    assert!(
      manager
        .save_checkpoint("../escape", "node1", &state)
        .await
        .is_err()
    );
    assert!(manager.load_latest_checkpoint("../escape").await.is_err());
    assert!(manager.load_all_checkpoints("../escape").await.is_err());
    assert!(
      manager
        .delete_checkpoint("../escape", Utc::now())
        .await
        .is_err()
    );
    assert!(manager.delete_all_checkpoints("../escape").await.is_err());
  }

  #[tokio::test]
  async fn test_checkpoint_with_status() {
    let (manager, _temp_dir) = create_test_manager();

    let mut state = HashMap::new();
    state.insert("node1".to_string(), serde_json::json!(1));

    manager
      .save_checkpoint_with_status("test_workflow", "node1", &state, WorkflowStatus::Completed)
      .await
      .unwrap();

    let checkpoint = manager
      .load_latest_checkpoint("test_workflow")
      .await
      .unwrap()
      .unwrap();

    assert_eq!(checkpoint.status, WorkflowStatus::Completed);
  }

  #[tokio::test]
  async fn test_cleanup_old_checkpoints() {
    let (manager, _temp_dir) = create_test_manager();

    // Create a checkpoint with old timestamp
    let mut checkpoint = Checkpoint {
      workflow_id: "test_workflow".to_string(),
      last_completed_node: "node1".to_string(),
      state: HashMap::new(),
      created_at: Utc::now() - Duration::days(8), // 8 days old
      status: WorkflowStatus::Completed,
      metadata: HashMap::new(),
    };

    manager.save_checkpoint_struct(&checkpoint).await.unwrap();

    // Create a recent checkpoint
    checkpoint.created_at = Utc::now();
    checkpoint.last_completed_node = "node2".to_string();
    manager.save_checkpoint_struct(&checkpoint).await.unwrap();

    let cleaned = manager.cleanup_old_checkpoints().await.unwrap();
    assert_eq!(cleaned, 1);

    let checkpoints = manager.load_all_checkpoints("test_workflow").await.unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].last_completed_node, "node2");
  }
}
