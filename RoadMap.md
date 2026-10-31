# Yanshi Roadmap

Last updated: 2026-07-23

This roadmap is forward-looking only. For implemented behavior and current
stability boundaries, see:

- `docs/CURRENT_STATUS.md`
- `docs/STABILITY.md`
- `docs/API_COMPATIBILITY.md`

Historical evaluation context remains in `docs/archive/PROJECT_EVALUATION_2026-05-01.md`.
The architecture direction below is set by `docs/RFC_CRATE_ARCHITECTURE.md`
(contract kernel) and validated by `docs/ARCHITECTURE_EVALUATION_2026-06-20.md`.
Short-term execution items live in `TODOs.md`.

## Direction

Yanshi is converging on a Rust-native runtime for reliable AI workflows and
agents. The near-term strategy is **Core Runtime Stabilization**: make the
execution kernel, agent runtime, tool/MCP/Skill composition, tracing, security
governance, CLI, Rust SDK, and local server/daemon surfaces dependable before
adding product-channel integrations.

The framework supports **four execution paradigms** that are converging onto one
coherent stack rather than parallel code paths:

- **static DAG workflows** — deterministic graphs for repeatable production
  automation (human authors the `Flow` in code/YAML);
- **agent-native loops** — planning, tool use, reflection, memory, and
  multi-agent collaboration (the agent plans implicitly per step);
- **harness governance** — a long-lived, workspace-aware, governable session
  shell that wraps any runtime (hooks / approval / background tasks);
- **dynamic workflows** — an agent that *generates* a `Flow` at runtime, then
  hands it to the executor (nodes may themselves be agents).

These unify **at the contract layer, not the driver layer**: everything reduces
to a small set of narrow interfaces — `Tool` (工具), `Capability` (能力),
`AgentRuntime`, the `Flow` IR, and store/event SPIs — while the four drivers stay
independent and never import each other. The convergence is delivered by an
**in-place strangler-fig migration to a narrow-waist contract kernel** (no
rewrite), governed by eight compiler/CI-enforced dependency laws
(`xtask check-arch`). See the **Contract Kernel + Dynamic Workflow** track below
and the `P-A` track in `TODOs.md`.

Those paths remain composable through stable node, tool, skill, plugin, trace,
and server contracts. Access is intentionally CLI-first and code-first, with a
local server/daemon as an execution control plane. Slack, Telegram, Discord,
desktop tray, and other channel integrations are deferred until the runtime
contracts and security model are stronger.

## Target Product Shape

Yanshi should expose three primary access modes:

1. **Rust SDK** for embedding `Flow`, `AgentRuntime`, `ToolRegistry`,
   `SkillBuilder`, MCP, RAG, memory, and tracing in applications.
2. **CLI** for local workflows, skills, agents, diagnostics, trace replay,
   evaluation, plugin/marketplace management, and automation-friendly JSON
   output.
3. **Local server / daemon** for run submission, cancellation, event streaming,
   graph/history retrieval, Skill execution, and Web UI debugging. The daemon
   is a runtime service, not a multi-channel messaging hub.

## Near-Term Priorities

### P0 — V1 Contract Hardening

- Keep `docs/STABILITY.md` and `docs/API_COMPATIBILITY.md` current whenever a
  public trait, manifest, trace schema, or server envelope changes.
- Add schema round-trip tests for every stable serialized contract.
- Document migration notes for every beta-to-stable promotion.

### P1 — Security And Tool Governance

- Add strict security profiles for trusted local development, normal local
  operation, and production/server operation.
- Fail closed in production server mode when authentication is missing.
- Make CORS, request body limits, run limits, and tool policy explicit.
- Harden HTTP/file/shell/script/plugin execution against SSRF, traversal,
  unsafe defaults, non-idempotent replay, and missing sandbox enforcement.
- Record tool policy, capability, sandbox, and idempotency decisions in traces
  and server events.

### P2 — Local Server / Daemon Reliability

- Add `yanshi serve` as the supported local runtime service command.
- Continue hardening server/database behavior with production run storage,
  retention, cleanup, and tenant/session policy.
- Exercise `/v1/runs`, cancellation, SSE, and Web UI workflows against real
  persisted runs.
- Define backup/restore expectations for run, trace, and artifact data.
- Allow the CLI to optionally submit to a local daemon while keeping direct
  in-process execution as the default dependable path.

