//! Y4.2: `examples/generators/mindmap.yml` under `vendor: mock` (its own
//! test binary: the mock's reply queue is process-wide).

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind, FileRole};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

#[tokio::test]
async fn mindmap_template_renders_svg_png_html_and_a_valid_deliverable() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-mindmap-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let tree = json!({"label": "Tides", "children": [
    {"label": "Causes", "children": [{"label": "Moon"}, {"label": "Sun"}]},
    {"label": "Rhythm"}
  ]});
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([tree.to_string()]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/generators/mindmap.yml"),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = HashMap::from([(
    "text".to_string(),
    FlowValue::Json(json!("Tides follow the moon.")),
  )]);
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

  let file = |id: &str| match &state[id].as_ref().expect("rendered")[&format!("{id}_output")] {
    FlowValue::File { path, .. } => std::fs::read(path).expect("artifact"),
    other => panic!("{id}: {other:?}"),
  };
  let svg = String::from_utf8(file("map_svg")).expect("utf-8");
  assert!(svg.contains(">Moon</text>") && svg.contains(">Rhythm</text>"));
  assert_eq!(&file("map_png")[1..4], b"PNG");
  assert!(
    String::from_utf8(file("map_html"))
      .expect("utf-8")
      .contains("## Causes")
  );

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::Mindmap);
  assert_eq!(deliverable.title, "Tides");
  assert_eq!(deliverable.content["tree"], tree);
  assert_eq!(deliverable.files.len(), 3);
  assert_eq!(deliverable.files[0].role, FileRole::Primary);
}
