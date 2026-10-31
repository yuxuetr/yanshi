# Yanshi Architecture

Last updated: 2026-07-23

> **Direction note:** the workspace is migrating (in place, no rewrite) to a
> narrow-waist **contract kernel** that converges the four execution paradigms.
> Two complementary mental models below: **Four Execution Paradigms** (the
> conceptual, three-axis model — the *target*, with an honest model-vs-code
> reality check) and the **Layered Mental Model** (the *current* L1–L4 crate
> structure). See `docs/RFC_CRATE_ARCHITECTURE.md` for the target design,
> `docs/ARCHITECTURE_EVALUATION_2026-06-20.md` for the dependency-graph
> validation, and `TODOs.md` §P-A for execution.

Yanshi is a Rust workspace for deterministic workflow execution and agent-native
runtime loops. The project is organized around a small core engine and separate
crates for nodes, LLM access, tools, Skills, MCP, memory, tracing, visualization,
and the CLI/server surfaces.

All workspace crates use Rust 2024 edition.

## Four Execution Paradigms — Mental Model

Yanshi supports four execution paradigms (static DAG, native agent loop,
harness governance, dynamic workflow). They are **not** four boxes at one level;
they sit on **three orthogonal axes**. Confusing the axes is the usual source of
"where does X belong?" questions.

### Axis 1 — Planning / binding-time (the execution paradigm)

*Who decides the plan, and when.* This is a single spectrum from "fully fixed at
author time" to "decided every step at runtime":

| Paradigm | Who plans | When | Execution | Use when… |
|---|---|---|---|---|
| **Static DAG** | human | author time | fully deterministic, replayable | the steps are known and stable |
| **Dynamic workflow** | agent | runtime, **once** (emits a `Flow`/code) | deterministic after emission | the task is plannable up front but varies per request |
| **Native agent loop** (ReAct) | agent | runtime, **every step** | non-deterministic | the task needs mid-course correction / exploration |

**Why dynamic workflow can raise reliability:** it collapses many scattered
runtime LLM decisions into **one up-front artifact** (a `Flow`, or sandboxed
code), then executes it deterministically — inheriting retry / checkpoint /
timeout / tracing / replay for free. You pay one LLM "compile" and get a
reproducible, governable run.

> **Caveat — this is not an absolute reliability ordering.** Later binding is not
> "always worse". Dynamic workflow wins *only when the task is plannable up front*;
> tasks that need replanning mid-flight are genuinely better served by the loop.
> The real rule is **match the binding-time to how predictable the task is**.
> A middle point is planned: a failure-driven **replan loop** (re-plan with
> completed-node outputs preserved, replan count budgeted) lets dynamic workflow
> slide back up the axis when a plan meets reality — tracked as `TODOs.md` §L1.

Dynamic workflow has two flavors, a deliberate trade-off:
- **structured `Flow`** (our first-class form): typed, inspectable, auto-governed
  and traced — but expressiveness is bounded by nodes + dependencies;
- **sandboxed code** (LLM writes code, executes in a sandbox): maximal
  expressiveness, but opaque to governance/observability.
They compose — "execute code" can be a node/tool inside a `Flow`.

### Axis 2 — Capability substrate (orthogonal)

*What can be invoked.* `Tool` (atomic callable), MCP (external tools), RAG /
Memory (knowledge / state), `Skill` (a packaged bundle — persona + tools +
knowledge + config — that **lowers** to tools + context at the runtime boundary).
**All four paradigms share this layer**: a DAG node, a dynamic-workflow node, and
one step of an agent loop all call the same `Tool`s. The lowering is now a real
contract — `yanshi_agent_spi::Capability::lower() -> Lowered { tools, context }`,
implemented by `SkillCapability` (P-A4.3); RAG sits on this axis too, as a
`KnowledgeBackend` behind a Skill's `knowledge: backend = "rag"` plus the
`rag_search` tool (P-A4.1 / P-A4.2), not a top-level mode.

### Axis 3 — Governance shell (orthogonal)

*What rules are enforced.* The **harness** — approval, pre/post hooks, sandbox,
audit, run limits, background tasks. It is **a shell wrapped around an execution,
not an execution mode itself.** It is not perfectly free-floating: governance
must hook into the execution at well-defined seams (step boundaries, tool calls),
which is exactly why the event/approval contracts (`HarnessEvent` / `Approval*`)
exist.

