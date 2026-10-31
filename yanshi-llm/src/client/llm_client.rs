use crate::{
  LLMError, Result, StreamingResponse,
  config::{GlobalDefaults, ModelConfig},
  multimodal::MultimodalMessage,
  providers::ProviderRequest,
  registry::ModelRegistry,
  thinking::ThinkingConfig,
  tool_calling::{LLMResponse, ToolChoice, ToolSpec},
  trace_context::{LlmTraceContext, scope as trace_scope},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

#[cfg(feature = "logging")]
use tracing::{debug, error, info, warn};

/// Q3.6.2: stable, non-cryptographic fingerprint of a prompt or response
/// body. Returns the first 16 hex chars of an FNV-1a 64-bit digest so
/// operators can correlate requests across logs without DEBUG-logging
/// the raw content (which routinely contains PII). The choice of FNV
/// over sha256 is deliberate — we're not authenticating anything, and
/// FNV avoids pulling in a crypto dep purely for log noise.
///
/// Q5.2: re-exported from `yanshi_llm::prompt_fingerprint` so other
/// crates (yanshi-agents in particular) can apply the same
/// DEBUG-vs-TRACE redaction discipline without duplicating the helper.
pub fn prompt_fingerprint(text: &str) -> String {
  let mut hash: u64 = 0xcbf29ce484222325;
  for byte in text.as_bytes() {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(0x100000001b3);
  }
  format!("{hash:016x}")
}

/// Longest a retry waits, whether the server asked (`Retry-After`) or the
/// backoff computed it. A server asking for longer fails the call instead:
/// retrying sooner than it asked only burns another attempt.
const MAX_RETRY_WAIT_MS: u64 = 60_000;

/// V1.5: retry a transient provider-call failure (rate limit / 5xx /
/// timeout) with exponential backoff, driven by the resolved model's
/// registry-wide `defaults.max_retries` / `defaults.retry_delay_ms`
/// (`GlobalDefaults`). `max_retries` of `None`/`0` disables retry
/// entirely — the first failure propagates immediately, matching
/// pre-V1.5 behavior. Non-retryable errors (bad request, auth,
/// unsupported feature, ...) always propagate on the first attempt;
/// see [`LLMError::is_retryable`].
///
/// P-LLM2.7: the computed exponential delay is equal-jittered (half
/// fixed, half randomized) so concurrent callers retrying the same
/// transient failure don't all wake up and re-hit the provider at the
/// exact same instant — a thundering herd the pre-fix pure-exponential
/// formula did nothing to prevent. When the error carries a server-
/// supplied `Retry-After` value ([`LLMError::retry_after_ms`], set by
/// [`crate::providers::chat_http_error`]), that value is honored
/// directly instead of the computed delay — the server knows its own
/// recovery time better than a guess does — though jitter is still
/// **not** applied on top of it, since a `Retry-After` value already
/// represents a concrete floor and the retry loop only has one caller
/// per invocation anyway (jitter's benefit is decorrelating *many*
/// concurrent callers, which the guessed path can't otherwise achieve).
/// Both waits are capped at [`MAX_RETRY_WAIT_MS`]; a longer `Retry-After`
/// returns the error at once.
async fn retry_transient<F, Fut, T>(defaults: &GlobalDefaults, mut call: F) -> Result<T>
where
  F: FnMut() -> Fut,
  Fut: std::future::Future<Output = Result<T>>,
{
  let max_retries = defaults.max_retries.unwrap_or(0);
  let base_delay_ms = defaults.retry_delay_ms.unwrap_or(1000);
  let mut attempt = 0u32;
  loop {
    match call().await {
      Ok(value) => return Ok(value),
      Err(err)
        if attempt < max_retries
          && err.is_retryable()
          && err
            .retry_after_ms()
            .is_none_or(|ms| ms <= MAX_RETRY_WAIT_MS) =>
      {
        let computed_delay_ms = base_delay_ms
          .saturating_mul(1u64 << attempt.min(20))
          .min(MAX_RETRY_WAIT_MS);
        let delay_ms = err
          .retry_after_ms()
          .unwrap_or_else(|| jittered_delay_ms(computed_delay_ms));
        #[cfg(feature = "logging")]
        warn!(
          "LLM call failed with a transient error (attempt {}/{}), retrying in {}ms: {}",
          attempt + 1,
          max_retries,
          delay_ms,
          err
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        attempt += 1;
      }
      Err(err) => return Err(err),
    }
  }
}

/// Equal-jitter backoff: half of `computed_delay_ms` is fixed, half is
/// randomized uniformly — see `retry_transient`'s doc comment for why.
fn jittered_delay_ms(computed_delay_ms: u64) -> u64 {
  use rand::Rng;
  let half = computed_delay_ms / 2;
  half + rand::thread_rng().gen_range(0..=half)
}

/// Response format options for model output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseFormat {
  /// Default text response
  Text,
  /// JSON object response (enforces valid JSON)
  JsonObject,
  /// JSON schema response with specific structure
  JsonSchema {
    /// Name of the response schema
    name: String,
    /// JSON schema definition
    schema: Value,
    /// Whether the schema is strict
    strict: Option<bool>,
  },
}

/// Main LLM client for executing model requests
pub struct LLMClient {
  pub model_name: String,
  pub prompt: String,
  pub multimodal_messages: Option<Vec<MultimodalMessage>>,
  pub temperature: Option<f32>,
  pub max_tokens: Option<u32>,
  pub top_p: Option<f32>,
  pub frequency_penalty: Option<f32>,
  pub stop: Option<Vec<String>>,
  /// Native tool / function-calling specification (typed).
  pub tools: Option<Vec<ToolSpec>>,
  /// Selection strategy when `tools` is set.
  pub tool_choice: Option<ToolChoice>,
  pub response_format: Option<ResponseFormat>,
  /// Extended-reasoning / "thinking" configuration. See
  /// [`crate::ThinkingConfig`] for the cross-provider mapping. Setting this
  /// on a model whose registry entry doesn't have `supports_thinking: true`
  /// produces [`LLMError::UnsupportedFeature`] at request-build time.
  pub thinking: Option<ThinkingConfig>,
  pub enable_logging: bool,
  pub additional_params: HashMap<String, Value>,
  /// Optional W3C trace context to propagate to the underlying HTTP call.
  ///
  /// When set, every `execute*` enters a [`trace_scope`] so all providers'
  /// `build_headers` pick up the same context and emit a `traceparent`
  /// header. When `None`, the LLM call inherits whatever context already
  /// happens to be active (set by a higher layer such as the agent
  /// runtime), or none at all.
  pub trace_context: Option<LlmTraceContext>,
}

impl LLMClient {
  /// Create a new LLM client
  pub fn new(model_name: &str) -> Self {
    Self {
      model_name: model_name.to_string(),
      prompt: String::new(),
      multimodal_messages: None,
      temperature: None,
      max_tokens: None,
      top_p: None,
      frequency_penalty: None,
      stop: None,
      tools: None,
      tool_choice: None,
      response_format: None,
      thinking: None,
      enable_logging: true,
      additional_params: HashMap::new(),
      trace_context: None,
    }
  }

  /// Execute the request and return a non-streaming response.
  ///
  /// Returns only the model's text content. Use [`Self::execute_full`] when
  /// you need access to native tool calls, stop reason, or token usage.
  pub async fn execute(&self) -> Result<String> {
    let start_time = Instant::now();

    if self.enable_logging {
      self.log_request_start();
    }

    let registry = ModelRegistry::global();
    let model_config = registry.get_model(&self.model_name)?;

    // Validate request against model capabilities
    let has_images = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_images()))
      .unwrap_or(false);
    let has_video = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_video()))
      .unwrap_or(false);
    let has_document = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_document()))
      .unwrap_or(false);

    model_config.validate_request(
      true, // has text
      has_images,
      false, // has audio
      has_video,
      has_document,
      false,                // requires streaming (this is non-streaming)
      self.tools.is_some(), // uses tools
    )?;
    let provider = registry.get_provider(&model_config.vendor)?;
    // P-LLM2.7: wait for this vendor's token bucket before dispatching —
    // `None` means no `rate_limit` configured for this vendor (no
    // enforcement, matching pre-fix behavior).
    if let Some(limiter) = registry.get_rate_limiter(&model_config.vendor) {
      limiter.until_ready().await;
    }
    let defaults = registry.get_defaults()?;

    let request = self.build_request(&model_config, false)?;
    let provider = provider.clone();
    let trace_context = self.trace_context.clone();
    let result = retry_transient(&defaults, || {
      let provider = provider.clone();
      let request = request.clone();
      let trace_context = trace_context.clone();
      async move {
        match trace_context {
          Some(ctx) => trace_scope(ctx, async move { provider.execute(&request).await }).await,
          None => provider.execute(&request).await,
        }
      }
    })
    .await;
    let duration = start_time.elapsed();

    let final_result = result.map(|response| response.content.to_string());

    if self.enable_logging {
      self.log_request_complete(&final_result, duration);
    }

    final_result
  }

  /// Log request start information
  fn log_request_start(&self) {
    #[cfg(feature = "logging")]
    {
      info!(
        "Starting LLM request: model={}, prompt_len={}, temp={:?}, format={:?}",
        self.model_name,
        self.prompt.len(),
        self.temperature,
        self.response_format
      );

      // Q3.6.2: do NOT log the full prompt at DEBUG. Prompts routinely
      // contain PII, retrieved documents, system prompts with internal
      // architecture details, etc. Default tracing config can be
      // CRIT/DEBUG without anyone realising they signed up for full
      // prompt persistence. Surface a short SHA-256 fingerprint + length
      // at DEBUG so operators can correlate requests across logs without
      // exposing content; the full prompt only lands at TRACE level,
      // which production setups intentionally exclude.
      debug!(
        "Prompt fingerprint: len={} sha256_prefix={}",
        self.prompt.len(),
        prompt_fingerprint(&self.prompt)
      );
      tracing::trace!("Full prompt: {}", self.prompt);

      if let Some(tools) = &self.tools {
        debug!("Tools enabled: {} functions", tools.len());
      }
    }

    #[cfg(not(feature = "logging"))]
    {
      println!(
        "[Yanshi] Request: {} ({})",
        self.model_name,
        self.prompt.len()
      );
    }
  }

  /// Log request completion information
  fn log_request_complete(&self, result: &Result<String>, duration: std::time::Duration) {
    #[cfg(feature = "logging")]
    {
      match result {
        Ok(response) => {
          info!(
            "LLM request completed: model={}, duration={}ms, response_len={}",
            self.model_name,
            duration.as_millis(),
            response.len()
          );

          // Q3.6.2: see prompt-logging note above. Responses can echo
          // back PII, contain LLM-rendered customer data, or recite
          // a tool's secrets. DEBUG gets a fingerprint + length; full
          // response body is TRACE-only.
          debug!(
            "Response fingerprint: len={} sha256_prefix={}",
            response.len(),
            prompt_fingerprint(response)
          );
          tracing::trace!("Response content: {}", response);

          // Validate JSON if JSON format was requested
          if let Some(ResponseFormat::JsonObject) | Some(ResponseFormat::JsonSchema { .. }) =
            &self.response_format
          {
            match serde_json::from_str::<Value>(response) {
              Ok(_) => debug!("✅ Response is valid JSON"),
              Err(e) => warn!("⚠️ Invalid JSON response: {}", e),
            }
          }
        }
        Err(e) => {
          error!(
            "LLM request failed: model={}, duration={}ms, error={}",
            self.model_name,
            duration.as_millis(),
            e
          );
        }
      }
    }

    #[cfg(not(feature = "logging"))]
    {
      match result {
        Ok(response) => println!(
          "[Yanshi] ✅ Response: {} chars in {}ms",
          response.len(),
          duration.as_millis()
        ),
        Err(e) => println!("[Yanshi] ❌ Error: {} ({}ms)", e, duration.as_millis()),
      }
    }
  }

  /// Execute the request and return the full structured response, including
  /// native tool calls, stop reason, and usage statistics.
  ///
  /// Agent runtimes (ReAct, Plan-Execute) call this to consume native
  /// `tool_calls` instead of re-parsing JSON from `content`.
  pub async fn execute_full(&self) -> Result<LLMResponse> {
    let start_time = Instant::now();

    if self.enable_logging {
      self.log_request_start();
    }

    let registry = ModelRegistry::global();
    let model_config = registry.get_model(&self.model_name)?;

    let has_images = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_images()))
      .unwrap_or(false);
    let has_video = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_video()))
      .unwrap_or(false);
    let has_document = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_document()))
      .unwrap_or(false);

    model_config.validate_request(
      true,
      has_images,
      false,
      has_video,
      has_document,
      false,
      self.tools.is_some(),
    )?;
    let provider = registry.get_provider(&model_config.vendor)?;
    // P-LLM2.7: wait for this vendor's token bucket before dispatching —
    // `None` means no `rate_limit` configured for this vendor (no
    // enforcement, matching pre-fix behavior).
    if let Some(limiter) = registry.get_rate_limiter(&model_config.vendor) {
      limiter.until_ready().await;
    }
    let defaults = registry.get_defaults()?;

    let request = self.build_request(&model_config, false)?;
    let provider = provider.clone();
    let trace_context = self.trace_context.clone();
    let provider_response = retry_transient(&defaults, || {
      let provider = provider.clone();
      let request = request.clone();
      let trace_context = trace_context.clone();
      async move {
        match trace_context {
          Some(ctx) => trace_scope(ctx, async move { provider.execute(&request).await }).await,
          None => provider.execute(&request).await,
        }
      }
    })
    .await?;
    let duration = start_time.elapsed();

    let content_text = provider_response.content.to_string();
    let response = LLMResponse {
      content: content_text.clone(),
      tool_calls: provider_response.tool_calls.clone(),
      stop_reason: provider_response.stop_reason.clone(),
      usage: provider_response.usage.clone(),
      raw_metadata: provider_response.metadata.clone(),
      thinking: provider_response.thinking.clone(),
    };

    if self.enable_logging {
      self.log_request_complete(&Ok(content_text), duration);
    }

    Ok(response)
  }

  /// V2.2: execute via the streaming path but reconstruct the same
  /// aggregate [`LLMResponse`] [`Self::execute_full`] returns, while
  /// invoking `on_chunk` for every [`crate::client::streaming::StreamChunk`]
  /// as it arrives — see [`crate::client::streaming::collect_streaming_response`]
  /// for the exact reconstruction contract and its documented limitations.
  pub async fn execute_streaming_collected<F, Fut>(&self, on_chunk: F) -> Result<LLMResponse>
  where
    F: FnMut(&crate::client::streaming::StreamChunk) -> Fut,
    Fut: std::future::Future<Output = ()>,
  {
    let stream = self.execute_streaming().await?;
    crate::client::streaming::collect_streaming_response(stream, on_chunk).await
  }

  /// Execute the request and return a streaming response
  pub async fn execute_streaming(&self) -> Result<Box<dyn StreamingResponse>> {
    if self.enable_logging {
      self.log_request_start();
    }

    let registry = ModelRegistry::global();
    let model_config = registry.get_model(&self.model_name)?;

    // Validate request against model capabilities (streaming mode)
    let has_images = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_images()))
      .unwrap_or(false);
    let has_video = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_video()))
      .unwrap_or(false);
    let has_document = self
      .multimodal_messages
      .as_ref()
      .map(|msgs| msgs.iter().any(|msg| msg.has_document()))
      .unwrap_or(false);

    model_config.validate_request(
      true, // has text
      has_images,
      false, // has audio
      has_video,
      has_document,
      true,                 // requires streaming
      self.tools.is_some(), // uses tools
    )?;

    let provider = registry.get_provider(&model_config.vendor)?;
    // P-LLM2.7: wait for this vendor's token bucket before dispatching —
    // `None` means no `rate_limit` configured for this vendor (no
    // enforcement, matching pre-fix behavior).
    if let Some(limiter) = registry.get_rate_limiter(&model_config.vendor) {
      limiter.until_ready().await;
    }
    let defaults = registry.get_defaults()?;

    let request = self.build_request(&model_config, true)?;

    let provider = provider.clone();
    let trace_context = self.trace_context.clone();
    let result = retry_transient(&defaults, || {
      let provider = provider.clone();
      let request = request.clone();
      let trace_context = trace_context.clone();
      async move {
        match trace_context {
          Some(ctx) => {
            trace_scope(
              ctx,
              async move { provider.execute_streaming(&request).await },
            )
            .await
          }
          None => provider.execute_streaming(&request).await,
        }
      }
    })
    .await;

    if self.enable_logging {
      #[cfg(feature = "logging")]
      match &result {
        Ok(_) => info!(
          "Streaming request started successfully: {}",
          self.model_name
        ),
        Err(e) => error!("Streaming request failed: {}", e),
      }

      #[cfg(not(feature = "logging"))]
      match &result {
        Ok(_) => println!("[Yanshi] 📡 Streaming started: {}", self.model_name),
        Err(e) => println!("[Yanshi] ❌ Streaming failed: {}", e),
      }
    }

    result
  }

  fn build_request(&self, model_config: &ModelConfig, streaming: bool) -> Result<ProviderRequest> {
    let mut params = HashMap::new();

    // Apply model defaults
    if let Some(temp) = model_config.temperature.or(self.temperature) {
      let num =
        serde_json::Number::from_f64(temp as f64).ok_or_else(|| LLMError::ConfigurationError {
          message: format!("Invalid temperature value: {}", temp),
        })?;
      params.insert("temperature".to_string(), Value::Number(num));
    }

    if let Some(tokens) = self.max_tokens.or(model_config.max_tokens) {
      params.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(tokens)),
      );
    }

    if let Some(top_p) = model_config.top_p.or(self.top_p) {
      let num =
        serde_json::Number::from_f64(top_p as f64).ok_or_else(|| LLMError::ConfigurationError {
          message: format!("Invalid top_p value: {}", top_p),
        })?;
      params.insert("top_p".to_string(), Value::Number(num));
    }

    if let Some(freq_penalty) = self.frequency_penalty {
      let num = serde_json::Number::from_f64(freq_penalty as f64).ok_or_else(|| {
        LLMError::ConfigurationError {
          message: format!("Invalid frequency_penalty value: {}", freq_penalty),
        }
      })?;
      params.insert("frequency_penalty".to_string(), Value::Number(num));
    }

    if let Some(stop_sequences) = &self.stop {
      if stop_sequences.len() == 1 {
        if let Some(first) = stop_sequences.first() {
          params.insert("stop".to_string(), Value::String(first.clone()));
        }
      } else {
        params.insert(
          "stop".to_string(),
          Value::Array(
            stop_sequences
              .iter()
              .map(|s| Value::String(s.clone()))
              .collect(),
          ),
        );
      }
    }

    // Note: tools / tool_choice / response_format are no longer dropped into
    // `parameters`. They travel as typed fields on `ProviderRequest::tools` /
    // `tool_choice` / `response_format`, and each provider serialises them to
    // its own native wire format (OpenAI/Moonshot/StepFun pass through;
    // Anthropic/Google adapt or conditionally skip — see
    // `ProviderRequest::response_format`'s doc comment for the exact rules;
    // Mock ignores them).

    // Override with user-provided params
    for (key, value) in &self.additional_params {
      params.insert(key.clone(), value.clone());
    }

    // Build messages based on input type
    let messages = if let Some(ref multimodal_messages) = self.multimodal_messages {
      // Use multimodal messages directly
      self.build_multimodal_messages(multimodal_messages, model_config)?
    } else {
      // Use traditional prompt
      vec![self.build_message_content(model_config)?]
    };

    // Fail-fast: caller asked for thinking but the model isn't configured
    // for it. Better here than after the HTTP round trip — silent provider-
    // side drops are the exact "I set X but didn't get X" bug class we
    // want to avoid.
    if let Some(thinking) = &self.thinking
      && !thinking.is_disabled()
      && !model_config.supports_thinking()
    {
      return Err(LLMError::UnsupportedFeature {
        model: self.model_name.clone(),
        feature: "thinking".to_string(),
      });
    }

    Ok(ProviderRequest {
      model: model_config
        .model_id
        .clone()
        .unwrap_or_else(|| self.model_name.clone()),
      messages,
      stream: streaming,
      parameters: params,
      tools: self.tools.clone(),
      tool_choice: self.tool_choice.clone(),
      thinking: self.thinking.clone(),
      response_format: self.response_format.clone(),
    })
  }

  /// Build multimodal messages for the request
  fn build_multimodal_messages(
    &self,
    multimodal_messages: &[MultimodalMessage],
    model_config: &ModelConfig,
  ) -> Result<Vec<serde_json::Value>> {
    let mut messages = Vec::new();

    // Analyze input types in the messages
    let has_images = multimodal_messages.iter().any(|msg| msg.has_images());
    let has_video = multimodal_messages.iter().any(|msg| msg.has_video());
    let has_document = multimodal_messages.iter().any(|msg| msg.has_document());
    let has_text = multimodal_messages
      .iter()
      .any(|msg| !msg.get_text().is_empty());

    // Validate against model capabilities
    model_config.validate_request(
      has_text,
      has_images,
      false, // audio
      has_video,
      has_document,
      false,                // streaming (checked separately)
      self.tools.is_some(), // uses tools
    )?;

    for msg in multimodal_messages {
      let capabilities = model_config.get_capabilities();

      if capabilities.is_multimodal() {
        // Use full multimodal format for multimodal models
        messages.push(msg.to_openai_format());
      } else {
        // Convert to text-only for non-multimodal models
        if msg.has_images() {
          return Err(crate::LLMError::InvalidModelConfig {
            message: format!("Model {} does not support image input", self.model_name),
          });
        }
        if msg.has_document() {
          return Err(crate::LLMError::InvalidModelConfig {
            message: format!("Model {} does not support document input", self.model_name),
          });
        }
        if msg.has_video() {
          return Err(crate::LLMError::InvalidModelConfig {
            message: format!("Model {} does not support video input", self.model_name),
          });
        }
        messages.push(serde_json::json!({
          "role": msg.role,
          "content": msg.to_text_format()
        }));
      }
    }

    Ok(messages)
  }

  /// Build message content based on model type and capabilities
  fn build_message_content(&self, model_config: &ModelConfig) -> Result<serde_json::Value> {
    // For now, we only support text content input
    // Future enhancement: detect and handle image/audio inputs based on model type
    match model_config.model_type() {
      "multimodal" => {
        // Check if prompt contains image markers or base64 data
        if self.prompt.contains("data:image/") || self.prompt.contains("![image]") {
          // TODO: Parse multimodal content - for now, pass as text
          Ok(serde_json::json!({"role": "user", "content": self.prompt}))
        } else {
          Ok(serde_json::json!({"role": "user", "content": self.prompt}))
        }
      }
      "image" => {
        // Image generation models might expect different prompt structure
        Ok(serde_json::json!({"role": "user", "content": self.prompt}))
      }
      "audio" | "tts" => {
        // Audio models might expect different prompt structure
        Ok(serde_json::json!({"role": "user", "content": self.prompt}))
      }
      _ => {
        // Default text-only message
        Ok(serde_json::json!({"role": "user", "content": self.prompt}))
      }
    }
  }
}

