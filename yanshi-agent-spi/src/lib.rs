//! `yanshi-agent-spi` — the agent-runtime contracts of the Yanshi kernel.
//!
//! Holds the interfaces a runtime loop and its governors agree on: the
//! [`AgentRuntime`] trait, the structured
//! [`AgentStep`] / [`AgentEvent`]
//! records, [`AgentContext`], `RuntimeLimits`, the
//! cancellation token, and the event/memory hook traits.
//!
//! Extracted from `yanshi-agents` in P-A1.1 (RFC §4 agent-spi). The concrete
//! runtimes (`ReActAgent`, `PlanExecuteAgent`, supervisors) stay in
//! `yanshi-agents`, which re-exports everything here under its original
//! paths. This lets `yanshi-harness` govern a runtime by depending on the
//! contract rather than the `agents` impl crate (the P-A2.1 target).
//!
//! The [`capability`] module holds the [`Capability`]
//! contract (RFC §2): a packaged ability that `lower()`s to tools + context.
//! `yanshi-skills` implements it for a Skill so a surface can merge any
//! capability into a runtime's registry + prompt uniformly.
//!
//! The [`harness`] module holds the Harness governance contracts
//! (`HarnessEvent`, `ApprovalRequest` / `ApprovalDecision`, the hook / approval
//! / context-provider / sink traits), moved here in P-A1.1 step 2/2 so the
//! operations crates depend on the contract rather than the `yanshi-harness`
//! runtime. `yanshi-harness` re-exports them under their original paths.
//!
//! The [`checkpoint`] module holds the agent-loop-level persistent
//! checkpoint contract (V2.4): [`checkpoint::AgentLoopCheckpoint`] +
//! [`checkpoint::AgentLoopCheckpointer`], distinct from the DAG-level `Flow`
//! checkpoint in `yanshi-core`. `yanshi-agents` provides the concrete
//! file-based implementation and wires it into `ReActAgent` /
//! `PlanExecuteAgent`.

pub mod aggregation;
pub mod capability;
pub mod checkpoint;
pub mod delegation;
pub mod harness;
pub mod runtime;
pub mod schema_validation;
pub mod turn;

pub use aggregation::{AggregationReport, AnswerGroup, SubagentAnswer, aggregate_answers};
pub use capability::{Capability, CapabilityError, Lowered};
pub use checkpoint::*;
pub use delegation::{DelegationSpec, SchemaValidation, validate_output};
pub use harness::*;
pub use runtime::*;
pub use schema_validation::{validate_json_against_schema, validate_json_str_against_schema};
pub use turn::*;
