//! `image_to_image` — image-to-image transformation `Tool` adapter.
//!
//! Thin wrapper over `yanshi_llm::Yanshi::image2image(model)` — see
//! `tts.rs`'s module docs for the shared design rationale.

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_llm::{Image2ImageRequest, Yanshi};
use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

use crate::common::{
  image_generation_output, map_resolution_error, modality_tool_metadata, optional_i32,
  optional_str, optional_u32, required_str,
};

/// Transform a source image, guided by a text prompt, via any
/// image-to-image-capable model declared in the model registry.
pub struct Image2ImageTool;

impl Image2ImageTool {
  pub fn new() -> Self {
    Self
  }
}

impl Default for Image2ImageTool {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl Tool for Image2ImageTool {
  fn name(&self) -> &str {
    "image_to_image"
  }

  fn description(&self) -> &str {
    "Transform a source image guided by a text prompt, using a configured \
     image-to-image model. Returns each output image as a URL reference \
     or inline base64 data."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "model": {
          "type": "string",
          "description": "Image-to-image model name from the model registry."
        },
        "prompt": {
          "type": "string",
          "description": "Text prompt for the transformation."
        },
        "source_url": {
          "type": "string",
          "description": "Source image — a public HTTP(S) URL or a `data:<mime>;base64,<payload>` URI."
        },
        "source_weight": {
          "type": "number",
          "description": "Weight of the source image in the (0.0, 1.0] range. Higher keeps the output closer to the source. Defaults to 0.5."
        },
        "size": {
          "type": "string",
          "description": "Output image size, vendor-specific format."
        },
        "n": {
          "type": "integer",
          "description": "Number of images to generate."
        },
        "response_format": {
          "type": "string",
          "description": "\"url\" or \"b64_json\"."
        },
        "seed": {
          "type": "integer",
          "description": "Random seed. Vendor support varies."
        },
        "steps": {
          "type": "integer",
          "description": "Sampling steps. Vendor-specific range."
        },
        "cfg_scale": {
          "type": "number",
          "description": "Classifier-free guidance scale. Vendor-specific range."
        }
      },
      "required": ["model", "prompt", "source_url"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    modality_tool_metadata()
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let model = required_str(&params, "model")?;
    let prompt = required_str(&params, "prompt")?.to_string();
    let source_url = required_str(&params, "source_url")?.to_string();

    let provider = Yanshi::image2image(model)
      .await
      .map_err(|err| map_resolution_error("failed to resolve image-to-image model", err))?;

    let request = Image2ImageRequest {
      model: model.to_string(),
      prompt,
      source_url,
      source_weight: params["source_weight"]
        .as_f64()
        .map(|n| n as f32)
        .unwrap_or(0.5),
      size: optional_str(&params, "size"),
      n: optional_u32(&params, "n"),
      response_format: optional_str(&params, "response_format"),
      seed: optional_i32(&params, "seed"),
      steps: optional_u32(&params, "steps"),
      cfg_scale: params["cfg_scale"].as_f64().map(|n| n as f32),
    };

    match provider.transform(request).await {
      Ok(response) => Ok(image_generation_output("image-to-image", response)),
      Err(err) => Ok(ToolOutput::error(format!(
        "image-to-image transformation failed: {err}"
      ))),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn execute_rejects_missing_source_url() {
    let tool = Image2ImageTool::new();
    let err = tool
      .execute(json!({"model": "gpt-image-2", "prompt": "make it blue"}))
      .await
      .expect_err("missing source_url must fail params validation");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn execute_surfaces_unknown_model_as_invalid_params() {
    let tool = Image2ImageTool::new();
    let err = tool
      .execute(json!({
        "model": "definitely-not-a-real-model",
        "prompt": "make it blue",
        "source_url": "https://example.com/cat.png"
      }))
      .await
      .expect_err("unknown model must fail resolution, not panic or hang");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }
}
