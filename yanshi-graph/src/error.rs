use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;

use crate::value::FlowValue;

/// Error category for classification and handling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCategory {
  /// Node-level errors (execution, input validation)
  Node,
  /// Workflow-level errors (definition, orchestration)
  Workflow,
  /// Network/IO errors (API calls, file operations)
  Network,
  /// Resource errors (memory, CPU, pools)
  Resource,
  /// Configuration errors
  Configuration,
  /// Internal system errors
  Internal,
}

/// Error context for better debugging and observability
#[derive(Debug, Serialize, Deserialize)]
pub struct InlineErrorContext {
  /// Node ID where error occurred (if applicable)
  pub node_id: Option<String>,
  /// Workflow ID where error occurred (if applicable)
  pub workflow_id: Option<String>,
  /// Timestamp when error occurred (ISO 8601 format)
  pub timestamp: String,
  /// Additional context key-value pairs
  pub metadata: std::collections::HashMap<String, String>,
  /// Optional cause chain (not cloneable)
  #[serde(skip)]
  pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Clone for InlineErrorContext {
  fn clone(&self) -> Self {
    Self {
      node_id: self.node_id.clone(),
      workflow_id: self.workflow_id.clone(),
      timestamp: self.timestamp.clone(),
      metadata: self.metadata.clone(),
      cause: None, // Cannot clone trait objects
    }
  }
}

impl InlineErrorContext {
  /// Create new error context
  pub fn new() -> Self {
    Self {
      node_id: None,
      workflow_id: None,
      timestamp: chrono::Utc::now().to_rfc3339(),
      metadata: std::collections::HashMap::new(),
      cause: None,
    }
  }

  /// Set node ID
  pub fn with_node_id(mut self, node_id: impl Into<String>) -> Self {
    self.node_id = Some(node_id.into());
    self
  }

  /// Set workflow ID
  pub fn with_workflow_id(mut self, workflow_id: impl Into<String>) -> Self {
    self.workflow_id = Some(workflow_id.into());
    self
  }

  /// Add metadata
  pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
    self.metadata.insert(key.into(), value.into());
    self
  }

  /// Set cause
  pub fn with_cause<E: std::error::Error + Send + Sync + 'static>(mut self, cause: E) -> Self {
    self.cause = Some(Box::new(cause));
    self
  }
}

impl Default for InlineErrorContext {
  fn default() -> Self {
    Self::new()
  }
}

impl fmt::Display for InlineErrorContext {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "[{}]", self.timestamp)?;
    if let Some(ref wf_id) = self.workflow_id {
      write!(f, " workflow={}", wf_id)?;
    }
    if let Some(ref node_id) = self.node_id {
      write!(f, " node={}", node_id)?;
    }
    if !self.metadata.is_empty() {
      write!(f, " metadata={:?}", self.metadata)?;
    }
    Ok(())
  }
}

