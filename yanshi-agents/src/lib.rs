//! Yanshi Agents - the agent-native runtimes (ReAct, Plan-Execute,
//! supervisors, dynamic workflows) and their DAG composition helpers.
//!
//! ## ReAct Agent (Phase 1)
//! Use [`react::ReActAgent`] for autonomous Thought/Action/Observation loops.

pub mod checkpoint;
pub mod citation;
pub mod delegation;
pub mod dynamic;
pub mod eval;
pub mod nodes;
pub mod plan_execute;
pub mod project_memory;
pub mod react;
pub mod reflection;
pub mod task_summary;
pub mod verification;
// The agent-runtime contracts moved to `yanshi-agent-spi` (P-A1.1).
// Re-export under the original `yanshi_agents::runtime` path so every
// consumer — and this crate's own runtimes — keep compiling unchanged.
pub use yanshi_agent_spi::runtime;
pub mod supervisor;
pub mod token_counter_adapter;
pub mod tools;

pub use checkpoint::FileLoopCheckpointer;

// Re-export core Yanshi types for convenience
pub use yanshi_graph::{AsyncNode, YanshiError};
pub use yanshi_llm::Yanshi;

// Re-export MCP utilities
pub use yanshi_mcp::client::MCPClient;
pub use yanshi_mcp::tools::{ToolCall, ToolRegistry as McpToolRegistry};

// Re-export new Phase-1 building blocks
pub use yanshi_memory;
pub use yanshi_tool;

// Re-export M3 multi-agent building blocks
pub use nodes::{
  AgentNode, AgentNodeResumeContract, AgentNodeResumeMode, AgentNodeToolReplayPolicy,
  AgentNodeToolResumeRecord,
};
pub use plan_execute::{PlanExecuteAgent, PlanExecuteConfig, PlanExecuteError, PlanExecuteStep};
pub use project_memory::{
  DeterministicProjectFactGenerator, ProjectFactCandidate, ProjectFactGenerator,
};
pub use react::{
  CompactMemorySummary, LoopDetectionConfig, MemorySummaryBackend, MemorySummaryContext,
  MemorySummaryStrategy, RecentOnlyMemorySummary,
};
pub use reflection::{
  FailureReflection, FinalReflection, NoOpReflection, Reflection, ReflectionContext,
  ReflectionError, ReflectionStrategy, ReflectionTrigger,
};
pub use runtime::{
  AgentCancellationToken, AgentContext, AgentEvent, AgentMemoryHook, AgentRunResult, AgentRuntime,
  AgentRuntimeError, AgentStep, AgentStepKind, AgentStopReason, BlackboardOpKind,
  MemoryHookContext, MemoryHookKind, RuntimeLimits,
};
pub use supervisor::{Supervisor, SupervisorBuilder};
pub use task_summary::{
  DeterministicTaskSummaryGenerator, TaskSummaryContext, TaskSummaryGenerator,
};
pub use tools::{AgentTool, WorkflowTool};
pub use verification::{
  AlwaysApprove, VerificationContext, VerificationError, VerificationOutcome, VerificationStrategy,
};

// Common result type for agents
pub type AgentResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(test)]
pub(crate) static LLM_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
