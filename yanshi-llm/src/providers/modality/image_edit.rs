//! Image-Edit (image bytes + text → image) provider trait.

use super::ImageGenerationResponse;
use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Request to edit an existing image with a text instruction.
///
/// Unlike [`Image2ImageRequest`](super::Image2ImageRequest) which takes
/// a URL, edit endpoints typically need the raw image bytes uploaded
/// as multipart form data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEditRequest {
  /// Model identifier (e.g. `"dall-e-2"`, `"gpt-image-2"`).
  pub model: String,
  /// Raw image bytes to be edited.
  pub image_data: Vec<u8>,
  /// Original filename (vendors use the extension for content-type).
  pub image_filename: String,
  /// Text instruction describing the edit.
  pub prompt: String,
  /// Random seed.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seed: Option<i32>,
  /// Sampling steps.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub steps: Option<u32>,
  /// Classifier-free guidance scale.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cfg_scale: Option<f32>,
  /// Output image size, vendor-specific format.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub size: Option<String>,
  /// Wire response format: `"url"` or `"b64_json"`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_format: Option<String>,
}

/// Provider trait for image-edit endpoints.
#[async_trait]
pub trait ImageEditProvider: Send + Sync {
  /// Short provider identifier.
  fn name(&self) -> &str;

  /// Apply the edit described by `request`.
  async fn edit(&self, request: ImageEditRequest) -> Result<ImageGenerationResponse>;
}
