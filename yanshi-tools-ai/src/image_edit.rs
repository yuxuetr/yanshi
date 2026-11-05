//! `image_edit` — image-edit `Tool` adapter.
//!
//! Thin wrapper over `yanshi_llm::Yanshi::image_edit(model)` — see
//! `tts.rs`'s module docs for the shared design rationale.

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_llm::{ImageEditRequest, Yanshi};
use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

use crate::common::{
  image_generation_output, map_resolution_error, modality_tool_metadata, optional_i32,
  optional_str, optional_u32, required_base64, required_str,
};

/// Edit an existing image per a text instruction, via any image-edit-
/// capable model declared in the model registry.
pub struct ImageEditTool;

impl ImageEditTool {
  pub fn new() -> Self {
    Self
  }
}

impl Default for ImageEditTool {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl Tool for ImageEditTool {
  fn name(&self) -> &str {
    "image_edit"
  }

  fn description(&self) -> &str {
    "Edit an existing image per a text instruction, using a configured \
     image-edit model. Image bytes are supplied inline as base64. Returns \
     each output image as a URL reference or inline base64 data."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "model": {
          "type": "string",
          "description": "Image-edit model name from the model registry (e.g. \"dall-e-2\", \"gpt-image-2\")."
        },
        "image_data": {
          "type": "string",
          "description": "Base64-encoded source image bytes to be edited."
        },
        "image_filename": {
          "type": "string",
          "description": "Original filename — vendors use the extension for content-type detection. Defaults to \"image.png\"."
        },
        "prompt": {
          "type": "string",
          "description": "Text instruction describing the edit."
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
        },
        "size": {
          "type": "string",
          "description": "Output image size, vendor-specific format."
        },
        "response_format": {
          "type": "string",
          "description": "\"url\" or \"b64_json\"."
        }
      },
      "required": ["model", "image_data", "prompt"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    modality_tool_metadata()
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let model = required_str(&params, "model")?;
    let image_data = required_base64(&params, "image_data")?;
    let prompt = required_str(&params, "prompt")?.to_string();

    let provider = Yanshi::image_edit(model)
      .await
      .map_err(|err| map_resolution_error("failed to resolve image-edit model", err))?;

    let request = ImageEditRequest {
      model: model.to_string(),
      image_data,
      image_filename: optional_str(&params, "image_filename")
        .unwrap_or_else(|| "image.png".to_string()),
      prompt,
      seed: optional_i32(&params, "seed"),
      steps: optional_u32(&params, "steps"),
      cfg_scale: params["cfg_scale"].as_f64().map(|n| n as f32),
      size: optional_str(&params, "size"),
      response_format: optional_str(&params, "response_format"),
    };

    match provider.edit(request).await {
      Ok(response) => Ok(image_generation_output("image-edit", response)),
      Err(err) => Ok(ToolOutput::error(format!("image edit failed: {err}"))),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn execute_rejects_missing_image_data() {
    let tool = ImageEditTool::new();
    let err = tool
      .execute(json!({"model": "dall-e-2", "prompt": "add a hat"}))
      .await
      .expect_err("missing image_data must fail params validation");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn execute_rejects_invalid_base64() {
    let tool = ImageEditTool::new();
    let err = tool
      .execute(json!({"model": "dall-e-2", "image_data": "!!!", "prompt": "add a hat"}))
      .await
      .expect_err("malformed base64 must fail params validation");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn execute_surfaces_unknown_model_as_invalid_params() {
    let tool = ImageEditTool::new();
    let err = tool
      .execute(json!({
        "model": "definitely-not-a-real-model",
        "image_data": "AAAA",
        "prompt": "add a hat"
      }))
      .await
      .expect_err("unknown model must fail resolution, not panic or hang");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }
}
