//! Y8.4: the `chemistry` and `block` figure domains. In each, the mock
//! model's first source compiles in principle but leaves something out;
//! the domain check sends it back and the second source renders through
//! lualatex. One test (the mock's reply queue is process-wide). Needs TeX
//! Live (chemfig) and poppler:
//! `cargo test -p yanshi-nodes-ai --test figure_domains -- --ignored`.

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

async fn draw(inputs: Value) -> (HashMap<String, FlowValue>, Vec<Value>) {
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
    Err(e) => panic!(
      "figure failed: {e}\nretries: {:#?}",
      retries.0.lock().expect("lock")
    ),
  };
  if let FlowValue::File { path, .. } = &outputs["fig_svg"] {
    let svg = std::fs::read_to_string(path).expect("svg");
    assert!(svg.contains("<svg"), "rendered SVG");
  } else {
    panic!("fig_svg is a file");
  }
  let retries = retries.0.lock().expect("lock").clone();
  (outputs, retries)
}

fn latex(body: &str, packages: &str) -> String {
  format!(
    "```latex\n\\documentclass[border=4pt]{{standalone}}\n{packages}\n\\begin{{document}}\n{body}\n\\end{{document}}\n```"
  )
}

#[tokio::test]
#[ignore = "needs TeX Live (lualatex, chemfig) and poppler (pdftocairo)"]
async fn a_figure_that_leaves_something_out_is_sent_back_and_redrawn() {
  let dir = std::env::temp_dir().join(format!("yanshi-figure-domains-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-figure:\n    vendor: mock\n    type: text\n    model_id: mock-figure\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let chem = "\\usepackage{chemfig}";
  let tikz = "\\usepackage{tikz}\n\\usetikzlibrary{positioning,arrows.meta}";
  let replies = json!([
    // Chemistry: "ethanol" drawn with the acid's two oxygens.
    latex(
      r"\schemestart \chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{ethanol} \arrow{->[\chemfig{O_2}]} \chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{acetic acid} \schemestop",
      chem
    ),
    latex(
      r"\schemestart \chemname{\chemfig{H_3C-CH_2-OH}}{ethanol} \arrow{->[\chemfig{O_2}]} \chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{acetic acid} \schemestop",
      chem
    ),
    // Block: the feedback arrow is missing.
    latex(
      "\\begin{tikzpicture}\n\\node[draw] (ctl) {Controller};\n\\node[draw, right=of ctl] (plant) {Plant};\n\\draw[-{Stealth}] (ctl) -- (plant);\n\\end{tikzpicture}",
      tikz
    ),
    latex(
      "\\begin{tikzpicture}\n\\node[draw] (ctl) {Controller};\n\\node[draw, right=of ctl] (plant) {Plant};\n\\draw[-{Stealth}] (ctl) -- (plant);\n\\draw[-{Stealth}] (plant.south) -- ++(0,-0.8) -| (ctl.south);\n\\end{tikzpicture}",
      tikz
    ),
  ]);
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var("YANSHI_MOCK_RESPONSES", replies.to_string());
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let (outputs, retries) = draw(json!({
    "language": "tikz",
    "domain": "chemistry",
    "model": "mock-figure",
    "description": "the oxidation of ethanol to acetic acid",
    "spec": {"reaction": true, "molecules": [
      {"name": "ethanol", "formula": "C2H6O"},
      {"name": "acetic acid", "formula": "C2H4O2"}
    ]}
  }))
  .await;
  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(2)));
  assert_eq!(retries.len(), 1);
  let error = retries[0]["error"].as_str().expect("error");
  assert!(error.contains("ethanol (C2H6O) should have O1"), "{error}");

  let (outputs, retries) = draw(json!({
    "language": "tikz",
    "domain": "block",
    "model": "mock-figure",
    "description": "a feedback control loop",
    "spec": {
      "blocks": [{"id": "ctl", "label": "Controller"}, {"id": "plant", "label": "Plant"}],
      "links": [{"from": "ctl", "to": "plant"}, {"from": "plant", "to": "ctl"}]
    }
  }))
  .await;
  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(2)));
  assert_eq!(retries.len(), 1);
  let error = retries[0]["error"].as_str().expect("error");
  assert!(
    error.contains("the link plant -> ctl is missing"),
    "{error}"
  );
}
