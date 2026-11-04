use crate::{
  LLMError, Result,
  model_types::{InputType, ModelCapabilities, ModelType, OutputType},
  thinking::ThinkingKind,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Configuration for a specific model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
  /// The vendor/provider name (e.g., "openai", "anthropic", "google")
  pub vendor: String,

  /// The model type (granular classification with input/output types).
  ///
  /// Canonical (post P-LLM.0) values:
  ///   - `chat` for all chat-shaped text-reasoning models
  ///     (regardless of whether they accept image / audio / video
  ///     input — that's expressed via `accepts`)
  ///   - `embedding`
  ///   - `text_to_image` / `image_to_image` / `image_edit`
  ///   - `text_to_video`
  ///   - `tts` / `asr`
  ///
  /// Legacy values still parsed for backward compatibility:
  /// `text`, `multimodal`, `imageunderstand`, `videounderstand`,
  /// `docunderstand`, `codegen`, `functioncalling` (all → `chat`),
  /// `generateimage` (→ `text_to_image`),
  /// `image` (→ `image_to_image`),
  /// `editimage` (→ `image_edit`).
  pub r#type: Option<String>,

  /// Input modalities this model accepts (`text` / `image` / `audio` /
  /// `video` / `document`).
  ///
  /// When `Some`, this is the authoritative source for what the model
  /// can ingest — `ModelConfig::accepts()` returns it directly.
  /// When `None`, the value is inferred from `granular_type()` (e.g.,
  /// `Chat` defaults to `[text]`, `Asr` to `[audio]`, etc.).
  ///
  /// Use this to distinguish e.g. GPT-4o (`type: chat,
  /// accepts: [text, image]`) from DeepSeek-Chat (`type: chat,
  /// accepts: [text]`) — both are chat-shaped but only the former
  /// can be wired into vision nodes.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub accepts: Option<Vec<InputType>>,

  /// Output modalities this model can produce (`text` / `image` / `audio`
  /// / `video` / `vector` / `function_call`).
  ///
  /// P-LLM2.6: reserved schema field, mirroring `accepts` on the output
  /// side. When `Some`, this is the authoritative source —
  /// `ModelConfig::outputs()` returns it directly. When `None`, the value
  /// is inferred from `granular_type().primary_output()` (e.g. `Chat`
  /// defaults to `[text]`, `Text2Image` to `[image]`).
  ///
  /// Exists for chat models whose responses are *not* single-modality —
  /// e.g. `gemini-2.0-flash-preview-image-generation` returns inline
  /// image parts alongside text in an ordinary chat turn, so `type: chat`
  /// alone (→ `primary_output() == Text`) undersells what the model
  /// actually emits. Declaring `outputs: [text, image]` records that
  /// distinction in the registry today; **this field is metadata only —
  /// no provider response parser yet constructs `ContentType::Image` for
  /// a chat response** (`ContentType::{Image,Audio,Mixed}` are fully
  /// modeled but currently unconstructed dead variants — see
  /// `providers::ContentType`). Wiring that parsing up is separate,
  /// larger follow-on work; this field just lets a caller ask "does this
  /// model produce image output at all" without it.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub outputs: Option<Vec<OutputType>>,

  /// Detailed model capabilities (computed from type)
  #[serde(skip_serializing_if = "Option::is_none")]
  pub capabilities: Option<ModelCapabilities>,

  /// The actual model ID used by the provider API (optional, defaults to the config key)
  pub model_id: Option<String>,

  /// Base URL for the API (optional, uses provider default if not specified)
  pub base_url: Option<String>,

  /// Default temperature for this model
  pub temperature: Option<f32>,

  /// Default top_p for this model
  pub top_p: Option<f32>,

  /// Default max_tokens for this model
  pub max_tokens: Option<u32>,

  /// Default frequency penalty for this model
  pub frequency_penalty: Option<f32>,

  /// Default stop sequences for this model
  pub stop: Option<Vec<String>>,

  /// Default number of responses to generate
  pub n: Option<u32>,

  /// Whether streaming is supported for this model
  pub supports_streaming: Option<bool>,

  /// Whether this model supports function calling/tools (any path).
  pub supports_tools: Option<bool>,

  /// Whether this model supports provider-native tool calling
  /// (OpenAI `tool_calls`, Anthropic `tool_use`, Google `functionCall`).
  ///
  /// When `false` or unset, callers fall back to prompt-based ReAct.
  pub native_tool_calling: Option<bool>,

  /// Whether this model supports multimodal input (images)
  pub supports_multimodal: Option<bool>,

  /// Whether this model supports the extended-reasoning / "thinking" knob
  /// (Anthropic `thinking` block, OpenAI `reasoning_effort`, Google
  /// `thinkingConfig`, DeepSeek-R1 `reasoning_content`).
  ///
  /// When `true`, callers can pass [`crate::ThinkingConfig`] via
  /// `LLMClientBuilder::thinking(...)` and the provider adapter will emit
  /// the native wire shape. When unset/`false`, calling `.thinking(...)`
  /// fails fast with [`LLMError::UnsupportedFeature`] — silent drops on
  /// the provider side are the kind of "I configured X but didn't get X"
  /// bug we want to avoid.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub supports_thinking: Option<bool>,

  /// How this model accepts the thinking/reasoning knob on the wire.
  ///
  /// Must be set when `supports_thinking: true`. Each variant maps to a
  /// distinct provider serialisation branch — see
  /// [`crate::ThinkingKind`] for the full mapping.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub thinking_kind: Option<ThinkingKind>,

  /// Response format configuration (e.g., "json_object")
  pub response_format: Option<String>,

  /// Additional model-specific parameters
  #[serde(flatten)]
  pub additional_params: HashMap<String, serde_json::Value>,
}

impl ModelConfig {
  /// Get the model type, defaulting to "text" if not specified
  pub fn model_type(&self) -> &str {
    self.r#type.as_deref().unwrap_or("text")
  }

