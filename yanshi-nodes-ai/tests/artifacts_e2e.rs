//! Y1.1 acceptance: modality nodes emit `FlowValue::File` and keep the
//! state pool small, driven end to end through the registry's `mock`
//! vendor (`yanshi_llm::providers::MockMediaProvider`).
//!
//! Lives in `tests/` (its own process) because the nodes call
//! `Yanshi::init()` on the process-global registry, and the lib's unit
//! tests point that registry elsewhere.

use std::collections::HashMap;
use std::path::PathBuf;

use yanshi_core::{async_node::AsyncNode, run_context, value::FlowValue};
use yanshi_nodes_ai::{ArtifactOutputFormat, TTSNode, TextToImageNode, TextToVideoNode};

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
  mock-t2v:
    vendor: mock
    type: text_to_video
    model_id: mock-t2v
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#;

/// Point the process-global registry at a mock-only config, once. The
/// `tts` node calls `Yanshi::init()` itself, whose first lookup is the
/// `YANSHI_MODELS_CONFIG` env var, so the file must outlive every test in
/// this process (hence a pid-named dir, not a per-test `TempDir`); the
/// image / video nodes do not re-init, so the registry is loaded here too.
async fn install_mock_registry() {
  static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-nodes-ai-e2e-{}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("create registry dir");
      let path = dir.join("models.yml");
      std::fs::write(&path, MOCK_REGISTRY).expect("write registry");
      // SAFETY: set exactly once, before any test reads it, under the
      // OnceCell's initialisation lock.
      unsafe { std::env::set_var("YANSHI_MODELS_CONFIG", &path) };
      yanshi_llm::Yanshi::init_with_config(path.to_str().expect("utf-8 path"))
        .await
        .expect("mock registry loads");
    })
    .await;
}

fn estimated_pool_bytes(outputs: &HashMap<String, FlowValue>) -> usize {
  outputs.values().map(FlowValue::estimated_size_bytes).sum()
}

async fn run_tts(
  root: PathBuf,
  chars: usize,
  format: ArtifactOutputFormat,
) -> HashMap<String, FlowValue> {
  let node =
    TTSNode::new("speak", "mock-tts", "narrator", &"a".repeat(chars)).with_output_format(format);
  run_context::with_artifact_root(root, async move {
    node
      .execute(&HashMap::new())
      .await
      .expect("tts runs under mock")
  })
  .await
}

#[tokio::test]
async fn tts_file_mode_keeps_the_state_pool_constant_while_audio_grows() {
  install_mock_registry().await;
  let run = tempfile::TempDir::new().expect("tempdir");

  // 700 chars × 100 ms × 16 KB/s ≈ 1.1 MiB of audio from the mock.
  let small = run_tts(run.path().to_path_buf(), 10, ArtifactOutputFormat::File).await;
  let large = run_tts(run.path().to_path_buf(), 700, ArtifactOutputFormat::File).await;

  let FlowValue::File { path, mime_type } = &large["speak_output"] else {
    panic!(
      "file mode must emit FlowValue::File, got {:?}",
      large["speak_output"]
    );
  };
  assert_eq!(mime_type.as_deref(), Some("audio/wav"));
  assert_eq!(
    path,
    &run.path().join("artifacts").join("speak").join("2.wav"),
    "second write in the same node dir takes slot 2"
  );
  let on_disk = std::fs::metadata(path).expect("artifact exists").len();
  assert!(
    on_disk > 1024 * 1024,
    "mock audio should exceed 1 MiB, got {on_disk}"
  );

  let small_bytes = estimated_pool_bytes(&small);
  let large_bytes = estimated_pool_bytes(&large);
  assert!(
    large_bytes < 1024,
    "state pool must hold a path, not audio: {large_bytes} bytes"
  );
  assert_eq!(
    large_bytes, small_bytes,
    "pool size must not depend on audio length (1.1 MiB vs 16 KiB of audio)"
  );
}

/// Reverse check for the test above: under the compatibility switch the
/// pool does grow with the audio, so the constant-size assertion is
/// actually measuring the `File` path and not a vacuous property.
#[tokio::test]
async fn tts_data_uri_mode_grows_the_state_pool_with_the_audio() {
  install_mock_registry().await;
  let run = tempfile::TempDir::new().expect("tempdir");

  let small = run_tts(run.path().to_path_buf(), 10, ArtifactOutputFormat::DataUri).await;
  let large = run_tts(run.path().to_path_buf(), 700, ArtifactOutputFormat::DataUri).await;

  assert!(matches!(large["speak_output"], FlowValue::Json(_)));
  let large_bytes = estimated_pool_bytes(&large);
  assert!(
    large_bytes > 1024 * 1024,
    "data_uri mode must inline the audio: {large_bytes} bytes"
  );
  assert!(large_bytes > estimated_pool_bytes(&small) * 10);
  assert!(
    !run.path().join("artifacts").exists(),
    "data_uri mode must not write artifacts"
  );
}

#[tokio::test]
async fn text_to_image_writes_a_png_artifact() {
  install_mock_registry().await;
  let run = tempfile::TempDir::new().expect("tempdir");
  let node = TextToImageNode::new("draw", "mock-t2i").with_prompt("a circuit");
  let outputs = run_context::with_artifact_root(run.path().to_path_buf(), async move {
    node
      .execute(&HashMap::new())
      .await
      .expect("t2i runs under mock")
  })
  .await;
  let FlowValue::File { path, mime_type } = &outputs["draw_image"] else {
    panic!("expected File, got {:?}", outputs["draw_image"]);
  };
  assert_eq!(mime_type.as_deref(), Some("image/png"));
  assert_eq!(
    path,
    &run.path().join("artifacts").join("draw").join("1.png")
  );
  let bytes = std::fs::read(path).expect("png exists");
  assert_eq!(
    &bytes[1..4],
    b"PNG",
    "decoded base64 must be the PNG the mock returns"
  );
}

#[tokio::test]
async fn text_to_video_persists_the_fetched_bytes() {
  install_mock_registry().await;
  let run = tempfile::TempDir::new().expect("tempdir");
  let node = TextToVideoNode::new("clip", "mock-t2v")
    .with_prompt("a short clip")
    .with_timeout(5_000);
  let outputs = run_context::with_artifact_root(run.path().to_path_buf(), async move {
    node
      .execute(&HashMap::new())
      .await
      .expect("t2v runs under mock")
  })
  .await;
  let FlowValue::File { path, mime_type } = &outputs["clip_video"] else {
    panic!("expected File, got {:?}", outputs["clip_video"]);
  };
  assert_eq!(mime_type.as_deref(), Some("video/mp4"));
  assert_eq!(
    path,
    &run.path().join("artifacts").join("clip").join("1.mp4")
  );
  let bytes = std::fs::read(path).expect("mp4 exists");
  assert_eq!(&bytes[4..8], b"ftyp", "the mock's clip is a real MP4");
}
