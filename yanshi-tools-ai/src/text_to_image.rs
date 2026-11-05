//! `text_to_image` — text-to-image generation `Tool` adapter.
//!
//! Thin wrapper over `yanshi_llm::Yanshi::text2image_for(model)` —
//! see `tts.rs`'s module docs for the shared design rationale.

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_llm::{Text2ImageRequest, Yanshi};
use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

use crate::common::{
  image_generation_output, map_resolution_error, modality_tool_metadata, optional_i32,
  optional_str, optional_u32, required_str,
};

/// Generate image(s) from a text prompt via any text-to-image-capable
/// model declared in the model registry.
pub struct Text2ImageTool;

impl Text2ImageTool {
  pub fn new() -> Self {
    Self
  }
}

impl Default for Text2ImageTool {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl Tool for Text2ImageTool {
  fn name(&self) -> &str {
    "text_to_image"
  }

  fn description(&self) -> &str {
    "Generate image(s) from a text prompt using a configured text-to-image model. \
     Returns each image as a URL reference or inline base64 data."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "model": {
          "type": "string",
          "description": "Text-to-image model name from the model registry (e.g. \"dall-e-3\", \"gpt-image-2\")."
        },
        "prompt": {
          "type": "string",
          "description": "Text prompt describing the desired image."
        },
        "size": {
          "type": "string",
          "description": "Output image size, vendor-specific format (e.g. \"1024x1024\")."
        },
        "n": {
          "type": "integer",
          "description": "Number of images to generate. Some vendors only support 1."
        },
        "response_format": {
          "type": "string",
          "description": "\"url\" or \"b64_json\". Defaults to the vendor's own default."
        },
        "seed": {
          "type": "integer",
          "description": "Random seed for reproducible generation. Vendor support varies."
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
      "required": ["model", "prompt"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    modality_tool_metadata()
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let model = required_str(&params, "model")?;
    let prompt = required_str(&params, "prompt")?.to_string();

    let provider = Yanshi::text2image_for(model)
      .await
      .map_err(|err| map_resolution_error("failed to resolve text-to-image model", err))?;

    let request = Text2ImageRequest {
      model: model.to_string(),
      prompt,
      size: optional_str(&params, "size"),
      n: optional_u32(&params, "n"),
      response_format: optional_str(&params, "response_format"),
      seed: optional_i32(&params, "seed"),
      steps: optional_u32(&params, "steps"),
      cfg_scale: params["cfg_scale"].as_f64().map(|n| n as f32),
    };

    match provider.generate(request).await {
      Ok(response) => Ok(image_generation_output("text-to-image", response)),
      Err(err) => Ok(ToolOutput::error(format!(
        "text-to-image generation failed: {err}"
      ))),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn execute_rejects_missing_prompt() {
    let tool = Text2ImageTool::new();
    let err = tool
      .execute(json!({"model": "dall-e-3"}))
      .await
      .expect_err("missing prompt must fail params validation");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn execute_surfaces_unknown_model_as_invalid_params() {
    let tool = Text2ImageTool::new();
    let err = tool
      .execute(json!({"model": "definitely-not-a-real-model", "prompt": "a cat"}))
      .await
      .expect_err("unknown model must fail resolution, not panic or hang");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }
}
