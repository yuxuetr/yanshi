//! `figure` with `language: d2`: the mock model's first source has a
//! dangling connection, d2's error goes back (with the work directory
//! stripped from it), and the second source renders. Its own binary
//! because the mock's reply queue is process-wide. Needs `d2`:
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

#[tokio::test]
#[ignore = "needs d2"]
async fn d2_errors_are_fed_back_until_the_diagram_renders() {
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
  ]);
  // SAFETY: the only test in this binary; set before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var("YANSHI_MOCK_RESPONSES", replies.to_string());
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let initial: AsyncNodeInputs = [
    ("language", json!("d2")),
    ("model", json!("mock-figure")),
    ("description", json!("sources feed the planner")),
  ]
  .into_iter()
  .map(|(k, v)| (k.to_string(), FlowValue::Json(v)))
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
  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(2)));
  let FlowValue::File { path, .. } = &outputs["fig_svg"] else {
    panic!("fig_svg is a file");
  };
  let svg = std::fs::read_to_string(path).expect("svg");
  assert!(svg.contains("<svg") && svg.contains("Planner"), "{svg}");

  let retries = retries.0.lock().expect("lock").clone();
  assert_eq!(retries.len(), 1, "{retries:?}");
  let error = retries[0]["error"].as_str().unwrap_or_default();
  assert!(error.contains("fig.d2:2:"), "{error}");
  assert!(error.contains("connection missing destination"), "{error}");
  assert!(
    !error.contains(".figure-"),
    "work directory stripped: {error}"
  );
}
