//! ImageUnderstand Node - Specialized node for multimodal image understanding using vision models

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::{Yanshi, multimodal::MultimodalMessage};
use yanshi_nodes::common::utils::{flow_value_to_string, load_data_uri_from_source};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUnderstandNode {
  pub name: String,
  pub model: String,
  pub text_prompt: String,
  pub image_source: String,
  pub system_message: Option<String>,
  pub temperature: Option<f32>,
  pub max_tokens: Option<u32>,
  pub output_key: String,
  pub input_keys: Vec<String>,
}

impl ImageUnderstandNode {
  pub fn new(name: &str, model: &str, text_prompt: &str, image_source: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      text_prompt: text_prompt.to_string(),
      image_source: image_source.to_string(),
      system_message: None,
      temperature: None,
      max_tokens: None,
      output_key: format!("{}_output", name),
      input_keys: vec![],
    }
  }
}

#[async_trait]
impl AsyncNode for ImageUnderstandNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    tracing::debug!(name = %self.name, "executing ImageUnderstandNode");

    Yanshi::ensure_init()
      .await
      .map_err(|e| YanshiError::ConfigurationError {
        message: format!("Failed to initialize Yanshi LLM service: {}", e),
      })?;

    let mut resolved_prompt = self.text_prompt.clone();
    for key in &self.input_keys {
      if let Some(value) = inputs.get(key) {
        let placeholder = format!("{{{{{}}}}}", key);
        resolved_prompt = resolved_prompt.replace(&placeholder, &flow_value_to_string(value));
      }
    }

    let image_data_uri = load_data_uri_from_source(&self.image_source, inputs).await?;

    let message = MultimodalMessage::user()
      .add_text(resolved_prompt)
      .add_image_url(image_data_uri)
      .build();

    let mut request = Yanshi::model(&self.model).multimodal_prompt(message);

    if let Some(system_message) = &self.system_message {
      request = request.system(system_message);
    }
    if let Some(temp) = self.temperature {
      request = request.temperature(temp);
    }
    if let Some(max_tokens) = self.max_tokens {
      request = request.max_tokens(max_tokens);
    }

    let response = request
      .execute()
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("LLM execution failed: {}", e),
      })?;

    tracing::debug!("ImageUnderstandNode execution successful");
    let mut outputs = HashMap::new();
    outputs.insert(
      self.output_key.clone(),
      FlowValue::Json(Value::String(response)),
    );

    Ok(outputs)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn test_image_understand_node_integration() {
    if std::env::var("STEP_API_KEY").is_err() {
      println!("Skipping ImageUnderstand integration test: STEP_API_KEY not set.");
      return;
    }

    let node = ImageUnderstandNode::new(
      "test_vision",
      "step-1o-turbo-vision", // This model should be available via the StepFun endpoint
      "What is in this image? A single word answer is fine.",
      "image.png",
    );

    let mut inputs = AsyncNodeInputs::new();
    let image_data = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAAAAACPAi4CAAAABGdBTUEAALGPC/xhBQAAAA1JREFUGFdjYGBgYAAAAAUAAYcA/wAAAABJRU5ErkJggg==";
    inputs.insert(
      "image.png".to_string(),
      FlowValue::Json(Value::String(image_data.to_string())),
    );

    let result = node.execute(&inputs).await;
    assert!(result.is_ok(), "Node execution failed: {:?}", result.err());

    let outputs = result.unwrap();
    let output_value = outputs.get("test_vision_output").unwrap();
    if let FlowValue::Json(Value::String(s)) = output_value {
      // The image is black, so the answer should be something like "black" or "nothing".
      println!("Vision model output: {}", s);
      assert!(
        !s.is_empty(),
        "Vision model should have returned a description."
      );
    } else {
      panic!("Output was not a FlowValue::Json(Value::String(...))");
    }
  }
}
