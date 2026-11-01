//! P-A2.2 — the harness governs a deterministic `Flow` run.
//!
//! These tests prove two things end-to-end:
//! 1. `run_flow` brackets a real `Flow` execution with the Harness envelope
//!    (`session_started` with `runtime = flow` … `stopped`).
//! 2. Tool calls *inside* the Flow's nodes are governed: with the node registry
//!    wrapped by a `HookConfig` sharing the harness seq counter + sinks, an
//!    `AutoDeny` approval provider blocks a mutating tool — the approval events
//!    land on the same stream and the Flow run fails.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_core::CoreFlowRunner;
use yanshi_graph::{
  AsyncNode, AsyncNodeInputs, AsyncNodeResult, Flow, FlowValue, GraphNode, NodeType, YanshiError,
};
use yanshi_harness::{
  AutoAllowApprovalProvider, AutoDenyApprovalProvider, HarnessEventBody, HarnessFlowRunOptions,
  HarnessProfile, HarnessRuntime, HookConfig, InMemoryEventSink, SinkChain, StopReason,
  wrap_registry,
};
use yanshi_tools::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput, ToolRegistry};

/// A mutating (NonIdempotent) tool — under `Production` profile this escalates
/// to the approval gate, so it exercises the governance path.
struct WriterTool;

#[async_trait]
impl Tool for WriterTool {
  fn name(&self) -> &str {
    "writer"
  }
  fn description(&self) -> &str {
    "writes something (mutating)"
  }
  fn parameters_schema(&self) -> Value {
    json!({ "type": "object" })
  }
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin().with_idempotency(ToolIdempotency::NonIdempotent)
  }
  async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::success("wrote"))
  }
}

/// A node that invokes a tool from its (wrapped) registry — the seam at which
/// harness governance applies inside a Flow.
struct CallToolNode {
  registry: Arc<ToolRegistry>,
  tool: String,
}

#[async_trait]
impl AsyncNode for CallToolNode {
  async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let output = self
      .registry
      .execute(&self.tool, json!({}))
      .await
      .map_err(|e| YanshiError::NodeExecutionFailed {
        message: format!("tool '{}' failed: {e}", self.tool),
      })?;
    if output.is_error {
      return Err(YanshiError::NodeExecutionFailed {
        message: format!("tool '{}' returned an error", self.tool),
      });
    }
    Ok(HashMap::from([(
      "result".to_string(),
      FlowValue::Json(json!(output.content)),
    )]))
  }
}

/// A no-op node that just succeeds — used to observe `step_started` events
/// without involving the tool/approval path.
struct NoopNode;

#[async_trait]
impl AsyncNode for NoopNode {
  async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    Ok(HashMap::new())
  }
}

fn standard_node(id: &str, node: Arc<dyn AsyncNode>, deps: Vec<String>) -> GraphNode {
  GraphNode {
    id: id.to_string(),
    node_type: NodeType::Standard(node),
    dependencies: deps,
    input_mapping: None,
    run_if: None,
    initial_inputs: HashMap::new(),
  }
}

/// Build a one-node Flow whose node calls `writer` through `registry`.
fn single_tool_flow(registry: Arc<ToolRegistry>) -> Flow {
  Flow::new(vec![GraphNode {
    id: "call".to_string(),
    node_type: NodeType::Standard(Arc::new(CallToolNode {
      registry,
      tool: "writer".to_string(),
    })),
    dependencies: Vec::new(),
    input_mapping: None,
    run_if: None,
    initial_inputs: HashMap::new(),
  }])
}

#[tokio::test]
async fn run_flow_emits_session_started_flow_runtime_and_stopped_completed() {
  let sink = Arc::new(InMemoryEventSink::new());
  let mut harness = HarnessRuntime::for_flow().with_event_sink(sink.clone());

  // Local profile + AutoAllow: the writer is silently allowed (no escalation).
  let hook = HookConfig::new(
    "sess-allow",
    Arc::new(AutoAllowApprovalProvider::new()),
    SinkChain::new().push(sink.clone()),
  )
  .with_seq_counter(harness.seq_counter());
  let registry = wrap_registry(
    {
      let mut r = ToolRegistry::new();
      r.register(Arc::new(WriterTool));
      r
    },
    hook,
  );

  let flow = single_tool_flow(registry);
  let result = harness
    .run_flow(
      &flow,
      Arc::new(CoreFlowRunner::serial()),
      HashMap::new(),
      HarnessFlowRunOptions::default(),
    )
    .await
    .expect("run_flow ok");

  let events = sink.snapshot().await;
  // First event is session_started with runtime = flow.
  match &events.first().expect("at least one event").body {
    HarnessEventBody::SessionStarted(p) => {
      assert_eq!(p.runtime.as_str(), "flow", "runtime kind must be flow");
    }
    other => panic!("expected session_started first, got {other:?}"),
  }
  // Last event is stopped(Completed).
  match &events.last().expect("terminal event").body {
    HarnessEventBody::Stopped(p) => assert_eq!(p.reason, StopReason::Completed),
    other => panic!("expected stopped last, got {other:?}"),
  }
  // Seqs are monotonic starting at 0.
  for (i, ev) in events.iter().enumerate() {
    assert_eq!(ev.seq, i as u64, "seq must be gap-free monotonic");
  }
  assert_eq!(result.final_event_seq, events.last().unwrap().seq);
  assert!(matches!(
    result.outcome,
    yanshi_harness::FlowRunOutcome::Completed(_)
  ));
}