#[derive(Error, Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum YanshiError {
  // ===== Node Errors =====
  #[error("Node execution failed: {message}")]
  NodeExecutionFailed { message: String },

  #[error("Node execution failed with partial outputs: {message}")]
  NodePartialExecutionFailed {
    message: String,
    partial_outputs: HashMap<String, FlowValue>,
  },

  #[error("Node input error: {message}")]
  NodeInputError { message: String },

  #[error("Node was skipped due to a condition.")]
  NodeSkipped,

  #[error("Node '{node_id}' was skipped because its dependency '{dependency_id}' was skipped.")]
  DependencyNotMet {
    node_id: String,
    dependency_id: String,
  },

  // ===== Workflow Errors =====
  #[error("Flow definition error: {message}")]
  FlowDefinitionError { message: String },

  #[error("Flow execution failed: {message}")]
  FlowExecutionFailed { message: String },

  #[error("Circular flow detected")]
  CircularFlow,

  #[error("Unknown transition: {action}")]
  UnknownTransition { action: String },

  // ===== Network/IO Errors =====
  #[error("Network error: {message}")]
  NetworkError { message: String },

  #[error("IO error: {message}")]
  IoError { message: String },

  #[error("Serialization error: {0}")]
  SerializationError(String),

  #[error("Persistence error: {message}")]
  PersistenceError { message: String },

  // ===== Resource Errors =====
  #[error("Resource pool exhausted: {resource_type}")]
  ResourcePoolExhausted { resource_type: String },

  #[error("Memory limit exceeded: {limit_mb}MB")]
  MemoryLimitExceeded { limit_mb: usize },

  #[error("Concurrency limit exceeded: {limit}")]
  ConcurrencyLimitExceeded { limit: usize },

  // ===== Timeout/Retry Errors =====
  #[error("Timeout exceeded after {duration_ms}ms")]
  TimeoutExceeded { duration_ms: u64 },

  /// Retry policy gave up after `attempts` failures.
  ///
  /// Q2.4.3: the variant now carries `last_error` so callers can
  /// downcast / branch on the underlying cause (network, rate
  /// limit, auth, etc.). Pre-fix every error funneled into a bare
  /// `RetryExhausted { attempts }` and the original error was
  /// thrown away by the executor.
  #[error("Retry attempts exhausted after {attempts} attempts: {last_error}")]
  RetryExhausted {
    attempts: u32,
    /// The error that the final attempt produced. `Box` keeps the
    /// enum small even though `YanshiError` is moderately sized.
    last_error: Box<YanshiError>,
  },

  // ===== Fault Tolerance Errors =====
  #[error("Circuit breaker open for node: {node_id}")]
  CircuitBreakerOpen { node_id: String },

  #[error("Rate limit exceeded: {limit} requests per {window_ms}ms")]
  RateLimitExceeded { limit: u32, window_ms: u64 },

  #[error("Load shed due to high system load")]
  LoadShed,

  // ===== Configuration Errors =====
  #[error("Configuration error: {message}")]
  ConfigurationError { message: String },

  #[error("Validation error: {0}")]
  ValidationError(String),

  // ===== System Errors =====
  #[error("Shared state error: {message}")]
  SharedStateError { message: String },

  #[error("Task cancelled")]
  TaskCancelled,

  /// Y2.3: the run's recorded spend passed its `cost_limit_usd`, so this
  /// node was not started.
  #[error("Cost limit exceeded: spent ${spent_usd:.4} of ${limit_usd:.4}")]
  CostLimitExceeded { spent_usd: f64, limit_usd: f64 },

  #[error("Async execution error: {message}")]
  AsyncExecutionError { message: String },

  #[error("Batch processing failed: {failed_items} of {total_items} items failed")]
  BatchProcessingFailed {
    failed_items: usize,
    total_items: usize,
  },

  #[error("Monitoring error: {message}")]
  MonitoringError { message: String },

  // ===== Lock Poisoning Errors =====
  #[error("Lock poisoned: {lock_type} in {location}")]
  LockPoisoned { lock_type: String, location: String },

  #[error("Generic error: {0}")]
  Generic(String),
}

impl YanshiError {
  /// Get error category for classification
  pub fn category(&self) -> ErrorCategory {
    match self {
      Self::NodeExecutionFailed { .. }
      | Self::NodePartialExecutionFailed { .. }
      | Self::NodeInputError { .. }
      | Self::NodeSkipped
      | Self::DependencyNotMet { .. } => ErrorCategory::Node,

      Self::FlowDefinitionError { .. }
      | Self::FlowExecutionFailed { .. }
      | Self::CircularFlow
      | Self::UnknownTransition { .. } => ErrorCategory::Workflow,

      Self::NetworkError { .. }
      | Self::IoError { .. }
      | Self::SerializationError(_)
      | Self::PersistenceError { .. } => ErrorCategory::Network,

      Self::ResourcePoolExhausted { .. }
      | Self::MemoryLimitExceeded { .. }
      | Self::ConcurrencyLimitExceeded { .. } => ErrorCategory::Resource,

      Self::ConfigurationError { .. } => ErrorCategory::Configuration,

      Self::ValidationError(_) => ErrorCategory::Configuration,

      _ => ErrorCategory::Internal,
    }
  }

  /// Check if error is retryable
  pub fn is_retryable(&self) -> bool {
    matches!(
      self,
      Self::NetworkError { .. }
        | Self::IoError { .. }
        | Self::TimeoutExceeded { .. }
        | Self::ResourcePoolExhausted { .. }
        | Self::RateLimitExceeded { .. }
        | Self::LoadShed
        | Self::AsyncExecutionError { .. }
    )
  }

  /// Check if error is transient (temporary)
  pub fn is_transient(&self) -> bool {
    matches!(
      self,
      Self::TimeoutExceeded { .. }
        | Self::RateLimitExceeded { .. }
        | Self::LoadShed
        | Self::ResourcePoolExhausted { .. }
    )
  }

  /// Create error with context
  pub fn with_context(self, context: InlineErrorContext) -> ContextualError {
    ContextualError {
      error: self,
      context,
    }
  }
}