### P3 — Rust SDK And CLI Experience

- Maintain clear code-first examples for workflows, agents, Skills, MCP, RAG,
  tracing, and tool security.
- Keep human-readable CLI output ergonomic and machine-readable JSON output
  stable enough for automation.
- Expand `yanshi doctor` into a full environment and security diagnostic
  covering config, providers, feature flags, MCP, sandbox, server, database,
  and plugin readiness.
- Keep official examples offline/mock runnable by default, with live provider
  execution opt-in.

### P4 — Memory, RAG, And Eval Foundations

- Keep RAG evaluation datasets and recall/MRR/nDCG baselines under versioned
  fixtures.
- Extend evaluation from RAG-only quality checks to local agent/task harnesses.
- Define memory layers for session memory, semantic memory, user preferences,
  entity facts, and retention/forgetting policy.
- Add prompt assembly, memory compaction, and agent trace compatibility
  golden tests.

### P5 — Plugin, Marketplace, And Worker Hardening

- Complete verified remote artifact cache to Skill/Plugin install directory
  handoff.
- Add signed marketplace fixture artifacts for local verification tests.
- Keep subprocess JSON-RPC as the stable plugin runtime; keep WASM as a later
  option only after real plugin usage validates the contract.
- Move the distributed scheduler from foundation to production readiness only
  after worker admission, authentication, resource limits, and failure-domain
  tests are in place.

### P-A — Contract Kernel And Dynamic Workflow

Converge the four execution paradigms onto one narrow-waist contract kernel by an
**in-place strangler-fig migration — explicitly not a rewrite**. Design:
`docs/RFC_CRATE_ARCHITECTURE.md`; validation + refinements:
`docs/ARCHITECTURE_EVALUATION_2026-06-20.md`; execution: `TODOs.md` §P-A.

- **Sequenced after the Q security/correctness waves** — an architecture refactor
  must never preempt production-blocking fixes. Every PR changes one thing, keeps
  old paths working via `pub use` re-exports, and stays `cargo test` +
  `clippy -D warnings` green.
- Extract the kernel: `value` (universal leaf — pulled **first**, it is a
  prerequisite of `graph`), `graph` (the `Flow` IR, split out of `core` so the
  IR ≠ the executor), `agent-spi` (`AgentRuntime` / `AgentEvent` / `HarnessEvent`
  / `Capability` / event + approval SPIs), `store-spi` (`KnowledgeBackend` /
  `MemoryStore` / embeddings), and `async-util` (one home for retry / timeout /
  cancellation, replacing the two duplicate implementations).
- Re-point the runtimes onto contracts: `harness → agent-spi` (not the `agents`
  crate), `agents` driven by injected `Tool` / `Capability` impls, and the
  surface crates become the only tier allowed to import the whole graph
  (`worker ⊥ server` via `worker-proto`; `server ⊥ cli` via a shared assembly).
- Enforce structurally: `xtask check-arch` activates each of the eight dependency
  laws as its kernel crate lands; the violation allowlist can only shrink.
- Land **dynamic workflow** on the cleaned dependencies — an agent generates a
  real `Flow` (with dependencies / parallel / conditional nodes, some of them
  `AgentNode`s) that the executor runs and the harness governs, requiring **no
  new sideways dependency**. Reposition RAG as a `KnowledgeBackend` capability
  behind a Skill's `knowledge:` declaration (the `rag search/index` CLI demotes
  to ops subcommands; the eval harness stays as the quality gate).
- Resolve the one open crate-division question the evaluation surfaced:
  decompose the fat `yanshi-nodes` straddler so tool-tier nodes
  (`template`/`file`/`http`/`batch`/`conditional`/`while`) stop dragging the
  `llm` / `rag` capabilities into the tool tier (evaluation R3).

### S — Sandbox And Code Execution Hardening

Deepen the two-layer sandbox (in-process `SandboxPolicy` + OS `SandboxBackend`)
into a **trust-aware code execution model**. Source: the 2026-07-23 sandbox
review; execution: `TODOs.md` §S. Sequenced as the next active wave now that the
P-A contract-kernel track has closed. The gap the review surfaced: the runtime
distinguishes *which tool* runs (shell/script/file) but not *where the code came
from* — author-signed skill scripts and LLM-generated content share the same
execution paths and trust assumptions.

