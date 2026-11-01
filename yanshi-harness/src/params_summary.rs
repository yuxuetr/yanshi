//! Shared redaction + size-capping for the `params_summary` field that
//! rides on `ToolCallRequested` / `ApprovalRequested` events.
//!
//! Phase 0 (RFC_HARNESS_LOOP_OWNERSHIP §4): the `ToolCallRequestedPayload`
//! and `ApprovalRequest` wire types document `params_summary` as
//! "redacted/**truncated**", but pre-Phase-0 only redaction happened — a
//! multi-megabyte `file:write` body went whole into the JSONL / SSE log.
//! Both the runtime's post-step translation
//! ([`crate::runtime`]) and the hook layer's approval path
//! ([`crate::hooks_runtime`]) now funnel through `redact_and_cap` so the
//! contract holds uniformly and the redaction call is not duplicated.

use yanshi_tracing::redaction::{RedactionConfig, redact_value};

/// Maximum serialized size (bytes) of a `params_summary` embedded in an
/// event. Above this the value is replaced with a bounded preview so an
/// operator still sees *what kind* of call was attempted without the log
/// line ballooning. 4 KiB comfortably fits a shell command line, an HTTP
/// header set, or a small JSON body while capping pathological inputs.
pub const DEFAULT_PARAMS_SUMMARY_CAP_BYTES: usize = 4 * 1024;

/// Redact secrets, then cap the serialized size of a tool parameter
/// summary before it is embedded in a [`crate::HarnessEvent`].
///
/// Redaction always runs first so the bounded preview produced on
/// overflow can never re-expose a secret the cap would otherwise have
/// truncated past.
pub(crate) fn redact_and_cap(mut value: serde_json::Value) -> serde_json::Value {
  redact_value(&mut value, &RedactionConfig::default());
  cap_value(value, DEFAULT_PARAMS_SUMMARY_CAP_BYTES)
}

/// Redact secrets in place without `redact_and_cap`'s size cap.
///
/// [`yanshi_agent_spi::harness::hooks::PendingToolCall::params`] is
/// documented "the runtime MUST redact secrets before constructing this
/// struct" (W1.7 / D4) — pre-fix the runtime copied the raw, unredacted
/// params straight into that field, so a third-party `PreToolHook` (which
/// may want structured field access, e.g. a risk classifier reading
/// `params["command"]`) saw whatever the tool call actually carried,
/// contract or not. `redact_and_cap`'s cap collapses an oversized value
/// into an opaque `{"_truncated": true, "preview": ...}` object, which
/// would corrupt that structured access for any hook inspecting a
/// specific field on a large-but-otherwise-ordinary payload; only the
/// human/UI-facing `params_summary` on the approval request needs that
/// cap, so `PendingToolCall.params` gets redaction only.
pub(crate) fn redact_only(mut value: serde_json::Value) -> serde_json::Value {
  redact_value(&mut value, &RedactionConfig::default());
  value
}

fn cap_value(value: serde_json::Value, cap: usize) -> serde_json::Value {
  match serde_json::to_string(&value) {
    Ok(serialized) if serialized.len() <= cap => value,
    Ok(serialized) => {
      // Take a char-bounded preview so we never split a UTF-8 boundary.
      let preview: String = serialized.chars().take(cap).collect();
      serde_json::json!({
        "_truncated": true,
        "_original_bytes": serialized.len(),
        "preview": preview,
      })
    }
    // A value that fails to serialize cannot be embedded safely; emit a
    // marker instead of propagating the error (this is observability, not
    // execution — it must never break the tool call).
    Err(_) => serde_json::json!({ "_truncated": true, "_error": "unserializable" }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn redacts_then_passes_small_values_through() {
    let value = serde_json::json!({
      "url": "https://api.example.com",
      "api_key": "sk-live-secret",
    });
    let out = redact_and_cap(value);
    let rendered = serde_json::to_string(&out).unwrap();
    assert!(
      !rendered.contains("sk-live-secret"),
      "secret leaked: {rendered}"
    );
    assert!(rendered.contains("api.example.com"));
    assert!(!rendered.contains("_truncated"));
  }

  #[test]
  fn caps_oversized_values_with_preview() {
    let big = "x".repeat(DEFAULT_PARAMS_SUMMARY_CAP_BYTES * 2);
    let value = serde_json::json!({ "body": big });
    let out = redact_and_cap(value);
    assert_eq!(out.get("_truncated").and_then(|v| v.as_bool()), Some(true));
    let original_bytes = out
      .get("_original_bytes")
      .and_then(|v| v.as_u64())
      .expect("preview carries original size");
    assert!(original_bytes > DEFAULT_PARAMS_SUMMARY_CAP_BYTES as u64);
    let preview_len = out.get("preview").and_then(|v| v.as_str()).unwrap().len();
    assert!(preview_len <= DEFAULT_PARAMS_SUMMARY_CAP_BYTES);
  }
}
