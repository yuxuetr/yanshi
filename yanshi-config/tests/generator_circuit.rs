//! Y6.5: `examples/generators/circuit_diagram.yml` under `vendor: mock`
//! (its own test binary: the mock's reply queue is process-wide). The
//! model's first schematic leaves out a part; the parts-list check sends
//! it back, and the second compiles. Needs TeX Live + poppler:
//! `cargo test -p yanshi-config --test generator_circuit -- --ignored`.

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

const MISSING_C1: &str = "```latex\n\\documentclass[border=4pt]{standalone}\n\\usepackage{circuitikz}\n\\begin{document}\n\\begin{circuitikz}\n\\draw (0,0) to[V, l=$V_1$] (0,3) to[R, l=$R_1$] (3,3) -- (3,0) -- (0,0);\n\\end{circuitikz}\n\\end{document}\n```";
const COMPLETE: &str = "```latex\n\\documentclass[border=4pt]{standalone}\n\\usepackage{circuitikz}\n\\begin{document}\n\\begin{circuitikz}\n\\draw (0,0) to[V, l=$V_1$, a=$5\\,V$] (0,3) to[R, l=$R_1$] (3,3) to[C, l=$C_1$] (3,0) -- (0,0);\n\\draw (0,0) node[ground]{};\n\\end{circuitikz}\n\\end{document}\n```";

#[tokio::test]
#[ignore = "needs TeX Live (lualatex, circuitikz) and poppler (pdftocairo)"]
async fn a_schematic_missing_a_part_is_sent_back_and_redrawn() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-circuit-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let parts = json!({"title": "RC low-pass filter", "brief": "V1 drives R1 in series with C1 to ground", "components": [
    {"id": "V1", "kind": "voltage_source", "value": "5 V"},
    {"id": "R1", "kind": "resistor", "value": "1 kΩ"},
    {"id": "C1", "kind": "capacitor", "value": "10 µF"},
    {"kind": "ground"}
  ]});
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([parts.to_string(), MISSING_C1, COMPLETE]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../examples/generators/circuit_diagram.yml"),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = HashMap::from([(
    "description".to_string(),
    FlowValue::Json(json!("an RC low-pass filter driven by a 5 V source")),
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

  let schematic = state["schematic"]
    .as_ref()
    .unwrap_or_else(|e| panic!("schematic: {e}"));
  assert_eq!(schematic["schematic_attempts"], FlowValue::Json(json!(2)));
  let FlowValue::File { path, .. } = &schematic["schematic_svg"] else {
    panic!("svg");
  };
  let svg = std::fs::read_to_string(path).expect("svg");
  assert!(svg.matches("<path").count() > 3, "{svg}");

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::Figure);
  assert_eq!(deliverable.content["attempts"], 2);
  assert!(
    deliverable.content["source"]
      .as_str()
      .is_some_and(|s| s.contains("l=$C_1$"))
  );
  assert_eq!(deliverable.files.len(), 2);
}
