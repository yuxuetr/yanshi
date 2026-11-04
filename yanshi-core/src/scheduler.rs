use crate::resource_limits::ResourceLimits;
use crate::retry::RetryPolicy;
use std::{
  path::PathBuf,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowExecutionMode {
  Serial,
  Concurrent,
}

// V1.4: `Eq` dropped (kept `PartialEq`) once `node_retry_policy` was
// added -- `RetryPolicy` only supports `PartialEq` (it nests an `f64`
// backoff multiplier). Nothing in the workspace relied on `Eq`
// specifically (no `HashSet<FlowExecutionConfig>` etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct FlowExecutionConfig {
  pub mode: FlowExecutionMode,
  pub max_concurrency: usize,
  pub fail_fast: bool,
  pub continue_on_skip: bool,
  pub run_base_dir: Option<PathBuf>,
  pub cancellation_token: Option<FlowCancellationToken>,
  /// V1.4: optional Flow-wide timeout applied to every node's execution
  /// (`Standard`/`Map`/`While` alike), inside the executor itself.
  /// `None` (default) is the pre-V1.4 behavior: no Flow-level timeout.
  /// Complements (does not replace) `yanshi-config`'s per-node YAML
  /// `timeout_ms` (T3.1's `TimeoutRetryNode`), which wraps a node
  /// *before* it ever reaches this executor and so has no visibility
  /// into `WorkflowEvent`.
  pub node_timeout_ms: Option<u64>,
  /// V1.4: optional Flow-wide retry policy applied to every node's
  /// execution on a transient failure (network / timeout /
  /// rate-limit-class, per [`RetryPolicy`]'s default classification).
  /// `None` (default) is the pre-V1.4 behavior: no Flow-level retry.
  /// Every attempt emits `WorkflowEvent::RetryAttempt` (see
  /// `FlowExecutor::execute_node_with_reliability` in `flow.rs`), unlike
  /// `yanshi-config`'s per-node YAML `max_retries`, which retries
  /// silently from the executor's point of view.
  pub node_retry_policy: Option<RetryPolicy>,
  /// W5.3: optional Flow-wide advisory state-pool size limit. `None`
  /// (default) is unchanged pre-W5.3 behavior: no limit checked. When
  /// set, every `notify_state_size` call compares the current estimated
  /// state pool size (already computed for the passive
  /// `StateSizeObserver`) against [`ResourceLimits::exceeds_state_limit`]
  /// and emits `WorkflowEvent::ResourceWarning` when exceeded. This is
  /// advisory-only — no eviction, no rejection, nothing is ever removed
  /// from the state pool. A prior wire-in attempt (`state_monitor.rs`'s
  /// LRU eviction, removed in W5.3) was unsafe: a node's output can be a
  /// real dependency for any later node via `input_mapping` regardless of
  /// how recently it was "accessed", unlike a cache entry.
  pub resource_limits: Option<ResourceLimits>,
}

impl FlowExecutionConfig {
  /// The base directory each run's `<run_id>/` dir is created under:
  /// `run_base_dir` when set, else `~/.yanshi/runs`.
  pub fn resolved_run_base_dir(&self) -> crate::Result<PathBuf> {
    match &self.run_base_dir {
      Some(dir) => Ok(dir.clone()),
      None => dirs::home_dir()
        .map(|home| home.join(".yanshi").join("runs"))
        .ok_or_else(|| crate::YanshiError::ConfigurationError {
          message: "Could not find home directory".to_string(),
        }),
    }
  }

  pub fn serial() -> Self {
    Self::default()
  }

  pub fn concurrent(max_concurrency: usize) -> Self {
    Self {
      mode: FlowExecutionMode::Concurrent,
      max_concurrency: max_concurrency.max(1),
      fail_fast: true,
      continue_on_skip: true,
      run_base_dir: None,
      cancellation_token: None,
      node_timeout_ms: None,
      node_retry_policy: None,
      resource_limits: None,
    }
  }

  pub fn with_run_base_dir(mut self, run_base_dir: impl Into<PathBuf>) -> Self {
    self.run_base_dir = Some(run_base_dir.into());
    self
  }

  pub fn with_cancellation_token(mut self, token: FlowCancellationToken) -> Self {
    self.cancellation_token = Some(token);
    self
  }

  /// V1.4: opt into a Flow-wide per-node timeout.
  pub fn with_node_timeout_ms(mut self, timeout_ms: u64) -> Self {
    self.node_timeout_ms = Some(timeout_ms);
    self
  }

  /// V1.4: opt into a Flow-wide per-node retry policy.
  pub fn with_node_retry_policy(mut self, policy: RetryPolicy) -> Self {
    self.node_retry_policy = Some(policy);
    self
  }

  /// W5.3: opt into an advisory Flow-wide state-pool size limit.
  pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
    self.resource_limits = Some(limits);
    self
  }
}

impl Default for FlowExecutionConfig {
  fn default() -> Self {
    Self {
      mode: FlowExecutionMode::Serial,
      max_concurrency: 1,
      fail_fast: true,
      continue_on_skip: true,
      run_base_dir: None,
      cancellation_token: None,
      node_timeout_ms: None,
      node_retry_policy: None,
      resource_limits: None,
    }
  }
}

/// Process-local cancellation signal for Flow execution.
#[derive(Debug, Clone)]
pub struct FlowCancellationToken {
  cancelled: Arc<AtomicBool>,
}

impl FlowCancellationToken {
  pub fn new() -> Self {
    Self {
      cancelled: Arc::new(AtomicBool::new(false)),
    }
  }

  pub fn cancel(&self) {
    self.cancelled.store(true, Ordering::SeqCst);
  }

  pub fn is_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::SeqCst)
  }
}

impl Default for FlowCancellationToken {
  fn default() -> Self {
    Self::new()
  }
}

impl PartialEq for FlowCancellationToken {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.cancelled, &other.cancelled) || self.is_cancelled() == other.is_cancelled()
  }
}

impl Eq for FlowCancellationToken {}