### Composition

The paradigms are **recursively composable**, not a strict hierarchy. An agent is
usually the flexible *entry/coordinator* that "slides down" the binding-time axis
toward determinism as a task crystallizes (loop → emit a Flow → call a prebuilt
DAG → call one tool). Two adapters make this bidirectional:
- **`AgentNode`** — an agent embedded *in* a DAG (a Flow step that is an agent);
- **`WorkflowTool`** — a DAG exposed *as* a tool to an agent.

So agent-in-workflow-in-agent nesting is expressible. (Today the choice of *where*
on the binding-time axis to run is a **developer build-time** decision —
`ReActAgent` vs `PlanExecuteAgent` vs a fixed DAG — not a runtime decision the
agent makes for itself.)

### Reality check — model vs current code

This model is the **target**. As of 2026-06-22 the P-A track has landed the
contract kernel and the dynamic-workflow / RAG-repositioning product work — be
honest about which parts are production vs aspirational:

| Model element | Status in code |
|---|---|
| Static DAG · native loop · capability substrate · `AgentNode`/`WorkflowTool` | ✅ production |
| **Dynamic workflow** | ✅ **library + CLI.** `yanshi_agents::dynamic::compile_plan_to_flow` compiles a declarative `WorkflowPlan` (the LLM-shaped JSON `{id, tool, params, depends_on}`) into a `Flow` of real tool calls with dependency-driven parallelism; `DynamicWorkflowAgent` makes the LLM planning call then compiles + executes via an injected `FlowRunner` (both tested). Surfaced as `yanshi workflow dynamic` with sandbox + approval governance (P-A4.4 / P-A4.5). Remaining: plans that include `AgentNode` steps, and a `PlanExecuteAgent` that emits a `Flow` rather than running sequentially. |
| **Harness as an orthogonal shell** | ✅ **MVP (P-A2.2).** `HarnessRuntime::run_flow` governs a deterministic `Flow` run: it brackets a `FlowRunner`-driven execution with the Harness envelope (`session_started` runtime=`flow` … `stopped`), and tool calls inside the Flow's nodes are governed (approval / hooks / audit) via a `wrap_registry`-wrapped node registry sharing the harness seq counter + sinks. Config-built nodes embed their own tools, so the `yanshi harness run-flow` CLI delivers the envelope + node events only, and refuses tool-running nodes under `--profile production` rather than run them ungoverned. Follow-ups: a server route (`yanshi harness run-flow` CLI + node-level `step_started` events landed). |
| "Paradigms meet only at the contract layer" | ✅ **true.** The P-A contract kernel is extracted and every tracked runtime/surface dependency edge is burned — `cargo xtask check-arch` reports 0 tracked violations with an empty allowlist. The runtimes (`core` / `agents` / `harness`) depend only on contracts, never on each other's impl crates. |

### Gaps map directly onto the `P-A` roadmap

| Model gap | Closing task | Status |
|---|---|---|
| Contractualize the four paradigms (so they compose orthogonally) | P-A1 contract kernel + edge burn-down | ✅ kernel extracted, 0 tracked edges (empty allowlist) |
| Dynamic workflow as a product | P-A4.4 plan→`Flow` compiler + `DynamicWorkflowAgent`; P-A4.5 CLI surface | ✅ library + `yanshi workflow dynamic`; ⏳ `AgentNode` steps in a plan |
| RAG on the capability axis (`KnowledgeBackend` + `rag_search`, Skill `knowledge: backend`) | P-A4.1 / P-A4.2 / P-A4.3 | ✅ |
| Harness governs a `Flow`, not only an agent loop | P-A2.2 | ✅ MVP + node-level events + `harness run-flow` CLI; ⏳ server route |
| Governance shell truly orthogonal (harness contracts in `agent-spi`) | P-A1.1 sub-step 2/2 | ✅ |
| Plan revision — dynamic workflow survives mid-flight failure (replan loop + loop-signature detection) | L1 | ⏳ backlog (2026-07-23 review) |
| Long-horizon recovery — structured `TaskSummary` survives context truncation | L2 | ⏳ backlog |
| Per-subagent capability narrowing + structured delegation contract | L5 | ⏳ backlog |

