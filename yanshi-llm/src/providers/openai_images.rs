//! OpenAI image generation + edit provider.
//!
//! Implements both [`Text2ImageProvider`] (`POST {base_url}/images/generations`,
//! JSON body) and [`ImageEditProvider`] (`POST {base_url}/images/edits`,
//! multipart form) on one struct — they share the same base URL and
//! auth, and both return the same `{data: [...], created}` response
//! shape, mirroring how `StepFunSpecializedClient` implements multiple
//! modality traits on one struct.
//!
//! Model note (P-LLM2.3 Batch 1): `gpt-image-2` supports generation but
//! currently rejects the edit endpoint — only `gpt-image-1` /
//! `gpt-image-1.5` / `gpt-image-1-mini` / `chatgpt-image-latest` /
//! `dall-e-2` support edit. The registry carries `gpt-image-1` as
//! `type: image_edit` for this reason; this provider doesn't gate models
//! itself — the API returns a 400 for an incompatible model/endpoint
//! pairing, surfaced verbatim as `LLMError::HttpError`.

use crate::{
  LLMError, Result,
  providers::modality::{
    GeneratedImage, Image2ImageProvider, Image2ImageRequest, ImageEditProvider, ImageEditRequest,
    ImageGenerationResponse, Text2ImageProvider, Text2ImageRequest,
  },
};
use async_trait::async_trait;
use reqwest::{
  Client,
  multipart::{Form, Part},
};
use serde_json::Value;

pub struct OpenAIImageProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl std::fmt::Debug for OpenAIImageProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OpenAIImageProvider")
      .field("base_url", &self.base_url)
      .field("api_key", &"<redacted>")
      .finish()
  }
}

