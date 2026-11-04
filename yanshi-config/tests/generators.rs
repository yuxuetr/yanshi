//! Y4 generator templates (`examples/generators/*.yml`) run end to end
//! under `vendor: mock`, and their final `deliverable` parses as the
//! `content::Deliverable` contract.

use std::collections::HashMap;

use serde_json::{Value, json};
use yanshi_config::content::{Deliverable, DeliverableKind};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

const MOCK_REGISTRY: &str = "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n";

/// Mock registry + the model's queued replies, once per process: the
/// mock provider reads `YANSHI_MOCK_RESPONSES` when the registry builds
/// it, so the queue must be in place before the first model call.
async fn install(replies: Vec<String>) {
  static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-generators-{}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("dir");
      let path = dir.join("models.yml");
      std::fs::write(&path, MOCK_REGISTRY).expect("registry");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe {
        std::env::set_var("YANSHI_MODELS_CONFIG", &path);
        std::env::set_var("YANSHI_MOCK_RESPONSES", json!(replies).to_string());
      }
      yanshi_llm::Yanshi::init_with_config(path.to_str().expect("utf-8"))
        .await
        .expect("mock registry");
    })
    .await;
}

fn template(name: &str) -> String {
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../examples/generators")
    .join(name);
  std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[tokio::test]
async fn report_template_produces_markdown_html_and_a_valid_deliverable() {
  install(vec![
    json!({"title": "Tides", "sections": [
      {"heading": "Cause", "brief": "why tides happen"},
      {"heading": "Rhythm", "brief": "how often"}
    ]})
    .to_string(),
    json!({"markdown": "The moon pulls the oceans.", "quotes": ["Tides are caused by the moon."]})
      .to_string(),
    json!({"markdown": "Two highs a day.", "quotes": ["High tide comes twice daily."]}).to_string(),
  ])
  .await;

  let definition = parse_workflow_definition(&template("report.yml")).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = [
    (
      "text",
      json!("Tides are caused by the moon. High tide comes twice daily."),
    ),
    ("sections", json!(2)),
    ("pdf", json!(false)),
  ]
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

  assert!(
    matches!(
      state["fetch"],
      Err(yanshi_core::error::YanshiError::NodeSkipped)
    ),
    "no url: the fetch is skipped"
  );
  let file = |id: &str| match &state[id].as_ref().expect("rendered")[&format!("{id}_output")] {
    FlowValue::File { path, .. } => std::fs::read_to_string(path).expect("artifact"),
    other => panic!("{id}: {other:?}"),
  };
  let markdown = file("report_md");
  assert!(markdown.starts_with("# Tides"), "{markdown}");
  assert!(markdown.contains("## Cause") && markdown.contains("## Rhythm"));
  assert!(
    markdown.contains("[^1]") && markdown.contains("[^2]:"),
    "{markdown}"
  );
  assert!(file("report_html").contains("<h2>Cause</h2>"));

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::Report);
  assert_eq!(deliverable.title, "Tides");
  assert_eq!(deliverable.citations.len(), 2);
  assert!(deliverable.citations.iter().all(|c| c.source_id == "text"));
  let names: Vec<&str> = deliverable
    .files
    .iter()
    .map(|f| f.node_id.as_str())
    .collect();
  assert_eq!(
    names,
    vec!["report_md", "report_html"],
    "no pdf when pdf=false"
  );
  assert_eq!(
    deliverable.content["sections"].as_array().map(Vec::len),
    Some(2)
  );
  let _: &Value = &deliverable.content;
}
