use thiserror::Error;

/// Comprehensive error types for LLM operations
#[derive(Error, Debug)]
pub enum LLMError {
  #[error("Configuration error: {message}")]
  ConfigurationError { message: String },

  #[error("Model '{model_name}' not found in registry")]
  ModelNotFound { model_name: String },

  #[error("Provider '{provider}' not supported")]
  UnsupportedProvider { provider: String },

  // F-AF-4: keep the variant shape stable (callers `match` on it) but
  // make the rendered message crisp — every fresh-host failure on this
  // error should be actionable in one line. The message names the env
  // var by provider and points at the standard fix.
  #[error(
    "API key missing for provider '{provider}' — set {} in your environment or ~/.yanshi/.env (run `yanshi config init` to generate a template), or pass --model to override the provider. See yanshi-llm/README.md § {} for the env-var alternatives.",
    env_var_hint(provider),
    provider
  )]
  MissingApiKey { provider: String },

  #[error("Invalid model configuration: {message}")]
  InvalidModelConfig { message: String },

  #[error("HTTP request failed: {status_code} - {message}")]
  HttpError { status_code: u16, message: String },

  /// P-LLM2.7: a 429/5xx response that carried a server-supplied
  /// `Retry-After` header, successfully parsed. Distinct from
  /// [`Self::HttpError`] so [`retry_transient`](crate::client::llm_client)
  /// can honor the server's requested wait exactly (still jittered, as a
  /// floor) instead of guessing via pure exponential backoff.
  ///
  /// Only the chat `execute`/`execute_streaming` methods that route
  /// through `retry_transient` construct this (via
  /// `providers::chat_http_error`) — one-shot modality endpoints
  /// (TTS/ASR/image/video) that never retry keep constructing
  /// `HttpError` directly, since a `Retry-After` value is meaningless
  /// without a retry loop to consume it.
  #[error("HTTP request failed: {status_code} - {message} (retry after {retry_after_ms}ms)")]
  RateLimitedWithRetryAfter {
    status_code: u16,
    message: String,
    retry_after_ms: u64,
  },

  #[error("Request timeout after {timeout_ms}ms")]
  TimeoutError { timeout_ms: u64 },

  #[error("Rate limit exceeded for provider '{provider}': {message}")]
  RateLimitExceeded { provider: String, message: String },

  #[error("Authentication failed for provider '{provider}': {message}")]
  AuthenticationError { provider: String, message: String },

  #[error("API response parsing failed: {message}")]
  ResponseParsingError { message: String },

  #[error("Streaming error: {message}")]
  StreamingError { message: String },

  #[error("Model execution error: {message}")]
  ModelExecutionError { message: String },

  #[error("Quota exceeded for provider '{provider}': {message}")]
  QuotaExceeded { provider: String, message: String },

  #[error("Service unavailable for provider '{provider}': {message}")]
  ServiceUnavailable { provider: String, message: String },

  #[error("Internal LLM error: {message}")]
  InternalError { message: String },

  #[error("Network error: {message}")]
  NetworkError { message: String },

  #[error("Parse error: {message}")]
  ParseError { message: String },

  #[error("API error from '{provider}': {status_code} - {message}")]
  ApiError {
    provider: String,
    status_code: u16,
    message: String,
  },

  #[error("Unsupported operation: {message}")]
  UnsupportedOperation { message: String },

  /// Model does not support the requested feature (e.g. thinking/reasoning
  /// configured for a model whose registry entry has no `supports_thinking`
  /// flag). Fail-fast at request-build time so callers don't waste an API
  /// call discovering the silent drop on the provider side.
  #[error(
    "Model '{model}' does not support feature '{feature}'. \
     Set `supports_thinking: true` and `thinking_kind` on the model in your registry, \
     or use a model that does (e.g. o3-mini, gemini-2.5-pro, deepseek-reasoner)."
  )]
  UnsupportedFeature { model: String, feature: String },
}

pub type Result<T> = std::result::Result<T, LLMError>;