/// Builder pattern for LLM client
pub struct LLMClientBuilder {
  client: LLMClient,
}

impl LLMClientBuilder {
  pub fn new(model_name: &str) -> Self {
    Self {
      client: LLMClient::new(model_name),
    }
  }

  pub fn prompt(mut self, prompt: &str) -> Self {
    self.client.prompt = prompt.to_string();
    self.client.multimodal_messages = None; // Clear multimodal if using prompt
    self
  }

  /// Set multimodal messages (replaces any existing prompt or messages)
  pub fn multimodal_messages(mut self, messages: Vec<MultimodalMessage>) -> Self {
    self.client.multimodal_messages = Some(messages);
    self.client.prompt = String::new(); // Clear prompt if using multimodal
    self
  }

  /// Add a single multimodal message
  pub fn add_multimodal_message(mut self, message: MultimodalMessage) -> Self {
    if let Some(ref mut messages) = self.client.multimodal_messages {
      messages.push(message);
    } else {
      self.client.multimodal_messages = Some(vec![message]);
      self.client.prompt = String::new(); // Clear prompt if using multimodal
    }
    self
  }

  /// Shortcut for creating a multimodal prompt with text and image
  pub fn multimodal_prompt(mut self, message: MultimodalMessage) -> Self {
    self.client.multimodal_messages = Some(vec![message]);
    self.client.prompt = String::new(); // Clear prompt if using multimodal
    self
  }

