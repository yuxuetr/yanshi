//! Yanshi Core - V2
//!
//! This crate provides the fundamental building blocks for the V2 Yanshi architecture.

// Core abstractions.
//
// The execution IR (`async_node` / `expr` / `error`) moved to the
// `yanshi-graph` crate (P-A1.3, IR ≠ executor per RFC §5). Re-export each
// under its original `yanshi_core::<module>` path so every existing
// `crate::async_node::AsyncNode` / `yanshi_core::YanshiError` consumer —
// inside core and downstream — keeps compiling unchanged. The `Flow` orchestrator
// + scheduler stay here for now (sub-step 2 moves the `Flow` *type* to graph).
// (The sync `node` module / `Node` trait that used to live alongside these —
// superseded by `AsyncNode` before this crate split even happened, zero real
// callers — was deleted in W5.2.)
pub use yanshi_graph::{async_node, error, expr};
pub mod flow;

// `FlowValue` lives in the `yanshi-value` leaf crate (P-A1.5); also re-exported
// transitively by `yanshi-graph`. Surface it under the original
// `yanshi_core::value` module path + crate root for backward compatibility.
pub use yanshi_value as value;

// Execution engine
pub mod health;
// `retry` + `timeout` combinators moved to `yanshi-async-util` (P-A1.4);
// `race_with_limits` added there (P-A3.2). Re-export under their original
// `yanshi_core::{retry,timeout}` paths (+ `race`). `retry_executor` stays
// here (`Flow`'s own retry logic depends on it via `execute_with_retry_and_hook`).
pub use yanshi_async_util::{RaceOutcome, race, race_with_limits, retry, timeout};
pub mod retry_executor;

// The `FlowRunner` contract lives in the `graph` IR crate; `CoreFlowRunner` is
// the executor-backed impl. Re-export both so surfaces inject one without an
// extra `yanshi-graph` import.
pub mod runner;
pub use runner::CoreFlowRunner;
pub use yanshi_graph::FlowRunner;

// Reliability
pub mod checkpoint;
// W5.3: `resource_manager` (pure facade over the two modules below) and
// `state_monitor` (its one real capability, LRU eviction, is unsafe for
// `Flow`'s state pool — a node's output can be a real dependency for any
// later node regardless of recency, unlike a cache) and `concurrency`
// (redundant with `Flow`'s own `FuturesUnordered`+`max_concurrency` DAG
// dispatch and its separate ad-hoc `Semaphore` for Map fan-out) were all
// deleted — verified-zero real callers, and (for `state_monitor`)
// actively unsafe to wire in as designed. `resource_limits` survives:
// its `ResourceLimits` predicates are pure/safe and are now wired into
// `Flow` as an advisory `WorkflowEvent::ResourceWarning` (see
// `FlowExecutionConfig::resource_limits`, `flow.rs::notify_state_size`).
pub mod resource_limits;
pub mod resume;
// Y1.1: per-run artifact root visible to nodes via a task-local.
pub mod run_context;
pub mod scheduler;

// `state_size` (StateSizeObserver) and `events` (EventListener / WorkflowEvent)
// moved to `yanshi-graph` (P-A1.3 step 2): they are the observability
// *contracts* a `Flow` holds, so the IR crate must own them. Re-exported here
// under their original `yanshi_core::*` paths. The event drain/dispatch
// *logic* (where it exists) stays in core.
pub use yanshi_graph::{events, state_size};

// Q5.3: shared SIGINT/SIGTERM shutdown handling used by the CLI,
// server, and worker binaries.
pub mod shutdown;

// Plugin runtime (subprocess-based; gated behind the `plugin` feature)
#[cfg(feature = "plugin")]
pub mod plugin;

// Core traits and types
pub use async_node::AsyncNode;
pub use checkpoint::{Checkpoint, CheckpointConfig, CheckpointManager, WorkflowStatus};
pub use error::{Result, YanshiError};
pub use events::{ConsoleListener, EventListener, MultiListener, NoOpListener, WorkflowEvent};
pub use flow::{Flow, FlowExt, GraphNode, NodeType};
pub use health::{HealthChecker, HealthReport, HealthStatus};
pub use resource_limits::ResourceLimits;
pub use resume::{
  RESUME_PLAN_SCHEMA_VERSION, ResumeDecision, ResumeIdempotency, ResumePlan, ResumePlanOptions,
  ResumeSummary, ResumeToolCall, build_resume_plan,
};
pub use retry::{ErrorPattern, RetryContext, RetryPolicy, RetryStrategy};
pub use retry_executor::{execute_with_retry, execute_with_retry_and_hook};
pub use scheduler::{FlowCancellationToken, FlowExecutionConfig, FlowExecutionMode};
pub use state_size::{StateSizeObserver, estimated_state_pool_bytes};
pub use value::FlowValue;