  /// Get the granular model type enum.
  ///
  /// Recognises both the canonical post-P-LLM.0 type strings
  /// (`chat`, `text_to_image`, `image_to_image`, `image_edit`,
  /// `text_to_video`, `embedding`, `tts`, `asr`) and the historical
  /// pre-P-LLM.0 strings (`text`, `multimodal`, `imageunderstand`,
  /// `videounderstand`, `docunderstand`, `codegen`, `functioncalling`
  /// → all collapse to `Chat` semantics; `generateimage` →
  /// `Text2Image`; `image` → `Image2Image`; `editimage` → `ImageEdit`).
  pub fn granular_type(&self) -> ModelType {
    // The full string-to-variant table is centralised in
    // `ModelType::from(&str)`. Keep this method as the historical
    // entry point so external callers keep working.
    ModelType::from(self.model_type())
  }

  /// Input modalities this model accepts.
  ///
  /// Returns the explicit `accepts` field when set; otherwise derives
  /// the default from `granular_type()` (e.g., a `Chat` model with no
  /// explicit `accepts` returns `[Text]`; an `Asr` model returns
  /// `[Audio]`).
  ///
  /// Callers that need to filter models by input capability — e.g.,
  /// "any chat model that takes image input" — should use this rather
  /// than `granular_type()` directly.
  pub fn accepts(&self) -> std::collections::HashSet<InputType> {
    if let Some(explicit) = self.accepts.as_ref() {
      return explicit.iter().cloned().collect();
    }
    self.granular_type().supported_inputs()
  }

  /// Output modalities this model can produce.
  ///
  /// Returns the explicit `outputs` field when set; otherwise derives the
  /// default from `granular_type().primary_output()` (e.g. a `Chat` model
  /// with no explicit `outputs` returns `[Text]`). See the `outputs` field
  /// doc comment for why this exists and its current scope (metadata
  /// only — see that comment for what's *not* wired up yet).
  pub fn outputs(&self) -> std::collections::HashSet<OutputType> {
    if let Some(explicit) = self.outputs.as_ref() {
      return explicit.iter().cloned().collect();
    }
    std::iter::once(self.granular_type().primary_output()).collect()
  }

  /// Get or compute model capabilities
  pub fn get_capabilities(&self) -> ModelCapabilities {
    if let Some(ref capabilities) = self.capabilities {
      capabilities.clone()
    } else {
      // Compute capabilities from model type and config
      let mut capabilities = ModelCapabilities::from_model_type(self.granular_type());

      // Authoritative `accepts` lives on ModelConfig — copy it into
      // capabilities so downstream consumers (`llm_client.rs`,
      // `validate_request`) see the right set. Explicit `accepts:
      // [...]` wins; `supports_multimodal: true` legacy field adds
      // Image as a tolerated input for callers that haven't migrated.
      let accepts = self.accepts();
      capabilities.accepts = accepts;
      if self.supports_multimodal.unwrap_or(false) {
        capabilities.accepts.insert(InputType::Image);
      }

      // P-LLM2.6: same overlay for the output side — see `outputs`'s doc
      // comment for scope (metadata only, no parser wired to it yet).
      capabilities.outputs = self.outputs();

      // Override with explicit config values
      if let Some(streaming) = self.supports_streaming {
        capabilities.supports_streaming = streaming;
      }
      if let Some(tools) = self.supports_tools {
        capabilities.supports_tools = tools;
      }
      if let Some(native) = self.native_tool_calling {
        capabilities.native_tool_calling = native;
      }
      if let Some(max_tokens) = self.max_tokens {
        capabilities.max_output_tokens = Some(max_tokens);
      }

      capabilities
    }
  }

  /// Whether this model is configured for provider-native tool calling.
  ///
  /// `false` means callers must fall back to prompt-based ReAct.
  pub fn supports_native_tool_calling(&self) -> bool {
    self.get_capabilities().native_tool_calling
  }

  /// Whether this model supports the extended-reasoning ("thinking") knob.
  ///
  /// Returns `true` only when the registry entry has `supports_thinking:
  /// true` AND `thinking_kind` set. The two-field requirement is
  /// intentional: a `supports_thinking: true` entry without a
  /// `thinking_kind` is misconfigured and treated as unsupported so the
  /// caller hits a hard error instead of a silent drop.
  pub fn supports_thinking(&self) -> bool {
    self.supports_thinking.unwrap_or(false) && self.thinking_kind.is_some()
  }

  /// How this model accepts the thinking knob. `None` when the model is
  /// not configured for thinking; see [`Self::supports_thinking`].
  pub fn thinking_kind(&self) -> Option<ThinkingKind> {
    if self.supports_thinking.unwrap_or(false) {
      self.thinking_kind
    } else {
      None
    }
  }

  /// Check if this is a multimodal model (legacy method for backward compatibility)
  pub fn is_multimodal(&self) -> bool {
    self.get_capabilities().is_multimodal() || self.supports_multimodal.unwrap_or(false)
  }

  /// Check if this is an image generation model
  pub fn is_image_model(&self) -> bool {
    matches!(
      self.granular_type(),
      ModelType::Text2Image | ModelType::Image2Image | ModelType::ImageEdit
    )
  }

  /// Check if this is an audio model
  pub fn is_audio_model(&self) -> bool {
    matches!(self.granular_type(), ModelType::Tts | ModelType::Asr)
  }

  /// Check if this is a text-to-speech model
  pub fn is_tts_model(&self) -> bool {
    matches!(self.granular_type(), ModelType::Tts)
  }

  /// Check if this model supports the given input type
  pub fn supports_input_type(&self, input_type: &InputType) -> bool {
    self.get_capabilities().supports_input(input_type)
  }

