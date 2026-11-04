//! Y6.4: `examples/generators/video_overview.yml` under mock chat,
//! text-to-image and TTS models (its own test binary: the mock's reply
//! queue is process-wide). Needs ffmpeg for `render_video`; CI runs it in
//! the `media-tools` job:
//! `cargo test -p yanshi-config --test generator_video -- --ignored`.

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind, FileRole};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

#[tokio::test]
#[ignore = "needs ffmpeg"]
async fn video_overview_pictures_and_voices_every_scene_into_one_mp4() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-video-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\n  mock-tts:\n    vendor: mock\n    type: tts\n    model_id: mock-tts\n  mock-image:\n    vendor: mock\n    type: text_to_image\n    model_id: mock-image\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let script = json!({"title": "Tides", "scenes": [
    {"narration": "Tides rise and fall twice a day. Why?", "visual": "A beach at high tide", "chunk_id": "c1"},
    {"narration": "The moon pulls the oceans toward it.", "visual": "The moon over the sea"}
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
      .join("../examples/generators/video_overview.yml"),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> = [
    ("tts_model", json!("mock-tts")),
    ("image_model", json!("mock-image")),
    (
      "chunks",
      json!([{"chunk_id": "c1", "source_id": "s1", "content": "Tides rise and fall twice a day."}]),
    ),
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

  let film = state["film"]
    .as_ref()
    .unwrap_or_else(|e| panic!("film: {e}"));
  let FlowValue::File { path, .. } = &film["film_output"] else {
    panic!("mp4");
  };
  let probe = std::process::Command::new("ffprobe")
    .args([
      "-v",
      "error",
      "-show_entries",
      "stream=codec_type",
      "-of",
      "csv=p=0",
    ])
    .arg(path)
    .output()
    .expect("ffprobe");
  let streams = String::from_utf8_lossy(&probe.stdout);
  assert!(
    streams.contains("video") && streams.contains("audio") && streams.contains("subtitle"),
    "{streams}"
  );
  let FlowValue::File { path: srt, .. } = &film["film_subtitles"] else {
    panic!("srt");
  };
  let srt = std::fs::read_to_string(srt).expect("srt");
  assert!(
    srt.contains("\nWhy?\n") && srt.contains("The moon pulls"),
    "{srt}"
  );

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::VideoOverview);
  assert_eq!(deliverable.title, "Tides");
  assert_eq!(deliverable.citations.len(), 1);
  assert_eq!(deliverable.files[0].role, FileRole::Primary);
  assert_eq!(deliverable.files[1].mime_type, "application/x-subrip");
  assert!(deliverable.content["seconds"].as_f64().expect("seconds") > 0.5);
  assert_eq!(yanshi_llm::providers::mock_media::text2image_calls(), 2);
}