#[tokio::test]
async fn run_flow_emits_step_started_per_node() {
  let sink = Arc::new(InMemoryEventSink::new());
  let mut harness = HarnessRuntime::for_flow().with_event_sink(sink.clone());

  // Two nodes: "first" then "second" (depends on first), so order is fixed.
  let flow = Flow::new(vec![
    standard_node("first", Arc::new(NoopNode), Vec::new()),
    standard_node("second", Arc::new(NoopNode), vec!["first".to_string()]),
  ]);

  harness
    .run_flow(
      &flow,
      Arc::new(CoreFlowRunner::serial()),
      HashMap::new(),
      HarnessFlowRunOptions::default(),
    )
    .await
    .expect("run_flow ok");

  let events = sink.snapshot().await;
  let step_types: Vec<String> = events
    .iter()
    .filter_map(|e| match &e.body {
      HarnessEventBody::StepStarted(p) => Some(p.step_type.clone()),
      _ => None,
    })
    .collect();
  assert!(
    step_types.contains(&"node:first".to_string()),
    "expected a step_started for node 'first', got {step_types:?}"
  );
  assert!(
    step_types.contains(&"node:second".to_string()),
    "expected a step_started for node 'second', got {step_types:?}"
  );
  // Still bracketed correctly + gap-free monotonic.
  assert!(matches!(
    events.first().unwrap().body,
    HarnessEventBody::SessionStarted(_)
  ));
  assert!(matches!(
    events.last().unwrap().body,
    HarnessEventBody::Stopped(_)
  ));
  for (i, ev) in events.iter().enumerate() {
    assert_eq!(ev.seq, i as u64, "seq must be gap-free monotonic");
  }
}

#[tokio::test]
async fn run_flow_governs_tool_calls_autodeny_blocks_and_fails() {
  let sink = Arc::new(InMemoryEventSink::new());
  let mut harness = HarnessRuntime::for_flow().with_event_sink(sink.clone());

  // Production profile auto-escalates the NonIdempotent writer to approval;
  // AutoDeny refuses it. The denial must reach the Flow's tool call.
  let hook = HookConfig::new(
    "sess-deny",
    Arc::new(AutoDenyApprovalProvider::new()),
    SinkChain::new().push(sink.clone()),
  )
  .with_profile(HarnessProfile::Production)
  .with_seq_counter(harness.seq_counter());
  let registry = wrap_registry(
    {
      let mut r = ToolRegistry::new();
      r.register(Arc::new(WriterTool));
      r
    },
    hook,
  );

  let flow = single_tool_flow(registry);
  let result = harness
    .run_flow(
      &flow,
      Arc::new(CoreFlowRunner::serial()),
      HashMap::new(),
      HarnessFlowRunOptions::default(),
    )
    .await
    .expect("run_flow ok (the Flow fails, but run_flow itself returns Ok)");

  // The governed tool call was denied, so the node — and the Flow — failed.
  assert!(
    matches!(result.outcome, yanshi_harness::FlowRunOutcome::Failed(_)),
    "denied tool call must fail the Flow run"
  );

  let events = sink.snapshot().await;
  // Governance reached the Flow's tool call: an approval was requested on the
  // same harness event stream, bracketed by session_started / stopped(Failed).
  assert!(
    events
      .iter()
      .any(|e| matches!(e.body, HarnessEventBody::ApprovalRequested(_))),
    "an approval_requested event must appear on the governed stream"
  );
  match &events.last().expect("terminal event").body {
    HarnessEventBody::Stopped(p) => assert_eq!(p.reason, StopReason::Failed),
    other => panic!("expected stopped(Failed) last, got {other:?}"),
  }
}
