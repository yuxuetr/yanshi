//! Y4.3: `examples/generators/audio_overview.yml` under mock chat + TTS
//! models (its own test binary: the mock's reply queue is process-wide).
//! Needs ffmpeg for `render_audio`; CI runs it in the `media-tools` job:
//! `cargo test -p yanshi-config --test generator_audio -- --ignored`.

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

#[tokio::test]
#[ignore = "needs ffmpeg"]
async fn audio_overview_voices_every_line_and_joins_them_into_one_mp3() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-audio-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\n  mock-tts:\n    vendor: mock\n    type: tts\n    model_id: mock-tts\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let script = json!({"title": "Tides", "lines": [
    {"speaker": "host", "text": "Welcome. Today we talk about tides."},
    {"speaker": "guest", "text": "Tides are caused by the pull of the moon."},
    {"speaker": "host", "text": "Thanks for listening."}
  ]});
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([script.to_string()]).to_string(),
    );
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");

  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../examples/generators/audio_overview.yml"),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = [
    ("text", json!("Tides are caused by the pull of the moon.")),
    ("tts_model", json!("mock-tts")),
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

  let FlowValue::File { path, mime_type } =
    &state["episode"].as_ref().expect("mixed")["episode_output"]
  else {
    panic!("episode must be a File");
  };
  assert_eq!(mime_type.as_deref(), Some("audio/mpeg"));
  let bytes = std::fs::read(path).expect("mp3");
  assert!(
    bytes.starts_with(b"ID3") || bytes[0] == 0xFF,
    "an mp3 stream"
  );

  let voiced = match &state["voice_lines"].as_ref().expect("map")["results_summary"] {
    FlowValue::Json(summary) => summary.clone(),
    other => panic!("{other:?}"),
  };
  assert_eq!(voiced["ok"], 3, "every line voiced: {voiced}");

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::AudioOverview);
  assert_eq!(deliverable.content["script"], script["lines"]);
  assert_eq!(deliverable.files[0].mime_type, "audio/mpeg");
}
