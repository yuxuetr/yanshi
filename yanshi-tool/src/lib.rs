//! # yanshi-tool
//!
//! The `Tool` contract for Yanshi (T3.3, `docs/RFC_TOOL_CONTRACT_SPLIT.md`):
//! the `Tool` trait, `ToolRegistry`, `ToolMetadata`, `Capability`,
//! `ToolPolicy`, `SecurityProfile`, and the `SandboxBackend` trait + its
//! DTOs (`SandboxScope`/`SandboxStatus`/`SandboxEnforcement`/`SandboxError`).
//!
//! No concrete tool implementation lives here — those (`ShellTool`,
//! `FileTool`, `HttpTool`, `ScriptTool`, `CodeExecTool`) and the concrete
//! OS-sandbox backends live in `yanshi-tools`, which depends on this
//! crate and re-exports it in full. A runtime (`yanshi-agents`,
//! `yanshi-harness`) that only needs to accept/report on tools — never
//! to construct a concrete one — should depend on this crate directly
//! rather than on `yanshi-tools`.

pub mod capability;
pub mod error;
pub mod plugin_policy;
pub mod policy;
pub mod registry;
pub mod sandbox;
pub mod security_profile;
pub mod tool;

pub use capability::{Capability, CapabilityDecisionEntry, EffectiveCapabilities, GrantSource};
pub use error::ToolError;
pub use plugin_policy::{
  PluginEvaluationInput, PluginNetworkPolicy, PluginPolicy, PluginPolicyDecision,
};
pub use policy::{ToolPolicy, ToolPolicyDecision};
pub use registry::ToolRegistry;
pub use sandbox::{SandboxBackend, SandboxEnforcement, SandboxError, SandboxScope, SandboxStatus};
pub use security_profile::{
  AuthDefaults, CorsDefaults, CorsMode, MarketplaceInstallDefaults, PluginExecutionDefaults,
  RequestLimitDefaults, SECURITY_PROFILE_ENV, SandboxingDefaults, SecurityProfile,
  SecurityProfileDefaults, SecurityProfileError, ToolPermissionDefaults, WorkerAdmissionDefaults,
};
pub use tool::{
  Tool, ToolCall, ToolDefinition, ToolEffect, ToolIdempotency, ToolMetadata, ToolOutput,
  ToolOutputPart, ToolPermission, ToolPermissionSet, ToolSource,
};
