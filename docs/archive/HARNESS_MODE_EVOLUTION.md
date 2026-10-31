# Harness Mode Evolution Assessment

Last updated: 2026-05-10

## Executive Summary

This document evaluates how Yanshi can evolve to support a Harness Agent
mode. The goal is not to clone OpenHarness. The goal is to absorb the useful
runtime pattern behind Harness-style agents: a long-lived, tool-using,
workspace-aware, governable, resumable agent session that can coordinate
skills, tools, memory, background tasks, and multi-agent collaboration.

Overall difficulty: **medium, about 5.5 / 10** for a practical Yanshi-native
Harness Mode.

Difficulty rises to **7.5 / 10 or higher** only if the target expands into a
full OpenHarness-like product shell with rich TUI, slash command ecosystem,
provider subscription bridges, plugin marketplace compatibility, and
multi-channel personal-assistant surfaces.

Recommended positioning:

> Yanshi Harness Mode should be a Rust-native intelligent work-session layer
> built on Yanshi's existing DAG, AgentRuntime, ToolRegistry, Skill, MCP,
> memory, tracing, checkpoint, and server foundations. It should not become a
> parallel framework or a UI-first clone of OpenHarness.

## Reference Model

OpenHarness describes itself as a full LLM agent harness with a streaming
tool-call cycle, parallel tool execution, Skills, plugins, memory, hooks,
interactive approvals, subagents, task lifecycle, session resume, and CLI/TUI
operation.

Reference:

- OpenHarness repository: <https://github.com/HKUDS/OpenHarness>

For Yanshi, the important part is the architectural pattern:

- The agent does not just answer a prompt.
- The agent operates inside a workspace.
- The agent sees durable project and session context.
- The agent uses tools under explicit policy.
- The agent can ask for approval before risky actions.
- The agent can delegate or run subtasks in the background.
- The agent can stream structured progress.
- The agent can recover from interruption.

## Current Yanshi Fit

Yanshi already has many of the lower-level primitives needed for this mode.
The missing work is mostly an integration layer and a stronger interactive
runtime protocol.

### Existing Strengths

#### Agent Runtime

Yanshi already has a shared `AgentRuntime` abstraction with:

- `AgentContext`
- `RuntimeLimits`
- cancellation token
- structured `AgentStep`
- structured `AgentEvent`
- `AgentStopReason`
- `AgentRunResult`

This is the right foundation for Harness Mode because it makes agent execution
serializable, inspectable, and composable.

Relevant files:

- `yanshi-agents/src/runtime.rs`
- `yanshi-agents/src/react/agent.rs`
- `yanshi-agents/src/plan_execute.rs`

#### Tool System

Yanshi already has a central `ToolRegistry` and tool contract:

- JSON-schema-like tool parameters
- typed tool output parts
- tool metadata
- tool source classification
- permission categories
- idempotency classification
- policy audit log
- capability audit log

Relevant files:

- `yanshi-tools/src/tool.rs`
- `yanshi-tools/src/registry.rs`
- `yanshi-tools/src/policy.rs`
- `yanshi-tools/src/capability.rs`

This maps well to Harness-style tool governance.

#### Security And Governance

Yanshi has already started moving toward explicit tool governance:

- tool allow-list policy
- permission allow-list policy
- capability merge across tool requirements, skill policy, tool policy, and
  CLI grants
- sandbox policy
- idempotency metadata for replay safety
- audit events for policy and capability decisions

Relevant roadmap areas:

- `TODOs.md` P1 Security And Tool Governance
- `RoadMap.md` P1 Security And Tool Governance

This is stronger than a basic agent loop, but it still needs an approval
protocol and hook system to become Harness-grade.

#### Skills

Yanshi supports `SKILL.md` parsing and converts skills into internal
manifests. It also supports Yanshi-specific extensions for MCP servers and
security controls.

Relevant files:

- `yanshi-skills/src/skill_md.rs`
- `yanshi-skills/src/builder.rs`
- `yanshi-skills/src/manifest.rs`

This gives Yanshi a natural path to on-demand capability packages.

#### MCP And Plugins

Yanshi has MCP client integration, MCP nodes, MCP CLI commands, and Skill
MCP attachment. It also has a plugin/custom node foundation using subprocess
JSON-RPC.

This matters because Harness Mode should not hard-code all tools. The runtime
should assemble tools from multiple capability sources:

- built-in tools
- MCP tools
- workflow tools
- agent tools
- plugin tools
- script tools where policy allows them

#### Multi-Agent Collaboration

