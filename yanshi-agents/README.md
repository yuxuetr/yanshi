# yanshi-agents

Agent-native runtimes for Yanshi (L3). Every runtime implements the
`AgentRuntime` contract from `yanshi-agent-spi` and runs with
`AgentContext`, `RuntimeLimits` and an `AgentCancellationToken`.

| Module | What it provides |
|---|---|
| `react` | `ReActAgent`: tool calling, memory summary, verification, parallel tool batches |
| `plan_execute` | `PlanExecuteAgent`: structured JSON plan, executed step by step |
| `reflection` / `verification` | `ReflectionStrategy` (advisory self-critique), `VerificationStrategy` (gates a final answer) |
| `supervisor` | `HandoffSupervisor`, `BlackboardSupervisor`, `DebateSupervisor` |
| `dynamic` | `compile_plan_to_flow` + `DynamicWorkflowAgent`: LLM-authored `WorkflowPlan` run as a DAG |
| `nodes` / `tools` | `AgentNode` (agent inside a DAG), `WorkflowTool` (DAG exposed as a tool) |
| `eval` | Agent eval harness and pricing table |

## Examples

Runnable without an API key (mock provider):

```bash
cargo run -p yanshi-agents --example agent_native_react
cargo run -p yanshi-agents --example plan_execute_agent
cargo run -p yanshi-agents --example hybrid_workflow_agent
```

Others: `multi_agent_{handoff,blackboard,debate}`, `dynamic_workflow_plan`,
and the extension examples `custom_{runtime,reflection,memory_summary,verification}`.

See `docs/AGENT_SDK.md` for writing your own runtime and
`docs/MULTI_AGENT.md` for the supervisors.
