//! Y6.2: `examples/generators/{flashcards,quiz}.yml` under `vendor: mock`
//! (its own test binary: the mock's reply queue is process-wide, and the
//! two runs consume it in order).

use std::collections::HashMap;

use serde_json::{Value, json};
use yanshi_config::content::{Deliverable, DeliverableKind, FileRole};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

async fn run(template: &str, inputs: Vec<(&str, Value)>) -> (Deliverable, String) {
  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../examples/generators")
      .join(template),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = inputs
    .into_iter()
    .map(|(k, v)| (k.to_string(), FlowValue::Json(v)))
    .collect();
  apply_declared_inputs(&definition, &mut inputs).expect("defaults");
  let runs = tempfile::TempDir::new().expect("tempdir");
  let state = build_flow_from_definition(&definition, Some("mock-model"))
    .expect("builds")
    .execute_from_inputs_with_config(
      inputs,
      FlowExecutionConfig::serial().with_run_base_dir(runs.path()),
    )
    .await
    .expect("runs");
  let html = match &state["page"].as_ref().expect("rendered")["page_output"] {
    FlowValue::File { path, .. } => std::fs::read_to_string(path).expect("artifact"),
    other => panic!("page: {other:?}"),
  };
  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  (
    Deliverable::from_value(value.clone()).expect("matches the contract"),
    html,
  )
}

#[tokio::test]
async fn flashcards_and_quiz_render_html_and_cite_their_chunks() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-study-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let cards = json!({"title": "Tides", "items": [
    {"front": "What causes tides?", "back": "The moon's gravity.", "chunk_id": "c1"},
    {"front": "High tides a day?", "back": "Two."}
  ]});
  let quiz = json!({"title": "Tides quiz", "items": [
    {"question": "What pulls the sea?", "options": ["The wind", "The moon"], "answer": 1,
     "explanation": "Gravity.", "chunk_id": "c1"}
  ]});
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([cards.to_string(), quiz.to_string()]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");
  let chunks = json!([{"chunk_id": "c1", "source_id": "s1", "content": "The moon's gravity pulls the oceans."}]);

  let (deck, html) = run("flashcards.yml", vec![("chunks", chunks.clone())]).await;
  assert_eq!(deck.kind, DeliverableKind::Flashcards);
  assert_eq!(deck.title, "Tides");
  assert_eq!(deck.content["items"], cards["items"]);
  assert_eq!(deck.citations.len(), 1, "only the card with a chunk_id");
  assert_eq!(deck.citations[0].source_id, "s1");
  assert_eq!(deck.files[0].role, FileRole::Primary);
  assert!(html.contains("<div class=\"front\">What causes tides?</div>"));

  let (test, html) = run("quiz.yml", vec![("chunks", chunks)]).await;
  assert_eq!(test.kind, DeliverableKind::Quiz);
  assert_eq!(test.content["items"][0]["answer"], 1);
  assert_eq!(test.citations[0].chunk_id, "c1");
  assert!(html.contains("data-right=\"1\"> The moon"), "{html}");
}