  /// Helper method to create text + image message quickly
  pub fn text_and_image<T: Into<String>, U: Into<String>>(self, text: T, image_url: U) -> Self {
    let message = MultimodalMessage::text_and_image("user", text, image_url);
    self.multimodal_prompt(message)
  }

  pub fn temperature(mut self, temperature: f32) -> Self {
    self.client.temperature = Some(temperature);
    self
  }

  pub fn max_tokens(mut self, max_tokens: u32) -> Self {
    self.client.max_tokens = Some(max_tokens);
    self
  }

  pub fn top_p(mut self, top_p: f32) -> Self {
    self.client.top_p = Some(top_p);
    self
  }

  pub fn frequency_penalty(mut self, penalty: f32) -> Self {
    self.client.frequency_penalty = Some(penalty);
    self
  }

  pub fn stop<S: Into<String>>(mut self, stop_sequences: Vec<S>) -> Self {
    self.client.stop = Some(stop_sequences.into_iter().map(|s| s.into()).collect());
    self
  }

  /// Set the typed tool specifications the model can call.
  ///
  /// Provider adapters serialise these to their native format (OpenAI tools
  /// array, Anthropic tools block, Google function_declarations).
  pub fn tools(mut self, tools: Vec<ToolSpec>) -> Self {
    self.client.tools = Some(tools);
    self
  }