/// Error with context for better debugging
#[derive(Debug)]
pub struct ContextualError {
  pub error: YanshiError,
  pub context: InlineErrorContext,
}

impl fmt::Display for ContextualError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{} {}", self.error, self.context)?;
    if let Some(ref cause) = self.context.cause {
      write!(f, "\nCaused by: {}", cause)?;
    }
    Ok(())
  }
}

impl std::error::Error for ContextualError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    self
      .context
      .cause
      .as_ref()
      .map(|e| e.as_ref() as &dyn std::error::Error)
  }
}

pub type Result<T> = std::result::Result<T, YanshiError>;

// ===== Error Conversions =====

impl From<serde_json::Error> for YanshiError {
  fn from(err: serde_json::Error) -> Self {
    YanshiError::SerializationError(err.to_string())
  }
}

impl From<std::io::Error> for YanshiError {
  fn from(err: std::io::Error) -> Self {
    YanshiError::IoError {
      message: err.to_string(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_yanshi_error_creation() {
    let error = YanshiError::NodeExecutionFailed {
      message: "Test error".to_string(),
    };
    assert_eq!(error.to_string(), "Node execution failed: Test error");
  }

  #[test]
  fn test_error_chaining() {
    let json_result: std::result::Result<serde_json::Value, serde_json::Error> =
      serde_json::from_str("{invalid");
    let inner_error = json_result.unwrap_err();
    let error = YanshiError::from(inner_error);
    assert!(error.to_string().contains("Serialization error"));
  }

  #[test]
  fn test_retry_exhausted_error() {
    // Q2.4.3: the variant carries the underlying cause so callers
    // can downcast / branch on the actual root error.
    let inner = YanshiError::NodeExecutionFailed {
      message: "connection refused".into(),
    };
    let error = YanshiError::RetryExhausted {
      attempts: 3,
      last_error: Box::new(inner),
    };
    let rendered = error.to_string();
    assert!(rendered.contains("Retry attempts exhausted after 3 attempts"));
    assert!(rendered.contains("connection refused"));
  }

  #[test]
  fn test_error_category() {
    assert_eq!(
      YanshiError::NodeExecutionFailed {
        message: "test".into()
      }
      .category(),
      ErrorCategory::Node
    );
    assert_eq!(
      YanshiError::NetworkError {
        message: "test".into()
      }
      .category(),
      ErrorCategory::Network
    );
    assert_eq!(
      YanshiError::ResourcePoolExhausted {
        resource_type: "test".into()
      }
      .category(),
      ErrorCategory::Resource
    );
  }

  #[test]
  fn test_is_retryable() {
    assert!(
      YanshiError::NetworkError {
        message: "test".into()
      }
      .is_retryable()
    );
    assert!(YanshiError::TimeoutExceeded { duration_ms: 1000 }.is_retryable());
    assert!(
      !YanshiError::ConfigurationError {
        message: "test".into()
      }
      .is_retryable()
    );
    assert!(!YanshiError::ValidationError("test".into()).is_retryable());
  }

  #[test]
  fn test_is_transient() {
    assert!(YanshiError::TimeoutExceeded { duration_ms: 1000 }.is_transient());
    assert!(
      YanshiError::RateLimitExceeded {
        limit: 100,
        window_ms: 1000
      }
      .is_transient()
    );
    assert!(!YanshiError::ValidationError("test".into()).is_transient());
  }

  #[test]
  fn test_error_context() {
    let context = InlineErrorContext::new()
      .with_node_id("test_node")
      .with_workflow_id("test_workflow")
      .with_metadata("key", "value");

    assert_eq!(context.node_id, Some("test_node".to_string()));
    assert_eq!(context.workflow_id, Some("test_workflow".to_string()));
    assert_eq!(context.metadata.get("key"), Some(&"value".to_string()));
  }

  #[test]
  fn test_contextual_error() {
    let error = YanshiError::NodeExecutionFailed {
      message: "test failure".into(),
    };
    let context = InlineErrorContext::new().with_node_id("node1");
    let contextual = error.with_context(context);

    let error_str = contextual.to_string();
    assert!(error_str.contains("Node execution failed"));
    assert!(error_str.contains("node1"));
  }

  #[test]
  fn test_error_context_display() {
    let context = InlineErrorContext::new()
      .with_node_id("test_node")
      .with_workflow_id("test_wf")
      .with_metadata("attempt", "3");

    let display = format!("{}", context);
    assert!(display.contains("test_node"));
    assert!(display.contains("test_wf"));
    assert!(display.contains("attempt"));
  }
}