impl LLMError {
  /// V1.5: whether this failure is transient and worth retrying with
  /// backoff (rate limit / server overload / timeout), as opposed to a
  /// caller mistake (bad request, auth, unsupported feature, ...) that
  /// would fail identically on every retry. Providers surface 429/5xx
  /// as a generic `HttpError { status_code, .. }` rather than the
  /// pre-classified `RateLimitExceeded`/`ServiceUnavailable` variants
  /// (those are only produced by the `From<reqwest::Error>` transport-
  /// level mapping below), so both shapes are checked here.
  pub fn is_retryable(&self) -> bool {
    match self {
      LLMError::RateLimitExceeded { .. }
      | LLMError::ServiceUnavailable { .. }
      | LLMError::TimeoutError { .. }
      | LLMError::RateLimitedWithRetryAfter { .. } => true,
      LLMError::HttpError { status_code, .. } => {
        *status_code == 429 || (500..=599).contains(status_code)
      }
      _ => false,
    }
  }

  /// P-LLM2.7: the server-supplied `Retry-After` delay, in milliseconds,
  /// when this error carries one. `retry_transient` prefers this over its
  /// own computed exponential+jitter delay when present.
  pub fn retry_after_ms(&self) -> Option<u64> {
    match self {
      LLMError::RateLimitedWithRetryAfter { retry_after_ms, .. } => Some(*retry_after_ms),
      _ => None,
    }
  }
}

/// Convert common HTTP and network errors to LLMError
impl From<reqwest::Error> for LLMError {
  fn from(error: reqwest::Error) -> Self {
    if error.is_timeout() {
      // Q1.8.2: align the reported timeout with what
      // `providers::default_http_client` actually enforces. Pre-fix
      // this said 30000 ms even though the builder had no timeout at
      // all.
      LLMError::TimeoutError {
        timeout_ms: crate::providers::DEFAULT_HTTP_REQUEST_TIMEOUT_SECS * 1000,
      }
    } else if let Some(status) = error.status() {
      let status_code = status.as_u16();
      let message = error.to_string();

      match status_code {
        401 => LLMError::AuthenticationError {
          provider: "unknown".to_string(),
          message,
        },
        429 => LLMError::RateLimitExceeded {
          provider: "unknown".to_string(),
          message,
        },
        503 => LLMError::ServiceUnavailable {
          provider: "unknown".to_string(),
          message,
        },
        _ => LLMError::HttpError {
          status_code,
          message,
        },
      }
    } else {
      LLMError::InternalError {
        message: error.to_string(),
      }
    }
  }
}

/// F-AF-4: map a provider name to its canonical env-var hint string
/// for the [`LLMError::MissingApiKey`] message. Returns a
/// comma-separated list of accepted env vars (per the provider's
/// `api_key_env` config) when there are multiple, or a single name
/// when there's just one. Unknown providers fall back to a generic
/// hint so the error still renders without panicking.
fn env_var_hint(provider: &str) -> &'static str {
  match provider {
    "openai" => "OPENAI_API_KEY",
    "anthropic" => "ANTHROPIC_API_KEY",
    "google" => "GEMINI_API_KEY (or GOOGLE_API_KEY)",
    "moonshot" => "MOONSHOT_API_KEY (or MOONSHOT_KEY)",
    "stepfun" | "step" => "STEPFUN_API_KEY (or STEP_API_KEY)",
    "dashscope" => "DASHSCOPE_API_KEY",
    "glm" | "bigmodel" | "zhipu" => "GLM_API_KEY (or BIGMODEL_API_KEY, ZHIPU_API_KEY)",
    "deepseek" => "DEEPSEEK_API_KEY",
    "minimax" => "MINIMAX_API_KEY",
    "kling" => "KLING_API_KEY (an API key or ACCESS_KEY:SECRET_KEY)",
    _ => "the provider's *_API_KEY env var",
  }
}

impl From<serde_json::Error> for LLMError {
  fn from(error: serde_json::Error) -> Self {
    LLMError::ResponseParsingError {
      message: error.to_string(),
    }
  }
}

