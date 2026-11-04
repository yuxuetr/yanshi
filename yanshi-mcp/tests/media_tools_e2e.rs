//! Y3.2: the `media-tools` ecosystem MCP server
//! (`examples/ecosystem/mcp/media-tools/server.py`) driven through this
//! crate's own stdio client, against media ffmpeg generates in a temp
//! root.
//!
//! Needs `python3` and `ffmpeg` on PATH, so it is `#[ignore]`d; run with
//! `cargo test -p yanshi-mcp --test media_tools_e2e -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use yanshi_mcp::client::{CallToolResult, ClientBuilder, Content, MCPClient};
use yanshi_mcp::transport::StdioTransport;

fn server_script() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../examples/ecosystem/mcp/media-tools/server.py")
    .canonicalize()
    .expect("server.py exists")
}

/// Make fixture media with ffmpeg's synthetic sources.
fn ffmpeg(root: &Path, args: &[&str]) {
  let status = Command::new("ffmpeg")
    .args(["-hide_banner", "-loglevel", "error", "-y"])
    .args(args)
    .current_dir(root)
    .status()
    .expect("ffmpeg runs");
  assert!(status.success(), "fixture ffmpeg {args:?} failed");
}

async fn client(root: &Path) -> MCPClient {
  let transport = StdioTransport::new(vec![
    "python3".to_string(),
    server_script().display().to_string(),
    "--root".to_string(),
    root.display().to_string(),
  ]);
  let mut client = ClientBuilder::new()
    .with_transport(transport)
    .build()
    .await
    .expect("client builds");
  client.connect().await.expect("initialize handshake");
  client
}

fn payload(result: &CallToolResult) -> Value {
  let Some(Content::Text { text }) = result.content.first() else {
    panic!("expected a text result, got {result:?}");
  };
  assert!(!result.is_error(), "tool failed: {text}");
  serde_json::from_str(text).expect("JSON payload")
}

fn error_text(result: &CallToolResult) -> String {
  assert!(result.is_error(), "expected an error result: {result:?}");
  match result.content.first() {
    Some(Content::Text { text }) => text.clone(),
    other => panic!("expected text, got {other:?}"),
  }
}

#[tokio::test]
#[ignore = "needs python3 and ffmpeg"]
async fn media_tools_finish_audio_and_video_inside_the_root() {
  let root = tempfile::TempDir::new().expect("tempdir");
  let dir = root.path();
  ffmpeg(
    dir,
    &[
      "-f",
      "lavfi",
      "-i",
      "sine=frequency=440:duration=2",
      "-ar",
      "48000",
      "a.wav",
    ],
  );
  ffmpeg(
    dir,
    &[
      "-f",
      "lavfi",
      "-i",
      "sine=frequency=660:duration=1",
      "-ar",
      "48000",
      "b.wav",
    ],
  );
  ffmpeg(
    dir,
    &[
      "-f",
      "lavfi",
      "-i",
      "color=c=red:s=320x240",
      "-frames:v",
      "1",
      "red.png",
    ],
  );
  ffmpeg(
    dir,
    &[
      "-f",
      "lavfi",
      "-i",
      "color=c=blue:s=200x300",
      "-frames:v",
      "1",
      "blue.png",
    ],
  );
  std::fs::write(
    dir.join("subs.srt"),
    "1\n00:00:00,000 --> 00:00:02,000\nHello tides\n",
  )
  .expect("srt");

  let mut client = client(dir).await;
  let names: Vec<String> = client
    .list_tools()
    .await
    .expect("tools/list")
    .into_iter()
    .map(|t| t.name)
    .collect();
  for expected in [
    "audio_concat",
    "loudness_normalize",
    "mux_video_audio",
    "burn_subtitles",
    "images_to_slideshow",
  ] {
    assert!(
      names.contains(&expected.to_string()),
      "{expected} in {names:?}"
    );
  }

  let joined = payload(
    &client
      .call_tool(
        "audio_concat",
        json!({"inputs": ["a.wav", "b.wav"], "output": "joined.wav"}),
      )
      .await
      .expect("call"),
  );
  assert_eq!(joined["segments"], 2);
  assert!(dir.join("joined.wav").exists());

  let loud = payload(
    &client
      .call_tool(
        "loudness_normalize",
        json!({"input": "joined.wav", "output": "out/mastered.wav", "target_lufs": -16}),
      )
      .await
      .expect("call"),
  );
  let output_lufs = loud["output_lufs"].as_f64().expect("output_lufs");
  assert!(
    (output_lufs + 16.0).abs() < 1.5,
    "normalised near -16 LUFS: {loud}"
  );
  assert!(loud["input_lufs"].as_f64().is_some());

  let slideshow = payload(
    &client
      .call_tool(
        "images_to_slideshow",
        json!({
          "images": ["red.png", "blue.png"],
          "output": "slides.mp4",
          "seconds_per_image": 1,
          "width": 320,
          "height": 240,
        }),
      )
      .await
      .expect("call"),
  );
  assert_eq!(slideshow["duration_seconds"], 2.0);

  payload(
    &client
      .call_tool(
        "mux_video_audio",
        json!({"video": "slides.mp4", "audio": "out/mastered.wav", "output": "episode.mp4"}),
      )
      .await
      .expect("call"),
  );
  assert!(
    std::fs::metadata(dir.join("episode.mp4"))
      .expect("mp4")
      .len()
      > 0
  );

  let subtitled = client
    .call_tool(
      "burn_subtitles",
      json!({"video": "slides.mp4", "subtitles": "subs.srt", "output": "subbed.mp4"}),
    )
    .await
    .expect("call");
  if subtitled.is_error() {
    // Only acceptable reason: this ffmpeg build lacks libass.
    let text = error_text(&subtitled);
    assert!(
      text.contains("subtitles") || text.contains("No such filter"),
      "{text}"
    );
  } else {
    assert!(dir.join("subbed.mp4").exists());
  }

  // Confinement: neither reads nor writes may leave the root.
  let escape = client
    .call_tool(
      "audio_concat",
      json!({"inputs": ["a.wav", "../../etc/hosts"], "output": "x.wav"}),
    )
    .await
    .expect("call");
  assert!(error_text(&escape).contains("outside the allowed root"));
  let escape = client
    .call_tool(
      "loudness_normalize",
      json!({"input": "a.wav", "output": "../escaped.wav"}),
    )
    .await
    .expect("call");
  assert!(error_text(&escape).contains("outside the allowed root"));
  assert!(!dir.parent().expect("parent").join("escaped.wav").exists());
}
