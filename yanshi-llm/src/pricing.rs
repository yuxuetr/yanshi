//! Per-model pricing table: translates token usage, and (Y2.3) images,
//! seconds of generated audio / video and characters of TTS input, into
//! a USD cost estimate. Used by the eval harness, ReAct / PlanExecute
//! cost budgets and the modality nodes' DAG cost accounting.
//!
//! Provider crates don't yet report per-call cost (and pricing drifts
//! independently of code), so the harness keeps a separate side-table.
//! Loadable from a YAML file (`pricing.yml`) or constructed in code
//! for tests.
//!
//! Pricing tier model: per-1k tokens for input + output. This mirrors
//! every public provider price sheet (OpenAI / Anthropic / Google /
//! Moonshot / StepFun all quote per-1k or per-1M, which is trivially
//! per-1k after a factor-of-1000 divide).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// USD price per 1k tokens for one model. `0.0` is a legal value (mock
/// providers, self-hosted models).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
  /// USD per 1k input (prompt) tokens.
  #[serde(default)]
  pub input_per_1k: f64,
  /// USD per 1k output (completion) tokens.
  #[serde(default)]
  pub output_per_1k: f64,
  /// Y2.3: USD per generated or edited image.
  #[serde(default)]
  pub per_image: f64,
  /// Y2.3: USD per second of generated media (TTS audio, video).
  #[serde(default)]
  pub per_output_second: f64,
  /// Y2.3: USD per 1k characters of TTS input text.
  #[serde(default)]
  pub per_1k_input_chars: f64,
}

impl ModelPricing {
  /// Compute the USD cost of one call from prompt + completion token
  /// counts. Token fields are `Option<u32>` because providers may not
  /// report them; missing fields contribute zero to the call's cost
  /// (the alternative would over-report by pretending to know).
  pub fn cost_for_call(self, prompt_tokens: Option<u32>, completion_tokens: Option<u32>) -> f64 {
    let prompt = prompt_tokens.unwrap_or(0) as f64;
    let completion = completion_tokens.unwrap_or(0) as f64;
    (prompt / 1000.0) * self.input_per_1k + (completion / 1000.0) * self.output_per_1k
  }

  /// Y2.3: cost of `images` generated / edited images.
  pub fn cost_for_images(self, images: usize) -> f64 {
    images as f64 * self.per_image
  }

  /// Y2.3: cost of a speech call — input characters plus, when the
  /// vendor reports it, seconds of audio produced.
  pub fn cost_for_speech(self, input_chars: usize, output_seconds: Option<f64>) -> f64 {
    (input_chars as f64 / 1000.0) * self.per_1k_input_chars
      + output_seconds.unwrap_or(0.0) * self.per_output_second
  }

  /// Y2.3: cost of `seconds` of generated video.
  pub fn cost_for_seconds(self, seconds: f64) -> f64 {
    seconds * self.per_output_second
  }
}

/// Errors surfaced by the pricing table loader.
#[derive(Debug, Error)]
pub enum PricingError {
  #[error("I/O error reading pricing table at {path}: {source}")]
  Io {
    path: String,
    #[source]
    source: std::io::Error,
  },

  #[error("invalid pricing table at {path}: {message}")]
  Parse { path: String, message: String },
}

/// Map of `model_id → ModelPricing`. Use [`PricingTable::lookup`] to
/// fetch a price (returns the table's `default` entry when no exact
/// match is found, falling back to zero cost when no default is set).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingTable {
  /// Per-model pricing. Key is the model id as it appears in
  /// `EvalCase::model` / `AgentEvent::LlmCallCompleted::model`.
  #[serde(default)]
  pub models: HashMap<String, ModelPricing>,
  /// Optional fallback when a model id isn't in [`Self::models`].
  /// `None` means "missing models cost $0" — which is the safe default
  /// for the mock provider and self-hosted models.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub default: Option<ModelPricing>,
}

impl PricingTable {
  /// Construct an empty table. Helpful for tests where the eval should
  /// report `cost_usd_actual = 0.0` everywhere.
  pub fn empty() -> Self {
    Self::default()
  }

  /// Load a pricing table from a YAML file. The file shape mirrors
  /// the struct:
  ///
  /// ```yaml
  /// default:
  ///   input_per_1k: 0.0
  ///   output_per_1k: 0.0
  /// models:
  ///   gpt-4o:
  ///     input_per_1k: 5.00
  ///     output_per_1k: 15.00
  /// ```
  pub fn load_from_yaml(path: impl AsRef<Path>) -> Result<Self, PricingError> {
    let p = path.as_ref();
    let body = fs::read_to_string(p).map_err(|e| PricingError::Io {
      path: p.display().to_string(),
      source: e,
    })?;
    let table: PricingTable = serde_yaml::from_str(&body).map_err(|e| PricingError::Parse {
      path: p.display().to_string(),
      message: e.to_string(),
    })?;
    Ok(table)
  }

  /// The operator's table: `YANSHI_PRICING_TABLE` when set, else
  /// `~/.yanshi/pricing.yml` when it exists, else empty (every model
  /// costs $0). A missing file is not an error; a malformed one is, so a
  /// cost limit the operator expected is never silently disabled.
  pub fn load_default() -> Result<Self, PricingError> {
    if let Ok(path) = std::env::var("YANSHI_PRICING_TABLE") {
      return Self::load_from_yaml(path);
    }
    match dirs::home_dir().map(|home| home.join(".yanshi").join("pricing.yml")) {
      Some(path) if path.exists() => Self::load_from_yaml(path),
      _ => Ok(Self::empty()),
    }
  }

