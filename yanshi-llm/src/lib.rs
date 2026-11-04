//! # Yanshi LLM Integration Crate
//!
//! Provides unified interface for multiple LLM providers with streaming support.
//!
//! ## Quick Start
//!
//! ### Text-only Models
//! ```ignore
//! use yanshi_llm::Yanshi;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), yanshi_llm::LLMError> {
//!   // Auto-initialize (tries project config → user config → built-in defaults)
//!   Yanshi::init().await?;
//!   
//!   // Use any supported model
//!   let response = Yanshi::model("gpt-4o")
//!     .prompt("Hello, world!")
//!     .temperature(0.7)
//!     .execute().await?;
//!   
//!   println!("Response: {}", response);
//!   Ok(())
//! }
//! ```
//!
//! ### Multimodal Models (Text + Images)
//! ```ignore
//! use yanshi_llm::{Yanshi, MultimodalMessage};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), yanshi_llm::LLMError> {
//!   Yanshi::init().await?;
//!   
//!   // Simple text + image
//!   let response = Yanshi::model("step-1o-turbo-vision")
//!     .text_and_image("Describe this image", "https://example.com/image.jpg")
//!     .temperature(0.7)
//!     .execute().await?;
//!   
//!   // Or build complex multimodal messages
//!   let message = MultimodalMessage::user()
//!     .add_text("Analyze these images:")
//!     .add_image_url("https://example.com/img1.jpg")
//!     .add_image_url("https://example.com/img2.jpg")
//!     .build();
//!   
//!   let response = Yanshi::model("step-1o-turbo-vision")
//!     .multimodal_prompt(message)
//!     .execute().await?;
//!   
//!   println!("Analysis: {}", response);
//!   Ok(())
//! }
//! ```
//!
//! ## Configuration Management
//!
//! Yanshi uses a unified configuration system with the following priority:
//!
//! 1. **Explicit override**: `YANSHI_MODELS_CONFIG`
//! 2. **User-specific**: `~/.yanshi/models.yml`
//! 3. **Legacy user-specific**: `~/.yanshi/models.yaml`
//! 4. **Built-in defaults**: Bundled in crate (lowest priority)
//!
//! ### Generate Configuration Files
//!
//! ```ignore
//! // Generate user-specific config
//! Yanshi::generate_config().await?;
//! ```
//!
//! ### Configuration Options
//!
//! - **For developers**: Use built-in defaults for immediate prototyping
//! - **For users**: Generate `~/.yanshi/models.yml` for persistent settings
//!
//! ## Supported Providers
//!
//! - **OpenAI**: GPT-4o, GPT-4o-mini, GPT-4-turbo (text-only)
//! - **Anthropic**: Claude-3.5-Sonnet, Claude-3-Haiku (text-only)  
//! - **Google**: Gemini-1.5-Pro, Gemini-1.5-Flash (text-only)
//! - **Moonshot**: Various models (text-only)
//! - **StepFun**: step-1o-turbo-vision, step-2-16k (multimodal support)

pub mod client;
pub mod config;
pub mod discovery;
pub mod error;
pub mod modality_dispatch;
pub mod model_types;
pub mod multimodal;
pub mod pricing;
pub mod providers;
pub mod registry;
pub mod thinking;
pub mod tokenizer;
pub mod tool_calling;
pub mod trace_context;

// Re-export main API components
pub use client::{LLMClient, ResponseFormat, StreamingResponse, prompt_fingerprint};
pub use config::{
  LLMConfig, LLMConfigSource, LLMConfigSourceKind, LoadingBenchmark, MODELS_CONFIG_ENV,
  ModelConfig, PerformanceComparison, VendorConfigManager,
};
pub use discovery::{ConfigUpdater, ModelFetcher, ModelValidator};
pub use error::{LLMError, Result};
pub use model_types::{InputType, ModelCapabilities, ModelType, OutputType};
pub use multimodal::{ImageData, ImageUrl, MessageContent, MultimodalMessage};
pub use providers::modality::{
  AsrProvider, AsrRequest, AsrResponse, GeneratedImage, GeneratedVideo, Image2ImageProvider,
  Image2ImageRequest, ImageEditProvider, ImageEditRequest, ImageGenerationResponse,
  Text2ImageProvider, Text2ImageRequest, Text2VideoProvider, Text2VideoRequest, TtsProvider,
  TtsRequest, TtsResponse, VideoGenerationResponse, VideoGenerationStatus, VideoGenerationTask,
};
pub use registry::ModelRegistry;
pub use thinking::{ThinkingConfig, ThinkingKind};
pub use tokenizer::{
  HeuristicCounter, TiktokenCounter, TokenCounter, TokenCounterError, count_tokens_for_model,
  counter_for_model,
};
pub use tool_calling::{LLMResponse, StopReason, ToolCallRequest, ToolChoice, ToolSpec};
pub use trace_context::LlmTraceContext;

