//! Mock providers for the non-chat modalities (Y1.1).
//!
//! The chat path has had [`super::MockProvider`] (`vendor: mock`) since
//! the beginning; TTS / ASR / image / video had nothing, so no node,
//! generator template or CLI test could exercise a modality success
//! path without a vendor key. This provider fills that gap for every
//! modality trait with deterministic, dependency-free output:
//!
//! - TTS: a valid PCM WAV (8 kHz, mono, 16-bit silence) whose duration
//!   scales with the input length, so size-related assertions (state-pool
//!   growth, artifact bytes) behave like a real vendor's would. The
//!   requested `response_format` is ignored — the bytes are always WAV
//!   and `mime_type` says so.
//! - ASR: the fixed transcript `"mock transcript"` plus the byte count.
//! - Text-to-image / image-to-image / image-edit: a 1×1 PNG as
//!   `b64_json` regardless of the requested `response_format`.
//! - Text-to-video: `submit` returns a task, the first `poll` completes
//!   with [`MOCK_MP4_BASE64`], a real one-second 64×36 H.264 MP4, so a
//!   generator can hand the clip to ffmpeg (Y8.3).
//!
//! Registered under vendor `mock` in `crate::modality_dispatch`, so a
//! registry entry like
//!
//! ```yaml
//! models:
//!   mock-tts:
//!     vendor: mock
//!     type: tts
//!     model_id: mock-tts
//! ```
//!
//! routes `Yanshi::tts("mock-tts")` here without any API key.
//!
//! Y2.5: a text-to-image prompt containing `mock:http=<status>` fails
//! with that HTTP status instead, and [`text2image_calls`] counts every
//! call, so retry behaviour can be asserted without a vendor.

use async_trait::async_trait;

use super::modality::{
  AsrProvider, AsrRequest, AsrResponse, GeneratedImage, GeneratedVideo, Image2ImageProvider,
  Image2ImageRequest, ImageEditProvider, ImageEditRequest, ImageGenerationResponse,
  Text2ImageProvider, Text2ImageRequest, Text2VideoProvider, Text2VideoRequest, TtsProvider,
  TtsRequest, TtsResponse, VideoGenerationResponse, VideoGenerationStatus, VideoGenerationTask,
};
use crate::Result;

