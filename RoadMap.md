# Yanshi Roadmap

Last updated: 2026-11-01

This roadmap is forward-looking only. For implemented behavior and current
stability boundaries, see:

- `docs/CURRENT_STATUS.md`
- `docs/STABILITY.md`
- `docs/API_COMPATIBILITY.md`

The architecture direction below is set by `docs/RFC_CRATE_ARCHITECTURE.md`
(contract kernel) and validated by `docs/ARCHITECTURE_EVALUATION_2026-06-20.md`.
Directional post-v1 ideas are staged in `docs/ROADMAP_v2.md`.

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
rewrite), governed by eight dependency laws (`docs/RFC_CRATE_ARCHITECTURE.md`
§7), three of which `xtask check-arch` enforces today.

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

## Closed Tracks

Every near-term track this roadmap used to list has landed. The records
are in the dated task snapshots under `docs/archive/TODOs-archive-*.md`
and in `CHANGELOG.md`.

| Track | What it delivered |
|---|---|
| P0–P5 | V1 contract hardening, security profiles and tool governance, `yanshi serve` with retention / cleanup / backup + restore, `yanshi doctor`, memory layers and the RAG + agent eval harnesses, plugin / marketplace / worker hardening |
| P-A | The contract kernel (`value` / `graph` / `agent-spi` / `store-spi` / `async-util` / `tool`), dynamic workflows, RAG as a `KnowledgeBackend` capability, the `yanshi-nodes` / `yanshi-nodes-ai` split |
| P-H (H0–H5) | Harness Agent Mode: event envelope, runtime, hooks + approval, parallel tool calls, background tasks, server + Web UI surface |
| S0–S4 | Code-provenance trust model, skill script hash inventories, per-skill Python venvs, Landlock / cgroups / macOS profiles with `os_sandbox` on by default, `code_exec` on an isolated `ContainerBackend` |
| L1–L5 | Failure-driven replan + loop detection, task-summary checkpoints, project memory, retrieval hardening (chunking, rerank chain, query rewrite, citation checks), delegation contracts |
| W, P-LLM2, X | Evaluation remediation waves: cross-replica gateway state, distributed runs, MCP Modern-era transport + server, multimodal provider matrix, secure-by-default gateway policy |

## Open Before v1.0 GA

Each item below was checked against the code on 2026-11-01.

### Release

- Walk the production deployment checklist in
  `docs/RELEASE_NOTES_v1.0.0-rc.1.md` (P7.4-FU4) on a real deployment.
- Install the Helm chart on a production-shaped cluster. On 2026-11-01 it
  was installed on two single-node k3s v1.36 clusters (Postgres 18 and 16,
  `securityProfile: production`, bearer token from `existingSecret`). The
  smoke checks passed on both: probes, 401/403, a fixed DAG to `succeeded`
  with SSE backfill, gateway policy rejecting a `shell` node, readiness
  503 while Postgres is down, and runs surviving a pod restart. With
  `replicaCount: 2`, cross-pod live SSE, cross-pod cancel and the
  per-tenant admission cap also passed. The rehearsal found and fixed the
  Dockerfile missing protoc, duplicated and never-closing SSE on the
  non-executing replica, and the stale single-replica chart guard. Not yet
  covered: multi-node scheduling, Ingress/TLS, autoscaling, image pulls
  from a registry, and upgrades between chart versions.

### Features that exist only as library APIs

These work and are tested, but no CLI, server, or skill entry point turns
them on:

- `VerificationStrategy` (gating a ReAct final answer before it stops).
- The background-task tools (`task_create` and friends) from
  `yanshi-harness::tasks`.
- DAG checkpoint recovery (`FlowExt::with_checkpointing`).
- `open_execution_trace_sink`, which turns harness events into an
  `ExecutionTrace`.
- `HarnessRuntime::run_flow` has a CLI (`yanshi harness run-flow`) but no
  server route.

Wire one up when a user needs it, not for symmetry.

### Observability

- OTLP export covers workflow runs only. Skill runs and harness sessions are
  not exported.
- The OTLP exporter speaks HTTP+JSON only. gRPC (with TLS and auth) is left
  to an operator-supplied `OtelSpanSink`.
- `FileTraceStorage` is the only `TraceStorage` implementation. The SQLite
  and Postgres schemas are DDL constants without a backend.

### Execution

- The worker's `agent` payload runs against an empty `ToolRegistry`, so
  agents dispatched to a worker cannot call tools.
- `code_exec` has no network access at all. An egress allowlist proxy
  (`docs/RFC_LLM_CODE_EXECUTION.md`) is needed before it can reach anything.
- MCP follow-ups from the Modern-era work: probing the protocol era on a
  single transport, and one client-construction path shared by
  `McpClientPool` and the MCP node.

### Architecture

- `cargo xtask check-arch` enforces 3 of the 8 dependency laws. It reports
  10 latent edges that would break the rest (for example
  `yanshi-memory -> yanshi-rag`). Activate a law once its latent edges are
  gone; `docs/ARCHITECTURE_EVALUATION_2026-06-20.md` §2 has the target
  state.

## Later Tracks

> [`docs/ROADMAP_v2.md`](docs/ROADMAP_v2.md) is the source of truth
> for what comes after v1.0 GA. The sections below keep the standing
> constraints for each area.

### Harness Agent Mode

H0–H5 are closed (see Closed Tracks). H6 (advanced compatibility) stays
open-ended in `docs/ROADMAP_v2.md` §F. The Harness layer must remain
additive: it wraps the existing `AgentRuntime`, `ToolRegistry`, Trace, and
Skill contracts rather than replacing them. Design history:
`docs/archive/HARNESS_MODE_EVOLUTION.md`.

Explicit Non-Goals for Harness Mode (preserved here):

- Clone of OpenHarness TUI or provider subscription bridges.
- UI-first product shell that freezes the protocol before stream-JSON
  envelopes stabilize (see `docs/archive/HARNESS_MODE_EVOLUTION.md` Risk 5).
- Parallel runtime bypassing AgentRuntime / ToolRegistry / Trace contracts
  (Risk 1).
- Unsafe tool approval defaults that allow mutating tools without explicit
  policy (Risk 2).

### Distributed Execution

- `POST /v1/runs` accepts `execution_mode: "distributed"` (W4.3b).
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
  `cargo xtask refresh-live-models` checks that each suite's default model is
  still listed and still callable.
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
  alone. `code_exec` runs it only inside the isolated `ContainerBackend`
  (`docs/RFC_LLM_CODE_EXECUTION.md`).
- Provider-specific agent behavior that bypasses the shared Tool/Trace/Policy
  surfaces.
- A Web UI that is required for headless operation; CLI and trace replay remain
  first-class.
- Slack, Telegram, Discord, desktop tray, webhook channel routing, or other
  channel integrations before the core runtime and local daemon are stable.
- Local OS keyboard/mouse control as a default capability. Any future OS
  control tools must be feature-gated, explicitly authorized, audited, and
  reversible where possible.