// Fluent API entry point
use crate::client::LLMClientBuilder;

/// Main entry point for Yanshi LLM integration
///
/// Example usage:
/// ```ignore
/// // Non-streaming request
/// let response = Yanshi::model("gpt-4o")
///     .prompt("Hello, world!")
///     .temperature(0.8)
///     .max_tokens(100)
///     .execute().await?;
///
/// // Streaming request
/// let stream = Yanshi::model("claude-3-5-sonnet")
///     .prompt("Tell me a story")
///     .temperature(0.7)
///     .top_p(0.9)
///     .execute_streaming().await?;
///
/// // With tools (future MCP integration)
/// let response = Yanshi::model("gpt-4o")
///     .prompt("Search for information about Rust")
///     .tools(mcp_tools) // Vec<Value> from yanshi-mcp
///     .execute().await?;
///
/// // JSON mode for structured output
/// let json_response = Yanshi::model("gpt-4o")
///     .prompt("Return user data as JSON with name, age, email fields")
///     .json_mode()
///     .execute().await?;
///
/// // With logging enabled
/// let response = Yanshi::model("claude-3-5-sonnet")
///     .prompt("Analyze this data")
///     .enable_logging(true)
///     .execute().await?;
/// ```
pub struct Yanshi;

impl Yanshi {
  /// Create a new LLM client builder for the specified model
  pub fn model(model_name: &str) -> LLMClientBuilder {
    LLMClientBuilder::new(model_name)
  }

  /// Initialize the LLM system with a configuration file
  pub async fn init_with_config(config_path: &str) -> Result<()> {
    let registry = ModelRegistry::global();
    registry.load_config(config_path).await?;
    Ok(())
  }

  /// Initialize the LLM system with default configuration
  ///
  /// Configuration priority (first found wins):
  /// 1. `YANSHI_MODELS_CONFIG`
  /// 2. `~/.yanshi/models.yml` (user-specific)
  /// 3. `~/.yanshi/models.yaml` (legacy user-specific)
  /// 4. Built-in defaults (bundled in crate)
  ///
  /// Also loads environment variables from ~/.yanshi/.env if available
  pub async fn init() -> Result<()> {
    // Load environment variables from ~/.yanshi/.env
    if let Some(home_dir) = dirs::home_dir() {
      let user_env = home_dir.join(".yanshi").join(".env");
      if user_env.exists() {
        dotenvy::from_path(&user_env).ok();
      }
    }

    let source = LLMConfig::resolve_default_source()?;
    for warning in &source.warnings {
      eprintln!("Warning: {warning}");
    }

    // Routes on `source.kind`: file-based sources go through
    // `LLMConfig::from_file`, `UserModelsDir` through
    // `VendorConfigManager`, and `BuiltInDefault` through the compile-time
    // bundled config — see `LLMConfig::from_source`.
    ModelRegistry::global().load_from_source(&source).await?;
    Ok(())
  }

  /// Initialize with built-in default configuration
  pub async fn init_with_builtin_config() -> Result<()> {
    let registry = ModelRegistry::global();
    registry.load_builtin_config().await?;
    Ok(())
  }

  /// Generate default configuration files in ~/.yanshi/
  /// Creates both models.yml and .env template files
  pub async fn generate_config() -> Result<()> {
    let home_dir = dirs::home_dir().ok_or_else(|| crate::LLMError::ConfigurationError {
      message: "Could not determine home directory".to_string(),
    })?;

    let config_dir = home_dir.join(".yanshi");
    let config_path = config_dir.join("models.yml");
    let env_path = config_dir.join(".env");

    // The bundled default is now split across `templates/models/*.yml` +
    // the slimmed `templates/default_models.yml` (providers/defaults
    // only), so the starter file users get is the merged, re-serialized
    // form rather than a single `include_str!` — see
    // `config::builtin_default_config_yaml`.
    let config_content = crate::config::builtin_default_config_yaml()?;
    let env_content = include_str!("../templates/default.env");

    // Create directory if it doesn't exist
    tokio::fs::create_dir_all(&config_dir).await.map_err(|e| {
      crate::LLMError::ConfigurationError {
        message: format!("Failed to create config directory: {}", e),
      }
    })?;

    // Write models.yml
    tokio::fs::write(&config_path, config_content)
      .await
      .map_err(|e| crate::LLMError::ConfigurationError {
        message: format!("Failed to write config file: {}", e),
      })?;

    // Write .env file
    tokio::fs::write(&env_path, env_content)
      .await
      .map_err(|e| crate::LLMError::ConfigurationError {
        message: format!("Failed to write .env file: {}", e),
      })?;

    println!("✅ Generated configuration file: {}", config_path.display());
    println!("✅ Generated environment file: {}", env_path.display());
    println!("⚠️  Add your API keys to ~/.yanshi/.env");
    Ok(())
  }