- Model code provenance explicitly (author-signed / user-provided /
  llm-generated) and gate every execution channel on it
  (`docs/RFC_CODE_EXECUTION_TRUST.md`, S0).
- Close the file+script composition channel: the per-skill merged
  `SandboxPolicy` must not let a `file`-tool write into `scripts/` become
  executable via the `script` tool (S0.2).
- Extend install-time trust to run time: manifest script inventories with
  content hashes, verified on every `ScriptTool` execution, enforcement staged
  by security profile (S1).
- Support real project code in Skills: declared dependencies with lockfiles and
  per-skill isolated interpreter environments (venv / node_modules), covered by
  the same integrity inventory (S2).
- Strengthen OS backends until `os_sandbox: true` can become the default:
  Landlock path scoping + cgroups resource limits on Linux, interpreter-aware
  SBPL profiles on macOS (S3).
- Any LLM code-interpreter capability (`code_exec`) is RFC-gated and requires a
  strongly isolated `ContainerBackend` (container / microVM) implementing the
  existing `SandboxBackend` trait — it must never reuse the skill-script trust
  model (S4).

### L — Long-Horizon Agents And Retrieval Hardening

Close the capability gaps confirmed by the 2026-07-23 curriculum-gap review
(an external AI-agent training syllabus checked against this codebase — most
of its themes are already covered here, usually more deeply; five concrete
gaps remain). Execution: `TODOs.md` §L. Orthogonal to the S security track;
can run in parallel after the S0 quick-fix wave. Directional leftovers that
did not make the cut (task-level state machine surface, model routing /
fallback chains, cross-cutting cache layer) stage in `docs/ROADMAP_v2.md` §K.

- **Plan revision for dynamic workflows** — today `compile_plan_to_flow` is
  plan-once → execute-deterministically; a failed node fails the run. Add a
  failure-driven replan loop (completed-node outputs are reused, replan count
  is budgeted) so long-horizon tasks survive mid-flight surprises, plus
  loop-signature detection in the ReAct runtime (repeated tool+args → forced
  reflection or a `LoopDetected` stop) (L1).
- **Task-summary recovery** — checkpoints restore state pools, not narratives.
  Persist a structured `TaskSummary` (goal / done / key results / open issues /
  next steps) at compaction + checkpoint time so an agent can resume a long
  task after context truncation (L2).
- **Project-level memory** — a memory scope for codebase-level facts (build /
  test commands, module boundaries, past fixes) so codebase agents stop
  cold-starting every run (L3).
- **Retrieval hardening** — AST-aware / fine-grained chunking (the P-A4.2
  leftover), a rerank + post-processing chain, query rewrite / decomposition,
  and citation-consistency checking — each admitted only on demonstrated
  `rag eval` gains (L4).
- **Subagent delegation contracts** — a structured `DelegationSpec` plus
  per-subagent capability narrowing through the existing
  `EffectiveCapabilities` intersection merge; result aggregation with explicit
  conflict arbitration (L5).

## Later Tracks

> **Looking for the consolidated post-v1.0 picture?** See
> [`docs/ROADMAP_v2.md`](docs/ROADMAP_v2.md) — it unifies the
> sections below, the `v1.x` entries in `TODOs.md`, and the
> §7 themes from the latest project evaluation into a single
> staging ground. The prose here is preserved as historical
> rationale; the v2 doc is the source of truth for "what comes
> after v1.0 GA".

### Harness Agent Mode

Yanshi's evolution toward a long-lived, workspace-aware, governable agent
session pattern. Full design lives in `docs/archive/HARNESS_MODE_EVOLUTION.md`; active
execution tasks live in `TODOs.md` under the `P-H` parallel track.

The Harness layer must remain additive — it wraps existing `AgentRuntime`,
`ToolRegistry`, Trace, and Skill contracts rather than replacing them. It is
scheduled as a parallel track to P1-P5 (not after them), with these phase
prerequisites:

- H0 (contract inventory) — no prereq, can start immediately.
- H1 (runtime MVP) — depends on H0; uses existing AgentRuntime/ToolRegistry.
- H2 (hooks + approval) — depends on H1 and `P1.7` non-idempotent resume
  visibility.
- H3 (parallel tool calls) — depends on H1 and `P3.7` LLM provider matrix.
- H4 (background task tools) — depends on H2 and an in-process task runtime
  design from H0.