static TEXT2IMAGE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Y2.5: text-to-image calls this process has made to the mock.
pub fn text2image_calls() -> usize {
  TEXT2IMAGE_CALLS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The status in a `mock:http=<status>` marker, if the prompt has one.
fn injected_status(prompt: &str) -> Option<u16> {
  let rest = prompt.split("mock:http=").nth(1)?;
  let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
  digits.parse().ok()
}

/// A minimal valid 1×1 opaque PNG, base64-encoded (no `data:` prefix).
pub const MOCK_PNG_BASE64: &str =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

/// A playable one-second, 64×36, 4 fps H.264 MP4 (no audio), base64.
/// Made with `ffmpeg -f lavfi -i color=c=0x3366aa:s=64x36:d=1:r=4
/// -c:v libx264 -pix_fmt yuv420p -crf 40 -movflags +faststart -an`.
pub const MOCK_MP4_BASE64: &str = concat!(
  "AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDEAAANkbW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAA+",
  "gAAQAAAQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAA",
  "AAAAAAAAAAAAAAAAAgAAAo90cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAA+gAAAAAAAAAAAAAAAAAAA",
  "AAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAEAAAAAkAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAA",
  "AAEAAAPoAAAgAAABAAAAAAIHbWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAABAAAAAQABVxAAAAAAALWhkbHIAAAAAAA",
  "AAAHZpZGUAAAAAAAAAAAAAAABWaWRlb0hhbmRsZXIAAAABsm1pbmYAAAAUdm1oZAAAAAEAAAAAAAAAAAAAACRkaW5m",
  "AAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAAAQAAAXJzdGJsAAAAwnN0c2QAAAAAAAAAAQAAALJhdmMxAAAAAAAAAA",
  "EAAAAAAAAAAAAAAAAAAAAAAEAAJABIAAAASAAAAAAAAAABFExhdmM2My4xLjEwMiBsaWJ4MjY0AAAAAAAAAAAAAAAA",
  "GP//AAAAOGF2Y0MBZAAK/+EAGmdkAAqscgREf58BEAAAAwAQAAADAIDxIlhGAQAHaOhDg5LIsP34+AAAAAAQcGFzcA",
  "AAAAEAAAABAAAAFGJ0cnQAAAAAAAAX4AAAAAAAAAAYc3R0cwAAAAAAAAABAAAABAAAEAAAAAAUc3RzcwAAAAAAAAAB",
  "AAAAAQAAAChjdHRzAAAAAAAAAAMAAAABAAAgAAAAAAEAAEAAAAAAAgAAEAAAAAAcc3RzYwAAAAAAAAABAAAAAQAAAA",
  "QAAAABAAAAJHN0c3oAAAAAAAAAAAAAAAQAAALVAAAADQAAAA0AAAANAAAAFHN0Y28AAAAAAAAAAQAAA5QAAABhdWR0",
  "YQAAAFltZXRhAAAAAAAAACFoZGxyAAAAAAAAAABtZGlyYXBwbAAAAAAAAAAAAAAAACxpbHN0AAAAJKl0b28AAAAcZG",
  "F0YQAAAAEAAAAATGF2ZjYzLjEuMTAyAAAACGZyZWUAAAMEbWRhdAAAAq8GBf//q9xF6b3m2Ui3lizYINkj7u94MjY0",
  "IC0gY29yZSAxNjUgcjMyMjIgYjM1NjA1YSAtIEguMjY0L01QRUctNCBBVkMgY29kZWMgLSBDb3B5bGVmdCAyMDAzLT",
  "IwMjUgLSBodHRwOi8vd3d3LnZpZGVvbGFuLm9yZy94MjY0Lmh0bWwgLSBvcHRpb25zOiBjYWJhYz0xIHJlZj0xNiBk",
  "ZWJsb2NrPTE6MDowIGFuYWx5c2U9MHgzOjB4MTMzIG1lPXVtaCBzdWJtZT0xMCBwc3k9MSBwc3lfcmQ9MS4wMDowLj",
  "AwIG1peGVkX3JlZj0xIG1lX3JhbmdlPTI0IGNocm9tYV9tZT0xIHRyZWxsaXM9MiA4eDhkY3Q9MSBjcW09MCBkZWFk",
  "em9uZT0yMSwxMSBmYXN0X3Bza2lwPTEgY2hyb21hX3FwX29mZnNldD0tMiB0aHJlYWRzPTEgbG9va2FoZWFkX3Rocm",
  "VhZHM9MSBzbGljZWRfdGhyZWFkcz0wIG5yPTAgZGVjaW1hdGU9MSBpbnRlcmxhY2VkPTAgYmx1cmF5X2NvbXBhdD0w",
  "IGNvbnN0cmFpbmVkX2ludHJhPTAgYmZyYW1lcz04IGJfcHlyYW1pZD0yIGJfYWRhcHQ9MiBiX2JpYXM9MCBkaXJlY3",
  "Q9MyB3ZWlnaHRiPTEgb3Blbl9nb3A9MCB3ZWlnaHRwPTIga2V5aW50PTI1MCBrZXlpbnRfbWluPTQgc2NlbmVjdXQ9",
  "NDAgaW50cmFfcmVmcmVzaD0wIHJjX2xvb2thaGVhZD02MCByYz1jcmYgbWJ0cmVlPTEgY3JmPTQwLjAgcWNvbXA9MC",
  "42MCBxcG1pbj0wIHFwbWF4PTY5IHFwc3RlcD00IGlwX3JhdGlvPTEuNDAgYXE9MToxLjAwAIAAAAAeZYiBAAJf/juz",
  "wKMohNyZyQq1hGqi4ksyWzk+pjzZAAAACUGaCG2IIf8CNgAAAAlBnhAnEEf/PiEAAAAJAZ4YTUgh/0nA",
);

/// Milliseconds of audio the mock synthesises per input character.
const MS_PER_CHAR: usize = 100;
/// Floor so an empty input still yields a non-trivial file.
const MIN_AUDIO_MS: usize = 100;
const SAMPLE_RATE: u32 = 8_000;

/// One provider type for all six modality traits — see the module docs.
#[derive(Debug, Clone, Default)]
pub struct MockMediaProvider;

impl MockMediaProvider {
  /// Same constructor shape as every vendor provider so the dispatcher's
  /// `Ctor` table can hold it; both arguments are ignored.
  pub fn new(_api_key: &str, _base_url: Option<String>) -> Result<Self> {
    Ok(Self)
  }
}

/// Build a PCM WAV file of `duration_ms` of silence.
pub fn silent_wav(duration_ms: usize) -> Vec<u8> {
  let samples = (SAMPLE_RATE as usize * duration_ms) / 1000;
  let data_len = (samples * 2) as u32;
  let mut out = Vec::with_capacity(44 + data_len as usize);
  out.extend_from_slice(b"RIFF");
  out.extend_from_slice(&(36 + data_len).to_le_bytes());
  out.extend_from_slice(b"WAVE");
  out.extend_from_slice(b"fmt ");
  out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
  out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
  out.extend_from_slice(&1u16.to_le_bytes()); // mono
  out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
  out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
  out.extend_from_slice(&2u16.to_le_bytes()); // block align
  out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
  out.extend_from_slice(b"data");
  out.extend_from_slice(&data_len.to_le_bytes());
  out.resize(44 + data_len as usize, 0);
  out
}

fn png_response() -> ImageGenerationResponse {
  ImageGenerationResponse {
    created: 0,
    images: vec![GeneratedImage {
      url: None,
      b64_json: Some(MOCK_PNG_BASE64.to_string()),
      seed: None,
    }],
    metadata: None,
  }
}

#[async_trait]
impl TtsProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn synthesize(&self, request: TtsRequest) -> Result<TtsResponse> {
    let duration_ms = (request.input.chars().count() * MS_PER_CHAR).max(MIN_AUDIO_MS);
    Ok(TtsResponse {
      audio: silent_wav(duration_ms),
      mime_type: "audio/wav".to_string(),
    })
  }
}