  /// Set the tool selection strategy. Has no effect when `tools` is unset.
  pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
    self.client.tool_choice = Some(choice);
    self
  }

  /// Set tools from raw JSON values in OpenAI tool format.
  ///
  /// Convenience for callers (notably MCP adapters) that already produce
  /// OpenAI-shaped JSON. Errors if any entry fails to parse.
  pub fn tools_from_openai_json(mut self, tools: Vec<Value>) -> Result<Self> {
    let parsed: std::result::Result<Vec<ToolSpec>, String> =
      tools.iter().map(ToolSpec::from_openai_value).collect();
    let parsed = parsed.map_err(|message| LLMError::ConfigurationError { message })?;
    self.client.tools = Some(parsed);
    Ok(self)
  }

  pub fn response_format(mut self, format: ResponseFormat) -> Self {
    self.client.response_format = Some(format);
    self
  }

  /// Enable extended reasoning / "thinking" for this request.
  ///
  /// Maps the unified [`ThinkingConfig`] onto the provider's native wire
  /// shape (Anthropic `thinking` block, OpenAI `reasoning_effort`, Google
  /// `thinkingConfig`). For models without thinking support (e.g.
  /// `gpt-4o`, `claude-3-5-sonnet-20241022`), `execute*` returns
  /// [`LLMError::UnsupportedFeature`] before any HTTP call — silent
  /// drops on the provider side would otherwise mask the misconfiguration.
  ///
  /// Example:
  /// ```ignore
  /// Yanshi::model("claude-sonnet-4-6")
  ///   .prompt("Reason step by step about ...")
  ///   .thinking(ThinkingConfig::Medium)
  ///   .execute_full()
  ///   .await?;
  /// ```
  pub fn thinking(mut self, config: ThinkingConfig) -> Self {
    self.client.thinking = Some(config);
    self
  }

  pub fn json_mode(mut self) -> Self {
    self.client.response_format = Some(ResponseFormat::JsonObject);
    self
  }

  pub fn json_schema<S: Into<String>>(mut self, name: S, schema: Value) -> Self {
    self.client.response_format = Some(ResponseFormat::JsonSchema {
      name: name.into(),
      schema,
      strict: Some(true),
    });
    self
  }

  pub fn enable_logging(mut self, enabled: bool) -> Self {
    self.client.enable_logging = enabled;
    self
  }

  /// Propagate the given W3C trace context as a `traceparent` header on
  /// the outbound HTTP call. Pass `None` to clear a previously set context.
  pub fn trace_context(mut self, context: impl Into<Option<LlmTraceContext>>) -> Self {
    self.client.trace_context = context.into();
    self
  }

  pub fn param<T: Into<Value>>(mut self, key: &str, value: T) -> Self {
    self
      .client
      .additional_params
      .insert(key.to_string(), value.into());
    self
  }

  /// Add a system message to the request
  /// This creates a multimodal message with system role
  pub fn system(mut self, system_message: &str) -> Self {
    let system_msg = crate::multimodal::MultimodalMessage::system()
      .add_text(system_message)
      .build();

    // If we already have multimodal messages, add to them
    if let Some(ref mut messages) = self.client.multimodal_messages {
      // Insert system message at the beginning
      messages.insert(0, system_msg);
    } else {
      // Create new multimodal messages with system message
      let mut messages = vec![system_msg];

      // If there's a prompt, add it as a user message
      if !self.client.prompt.is_empty() {
        let user_msg = crate::multimodal::MultimodalMessage::user()
          .add_text(&self.client.prompt)
          .build();
        messages.push(user_msg);
        self.client.prompt = String::new(); // Clear the prompt since we're using multimodal now
      }

      self.client.multimodal_messages = Some(messages);
    }

    self
  }

  pub async fn execute(self) -> Result<String> {
    self.client.execute().await
  }

  /// Execute and return the full structured `LLMResponse`, including native
  /// tool calls and stop reason.
  pub async fn execute_full(self) -> Result<LLMResponse> {
    self.client.execute_full().await
  }

  pub async fn execute_streaming(self) -> Result<Box<dyn StreamingResponse>> {
    self.client.execute_streaming().await
  }

  /// See [`LLMClient::execute_streaming_collected`].
  pub async fn execute_streaming_collected<F, Fut>(self, on_chunk: F) -> Result<LLMResponse>
  where
    F: FnMut(&crate::client::streaming::StreamChunk) -> Fut,
    Fut: std::future::Future<Output = ()>,
  {
    self.client.execute_streaming_collected(on_chunk).await
  }
}

