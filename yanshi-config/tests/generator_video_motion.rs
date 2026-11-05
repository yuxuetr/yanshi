//! Y8.3: `video_overview.yml` with a text-to-video model. A scene the
//! script marks `motion` becomes a generated clip; the others stay
//! pictures. Its own test binary (the mock's reply queue and call
//! counter are process-wide). Needs ffmpeg:
//! `cargo test -p yanshi-config --test generator_video_motion -- --ignored`.

use std::collections::HashMap;

use serde_json::json;
use yanshi_config::content::{Deliverable, DeliverableKind};
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

#[tokio::test]
#[ignore = "needs ffmpeg"]
async fn video_overview_turns_motion_scenes_into_clips() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-motion-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\n  mock-tts:\n    vendor: mock\n    type: tts\n    model_id: mock-tts\n  mock-image:\n    vendor: mock\n    type: text_to_image\n    model_id: mock-image\n  mock-video:\n    vendor: mock\n    type: text_to_video\n    model_id: mock-video\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  let script = json!({"title": "Tides", "scenes": [
    {"narration": "Tides rise and fall twice a day.", "visual": "A beach at high tide"},
    {"narration": "Watch the water creep up the sand.", "visual": "Water rising on a beach", "motion": true},
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
    (
      "text",
      json!("Tides rise and fall twice a day because the moon pulls the oceans."),
    ),
    ("tts_model", json!("mock-tts")),
    ("image_model", json!("mock-image")),
    ("video_model", json!("mock-video")),
    ("video_seconds", json!(1)),
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

  let produce = state["produce"].as_ref().expect("produce");
  let FlowValue::Json(summary) = &produce["results_summary"] else {
    panic!("summary");
  };
  assert_eq!(
    summary["err_indexes"],
    json!([]),
    "a skipped branch is not an error"
  );

  let FlowValue::Json(cut) = &state["cut"].as_ref().expect("cut")["scenes"] else {
    panic!("scenes");
  };
  let kinds: Vec<&str> = cut
    .as_array()
    .expect("array")
    .iter()
    .map(|scene| {
      if scene.get("video").is_some() {
        "video"
      } else {
        "image"
      }
    })
    .collect();
  assert_eq!(kinds, ["image", "video", "image"]);
  assert_eq!(yanshi_llm::providers::mock_media::text2image_calls(), 2);

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
    streams.contains("video") && streams.contains("audio"),
    "{streams}"
  );

  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  let deliverable = Deliverable::from_value(value.clone()).expect("matches the contract");
  assert_eq!(deliverable.kind, DeliverableKind::VideoOverview);
}