In short: the three-axis model is sound and self-consistent; three paradigms +
the capability substrate + the composition adapters are production-grade;
**dynamic workflow now has a real, tested library path (P-A4.4) plus a CLI surface
(P-A4.5, `yanshi workflow dynamic`), and the capability axis is contractualized
— `Capability` lowering (P-A4.3) and RAG as a `KnowledgeBackend` behind a Skill's
`knowledge: backend = "rag"` (P-A4.1 / P-A4.2)**. Orthogonal governance now has
its MVP too — the harness governs a deterministic `Flow` run via
`HarnessRuntime::run_flow` (P-A2.2), emitting node-level `step_started` events
and surfaced as the `yanshi harness run-flow` CLI, with a server route as the
remaining polish. The contract-kernel foundation that makes the
rest compose is complete (0 tracked dependency violations). See
`docs/RFC_CRATE_ARCHITECTURE.md` for the kernel design.

## Layered Mental Model

The workspace crates fall into five layers. **L0**, the **contract kernel**, was
extracted by the P-A track (`docs/RFC_CRATE_ARCHITECTURE.md`): a narrow waist of
contract crates everyone depends on and that depend on no implementation.

```text
+----------------------------------------------------------+
| L4 Operations / Productization                          |
|   tracing · server · db · worker · ui                   |
+----------------------------------------------------------+
| L3 Agent / Orchestration / Governance                    |
|   agents · skills · harness · config · cli               |
+----------------------------------------------------------+
| L2 Capability Adapters                                   |
|   nodes · nodes-ai · llm · tools · mcp · rag · memory    |
+----------------------------------------------------------+
| L1 Execution Core (the executor)                         |
|   core (FlowExt / FlowExecutor / scheduler / checkpoint  |
|         / retry-executor / resource & health primitives) |
+----------------------------------------------------------+
| L0 Contract Kernel (narrow waist)                        |
|   value (FlowValue) · graph (Flow IR / AsyncNode)        |
|   store-spi (MemoryStore + KnowledgeBackend)             |
|   agent-spi (AgentRuntime + Capability)                  |
|   async-util (retry/timeout/race) · tool (Tool contract) |
+----------------------------------------------------------+
```

L0 holds the **types and traits** (the `Flow` IR, `FlowValue`, `AgentRuntime`,
`MemoryStore`, the reliability combinators). L1 `core` is the **executor** of the
L0 graph IR — the topological/concurrent scheduler, exposed as the `FlowExt`
trait (`flow.run()`). The split (IR ≠ executor) is what lets a runtime *construct*
a `Flow` by depending on `graph` alone — the dynamic-workflow prerequisite. L2
capabilities reach L3 as `AsyncNode` impls (DAG path) or as tools/clients consumed
by an `AgentRuntime` (agent-native path); L3 `harness` governs a runtime; L4 is
observation/operation cross-cutting. The eight dependency laws between these
layers are enforced by `cargo xtask check-arch`.

## Runtime Model

Yanshi supports the four execution paradigms above (see § Four Execution
Paradigms). The two foundational runtimes they build on:

- **DAG workflows**: a `Flow` (the `yanshi-graph` IR) is run by the executor in
  `yanshi-core` via the `FlowExt` trait (`use yanshi_core::FlowExt; flow.run().await`).
  Nodes carry declared dependencies, input mappings, optional conditions,
  checkpoints, retry, timeout, resource limits, and health primitives. Two
  execution modes are available:
  - `FlowExecutionMode::Serial` (default): topological order, one node at a time.
  - `FlowExecutionMode::Concurrent`: dependency-ready dispatch via
    `FuturesUnordered` with a configurable `max_concurrency` window. Nodes whose
    dependencies are all `Ok(_)` or `NodeSkipped` are launched immediately.
- **Agent loops**: `yanshi-agents::AgentRuntime` records observe, plan, tool
  call, tool result, reflection, verification, and final answer steps. ReAct,
  plan/execute, and multi-agent examples are built on this runtime. A
  `ReflectionStrategy` only observes a stop decision already made; a
  `VerificationStrategy` gates it — a rejection sends the loop back around for
  another attempt instead of stopping. Each run produces an `AgentRunResult`
  with a structured `AgentStopReason` (one of: final answer, stop condition,
  max steps, max tool calls, timeout, cancelled, token budget, error).

