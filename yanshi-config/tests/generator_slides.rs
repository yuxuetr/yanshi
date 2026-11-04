//! Y6.1: `examples/generators/slides.yml` under a mock model. The deck
//! has a title slide and two content slides, one with an SVG figure
//! (source given, so it renders without a model call). The PPTX must
//! hold the three slides, the figure as an embedded SVG, and the speaker
//! notes. Needs pandoc; skips (with a note) where it is not installed.

use std::collections::HashMap;
use std::io::Read as _;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind, FileRole};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

const FIGURE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect width="200" height="100" fill="#0072B2"/><text x="20" y="55" fill="white">Moon pulls</text></svg>"##;

#[tokio::test]
async fn slides_template_builds_a_pptx_with_an_svg_figure_and_notes() {
  if yanshi_nodes::common::sandboxed::find_tool("pandoc").is_none() {
    eprintln!("skipping: pandoc is not installed");
    return;
  }
  let dir = std::env::temp_dir().join(format!("yanshi-gen-slides-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let deck = json!({"title": "Tides", "subtitle": "Why the sea moves", "slides": [
    {"title": "The cause", "bullets": ["The moon's gravity pulls the ocean", "The sun adds a smaller pull"],
     "notes": "Start with the moon.", "figure": {"language": "svg", "description": "moon pulling", "source": FIGURE}},
    {"title": "The rhythm", "bullets": ["Two high tides a day"], "notes": "End with the timing."}
  ]});
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([deck.to_string()]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/generators/slides.yml"),
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

  let FlowValue::File { path, .. } =
    &state["deck_pptx"].as_ref().expect("pptx")["deck_pptx_output"]
  else {
    panic!("pptx file");
  };
  let mut zip = zip::ZipArchive::new(std::fs::File::open(path).expect("pptx")).expect("a zip");
  let names: Vec<String> = zip
    .file_names()
    .map(|name| name.expect("utf-8 entry name").into_owned())
    .collect();
  let slides = names
    .iter()
    .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
    .count();
  assert_eq!(slides, 3, "title + two content slides: {names:?}");
  assert!(
    names
      .iter()
      .any(|n| n.starts_with("ppt/media/") && n.ends_with(".svg")),
    "the figure is an embedded SVG: {names:?}"
  );
  let mut notes = String::new();
  for name in names
    .iter()
    .filter(|n| n.starts_with("ppt/notesSlides/") && n.ends_with(".xml"))
  {
    zip
      .by_name(name)
      .expect("notes")
      .read_to_string(&mut notes)
      .expect("xml");
  }
  assert!(
    notes.contains("Start with the moon.") && notes.contains("End with the timing."),
    "speaker notes kept"
  );

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::Slides);
  assert_eq!(deliverable.files.len(), 2, "the pptx and the figure");
  assert_eq!(deliverable.files[1].role, FileRole::Asset);
}
