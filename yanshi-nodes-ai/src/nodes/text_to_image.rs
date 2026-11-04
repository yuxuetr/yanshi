use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::{Yanshi, providers::modality::Text2ImageRequest as ModalityText2ImageRequest};

use crate::artifacts::{self, ArtifactOutputFormat};

// ... (rest of the file is the same until the AsyncNode implementation)

/// Text-to-Image generation node
#[derive(Debug, Clone)]
pub struct TextToImageNode {
  pub name: String,
  pub model: String,
  pub prompt_template: String,
  pub negative_prompt: Option<String>,
  pub input_keys: Vec<String>,
  pub output_key: String,

  // Image generation specific parameters
  pub size: Option<String>, // "256x256", "512x512", "768x768", "1024x1024"
  pub response_format: ImageResponseFormat, // b64_json or url
  pub steps: Option<u32>,   // Generation steps
  pub cfg_scale: Option<f32>, // Classifier-free guidance scale
  pub style_reference: Option<StyleReference>,
  pub n: Option<u32>,    // Number of images to generate (default 1)
  pub seed: Option<u64>, // For reproducible generation

  // Workflow control
  pub dependencies: Vec<String>,
  pub condition: Option<String>,
  pub timeout_ms: Option<u64>,
  /// Y1.1: see [`ArtifactOutputFormat`]. Orthogonal to `response_format`,
  /// which is what we *ask the vendor for*; this is what the node emits.
  pub output_format: ArtifactOutputFormat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ImageResponseFormat {
  #[serde(rename = "b64_json")]
  #[default]
  Base64Json, // Return as base64 encoded JSON
  #[serde(rename = "url")]
  Url, // Return as URL reference
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleReference {
  pub image_url: Option<String>,  // Reference image URL
  pub style_weight: Option<f32>,  // Style influence weight (0.0-1.0)
  pub style_name: Option<String>, // Named style preset
}

impl TextToImageNode {
  pub fn new(name: &str, model: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      prompt_template: String::new(),
      negative_prompt: None,
      input_keys: Vec::new(),
      output_key: format!("{}_image", name),
      size: None,
      response_format: ImageResponseFormat::default(),
      steps: None,
      cfg_scale: None,
      style_reference: None,
      n: None,
      seed: None,
      dependencies: Vec::new(),
      condition: None,
      timeout_ms: None,
      output_format: ArtifactOutputFormat::default(),
    }
  }

  pub fn with_prompt(mut self, template: &str) -> Self {
    self.prompt_template = template.to_string();
    self
  }

  pub fn with_output_format(mut self, format: ArtifactOutputFormat) -> Self {
    self.output_format = format;
    self
  }

  pub fn with_negative_prompt(mut self, negative: &str) -> Self {
    self.negative_prompt = Some(negative.to_string());
    self
  }

  pub fn with_size(mut self, size: &str) -> Self {
    self.size = Some(size.to_string());
    self
  }

  pub fn with_response_format(mut self, format: ImageResponseFormat) -> Self {
    self.response_format = format;
    self
  }

  pub fn with_steps(mut self, steps: u32) -> Self {
    self.steps = Some(steps);
    self
  }

  pub fn with_cfg_scale(mut self, scale: f32) -> Self {
    self.cfg_scale = Some(scale);
    self
  }

  pub fn with_style_reference(mut self, style: StyleReference) -> Self {
    self.style_reference = Some(style);
    self
  }

  pub fn with_count(mut self, n: u32) -> Self {
    self.n = Some(n);
    self
  }

  pub fn with_seed(mut self, seed: u64) -> Self {
    self.seed = Some(seed);
    self
  }

  pub fn with_input_keys(mut self, keys: Vec<String>) -> Self {
    self.input_keys = keys;
    self
  }

  pub fn with_output_key(mut self, key: &str) -> Self {
    self.output_key = key.to_string();
    self
  }

  pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
    self.timeout_ms = Some(timeout_ms);
    self
  }

  /// Resolve template variables in the prompt using inputs
  fn resolve_prompt(&self, inputs: &AsyncNodeInputs) -> Result<String, YanshiError> {
    let mut resolved = self.prompt_template.clone();
    for (key, value) in inputs {
      let placeholder = format!("{{{{{}}}}}", key);
      if resolved.contains(&placeholder)
        && let FlowValue::Json(Value::String(s)) = value
      {
        resolved = resolved.replace(&placeholder, s);
      }
    }
    Ok(resolved)
  }