The intended direction of composition is:

```text
Flow -> AgentNode -> AgentRuntime -> ToolRegistry -> Tool / MCP / WorkflowTool
```

Use workflows for deterministic production automation. Use agents when the next
step depends on model reasoning, tool feedback, memory, or reflection. Use
`AgentNode` when a workflow needs one agent-driven step, and use workflow tools
when an agent should call a stable DAG as a tool.

YAML can declare both styles: `llm` / `template` / `http` / `file` / `map` /
`while` and so on for DAG nodes; `agent` / `skill_agent` for agent-native
nodes that build a `ReActAgent` from a Skill manifest at run time.

## Workspace Crates

| Crate | Role |
| --- | --- |
| `yanshi-value` | **L0 kernel.** `FlowValue` — the universal data contract passed between nodes. Zero internal dependencies. |
| `yanshi-graph` | **L0 kernel.** The execution IR: `Flow`, `GraphNode`, `NodeType`, `AsyncNode`, the `expr` mini-language, and `YanshiError`. A runtime can construct a `Flow` by depending on this alone. |
| `yanshi-store-spi` | **L0 kernel.** Storage contracts: `MemoryStore`, `Message`/`Role`/`TokenCounter`, `MemoryError`. |
| `yanshi-agent-spi` | **L0 kernel.** Agent-runtime contracts: `AgentRuntime`, `AgentEvent`/`AgentStep`/`AgentContext`, and the turn-driven (`TurnDrivenRuntime`/`LoopSession`) façade the harness governs. |
| `yanshi-async-util` | **L0 kernel.** Reliability combinators (retry policies, timeout) shared by the executor and the agent loop. |
| `yanshi-core` | **L1 executor.** The DAG executor for the `yanshi-graph` IR: the topological/concurrent scheduler exposed via the `FlowExt` trait (`flow.run()`), checkpoint recovery, retry-executor, resource controls, health checks, and execution events. Re-exports the L0 IR types under their original `yanshi_core::*` paths. |
| `yanshi-nodes` | Config-first node implementations such as `llm`, `template`, `http`, `file`, `arxiv`, audio, image, MCP, RAG, `map`, and `while`. |
| `yanshi-llm` | Model configuration, provider clients, streaming, multimodal helpers, discovery, and model registry support. |
| `yanshi-cli` | User-facing commands for workflow run/validate/debug, dynamic workflow (`workflow dynamic`), config, LLM model discovery, MCP, Skills, tracing, audio, image, and optional RAG operations. |
| `yanshi-config` | Shared config-first workflow assembly: YAML workflow schema (`config::v2`), the `executor` that builds an `yanshi-core` `Flow`, and the `diagnostics` report builder. Consumed by both `yanshi-cli` and `yanshi-server` (P-A2.4). |
| `yanshi-agents` | ReAct / plan-execute / supervisor runtimes, `AgentNode`, `WorkflowTool`, and the `dynamic` module (`compile_plan_to_flow` + `DynamicWorkflowAgent`) for dynamic workflows. The runtime *contracts* live in `yanshi-agent-spi`. |
| `yanshi-tools` | Built-in tool interfaces, registry, sandbox and permission policy, file/http/shell/script/`code_exec` tools. `code_exec` (S4.2) runs LLM-generated Python inside a strongly-isolated `ContainerBackend` (Apple `container` CLI / rootless Podman) — a separate tier from the OS-sandbox backend the other tools use. |
| `yanshi-skills` | Skill loading, `SKILL.md` parsing, manifests, registry indexes, marketplace files, MCP tool discovery, and Skill builder integration. |
| `yanshi-mcp` | MCP stdio transport, client sessions, tools, resources, prompts, retry, and builder APIs. |
| `yanshi-rag` | RAG abstractions including vector store and reranking modules. |
| `yanshi-memory` | Session, SQLite, semantic memory types, and memory store abstractions. |
| `yanshi-tracing` | Structured trace events, file storage, redaction, replay, OpenTelemetry integration, and terminal timeline inspection. |
| `yanshi-db` | SQLx database layer with migrations, models, and repository traits for runs, steps, events, artifacts, Skill installs, and MCP sessions. |
| `yanshi-server` | Axum gateway with health endpoints, run submission/query routes, SSE event streams, Skill routes, bearer auth, Web UI embedding, and distributed scheduler control-plane primitives. |
| `yanshi-worker` | Distributed worker runtime and binary built around the `WorkerProtocol` abstraction. |
| `yanshi-harness` | **Governance shell.** Wraps a runtime (`AgentRuntime`/`TurnDrivenRuntime` via `yanshi-agent-spi`) with hooks, interactive approval, sandboxing, audit, run limits, and background tasks; emits the `HarnessEvent` envelope. |
| `yanshi-ui` | React + Vite + TypeScript SPA embedded by the server at `/ui` (run list, DAG status, event replay, SSE). |