  /// [`Self::load_default`], loaded once per process. Nodes price every
  /// call against it; the error is kept as text so every later caller
  /// sees the same failure.
  pub fn shared() -> Result<&'static PricingTable, String> {
    static SHARED: std::sync::OnceLock<Result<PricingTable, String>> = std::sync::OnceLock::new();
    SHARED
      .get_or_init(|| PricingTable::load_default().map_err(|e| e.to_string()))
      .as_ref()
      .map_err(Clone::clone)
  }

  /// Resolve the pricing for a model id, falling back to
  /// [`Self::default`] and finally to a zero-cost row.
  pub fn lookup(&self, model: &str) -> ModelPricing {
    if let Some(pricing) = self.models.get(model) {
      return *pricing;
    }
    self.default.unwrap_or_default()
  }

  /// Convenience builder for in-test fixtures.
  pub fn with_model(mut self, model: impl Into<String>, pricing: ModelPricing) -> Self {
    self.models.insert(model.into(), pricing);
    self
  }

  /// Set the fallback pricing for models not enumerated under
  /// [`Self::models`].
  pub fn with_default(mut self, pricing: ModelPricing) -> Self {
    self.default = Some(pricing);
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn model_pricing_cost_for_call_multiplies_per_1k() {
    let p = ModelPricing {
      input_per_1k: 5.0,
      output_per_1k: 15.0,
      ..Default::default()
    };
    // 1k prompt + 1k completion → $5 + $15 = $20.
    let cost = p.cost_for_call(Some(1000), Some(1000));
    assert!((cost - 20.0).abs() < 1e-9, "cost = {cost}");
  }

  #[test]
  fn model_pricing_cost_for_call_handles_partial_token_counts() {
    let p = ModelPricing {
      input_per_1k: 5.0,
      output_per_1k: 15.0,
      ..Default::default()
    };
    // 500 prompt, no completion → $2.5.
    let cost = p.cost_for_call(Some(500), None);
    assert!((cost - 2.5).abs() < 1e-9, "cost = {cost}");
  }

  #[test]
  fn pricing_table_lookup_falls_back_to_default_then_zero() {
    let table = PricingTable::default()
      .with_model(
        "gpt-4o",
        ModelPricing {
          input_per_1k: 5.0,
          output_per_1k: 15.0,
          ..Default::default()
        },
      )
      .with_default(ModelPricing {
        input_per_1k: 0.001,
        output_per_1k: 0.002,
        ..Default::default()
      });
    assert_eq!(table.lookup("gpt-4o").input_per_1k, 5.0);
    assert_eq!(table.lookup("unknown-model").input_per_1k, 0.001);
    // No default set + no models → zero cost.
    let zero_table = PricingTable::default();
    assert_eq!(zero_table.lookup("anything"), ModelPricing::default());
    assert_eq!(
      zero_table
        .lookup("anything")
        .cost_for_call(Some(1_000_000), Some(1_000_000)),
      0.0
    );
  }

  #[test]
  fn pricing_table_load_from_yaml_round_trip() {
    let yaml = r#"
default:
  input_per_1k: 0.001
  output_per_1k: 0.002
models:
  gpt-4o:
    input_per_1k: 5.00
    output_per_1k: 15.00
  claude-sonnet-4-6:
    input_per_1k: 3.00
    output_per_1k: 15.00
"#;
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), yaml).unwrap();
    let table = PricingTable::load_from_yaml(f.path()).unwrap();
    assert_eq!(table.lookup("gpt-4o").input_per_1k, 5.0);
    assert_eq!(table.lookup("claude-sonnet-4-6").output_per_1k, 15.0);
    assert_eq!(table.lookup("missing").input_per_1k, 0.001);
  }

  #[test]
  fn pricing_table_load_from_yaml_rejects_garbage() {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), "this is: [not valid yaml because:: ::: ").unwrap();
    let err = PricingTable::load_from_yaml(f.path()).unwrap_err();
    assert!(matches!(err, PricingError::Parse { .. }));
  }

  #[test]
  fn modality_prices_multiply_by_units() {
    let p = ModelPricing {
      per_image: 0.04,
      per_output_second: 0.5,
      per_1k_input_chars: 0.015,
      ..Default::default()
    };
    assert!((p.cost_for_images(3) - 0.12).abs() < 1e-9);
    assert!((p.cost_for_speech(2_000, Some(4.0)) - (0.03 + 2.0)).abs() < 1e-9);
    assert!((p.cost_for_speech(2_000, None) - 0.03).abs() < 1e-9);
    assert!((p.cost_for_seconds(8.0) - 4.0).abs() < 1e-9);
  }

  #[test]
  fn modality_fields_load_from_yaml_and_default_to_zero() {
    let table: PricingTable = serde_yaml::from_str(
      "models:\n  gpt-image-1:\n    per_image: 0.04\n  gpt-4o:\n    input_per_1k: 5.0\n",
    )
    .expect("yaml");
    assert_eq!(table.lookup("gpt-image-1").per_image, 0.04);
    assert_eq!(table.lookup("gpt-4o").per_image, 0.0);
  }
}