  /// Create configuration for image generation.
  ///
  /// Returns the raw `serde_json::Map` rather than `Value::Object(...)` so
  /// the caller can pass `&Map<...>` to `execute_real_image_generation`
  /// without an `as_object().unwrap()` round-trip (Q5.1).
  fn create_image_config(
    &self,
    resolved_prompt: &str,
    inputs: &AsyncNodeInputs,
  ) -> Result<serde_json::Map<String, Value>, YanshiError> {
    let mut config = serde_json::Map::new();

    config.insert("model".to_string(), Value::String(self.model.clone()));
    config.insert(
      "prompt".to_string(),
      Value::String(resolved_prompt.to_string()),
    );

    if let Some(ref neg_prompt) = self.negative_prompt {
      let mut resolved_negative = neg_prompt.clone();
      for (key, value) in inputs {
        let placeholder = format!("{{{{{}}}}}", key);
        if resolved_negative.contains(&placeholder)
          && let FlowValue::Json(Value::String(s)) = value
        {
          resolved_negative = resolved_negative.replace(&placeholder, s);
        }
      }
      config.insert(
        "negative_prompt".to_string(),
        Value::String(resolved_negative),
      );
    }

    if let Some(ref size) = self.size {
      config.insert("size".to_string(), Value::String(size.clone()));
    }

    // `ImageResponseFormat` is a renamed unit-variant enum — serialization
    // cannot fail. Mapping it through `serde_json::to_value` was Q5.1
    // unwrap-bait; use the rename literals directly.
    let response_format_str = match self.response_format {
      ImageResponseFormat::Base64Json => "b64_json",
      ImageResponseFormat::Url => "url",
    };
    config.insert(
      "response_format".to_string(),
      Value::String(response_format_str.to_string()),
    );

    if let Some(steps) = self.steps {
      config.insert(
        "steps".to_string(),
        Value::Number(serde_json::Number::from(steps)),
      );
    }

    if let Some(cfg_scale) = self.cfg_scale {
      let number = serde_json::Number::from_f64(cfg_scale as f64).ok_or_else(|| {
        YanshiError::NodeInputError {
          message: format!("Invalid value for cfg_scale: {}", cfg_scale),
        }
      })?;
      config.insert("cfg_scale".to_string(), Value::Number(number));
    }

    if let Some(ref style_ref) = self.style_reference {
      // `serde_json::to_value` only fails on non-finite f32 (NaN/Inf) inside
      // `style_weight`; surface as a validation error rather than panicking
      // (Q5.1).
      let style_value =
        serde_json::to_value(style_ref).map_err(|err| YanshiError::NodeInputError {
          message: format!(
            "TextToImage node '{}': style_reference contains a non-finite \
             style_weight value: {err}",
            self.name
          ),
        })?;
      config.insert("style_reference".to_string(), style_value);
    }

    if let Some(n) = self.n {
      config.insert("n".to_string(), Value::Number(serde_json::Number::from(n)));
    }

    if let Some(seed) = self.seed {
      config.insert(
        "seed".to_string(),
        Value::Number(serde_json::Number::from(seed)),
      );
    }

    Ok(config)
  }

  /// Execute real image generation through the modality dispatcher.
  ///
  /// Post-P-LLM.3: vendor selection comes from the model registry, not
  /// a hardcoded StepFun client. `self.style_reference` is currently
  /// dropped because it's StepFun-specific and the cross-vendor
  /// `Text2ImageRequest` trait surface doesn't carry it; if a future
  /// trait extension adds vendor extras (`extra: Map<String, Value>`),
  /// the conversion below regains it.
  async fn execute_real_image_generation(
    &self,
    config: &serde_json::Map<String, Value>,
  ) -> Result<FlowValue, YanshiError> {
    let prompt = config
      .get("prompt")
      .and_then(|v| v.as_str())
      .unwrap_or("")
      .to_string();
    let model = config
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or(self.model.as_str())
      .to_string();
    let size = config
      .get("size")
      .and_then(|s| s.as_str())
      .unwrap_or("1024x1024");

    tracing::debug!(
      model = %model,
      prompt = %prompt,
      size = %size,
      "executing text-to-image request via modality dispatcher"
    );

    let provider =
      Yanshi::text2image_for(&model)
        .await
        .map_err(|e| YanshiError::ConfigurationError {
          message: format!(
            "Failed to resolve text-to-image provider for '{}': {}",
            model, e
          ),
        })?;

    let response_format = match &self.response_format {
      ImageResponseFormat::Base64Json => "b64_json",
      ImageResponseFormat::Url => "url",
    };

    let request = ModalityText2ImageRequest {
      model: model.clone(),
      prompt,
      size: Some(size.to_string()),
      n: self.n,
      response_format: Some(response_format.to_string()),
      seed: self.seed.map(|s| s as i32),
      steps: self.steps,
      cfg_scale: self.cfg_scale,
    };

    let image_response =
      provider
        .generate(request)
        .await
        .map_err(|e| YanshiError::AsyncExecutionError {
          message: format!("Text-to-image generation failed: {}", e),
        })?;

    let first_image =
      image_response
        .images
        .first()
        .ok_or_else(|| YanshiError::AsyncExecutionError {
          message: "No images returned from text-to-image provider".to_string(),
        })?;

    let result = artifacts::image_output(&self.name, first_image, self.output_format).await?;

    tracing::debug!(
      provider = %provider.name(),
      size = %size,
      format = %response_format,
      "image generation complete"
    );
    Ok(result)
  }

