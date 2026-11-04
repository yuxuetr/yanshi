//! Y2.3: price a billed modality call against the operator's pricing
//! table (`yanshi_llm::pricing::PricingTable::shared`) and record it on
//! the running node's cost meter, where the executor enforces the run's
//! `cost_limit_usd`. Outside a flow the record is a no-op.

use yanshi_core::{error::YanshiError, run_context};
use yanshi_llm::pricing::{ModelPricing, PricingTable};

/// Look up `model`'s prices, let `price` turn them into USD, and record
/// the amount. A malformed pricing file fails the node: a cost limit the
/// operator configured must not be silently unenforced.
pub(crate) fn record(
  model: &str,
  price: impl FnOnce(ModelPricing) -> f64,
) -> Result<f64, YanshiError> {
  let table = PricingTable::shared().map_err(|message| YanshiError::ConfigurationError {
    message: format!("pricing table: {message}"),
  })?;
  let usd = price(table.lookup(model));
  run_context::record_cost_usd(usd);
  Ok(usd)
}

/// Seconds of audio in a PCM WAV (RIFF) buffer, from the header's byte
/// rate; `None` for anything else (mp3 / opus durations are not derived).
pub(crate) fn wav_duration_seconds(audio: &[u8]) -> Option<f64> {
  if audio.len() < 44 || &audio[0..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
    return None;
  }
  let byte_rate = u32::from_le_bytes([audio[28], audio[29], audio[30], audio[31]]);
  if byte_rate == 0 {
    return None;
  }
  Some((audio.len() - 44) as f64 / f64::from(byte_rate))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn wav_duration_reads_the_byte_rate() {
    let mut wav = vec![0u8; 44 + 32_000];
    wav[0..4].copy_from_slice(b"RIFF");
    wav[8..12].copy_from_slice(b"WAVE");
    wav[28..32].copy_from_slice(&16_000u32.to_le_bytes());
    assert_eq!(wav_duration_seconds(&wav), Some(2.0));
    assert_eq!(wav_duration_seconds(b"ID3not a wav"), None);
  }
}
