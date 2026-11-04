# Yanshi 偃师

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Documentation](https://img.shields.io/badge/docs-manual-green.svg)](https://yuxuetr.github.io/yanshi/)

> **A multimodal content-production runtime in Rust: sources in (an idea, a URL, files, a recording), finished deliverables out (reports, mind maps, audio overviews, slides, quizzes, infographics, videos, domain diagrams) — built on deterministic DAG workflows, agent-native loops, Skills, MCP tools, memory, tracing and checkpointed recovery.**

**About the name.** In the *Liezi* (列子·汤问), the artificer Yanshi (偃师) presents
King Mu of Zhou with a figure that sings, dances and acts on its own — and, taken
apart, turns out to be nothing but leather, wood, glue and lacquer, assembled
by structure, that "works again as before" once put back together. It is the
oldest Chinese account of a built autonomous agent, and the idea this project
is built on: autonomous agents assembled from explicit, inspectable structure
(workflows, contracts, tools) that can be traced, replayed and recomposed.

**Direction (reset 2026-11-04).** Yanshi is a runtime for turning sources into
finished content, in the shape of NotebookLM Studio but with a richer and
extensible catalogue. Every deliverable runs the same five stages — Sources →
Planner → Generators → Renderers → Artifacts — where the LLM plans the
*content* (outline, which deliverables, which figures) and hand-written DAG
templates do the work, so cost is predictable and quality is testable. Media
is a first-class value (`FlowValue::File`), and renderers (pandoc, TeX, ffmpeg,
mermaid) run as sandboxed external tools. The design is
[`docs/RFC_CONTENT_STUDIO.md`](docs/RFC_CONTENT_STUDIO.md); the active track
is `RoadMap.md` § Y. It is not a coding-agent framework.

Underneath sits the general runtime this README describes: deterministic DAG
workflows for production automation and agent-native loops for planning, tool
use, reflection, memory, and multi-step decision making.

For the maintained snapshot of implemented surfaces, stability boundaries, and
active work, see [docs/CURRENT_STATUS.md](docs/CURRENT_STATUS.md). For the
per-provider LLM capability matrix (streaming, tool calling, multimodal,
context windows, error mapping, rate-limit handling), see
[docs/LLM_PROVIDERS_MATRIX.md](docs/LLM_PROVIDERS_MATRIX.md).

## 🏗️ Architecture: Shared Runtime For Workflows And Agents

Yanshi is built on five core principles:

1. **Directed Acyclic Graph (DAG)**: Workflows are defined as graphs where nodes have explicit dependencies.
2. **Agent-native runtime**: Agents record observe, plan, tool call, tool result, reflection, and final answer steps.
3. **Explicit I/O**: Nodes and tools receive typed inputs and produce typed outputs.
4. **Shared tools and skills**: Built-in tools, script tools, MCP tools, and workflow tools all adapt into one `ToolRegistry`.
5. **Recoverable execution**: Workflow checkpoints preserve node outputs and agent step history so interrupted runs can resume.

## 🤖 Execution Paradigms

The content runtime above is built on **four execution paradigms** sharing one runtime — points on a
single *planning / binding-time* spectrum, from fully-fixed to decided-every-step:

- **Static DAG** — `yanshi-core::Flow` for deterministic production pipelines,
  batch jobs, RAG flows, and business processes (the plan is authored up front).
- **Native agent loop** — `yanshi-agents::ReActAgent` for autonomous
  observe/plan/act loops with tool use, reflection, memory, and runtime guards
  (the plan is decided every step).
- **Dynamic workflow** — an agent *generates* a plan at runtime that compiles to a
  `Flow` and executes deterministically: `yanshi_agents::dynamic::{compile_plan_to_flow,
  DynamicWorkflowAgent}`. One up-front planning decision, then a replayable,
  parallel DAG — the flexibility of an agent with the reliability of a workflow.
- **Harness governance** — `yanshi-harness` wraps a runtime with approval,
  hooks, sandbox, audit, limits, and background tasks.

Two orthogonal axes round it out: a shared **capability substrate** (Tools, MCP,
RAG, Memory, Skills — all paradigms call the same `ToolRegistry`) and the
**governance shell** (harness). The paradigms compose recursively:

- Use **`AgentNode`** to embed an agent *in* a DAG (a Flow step that is an agent).
- Use **`WorkflowTool`** to expose a DAG *as* a tool to an agent.

These unify at a narrow **contract kernel** (`yanshi-value` / `-graph` /
`-store-spi` / `-agent-spi` / `-async-util`): the runtimes never depend on each
other, only on shared contracts. See
[`docs/ARCHITECTURE.md` § Four Execution Paradigms](docs/ARCHITECTURE.md#four-execution-paradigms--mental-model)
for the full three-axis mental model (with an honest model-vs-code status), and
[`docs/RFC_CRATE_ARCHITECTURE.md`](docs/RFC_CRATE_ARCHITECTURE.md) for the kernel design.

## 🎯 Two Ways to Build

### 💻 **Code-First Approach (Core SDK)**
For developers who need maximum power and type-safety.

```rust
use yanshi_core::{
    FlowExt, // brings `flow.run()` into scope (the executor for the graph IR)
    flow::{Flow, GraphNode, NodeType},
    async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
    value::FlowValue,
};
use yanshi_nodes::nodes::llm::LlmNode;
use yanshi_nodes::nodes::template::TemplateNode;
use std::collections::HashMap;
use std::sync::Arc;
use serde_json::json;

#[tokio::main]
async fn main() {
    // 1. Define the nodes in the workflow
    let template_node = GraphNode {
        id: "get_topic".to_string(),
        node_type: NodeType::Standard(Arc::new(TemplateNode::new("get_topic", "A short poem about {{topic}}."))),
        dependencies: vec![],
        input_mapping: None,
        run_if: None,
        initial_inputs: {
            let mut map = HashMap::new();
            map.insert("topic".to_string(), FlowValue::Json(json!("the moon")));
            map
        },
    };

    let llm_node = GraphNode {
        id: "generate_poem".to_string(),
        node_type: NodeType::Standard(Arc::new(LlmNode::default())),
        dependencies: vec!["get_topic".to_string()],
        input_mapping: Some({
            let mut map = HashMap::new();
            map.insert("prompt".to_string(), ("get_topic".to_string(), "output".to_string()));
            map
        }),
        run_if: None,
        initial_inputs: HashMap::new(),
    };

    // 2. Create and run the flow
    let flow = Flow::new(vec![template_node, llm_node]);
    let final_state = flow.run().await.expect("Flow execution failed");

    // 3. Safely access the result
    if let Some(Ok(llm_outputs)) = final_state.get("generate_poem") {
        if let Some(FlowValue::Json(serde_json::Value::String(poem))) = llm_outputs.get("output") {
            println!("Generated poem: {}", poem);
        }
    }
}
```

### 📋 **Configuration-First Approach (CLI)**
For users who prefer declarative, dynamic workflows.

```yaml
# workflow_v2.yml
name: "Calculation Pipeline"
nodes:
  - id: initial_values
    type: template
    parameters:
      template: "{{a}} + {{b}}"
      a: 10
      b: 5

  - id: step_1
    type: llm
    dependencies: ["initial_values"]
    input_mapping:
      prompt: "Calculate {{ nodes.initial_values.outputs.output }}"

  - id: step_2
    type: llm
    dependencies: ["step_1"]
    run_if: "{{ nodes.step_1.outputs.is_valid }}" # Conditional execution
    input_mapping:
      prompt: "Summarize the result: {{ nodes.step_1.outputs.result }}"
```

```bash
# Execute via CLI
yanshi workflow run workflow_v2.yml
```

## 🚀 Key V2 Features

- **`FlowValue` Enum**: A unified, type-safe wrapper for multi-modal data (JSON, files, URLs).
- **Stateless Nodes**: Nodes are self-contained and receive all data via an `inputs` map.
- **DAG Execution Engine**: Workflows are defined with explicit `dependencies` for clear, traceable execution.
- **Explicit Input Mapping**: The `input_mapping` field provides full control over data flow between nodes.
- **Powerful Control Flow**: Native support for conditional execution (`run_if`), `while` loops, and `map` iteration (with parallel execution support).
- **File-based Persistence**: Each workflow run is saved to a unique directory for debugging and auditing; `workflow run --run-dir` or `YANSHI_RUN_DIR` can make the base path explicit.
- **Agent Runtime**: ReAct-compatible runtime with structured steps/events, stop reasons, reflection hooks, runtime guards, and golden test coverage.
- **Hybrid DAG + Agent Execution**: `AgentNode` embeds agents in DAGs; `WorkflowTool` lets agents call DAG workflows.
- **Skills + MCP Tools**: Skills can declare MCP servers, discover tools, expose schemas, and call them through the unified tool registry.
- **Tracing Across Boundaries**: Workflow, agent, tool, and MCP calls can be linked through structured trace events.
- **Checkpointed Recovery**: Workflow resume skips completed nodes and preserves serialized agent step history.

## ✨ New in v0.2.0: Production-Ready Stability

Yanshi v0.2.0 introduces comprehensive stability and observability improvements for production workflows. The 2026-05-24 deep audit (`docs/audit/`) flagged several gaps in the retry / timeout / checkpoint paths; those are tracked in `TODOs.md` under the Q2 wave and progressively hardened — most recently `ExponentialBackoff` jitter (Q2.4.2), retry error-cause preservation (Q2.4.3), and graceful Ctrl-C / SIGTERM handling for CLI + worker + server (Q3.1.1 / Q3.1.2 / Q3.1.3). The examples below reflect the post-audit shape; see `TODOs.md` "Quality Gates" for any open items.

### 🔄 Retry Mechanism
Automatic retry with configurable strategies for handling transient failures.

```rust
use yanshi_core::{RetryPolicy, RetryStrategy, execute_with_retry};

let policy = RetryPolicy::builder()
    .max_attempts(3)
    .strategy(RetryStrategy::ExponentialBackoff {
        initial_delay_ms: 100,
        max_delay_ms: 5000,
        multiplier: 2.0,
        jitter: true,
    })
    .build();

let result = execute_with_retry(&policy, "api_call", || async {
    api_client.fetch_data().await
}).await?;
```

### 📝 Error Context Enhancement
Detailed error tracking with full execution context and history.

```rust
use yanshi_core::{execute_with_retry_and_context, ErrorContext};

let result = execute_with_retry_and_context(
    &policy, "run_123", "process_node", Some("processor"),
    || async { workflow.execute().await }
).await;

match result {
    Ok(value) => println!("Success: {:?}", value),
    Err((error, context)) => {
        eprintln!("{}", context.detailed_report());
    }
}
```

### 🔍 Workflow Debugging Tools
Interactive workflow debugging and inspection via CLI.

```bash
# Validate workflow configuration
yanshi workflow debug workflow.yml --validate

# Visualize DAG structure
yanshi workflow debug workflow.yml --visualize

# Analyze complexity and bottlenecks
yanshi workflow debug workflow.yml --analyze

# Dry-run without execution
yanshi workflow debug workflow.yml --dry-run --verbose
```

### 💾 Resource Management
Configurable memory limits with automatic cleanup and monitoring.

```rust
use yanshi_core::{ResourceLimits, StateMonitor};

let limits = ResourceLimits::builder()
    .max_state_size(100 * 1024 * 1024)  // 100 MB
    .max_value_size(10 * 1024 * 1024)   // 10 MB
    .cleanup_threshold(0.8)              // Clean at 80%
    .auto_cleanup(true)
    .build();

let monitor = StateMonitor::new(limits);

// Track allocations
monitor.record_allocation("data", data.len());

// Automatic cleanup when needed
if monitor.should_cleanup() {
    monitor.cleanup(0.5)?;  // Clean to 50%
}
```

### ⏱️ Timeout Control
Comprehensive timeout management for async operations.

```rust
use yanshi_core::timeout::{with_timeout, TimeoutConfig};

let config = TimeoutConfig::production();

let result = with_timeout(
    long_running_operation(),
    config.workflow_execution_timeout
).await?;
```

### 🏥 Health Checks
Kubernetes-compatible health and readiness monitoring.

```rust
use yanshi_core::health::{HealthChecker, HealthStatus};

let checker = HealthChecker::new();

checker.add_check("database", || {
    Box::pin(async {
        match db.ping().await {
            Ok(_) => Ok(HealthStatus::Healthy),
            Err(e) => Err(format!("DB error: {}", e)),
        }
    })
}).await;

let report = checker.check_health().await;
```

### 💾 Checkpoint Recovery
Persistent workflow state for fault tolerance and resumability.

```rust
use yanshi_core::checkpoint::{CheckpointManager, CheckpointConfig};

let config = CheckpointConfig::default();
let manager = CheckpointManager::new(config)?;

// Save checkpoint after node execution
manager.save_checkpoint("workflow_123", "node1", &state).await?;

// Resume from checkpoint
if let Some(checkpoint) = manager.load_latest_checkpoint("workflow_123").await? {
    println!("Resuming from: {}", checkpoint.last_completed_node);
}
```

### 📊 Performance Guarantees

All features meet strict performance targets:
- Retry overhead: **< 5ms** per retry ✅
- Resource limit enforcement: **< 100μs** per operation ✅
- Error context creation: **< 1ms** ✅
- State monitor operations: **< 10μs** ✅
- Combined overhead: **< 1ms** ✅
- Timeout overhead: **< 100μs** ✅
- Health checks: **< 1ms** single, **< 10ms** multiple ✅
- Checkpoint save: **< 10ms** small, **< 50ms** large ✅
- Checkpoint load: **< 10ms** ✅

### 🎯 Quality Gates

Use the workspace tests and targeted crate checks as the source of truth for
current quality status:

```bash
cargo test --workspace
cargo check --workspace
```

## 📚 Documentation

### Core Documentation
- **[Docs Index](docs/README.md)**: Current documentation entry point.
- **[RoadMap](RoadMap.md)**: Current direction — the Content Studio runtime (track Y) — plus deferred tracks with their re-evaluate conditions.
- **[Content Studio RFC](docs/RFC_CONTENT_STUDIO.md)**: Five-stage model, artifact and `Deliverable` / `Citation` contracts, notebook entity, renderer strategy, decision log.
- **[Project Evaluation 2026-11-04](docs/PROJECT_EVALUATION_2026-11-04.md)**: Latest full evaluation, re-weighted for the content-production goal.
- **[Architecture](docs/ARCHITECTURE.md)**: The four-paradigm mental model (with an honest model-vs-code status), workspace layout, runtime model, CLI surface, and persistence.
- **[Crate Architecture RFC](docs/RFC_CRATE_ARCHITECTURE.md)** + **[Evaluation](docs/ARCHITECTURE_EVALUATION_2026-06-20.md)**: The contract-kernel design (narrow-waist `value`/`graph`/`store-spi`/`agent-spi`/`async-util` + eight dependency laws, enforced by `cargo xtask check-arch`) and its dependency-graph validation.
- **[Configuration](docs/CONFIGURATION.md)**: CLI config, secrets, workflow YAML, and run directories.
- **[Workflow Schema](docs/WORKFLOW_SCHEMA.md)**: Implemented config-first workflow validation contract.
- **[Agent Runtime](docs/AGENT_RUNTIME.md)**: Runtime boundary, core types, ReAct trace contract, and DAG interop.
- **[Skills](docs/SKILLS.md)**: User-facing guide for packaging agent instructions, tools, MCP servers, knowledge, and memory.
- **[Extensibility Model](docs/EXTENSIBILITY_MODEL.md)**: Boundaries between Rust nodes, Tools, MCP, Skills, Skill catalogs, and future plugins.
- **[Release Checklist](docs/RELEASE_CHECKLIST.md)**: Manual quality gate before tagging or publishing.

### v0.2.0 Feature Guides
- **[Retry Mechanism](docs/RETRY_MECHANISM.md)**: Comprehensive guide to retry configuration and strategies
- **[Workflow Debugging](docs/WORKFLOW_DEBUGGING.md)**: CLI debugging tools and workflow visualization
- **[Resource Management (archived)](docs/archive/RESOURCE_MANAGEMENT_2026-08-12-pre-w5.3.md)**: Superseded by an advisory-only `ResourceLimits` warning (see `yanshi-core/src/scheduler.rs`'s `FlowExecutionConfig::resource_limits`) — the eviction/monitoring design this doc described was never wired in and part of it was found unsafe (W5.3)
- **[Timeout Control](docs/TIMEOUT_CONTROL.md)**: Operation timeout management and configuration
- **[Health Checks](docs/HEALTH_CHECKS.md)**: Kubernetes-compatible health and readiness monitoring
- **[Checkpoint Recovery](docs/CHECKPOINT_RECOVERY.md)**: Workflow state persistence and fault tolerance
- **[Migration Guide v0.2.0](docs/MIGRATION_GUIDE_v0.2.0.md)**: Upgrade guide from v0.1.0 to v0.2.0
- **[Release Notes v0.2.0](docs/RELEASE_NOTES_v0.2.0.md)**: Complete changelog and improvements

### Examples
- `yanshi-core/examples/retry_example.rs`: Retry mechanism demonstrations
- `yanshi-core/examples/fixed_dag_workflow.rs`: Deterministic fixed DAG workflow example
- `yanshi-core/examples/resource_management_example.rs`: Resource monitoring examples
- `yanshi-agents/examples/agent_native_react.rs`: Self-contained ReAct agent-native runtime example
- `yanshi-agents/examples/react_agent.rs`: ReAct agent runtime example
- `yanshi-agents/examples/hybrid_workflow_agent.rs`: DAG + Agent hybrid example
- `yanshi-agents/examples/dynamic_workflow_spike.rs`: Dynamic workflow — a runtime generates a `Flow` and the engine executes it
- `yanshi-agents/examples/dynamic_workflow_plan.rs`: Dynamic workflow from a declarative JSON plan, compiled to a parallel `Flow` of tool calls
- `yanshi-skills/examples/skill_calls_mcp_tool.rs`: Skill-to-MCP tool call example
- `examples/skills/mcp-basic`: Minimal Skill with MCP server configuration
- `yanshi-cli/examples/workflows/`: Complete workflow examples including AI research assistant

## 📦 Installation

### For Developers (Core SDK)
```toml
[dependencies]
yanshi-core = { path = "yanshi-core" }
yanshi-nodes = { path = "yanshi-nodes" }
```

### For CLI Usage
```bash
cargo install --path yanshi-cli
yanshi --help
```

## 🛣️ Development Plan

See the current framework direction in [RoadMap.md](RoadMap.md). Keep active docs tied to implemented behavior; time-boxed plans and TODO trackers should stay outside `docs/` unless they have been folded into stable guides.