  // Q1.3.3: the `execute_mock_image_generation` fallback was removed
  // because callers were silently receiving a 1x1 placeholder when the
  // upstream API failed. Workflows that genuinely need a stand-in
  // should use `MockNode` or a conditional branch — the image
  // generation path must fail loudly so operators can decide what to
  // do with the error.
}

#[async_trait]
impl AsyncNode for TextToImageNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    if let Some(ref condition) = self.condition
      && let Some(FlowValue::Json(Value::String(cond))) = inputs.get(condition)
      && cond != "true"
    {
      tracing::debug!(
        name = %self.name,
        condition = %cond,
        "skipping TextToImage node due to condition"
      );
      return Ok(HashMap::new());
    }

    let enriched_prompt = self.resolve_prompt(inputs)?;
    let config = self.create_image_config(&enriched_prompt, inputs)?;

    tracing::debug!(
      name = %self.name,
      model = %self.model,
      prompt = %enriched_prompt,
      size = ?self.size,
      "TextToImage node prepared"
    );

    // Q1.3.3: previously, an upstream API failure silently fell back to a
    // 1x1 mock PNG so the workflow saw `Ok(...)` carrying a placeholder
    // image. Downstream nodes (and operators) had no way to distinguish
    // a real generation from a stand-in. The mock path is removed —
    // upstream failures now surface as `YanshiError::AsyncExecutionError`
    // with the underlying provider message intact. If a workflow author
    // truly wants a placeholder, they should wire an explicit `MockNode`
    // or use a conditional/fallback branch in the DAG.
    let real_call = self.execute_real_image_generation(&config);
    let response = if let Some(timeout_ms) = self.timeout_ms {
      let timeout_duration = std::time::Duration::from_millis(timeout_ms);
      match tokio::time::timeout(timeout_duration, real_call).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
          return Err(YanshiError::AsyncExecutionError {
            message: format!(
              "TextToImage node '{}': image generation failed: {err}",
              self.name
            ),
          });
        }
        Err(_) => {
          return Err(YanshiError::TimeoutExceeded {
            duration_ms: timeout_ms,
          });
        }
      }
    } else {
      real_call
        .await
        .map_err(|err| YanshiError::AsyncExecutionError {
          message: format!(
            "TextToImage node '{}': image generation failed: {err}",
            self.name
          ),
        })?
    };

    let mut outputs = HashMap::new();
    outputs.insert(self.output_key.clone(), response);

    Ok(outputs)
  }
}

/// Helper constructors for common text-to-image scenarios
impl TextToImageNode {
  /// Create a high-quality artistic image generator
  pub fn artistic_generator(name: &str, model: &str) -> Self {
    Self::new(name, model)
      .with_size("1024x1024")
      .with_steps(50)
      .with_cfg_scale(7.5)
      .with_response_format(ImageResponseFormat::Base64Json)
  }

  /// Create a fast prototype image generator
  pub fn quick_generator(name: &str, model: &str) -> Self {
    Self::new(name, model)
      .with_size("512x512")
      .with_steps(20)
      .with_cfg_scale(5.0)
      .with_response_format(ImageResponseFormat::Url)
  }

  /// Create a batch image generator
  pub fn batch_generator(name: &str, model: &str, count: u32) -> Self {
    Self::new(name, model)
      .with_size("768x768")
      .with_count(count)
      .with_steps(30)
      .with_response_format(ImageResponseFormat::Base64Json)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Q1.3.3: without a configured API key, the real image generation
  /// path must surface a real error — no more silent 1x1 PNG fallback.
  /// We can't easily exercise the success path in CI (no API key), so
  /// the unit test asserts the *failure mode*: callers see an error
  /// instead of a placeholder image.
  #[tokio::test]
  async fn execute_propagates_upstream_failure_instead_of_returning_mock() {
    // Force a guaranteed failure by pointing at an invalid model / no
    // env key. With the mock fallback removed, the node must return Err
    // rather than `Ok(data:image/png;base64,...)`.
    let node = TextToImageNode::new("test_gen", "definitely-not-a-real-model")
      .with_prompt("A beautiful sunset over mountains")
      .with_size("512x512")
      .with_timeout(500);

    let inputs = AsyncNodeInputs::new();
    let result = node.execute(&inputs).await;
    assert!(
      result.is_err(),
      "upstream failure must propagate; got Ok({:?})",
      result.ok()
    );
  }
}