  /// Check if this model supports the given content type (legacy method)
  pub fn supports_content_type(&self, content_type: &str) -> bool {
    let input_type = match content_type {
      "text" => InputType::Text,
      "image" => InputType::Image,
      "audio" => InputType::Audio,
      "video" => InputType::Video,
      "document" => InputType::Document,
      _ => return false,
    };

    self.supports_input_type(&input_type)
  }

  /// Validate a request against this model's capabilities
  #[allow(clippy::too_many_arguments)]
  pub fn validate_request(
    &self,
    has_text: bool,
    has_images: bool,
    has_audio: bool,
    has_video: bool,
    has_document: bool,
    requires_streaming: bool,
    uses_tools: bool,
  ) -> Result<()> {
    let capabilities = self.get_capabilities();

    capabilities
      .validate_request(
        has_text,
        has_images,
        has_audio,
        has_video,
        has_document,
        requires_streaming,
        uses_tools,
      )
      .map_err(|msg| LLMError::InvalidModelConfig { message: msg })
  }

  /// Get supported input types for this model
  pub fn supported_inputs(&self) -> std::collections::HashSet<InputType> {
    self.granular_type().supported_inputs()
  }

  /// Get the primary output type for this model
  pub fn primary_output(&self) -> crate::model_types::OutputType {
    self.granular_type().primary_output()
  }

  /// Check if this model supports streaming
  pub fn supports_streaming_capability(&self) -> bool {
    self.get_capabilities().supports_streaming
  }

  /// Check if this model requires streaming (no non-streaming mode)
  pub fn requires_streaming(&self) -> bool {
    self.get_capabilities().requires_streaming
  }

  /// Check if this model supports tools/function calling
  pub fn supports_tools_capability(&self) -> bool {
    self.get_capabilities().supports_tools
  }

  /// Get a description of what this model does
  pub fn description(&self) -> &'static str {
    self.granular_type().description()
  }

  /// Get typical use cases for this model
  pub fn use_cases(&self) -> Vec<&'static str> {
    self.granular_type().use_cases()
  }
}

/// Configuration for a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
  /// Environment variable name for the API key
  pub api_key_env: String,

  /// Default base URL for this provider
  pub base_url: Option<String>,

  /// Default timeout in seconds
  pub timeout_seconds: Option<u64>,

  /// Rate limiting configuration
  pub rate_limit: Option<RateLimitConfig>,
}

/// P-LLM2.7: this used to be parsed, validated (`requests_per_minute != 0`
/// only, in `config::validation`), and then discarded at
/// `ModelRegistry::initialize_providers` — nothing consumed it anywhere in
/// the crate. `requests_per_minute` is now enforced: `ModelRegistry`
/// builds one `governor` token bucket per vendor from it, and
/// `LLMClient::execute`/`execute_full`/`execute_streaming` wait on it
/// before dispatching (see `ModelRegistry::get_rate_limiter`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
  pub requests_per_minute: u32,
  /// **Not enforced.** Throttling by token volume (rather than request
  /// count) needs a pre-call token estimate, which is only solid for the
  /// OpenAI-BPE-family vendors today (`tokenizer.rs`) — Anthropic/Google
  /// fall back to a heuristic counter there, so enforcing this uniformly
  /// across all 9 vendors would silently under- or over-throttle some of
  /// them. Parsed and kept for forward compatibility (a future TPM
  /// limiter can read it once a good enough per-vendor token estimate
  /// exists) rather than removed.
  pub tokens_per_minute: Option<u32>,
}

/// Main configuration structure for all LLM models and providers
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LLMConfig {
  /// Model configurations keyed by model name.
  ///
  /// `#[serde(default)]` so a `providers:`/`defaults:`-only file (the
  /// slimmed `templates/default_models.yml`, models split out to
  /// `templates/models/*.yml`) deserializes without an explicit empty
  /// `models: {}` key.
  #[serde(default)]
  pub models: HashMap<String, ModelConfig>,

  /// Provider configurations keyed by provider name
  #[serde(default)]
  pub providers: HashMap<String, ProviderConfig>,

  /// Global defaults
  #[serde(default)]
  pub defaults: GlobalDefaults,
}

/// Environment variable that overrides the default model configuration path.
pub const MODELS_CONFIG_ENV: &str = "YANSHI_MODELS_CONFIG";

/// Source used when loading the default LLM model configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LLMConfigSource {
  /// Stable source kind for diagnostics and machine-readable output.
  pub kind: LLMConfigSourceKind,

  /// File path for user-provided sources. Built-in defaults have no path.
  pub path: Option<PathBuf>,

  /// Non-fatal diagnostics collected during source resolution.
  pub warnings: Vec<String>,
}

impl LLMConfigSource {
  /// Human-readable description of the selected source.
  pub fn display_path(&self) -> String {
    self
      .path
      .as_ref()
      .map(|path| path.display().to_string())
      .unwrap_or_else(|| "built-in default_models.yml".to_string())
  }

  /// Returns true when the selected source is a file on disk.
  pub fn is_file(&self) -> bool {
    self.path.is_some()
  }
}

/// Stable kind for default model configuration resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LLMConfigSourceKind {
  EnvOverride,
  UserModelsYml,
  UserModelsYaml,
  /// A directory-based config root laid out the way
  /// [`crate::config::VendorConfigManager`] expects: `<dir>/config.yml`
  /// (optional — providers/defaults) + `<dir>/models/*.yml` (one file per
  /// vendor). `LLMConfigSource::path` is the directory itself, not a file.
  UserModelsDir,
  BuiltInDefault,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalDefaults {
  pub timeout_seconds: Option<u64>,
  pub max_retries: Option<u32>,
  pub retry_delay_ms: Option<u64>,
}

impl LLMConfig {
  /// Resolve the model configuration source using Yanshi's default priority:
  ///
  /// 1. `YANSHI_MODELS_CONFIG`
  /// 2. `~/.yanshi/models.yml`
  /// 3. `~/.yanshi/models.yaml`
  /// 4. Built-in defaults bundled in the crate
  pub fn resolve_default_source() -> Result<LLMConfigSource> {
    let config_dir = dirs::home_dir().map(|home| home.join(".yanshi"));
    Self::resolve_default_source_from(config_dir.as_deref(), env::var_os(MODELS_CONFIG_ENV))
  }