  /// Initialize logging for Yanshi LLM
  ///
  /// Sets up structured logging with appropriate levels:
  /// - ERROR: Critical failures
  /// - WARN: Invalid responses, API issues
  /// - INFO: Request/response summaries
  /// - DEBUG: Full request/response content
  #[cfg(feature = "logging")]
  pub fn init_logging() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter =
      EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("yanshi_llm=info"));

    fmt()
      .with_env_filter(filter)
      .with_target(false)
      .with_thread_ids(false)
      .with_file(false)
      .with_line_number(false)
      .init();

    Ok(())
  }

  /// Initialize logging (no-op when logging feature is disabled)
  #[cfg(not(feature = "logging"))]
  pub fn init_logging() -> Result<()> {
    println!("[Yanshi] Logging feature not enabled. Use --features logging to enable.");
    Ok(())
  }

  /// Fetch models from all supported vendors
  ///
  /// Returns a HashMap where keys are vendor names and values are lists of discovered models.
  /// Only vendors that support model list fetching will be included.
  ///
  /// Example:
  /// ```ignore
  /// let models = Yanshi::fetch_all_models().await?;
  /// for (vendor, model_list) in models {
  ///   println!("Vendor {}: {} models", vendor, model_list.len());
  /// }
  /// ```
  pub async fn fetch_all_models() -> std::result::Result<
    std::collections::HashMap<String, Vec<discovery::DiscoveredModel>>,
    LLMError,
  > {
    let fetcher = discovery::ModelFetcher::new()?;
    Ok(fetcher.fetch_all_models().await)
  }

  /// Fetch models from a specific vendor
  ///
  /// Example:
  /// ```ignore
  /// let moonshot_models = Yanshi::fetch_vendor_models("moonshot").await?;
  /// println!("Found {} MoonShot models", moonshot_models.len());
  /// ```
  pub async fn fetch_vendor_models(vendor: &str) -> Result<Vec<discovery::DiscoveredModel>> {
    let fetcher = discovery::ModelFetcher::new()?;
    fetcher.fetch_models_by_vendor_name(vendor).await
  }

  /// Validate all models in the current configuration
  ///
  /// This checks if user-specified models actually exist in the vendor APIs.
  /// Returns a validation result with details about valid/invalid models.
  ///
  /// Example:
  /// ```ignore
  /// Yanshi::init().await?;
  /// let result = Yanshi::validate_models().await?;
  /// println!("Validation report:\n{}", result.create_report());
  /// ```
  pub async fn validate_models() -> Result<discovery::model_validator::ValidationResult> {
    let registry = ModelRegistry::global();
    let config = registry.get_config().await?;
    let validator = discovery::ModelValidator::new()?;
    Ok(validator.validate_config(&config).await)
  }

  /// Validate a specific model by name and vendor
  ///
  /// Example:
  /// ```ignore
  /// let is_valid = Yanshi::validate_model("kimi-k2.6", "moonshot").await?;
  /// println!("Model is valid: {}", is_valid);
  /// ```
  pub async fn validate_model(model_name: &str, vendor: &str) -> Result<bool> {
    let validator = discovery::ModelValidator::new()?;
    validator.validate_model(model_name, vendor).await
  }

  /// Update the default models configuration with newly discovered models
  ///
  /// This fetches models from all supported vendors and updates the specified
  /// configuration file. If the file doesn't exist, it will be created.
  ///
  /// Example:
  /// ```ignore
  /// let result = Yanshi::update_models_config("templates/default_models.yml").await?;
  /// println!("Update report:\n{}", result.create_report());
  /// ```
  pub async fn update_models_config(
    config_path: &str,
  ) -> Result<discovery::config_updater::UpdateResult> {
    let updater = discovery::ConfigUpdater::new()?;
    updater.update_default_models(config_path).await
  }

  /// Check if a model exists for a vendor
  ///
  /// Example:
  /// ```ignore
  /// let exists = Yanshi::model_exists("claude-3-5-sonnet-20241022", "anthropic").await?;
  /// println!("Claude 3.5 Sonnet exists: {}", exists);
  /// ```
  pub async fn model_exists(model_id: &str, vendor: &str) -> Result<bool> {
    let fetcher = discovery::ModelFetcher::new()?;
    fetcher.model_exists(vendor, model_id).await
  }

  /// Get information about a specific model if it exists
  ///
  /// Example:
  /// ```ignore
  /// if let Some(model_info) = Yanshi::get_model_info("gpt-4o", "openai").await? {
  ///   println!("Model: {} owned by {}", model_info.id, model_info.owned_by.unwrap_or_default());
  /// }
  /// ```
  pub async fn get_model_info(
    model_id: &str,
    vendor: &str,
  ) -> Result<Option<discovery::DiscoveredModel>> {
    let fetcher = discovery::ModelFetcher::new()?;
    fetcher.get_model_info(vendor, model_id).await
  }

  /// Suggest similar models when a requested model is not found
  ///
  /// Example:
  /// ```ignore
  /// let suggestions = Yanshi::suggest_similar_models("gpt-4-turbo", "openai").await?;
  /// println!("Did you mean one of these? {:?}", suggestions);
  /// ```
  pub async fn suggest_similar_models(target_model: &str, vendor: &str) -> Result<Vec<String>> {
    let validator = discovery::ModelValidator::new()?;
    validator.suggest_similar_models(target_model, vendor).await
  }

  // P-LLM.4 removed:
  // - `stepfun_client` / `stepfun_client_with_base_url`
  // - `text2image(model, prompt)` returning `Text2ImageBuilder`
  // - `text_to_speech(model, input, voice)` returning `TTSBuilder`
  //
  // These directly handed callers a StepFun-internal builder, bypassing
  // the modality dispatcher. Replaced by `Yanshi::asr` / `tts` /
  // `text2image_for` / `image2image` / `image_edit` below, which route
  // through the registry by model name and return a vendor-agnostic
  // trait object.

  // -------------------------------------------------------------
  // P-LLM.2: per-modality registry dispatchers
  //
  // Each method looks up `model_name` in the global registry,
  // asserts its declared `type` matches the requested modality,
  // resolves the vendor's API key, and returns a boxed trait
  // object from `providers::modality`. Used by `yanshi-nodes`
  // multimodal nodes after P-LLM.3 wires them up.
  // -------------------------------------------------------------

  /// Build a boxed [`AsrProvider`] for `model_name`.
  pub async fn asr(model_name: &str) -> Result<Box<dyn AsrProvider>> {
    modality_dispatch::asr_provider(model_name).await
  }

  /// Build a boxed [`TtsProvider`] for `model_name`.
  pub async fn tts(model_name: &str) -> Result<Box<dyn TtsProvider>> {
    modality_dispatch::tts_provider(model_name).await
  }

  /// Build a boxed [`Text2ImageProvider`] for `model_name`.
  pub async fn text2image_for(model_name: &str) -> Result<Box<dyn Text2ImageProvider>> {
    modality_dispatch::text2image_provider(model_name).await
  }

  /// Build a boxed [`Image2ImageProvider`] for `model_name`.
  pub async fn image2image(model_name: &str) -> Result<Box<dyn Image2ImageProvider>> {
    modality_dispatch::image2image_provider(model_name).await
  }

  /// Build a boxed [`ImageEditProvider`] for `model_name`.
  pub async fn image_edit(model_name: &str) -> Result<Box<dyn ImageEditProvider>> {
    modality_dispatch::image_edit_provider(model_name).await
  }

  /// Build a boxed [`Text2VideoProvider`] for `model_name`. Unlike the
  /// other modality entry points above, the returned provider is an
  /// async job API — see [`Text2VideoProvider::submit`]/[`Text2VideoProvider::poll`]/
  /// [`Text2VideoProvider::generate_and_wait`].
  pub async fn text2video_for(model_name: &str) -> Result<Box<dyn Text2VideoProvider>> {
    modality_dispatch::text2video_provider(model_name).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_yanshi_model_builder() {
    let builder = Yanshi::model("gpt-4o");
    // Just test that the builder is created successfully
    // The actual functionality is tested in integration tests
    drop(builder); // Explicit drop to show the test passed
  }
}