Yanshi already supports three multi-agent collaboration patterns:

- handoff
- blackboard
- debate

These are stronger foundations than a single subagent abstraction because they
cover routing, shared-artifact collaboration, and independent verification.

Relevant files:

- `docs/MULTI_AGENT.md`
- `yanshi-agents/src/supervisor/handoff.rs`
- `yanshi-agents/src/supervisor/blackboard.rs`
- `yanshi-agents/src/supervisor/debate.rs`

#### Checkpoint, Resume, And Trace

Yanshi already has workflow checkpointing and partial ReAct resume support.
The current ReAct resume strategy restores durable observations and refuses
unsafe unresolved tool calls unless replay is safe.

This is aligned with Harness Mode, where long-lived sessions must survive
interruptions and avoid silently repeating side-effecting tools.

#### Server, SSE, And Web UI Foundation

Yanshi already has:

- `/v1/runs`
- run cancellation
- event history
- SSE streaming
- run graph endpoint
- embedded Web UI debugger

Relevant files:

- `yanshi-server/src/lib.rs`
- `yanshi-server/src/runs.rs`
- `yanshi-server/src/events_stream.rs`
- `docs/WEB_UI.md`

This can become the control plane for Harness sessions without replacing the
CLI-first and SDK-first model.

## Gap Analysis

### Gap 1: No First-Class Harness Session

Current Yanshi has workflows, agent runs, skills, and server runs. It does
not yet have a first-class long-lived work session that coordinates all of
them.

Needed abstraction:

```text
HarnessSession
  session_id
  workspace_root
  user_input
  selected_model
  selected_runtime
  loaded_skills
  loaded_tools
  memory_policy
  tool_policy
  approval_policy
  event_sink
  checkpoint_policy
```

This should be an orchestration layer over existing crates, not a replacement
for `AgentRuntime`.

Difficulty: **medium**.

### Gap 2: Hook Bus Is Too Narrow

Yanshi currently has memory hooks and rich trace events, but it does not
have a generic runtime hook bus comparable to Harness-style hooks.

Needed hooks:

- `SessionStart`
- `SessionStop`
- `PreLLMCall`
- `PostLLMCall`
- `PreToolUse`
- `PostToolUse`
- `ToolDenied`
- `ApprovalRequested`
- `ApprovalResolved`
- `TaskCreated`
- `TaskStopped`
- `CheckpointSaved`

Hooks should be non-invasive by default, but some hooks such as `PreToolUse`
must be able to block or request approval.

Difficulty: **medium**.

### Gap 3: Interactive Approval Protocol

Yanshi has policy decisions and capability decisions, but not a runtime
approval loop.

Harness Mode needs a protocol that can work across:

- direct CLI execution
- stream-json CLI execution
- local server execution
- Web UI debugger
- future TUI

Suggested data model:

```rust
pub struct ApprovalRequest {
  pub id: String,
  pub session_id: String,
  pub step_index: usize,
  pub tool: String,
  pub params_summary: serde_json::Value,
  pub permissions: Vec<String>,
  pub capabilities: Vec<String>,
  pub risk: ApprovalRisk,
  pub reason: String,
}

pub enum ApprovalDecision {
  AllowOnce,
  AllowForSession,
  Deny,
  DenyAndStop,
}
```

This protocol is more important than the UI. CLI can block on stdin. Server
can expose a pending approval endpoint. Web UI can render the same request.
Future TUI can subscribe to the same event stream.

Difficulty: **medium-high** because it touches runtime control flow.

### Gap 4: Parallel Native Tool Calls

OpenHarness-style operation expects a model to issue multiple tool calls in
one turn, and for the runtime to execute safe calls in parallel.

Yanshi has DAG-level concurrency and Rust async foundations. However,
`ReActAgent` currently dispatches only the first native tool call from an LLM
response and warns when multiple calls are returned.

Needed behavior:

- accept multiple native tool calls in one model response
- classify each call by idempotency, permissions, and approval state
- execute safe independent calls concurrently
- serialize risky or mutating calls when needed
- preserve deterministic step order in trace output

Difficulty: **medium**.

### Gap 5: Background Task Runtime

Yanshi has server runs, cancellation, workers, and multi-agent supervisors.
It does not yet expose Harness-style task tools directly to agents.

Suggested tools:

- `task_create`
- `task_get`
- `task_list`
- `task_stop`
- `task_output`

These tools should map to existing server/run infrastructure where possible.
The first implementation can be process-local. A later implementation can
delegate to `yanshi-server` and worker runtime.