  /// Resolve a model configuration source from explicit inputs.
  ///
  /// This is public so CLI commands and tests can use the same precedence rules
  /// without mutating process-global environment variables.
  pub fn resolve_default_source_from(
    config_dir: Option<&Path>,
    env_override: Option<OsString>,
  ) -> Result<LLMConfigSource> {
    if let Some(path) = env_override.filter(|value| !value.is_empty()) {
      let path = PathBuf::from(path);
      // Directory-based override: `YANSHI_MODELS_CONFIG` pointed at a
      // `VendorConfigManager`-shaped directory (`<dir>/models/*.yml`)
      // rather than a single file.
      if path.join("models").is_dir() {
        return Ok(LLMConfigSource {
          kind: LLMConfigSourceKind::UserModelsDir,
          path: Some(path),
          warnings: Vec::new(),
        });
      }
      return Ok(LLMConfigSource {
        kind: LLMConfigSourceKind::EnvOverride,
        path: Some(path),
        warnings: Vec::new(),
      });
    }

    if let Some(config_dir) = config_dir {
      let yml_path = config_dir.join("models.yml");
      let yaml_path = config_dir.join("models.yaml");
      let yml_exists = yml_path.exists();
      let yaml_exists = yaml_path.exists();

      if yml_exists {
        let mut warnings = Vec::new();
        if yaml_exists {
          warnings.push(format!(
            "Both '{}' and '{}' exist; using '{}'",
            yml_path.display(),
            yaml_path.display(),
            yml_path.display()
          ));
        }
        return Ok(LLMConfigSource {
          kind: LLMConfigSourceKind::UserModelsYml,
          path: Some(yml_path),
          warnings,
        });
      }

      if yaml_exists {
        return Ok(LLMConfigSource {
          kind: LLMConfigSourceKind::UserModelsYaml,
          path: Some(yaml_path),
          warnings: Vec::new(),
        });
      }

      // Directory-based config under the user's own config dir, e.g.
      // `~/.yanshi/models/*.yml` (+ optional `~/.yanshi/config.yml`
      // for providers/defaults) — only consulted once neither single-file
      // form above is present, so existing single-file setups are
      // unaffected.
      if config_dir.join("models").is_dir() {
        return Ok(LLMConfigSource {
          kind: LLMConfigSourceKind::UserModelsDir,
          path: Some(config_dir.to_path_buf()),
          warnings: Vec::new(),
        });
      }
    }

    Ok(LLMConfigSource {
      kind: LLMConfigSourceKind::BuiltInDefault,
      path: None,
      warnings: Vec::new(),
    })
  }

  /// Load configuration from an already-resolved [`LLMConfigSource`].
  ///
  /// Centralises the per-kind loading strategy: file-based sources
  /// (`EnvOverride` / `UserModelsYml` / `UserModelsYaml`) go through
  /// [`Self::from_file`]; `UserModelsDir` routes through
  /// [`crate::config::VendorConfigManager`] instead (disk-based,
  /// vendor-file-per-directory loading — the first production caller of
  /// that manager, previously exercised only by its own tests);
  /// `BuiltInDefault` uses the compile-time bundled config.
  pub async fn from_source(source: &LLMConfigSource) -> Result<Self> {
    match source.kind {
      LLMConfigSourceKind::BuiltInDefault => crate::config::builtin_default_config(),
      LLMConfigSourceKind::UserModelsDir => {
        let dir = source
          .path
          .as_ref()
          .ok_or_else(|| LLMError::ConfigurationError {
            message: "UserModelsDir source is missing its directory path".to_string(),
          })?;
        crate::config::VendorConfigManager::new(dir)
          .load_config()
          .await
      }
      LLMConfigSourceKind::EnvOverride
      | LLMConfigSourceKind::UserModelsYml
      | LLMConfigSourceKind::UserModelsYaml => {
        let path = source
          .path
          .as_ref()
          .ok_or_else(|| LLMError::ConfigurationError {
            message: "file-based config source is missing its path".to_string(),
          })?;
        Self::from_file(path).await
      }
    }
  }

