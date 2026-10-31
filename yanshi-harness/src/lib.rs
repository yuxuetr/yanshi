//! Yanshi Harness Agent Mode — contract crate.
//!
//! Phase H0 (`P-H.0` in `TODOs.md`) deliverable: this crate freezes the
//! public contract surface that the Harness runtime (Phase H1+) will fill
//! in. It is intentionally **contract-only**: no execution logic, no
//! orchestration, no platform side effects. That keeps the freeze
//! reviewable and lets downstream UIs / SDKs depend on the envelopes
//! before the runtime lands.
//!
//! The Harness Mode design is documented in [`docs/HARNESS_MODE.md`] and
//! the longer rationale lives in `docs/archive/HARNESS_MODE_EVOLUTION.md`. The
//! stability tier for every type in this crate is **experimental** until
//! Phase H1 exercises them end-to-end (see `docs/STABILITY.md`).
//!
//! Surface summary:
//!
//! - [`HarnessEvent`] — line-delimited JSON envelope streamed by
//!   `yanshi harness run --output stream-json`. Wraps all in-session
//!   activity. Closed `tag = "kind", content = "payload"` enum so trace
//!   replay can decode it without per-version branching.
//! - [`ApprovalRequest`] / [`ApprovalDecision`] — runtime approval
//!   protocol. Decoupled from any UI; CLI/server/UI all consume the same
//!   envelope.
//! - [`ContextProvider`], [`PreToolHook`], [`PostToolHook`],
//!   [`ApprovalProvider`] — async trait boundaries the runtime composes
//!   to assemble project context and to govern tool execution.
//! - [`HarnessContext`] / [`HarnessProfile`] / [`HarnessRuntimeKind`] —
//!   session-scoped descriptor passed to providers and hooks.
//!
//! [`docs/HARNESS_MODE.md`]: ../../docs/HARNESS_MODE.md

pub mod approval;
pub mod approval_providers;
pub mod compaction;
pub mod context;
pub mod error;
pub mod event;
pub mod flow_run;
pub mod hooks;
pub mod hooks_runtime;
pub mod params_summary;
pub mod persistence;
pub mod providers;
pub mod runtime;
pub mod seq;
pub mod tasks;
pub mod tracing_bridge;

pub use approval::{
  ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalRisk, ApprovalScope,
};
pub use approval_providers::{
  AutoAllowApprovalProvider, AutoDenyApprovalProvider, CliApprovalProvider,
};
pub use compaction::{ContextSummarizer, DeterministicContextSummarizer};
pub use context::{
  ContextItem, ContextPriority, ContextProvider, HarnessContext, HarnessProfile, HarnessRuntimeKind,
};
pub use error::HarnessError;
pub use event::{
  ApprovalDecidedPayload, ApprovalRequestedPayload, BackgroundTaskStatus,
  BackgroundTaskUpdatedPayload, HarnessEvent, HarnessEventBody, MemorySummaryAddedPayload,
  SessionStartedPayload, StepStartedPayload, StopReason, StoppedPayload, TokenDeltaPayload,
  ToolCallCompletedPayload, ToolCallRequestedPayload,
};
pub use flow_run::{FlowRunOutcome, HarnessFlowRunOptions, HarnessFlowRunResult};
pub use hooks::{CompletedToolCall, PendingToolCall, PostToolHook, PreToolDecision, PreToolHook};
pub use hooks_runtime::{
  DEFAULT_APPROVAL_TIMEOUT, DEFAULT_HOOK_TIMEOUT, HookConfig, HookedTool, wrap_registry,
};
pub use persistence::{
  HarnessEventSink, InMemoryEventSink, JsonlEventSink, SinkChain, StdoutEventSink,
  default_session_dir,
};
pub use providers::{
  AgentsMdProvider, DEFAULT_DOC_CHAR_CAP, RoadmapMdProvider, TodosMdProvider,
  WorkspaceLayoutProvider, default_providers,
};
pub use runtime::{HarnessRunOptions, HarnessRunResult, HarnessRuntime};
pub use seq::{Seq, SeqAllocator};
pub use tasks::{
  DEFAULT_MAX_OUTPUT_BYTES, TaskAgentBundle, TaskAgentFactory, TaskCreateTool, TaskGetTool,
  TaskHandle, TaskListTool, TaskOutputSnapshot, TaskOutputTool, TaskRuntime, TaskSpec, TaskStatus,
  TaskStopTool, TaskWriter, task_tools,
};
pub use tracing_bridge::{YANSHI_TRACE_DIR_ENV, open_tracing_sink, resolve_trace_session_dir};

/// Crate version exposed for diagnostics; matches `Cargo.toml`.
pub const HARNESS_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Stable envelope identifier emitted by tracers and CLI consumers to
/// detect schema drift. Bump on any breaking change to the
/// [`HarnessEvent`] / [`ApprovalRequest`] / [`ApprovalDecision`] wire
/// shapes; additive changes (new optional fields, new kinds) keep the
/// same value.
pub const HARNESS_ENVELOPE_SCHEMA_VERSION: &str = "harness/1";
