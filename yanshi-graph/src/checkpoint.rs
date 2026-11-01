//! Checkpoint *configuration* — the data a `Flow` carries to describe how it
//! should be checkpointed. The checkpoint *manager* (the IO logic that reads /
//! writes / prunes checkpoints) lives in `yanshi-core`; only this config
//! type belongs in the IR because the `Flow` struct holds it (P-A1.3 step 2).

use std::path::PathBuf;

/// Checkpoint configuration.
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
  /// Directory to store checkpoints
  pub checkpoint_dir: PathBuf,

  /// Retention period for successful workflows (days)
  pub success_retention_days: i64,

  /// Retention period for failed workflows (days)
  pub failure_retention_days: i64,

  /// Enable automatic cleanup on startup
  pub auto_cleanup: bool,

  /// Compress checkpoints
  pub compression: bool,
}

impl Default for CheckpointConfig {
  fn default() -> Self {
    Self {
      checkpoint_dir: std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yanshi")
        .join("checkpoints"),
      success_retention_days: 7,
      failure_retention_days: 30,
      auto_cleanup: true,
      compression: false,
    }
  }
}

impl CheckpointConfig {
  /// Set checkpoint directory
  pub fn with_checkpoint_dir(mut self, dir: impl Into<PathBuf>) -> Self {
    self.checkpoint_dir = dir.into();
    self
  }

  /// Set retention days for successful workflows
  pub fn with_success_retention_days(mut self, days: i64) -> Self {
    self.success_retention_days = days;
    self
  }

  /// Set retention days for failed workflows
  pub fn with_failure_retention_days(mut self, days: i64) -> Self {
    self.failure_retention_days = days;
    self
  }

  /// Set retention days for both success and failure
  pub fn with_retention_days(mut self, days: i64) -> Self {
    self.success_retention_days = days;
    self.failure_retention_days = days;
    self
  }

  /// Enable/disable automatic cleanup
  pub fn with_auto_cleanup(mut self, enabled: bool) -> Self {
    self.auto_cleanup = enabled;
    self
  }

  /// Enable/disable compression
  pub fn with_compression(mut self, enabled: bool) -> Self {
    self.compression = enabled;
    self
  }
}