impl OpenAIImageProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`]. Mirrors
  /// `OpenAIAsrProvider::with_client`.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "openai".to_string(),
      });
    }
    let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_generate_body(request: &Text2ImageRequest) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("prompt".to_string(), Value::String(request.prompt.clone()));
    if let Some(ref size) = request.size {
      body.insert("size".to_string(), Value::String(size.clone()));
    }
    if let Some(n) = request.n {
      body.insert("n".to_string(), Value::Number(n.into()));
    }
    if let Some(ref response_format) = request.response_format
      && accepts_response_format(&request.model)
    {
      body.insert(
        "response_format".to_string(),
        Value::String(response_format.clone()),
      );
    }
    Value::Object(body)
  }

  /// Build the multipart form for an [`ImageEditRequest`]. Public-in-crate
  /// so unit tests can exercise construction without a network call —
  /// mirrors `OpenAIAsrProvider::build_form`.
  pub(crate) fn build_edit_form(request: &ImageEditRequest) -> Form {
    let image_part = Part::bytes(request.image_data.clone())
      .file_name(request.image_filename.clone())
      .mime_str(mime_for_filename(&request.image_filename))
      .unwrap_or_else(|_| {
        Part::bytes(request.image_data.clone()).file_name(request.image_filename.clone())
      });

    let mut form = Form::new()
      .text("model", request.model.clone())
      .text("prompt", request.prompt.clone())
      .part("image", image_part);

    if let Some(ref size) = request.size {
      form = form.text("size", size.clone());
    }
    if let Some(ref response_format) = request.response_format
      && accepts_response_format(&request.model)
    {
      form = form.text("response_format", response_format.clone());
    }
    form
  }
}

/// Only DALL·E takes `response_format`; the GPT image models always
/// return `b64_json` and answer the field with a 400 `unknown_parameter`.
/// [`parse_image_response`] reads either shape, so dropping it loses
/// nothing.
fn accepts_response_format(model: &str) -> bool {
  model.starts_with("dall-e")
}

/// Map a filename's extension to an image MIME type. Falls back to
/// `application/octet-stream` (the request still flies — the server
/// reads the codec from bytes).
fn mime_for_filename(filename: &str) -> &'static str {
  let lower = filename.to_lowercase();
  if lower.ends_with(".png") {
    "image/png"
  } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
    "image/jpeg"
  } else if lower.ends_with(".webp") {
    "image/webp"
  } else {
    "application/octet-stream"
  }
}

/// Decode an OpenAI `{data: [...], created}` image response body
/// (shared shape between `/images/generations` and `/images/edits`)
/// into the modality [`ImageGenerationResponse`] envelope.
pub(crate) fn parse_image_response(body: &str) -> Result<ImageGenerationResponse> {
  let value: Value = serde_json::from_str(body).map_err(|e| LLMError::ResponseParsingError {
    message: format!("OpenAI image response JSON parse failed: {e}"),
  })?;

  let created = value.get("created").and_then(|v| v.as_u64()).unwrap_or(0);

  let data = value
    .get("data")
    .and_then(|v| v.as_array())
    .ok_or_else(|| LLMError::ResponseParsingError {
      message: format!("OpenAI image response missing 'data' array. Body: {body}"),
    })?;

  let images = data
    .iter()
    .map(|entry| GeneratedImage {
      url: entry.get("url").and_then(|v| v.as_str()).map(String::from),
      b64_json: entry
        .get("b64_json")
        .and_then(|v| v.as_str())
        .map(String::from),
      seed: None,
    })
    .collect();

  Ok(ImageGenerationResponse {
    created,
    images,
    metadata: Some(value),
  })
}

#[async_trait]
impl Text2ImageProvider for OpenAIImageProvider {
  fn name(&self) -> &str {
    "openai"
  }

  async fn generate(&self, request: Text2ImageRequest) -> Result<ImageGenerationResponse> {
    let url = format!("{}/images/generations", self.base_url);
    let body = Self::build_generate_body(&request);

    let response = self
      .client
      .post(&url)
      .bearer_auth(&self.api_key)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let message = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message,
      });
    }

    parse_image_response(&response.text().await?)
  }
}

#[async_trait]
impl ImageEditProvider for OpenAIImageProvider {
  fn name(&self) -> &str {
    "openai"
  }

  async fn edit(&self, request: ImageEditRequest) -> Result<ImageGenerationResponse> {
    let url = format!("{}/images/edits", self.base_url);
    let form = Self::build_edit_form(&request);

    let response = self
      .client
      .post(&url)
      .bearer_auth(&self.api_key)
      .multipart(form)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let message = response.text().await.unwrap_or_default();
      return Err(LLMError::HttpError {
        status_code,
        message,
      });
    }

    parse_image_response(&response.text().await?)
  }
}

/// Largest source image accepted for `/images/edits` (OpenAI's limit for
/// GPT image models is 50 MB per image).
const MAX_SOURCE_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// Y6.6: image-to-image is `/images/edits` with the source image and no
/// mask (the whole image may change). `source_weight`, `steps` and
/// `cfg_scale` have no OpenAI counterpart and are not sent.
#[async_trait]
impl Image2ImageProvider for OpenAIImageProvider {
  fn name(&self) -> &str {
    "openai"
  }

  async fn transform(&self, request: Image2ImageRequest) -> Result<ImageGenerationResponse> {
    let (image_data, mime) =
      super::modality::source_image_bytes(&request.source_url, MAX_SOURCE_IMAGE_BYTES).await?;
    let extension = match mime.as_str() {
      "image/jpeg" => "jpg",
      "image/webp" => "webp",
      _ => "png",
    };
    self
      .edit(ImageEditRequest {
        model: request.model,
        image_data,
        image_filename: format!("source.{extension}"),
        prompt: request.prompt,
        seed: None,
        steps: None,
        cfg_scale: None,
        size: request.size,
        response_format: request.response_format,
      })
      .await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn empty_api_key_is_rejected_at_construction() {
    let err = OpenAIImageProvider::new("", None).unwrap_err();
    assert!(matches!(err, LLMError::MissingApiKey { ref provider } if provider == "openai"));
  }

  #[test]
  fn parse_image_response_extracts_url() {
    let body = json!({
      "created": 1700000000,
      "data": [{ "url": "https://example.com/img.png", "revised_prompt": "a cat" }]
    })
    .to_string();
    let response = parse_image_response(&body).expect("parse ok");
    assert_eq!(response.created, 1700000000);
    assert_eq!(response.images.len(), 1);
    assert_eq!(
      response.images[0].url.as_deref(),
      Some("https://example.com/img.png")
    );
    assert!(response.images[0].b64_json.is_none());
  }

  #[test]
  fn parse_image_response_extracts_b64_json() {
    let body = json!({
      "created": 1700000000,
      "data": [{ "b64_json": "aGVsbG8=" }]
    })
    .to_string();
    let response = parse_image_response(&body).expect("parse ok");
    assert_eq!(response.images[0].b64_json.as_deref(), Some("aGVsbG8="));
    assert!(response.images[0].url.is_none());
  }

  #[test]
  fn parse_image_response_with_missing_data_returns_typed_error() {
    let body = json!({ "created": 1700000000 }).to_string();
    let err = parse_image_response(&body).unwrap_err();
    assert!(err.to_string().contains("missing 'data' array"));
  }

  #[test]
  fn parse_image_response_with_invalid_json_returns_typed_error() {
    let err = parse_image_response("{not json").unwrap_err();
    assert!(err.to_string().contains("JSON parse failed"));
  }

  #[test]
  fn mime_for_filename_covers_documented_formats() {
    assert_eq!(mime_for_filename("img.png"), "image/png");
    assert_eq!(mime_for_filename("img.jpg"), "image/jpeg");
    assert_eq!(mime_for_filename("img.jpeg"), "image/jpeg");
    assert_eq!(mime_for_filename("img.webp"), "image/webp");
    assert_eq!(mime_for_filename("img.unknown"), "application/octet-stream");
    assert_eq!(mime_for_filename("IMG.PNG"), "image/png");
  }

  #[test]
  fn build_edit_form_smoke_test() {
    let minimal = ImageEditRequest {
      model: "gpt-image-1".into(),
      image_data: vec![1, 2, 3],
      image_filename: "source.png".into(),
      prompt: "add a hat".into(),
      seed: None,
      steps: None,
      cfg_scale: None,
      size: None,
      response_format: None,
    };
    let _ = OpenAIImageProvider::build_edit_form(&minimal);

    let full = ImageEditRequest {
      model: "gpt-image-1".into(),
      image_data: vec![1, 2, 3],
      image_filename: "source.png".into(),
      prompt: "add a hat".into(),
      seed: Some(42),
      steps: Some(20),
      cfg_scale: Some(7.5),
      size: Some("1024x1024".into()),
      response_format: Some("b64_json".into()),
    };
    let _ = OpenAIImageProvider::build_edit_form(&full);
  }

  #[test]
  fn build_generate_body_includes_optional_fields() {
    let request = Text2ImageRequest {
      model: "gpt-image-2".into(),
      prompt: "a red square".into(),
      size: Some("1024x1024".into()),
      n: Some(2),
      response_format: Some("b64_json".into()),
      seed: None,
      steps: None,
      cfg_scale: None,
    };
    let body = OpenAIImageProvider::build_generate_body(&request);
    assert_eq!(body["model"], "gpt-image-2");
    assert_eq!(body["prompt"], "a red square");
    assert_eq!(body["size"], "1024x1024");
    assert_eq!(body["n"], 2);
    assert!(
      body.get("response_format").is_none(),
      "gpt-image-* rejects response_format with a 400"
    );

    let dalle = Text2ImageRequest {
      model: "dall-e-3".into(),
      ..request
    };
    let body = OpenAIImageProvider::build_generate_body(&dalle);
    assert_eq!(body["response_format"], "b64_json");
  }
}
