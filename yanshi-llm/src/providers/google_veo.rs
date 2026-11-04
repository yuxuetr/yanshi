//! Google Veo text-to-video client (Gemini API, not Vertex AI).
//!
//! Authenticates identically to [`super::google::GoogleProvider`]
//! (`x-goog-api-key` header against `generativelanguage.googleapis.com`)
//! but speaks the long-running-operation shape Veo uses instead of the
//! synchronous `generateContent` shape chat uses:
//!
//! - `submit`: `POST /v1beta/models/{model}:predictLongRunning` →
//!   `{"name": "operations/..."}`.
//! - `poll`: `GET /v1beta/{operation name}` → `{"done": bool, ...}`,
//!   either `response.generateVideoResponse.generatedSamples[..].video.uri`
//!   on success or an `error` object on failure.
//!
//! The returned video `uri` may require the same API key to fetch bytes
//! from later (it is not always a plain public URL) — downloading and
//! persisting video bytes is out of scope here, same as how the image
//! modality traits only ever hand back the vendor's URL/base64 as-is.

use super::modality::{
  GeneratedVideo, Text2VideoProvider, Text2VideoRequest, VideoGenerationResponse,
  VideoGenerationStatus, VideoGenerationTask,
};
use crate::{LLMError, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct GoogleVeoClient {
  client: Client,
  api_key: String,
  base_url: String,
}

impl GoogleVeoClient {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`]. See
  /// [`crate::providers::OpenAIProvider::with_client`] for the rationale.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "google".to_string(),
      });
    }

    let base_url =
      base_url.unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());

    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_headers(&self) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Same rationale as `GoogleProvider::build_headers` (Q1.8.1): the
    // key rides in a header, never a URL, so it can't leak into
    // `reqwest::Error::to_string()` output.
    headers.insert(
      "x-goog-api-key",
      HeaderValue::from_str(&self.api_key).map_err(|err| LLMError::ConfigurationError {
        message: format!("Google API key contains non-ASCII bytes: {err}"),
      })?,
    );
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  fn build_request_body(&self, request: &Text2VideoRequest) -> Value {
    let mut instance = serde_json::Map::new();
    instance.insert("prompt".to_string(), Value::String(request.prompt.clone()));

    let mut parameters = serde_json::Map::new();
    if let Some(ref aspect_ratio) = request.aspect_ratio {
      parameters.insert(
        "aspectRatio".to_string(),
        Value::String(aspect_ratio.clone()),
      );
    }
    if let Some(duration_seconds) = request.duration_seconds {
      parameters.insert(
        "durationSeconds".to_string(),
        Value::Number(duration_seconds.into()),
      );
    }
    if let Some(ref resolution) = request.resolution {
      parameters.insert("resolution".to_string(), Value::String(resolution.clone()));
    }
    if let Some(seed) = request.seed {
      parameters.insert("seed".to_string(), Value::Number(seed.into()));
    }
    if let Some(Value::Object(extra)) = &request.extra {
      parameters.extend(extra.clone());
    }

    json!({
      "instances": [instance],
      "parameters": Value::Object(parameters),
    })
  }
}

/// `POST .../{model}:predictLongRunning` response shape.
#[derive(Debug, Deserialize)]
struct SubmitResponse {
  name: String,
}

/// `GET .../{operation name}` response shape. `response`/`error` are
/// mutually exclusive and only present once `done` is `true`.
#[derive(Debug, Deserialize)]
struct OperationResponse {
  done: bool,
  #[serde(default)]
  response: Option<GenerateVideoResponseEnvelope>,
  #[serde(default)]
  error: Option<OperationError>,
}

#[derive(Debug, Deserialize)]
struct OperationError {
  message: String,
}

#[derive(Debug, Deserialize)]
struct GenerateVideoResponseEnvelope {
  #[serde(rename = "generateVideoResponse")]
  generate_video_response: GenerateVideoResponse,
}

#[derive(Debug, Deserialize)]
struct GenerateVideoResponse {
  #[serde(default, rename = "generatedSamples")]
  generated_samples: Vec<GeneratedSample>,
}

#[derive(Debug, Deserialize)]
struct GeneratedSample {
  video: VideoRef,
}

#[derive(Debug, Deserialize)]
struct VideoRef {
  uri: String,
}

/// Parse an [`OperationResponse`] into the trait's [`VideoGenerationStatus`].
/// Pure function — no I/O — so it can be unit-tested directly against
/// hand-written fixture JSON without a live API call.
fn parse_operation_response(op: OperationResponse) -> Result<VideoGenerationStatus> {
  if let Some(error) = op.error {
    return Ok(VideoGenerationStatus::Failed {
      message: error.message,
    });
  }
  if !op.done {
    return Ok(VideoGenerationStatus::Pending);
  }
  let response = op.response.ok_or_else(|| LLMError::ResponseParsingError {
    message: "Veo operation marked done but carried neither `response` nor `error`".to_string(),
  })?;

  let videos = response
    .generate_video_response
    .generated_samples
    .into_iter()
    .map(|sample| GeneratedVideo {
      url: Some(sample.video.uri),
      b64_data: None,
      duration_seconds: None,
    })
    .collect();

  Ok(VideoGenerationStatus::Completed(VideoGenerationResponse {
    created: 0,
    videos,
    metadata: None,
  }))
}

#[async_trait]
impl Text2VideoProvider for GoogleVeoClient {
  fn name(&self) -> &str {
    "google"
  }

  async fn submit(&self, request: Text2VideoRequest) -> Result<VideoGenerationTask> {
    let url = format!(
      "{}/v1beta/models/{}:predictLongRunning",
      self.base_url, request.model
    );
    let body = self.build_request_body(&request);

    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    let submitted: SubmitResponse = response.json().await?;
    Ok(VideoGenerationTask {
      task_id: submitted.name,
      metadata: None,
    })
  }

  async fn poll(&self, task: &VideoGenerationTask) -> Result<VideoGenerationStatus> {
    let url = format!("{}/v1beta/{}", self.base_url, task.task_id);

    let response = self
      .client
      .get(&url)
      .headers(self.build_headers()?)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let error_text = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message: error_text,
      });
    }

    let op: OperationResponse = response.json().await?;
    parse_operation_response(op)
  }

  /// Veo's `video.uri` is served by the same API host and needs the
  /// same `x-goog-api-key` header; a plain GET returns 403.
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
    let mut headers = self.build_headers()?;
    headers.remove(reqwest::header::CONTENT_TYPE);
    super::modality::download_with_cap(&self.client, url, headers, max_bytes).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn client_creation_requires_api_key() {
    assert!(GoogleVeoClient::new("", None).is_err());
    assert!(GoogleVeoClient::new("test-key", None).is_ok());
  }

  #[test]
  fn client_creation_defaults_base_url() {
    let client = GoogleVeoClient::new("test-key", None).unwrap();
    assert_eq!(client.base_url, "https://generativelanguage.googleapis.com");
  }

  #[test]
  fn parses_pending_operation() {
    let op: OperationResponse = serde_json::from_value(json!({
      "name": "operations/abc123",
      "done": false,
    }))
    .unwrap();
    let status = parse_operation_response(op).unwrap();
    assert!(matches!(status, VideoGenerationStatus::Pending));
  }

  #[test]
  fn parses_completed_operation_and_extracts_video_url() {
    let op: OperationResponse = serde_json::from_value(json!({
      "name": "operations/abc123",
      "done": true,
      "response": {
        "generateVideoResponse": {
          "generatedSamples": [
            { "video": { "uri": "https://example.com/video.mp4" } }
          ]
        }
      }
    }))
    .unwrap();
    let status = parse_operation_response(op).unwrap();
    match status {
      VideoGenerationStatus::Completed(response) => {
        assert_eq!(response.videos.len(), 1);
        assert_eq!(
          response.videos[0].url.as_deref(),
          Some("https://example.com/video.mp4")
        );
      }
      other => panic!("expected Completed, got {other:?}"),
    }
  }

  #[test]
  fn parses_failed_operation() {
    let op: OperationResponse = serde_json::from_value(json!({
      "name": "operations/abc123",
      "done": true,
      "error": { "code": 3, "message": "prompt violates content policy" }
    }))
    .unwrap();
    let status = parse_operation_response(op).unwrap();
    match status {
      VideoGenerationStatus::Failed { message } => {
        assert_eq!(message, "prompt violates content policy");
      }
      other => panic!("expected Failed, got {other:?}"),
    }
  }

  #[test]
  fn done_without_response_or_error_is_a_parsing_error() {
    let op: OperationResponse = serde_json::from_value(json!({
      "name": "operations/abc123",
      "done": true,
    }))
    .unwrap();
    assert!(parse_operation_response(op).is_err());
  }

  #[test]
  fn request_body_maps_common_fields_and_merges_extra() {
    let client = GoogleVeoClient::new("test-key", None).unwrap();
    let request = Text2VideoRequest {
      model: "veo-3.1-generate-preview".to_string(),
      prompt: "a cat playing piano".to_string(),
      aspect_ratio: Some("16:9".to_string()),
      duration_seconds: Some(8),
      resolution: Some("1080p".to_string()),
      seed: Some(42),
      extra: Some(json!({ "personGeneration": "allow_adult" })),
    };
    let body = client.build_request_body(&request);
    assert_eq!(body["instances"][0]["prompt"], "a cat playing piano");
    assert_eq!(body["parameters"]["aspectRatio"], "16:9");
    assert_eq!(body["parameters"]["durationSeconds"], 8);
    assert_eq!(body["parameters"]["resolution"], "1080p");
    assert_eq!(body["parameters"]["seed"], 42);
    assert_eq!(body["parameters"]["personGeneration"], "allow_adult");
  }
}
