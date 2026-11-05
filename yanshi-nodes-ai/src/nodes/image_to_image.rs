//! ImageToImage Node - Transforms an image based on a text prompt.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
};
use yanshi_llm::{Yanshi, providers::modality::Image2ImageRequest as ModalityImage2ImageRequest};
use yanshi_nodes::common::utils::{flow_value_to_string, load_data_uri_from_source};

use crate::artifacts::{self, ArtifactOutputFormat};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageToImageNode {
  pub name: String,
  pub model: String,
  pub prompt: String,
  pub source_image: String,
  pub source_weight: f32,
  pub size: Option<String>,
  pub n: Option<u32>,
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

impl ImageToImageNode {
  pub fn new(name: &str, model: &str, prompt: &str, source_image: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      prompt: prompt.to_string(),
      source_image: source_image.to_string(),
      source_weight: 0.8,
      size: Some("1024x1024".to_string()),
      n: Some(1),
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
impl AsyncNode for ImageToImageNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    tracing::debug!(name = %self.name, "executing ImageToImageNode");

    let mut resolved_prompt = self.prompt.clone();
    for key in &self.input_keys {
      if let Some(value) = inputs.get(key) {
        let placeholder = format!("{{{{{}}}}}", key);
        resolved_prompt = resolved_prompt.replace(&placeholder, &flow_value_to_string(value));
      }
    }

    let source_url = load_data_uri_from_source(&self.source_image, inputs).await?;

    // P-LLM.3: route through the modality dispatcher.
    let provider =
      Yanshi::image2image(&self.model)
        .await
        .map_err(|e| YanshiError::ConfigurationError {
          message: format!(
            "Failed to resolve image-to-image provider for '{}': {}",
            self.model, e
          ),
        })?;

    let request = ModalityImage2ImageRequest {
      model: self.model.clone(),
      prompt: resolved_prompt,
      source_url,
      source_weight: self.source_weight,
      size: self.size.clone(),
      n: self.n,
      response_format: self.response_format.clone(),
      seed: self.seed,
      steps: self.steps,
      cfg_scale: self.cfg_scale,
    };

    tracing::debug!(provider = %provider.name(), "transforming via provider");
    let response = provider
      .transform(request)
      .await
      .map_err(|e| crate::vendor_error::vendor_error("Image-to-image transform failed", e))?;
    crate::cost::record(&self.model, |p| p.cost_for_images(response.images.len()))?;

    let first_image = response
      .images
      .first()
      .ok_or_else(|| YanshiError::AsyncExecutionError {
        message: "No images returned in response".to_string(),
      })?;
    let value = artifacts::image_output(&self.name, first_image, self.output_format).await?;

    tracing::debug!("ImageToImageNode execution successful");
    let mut outputs = HashMap::new();
    outputs.insert(self.output_key.clone(), value);

    Ok(outputs)
  }
}
