//! Y7.1: the price of a modality tool call, estimated *before* it runs so
//! a budget can decide whether it needs approval.

use serde_json::Value;
use yanshi_llm::pricing::{ModelPricing, PricingTable};

/// Seconds of video assumed when a `text_to_video` call names none (the
/// usual vendor default is 5-8 s; the higher end keeps the estimate
/// conservative).
const DEFAULT_VIDEO_SECONDS: f64 = 8.0;
/// Characters of speech per second, to price per-second TTS before the
/// audio exists.
const SPEECH_CHARS_PER_SECOND: f64 = 15.0;

/// The estimated USD price of calling modality tool `tool` with `params`,
/// from `table`. `None` when the tool has no per-call price here (`asr`,
/// `image_understand`), the call names no model, or the model has no row
/// in the table: an unpriced call must not look free to a budget.
pub fn estimate_cost_usd(table: &PricingTable, tool: &str, params: &Value) -> Option<f64> {
  let model = params.get("model").and_then(Value::as_str)?;
  let pricing = table.lookup(model);
  if pricing == ModelPricing::default() {
    return None;
  }
  let count = |key: &str| params.get(key).and_then(Value::as_u64);
  match tool {
    "text_to_image" | "image_to_image" => {
      Some(pricing.cost_for_images(count("n").unwrap_or(1) as usize))
    }
    "image_edit" => Some(pricing.cost_for_images(1)),
    "text_to_video" => Some(
      pricing.cost_for_seconds(
        count("duration_seconds")
          .map(|s| s as f64)
          .unwrap_or(DEFAULT_VIDEO_SECONDS),
      ),
    ),
    "tts" => {
      let chars = params
        .get("input")
        .and_then(Value::as_str)
        .map(|text| text.chars().count())
        .unwrap_or(0);
      Some(pricing.cost_for_speech(chars, Some(chars as f64 / SPEECH_CHARS_PER_SECOND)))
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn table() -> PricingTable {
    PricingTable::empty()
      .with_model(
        "img",
        ModelPricing {
          per_image: 0.04,
          ..ModelPricing::default()
        },
      )
      .with_model(
        "vid",
        ModelPricing {
          per_output_second: 0.10,
          ..ModelPricing::default()
        },
      )
      .with_model(
        "voice",
        ModelPricing {
          per_1k_input_chars: 0.015,
          ..ModelPricing::default()
        },
      )
  }

  #[test]
  fn priced_calls_are_estimated_from_their_parameters() {
    let t = table();
    let near = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-9);
    assert!(near(
      estimate_cost_usd(&t, "text_to_image", &json!({"model": "img", "n": 3})),
      0.12
    ));
    assert!(near(
      estimate_cost_usd(&t, "text_to_image", &json!({"model": "img"})),
      0.04
    ));
    assert!(near(
      estimate_cost_usd(
        &t,
        "text_to_video",
        &json!({"model": "vid", "duration_seconds": 5})
      ),
      0.5
    ));
    assert!(near(
      estimate_cost_usd(&t, "text_to_video", &json!({"model": "vid"})),
      0.8
    ));
    assert!(near(
      estimate_cost_usd(
        &t,
        "tts",
        &json!({"model": "voice", "input": "x".repeat(2000)})
      ),
      0.03
    ));
  }

  #[test]
  fn unpriced_calls_have_no_estimate() {
    let t = table();
    assert_eq!(
      estimate_cost_usd(&t, "text_to_image", &json!({"model": "unknown"})),
      None
    );
    assert_eq!(
      estimate_cost_usd(&t, "text_to_image", &json!({"prompt": "no model"})),
      None
    );
    assert_eq!(
      estimate_cost_usd(&t, "asr", &json!({"model": "voice"})),
      None
    );
    assert_eq!(
      estimate_cost_usd(&t, "file", &json!({"model": "img"})),
      None
    );
  }
}
