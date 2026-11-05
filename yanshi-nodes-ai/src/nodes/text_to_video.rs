use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::{Yanshi, providers::modality::Text2VideoRequest as ModalityText2VideoRequest};

use crate::artifacts::{self, ArtifactOutputFormat};

/// Default poll interval while waiting on a submitted video job.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Default max wait, used when the node has no `timeout_ms` configured.
/// Veo's own docs cite up to ~6 minutes of peak-usage latency.
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(360);

/// Text-to-Video generation node.
///
/// Unlike [`super::text_to_image::TextToImageNode`], the underlying
/// provider call is an async job API (submit + poll) that can take
/// minutes rather than seconds — see
/// [`yanshi_llm::Text2VideoProvider::generate_and_wait`].
#[derive(Debug, Clone)]
pub struct TextToVideoNode {
  pub name: String,
  pub model: String,
  pub prompt_template: String,
  pub input_keys: Vec<String>,
  pub output_key: String,

  // Video generation specific parameters
  pub aspect_ratio: Option<String>,
  pub duration_seconds: Option<u32>,
  pub resolution: Option<String>,
  pub seed: Option<i64>,

  // Workflow control
  pub dependencies: Vec<String>,
  pub condition: Option<String>,
  /// Doubles as the provider's `max_wait` — see
  /// `execute_real_video_generation`.
  pub timeout_ms: Option<u64>,
  /// Y1.1: see [`ArtifactOutputFormat`]. Under `file` the vendor's
  /// time-limited URL is fetched (through the provider, which knows what
  /// credentials the URL needs) and persisted; under `data_uri` the URL
  /// string is passed through as before.
  pub output_format: ArtifactOutputFormat,
  /// Cap on the fetched video size under `file`.
  pub max_download_bytes: u64,
}

/// What one call asks the vendor for, after input overrides.
struct Shot {
  model: String,
  aspect_ratio: Option<String>,
  duration_seconds: Option<u32>,
}

