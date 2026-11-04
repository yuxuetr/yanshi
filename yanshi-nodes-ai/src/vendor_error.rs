//! Y2.5: retry discipline for billed vendor calls.
//!
//! Node-level retry (`max_retries`, `FlowExecutionConfig::node_retry_policy`)
//! classifies a `YanshiError` by variant and, for `AsyncExecutionError`,
//! by words in the message. Every modality node used to flatten the
//! vendor's `LLMError` into an `AsyncExecutionError` string, which
//! dropped the HTTP status: a 5xx was not retried (no default pattern
//! matched it), and a 4xx content rejection whose body happened to say
//! "connection" or "timeout" was retried, and billed again.
//!
//! [`vendor_error`] keeps the status's meaning in the variant instead:
//! transient failures (5xx, network, timeout, 429) land on variants the
//! default policy retries; everything else (4xx, auth, parse) lands on
//! `NodeExecutionFailed`, which no default pattern matches.

use yanshi_core::error::YanshiError;
use yanshi_llm::LLMError;

/// Map a vendor call's error, prefixed with `context` ("Text-to-image
/// generation failed"), onto the variant that encodes whether repeating
/// the (billed) call can help.
pub(crate) fn vendor_error(context: &str, error: LLMError) -> YanshiError {
  match error {
    LLMError::HttpError {
      status_code,
      message,
    } if status_code >= 500 => YanshiError::NetworkError {
      message: format!("{context}: vendor server error (HTTP {status_code}): {message}"),
    },
    LLMError::HttpError {
      status_code: 429,
      message,
    } => YanshiError::AsyncExecutionError {
      message: format!("{context}: rate limited (HTTP 429): {message}"),
    },
    LLMError::RateLimitExceeded { .. } | LLMError::RateLimitedWithRetryAfter { .. } => {
      YanshiError::AsyncExecutionError {
        message: format!("{context}: rate limited (HTTP 429): {error}"),
      }
    }
    LLMError::TimeoutError { timeout_ms } => YanshiError::TimeoutExceeded {
      duration_ms: timeout_ms,
    },
    LLMError::NetworkError { .. } => YanshiError::NetworkError {
      message: format!("{context}: {error}"),
    },
    other => YanshiError::NodeExecutionFailed {
      message: format!("{context}: {other}"),
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use yanshi_core::retry::RetryPolicy;

  fn http(status_code: u16, message: &str) -> LLMError {
    LLMError::HttpError {
      status_code,
      message: message.to_string(),
    }
  }

  #[test]
  fn only_transient_vendor_failures_are_retryable_by_default() {
    let policy = RetryPolicy::default();
    let retryable = |e: LLMError| policy.is_retryable(&vendor_error("call failed", e));

    assert!(retryable(http(500, "boom")));
    assert!(retryable(http(503, "unavailable")));
    assert!(retryable(http(429, "slow down")));
    assert!(retryable(LLMError::TimeoutError { timeout_ms: 10 }));

    // A moderation rejection whose text mentions words the message
    // heuristics look for must still not be retried.
    assert!(!retryable(http(
      400,
      "connection to policy server timed out: content rejected"
    )));
    assert!(!retryable(http(401, "bad key")));
    assert!(!retryable(http(404, "no such model")));
  }
}