Difficulty: **medium-high**.

### Gap 6: Project Context Provider

Harness agents need workspace context beyond normal chat memory.

Yanshi currently has:

- `AGENTS.md`
- `TODOs.md`
- `RoadMap.md`
- Skill instructions
- memory stores
- file tools
- RAG

But it lacks a standard context provider that assembles project context for a
work session.

Suggested interface:

```rust
pub trait ContextProvider {
  async fn collect(&self, context: &HarnessContext) -> Result<Vec<ContextItem>>;
}
```

Initial providers:

- workspace instructions provider: `AGENTS.md`, `CLAUDE.md`, `.yanshi/*`
- TODO provider: `TODOs.md`
- roadmap provider: `RoadMap.md`
- git status provider
- recent trace provider
- memory provider
- RAG provider

Difficulty: **medium**.

### Gap 7: Stable Stream-JSON Agent Output

Server SSE exists, and trace replay exists. CLI output is not yet uniformly
designed around Harness-style streaming agent events.

Needed CLI mode:

```bash
yanshi harness run "..." --output stream-json
```

Each line should be a stable event envelope:

```json
{"seq":0,"kind":"session.started","ts":"...","payload":{...}}
{"seq":1,"kind":"llm.started","ts":"...","payload":{...}}
{"seq":2,"kind":"tool.approval_requested","ts":"...","payload":{...}}
{"seq":3,"kind":"tool.completed","ts":"...","payload":{...}}
{"seq":4,"kind":"session.completed","ts":"...","payload":{...}}
```

This is critical for automation and UI integration.

Difficulty: **medium**.

### Gap 8: Provider Profile And Subscription Bridge

OpenHarness includes strong provider setup ergonomics and subscription bridge
ideas. Yanshi has multiple LLM providers and model registry support, but it
does not aim to bridge third-party subscription products.

Recommendation:

- Support provider profiles for Yanshi-native config.
- Do not prioritize subscription bridge compatibility in V1 Harness Mode.
- Keep provider behavior behind `yanshi-llm`.

Difficulty for provider profiles: **medium**.
Difficulty for subscription bridges: **high**, and strategically lower value.

## Proposed Architecture

### New Crate

Add a new crate:

```text
yanshi-harness
```

This crate should depend on existing surfaces:

- `yanshi-agents`
- `yanshi-tools`
- `yanshi-skills`
- `yanshi-memory`
- `yanshi-mcp`
- `yanshi-tracing`
- `yanshi-core`

It should not own low-level tool execution, workflow scheduling, or LLM
provider logic.

### Core Types

```rust
pub struct HarnessRuntime {
  session_store: Arc<dyn HarnessSessionStore>,
  context_providers: Vec<Arc<dyn ContextProvider>>,
  hooks: Arc<HarnessHookRegistry>,
  approval: Arc<dyn ApprovalProvider>,
}

pub struct HarnessSession {
  pub id: String,
  pub workspace_root: PathBuf,
  pub profile: HarnessProfile,
  pub status: HarnessSessionStatus,
}

pub struct HarnessContext {
  pub session_id: String,
  pub input: String,
  pub workspace_root: PathBuf,
  pub model: String,
  pub runtime: HarnessAgentRuntimeKind,
  pub metadata: serde_json::Value,
}

pub enum HarnessAgentRuntimeKind {
  React,
  PlanExecute,
  Handoff,
  Blackboard,
  Debate,
}
```

### Runtime Flow

```text
1. Create or resume HarnessSession.
2. Load project instructions and memory through ContextProvider.
3. Resolve skills and tool sources.
4. Build ToolRegistry with policy and capability grants.
5. Attach hook registry and approval provider.
6. Build selected AgentRuntime.
7. Stream HarnessEvent envelopes during execution.
8. Persist trace, memory, and checkpoint state.
9. Return final AgentRunResult plus HarnessSession metadata.
```

### Event Model

Harness events should wrap existing `AgentEvent` and workflow events instead
of replacing them.

```rust
pub struct HarnessEvent {
  pub seq: u64,
  pub session_id: String,
  pub kind: String,
  pub ts: DateTime<Utc>,
  pub payload: serde_json::Value,
}
```

Recommended event kind namespace:

- `session.started`
- `session.resumed`
- `session.completed`
- `session.failed`
- `context.collected`
- `skill.loaded`
- `tool.policy_decision`
- `tool.capability_decision`
- `tool.approval_requested`
- `tool.approval_resolved`
- `tool.started`
- `tool.completed`
- `llm.started`
- `llm.completed`
- `task.created`
- `task.updated`
- `task.completed`
- `checkpoint.saved`