impl TextToVideoNode {
  pub fn new(name: &str, model: &str) -> Self {
    Self {
      name: name.to_string(),
      model: model.to_string(),
      prompt_template: String::new(),
      input_keys: Vec::new(),
      output_key: format!("{}_video", name),
      aspect_ratio: None,
      duration_seconds: None,
      resolution: None,
      seed: None,
      dependencies: Vec::new(),
      condition: None,
      timeout_ms: None,
      output_format: ArtifactOutputFormat::default(),
      max_download_bytes: artifacts::MAX_VIDEO_DOWNLOAD_BYTES,
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

  pub fn with_max_download_bytes(mut self, max_bytes: u64) -> Self {
    self.max_download_bytes = max_bytes;
    self
  }

  pub fn with_aspect_ratio(mut self, aspect_ratio: &str) -> Self {
    self.aspect_ratio = Some(aspect_ratio.to_string());
    self
  }

  pub fn with_duration_seconds(mut self, duration_seconds: u32) -> Self {
    self.duration_seconds = Some(duration_seconds);
    self
  }

  pub fn with_resolution(mut self, resolution: &str) -> Self {
    self.resolution = Some(resolution.to_string());
    self
  }

  pub fn with_seed(mut self, seed: i64) -> Self {
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

  /// Max total wait for the submitted job to complete (submit + poll
  /// loop), not a single HTTP request timeout.
  pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
    self.timeout_ms = Some(timeout_ms);
    self
  }

  /// Resolve template variables in the prompt using inputs. Mirrors
  /// `TextToImageNode::resolve_prompt` exactly.
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

  /// Y8.3: `model` / `aspect_ratio` / `duration_seconds` may also arrive
  /// as inputs (a Map item picks them per scene), overriding the
  /// build-time parameters, as `text_to_image` does for `model` / `size`.
  fn shot(&self, inputs: &AsyncNodeInputs) -> Shot {
    let string_input = |key: &str| match inputs.get(key) {
      Some(FlowValue::Json(Value::String(s))) if !s.trim().is_empty() => Some(s.clone()),
      _ => None,
    };
    let seconds_input = match inputs.get("duration_seconds") {
      Some(FlowValue::Json(Value::Number(n))) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
      // `--input duration_seconds 5` on the CLI arrives as a string.
      Some(FlowValue::Json(Value::String(s))) => s.trim().parse().ok(),
      _ => None,
    };
    Shot {
      model: string_input("model").unwrap_or_else(|| self.model.clone()),
      aspect_ratio: string_input("aspect_ratio").or_else(|| self.aspect_ratio.clone()),
      duration_seconds: seconds_input.or(self.duration_seconds),
    }
  }

  /// Execute real video generation through the modality dispatcher,
  /// submitting the job and polling until it completes.
  async fn execute_real_video_generation(
    &self,
    prompt: &str,
    shot: Shot,
  ) -> Result<FlowValue, YanshiError> {
    let Shot {
      model,
      aspect_ratio,
      duration_seconds,
    } = shot;
    tracing::debug!(
      model = %model,
      prompt = %prompt,
      "executing text-to-video request via modality dispatcher"
    );

    let provider =
      Yanshi::text2video_for(&model)
        .await
        .map_err(|e| YanshiError::ConfigurationError {
          message: format!(
            "Failed to resolve text-to-video provider for '{}': {}",
            model, e
          ),
        })?;

    let request = ModalityText2VideoRequest {
      model: model.clone(),
      prompt: prompt.to_string(),
      aspect_ratio,
      duration_seconds,
      resolution: self.resolution.clone(),
      seed: self.seed,
      extra: None,
    };

    let max_wait = self
      .timeout_ms
      .map(Duration::from_millis)
      .unwrap_or(DEFAULT_MAX_WAIT);

    let video_response = provider
      .generate_and_wait(request, DEFAULT_POLL_INTERVAL, max_wait)
      .await
      .map_err(|e| crate::vendor_error::vendor_error("Text-to-video generation failed", e))?;

    let first_video =
      video_response
        .videos
        .first()
        .ok_or_else(|| YanshiError::AsyncExecutionError {
          message: "No videos returned from text-to-video provider".to_string(),
        })?;
    // Billed per second of video produced; fall back to the requested
    // length when the vendor does not report one.
    let seconds: f64 = video_response
      .videos
      .iter()
      .filter_map(|video| video.duration_seconds.or(duration_seconds))
      .map(f64::from)
      .sum();
    crate::cost::record(&model, |p| p.cost_for_seconds(seconds))?;

    let result = match self.output_format {
      ArtifactOutputFormat::File => {
        let bytes = provider
          .fetch_video_bytes(first_video, self.max_download_bytes)
          .await
          .map_err(|e| crate::vendor_error::vendor_error("Failed to fetch generated video", e))?;
        artifacts::write_artifact(&self.name, &bytes, "video/mp4").await?
      }
      ArtifactOutputFormat::DataUri => match (&first_video.b64_data, &first_video.url) {
        (Some(b64), _) => FlowValue::Json(Value::String(format!("data:video/mp4;base64,{b64}"))),
        (None, Some(url)) => FlowValue::Json(Value::String(url.clone())),
        (None, None) => {
          return Err(YanshiError::AsyncExecutionError {
            message: "No video URL returned from provider".to_string(),
          });
        }
      },
    };

    tracing::debug!(
      provider = %provider.name(),
      "video generation complete"
    );
    Ok(result)
  }
}

#[async_trait]
impl AsyncNode for TextToVideoNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    if let Some(ref condition) = self.condition
      && let Some(FlowValue::Json(Value::String(cond))) = inputs.get(condition)
      && cond != "true"
    {
      tracing::debug!(
        name = %self.name,
        condition = %cond,
        "skipping TextToVideo node due to condition"
      );
      return Ok(HashMap::new());
    }

    let enriched_prompt = self.resolve_prompt(inputs)?;
    let shot = self.shot(inputs);

    tracing::debug!(
      name = %self.name,
      model = %shot.model,
      prompt = %enriched_prompt,
      "TextToVideo node prepared"
    );

    // Upstream failure surfaces as a real error — no mock/placeholder
    // fallback, matching `TextToImageNode`'s Q1.3.3 precedent.
    // Y2.5: passed through unchanged; the variant tells retry whether
    // repeating the billed call can help.
    let response = self
      .execute_real_video_generation(&enriched_prompt, shot)
      .await?;

    let mut outputs = HashMap::new();
    outputs.insert(self.output_key.clone(), response);

    Ok(outputs)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn inputs_override_model_aspect_ratio_and_duration() {
    let node = TextToVideoNode::new("clip", "built-in")
      .with_aspect_ratio("1:1")
      .with_duration_seconds(10);
    let inputs = AsyncNodeInputs::from([
      (
        "model".to_string(),
        FlowValue::Json(Value::from("per-scene")),
      ),
      (
        "duration_seconds".to_string(),
        FlowValue::Json(Value::from(5)),
      ),
    ]);
    let shot = node.shot(&inputs);
    assert_eq!(shot.model, "per-scene");
    assert_eq!(shot.duration_seconds, Some(5));
    assert_eq!(shot.aspect_ratio.as_deref(), Some("1:1"));
    let defaults = node.shot(&AsyncNodeInputs::new());
    assert_eq!(defaults.model, "built-in");
    assert_eq!(defaults.duration_seconds, Some(10));
  }

  /// Without a configured API key, the real video generation path must
  /// surface a real error — no silent placeholder, mirroring
  /// `TextToImageNode`'s equivalent test.
  #[tokio::test]
  async fn execute_propagates_upstream_failure_instead_of_returning_mock() {
    let node = TextToVideoNode::new("test_gen", "definitely-not-a-real-model")
      .with_prompt("A cat playing piano")
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