#[async_trait]
impl AsrProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn transcribe(&self, request: AsrRequest) -> Result<AsrResponse> {
    Ok(AsrResponse {
      text: "mock transcript".to_string(),
      metadata: Some(serde_json::json!({ "bytes": request.audio_data.len() })),
    })
  }
}

#[async_trait]
impl Text2ImageProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn generate(&self, request: Text2ImageRequest) -> Result<ImageGenerationResponse> {
    TEXT2IMAGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Some(status_code) = injected_status(&request.prompt) {
      return Err(crate::LLMError::HttpError {
        status_code,
        message: "mock vendor failure".to_string(),
      });
    }
    Ok(png_response())
  }
}

#[async_trait]
impl Image2ImageProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn transform(&self, _request: Image2ImageRequest) -> Result<ImageGenerationResponse> {
    Ok(png_response())
  }
}

#[async_trait]
impl ImageEditProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn edit(&self, _request: ImageEditRequest) -> Result<ImageGenerationResponse> {
    Ok(png_response())
  }
}

#[async_trait]
impl Text2VideoProvider for MockMediaProvider {
  fn name(&self) -> &str {
    "mock"
  }

  async fn submit(&self, _request: Text2VideoRequest) -> Result<VideoGenerationTask> {
    Ok(VideoGenerationTask {
      task_id: "mock-video-task".to_string(),
      metadata: None,
    })
  }

  async fn poll(&self, _task: &VideoGenerationTask) -> Result<VideoGenerationStatus> {
    Ok(VideoGenerationStatus::Completed(VideoGenerationResponse {
      created: 0,
      videos: vec![GeneratedVideo {
        url: None,
        b64_data: Some(MOCK_MP4_BASE64.to_string()),
        duration_seconds: Some(1),
      }],
      metadata: None,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn silent_wav_has_valid_riff_header_and_expected_length() {
    let wav = silent_wav(1000);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    // 8 kHz × 2 bytes × 1 s + 44-byte header.
    assert_eq!(wav.len(), 44 + 16_000);
  }

  #[tokio::test]
  async fn tts_duration_scales_with_input_length() {
    let p = MockMediaProvider;
    let short = p
      .synthesize(TtsRequest {
        model: "m".into(),
        input: "a".repeat(10),
        voice: "v".into(),
        response_format: Some("mp3".into()),
        speed: None,
        volume: None,
        sample_rate: None,
      })
      .await
      .expect("mock never fails");
    let long = p
      .synthesize(TtsRequest {
        model: "m".into(),
        input: "a".repeat(100),
        voice: "v".into(),
        response_format: None,
        speed: None,
        volume: None,
        sample_rate: None,
      })
      .await
      .expect("mock never fails");
    assert_eq!(short.mime_type, "audio/wav");
    assert!(long.audio.len() > short.audio.len() * 5);
  }
}
