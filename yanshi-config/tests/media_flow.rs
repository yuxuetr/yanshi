//! Y1.2 acceptance: config-first workflows consume `FlowValue::File`
//! produced by modality nodes, end to end under `vendor: mock`.
//!
//! - `tts → file(copy)`: the audio artifact is copied to a caller-chosen
//!   path and the state pool never holds the audio.
//! - `text_to_image → image_understand`: a `File` image output feeds a
//!   chat model's image input directly through `input_mapping`.

use std::collections::HashMap;

use yanshi_config::executor::build_flow_from_yaml;
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

const MOCK_REGISTRY: &str = r#"
models:
  mock-tts:
    vendor: mock
    type: tts
    model_id: mock-tts
  mock-t2i:
    vendor: mock
    type: text_to_image
    model_id: mock-t2i
  mock-vision:
    vendor: mock
    type: chat
    model_id: mock-vision
    accepts: [text, image]
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#;

/// Process-wide mock registry: the `tts` / `image_understand` nodes call
/// `Yanshi::init()` themselves and read `YANSHI_MODELS_CONFIG` first.
async fn install_mock_registry() {
  static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-config-media-{}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("registry dir");
      let path = dir.join("models.yml");
      std::fs::write(&path, MOCK_REGISTRY).expect("write registry");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe { std::env::set_var("YANSHI_MODELS_CONFIG", &path) };
      yanshi_llm::Yanshi::init_with_config(path.to_str().expect("utf-8"))
        .await
        .expect("mock registry loads");
    })
    .await;
}

#[tokio::test]
async fn tts_to_file_copy_delivers_the_audio_without_base64_in_the_pool() {
  install_mock_registry().await;
  let work = tempfile::TempDir::new().expect("tempdir");
  let runs = work.path().join("runs");
  let dest = work.path().join("out").join("narration.wav");

  let yaml = format!(
    r#"
name: "tts to file"
nodes:
  - id: speak
    type: tts
    parameters:
      model: mock-tts
      voice: narrator
      input_template: "{}"
  - id: save
    type: file
    dependencies: ["speak"]
    input_mapping:
      content: "{{{{ nodes.speak.outputs.speak_output }}}}"
    parameters:
      operation: copy
      path: "{}"
      allowed_paths: ["{}"]
"#,
    "a".repeat(200),
    dest.display(),
    work.path().display()
  );

  let flow = build_flow_from_yaml(&yaml, None).expect("flow builds");
  let state = flow
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(&runs),
    )
    .await
    .expect("flow runs under mock");

  let speak = state["speak"].as_ref().expect("tts ok");
  let FlowValue::File {
    path: artifact,
    mime_type,
  } = &speak["speak_output"]
  else {
    panic!("tts must emit a File, got {:?}", speak["speak_output"]);
  };
  assert_eq!(mime_type.as_deref(), Some("audio/wav"));
  assert!(
    artifact.starts_with(&runs),
    "artifact {artifact:?} must live under the run dir"
  );

  let save = state["save"].as_ref().expect("copy ok");
  assert!(matches!(save["path"], FlowValue::File { .. }));
  let bytes = std::fs::read(&dest).expect("copied file exists");
  assert_eq!(&bytes[0..4], b"RIFF");
  assert_eq!(bytes, std::fs::read(artifact).expect("artifact exists"));

  let pool_bytes: usize = state
    .values()
    .filter_map(|r| r.as_ref().ok())
    .flat_map(|outputs| outputs.values())
    .map(FlowValue::estimated_size_bytes)
    .sum();
  assert!(
    pool_bytes < 2048,
    "state pool must hold paths only, got {pool_bytes} bytes"
  );
}

#[tokio::test]
async fn image_understand_reads_a_file_image_from_text_to_image() {
  install_mock_registry().await;
  let work = tempfile::TempDir::new().expect("tempdir");

  let yaml = r#"
name: "draw then describe"
nodes:
  - id: draw
    type: text_to_image
    parameters:
      model: mock-t2i
      prompt: "an RC circuit"
  - id: describe
    type: image_understand
    dependencies: ["draw"]
    input_mapping:
      picture: "{{ nodes.draw.outputs.draw_image }}"
    parameters:
      model: mock-vision
      text_prompt: "What is in this picture?"
      image_source: picture
"#;

  let flow = build_flow_from_yaml(yaml, None).expect("flow builds");
  let state = flow
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(work.path()),
    )
    .await
    .expect("flow runs under mock");

  let draw = state["draw"].as_ref().expect("t2i ok");
  assert!(matches!(draw["draw_image"], FlowValue::File { .. }));
  let describe = state["describe"].as_ref().expect("image_understand ok");
  let FlowValue::Json(serde_json::Value::String(answer)) = &describe["describe_output"] else {
    panic!(
      "expected a text answer, got {:?}",
      describe["describe_output"]
    );
  };
  assert!(!answer.is_empty(), "mock chat model must answer");
}

/// The YAML `prompt` reaches the vendor (the factory used to drop it).
/// The mock fails a prompt carrying `mock:http=400`, so a failing node
/// proves the prompt arrived.
#[tokio::test]
async fn text_to_image_yaml_prompt_reaches_the_vendor() {
  install_mock_registry().await;
  let work = tempfile::TempDir::new().expect("tempdir");
  let yaml = r#"
name: "prompt reaches vendor"
nodes:
  - id: draw
    type: text_to_image
    parameters:
      model: mock-t2i
      prompt: "an RC circuit mock:http=400"
"#;
  let state = build_flow_from_yaml(yaml, None)
    .expect("flow builds")
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(work.path()),
    )
    .await
    .expect("run returns its state");
  let error = state["draw"]
    .as_ref()
    .expect_err("the prompt's marker fails the call");
  assert!(error.to_string().contains("400"), "{error}");
}

/// Y3.1: the three render nodes from YAML. `render_table` takes rows from
/// `parameters`; `render_markdown` takes its Markdown from an upstream
/// template through `input_mapping`; `render_svg` writes a PNG.
#[tokio::test]
async fn render_nodes_produce_files_from_yaml() {
  let work = tempfile::TempDir::new().expect("tempdir");
  let yaml = r##"
name: "render"
nodes:
  - id: table
    type: render_table
    parameters:
      format: csv
      rows: [{ name: "Ada", year: 1843 }]
  - id: note
    type: template
    parameters:
      template: "# Notes\n\nBorn {{ year }}."
      year: 1815
  - id: page
    type: render_markdown
    dependencies: ["note"]
    input_mapping:
      markdown: "{{ nodes.note.outputs.output }}"
  - id: chart
    type: render_svg
    parameters:
      svg: '<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="#00ff00"/></svg>'
"##;
  let state = build_flow_from_yaml(yaml, None)
    .expect("flow builds")
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(work.path()),
    )
    .await
    .expect("flow runs");
  let file = |id: &str| match &state[id].as_ref().expect("node ok")[&format!("{id}_output")] {
    FlowValue::File { path, .. } => path.clone(),
    other => panic!("{id}: expected File, got {other:?}"),
  };
  assert_eq!(
    std::fs::read_to_string(file("table")).expect("csv"),
    "name,year\nAda,1843\n"
  );
  let html = std::fs::read_to_string(file("page")).expect("html");
  assert!(
    html.contains("<h1>Notes</h1>") && html.contains("Born 1815."),
    "{html}"
  );
  assert_eq!(&std::fs::read(file("chart")).expect("png")[1..4], b"PNG");
}
