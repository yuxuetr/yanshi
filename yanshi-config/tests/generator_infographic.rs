//! Y6.3: `examples/generators/infographic.yml` under `vendor: mock` (its
//! own test binary: the mock's reply queue is process-wide).

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind, FileRole};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

#[tokio::test]
async fn infographic_template_lays_out_wrapped_text_as_svg_and_png() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-infographic-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let draft = json!({
    "title": "Tides & the moon",
    "subtitle": "Why the sea rises and falls twice a day",
    "stats": [
      {"value": "2", "label": "high tides a day", "chunk_id": "c1"},
      {"value": "12h 25m", "label": "between two high tides"}
    ],
    "sections": [
      {"heading": "Cause", "points": [
        "The moon's gravity pulls the near side of the ocean toward it, and inertia raises a second bulge on the far side of the planet",
        "<Sun> adds a smaller pull"
      ], "chunk_id": "c1"},
      {"heading": "潮汐的节律", "points": ["月球每天绕地球运行的位置变化使得高潮时间每天推迟大约五十分钟左右，这一现象在世界各地的海岸都能观察到"]}
    ],
    "takeaway": "Tides follow the moon."
  });
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([draft.to_string()]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/generators/infographic.yml"),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = HashMap::from([(
    "chunks".to_string(),
    FlowValue::Json(
      json!([{"chunk_id": "c1", "source_id": "s1", "content": "Two high tides a day, pulled by the moon."}]),
    ),
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

  let file = |id: &str| match &state[id].as_ref().unwrap_or_else(|e| panic!("{id}: {e}"))
    [&format!("{id}_output")]
  {
    FlowValue::File { path, .. } => std::fs::read(path).expect("artifact"),
    other => panic!("{id}: {other:?}"),
  };
  let svg = String::from_utf8(file("graphic_svg")).expect("utf-8");
  assert!(
    svg.contains(">Tides &amp; the moon</text>"),
    "escaped: {svg}"
  );
  assert!(svg.contains("&lt;Sun&gt; adds a smaller pull"));
  assert!(svg.contains(">12h 25m</text>"));
  // The long English point wraps by words into several lines, none cut mid-word.
  assert!(
    svg.contains(">The moon&#x27;s gravity pulls the near side of the ocean toward it,"),
    "{svg}"
  );
  // The CJK point wraps by characters.
  let cjk_lines = svg.matches("font-size=\"22\" fill=\"#334155\">").count();
  assert!(cjk_lines >= 5, "{cjk_lines} point lines: {svg}");
  assert_eq!(&file("graphic_png")[1..4], b"PNG");

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::Infographic);
  assert_eq!(deliverable.title, "Tides & the moon");
  assert_eq!(deliverable.content["stats"][1]["value"], "12h 25m");
  assert_eq!(
    deliverable.citations.len(),
    2,
    "the stat and the section citing c1"
  );
  assert_eq!(deliverable.files[0].role, FileRole::Primary);
  assert_eq!(deliverable.files[1].mime_type, "image/png");
}