`yanshi-config` is the shared config-first workflow-assembly crate (extracted
from `yanshi-cli` by P-A2.4): the YAML workflow schema (`config::v2`), the
`executor` that compiles it into an `yanshi-core` `Flow`
(`build_flow_from_yaml`), and the `diagnostics` report builder behind
`yanshi doctor` / the server's `/v1/diagnostics`. Both `yanshi-cli` (which
re-exports `config` / `executor` under their original paths) and
`yanshi-server` depend on it, so the gateway no longer depends on the CLI
binary crate.

## CLI Surface

Current top-level commands are:

```bash
yanshi workflow run|validate|debug
yanshi workflow dynamic --goal ... --model ...   # LLM authors a plan, governed execution
yanshi config init|show|validate
yanshi llm models
yanshi mcp list-tools|call-tool|list-resources
yanshi skill init|install|validate|inspect|run|chat|list|list-tools|test|index|marketplace
yanshi marketplace search|install|update|verify
yanshi plugin install|list|inspect|uninstall   # when built with the plugin feature
yanshi trace replay|tui
yanshi audio asr|tts
yanshi image generate|understand
yanshi rag ops search|index|collections         # operator vector-store ops (rag feature)
yanshi rag eval                                  # retriever eval harness (rag feature)
```

The old bare prompt/chat command is not part of the public CLI. Interactive model
use should go through Skills, agents, or workflows.

## Configuration And Secrets

The CLI reads model configuration from `~/.yanshi/models.yml`, falling back to
bundled defaults when no user config exists. Secret values belong in the process
environment or `~/.yanshi/.env`; model entries should reference them by
environment variable name instead of storing raw keys.

Useful commands:

```bash
yanshi config init
yanshi config show models
yanshi config show providers
yanshi config validate
yanshi llm models --provider openai --detailed
```

## Workflow YAML Contract

Config-first workflows use `FlowDefinitionV2`:

```yaml
name: Example
inputs:
  topic:
    description: Topic to process
    required: false
    default: "Yanshi"
nodes:
  - id: render
    type: template
    parameters:
      template: "Explain {{topic}}"
  - id: answer
    type: llm
    dependencies: [render]
    input_mapping:
      prompt: "{{ nodes.render.outputs.output }}"
    parameters:
      model: gpt-4o-mini
```

Each node has `id`, `type`, optional `dependencies`, optional `input_mapping`,
optional `run_if`, and a `parameters` map. `yanshi workflow validate` checks
node support, required parameters, basic parameter types, dependency references,
and supported `input_mapping` expressions before execution.

See [WORKFLOW_SCHEMA.md](WORKFLOW_SCHEMA.md) for the current node parameter table.

## Persistence And Observability

- Workflow run artifacts default to `~/.yanshi/runs`; override with
  `yanshi workflow run --run-dir <dir>` or `YANSHI_RUN_DIR`.
- Trace files default to `~/.yanshi/traces`; inspect them with
  `yanshi trace replay` or `yanshi trace tui`.
- Checkpoint recovery preserves completed workflow node outputs and serialized
  agent step history so interrupted runs can resume.

## Related Guides

- [CONFIGURATION.md](CONFIGURATION.md)
- [WORKFLOW_SCHEMA.md](WORKFLOW_SCHEMA.md)
- [AGENT_RUNTIME.md](AGENT_RUNTIME.md)
- [SKILLS.md](SKILLS.md)
- [MCP_SKILLS.md](MCP_SKILLS.md)
- [TRACING_USAGE.md](TRACING_USAGE.md)
- [CHECKPOINT_RECOVERY.md](CHECKPOINT_RECOVERY.md)