## CLI Surface

Minimum CLI:

```bash
yanshi harness run "Analyze this project and propose next steps"
yanshi harness run --skill ./skills/code-review "Review current changes"
yanshi harness run --output stream-json "Implement the next TODO safely"
yanshi harness resume <session_id>
yanshi harness list
yanshi harness inspect <session_id>
```

Useful flags:

```text
--model <model>
--runtime react|plan-execute|handoff|blackboard|debate
--skill <path-or-name>
--mcp-config <path>
--permission-mode ask|deny|auto
--security-profile dev|local|production
--output text|json|stream-json
--run-dir <path>
--trace-dir <path>
```

Initial implementation should avoid TUI. A stable stream-json interface gives
TUI and Web UI a clean integration point later.

## Server Surface

Harness Mode can reuse the existing local server direction.

Potential routes:

```text
POST /v1/harness/sessions
GET  /v1/harness/sessions
GET  /v1/harness/sessions/{id}
POST /v1/harness/sessions/{id}:resume
POST /v1/harness/sessions/{id}:cancel
GET  /v1/harness/sessions/{id}/events
GET  /v1/harness/sessions/{id}/events/history
GET  /v1/harness/sessions/{id}/approvals
POST /v1/harness/sessions/{id}/approvals/{approval_id}
```

Do not make server execution mandatory. CLI direct execution should remain
first-class.

## Compatibility Strategy

### Compatible With Harness Agent Mode

Yanshi should be compatible with the mode, meaning:

- long-lived work sessions
- workspace-aware context
- Skills and MCP tools
- tool governance
- approval workflow
- event streaming
- background tasks
- resumability
- multi-agent delegation

### Not A Clone

Yanshi should not treat these as short-term requirements:

- exact OpenHarness command compatibility
- exact OpenHarness plugin format compatibility
- exact OpenHarness TUI behavior
- provider subscription bridge compatibility
- ohmo-like multi-channel assistant behavior

Those may be evaluated later if user demand appears, but they should not drive
the core architecture.

## Phased Implementation Plan

### Phase H0: Design And Contract Inventory

Goal: design the stable interfaces before code.

Tasks:

- Write `docs/HARNESS_MODE.md` or expand this document into an implementation
  spec.
- Define `HarnessEvent` JSON envelope.
- Define `ApprovalRequest` and `ApprovalDecision`.
- Define hook trait boundaries.
- Define minimal CLI contract.
- Decide whether `yanshi-harness` is a new crate or initially a module in
  `yanshi-agents`.

Estimated effort: **2-4 days**.

Risk: low.

### Phase H1: Harness Runtime MVP

Goal: run one Harness session locally through CLI.

Scope:

- `yanshi-harness` crate
- `HarnessRuntime`
- `HarnessContext`
- `HarnessEvent`
- context providers for `AGENTS.md`, `TODOs.md`, and `RoadMap.md`
- ReAct runtime integration
- SkillBuilder integration
- ToolRegistry integration
- text/json/stream-json output

Estimated effort: **1-2 weeks**.

Risk: medium.

### Phase H2: Hooks And Approval

Goal: make risky tool use governable.

Scope:

- hook registry
- pre/post tool hooks
- approval provider trait
- CLI blocking approval provider
- non-interactive deny/auto modes
- event emission for approval lifecycle
- tests for denied, allowed once, allowed for session, and cancelled cases

Estimated effort: **1-2 weeks**.

Risk: medium-high because this changes tool execution control flow.

### Phase H3: Parallel Tool Calls

Goal: support multiple native tool calls in one LLM turn.

Scope:

- modify ReAct tool-call dispatch
- preserve trace order
- execute safe calls concurrently
- serialize or approval-gate risky calls
- tests for mixed safe/risky tool batches

Estimated effort: **1-2 weeks**.

Risk: medium.

### Phase H4: Background Task Tools

Goal: let agents delegate work to managed subtasks.

Scope:

- `task_create`
- `task_get`
- `task_list`
- `task_stop`
- `task_output`
- process-local task runtime first
- optional server-backed task runtime later
- trace and cancellation integration

Estimated effort: **2-3 weeks**.

Risk: medium-high.

### Phase H5: Server And Web UI Integration

Goal: expose Harness sessions through the local server and debugger UI.

Scope:

- session routes
- approval routes
- event history and SSE
- Web UI timeline rendering
- Web UI approval panel
- persisted session metadata