- H5 (server + Web UI integration) — depends on `P2.1` (`yanshi serve`),
  `P2.2` (retention), `P2.4` (SSE robustness), `P-H.2`, and `P6.*` Web UI
  baseline.
- H6 (advanced compatibility) — open-ended; remains in this Later Track and
  is promoted to TODOs only when individual items become concrete.

Explicit Non-Goals for Harness Mode (preserved here):

- Clone of OpenHarness TUI or provider subscription bridges.
- UI-first product shell that freezes the protocol before stream-JSON
  envelopes stabilize (see `docs/archive/HARNESS_MODE_EVOLUTION.md` Risk 5).
- Parallel runtime bypassing AgentRuntime / ToolRegistry / Trace contracts
  (Risk 1).
- Unsafe tool approval defaults that allow mutating tools without explicit
  policy (Risk 2).

### Distributed Execution

- Move the distributed scheduler from foundation to production readiness.
- Worker admission, authentication, resource limits, and failure-domain tests
  are done (`P5.5`–`P5.7`, closed).
- Worker-executable node types are done (`P2.8`, closed): `template`, `file`,
  `mock`, `llm`, `http`, `mcp`, and `agent` all dispatch
  (`yanshi-worker/src/lib.rs::execute_supported_node_payload`). The
  `agent` payload's tool wiring stays minimal — it runs against a fresh,
  empty `ToolRegistry` (no distributed tool-call support yet); richer tool
  wiring rides the same `parameters` plumbing once that distribution
  contract is decided.
- Validate large DAG scheduling with mixed local and worker-executed nodes.

### Evaluation Expansion

- Maintain live provider tests as explicit opt-in suites with cost guards.
- Add RAG evaluation datasets with recall/MRR/nDCG baselines.
- Grow the agent-level eval harness toward a benchmark platform: golden task
  sets, batch runs, and metrics beyond retrieval — task success rate,
  tool-call / parameter accuracy, citation accuracy, hallucination rate,
  failure-recovery rate, human-handoff rate — plus failure-sample clustering,
  pairwise comparison, and before/after version diffs. Security counters
  (blocked over-privileged tool calls, approval-trigger rate, dangerous-command
  intercepts) join the same report and double as the §S observability
  acceptance list.
- Keep release gates focused on formatting, clippy, workspace tests, schema
  compatibility tests, and example validation.

### Web UI Productization

- **Positioning (P10.17.1, committed)**: debugger / run console only.
  Operator-dashboard features (cost aggregation, retry-rate trends,
  worker utilization, policy-decision summary) are intentionally
  out of scope — Prometheus + Grafana + BI tools cover those better.
  Full rationale + in/out scope table in
  [`docs/WEB_UI.md` § Product positioning](docs/WEB_UI.md#product-positioning).
- Add richer run creation flows, provider configuration diagnostics, and trace
  comparison.
- Add durable user preferences without leaking API tokens.
- Add operator-focused filtering for agent/tool/MCP/RAG events.

### Plugin Runtime Expansion

- Keep subprocess JSON-RPC as the stable v1 runtime.
- Re-evaluate WASM only after the subprocess contract is exercised by real
  third-party plugins.
- Avoid native `dlopen` plugin loading unless there is a concrete ABI strategy.

### Operations

- Define deployment profiles for local dev, single-node server, and distributed
  worker clusters.
- Add end-to-end smoke tests that cover CLI, server, worker, tracing, and Web
  UI paths.
- Improve observability dashboards around run latency, retry rates, tool policy
  decisions, and worker utilization.

## Non-Goals For V1

- Native dynamic library plugins.
- Unbounded remote code execution from marketplace packages.
- Executing LLM-generated code in-process or under the process-level sandbox
  alone; a dedicated strongly isolated backend is a prerequisite, and the
  capability itself is RFC-gated (see `TODOs.md` §S4).
- Provider-specific agent behavior that bypasses the shared Tool/Trace/Policy
  surfaces.
- A Web UI that is required for headless operation; CLI and trace replay remain
  first-class.
- Slack, Telegram, Discord, desktop tray, webhook channel routing, or other
  channel integrations before the core runtime and local daemon are stable.
- Local OS keyboard/mouse control as a default capability. Any future OS
  control tools must be feature-gated, explicitly authorized, audited, and
  reversible where possible.
