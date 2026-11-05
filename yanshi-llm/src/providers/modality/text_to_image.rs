//! Text-to-Image (text → image) provider trait.

use super::ImageGenerationResponse;
use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Request to generate an image from a text prompt.
///
/// Field set is the intersection of OpenAI DALL-E 3 / Imagen / StepFun
/// T2I — anything vendor-specific that doesn't fit here (StepFun's
/// `style_reference`, DALL-E `quality`) should land on the vendor's
/// own request struct that the modality impl can take a closure into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text2ImageRequest {
  /// Model identifier (e.g. `"gpt-image-2"`, `"dall-e-3"`,
  /// `"imagen-3.0-generate-002"`).
  pub model: String,
  /// Text prompt describing the desired image.
  pub prompt: String,
  /// Output image size, vendor-specific format (e.g. `"1024x1024"`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub size: Option<String>,
  /// Number of images to generate. Some vendors only support `1`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub n: Option<u32>,
  /// Wire response format: `"url"` (default for most vendors) or
  /// `"b64_json"`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_format: Option<String>,
  /// Random seed for reproducible generation. Vendor support varies.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seed: Option<i32>,
  /// Sampling steps. Vendor-specific range.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub steps: Option<u32>,
  /// Classifier-free guidance scale. Vendor-specific range.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cfg_scale: Option<f32>,
}

/// Provider trait for text-to-image endpoints.
#[async_trait]
pub trait Text2ImageProvider: Send + Sync {
  /// Short provider identifier, e.g. `"stepfun"` or `"openai"`.
  fn name(&self) -> &str;

  /// Generate image(s) from `request`.
  async fn generate(&self, request: Text2ImageRequest) -> Result<ImageGenerationResponse>;
}
