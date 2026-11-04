//! TTS Node — converts text to speech via the modality dispatcher.
//!
//! Post-P-LLM.3 this node no longer talks to StepFun directly. It picks
//! the vendor via the registry by model name: `Yanshi::tts(&self.model)`
//! returns a boxed [`yanshi_llm::TtsProvider`] which routes to the
//! right vendor implementation (today StepFun; P-LLM.5 will add others).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::{TtsRequest, Yanshi};

use crate::artifacts::{self, ArtifactOutputFormat};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum AudioResponseFormat {
  Wav,
  #[default]
  Mp3,
  Flac,
  Opus,
}

impl AudioResponseFormat {
  fn as_wire_str(&self) -> &'static str {
    match self {
      AudioResponseFormat::Wav => "wav",
      AudioResponseFormat::Mp3 => "mp3",
      AudioResponseFormat::Flac => "flac",
      AudioResponseFormat::Opus => "opus",
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TTSNode {
  pub name: String,
  pub model: String,
  pub voice: String,
  pub input_template: String,
  pub response_format: AudioResponseFormat,
  pub speed: Option<f32>,
  pub output_key: String,
  pub input_keys: Vec<String>,
  /// Y1.1: `file` (default) writes the audio under the run's artifact
  /// root and returns `FlowValue::File`; `data_uri` keeps the pre-Y1.1
  /// base64 string.
  #[serde(default)]
  pub output_format: ArtifactOutputFormat,
}

impl TTSNode {
  pub fn new(name: &str, model: &str, voice: &str, input_template: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      voice: voice.to_string(),
      input_template: input_template.to_string(),
      response_format: Default::default(),
      speed: None,
      output_key: format!("{}_output", name),
      input_keys: vec![],
      output_format: ArtifactOutputFormat::default(),
    }
  }

  pub fn with_output_format(mut self, format: ArtifactOutputFormat) -> Self {
    self.output_format = format;
    self
  }

  fn flow_value_to_string(value: &FlowValue) -> String {
    match value {
      FlowValue::Json(Value::String(s)) => s.clone(),
      FlowValue::Json(v) => v.to_string().trim_matches('"').to_string(),
      FlowValue::File { path, .. } => path.to_string_lossy().to_string(),
      FlowValue::Url { url, .. } => url.clone(),
    }
  }
}

#[async_trait]
impl AsyncNode for TTSNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    tracing::debug!(name = %self.name, "executing TTSNode");

    Yanshi::ensure_init()
      .await
      .map_err(|e| YanshiError::ConfigurationError {
        message: format!("Failed to initialize Yanshi LLM service: {}", e),
      })?;

    // Y4.3: `model`, `voice` and `input_template` given as inputs (e.g.
    // `input_mapping` from a Map `item`) override the parameters, so one
    // node can speak every line of a two-voice script.
    let string_input = |key: &str| match inputs.get(key) {
      Some(FlowValue::Json(Value::String(s))) if !s.is_empty() => Some(s.clone()),
      _ => None,
    };
    let model = string_input("model").unwrap_or_else(|| self.model.clone());
    let voice = string_input("voice").unwrap_or_else(|| self.voice.clone());
    let mut resolved_input =
      string_input("input_template").unwrap_or_else(|| self.input_template.clone());
    for key in &self.input_keys {
      if let Some(value) = inputs.get(key) {
        let placeholder = format!("{{{{{}}}}}", key);
        resolved_input = resolved_input.replace(&placeholder, &Self::flow_value_to_string(value));
      }
    }

    // P-LLM.3: route through the modality dispatcher. The registry
    // entry for `self.model` decides which vendor handles the call
    // (StepFun, OpenAI, Google, DashScope, MiniMax or the mock).
    let provider = Yanshi::tts(&model)
      .await
      .map_err(|e| YanshiError::ConfigurationError {
        message: format!(
          "Failed to resolve TTS provider for model '{}': {}",
          model, e
        ),
      })?;

    let response_format = self.response_format.as_wire_str().to_string();
    let input_chars = resolved_input.chars().count();
    let request = TtsRequest {
      model: model.clone(),
      input: resolved_input,
      voice,
      response_format: Some(response_format),
      speed: self.speed,
      volume: None,
      sample_rate: None,
    };

    tracing::debug!(provider = %provider.name(), "synthesizing speech via provider");
    let tts_response = provider
      .synthesize(request)
      .await
      .map_err(|e| crate::vendor_error::vendor_error("TTS synthesis failed", e))?;
    let seconds = crate::cost::wav_duration_seconds(&tts_response.audio);
    crate::cost::record(&model, |p| p.cost_for_speech(input_chars, seconds))?;

    let value = match self.output_format {
      ArtifactOutputFormat::File => {
        artifacts::write_artifact(&self.name, &tts_response.audio, &tts_response.mime_type).await?
      }
      ArtifactOutputFormat::DataUri => {
        artifacts::data_uri_value(&tts_response.audio, &tts_response.mime_type)
      }
    };

    tracing::debug!("TTSNode execution successful");
    let mut outputs = HashMap::new();
    outputs.insert(self.output_key.clone(), value);

    Ok(outputs)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  #[ignore] // Requires STEPFUN_API_KEY environment variable
  async fn test_tts_node_integration() {
    // This test requires a valid STEP_API_KEY to be set in the environment.
    // R4.3 (2026-07-28 audit): this crate was never in the CI test matrix
    // before, so a missing skip guard here (unlike its asr/image_to_image/
    // image_understand siblings) went unnoticed until R0.3 added it.
    if std::env::var("STEP_API_KEY").is_err() {
      println!("Skipping TTS integration test: STEP_API_KEY not set.");
      return;
    }

    let node = TTSNode {
      name: "test_tts".to_string(),
      model: "step-tts-mini".to_string(),
      voice: "cixingnansheng".to_string(),
      input_template: "Hello world!".to_string(),
      response_format: AudioResponseFormat::Mp3,
      speed: Some(1.0),
      output_key: "audio_output".to_string(),
      input_keys: vec![],
      output_format: ArtifactOutputFormat::DataUri,
    };

    let inputs = AsyncNodeInputs::new();

    let result = node.execute(&inputs).await;
    assert!(result.is_ok(), "Node execution failed: {:?}", result.err());

    let outputs = result.unwrap();
    let output_value = outputs.get("audio_output").unwrap();
    if let FlowValue::Json(Value::String(data)) = output_value {
      assert!(data.starts_with("data:audio/mpeg;base64,"));
      let base64_part = data.split(",").nth(1).unwrap();
      assert!(!base64_part.is_empty(), "Base64 data should not be empty");
    } else {
      panic!("Output was not a FlowValue::Json(Value::String(...))");
    }
  }
}