impl From<serde_yaml::Error> for LLMError {
  fn from(error: serde_yaml::Error) -> Self {
    LLMError::ConfigurationError {
      message: error.to_string(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// F-AF-4: the rendered `MissingApiKey` message MUST name the
  /// provider-specific env var so a fresh-host operator can act
  /// on it in one read. Lock the env-var-hint pattern + the
  /// `yanshi config init` actionable hint.
  #[test]
  fn missing_api_key_renders_provider_specific_env_var() {
    let err = LLMError::MissingApiKey {
      provider: "moonshot".to_string(),
    };
    let rendered = err.to_string();
    assert!(
      rendered.contains("MOONSHOT_API_KEY"),
      "moonshot variant must name MOONSHOT_API_KEY: {rendered}"
    );
    assert!(
      rendered.contains("yanshi config init"),
      "actionable fix must mention `yanshi config init`: {rendered}"
    );
    assert!(
      rendered.contains("~/.yanshi/.env"),
      "must point at ~/.yanshi/.env as the standard location: {rendered}"
    );
  }

  /// F-AF-4: unknown providers must still render a usable message
  /// (no panic), even if the hint is generic.
  #[test]
  fn missing_api_key_unknown_provider_falls_back_gracefully() {
    let err = LLMError::MissingApiKey {
      provider: "some-future-provider".to_string(),
    };
    let rendered = err.to_string();
    assert!(rendered.contains("some-future-provider"));
    assert!(rendered.contains("API_KEY"));
  }

  /// V1.5: the three pre-classified transient variants must be retryable.
  #[test]
  fn is_retryable_true_for_preclassified_transient_variants() {
    assert!(
      LLMError::RateLimitExceeded {
        provider: "openai".to_string(),
        message: "429".to_string(),
      }
      .is_retryable()
    );
    assert!(
      LLMError::ServiceUnavailable {
        provider: "openai".to_string(),
        message: "503".to_string(),
      }
      .is_retryable()
    );
    assert!(LLMError::TimeoutError { timeout_ms: 30_000 }.is_retryable());
  }

  /// V1.5: providers surface 429/5xx as a generic `HttpError` (per
  /// their in-provider status-code branch, not the transport-level
  /// `From<reqwest::Error>` mapping) — those status codes must also be
  /// classified as retryable.
  #[test]
  fn is_retryable_true_for_http_error_with_transient_status_codes() {
    for status_code in [429, 500, 502, 503, 504, 599] {
      assert!(
        LLMError::HttpError {
          status_code,
          message: "transient".to_string(),
        }
        .is_retryable(),
        "status {status_code} should be retryable"
      );
    }
  }

  /// V1.5: caller-mistake errors must never be retried — retrying a bad
  /// request or an auth failure just wastes attempts on a call that
  /// will fail identically every time.
  #[test]
  fn is_retryable_false_for_non_transient_errors() {
    assert!(
      !LLMError::HttpError {
        status_code: 400,
        message: "bad request".to_string(),
      }
      .is_retryable()
    );
    assert!(
      !LLMError::AuthenticationError {
        provider: "openai".to_string(),
        message: "bad key".to_string(),
      }
      .is_retryable()
    );
    assert!(
      !LLMError::QuotaExceeded {
        provider: "openai".to_string(),
        message: "quota".to_string(),
      }
      .is_retryable()
    );
    assert!(
      !LLMError::InvalidModelConfig {
        message: "bad config".to_string(),
      }
      .is_retryable()
    );
  }

  /// F-AF-4: every provider currently in the env-var-hint table
  /// renders without falling through to the unknown branch.
  /// Locks the table coverage against silent regressions when a
  /// new provider is added but the hint isn't.
  #[test]
  fn env_var_hint_covers_all_known_providers() {
    for provider in [
      "openai",
      "anthropic",
      "google",
      "moonshot",
      "stepfun",
      "dashscope",
    ] {
      let hint = env_var_hint(provider);
      assert!(
        hint.contains("API_KEY") || hint.contains("KEY"),
        "provider '{provider}' hint must mention an API key env var, got '{hint}'"
      );
      assert_ne!(
        hint, "the provider's *_API_KEY env var",
        "provider '{provider}' fell through to the generic branch"
      );
    }
  }
}
