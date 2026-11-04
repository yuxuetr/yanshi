//! ImageEdit Node - Specialized node for AI-powered image editing (inpainting/outpainting)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
};
use yanshi_llm::{Yanshi, providers::modality::ImageEditRequest as ModalityImageEditRequest};
use yanshi_nodes::common::utils::{flow_value_to_string, load_bytes_from_source};

use crate::artifacts::{self, ArtifactOutputFormat};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEditNode {
  pub name: String,
  pub model: String,
  pub prompt: String,
  pub image_source: String,
  pub size: Option<String>,
  pub response_format: Option<String>,
  pub seed: Option<i32>,
  pub steps: Option<u32>,
  pub cfg_scale: Option<f32>,
  pub output_key: String,
  pub input_keys: Vec<String>,
  /// Y1.1: see [`ArtifactOutputFormat`].
  #[serde(default)]
  pub output_format: ArtifactOutputFormat,
}

impl ImageEditNode {
  pub fn new(name: &str, model: &str, prompt: &str, image_source: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      prompt: prompt.to_string(),
      image_source: image_source.to_string(),
      size: Some("1024x1024".to_string()),
      response_format: Some("b64_json".to_string()),
      seed: None,
      steps: None,
      cfg_scale: None,
      output_key: format!("{}_output", name),
      input_keys: vec![],
      output_format: ArtifactOutputFormat::default(),
    }
  }

  pub fn with_output_format(mut self, format: ArtifactOutputFormat) -> Self {
    self.output_format = format;
    self
  }
}

#[async_trait]
impl AsyncNode for ImageEditNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    tracing::debug!(name = %self.name, "executing ImageEditNode");

    let mut resolved_prompt = self.prompt.clone();
    for key in &self.input_keys {
      if let Some(value) = inputs.get(key) {
        let placeholder = format!("{{{{{}}}}}", key);
        resolved_prompt = resolved_prompt.replace(&placeholder, &flow_value_to_string(value));
      }
    }

    let image_data = load_bytes_from_source(&self.image_source, inputs).await?;

    // P-LLM.3: route through the modality dispatcher.
    let provider =
      Yanshi::image_edit(&self.model)
        .await
        .map_err(|e| YanshiError::ConfigurationError {
          message: format!(
            "Failed to resolve image-edit provider for '{}': {}",
            self.model, e
          ),
        })?;

    let request = ModalityImageEditRequest {
      model: self.model.clone(),
      prompt: resolved_prompt,
      image_data,
      image_filename: self.image_source.clone(),
      seed: self.seed,
      steps: self.steps,
      cfg_scale: self.cfg_scale,
      size: self.size.clone(),
      response_format: self.response_format.clone(),
    };

    tracing::debug!(provider = %provider.name(), "editing image via provider");
    let response = provider
      .edit(request)
      .await
      .map_err(|e| crate::vendor_error::vendor_error("Image edit failed", e))?;
    crate::cost::record(&self.model, |p| p.cost_for_images(response.images.len()))?;

    let first_image = response
      .images
      .first()
      .ok_or_else(|| YanshiError::AsyncExecutionError {
        message: "No images returned in response".to_string(),
      })?;
    let value = artifacts::image_output(&self.name, first_image, self.output_format).await?;

    tracing::debug!("ImageEditNode execution successful");
    let mut outputs = HashMap::new();
    outputs.insert(self.output_key.clone(), value);

    Ok(outputs)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::Value;
  use yanshi_core::value::FlowValue;

  #[tokio::test]
  #[ignore] // Requires STEPFUN_API_KEY environment variable
  async fn test_image_edit_node_integration() {
    if std::env::var("STEP_API_KEY").is_err() {
      println!("Skipping ImageEdit integration test: STEP_API_KEY not set.");
      return;
    }

    let node = ImageEditNode::new(
      "test_edit",
      "step-1x-edit",
      "add a blue sky to the top half of the image",
      "image.png",
    );

    let mut inputs = AsyncNodeInputs::new();
    // A minimal, silent PNG file encoded in base64
    const TEST_IMAGE_BASE64: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAAAAACPAi4CAAAABGdBTUEAALGPC/xhBQAAAA1JREFUGFdjYGBgYAAAAAUAAYcA/wAAAABJRU5ErkJggg==";
    inputs.insert(
      "image.png".to_string(),
      FlowValue::Json(Value::String(TEST_IMAGE_BASE64.to_string())),
    );

    let result = node.execute(&inputs).await;
    assert!(result.is_ok(), "Node execution failed: {:?}", result.err());

    let outputs = result.unwrap();
    let output_value = outputs.get("test_edit_output").unwrap();
    if let FlowValue::Json(Value::String(data)) = output_value {
      assert!(data.starts_with("data:image/png;base64,") || data.starts_with("http"));
      let base64_part = data.split(",").nth(1).unwrap();
      assert!(!base64_part.is_empty(), "Base64 data should not be empty");
    } else {
      panic!("Output was not a FlowValue::Json(Value::String(...))");
    }
  }
}
