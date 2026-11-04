//! Text-to-Video (text → video) provider trait.
//!
//! Unlike every other modality trait in this module, video-generation
//! APIs are **async job APIs**: a request submits a job and returns a
//! handle, and the caller polls that handle until the job finishes
//! (minutes later, not seconds). The trait models that shape directly
//! ([`Text2VideoProvider::submit`] / [`Text2VideoProvider::poll`])
//! instead of pretending it's a synchronous request/response —
//! [`Text2VideoProvider::generate_and_wait`] is a convenience default
//! for callers that just want the final result.

use crate::{LLMError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Request to generate a video from a text prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text2VideoRequest {
  /// Model identifier (e.g. `"veo-3.1-generate-preview"`).
  pub model: String,
  /// Text prompt describing the desired video.
  pub prompt: String,
  /// Output aspect ratio, vendor-specific format (e.g. `"16:9"`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub aspect_ratio: Option<String>,
  /// Output duration in seconds. Vendor-specific allowed values.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub duration_seconds: Option<u32>,
  /// Output resolution, vendor-specific format (e.g. `"1080p"`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub resolution: Option<String>,
  /// Random seed for reproducible generation. Vendor support varies.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seed: Option<i64>,
  /// Vendor-specific extras that don't fit the common fields above
  /// (e.g. reference images, negative prompts). `Text2ImageRequest`
  /// has no equivalent escape hatch, which is why
  /// `TextToImageNode::execute_real_image_generation`'s doc comment
  /// notes vendor-specific fields get silently dropped — this field
  /// exists so the video trait doesn't repeat that lesson.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub extra: Option<serde_json::Value>,
}

/// Handle to a submitted, in-flight (or completed) video generation job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoGenerationTask {
  /// Opaque provider-specific handle (e.g. Google's
  /// `"operations/abc123"`). Round-trip this back into [`poll`](Text2VideoProvider::poll)
  /// verbatim — don't parse or reconstruct it.
  pub task_id: String,
  /// Vendor-specific extras preserved for caller inspection.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metadata: Option<serde_json::Value>,
}

/// Current state of a submitted video generation job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VideoGenerationStatus {
  /// Still running — poll again later.
  Pending,
  /// Finished successfully.
  Completed(VideoGenerationResponse),
  /// Finished with an error. The job will not complete on further polls.
  Failed { message: String },
}

/// Result of a completed video generation job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoGenerationResponse {
  /// Unix timestamp the provider says the video was created.
  pub created: u64,
  /// Generated video(s).
  pub videos: Vec<GeneratedVideo>,
  /// Vendor-specific extras (request id, billing info, etc.) preserved
  /// for caller inspection without forcing every consumer to know
  /// about them.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metadata: Option<serde_json::Value>,
}

/// A single video entry in a [`VideoGenerationResponse`].
///
/// Exactly one of `url` / `b64_data` is typically populated — most
/// video vendors (including Google Veo today) only return a URL, never
/// inline bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedVideo {
  /// URL the vendor will serve the video from (time-limited on most
  /// providers, and may require the same API key to fetch — see
  /// `GoogleVeoClient`'s module docs).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
  /// Base64-encoded video bytes, when a vendor offers inline data.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub b64_data: Option<String>,
  /// Actual output duration in seconds, when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub duration_seconds: Option<u32>,
}

/// Provider trait for text-to-video endpoints.
#[async_trait]
pub trait Text2VideoProvider: Send + Sync {
  /// Short provider identifier, e.g. `"google"`.
  fn name(&self) -> &str;

  /// Submit a video generation job. Returns immediately with a handle
  /// to poll — does not wait for the job to finish.
  async fn submit(&self, request: Text2VideoRequest) -> Result<VideoGenerationTask>;

  /// Check a submitted job's current status. Safe to call repeatedly.
  async fn poll(&self, task: &VideoGenerationTask) -> Result<VideoGenerationStatus>;

  /// Submit `request`, then poll every `poll_interval` until the job
  /// completes, fails, or `max_wait` elapses. Convenience default for
  /// callers that just want the final result — implementors only need
  /// `submit`/`poll`.
  async fn generate_and_wait(
    &self,
    request: Text2VideoRequest,
    poll_interval: std::time::Duration,
    max_wait: std::time::Duration,
  ) -> Result<VideoGenerationResponse> {
    let task = self.submit(request).await?;
    let start = std::time::Instant::now();
    loop {
      match self.poll(&task).await? {
        VideoGenerationStatus::Completed(response) => return Ok(response),
        VideoGenerationStatus::Failed { message } => {
          return Err(LLMError::ResponseParsingError {
            message: format!("video generation failed: {message}"),
          });
        }
        VideoGenerationStatus::Pending => {
          if start.elapsed() >= max_wait {
            return Err(LLMError::TimeoutError {
              timeout_ms: max_wait.as_millis() as u64,
            });
          }
          tokio::time::sleep(poll_interval).await;
        }
      }
    }
  }

  /// Y1.1: materialise a finished video's bytes so a node can write it
  /// as an artifact. The default decodes inline `b64_data` or does a
  /// plain capped GET of `url`; vendors whose download URL needs the
  /// same credentials as the API (Google Veo) override this to attach
  /// them. `max_bytes` bounds the in-memory buffer.
  async fn fetch_video_bytes(&self, video: &GeneratedVideo, max_bytes: u64) -> Result<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    if let Some(b64) = &video.b64_data {
      return STANDARD
        .decode(b64)
        .map_err(|err| LLMError::ResponseParsingError {
          message: format!("video b64_data is not valid base64: {err}"),
        });
    }
    let url = video
      .url
      .as_deref()
      .ok_or_else(|| LLMError::ResponseParsingError {
        message: "generated video carries neither a url nor b64_data".to_string(),
      })?;
    let client = crate::providers::default_http_client()?;
    super::download_with_cap(&client, url, reqwest::header::HeaderMap::new(), max_bytes).await
  }
}
