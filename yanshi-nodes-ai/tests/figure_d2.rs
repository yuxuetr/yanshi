//! `figure` with `language: d2`. First the mock model's source has a
//! dangling connection: d2's error goes back (with the work directory
//! stripped from it) and the second source renders. Then, under `domain:
//! block`, a source that renders but draws a link backwards is sent back
//! by the domain check, and the corrected one renders. One test in its
//! own binary because the mock's reply queue is process-wide. Needs `d2`:
//! `cargo test -p yanshi-nodes-ai --test figure_d2 -- --ignored`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yanshi_core::{
  EventListener, FlowExecutionConfig, FlowExt, WorkflowEvent,
  async_node::AsyncNodeInputs,
  flow::{Flow, GraphNode, NodeType},
  value::FlowValue,
};
use yanshi_nodes_ai::FigureNode;

#[derive(Default)]
struct Retries(Mutex<Vec<Value>>);

impl EventListener for Retries {
  fn on_event(&self, event: &WorkflowEvent) {
    if let WorkflowEvent::NodeProgress { kind, detail, .. } = event
      && kind == "figure_retry"
    {
      self.0.lock().expect("lock").push(detail.clone());
    }
  }
}

/// Run one figure node; its outputs, its SVG (read before the run
/// directory goes) and its retries.
async fn draw(inputs: Value) -> (HashMap<String, FlowValue>, String, Vec<Value>) {
  let initial: AsyncNodeInputs = inputs
    .as_object()
    .expect("object")
    .iter()
    .map(|(k, v)| (k.clone(), FlowValue::Json(v.clone())))
    .collect();
  let node = GraphNode {
    id: "fig".to_string(),
    node_type: NodeType::Standard(Arc::new(FigureNode::new("fig"))),
    dependencies: vec![],
    input_mapping: None,
    run_if: None,
    initial_inputs: initial,
  };
  let retries = Arc::new(Retries::default());
  let runs = tempfile::TempDir::new().expect("tempdir");
  let state = Flow::new(vec![node])
    .with_event_listener(retries.clone())
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(runs.path()),
    )
    .await
    .expect("run");
  let outputs = match &state["fig"] {
    Ok(outputs) => outputs.clone(),
    Err(e) => panic!("figure failed: {e}"),
  };
  let FlowValue::File { path, .. } = &outputs["fig_svg"] else {
    panic!("fig_svg is a file");
  };
  let svg = std::fs::read_to_string(path).expect("svg");
  let retries = retries.0.lock().expect("lock").clone();
  (outputs, svg, retries)
}

#[tokio::test]
#[ignore = "needs d2"]
async fn d2_errors_and_block_violations_are_fed_back_until_the_diagram_renders() {
  let dir = tempfile::TempDir::new().expect("tempdir");
  let registry = dir.path().join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-figure:\n    vendor: mock\n    type: text\n    model_id: mock-figure\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let replies = json!([
    "```d2\nsources: Sources\nsources -> \n```",
    "```d2\nsources: Sources\nplanner: Planner\nsources -> planner: notes\n```",
    "```d2\ndirection: right\nctl: Controller\nplant: Plant\nplant -> ctl\n```",
    "```d2\ndirection: right\nctl: Controller\nplant: Plant\nctl -> plant: u\n```",
  ]);
  // SAFETY: the only test in this binary; set before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var("YANSHI_MOCK_RESPONSES", replies.to_string());
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let (outputs, svg, retries) = draw(json!({
    "language": "d2",
    "model": "mock-figure",
    "description": "sources feed the planner",
  }))
  .await;
  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(2)));
  assert!(svg.contains("<svg") && svg.contains("Planner"), "{svg}");
  assert_eq!(retries.len(), 1, "{retries:?}");
  let error = retries[0]["error"].as_str().unwrap_or_default();
  assert!(error.contains("fig.d2:2:"), "{error}");
  assert!(error.contains("connection missing destination"), "{error}");
  assert!(
    !error.contains(".figure-"),
    "work directory stripped: {error}"
  );

  let (outputs, svg, retries) = draw(json!({
    "language": "d2",
    "domain": "block",
    "model": "mock-figure",
    "description": "a controller driving a plant",
    "spec": {
      "blocks": [{"id": "ctl", "label": "Controller"}, {"id": "plant", "label": "Plant"}],
      "links": [{"from": "ctl", "to": "plant", "label": "u"}]
    },
  }))
  .await;
  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(2)));
  assert!(svg.contains("Controller"), "{svg}");
  assert!(
    outputs["fig_source"]
      == FlowValue::Json(json!(
        "direction: right\nctl: Controller\nplant: Plant\nctl -> plant: u"
      )),
    "{:?}",
    outputs["fig_source"]
  );
  assert_eq!(retries.len(), 1, "{retries:?}");
  let error = retries[0]["error"].as_str().unwrap_or_default();
  assert!(
    error.contains("the link ctl -> plant is missing: draw it as `ctl -> plant`"),
    "{error}"
  );
}
