//! Y3.4 acceptance for the `figure` node.
//!
//! - A mock model writes broken SVG twice, then valid SVG: the node
//!   succeeds on attempt 3 and the run's event stream carries two
//!   `node.progress` events of kind `figure_retry`.
//! - `#[ignore]`d (need TeX Live / typst): a circuitikz source and a cetz
//!   source render to non-empty SVG under the OS sandbox.

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

const GOOD_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="teal"/></svg>"#;

#[derive(Default)]
struct Progress(Mutex<Vec<(String, Value)>>);

impl EventListener for Progress {
  fn on_event(&self, event: &WorkflowEvent) {
    if let WorkflowEvent::NodeProgress { kind, detail, .. } = event {
      self
        .0
        .lock()
        .expect("lock")
        .push((kind.clone(), detail.clone()));
    }
  }
}

async fn run_figure(
  inputs: Vec<(&str, Value)>,
) -> (
  HashMap<String, FlowValue>,
  Vec<(String, Value)>,
  tempfile::TempDir,
) {
  let initial: AsyncNodeInputs = inputs
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
  let progress = Arc::new(Progress::default());
  let runs = tempfile::TempDir::new().expect("tempdir");
  let state = Flow::new(vec![node])
    .with_event_listener(progress.clone())
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(runs.path()),
    )
    .await
    .expect("run returns its state");
  let outputs = match &state["fig"] {
    Ok(outputs) => outputs.clone(),
    Err(e) => panic!("figure failed: {e}"),
  };
  let events = progress.0.lock().expect("lock").clone();
  (outputs, events, runs)
}

fn svg_of(outputs: &HashMap<String, FlowValue>) -> String {
  match &outputs["fig_svg"] {
    FlowValue::File { path, mime_type } => {
      assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
      std::fs::read_to_string(path).expect("svg file")
    }
    other => panic!("expected File, got {other:?}"),
  }
}

#[tokio::test]
async fn render_errors_are_fed_back_until_the_figure_renders() {
  let dir = std::env::temp_dir().join(format!("yanshi-figure-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-figure:\n    vendor: mock\n    type: text\n    model_id: mock-figure\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let replies = json!([
    "```svg\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\"\n```",
    "```svg\n<html><body>not svg</body></html>\n```",
    format!("Here you go:\n```svg\n{GOOD_SVG}\n```"),
  ]);
  // SAFETY: this test binary's only env writers are here, before any
  // model call; the other tests in it never touch the LLM.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var("YANSHI_MOCK_RESPONSES", replies.to_string());
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let (outputs, events, _runs) = run_figure(vec![
    ("language", json!("svg")),
    ("model", json!("mock-figure")),
    ("description", json!("a teal bar")),
    ("png", json!(true)),
  ])
  .await;

  assert_eq!(outputs["fig_attempts"], FlowValue::Json(json!(3)));
  assert_eq!(svg_of(&outputs), GOOD_SVG);
  assert!(matches!(outputs["fig_png"], FlowValue::File { .. }));
  let retries: Vec<&Value> = events
    .iter()
    .filter(|(kind, _)| kind == "figure_retry")
    .map(|(_, detail)| detail)
    .collect();
  assert_eq!(retries.len(), 2, "two failed renders: {events:?}");
  assert_eq!(retries[0]["attempt"], 1);
  assert_eq!(retries[1]["attempt"], 2);
  assert!(
    retries[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
    "the render error is reported"
  );
}

#[tokio::test]
#[ignore = "needs TeX Live (lualatex) and poppler (pdftocairo)"]
async fn circuitikz_source_renders_to_svg() {
  let source = r"\documentclass[border=4pt]{standalone}
\usepackage{circuitikz}
\begin{document}
\begin{circuitikz}
\draw (0,0) to[R=$R_1$] (2,0) to[C=$C_1$] (2,-2) -- (0,-2) to[V=$V_s$] (0,0);
\end{circuitikz}
\end{document}";
  let (outputs, events, runs) =
    run_figure(vec![("language", json!("tikz")), ("source", json!(source))]).await;
  let svg = svg_of(&outputs);
  assert!(
    svg.contains("<svg") && svg.matches("<path").count() > 3,
    "{svg}"
  );
  assert!(events.is_empty(), "no retries for a valid source");
  let leftovers = walk(runs.path())
    .into_iter()
    .filter(|p| p.to_string_lossy().contains(".figure-"))
    .count();
  assert_eq!(leftovers, 0, "the work directory is removed");
}

#[tokio::test]
#[ignore = "needs typst with @preview/cetz:0.3.4 in its package cache (the sandbox has no network)"]
async fn cetz_source_renders_to_svg() {
  let source = r#"#import "@preview/cetz:0.3.4": canvas, draw
#set page(width: auto, height: auto, margin: 4pt)
#canvas({
  draw.circle((0, 0), radius: 1)
  draw.line((0, 0), (1, 1))
})"#;
  let (outputs, _, _runs) = run_figure(vec![
    ("language", json!("typst")),
    ("source", json!(source)),
  ])
  .await;
  let svg = svg_of(&outputs);
  assert!(svg.contains("<svg") && svg.contains("<path"), "{svg}");
}

/// Y3.5 acceptance: RDKit draws caffeine inside the code_exec container
/// tier. Needs a container engine and the image from
/// `scripts/figure_python/Containerfile`:
/// `container build -t yanshi-figure-python:latest scripts/figure_python`.
#[tokio::test]
#[ignore = "needs a container engine and the yanshi-figure-python image"]
async fn rdkit_draws_caffeine_in_the_container() {
  let source = r#"from rdkit import Chem
from rdkit.Chem.Draw import rdMolDraw2D

caffeine = Chem.MolFromSmiles("CN1C=NC2=C1C(=O)N(C(=O)N2C)C")
drawer = rdMolDraw2D.MolDraw2DSVG(400, 300)
drawer.DrawMolecule(caffeine)
drawer.FinishDrawing()
with open("figure.svg", "w") as out:
    out.write(drawer.GetDrawingText())
"#;
  let (outputs, events, _runs) = run_figure(vec![
    ("language", json!("python")),
    ("source", json!(source)),
  ])
  .await;
  let svg = svg_of(&outputs);
  assert!(
    svg.contains("<svg") && svg.matches("<path").count() > 10,
    "{svg}"
  );
  assert!(events.is_empty());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
  let mut found = Vec::new();
  if let Ok(entries) = std::fs::read_dir(dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path.is_dir() {
        found.extend(walk(&path));
      }
      found.push(path);
    }
  }
  found
}