  /// Load configuration from a YAML file
  pub async fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
    let content =
      tokio::fs::read_to_string(path)
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to read config file: {}", e),
        })?;

    Self::from_yaml(&content)
  }

  /// Parse configuration from YAML string
  pub fn from_yaml(yaml_content: &str) -> Result<Self> {
    serde_yaml::from_str(yaml_content).map_err(|e| LLMError::ConfigurationError {
      message: format!("Failed to parse YAML config: {}", e),
    })
  }

  /// Load configuration from the default resolved source.
  pub async fn from_default_source() -> Result<(Self, LLMConfigSource)> {
    let source = Self::resolve_default_source()?;
    let config = Self::from_source(&source).await?;
    Ok((config, source))
  }

  /// Get a model configuration by name
  pub fn get_model(&self, model_name: &str) -> Result<&ModelConfig> {
    self
      .models
      .get(model_name)
      .ok_or_else(|| LLMError::ModelNotFound {
        model_name: model_name.to_string(),
      })
  }

  /// Get a provider configuration by name
  pub fn get_provider(&self, provider_name: &str) -> Option<&ProviderConfig> {
    self.providers.get(provider_name)
  }

  /// Get API key for a provider from environment variables
  pub fn get_api_key(&self, provider_name: &str) -> Result<String> {
    if provider_name.eq_ignore_ascii_case("mock") {
      return Ok("mock".to_string());
    }

    // First try provider-specific config
    if let Some(provider_config) = self.get_provider(provider_name)
      && let Ok(api_key) = env::var(&provider_config.api_key_env)
    {
      return Ok(api_key);
    }

    // Fallback to common environment variable patterns
    let common_env_vars = match provider_name.to_lowercase().as_str() {
      "openai" => vec!["OPENAI_API_KEY", "OPENAI_KEY"],
      "anthropic" => vec!["ANTHROPIC_API_KEY", "ANTHROPIC_KEY", "CLAUDE_API_KEY"],
      "google" | "gemini" => vec!["GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_AI_KEY"],
      "moonshot" => vec!["MOONSHOT_API_KEY", "MOONSHOT_KEY"],
      "stepfun" | "step" => vec!["STEPFUN_API_KEY", "STEP_API_KEY"],
      "glm" | "bigmodel" | "zhipu" => vec!["GLM_API_KEY", "BIGMODEL_API_KEY", "ZHIPU_API_KEY"],
      "dashscope" => vec!["DASHSCOPE_API_KEY"],
      "deepseek" => vec!["DEEPSEEK_API_KEY"],
      "minimax" => vec!["MINIMAX_API_KEY"],
      "kling" => vec!["KLING_API_KEY"],
      _ => vec![],
    };

    for env_var in common_env_vars {
      if let Ok(api_key) = env::var(env_var) {
        return Ok(api_key);
      }
    }

    Err(LLMError::MissingApiKey {
      provider: provider_name.to_string(),
    })
  }

  /// Validate the configuration's structural correctness.
  ///
  /// **Lenient on missing API keys** (P10.3.1): models whose vendor's
  /// API key env var is unset emit an `eprintln!` warning naming the
  /// affected models so the user knows what's being skipped, but do
  /// NOT cause this method to return `Err`. The fail-fast moves to
  /// the lookup path — `ModelRegistry::get_provider(name)` returns
  /// [`LLMError::MissingApiKey`] only when the user actually requests
  /// a model whose provider key is missing.
  ///
  /// This lets a fresh user with only `OPENAI_API_KEY` set call
  /// `Yanshi::init()` against the bundled `default_models.yml`
  /// (which references ~9 providers) without fail-closing on the 8
  /// unset keys.
  ///
  /// Callers that need the old strict behaviour (e.g.,
  /// `yanshi doctor --profile production`) should use
  /// [`LLMConfig::validate_strict`] instead.
  pub fn validate(&self) -> Result<()> {
    self.validate_inner(false)
  }

  /// Strict validation: same as [`LLMConfig::validate`] but ALSO
  /// returns `Err(LLMError::MissingApiKey)` for any provider whose
  /// API key env var is unset. Used by production health checks
  /// (e.g., `yanshi doctor --profile production`) where a missing
  /// key represents a hard misconfiguration rather than a partial
  /// install.
  pub fn validate_strict(&self) -> Result<()> {
    self.validate_inner(true)
  }

  fn validate_inner(&self, strict_api_keys: bool) -> Result<()> {
    // Group models by vendor first so the warning can list affected
    // model names (helps the user understand what they're losing if
    // a key is missing).
    let mut models_by_vendor: HashMap<&str, Vec<&str>> = HashMap::new();
    for (model_name, model_config) in &self.models {
      models_by_vendor
        .entry(model_config.vendor.as_str())
        .or_default()
        .push(model_name.as_str());
    }

    for (model_name, model_config) in &self.models {
      // Check if vendor is supported — always a hard error.
      if ![
        "openai",
        "anthropic",
        "google",
        "gemini",
        "moonshot",
        "dashscope",
        "step",
        "stepfun",
        "glm",
        "bigmodel",
        "zhipu",
        "deepseek",
        "minimax",
        "kling",
        "mock",
      ]
      .contains(&model_config.vendor.as_str())
      {
        return Err(LLMError::UnsupportedProvider {
          provider: model_config.vendor.clone(),
        });
      }

      // Validate model-specific numeric ranges — always hard errors.
      if let Some(temp) = model_config.temperature
        && !(0.0..=2.0).contains(&temp)
      {
        return Err(LLMError::InvalidModelConfig {
          message: format!(
            "Temperature for model '{}' must be between 0.0 and 2.0",
            model_name
          ),
        });
      }

      if let Some(top_p) = model_config.top_p
        && !(0.0..=1.0).contains(&top_p)
      {
        return Err(LLMError::InvalidModelConfig {
          message: format!(
            "top_p for model '{}' must be between 0.0 and 1.0",
            model_name
          ),
        });
      }

      if let Some(freq_penalty) = model_config.frequency_penalty
        && !(0.0..=2.0).contains(&freq_penalty)
      {
        return Err(LLMError::InvalidModelConfig {
          message: format!(
            "frequency_penalty for model '{}' must be between 0.0 and 2.0",
            model_name
          ),
        });
      }

      if let Some(n) = model_config.n
        && (n == 0 || n > 10)
      {
        return Err(LLMError::InvalidModelConfig {
          message: format!("n for model '{}' must be between 1 and 10", model_name),
        });
      }
    }

    // API-key check is deferred until after structural validation
    // so the warning message can group every affected model per
    // provider in one line.
    let mut missing_providers: Vec<&str> = Vec::new();
    for vendor in models_by_vendor.keys() {
      if self.get_api_key(vendor).is_err() {
        missing_providers.push(vendor);
      }
    }
    // Sort for deterministic output ordering across runs.
    missing_providers.sort_unstable();

    if strict_api_keys {
      if let Some(first) = missing_providers.first() {
        return Err(LLMError::MissingApiKey {
          provider: (*first).to_string(),
        });
      }
    } else {
      for vendor in &missing_providers {
        let mut affected = models_by_vendor.get(vendor).cloned().unwrap_or_default();
        affected.sort_unstable();
        eprintln!(
          "Warning: provider '{}' has no API key configured; \
           {} model(s) unavailable until the key is set: {}",
          vendor,
          affected.len(),
          affected.join(", ")
        );
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::env;
  use tempfile::TempDir;

  #[test]
  fn test_model_config_parsing() {
    let yaml = r#"
models:
  gpt-4o:
    vendor: openai
    base_url: "https://api.openai.com/v1/chat/completions"
    temperature: 0.7
    top_p: 0.9
    max_tokens: 4096
    supports_streaming: true
  
  claude-3-sonnet:
    vendor: anthropic
    model_id: "claude-3-sonnet-20240229"
    temperature: 0.5
    max_tokens: 4096

providers:
  openai:
    api_key_env: "OPENAI_API_KEY"
    timeout_seconds: 30
  anthropic:
    api_key_env: "ANTHROPIC_API_KEY"
    timeout_seconds: 60

defaults:
  timeout_seconds: 30
  max_retries: 3
"#;

    let config = LLMConfig::from_yaml(yaml).unwrap();

    assert_eq!(config.models.len(), 2);
    assert_eq!(config.providers.len(), 2);

    let gpt4_config = config.get_model("gpt-4o").unwrap();
    assert_eq!(gpt4_config.vendor, "openai");
    assert_eq!(gpt4_config.temperature, Some(0.7));
    assert_eq!(gpt4_config.supports_streaming, Some(true));

    let claude_config = config.get_model("claude-3-sonnet").unwrap();
    assert_eq!(claude_config.vendor, "anthropic");
    assert_eq!(
      claude_config.model_id,
      Some("claude-3-sonnet-20240229".to_string())
    );
  }

  #[test]
  fn test_api_key_resolution() {
    // SAFETY: this unit test mutates a dedicated test env var before reading it.
    unsafe {
      env::set_var("TEST_OPENAI_KEY", "test-key");
    }

    let yaml = r#"
models:
  gpt-4o:
    vendor: openai

providers:
  openai:
    api_key_env: "TEST_OPENAI_KEY"
"#;

    let config = LLMConfig::from_yaml(yaml).unwrap();
    let api_key = config.get_api_key("openai").unwrap();
    assert_eq!(api_key, "test-key");

    // SAFETY: cleanup of the dedicated test env var after the test read.
    unsafe {
      env::remove_var("TEST_OPENAI_KEY");
    }
  }

  #[test]
  fn test_mock_provider_needs_no_api_key() {
    let yaml = r#"
models:
  mock-model:
    vendor: mock
    type: text
"#;

    let config = LLMConfig::from_yaml(yaml).unwrap();
    assert_eq!(config.get_api_key("mock").unwrap(), "mock");
    assert!(config.validate().is_ok());
  }

  /// P10.3.1: `validate()` is lenient on missing API keys — it
  /// must NOT return Err for a config that references providers
  /// whose keys aren't set in the environment. The warning
  /// (printed to stderr) names the affected models so the user
  /// knows what's being skipped, but the call itself succeeds so
  /// fresh users with only one provider key can still init
  /// against the bundled default registry.
  #[test]
  fn validate_emits_warning_but_no_err_for_missing_api_key() {
    // SAFETY: this unit test mutates a dedicated test env var.
    unsafe {
      env::remove_var("P10_3_1_MISSING_KEY_ENV");
    }
    let yaml = r#"
models:
  some-model:
    vendor: openai
providers:
  openai:
    api_key_env: "P10_3_1_MISSING_KEY_ENV"
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    // The hard contract for P10.3.1: lenient path returns Ok even
    // though the configured api_key_env is unset.
    config
      .validate()
      .expect("validate() must be lenient on missing keys (P10.3.1)");
  }

  /// P10.3.1: `validate_strict()` preserves the old fail-close
  /// behaviour for callers that need it (e.g.,
  /// `yanshi doctor --profile production`). A configured
  /// `api_key_env` that's unset must produce
  /// [`LLMError::MissingApiKey`].
  ///
  /// **Test isolation**: `get_api_key` falls back to provider-
  /// specific common env vars (`ANTHROPIC_API_KEY` /
  /// `DEEPSEEK_API_KEY` / …) when the configured `api_key_env` is
  /// unset, so the test must temporarily clear those fallbacks to
  /// be deterministic regardless of the developer's environment.
  /// `deepseek` is chosen because it has a single fallback
  /// (`DEEPSEEK_API_KEY`), minimising snapshot+restore noise.
  #[test]
  fn validate_strict_returns_missing_api_key_err_when_env_unset() {
    let snapshot_configured = env::var("P10_3_1_STRICT_MISSING_KEY_ENV").ok();
    let snapshot_fallback = env::var("DEEPSEEK_API_KEY").ok();

    // SAFETY: test isolates by clearing the configured slot and
    // the single fallback for `deepseek`, then restores at exit.
    unsafe {
      env::remove_var("P10_3_1_STRICT_MISSING_KEY_ENV");
      env::remove_var("DEEPSEEK_API_KEY");
    }

    let yaml = r#"
models:
  some-model:
    vendor: deepseek
providers:
  deepseek:
    api_key_env: "P10_3_1_STRICT_MISSING_KEY_ENV"
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let result = config.validate_strict();

    // SAFETY: restore env state regardless of assertion outcome.
    unsafe {
      if let Some(value) = snapshot_configured {
        env::set_var("P10_3_1_STRICT_MISSING_KEY_ENV", value);
      }
      if let Some(value) = snapshot_fallback {
        env::set_var("DEEPSEEK_API_KEY", value);
      }
    }

    match result {
      Err(LLMError::MissingApiKey { provider }) => assert_eq!(provider, "deepseek"),
      Err(other) => panic!("expected MissingApiKey, got: {other:?}"),
      Ok(()) => panic!("validate_strict() must err on missing keys (P10.3.1)"),
    }
  }

  /// P10.3.1 regression guard: structural checks (unsupported
  /// vendor, out-of-range numeric fields) must STILL be hard
  /// errors in the lenient path. The leniency only relaxes the
  /// API-key check.
  #[test]
  fn validate_still_errors_on_unsupported_vendor() {
    let yaml = r#"
models:
  bad-model:
    vendor: definitely-not-a-real-provider
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let err = config
      .validate()
      .expect_err("unsupported vendor must remain a hard error");
    assert!(matches!(err, LLMError::UnsupportedProvider { .. }));
  }

  #[test]
  fn validate_still_errors_on_invalid_temperature() {
    let yaml = r#"
models:
  hot-model:
    vendor: mock
    temperature: 3.0
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let err = config
      .validate()
      .expect_err("out-of-range temperature must remain a hard error");
    assert!(matches!(err, LLMError::InvalidModelConfig { .. }));
  }

  #[test]
  fn resolves_models_yml_before_legacy_yaml() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path();
    std::fs::write(config_dir.join("models.yml"), "models: {}\n").unwrap();
    std::fs::write(config_dir.join("models.yaml"), "models: {}\n").unwrap();

    let source = LLMConfig::resolve_default_source_from(Some(config_dir), None).unwrap();

    assert_eq!(source.kind, LLMConfigSourceKind::UserModelsYml);
    assert_eq!(source.path.as_ref(), Some(&config_dir.join("models.yml")));
    assert_eq!(source.warnings.len(), 1);
  }

  #[test]
  fn resolves_legacy_models_yaml_when_yml_is_absent() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path();
    std::fs::write(config_dir.join("models.yaml"), "models: {}\n").unwrap();

    let source = LLMConfig::resolve_default_source_from(Some(config_dir), None).unwrap();

    assert_eq!(source.kind, LLMConfigSourceKind::UserModelsYaml);
    assert_eq!(source.path.as_ref(), Some(&config_dir.join("models.yaml")));
    assert!(source.warnings.is_empty());
  }

  #[test]
  fn resolves_env_override_before_user_config() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path();
    let override_path = config_dir.join("override.yml");
    std::fs::write(config_dir.join("models.yml"), "models: {}\n").unwrap();

    let source = LLMConfig::resolve_default_source_from(
      Some(config_dir),
      Some(override_path.clone().into_os_string()),
    )
    .unwrap();

    assert_eq!(source.kind, LLMConfigSourceKind::EnvOverride);
    assert_eq!(source.path.as_ref(), Some(&override_path));
  }

  #[test]
  fn chat_type_string_parses_to_chat_shaped_model_type() {
    let yaml = r#"
models:
  gpt-4o:
    vendor: openai
    type: chat
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let model = config.get_model("gpt-4o").unwrap();
    assert_eq!(model.granular_type(), ModelType::Chat);
  }

  #[test]
  fn legacy_type_aliases_still_parse() {
    // Pre-P-LLM.0 YAML used three labels for chat-shaped models; all
    // three must keep working so existing model registries don't break.
    let yaml = r#"
models:
  text-model:
    vendor: openai
    type: text
  multimodal-model:
    vendor: openai
    type: multimodal
  vision-model:
    vendor: stepfun
    type: imageunderstand
  legacy-gen-image:
    vendor: stepfun
    type: generateimage
  legacy-edit-image:
    vendor: stepfun
    type: editimage
  legacy-imagen:
    vendor: google
    type: image
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    // Post-P-LLM.0 Slice 3: all three chat-shaped legacy labels
    // collapse onto `Chat`. Distinguishing image-capable from text-
    // only is the job of `accepts`, not of the variant.
    assert_eq!(
      config.get_model("text-model").unwrap().granular_type(),
      ModelType::Chat
    );
    assert_eq!(
      config
        .get_model("multimodal-model")
        .unwrap()
        .granular_type(),
      ModelType::Chat
    );
    assert_eq!(
      config.get_model("vision-model").unwrap().granular_type(),
      ModelType::Chat
    );
    assert_eq!(
      config
        .get_model("legacy-gen-image")
        .unwrap()
        .granular_type(),
      ModelType::Text2Image
    );
    assert_eq!(
      config
        .get_model("legacy-edit-image")
        .unwrap()
        .granular_type(),
      ModelType::ImageEdit
    );
    // Historical `type: image` carried the "text → image generation"
    // meaning (Google Imagen entries used it pre-P-LLM.0); keep that
    // mapping stable so existing registries don't silently change
    // dispatch class on upgrade.
    assert_eq!(
      config.get_model("legacy-imagen").unwrap().granular_type(),
      ModelType::Text2Image
    );
  }

  #[test]
  fn accepts_field_overrides_inferred_modalities() {
    // Explicit `accepts` must win over what the type alone would imply.
    // This is the mechanism that lets us encode "GPT-4o is chat-shaped
    // AND accepts images" without inventing a separate type.
    let yaml = r#"
models:
  gpt-4o:
    vendor: openai
    type: chat
    accepts: [text, image]
  claude-text-only:
    vendor: anthropic
    type: chat
    accepts: [text]
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();

    let gpt4o = config.get_model("gpt-4o").unwrap();
    let gpt4o_accepts = gpt4o.accepts();
    assert!(gpt4o_accepts.contains(&InputType::Text));
    assert!(gpt4o_accepts.contains(&InputType::Image));
    assert_eq!(gpt4o_accepts.len(), 2);

    let claude = config.get_model("claude-text-only").unwrap();
    let claude_accepts = claude.accepts();
    assert!(claude_accepts.contains(&InputType::Text));
    assert!(!claude_accepts.contains(&InputType::Image));
    assert_eq!(claude_accepts.len(), 1);
  }

  /// P-LLM2.6: mirrors `accepts_field_overrides_inferred_modalities` for
  /// the output side — a chat model that also emits images (e.g. Gemini's
  /// image-generation chat variants) can declare `outputs: [text, image]`
  /// without needing a dedicated `type:`.
  #[test]
  fn outputs_field_overrides_inferred_modalities() {
    let yaml = r#"
models:
  gemini-image-chat:
    vendor: google
    type: chat
    outputs: [text, image]
  plain-chat:
    vendor: openai
    type: chat
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();

    let image_chat = config.get_model("gemini-image-chat").unwrap();
    let image_chat_outputs = image_chat.outputs();
    assert!(image_chat_outputs.contains(&OutputType::Text));
    assert!(image_chat_outputs.contains(&OutputType::Image));
    assert_eq!(image_chat_outputs.len(), 2);

    let plain = config.get_model("plain-chat").unwrap();
    let plain_outputs = plain.outputs();
    assert!(plain_outputs.contains(&OutputType::Text));
    assert_eq!(plain_outputs.len(), 1);
  }

  /// P-LLM2.6: an unset `outputs` field falls back to
  /// `granular_type().primary_output()` — a `text_to_image` model with no
  /// explicit `outputs` still correctly reports `[Image]`, not `[Text]`.
  #[test]
  fn outputs_falls_back_to_type_default_when_unset() {
    let yaml = r#"
models:
  chat-model:
    vendor: openai
    type: chat
  image-gen-model:
    vendor: openai
    type: text_to_image
  tts-model:
    vendor: openai
    type: tts
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();

    let chat_outputs = config.get_model("chat-model").unwrap().outputs();
    assert_eq!(chat_outputs, [OutputType::Text].into_iter().collect());

    let image_outputs = config.get_model("image-gen-model").unwrap().outputs();
    assert_eq!(image_outputs, [OutputType::Image].into_iter().collect());

    let tts_outputs = config.get_model("tts-model").unwrap().outputs();
    assert_eq!(tts_outputs, [OutputType::Audio].into_iter().collect());
  }

  /// `get_capabilities()` must copy the resolved `outputs()` set onto
  /// `ModelCapabilities`, mirroring how it already does for `accepts`.
  #[test]
  fn get_capabilities_copies_outputs_onto_capabilities() {
    let yaml = r#"
models:
  gemini-image-chat:
    vendor: google
    type: chat
    outputs: [text, image]
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let capabilities = config
      .get_model("gemini-image-chat")
      .unwrap()
      .get_capabilities();
    assert!(capabilities.outputs.contains(&OutputType::Text));
    assert!(capabilities.outputs.contains(&OutputType::Image));
    assert_eq!(capabilities.outputs.len(), 2);
  }

  #[test]
  fn bundled_default_models_yaml_uses_post_pllm0_schema() {
    // Snapshot test for the P-LLM.0 Slice 2 migration. Locks down the
    // post-migration shape of the bundled default config so a future
    // edit can't accidentally re-introduce the legacy `text` /
    // `multimodal` / `imageunderstand` / `generateimage` / `editimage` /
    // `image` strings.
    //
    // Reads `builtin_default_config()` (the merged vendor files +
    // slimmed `default_models.yml`), not `templates/default_models.yml`
    // directly — since the T-split moved the actual model entries out to
    // `templates/models/*.yml`, the bare file now carries only
    // `providers:`/`defaults:`.
    let config = crate::config::builtin_default_config().unwrap();

    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut accepts_image_count = 0usize;
    for model in config.models.values() {
      let t = model.model_type();
      *counts
        .entry(Box::leak(t.to_string().into_boxed_str()))
        .or_default() += 1;
      if model.accepts().contains(&InputType::Image) {
        accepts_image_count += 1;
      }
    }

    // No legacy strings should appear.
    for legacy in [
      "multimodal",
      "imageunderstand",
      "generateimage",
      "editimage",
    ] {
      assert!(
        !counts.contains_key(legacy),
        "legacy type label '{legacy}' must be migrated out of default_models.yml; \
         still present in {counts:?}"
      );
    }

    // Canonical chat type dominates the registry.
    let chat_count = counts.get("chat").copied().unwrap_or(0);
    assert!(
      chat_count >= 180,
      "expected ≥ 180 chat-shaped entries after migration, got {chat_count}"
    );

    // Generation/editing types use the new canonical names.
    assert!(counts.get("text_to_image").copied().unwrap_or(0) >= 1);
    assert!(counts.get("image_edit").copied().unwrap_or(0) >= 1);

    // At least the 63 image-accepting chat models surface their
    // capability via the explicit `accepts: [text, image]` field. (Was 65
    // pre the 800dbae StepFun step-1/step-2 retirement migration, which
    // dropped a couple of the retired lineup's vision variants along with
    // the rest of that dead model line.)
    assert!(
      accepts_image_count >= 63,
      "expected ≥ 63 entries with image in accepts, got {accepts_image_count}"
    );
  }

  #[test]
  fn accepts_falls_back_to_type_default_when_unset() {
    // The fallback path is the back-compat seam: a YAML entry written
    // before P-LLM.0 has no `accepts` field, so we derive it from the
    // historic `granular_type().supported_inputs()` behaviour.
    let yaml = r#"
models:
  legacy-text:
    vendor: openai
    type: text
  legacy-asr:
    vendor: openai
    type: asr
  legacy-tts:
    vendor: openai
    type: tts
"#;
    let config = LLMConfig::from_yaml(yaml).unwrap();
    let text_accepts = config.get_model("legacy-text").unwrap().accepts();
    assert!(text_accepts.contains(&InputType::Text));
    assert_eq!(text_accepts.len(), 1);

    let asr_accepts = config.get_model("legacy-asr").unwrap().accepts();
    assert!(asr_accepts.contains(&InputType::Audio));
    assert_eq!(asr_accepts.len(), 1);

    let tts_accepts = config.get_model("legacy-tts").unwrap().accepts();
    assert!(tts_accepts.contains(&InputType::Text));
    assert_eq!(tts_accepts.len(), 1);
  }

  #[test]
  fn resolves_builtin_default_when_no_user_config_exists() {
    let temp = TempDir::new().unwrap();

    let source = LLMConfig::resolve_default_source_from(Some(temp.path()), None).unwrap();

    assert_eq!(source.kind, LLMConfigSourceKind::BuiltInDefault);
    assert!(source.path.is_none());
  }
}