Estimated effort: **3-5 weeks**.

Risk: medium-high.

### Phase H6: Advanced Compatibility

Goal: selectively absorb ecosystem features only if needed.

Potential scope:

- richer slash-command model
- TUI
- OpenHarness-style config import
- plugin compatibility adapters
- provider profile migration helpers

Estimated effort: **open-ended**.

Risk: high if not scoped tightly.

## Difficulty Breakdown

| Area | Difficulty | Reason |
|---|---:|---|
| Harness session abstraction | Medium | Requires new orchestration layer, but reuses existing crates |
| Skill/MCP/tool assembly | Low-medium | Most components already exist |
| Context providers | Medium | Needs good defaults and token budgeting |
| Hook bus | Medium | New cross-cutting runtime extension point |
| Approval protocol | Medium-high | Requires pausable execution and CLI/server integration |
| Parallel native tool calls | Medium | Rust async helps, trace determinism needs care |
| Background task tools | Medium-high | Requires lifecycle, cancellation, output capture, persistence |
| Resume semantics | Medium | Existing foundations exist, but side effects remain hard |
| Server integration | Medium-high | Must align with auth, SSE, retention, and run storage |
| TUI/product shell | High | Separate product surface, not required for core mode |
| Subscription bridge compatibility | High | Strategically optional and provider-specific |

## Main Risks

### Risk 1: Creating A Parallel Runtime

If Harness Mode bypasses `AgentRuntime`, `ToolRegistry`, or trace contracts, it
will fragment Yanshi.

Mitigation:

- Harness Mode must wrap existing runtime contracts.
- New behavior should be additive through hooks, events, and session context.

### Risk 2: Unsafe Tool Approval Defaults

Harness-style agents are powerful because they work in a real workspace. That
also makes unsafe defaults dangerous.

Mitigation:

- Make security profile explicit.
- Default to ask/deny for mutating tools in local/server modes.
- Record approval decisions in trace.
- Preserve non-interactive fail-closed behavior for production profile.

### Risk 3: Resume Repeats Side Effects

Long-running sessions need resume. Tool side effects make replay dangerous.

Mitigation:

- Require idempotency metadata.
- Do not replay unknown or non-idempotent calls automatically.
- Record unresolved tool calls and expose manual recovery instructions.
- Continue the P1.7 non-idempotent resume policy work in `TODOs.md`.

### Risk 4: Context Overload

Workspace context can become too large and noisy.

Mitigation:

- Context providers must emit structured items with priority and token cost.
- Prompt assembly should have golden tests.
- Summarize or retrieve context instead of dumping files blindly.

### Risk 5: UI-First Drift

Building TUI/Web UI too early can freeze the wrong protocol.

Mitigation:

- Stabilize stream-json and event envelopes first.
- Keep CLI direct execution first-class.
- Treat UI as a client of the protocol.

## Recommended Priority

Harness Mode should be placed after or alongside the current stabilization
tracks:

- after P0 contract hardening starts
- alongside P1 security and tool governance
- after enough P2 local server reliability exists for session/event reuse
- before investing heavily in extra product channels

It fits especially well with:

- P1 Security And Tool Governance
- P2 Local Server / Daemon Reliability
- P3 Rust SDK And CLI Experience
- P4 Memory, RAG, And Eval Foundations

## Acceptance Criteria For MVP

Harness MVP is useful when all of the following are true:

- A user can run `yanshi harness run "..."` from a workspace.
- The runtime reads project instructions from `AGENTS.md` when present.
- The runtime can load one explicit Skill.
- The runtime can use built-in tools through `ToolRegistry`.
- Tool policy and capability decisions appear in events.
- Risky tool calls can request approval in CLI mode.
- `--output stream-json` emits stable line-delimited events.
- The final answer includes a session id.
- The session can be resumed by id.
- Tests cover allow, deny, cancel, resume, and tool failure cases.

## Final Recommendation

Yanshi should support Harness Agent mode as an evolution of its current
runtime architecture.

The right implementation is not a clone of OpenHarness. The right
implementation is a small, stable, Yanshi-native Harness layer that:

- makes agents workspace-aware
- composes Skills, MCP, plugins, workflows, and built-in tools
- adds hooks and approval
- supports structured streaming
- supports resumable sessions
- exposes task delegation
- reuses existing multi-agent supervisors

This direction increases Yanshi's practical intelligence without abandoning
its core strengths: deterministic workflows, Rust-native reliability,
structured tracing, explicit security governance, and composable runtime
contracts.