#[cfg(test)]
mod retry_transient_tests {
  use super::*;
  use std::sync::atomic::{AtomicU32, Ordering};

  fn defaults_with(max_retries: u32, retry_delay_ms: u64) -> GlobalDefaults {
    GlobalDefaults {
      timeout_seconds: None,
      max_retries: Some(max_retries),
      retry_delay_ms: Some(retry_delay_ms),
    }
  }

  /// V1.5: a transient error (429-class) followed by success must
  /// ultimately succeed once retries are configured, and must have
  /// actually retried (not just gotten lucky on attempt 1).
  #[tokio::test]
  async fn retries_a_transient_error_then_succeeds() {
    let attempts = AtomicU32::new(0);
    let defaults = defaults_with(3, 1);

    let result = retry_transient(&defaults, || {
      let attempt = attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        if attempt == 0 {
          Err(LLMError::RateLimitExceeded {
            provider: "mock".to_string(),
            message: "429".to_string(),
          })
        } else {
          Ok("ok".to_string())
        }
      }
    })
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(
      attempts.load(Ordering::SeqCst),
      2,
      "must have retried exactly once after the first transient failure"
    );
  }

  /// V1.5: exhausting `max_retries` on a persistently transient error
  /// must surface the underlying error, not loop forever.
  #[tokio::test]
  async fn gives_up_and_returns_the_error_once_retries_are_exhausted() {
    let attempts = AtomicU32::new(0);
    let defaults = defaults_with(2, 1);

    let result: Result<()> = retry_transient(&defaults, || {
      attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        Err(LLMError::ServiceUnavailable {
          provider: "mock".to_string(),
          message: "503".to_string(),
        })
      }
    })
    .await;

    assert!(matches!(result, Err(LLMError::ServiceUnavailable { .. })));
    assert_eq!(
      attempts.load(Ordering::SeqCst),
      3,
      "1 initial attempt + 2 retries = 3 total calls"
    );
  }

  /// V1.5: a non-retryable error (e.g. a 4xx parameter error) must
  /// propagate on the very first attempt, even with retries configured
  /// — retrying a caller mistake just wastes attempts on a call that
  /// will fail identically every time.
  #[tokio::test]
  async fn does_not_retry_a_non_transient_error() {
    let attempts = AtomicU32::new(0);
    let defaults = defaults_with(5, 1);

    let result: Result<()> = retry_transient(&defaults, || {
      attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        Err(LLMError::HttpError {
          status_code: 400,
          message: "bad request".to_string(),
        })
      }
    })
    .await;

    assert!(matches!(
      result,
      Err(LLMError::HttpError {
        status_code: 400,
        ..
      })
    ));
    assert_eq!(
      attempts.load(Ordering::SeqCst),
      1,
      "a non-retryable error must not be retried"
    );
  }

  /// V1.5: `max_retries: None`/`0` (the pre-V1.5 default, i.e. no
  /// `defaults:` block configured) must preserve pre-V1.5 behavior —
  /// the first transient failure propagates immediately with no retry.
  #[tokio::test]
  async fn max_retries_none_disables_retry_entirely() {
    let attempts = AtomicU32::new(0);
    let defaults = GlobalDefaults::default();

    let result: Result<()> = retry_transient(&defaults, || {
      attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        Err(LLMError::RateLimitExceeded {
          provider: "mock".to_string(),
          message: "429".to_string(),
        })
      }
    })
    .await;

    assert!(matches!(result, Err(LLMError::RateLimitExceeded { .. })));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
  }

  /// P-LLM2.7: equal jitter must stay within `[computed/2, computed]` —
  /// never below half (that would defeat exponential backoff's purpose)
  /// and never above the computed delay (that would slow retries down
  /// beyond what was configured). Run many iterations since it's
  /// randomized.
  #[test]
  fn jittered_delay_stays_within_equal_jitter_bounds() {
    for computed in [0u64, 1, 2, 1000, 60_000] {
      for _ in 0..200 {
        let delay = jittered_delay_ms(computed);
        assert!(
          delay >= computed / 2,
          "delay {delay} below half of computed {computed}"
        );
        assert!(delay <= computed, "delay {delay} above computed {computed}");
      }
    }
  }

  /// P-LLM2.7: a `RateLimitedWithRetryAfter` error's delay must be
  /// honored exactly, not the computed exponential+jitter delay — proven
  /// by pausing the clock and asserting the retry only becomes observable
  /// once *exactly* `retry_after_ms` has elapsed, not a moment before.
  #[tokio::test(start_paused = true)]
  async fn retry_after_ms_is_honored_exactly_instead_of_computed_backoff() {
    let attempts = AtomicU32::new(0);
    // A large retry_delay_ms so the computed exponential delay (1 hour on
    // attempt 0) would be trivially distinguishable from the much shorter
    // retry_after_ms this test expects to actually be honored.
    let defaults = defaults_with(1, 3_600_000);

    let call = retry_transient(&defaults, || {
      let attempt = attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        if attempt == 0 {
          Err(LLMError::RateLimitedWithRetryAfter {
            status_code: 429,
            message: "slow down".to_string(),
            retry_after_ms: 5_000,
          })
        } else {
          Ok(())
        }
      }
    });
    tokio::pin!(call);

    // Not enough time has passed yet for the honored retry_after_ms —
    // the future must still be pending.
    tokio::time::advance(std::time::Duration::from_millis(4_999)).await;
    assert!(
      matches!(futures::poll!(&mut call), std::task::Poll::Pending),
      "must still be waiting just before retry_after_ms elapses"
    );

    // The remaining 1ms crosses the retry_after_ms threshold.
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    let result = call.await;
    assert!(result.is_ok());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
  }

  /// Eval 2026-10-07 L3: a server's `Retry-After` (and the computed
  /// backoff) had no ceiling, so `Retry-After: 3600` parked a caller with
  /// no outer timeout for an hour. A longer wait fails fast instead; the
  /// computed backoff is capped.
  #[tokio::test(start_paused = true)]
  async fn retry_waits_are_capped() {
    let attempts = AtomicU32::new(0);
    let defaults = defaults_with(3, 1_000);
    let call = retry_transient(&defaults, || {
      attempts.fetch_add(1, Ordering::SeqCst);
      async {
        Err::<(), _>(LLMError::RateLimitedWithRetryAfter {
          status_code: 429,
          message: "come back in an hour".to_string(),
          retry_after_ms: 3_600_000,
        })
      }
    });
    tokio::pin!(call);
    assert!(
      matches!(futures::poll!(&mut call), std::task::Poll::Ready(Err(_))),
      "a Retry-After past the cap must fail without waiting"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let attempts = AtomicU32::new(0);
    let defaults = defaults_with(1, 3_600_000);
    let call = retry_transient(&defaults, || {
      let attempt = attempts.fetch_add(1, Ordering::SeqCst);
      async move {
        if attempt == 0 {
          Err(LLMError::HttpError {
            status_code: 503,
            message: "busy".to_string(),
          })
        } else {
          Ok(())
        }
      }
    });
    tokio::pin!(call);
    // First attempt fails and starts the backoff sleep.
    assert!(matches!(
      futures::poll!(&mut call),
      std::task::Poll::Pending
    ));
    tokio::time::advance(std::time::Duration::from_millis(MAX_RETRY_WAIT_MS)).await;
    assert!(
      matches!(futures::poll!(&mut call), std::task::Poll::Ready(Ok(()))),
      "the computed backoff must not exceed the cap"
    );
  }
}
