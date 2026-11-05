//! Live LLM cross-provider consistency suite.
//!
//! Companion to `provider_consistency.rs`. The offline file uses hand-rolled
//! TCP mocks to verify wire-format parsing for each provider; this file makes
//! a single real API call per provider with a minimum-cost model and asserts
//! the same cross-provider contract holds against production endpoints:
//!
//!   * `ContentType::Text` is returned and non-empty.
//!   * `TokenUsage` is populated (`prompt_tokens` / `completion_tokens` /
//!     `total_tokens` all present).
//!   * `StopReason::Stop` is reported on a successful single-turn completion.
//!
//! Tests skip cleanly (no failure) when the global gate env var
//! `YANSHI_LIVE_LLM_TESTS` is not set to a truthy value, and per provider
//! when its API key env var is unset. This means default `cargo test` runs
//! never hit the network, while nightly CI can opt in by setting the env vars
//! from secrets. See `docs/LLM_PROVIDERS_MATRIX.md` for the full design.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::json;
use yanshi_llm::providers::stepfun::{
  ASRRequest, StepFunSpecializedClient, TTSBuilder, Text2ImageBuilder,
};
use yanshi_llm::providers::{
  AnthropicProvider, ContentType, GoogleProvider, LLMProvider, MoonshotProvider, OpenAIProvider,
  ProviderRequest, StepFunProvider, TokenUsage,
};
use yanshi_llm::tool_calling::{StopReason, ToolChoice, ToolSpec};
use yanshi_llm::{LLMConfig, Yanshi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveCapability {
  Llm,
  Multimodal,
  Image,
  Audio,
  Video,
}

impl LiveCapability {
  fn gate_env(self) -> &'static str {
    match self {
      Self::Llm => "YANSHI_LIVE_LLM_TESTS",
      Self::Multimodal => "YANSHI_LIVE_MULTIMODAL_TESTS",
      Self::Image => "YANSHI_LIVE_IMAGE_TESTS",
      Self::Audio => "YANSHI_LIVE_AUDIO_TESTS",
      Self::Video => "YANSHI_LIVE_VIDEO_TESTS",
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct LiveProviderProfile {
  name: &'static str,
  capabilities: &'static [LiveCapability],
}

const LIVE_PROVIDER_PROFILES: &[LiveProviderProfile] = &[
  LiveProviderProfile {
    name: "openai",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Multimodal,
      LiveCapability::Audio,
      LiveCapability::Image,
    ],
  },
  LiveProviderProfile {
    name: "anthropic",
    capabilities: &[LiveCapability::Llm, LiveCapability::Multimodal],
  },
  LiveProviderProfile {
    name: "google",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Multimodal,
      LiveCapability::Image,
      LiveCapability::Audio,
    ],
  },
  LiveProviderProfile {
    name: "moonshot",
    capabilities: &[LiveCapability::Llm],
  },
  LiveProviderProfile {
    name: "stepfun",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Multimodal,
      LiveCapability::Image,
      LiveCapability::Audio,
    ],
  },
  LiveProviderProfile {
    name: "glm",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Multimodal,
      LiveCapability::Image,
    ],
  },
  LiveProviderProfile {
    name: "dashscope",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Image,
      LiveCapability::Audio,
      LiveCapability::Video,
    ],
  },
  LiveProviderProfile {
    name: "deepseek",
    capabilities: &[LiveCapability::Llm],
  },
  LiveProviderProfile {
    name: "minimax",
    capabilities: &[
      LiveCapability::Llm,
      LiveCapability::Audio,
      LiveCapability::Video,
    ],
  },
  // Y6.6: video only (Kling has no chat API).
  LiveProviderProfile {
    name: "kling",
    capabilities: &[LiveCapability::Video],
  },
];

fn provider_supports_capability(provider_name: &str, capability: LiveCapability) -> bool {
  LIVE_PROVIDER_PROFILES
    .iter()
    .find(|profile| profile.name == provider_name)
    .is_some_and(|profile| profile.capabilities.contains(&capability))
}

fn no_proxy_client() -> reqwest::Client {
  reqwest::Client::builder()
    .no_proxy()
    .build()
    .expect("no-proxy reqwest client")
}

fn glm_live_lock() -> &'static tokio::sync::Mutex<()> {
  static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn dashscope_live_lock() -> &'static tokio::sync::Mutex<()> {
  static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Resolve the DashScope OpenAI-compatible endpoint. Honors the bundled
/// `default_models.yml` provider entry (which points at
/// `https://dashscope.aliyuncs.com/compatible-mode/v1`); falls back to the
/// same hard-coded URL when the config is unreadable so the test is robust
/// against `~/.yanshi/models.yml` corruption.
async fn dashscope_base_url() -> Option<String> {
  let Ok((config, _source)) = LLMConfig::from_default_source().await else {
    return Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_string());
  };

  config
    .get_provider("dashscope")
    .and_then(|provider| provider.base_url.clone())
    .or_else(|| Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_string()))
}

async fn dashscope_live_context(capability: LiveCapability) -> Option<(String, Option<String>)> {
  if !prepare_live_provider("dashscope", capability).await {
    return None;
  }

  let Some((env_used, api_key)) = pick_api_key(&["DASHSCOPE_API_KEY"]) else {
    eprintln!("[live] dashscope: skipped (DASHSCOPE_API_KEY not set; live gate is on)");
    return None;
  };
  eprintln!("[live] dashscope: using API key from {env_used}");

  Some((api_key, dashscope_base_url().await))
}

fn deepseek_live_lock() -> &'static tokio::sync::Mutex<()> {
  static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Resolve the DeepSeek OpenAI-compatible endpoint. DeepSeek explicitly
/// supports both `https://api.deepseek.com/v1/chat/completions` and the
/// non-versioned `https://api.deepseek.com/chat/completions`; we use the
/// `/v1` form to match how `OpenAIProvider` appends `/chat/completions`
/// to the base URL. There's no `default_models.yml` provider entry yet,
/// so this is the unconditional fallback.
async fn deepseek_base_url() -> Option<String> {
  Some("https://api.deepseek.com/v1".to_string())
}

async fn deepseek_live_context(capability: LiveCapability) -> Option<(String, Option<String>)> {
  if !prepare_live_provider("deepseek", capability).await {
    return None;
  }

  let Some((env_used, api_key)) = pick_api_key(&["DEEPSEEK_API_KEY"]) else {
    eprintln!("[live] deepseek: skipped (DEEPSEEK_API_KEY not set; live gate is on)");
    return None;
  };
  eprintln!("[live] deepseek: using API key from {env_used}");

  Some((api_key, deepseek_base_url().await))
}

fn minimax_live_lock() -> &'static tokio::sync::Mutex<()> {
  static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Resolve the MiniMax OpenAI-compatible endpoint. Note the host is
/// `api.minimaxi.com` (with an `i`), NOT `api.minimax.com` — the
/// `minimaxi.com` (海螺 AI domain) is the canonical API host. The
/// hardcoded fallback matches what the docs publish.
async fn minimax_base_url() -> Option<String> {
  Some("https://api.minimaxi.com/v1".to_string())
}

async fn minimax_live_context(capability: LiveCapability) -> Option<(String, Option<String>)> {
  if !prepare_live_provider("minimax", capability).await {
    return None;
  }

  let Some((env_used, api_key)) = pick_api_key(&["MINIMAX_API_KEY"]) else {
    eprintln!("[live] minimax: skipped (MINIMAX_API_KEY not set; live gate is on)");
    return None;
  };
  eprintln!("[live] minimax: using API key from {env_used}");

  Some((api_key, minimax_base_url().await))
}

fn env_truthy(value: &str) -> bool {
  let value = value.trim().to_ascii_lowercase();
  matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

fn live_gate_enabled(capability: LiveCapability) -> bool {
  std::env::var(capability.gate_env())
    .ok()
    .map(|v| env_truthy(&v))
    .unwrap_or(false)
}

async fn prepare_live_provider(provider_name: &str, capability: LiveCapability) -> bool {
  if !provider_supports_capability(provider_name, capability) {
    eprintln!("[live] {provider_name}: skipped ({capability:?} is not marked supported)");
    return false;
  }

  let gate_env = capability.gate_env();
  if !live_gate_enabled(capability) {
    eprintln!("[live] {provider_name}: skipped ({gate_env} not set)");
    return false;
  }

  // Live tests construct providers directly via `<Provider>::with_client`
  // and resolve API keys with `pick_api_key()`; the only registry use is the
  // hermetic-seed `whisper_via_modality_dispatcher_transcribes_audio` test
  // below, which seeds its own one-row config. Calling `Yanshi::init()`
  // here would force-validate every model in the bundled `default_models.yml`
  // (which includes `dashscope` / `alibaba` entries unrelated to the
  // 6-provider live matrix), so a single missing unrelated key would fail-
  // close the entire suite. We instead load `~/.yanshi/.env` only — that's
  // the part that helps devs running the suite outside CI; in CI the workflow
  // sets every needed env var via the `env:` block directly.
  if let Some(home_dir) = dirs::home_dir() {
    let user_env = home_dir.join(".yanshi").join(".env");
    if user_env.exists() {
      dotenvy::from_path(&user_env).ok();
    }
  }
  true
}

fn pick_api_key(candidates: &[&str]) -> Option<(String, String)> {
  for name in candidates {
    if let Ok(value) = std::env::var(name) {
      let trimmed = value.trim();
      if !trimmed.is_empty() {
        return Some(((*name).to_string(), trimmed.to_string()));
      }
    }
  }
  None
}

fn provider_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": "Reply with exactly one word: ok"
    })],
    stream: false,
    parameters: HashMap::from([
      ("temperature".to_string(), json!(0.0)),
      // 16-token budget is enough for "ok" on the classic chat models
      // (openai / anthropic / moonshot / stepfun / glm) but Gemini 2.5
      // Flash burns the entire budget on internal thinking tokens before
      // emitting any output, surfacing as StopReason::Length. 256 is
      // still a tiny cap (~$0.0001/call) but gives reasoning models
      // enough headroom to actually finish.
      ("max_tokens".to_string(), json!(256)),
    ]),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

fn provider_streaming_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    stream: true,
    ..provider_request(model)
  }
}

fn glm_provider_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": "Reply with exactly one word: ok"
    })],
    stream: false,
    parameters: HashMap::from([
      ("do_sample".to_string(), json!(false)),
      ("max_tokens".to_string(), json!(64)),
      ("thinking".to_string(), json!({ "type": "disabled" })),
    ]),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

fn glm_provider_streaming_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    stream: true,
    ..glm_provider_request(model)
  }
}

fn provider_tool_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": "Use the get_weather tool to look up the weather in Tokyo. Do not answer directly."
    })],
    stream: false,
    parameters: HashMap::from([
      ("temperature".to_string(), json!(0.0)),
      // 64 was enough for the classic chat models, but reasoning models
      // (e.g. StepFun's step-3.5-flash) burn a chunk of the budget on
      // internal reasoning tokens before emitting the tool call — see the
      // 256-token comment on `provider_request` above for the same
      // rationale applied there first.
      ("max_tokens".to_string(), json!(256)),
    ]),
    tools: Some(vec![ToolSpec::new(
      "get_weather",
      "Get current weather for a city.",
      json!({
        "type": "object",
        "properties": {
          "city": {
            "type": "string",
            "description": "City name"
          }
        },
        "required": ["city"]
      }),
    )]),
    tool_choice: Some(ToolChoice::Tool {
      name: "get_weather".to_string(),
    }),
    thinking: None,
    response_format: None,
  }
}

fn glm_provider_tool_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": "Use the get_weather tool to look up the weather in Tokyo. Do not answer directly."
    })],
    stream: false,
    parameters: HashMap::from([
      ("do_sample".to_string(), json!(false)),
      ("max_tokens".to_string(), json!(128)),
      ("thinking".to_string(), json!({ "type": "disabled" })),
    ]),
    tools: Some(vec![ToolSpec::new(
      "get_weather",
      "Get current weather for a city.",
      json!({
        "type": "object",
        "properties": {
          "city": {
            "type": "string",
            "description": "City name"
          }
        },
        "required": ["city"]
      }),
    )]),
    tool_choice: Some(ToolChoice::Auto),
    thinking: None,
    response_format: None,
  }
}

fn provider_vision_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    model: model.to_string(),
    messages: vec![json!({
      "role": "user",
      "content": [
        {
          "type": "text",
          "text": "Reply with one word naming the dominant color in this image."
        },
        {
          "type": "image_url",
          "image_url": {
            "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADUlEQVR42mP8z8BQDwAFgwJ/l1u37wAAAABJRU5ErkJggg=="
          }
        }
      ]
    })],
    stream: false,
    parameters: HashMap::from([
      ("temperature".to_string(), json!(0.0)),
      ("max_tokens".to_string(), json!(16)),
    ]),
    tools: None,
    tool_choice: None,
    thinking: None,
    response_format: None,
  }
}

fn glm_provider_vision_request(model: &str) -> ProviderRequest {
  ProviderRequest {
    messages: vec![json!({
      "role": "user",
      "content": [
        {
          "type": "text",
          "text": "Reply with one word naming the dominant color in this image."
        },
        {
          "type": "image_url",
          "image_url": {
            "url": "https://www.gstatic.com/webp/gallery/1.jpg"
          }
        }
      ]
    })],
    parameters: HashMap::from([
      ("do_sample".to_string(), json!(false)),
      ("max_tokens".to_string(), json!(64)),
      ("thinking".to_string(), json!({ "type": "disabled" })),
    ]),
    ..provider_vision_request(model)
  }
}

fn assert_text_non_empty(content: &ContentType) {
  match content {
    ContentType::Text(t) => {
      assert!(
        !t.trim().is_empty(),
        "expected non-empty text content from live provider"
      );
    }
    other => panic!("expected ContentType::Text, got {other:?}"),
  }
}

fn assert_usage_populated(usage: &Option<TokenUsage>, provider: &str) {
  let Some(usage) = usage.as_ref() else {
    panic!("{provider}: expected populated TokenUsage, got None");
  };
  assert!(
    usage.prompt_tokens.is_some(),
    "{provider}: prompt_tokens must be populated"
  );
  assert!(
    usage.completion_tokens.is_some(),
    "{provider}: completion_tokens must be populated"
  );
  assert!(
    usage.total_tokens.is_some(),
    "{provider}: total_tokens must be populated"
  );
}

async fn assert_live_stream_non_empty(mut stream: Box<dyn yanshi_llm::StreamingResponse>) {
  let mut text = String::new();
  let mut saw_final = false;

  while let Some(chunk) = stream
    .next_chunk()
    .await
    .expect("live streaming chunk must parse")
  {
    if !chunk.content.is_empty() {
      text.push_str(&chunk.content);
    }
    if chunk.is_final {
      saw_final = true;
    }
  }

  assert!(
    !text.trim().is_empty(),
    "expected non-empty text from live streaming response"
  );
  assert!(
    saw_final,
    "expected live streaming response to include a final chunk"
  );
}

/// Default Google text model for the text and streaming live tests.
/// `gemini-1.5-flash` was retired from v1beta; `gemini-2.0-flash` and
/// (since 2026-11) `gemini-2.5-flash` are "no longer available to new
/// users" (404) even though `/v1beta/models` still lists them.
const GOOGLE_TEXT_MODEL: &str = "gemini-3.5-flash";

/// Minimum-cost model per provider for live smoke tests.
///
/// Override via the matching env var when a provider deprecates a model. The
/// defaults are picked for low cost-per-call and broad availability rather
/// than capability.
fn live_text_model(provider: &str, default: &str) -> String {
  live_model(provider, "TEXT", default)
}

fn live_model_override(provider: &str, capability: &str) -> Option<String> {
  let provider_upper = provider.to_ascii_uppercase();
  let specific = format!("YANSHI_LIVE_{provider_upper}_{capability}_MODEL");
  let legacy = format!("YANSHI_LIVE_{provider_upper}_MODEL");
  for env_var in [&specific, &legacy] {
    if let Ok(value) = std::env::var(env_var) {
      let trimmed = value.trim();
      if !trimmed.is_empty() {
        eprintln!("[live] {provider}: using {capability} model override from {env_var}");
        return Some(trimmed.to_string());
      }
    }
  }
  None
}

fn live_model(provider: &str, capability: &str, default: &str) -> String {
  if let Some(model) = live_model_override(provider, capability) {
    return model;
  }
  let provider_upper = provider.to_ascii_uppercase();
  let specific = format!("YANSHI_LIVE_{provider_upper}_{capability}_MODEL");
  eprintln!(
    "[live] {provider}: using default {capability} model `{default}` (set {specific} to override)"
  );
  default.to_string()
}

fn model_id_from_config(alias: &str, model: &yanshi_llm::ModelConfig) -> String {
  model
    .model_id
    .clone()
    .or_else(|| {
      model
        .additional_params
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
    })
    .unwrap_or_else(|| alias.to_string())
}

async fn stepfun_configured_model<F>(
  capability: &str,
  preferred_models: &[&str],
  accepts: F,
) -> Option<String>
where
  F: Fn(&yanshi_llm::ModelConfig) -> bool,
{
  let Ok((config, _source)) = LLMConfig::from_default_source().await else {
    return None;
  };

  let mut candidates = config
    .models
    .iter()
    .filter(|(_, model)| {
      matches!(
        model.vendor.to_ascii_lowercase().as_str(),
        "stepfun" | "step"
      ) && accepts(model)
    })
    .map(|(alias, model)| (alias.as_str(), model_id_from_config(alias, model)))
    .collect::<Vec<_>>();

  candidates.sort_by(|(left_alias, left_model), (right_alias, right_model)| {
    let left_rank = preferred_models
      .iter()
      .position(|preferred| preferred == left_alias || preferred == left_model)
      .unwrap_or(usize::MAX);
    let right_rank = preferred_models
      .iter()
      .position(|preferred| preferred == right_alias || preferred == right_model)
      .unwrap_or(usize::MAX);
    left_rank
      .cmp(&right_rank)
      .then_with(|| left_alias.cmp(right_alias))
  });

  let selected = candidates.into_iter().next().map(|(_alias, model)| model);

  if let Some(model) = &selected {
    eprintln!("[live] stepfun: using {capability} model `{model}` from Yanshi models config");
  }
  selected
}

async fn stepfun_live_model<F>(
  capability: &str,
  default: &str,
  preferred_models: &[&str],
  accepts: F,
) -> String
where
  F: Fn(&yanshi_llm::ModelConfig) -> bool,
{
  if let Some(env_model) = live_model_override("stepfun", capability) {
    return env_model;
  }

  stepfun_configured_model(capability, preferred_models, accepts)
    .await
    .unwrap_or_else(|| {
      eprintln!("[live] stepfun: using default {capability} model `{default}`");
      default.to_string()
    })
}

async fn stepfun_base_url() -> Option<String> {
  let Ok((config, _source)) = LLMConfig::from_default_source().await else {
    return None;
  };

  config
    .get_provider("stepfun")
    .or_else(|| config.get_provider("step"))
    .and_then(|provider| provider.base_url.clone())
}

async fn stepfun_live_context(capability: LiveCapability) -> Option<(String, Option<String>)> {
  if !prepare_live_provider("stepfun", capability).await {
    return None;
  }

  let Some((env_used, api_key)) = pick_api_key(&["STEPFUN_API_KEY", "STEP_API_KEY"]) else {
    eprintln!(
      "[live] stepfun: skipped (none of STEPFUN_API_KEY / STEP_API_KEY set; live gate is on)"
    );
    return None;
  };
  eprintln!("[live] stepfun: using API key from {env_used}");

  Some((api_key, stepfun_base_url().await))
}

async fn glm_configured_model<F>(
  capability: &str,
  preferred_models: &[&str],
  accepts: F,
) -> Option<String>
where
  F: Fn(&yanshi_llm::ModelConfig) -> bool,
{
  let Ok((config, _source)) = LLMConfig::from_default_source().await else {
    return None;
  };

  let mut candidates = config
    .models
    .iter()
    .filter(|(_, model)| {
      matches!(
        model.vendor.to_ascii_lowercase().as_str(),
        "glm" | "bigmodel" | "zhipu"
      ) && accepts(model)
    })
    .map(|(alias, model)| (alias.as_str(), model_id_from_config(alias, model)))
    .collect::<Vec<_>>();

  candidates.sort_by(|(left_alias, left_model), (right_alias, right_model)| {
    let left_rank = preferred_models
      .iter()
      .position(|preferred| preferred == left_alias || preferred == left_model)
      .unwrap_or(usize::MAX);
    let right_rank = preferred_models
      .iter()
      .position(|preferred| preferred == right_alias || preferred == right_model)
      .unwrap_or(usize::MAX);
    left_rank
      .cmp(&right_rank)
      .then_with(|| left_alias.cmp(right_alias))
  });

  let selected = candidates.into_iter().next().map(|(_alias, model)| model);

  if let Some(model) = &selected {
    eprintln!("[live] glm: using {capability} model `{model}` from Yanshi models config");
  }
  selected
}

async fn glm_live_model<F>(
  capability: &str,
  default: &str,
  preferred_models: &[&str],
  accepts: F,
) -> String
where
  F: Fn(&yanshi_llm::ModelConfig) -> bool,
{
  if let Some(env_model) = live_model_override("glm", capability) {
    return env_model;
  }

  glm_configured_model(capability, preferred_models, accepts)
    .await
    .unwrap_or_else(|| {
      eprintln!("[live] glm: using default {capability} model `{default}`");
      default.to_string()
    })
}

async fn glm_base_url() -> Option<String> {
  let Ok((config, _source)) = LLMConfig::from_default_source().await else {
    return Some("https://open.bigmodel.cn/api/paas/v4".to_string());
  };

  config
    .get_provider("glm")
    .or_else(|| config.get_provider("bigmodel"))
    .or_else(|| config.get_provider("zhipu"))
    .and_then(|provider| provider.base_url.clone())
    .or_else(|| Some("https://open.bigmodel.cn/api/paas/v4".to_string()))
}

async fn glm_live_context(capability: LiveCapability) -> Option<(String, Option<String>)> {
  if !prepare_live_provider("glm", capability).await {
    return None;
  }

  let Some((env_used, api_key)) =
    pick_api_key(&["GLM_API_KEY", "BIGMODEL_API_KEY", "ZHIPU_API_KEY"])
  else {
    eprintln!(
      "[live] glm: skipped (none of GLM_API_KEY / BIGMODEL_API_KEY / ZHIPU_API_KEY set; live gate is on)"
    );
    return None;
  };
  eprintln!("[live] glm: using API key from {env_used}");

  Some((api_key, glm_base_url().await))
}

async fn run_text_path<P, F>(provider_name: &str, key_envs: &[&str], build: F)
where
  P: LLMProvider + 'static,
  F: FnOnce(&str) -> P,
{
  if !prepare_live_provider(provider_name, LiveCapability::Llm).await {
    return;
  }
  let Some((env_used, api_key)) = pick_api_key(key_envs) else {
    eprintln!("[live] {provider_name}: skipped (none of {key_envs:?} set; live gate is on)");
    return;
  };
  eprintln!("[live] {provider_name}: using API key from {env_used}");

  let provider = build(&api_key);
  let model = match provider_name {
    "openai" => live_text_model(provider_name, "gpt-4o-mini"),
    // `claude-3-5-haiku-latest` alias and the dated `-20241022` revision
    // both 404 against current Anthropic tiers; `claude-haiku-4-5` is the
    // current cheapest model per CLAUDE.md.
    "anthropic" => live_text_model(provider_name, "claude-haiku-4-5"),
    "google" => live_text_model(provider_name, GOOGLE_TEXT_MODEL),
    // The whole `moonshot-v1-*` lineage disappeared from Moonshot's
    // `/v1/models` on 2026-08-31 (404 `resource_not_found_error`);
    // `kimi-k2.6` is the cheapest chat model still listed.
    "moonshot" => live_text_model(provider_name, "kimi-k2.6"),
    // The whole step-1-*/step-2-* lineage was retired from StepFun's
    // `/v1/models` (confirmed 404 `model_invalid`); step-3.5-flash is the
    // current text-reasoning default.
    "stepfun" => live_text_model(provider_name, "step-3.5-flash"),
    other => panic!("unknown provider in live harness: {other}"),
  };

  let mut request = provider_request(&model);
  // Kimi K2.x rejects any temperature other than 1 with a 400 (the
  // registry pins `kimi-k2.6` to `temperature: 1.0` for the same reason).
  if model.starts_with("kimi-") {
    request
      .parameters
      .insert("temperature".to_string(), json!(1.0));
  }

  let response = tokio::time::timeout(Duration::from_secs(30), provider.execute(&request))
    .await
    .unwrap_or_else(|_| panic!("{provider_name}: live request timed out after 30s"))
    .unwrap_or_else(|e| panic!("{provider_name}: live request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, provider_name);
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "{provider_name}: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_live_text_path() {
  run_text_path::<OpenAIProvider, _>("openai", &["OPENAI_API_KEY", "OPENAI_KEY"], |key| {
    OpenAIProvider::new(key, None).expect("openai provider")
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_live_text_path() {
  run_text_path::<AnthropicProvider, _>(
    "anthropic",
    &["ANTHROPIC_API_KEY", "ANTHROPIC_KEY", "CLAUDE_API_KEY"],
    |key| AnthropicProvider::new(key, None).expect("anthropic provider"),
  )
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_live_text_path() {
  run_text_path::<GoogleProvider, _>(
    "google",
    &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_AI_KEY"],
    |key| GoogleProvider::new(key, None).expect("google provider"),
  )
  .await;
}

/// Eval 2026-10-07 L1: Google streaming was never exercised live, and
/// came back empty for want of `alt=sse`. ReAct streams unconditionally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_live_streaming_path() {
  if !prepare_live_provider("google", LiveCapability::Llm).await {
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_AI_KEY"])
  else {
    eprintln!("[live] google streaming: skipped (no Gemini API key env var set)");
    return;
  };
  let provider =
    GoogleProvider::with_client(no_proxy_client(), &api_key, None).expect("google provider");
  let model = live_text_model("google", GOOGLE_TEXT_MODEL);

  let stream = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute_streaming(&provider_streaming_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("google: live streaming request timed out after 30s"))
  .unwrap_or_else(|e| panic!("google: live streaming request failed: {e}"));

  assert_live_stream_non_empty(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moonshot_live_text_path() {
  run_text_path::<MoonshotProvider, _>("moonshot", &["MOONSHOT_API_KEY", "MOONSHOT_KEY"], |key| {
    MoonshotProvider::new(key, None).expect("moonshot provider")
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_text_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    StepFunProvider::with_client(no_proxy_client(), &api_key, base_url).expect("stepfun provider");
  let model = stepfun_live_model(
    "TEXT",
    "step-3.5-flash",
    &["step-3.5-flash-2603", "step-3.5-flash"],
    |model| model.model_type() == "text",
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live text request timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live text request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "stepfun");
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "stepfun: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_streaming_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    StepFunProvider::with_client(no_proxy_client(), &api_key, base_url).expect("stepfun provider");
  let model = stepfun_live_model(
    "TEXT",
    "step-3.5-flash",
    &["step-3.5-flash-2603", "step-3.5-flash"],
    |model| model.model_type() == "text" && model.supports_streaming_capability(),
  )
  .await;

  let stream = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute_streaming(&provider_streaming_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live streaming request timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live streaming request failed: {e}"));

  assert_live_stream_non_empty(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_tool_calling_or_fallback_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    StepFunProvider::with_client(no_proxy_client(), &api_key, base_url).expect("stepfun provider");
  let model = stepfun_live_model(
    "TOOLS",
    "step-3.5-flash",
    &["step-3.5-flash-2603", "step-3.5-flash"],
    |model| model.model_type() == "text" && model.supports_tools_capability(),
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_tool_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live tool request timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live tool request failed: {e}"));

  if let Some(tool_call) = response.tool_calls.first() {
    assert_eq!(tool_call.name, "get_weather");
    assert!(
      tool_call.arguments.get("city").is_some(),
      "stepfun: tool call must include city argument"
    );
    assert_eq!(
      response.stop_reason,
      Some(StopReason::ToolCalls),
      "stepfun: native tool calls should map to StopReason::ToolCalls"
    );
  } else {
    assert_text_non_empty(&response.content);
    eprintln!("[live] stepfun: model returned text fallback instead of native tool_calls");
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_vision_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Multimodal).await else {
    return;
  };
  let provider =
    StepFunProvider::with_client(no_proxy_client(), &api_key, base_url).expect("stepfun provider");
  let model = stepfun_live_model(
    "VISION",
    "step-1o-turbo-vision",
    &["step-1o-turbo-vision", "step-3.7-flash"],
    |model| matches!(model.model_type(), "imageunderstand" | "multimodal") || model.is_multimodal(),
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_vision_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live vision request timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live vision request failed: {e}"));

  assert_text_non_empty(&response.content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_image_generation_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Image).await else {
    return;
  };
  let client = StepFunSpecializedClient::with_client(no_proxy_client(), &api_key, base_url)
    .expect("stepfun specialized client");
  let model = stepfun_live_model("IMAGE", "step-2x-large", &["step-2x-large"], |model| {
    matches!(model.model_type(), "generateimage" | "text2image" | "image") || model.is_image_model()
  })
  .await;

  let request = Text2ImageBuilder::new(&model, "a tiny red square icon on a white background")
    .size("512x512")
    .response_format("b64_json")
    .steps(1)
    .build();

  let response = tokio::time::timeout(Duration::from_secs(60), client.text_to_image(request))
    .await
    .unwrap_or_else(|_| panic!("stepfun: live image generation timed out after 60s"))
    .unwrap_or_else(|e| panic!("stepfun: live image generation failed: {e}"));

  let first = response
    .data
    .first()
    .expect("stepfun: image generation response must contain at least one image");
  assert!(
    first.image.as_deref().is_some_and(|v| !v.is_empty())
      || first.b64_json.as_deref().is_some_and(|v| !v.is_empty())
      || first.url.as_deref().is_some_and(|v| !v.is_empty()),
    "stepfun: image generation response must contain image, b64_json, or url"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_tts_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Audio).await else {
    return;
  };
  let client = StepFunSpecializedClient::with_client(no_proxy_client(), &api_key, base_url)
    .expect("stepfun specialized client");
  let model = stepfun_live_model(
    "TTS",
    "step-tts-mini",
    &["step-tts-mini", "step-tts-vivid"],
    |model| model.model_type() == "tts",
  )
  .await;
  let voice = std::env::var("YANSHI_LIVE_STEPFUN_TTS_VOICE")
    .ok()
    .filter(|value| !value.trim().is_empty())
    .unwrap_or_else(|| "cixingnansheng".to_string());

  let request = TTSBuilder::new(&model, "ok", &voice)
    .response_format("mp3")
    .build();

  let audio = tokio::time::timeout(Duration::from_secs(30), client.text_to_speech(request))
    .await
    .unwrap_or_else(|_| panic!("stepfun: live TTS request timed out after 30s"))
    .unwrap_or_else(|e| panic!("stepfun: live TTS request failed: {e}"));

  assert!(
    audio.len() > 128,
    "stepfun: TTS response should contain audio bytes"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stepfun_live_asr_path() {
  let Some((api_key, base_url)) = stepfun_live_context(LiveCapability::Audio).await else {
    return;
  };
  let client = StepFunSpecializedClient::with_client(no_proxy_client(), &api_key, base_url)
    .expect("stepfun specialized client");
  let tts_model = stepfun_live_model(
    "TTS",
    "step-tts-mini",
    &["step-tts-mini", "step-tts-vivid"],
    |model| model.model_type() == "tts",
  )
  .await;
  let asr_model = stepfun_live_model("ASR", "step-asr", &["step-asr"], |model| {
    model.model_type() == "asr"
  })
  .await;
  let voice = std::env::var("YANSHI_LIVE_STEPFUN_TTS_VOICE")
    .ok()
    .filter(|value| !value.trim().is_empty())
    .unwrap_or_else(|| "cixingnansheng".to_string());

  let speech = tokio::time::timeout(
    Duration::from_secs(30),
    client.text_to_speech(
      TTSBuilder::new(&tts_model, "yanshi live test", &voice)
        .response_format("mp3")
        .build(),
    ),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live ASR fixture TTS timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live ASR fixture TTS failed: {e}"));

  let transcript = tokio::time::timeout(
    Duration::from_secs(30),
    client.speech_to_text(ASRRequest {
      model: asr_model,
      response_format: "text".to_string(),
      audio_data: speech,
      filename: "yanshi-live-test.mp3".to_string(),
    }),
  )
  .await
  .unwrap_or_else(|_| panic!("stepfun: live ASR request timed out after 30s"))
  .unwrap_or_else(|e| panic!("stepfun: live ASR request failed: {e}"));

  assert!(
    !transcript.trim().is_empty(),
    "stepfun: ASR transcript must be non-empty"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_live_text_path() {
  let _guard = glm_live_lock().lock().await;
  let Some((api_key, base_url)) = glm_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("glm provider");
  let model = glm_live_model(
    "TEXT",
    "glm-5.1",
    &["glm-5.1", "glm-4.5-flash", "glm-4-flash-250414"],
    |model| model.model_type() == "text",
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&glm_provider_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("glm: live text request timed out after 30s"))
  .unwrap_or_else(|e| panic!("glm: live text request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "glm");
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "glm: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_live_openai_compatible_chat_path() {
  let _guard = glm_live_lock().lock().await;
  let Some((api_key, base_url)) = glm_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("glm provider");
  let model = glm_live_model(
    "TEXT",
    "glm-5.1",
    &["glm-5.1", "glm-4.5-flash", "glm-4-flash-250414"],
    |model| model.model_type() == "text",
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&ProviderRequest {
      messages: vec![json!({
        "role": "user",
        "content": "Reply with exactly these two lowercase letters: ok"
      })],
      ..glm_provider_request(&model)
    }),
  )
  .await
  .unwrap_or_else(|_| panic!("glm: OpenAI-compatible chat request timed out after 30s"))
  .unwrap_or_else(|e| panic!("glm: OpenAI-compatible chat request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "glm");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_live_streaming_path() {
  let _guard = glm_live_lock().lock().await;
  let Some((api_key, base_url)) = glm_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("glm provider");
  let model = glm_live_model(
    "TEXT",
    "glm-5.1",
    &["glm-5.1", "glm-4.5-flash", "glm-4-flash-250414"],
    |model| model.model_type() == "text" && model.supports_streaming_capability(),
  )
  .await;

  let stream = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute_streaming(&glm_provider_streaming_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("glm: live streaming request timed out after 30s"))
  .unwrap_or_else(|e| panic!("glm: live streaming request failed: {e}"));

  assert_live_stream_non_empty(stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_live_tool_calling_or_fallback_path() {
  let _guard = glm_live_lock().lock().await;
  let Some((api_key, base_url)) = glm_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("glm provider");
  let model = glm_live_model(
    "TOOLS",
    "glm-5.1",
    &["glm-5.1", "glm-4.5-flash", "glm-4.7-flash"],
    |model| model.model_type() == "text" && model.supports_tools_capability(),
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&glm_provider_tool_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("glm: live tool request timed out after 30s"))
  .unwrap_or_else(|e| panic!("glm: live tool request failed: {e}"));

  if let Some(tool_call) = response.tool_calls.first() {
    assert_eq!(tool_call.name, "get_weather");
    assert!(
      tool_call.arguments.get("city").is_some(),
      "glm: tool call must include city argument"
    );
    assert_eq!(
      response.stop_reason,
      Some(StopReason::ToolCalls),
      "glm: native tool calls should map to StopReason::ToolCalls"
    );
  } else {
    assert_text_non_empty(&response.content);
    eprintln!("[live] glm: model returned text fallback instead of native tool_calls");
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_live_vision_path() {
  let _guard = glm_live_lock().lock().await;
  let Some((api_key, base_url)) = glm_live_context(LiveCapability::Multimodal).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("glm provider");
  let model = glm_live_model(
    "VISION",
    "glm-4.5v",
    &["glm-4.5v", "glm-5v-turbo", "glm-4.1v-thinking"],
    |model| matches!(model.model_type(), "imageunderstand" | "multimodal") || model.is_multimodal(),
  )
  .await;

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&glm_provider_vision_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("glm: live vision request timed out after 30s"))
  .unwrap_or_else(|e| panic!("glm: live vision request failed: {e}"));

  assert_text_non_empty(&response.content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashscope_live_text_path() {
  // DashScope (Alibaba Bailian) exposes an OpenAI-compatible chat completions
  // endpoint at `<base>/compatible-mode/v1`, so we drive it through
  // `OpenAIProvider::with_client` like GLM. `qwen-plus` is a long-standing
  // stable alias that Alibaba maintains across model generations — it
  // currently points at the latest qwen-plus revision and 91 dashscope
  // entries in `default_models.yml` use it. Override at workflow level
  // via `YANSHI_LIVE_DASHSCOPE_TEXT_MODEL` if the alias ever decays.
  let _guard = dashscope_live_lock().lock().await;
  let Some((api_key, base_url)) = dashscope_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("dashscope provider");
  let model = live_text_model("dashscope", "qwen-plus");

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("dashscope: live text request timed out after 30s"))
  .unwrap_or_else(|e| panic!("dashscope: live text request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "dashscope");
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "dashscope: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deepseek_live_text_path() {
  // DeepSeek (https://api.deepseek.com) is OpenAI-compatible.
  // `deepseek-chat` is on DeepSeek's deprecation list (2026-07-24);
  // `deepseek-v4-flash` is the current cheapest V4 model per the API
  // docs. Override at workflow level via
  // `YANSHI_LIVE_DEEPSEEK_TEXT_MODEL` when the lineup shifts again.
  let _guard = deepseek_live_lock().lock().await;
  let Some((api_key, base_url)) = deepseek_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("deepseek provider");
  let model = live_text_model("deepseek", "deepseek-v4-flash");

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("deepseek: live text request timed out after 30s"))
  .unwrap_or_else(|e| panic!("deepseek: live text request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "deepseek");
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "deepseek: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minimax_live_text_path() {
  // MiniMax (https://api.minimaxi.com — note the `i` in `minimaxi`) advertises
  // OpenAI SDK compatibility per their API overview. `MiniMax-M2` is the
  // cheapest current model in the M2 family per the docs. Override at the
  // workflow level via `YANSHI_LIVE_MINIMAX_TEXT_MODEL` if the lineup
  // shifts.
  let _guard = minimax_live_lock().lock().await;
  let Some((api_key, base_url)) = minimax_live_context(LiveCapability::Llm).await else {
    return;
  };
  let provider =
    OpenAIProvider::with_client(no_proxy_client(), &api_key, base_url).expect("minimax provider");
  let model = live_text_model("minimax", "MiniMax-M2");

  let response = tokio::time::timeout(
    Duration::from_secs(30),
    provider.execute(&provider_request(&model)),
  )
  .await
  .unwrap_or_else(|_| panic!("minimax: live text request timed out after 30s"))
  .unwrap_or_else(|e| panic!("minimax: live text request failed: {e}"));

  assert_text_non_empty(&response.content);
  assert_usage_populated(&response.usage, "minimax");
  assert_eq!(
    response.stop_reason,
    Some(StopReason::Stop),
    "minimax: expected StopReason::Stop on a single-turn text completion"
  );
}

#[tokio::test]
async fn gate_disabled_by_default() {
  // Belt-and-suspenders: confirm that without the gate env var set, all
  // per-provider tests above would short-circuit before issuing any HTTP
  // request. This guards the offline `cargo test --all-targets` invariant.
  if std::env::var(LiveCapability::Llm.gate_env()).is_ok() {
    eprintln!("[live] gate is set; skipping the gate-default-off invariant test");
    return;
  }
  assert!(
    !live_gate_enabled(LiveCapability::Llm),
    "live_gate_enabled() must default to false when YANSHI_LIVE_LLM_TESTS is unset"
  );
}

#[test]
fn live_capability_gate_names_are_stable() {
  assert_eq!(LiveCapability::Llm.gate_env(), "YANSHI_LIVE_LLM_TESTS");
  assert_eq!(
    LiveCapability::Multimodal.gate_env(),
    "YANSHI_LIVE_MULTIMODAL_TESTS"
  );
  assert_eq!(LiveCapability::Image.gate_env(), "YANSHI_LIVE_IMAGE_TESTS");
  assert_eq!(LiveCapability::Audio.gate_env(), "YANSHI_LIVE_AUDIO_TESTS");
  assert_eq!(LiveCapability::Video.gate_env(), "YANSHI_LIVE_VIDEO_TESTS");
}

#[test]
fn live_gate_truthy_values_are_explicit() {
  for value in ["1", "true", "TRUE", "yes", "on"] {
    assert!(env_truthy(value), "{value} should enable live tests");
  }
  for value in ["", "0", "false", "no", "off", "please"] {
    assert!(!env_truthy(value), "{value} should not enable live tests");
  }
}

#[test]
fn live_provider_profiles_probe_supported_capabilities() {
  assert!(provider_supports_capability(
    "stepfun",
    LiveCapability::Image
  ));
  assert!(provider_supports_capability(
    "stepfun",
    LiveCapability::Audio
  ));
  assert!(provider_supports_capability(
    "openai",
    LiveCapability::Multimodal
  ));
  assert!(!provider_supports_capability(
    "openai",
    LiveCapability::Video
  ));
  assert!(provider_supports_capability("glm", LiveCapability::Llm));
  assert!(provider_supports_capability(
    "glm",
    LiveCapability::Multimodal
  ));
  assert!(!provider_supports_capability("glm", LiveCapability::Audio));
  assert!(!provider_supports_capability(
    "unknown",
    LiveCapability::Llm
  ));
}

// ============================================================================
// P-LLM.5 — OpenAI Whisper via the modality dispatcher
//
// Validates that `Yanshi::asr("whisper-1")` resolves through the registry
// to the OpenAI vendor, builds an `OpenAIAsrProvider`, and successfully
// transcribes a tiny audio clip. Gated on `YANSHI_LIVE_AUDIO_TESTS=1` AND
// `OPENAI_API_KEY` so the workspace stays hermetic without the key.
//
// Uses StepFun TTS to produce the audio fixture (live), then runs the
// transcript through Whisper. If StepFun isn't reachable the audio
// fixture falls back to a small WAV header so the test still validates
// the dispatcher / multipart / response-parsing path even when only
// OPENAI_API_KEY is configured.
// ============================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whisper_via_modality_dispatcher_transcribes_audio() {
  if !live_gate_enabled(LiveCapability::Audio) {
    eprintln!("[live] whisper: skipped (YANSHI_LIVE_AUDIO_TESTS not enabled)");
    return;
  }
  let api_key = match std::env::var("OPENAI_API_KEY") {
    Ok(v) if !v.trim().is_empty() => v,
    _ => {
      eprintln!("[live] whisper: skipped (OPENAI_API_KEY missing)");
      return;
    }
  };

  // Ensure the test process has the OpenAI key registered before
  // dispatcher key resolution.
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("OPENAI_API_KEY", api_key);
  }

  // Generate the audio fixture FIRST — `stepfun_live_context` triggers
  // `Yanshi::init()` which reloads the user's `~/.yanshi/models.yml`,
  // so any registry mutation must happen after this point.
  let audio: Vec<u8> =
    if let Some((sf_key, sf_base)) = stepfun_live_context(LiveCapability::Audio).await {
      let client = StepFunSpecializedClient::with_client(no_proxy_client(), &sf_key, sf_base)
        .expect("stepfun specialized client");
      let req = TTSBuilder::new("step-tts-mini", "yanshi whisper test", "cixingnansheng")
        .response_format("mp3")
        .build();
      match tokio::time::timeout(Duration::from_secs(30), client.text_to_speech(req)).await {
        Ok(Ok(bytes)) => bytes,
        _ => silent_wav_one_second(),
      }
    } else {
      silent_wav_one_second()
    };

  // Now seed the hermetic whisper-1 entry. This wins over the user's
  // ~/.yanshi/models.yml (which may predate the P-LLM.5 registry
  // additions). The bundled `default_models.yml` already carries
  // whisper-1; this seed just makes the test independent of the host's
  // local registry choices.
  let hermetic_registry_yaml = r#"
models:
  whisper-1:
    vendor: openai
    type: asr
    accepts: [audio]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic whisper-1 entry");

  let provider = Yanshi::asr("whisper-1")
    .await
    .expect("whisper-1 resolves through dispatcher");
  assert_eq!(provider.name(), "openai", "whisper-1 must route to openai");

  let request = yanshi_llm::AsrRequest {
    model: "whisper-1".to_string(),
    audio_data: audio,
    filename: "yanshi-whisper-test.mp3".to_string(),
    response_format: "json".to_string(),
    language: Some("en".to_string()),
    temperature: Some(0.0),
    prompt: Some("yanshi, whisper, modality dispatcher".to_string()),
  };
  let response = tokio::time::timeout(Duration::from_secs(60), provider.transcribe(request))
    .await
    .unwrap_or_else(|_| panic!("whisper transcription timed out after 60s"))
    .unwrap_or_else(|e| panic!("whisper transcription failed: {e}"));

  // Silent fallback may produce empty transcript; only assert that the
  // call succeeded and the metadata round-tripped. For the StepFun-
  // produced fixture the transcript is usually non-empty.
  assert!(
    response.metadata.is_some(),
    "json response_format must carry metadata"
  );
}

// ============================================================================
// OpenAI image generation + TTS via the modality dispatcher (P-LLM2.3 Batch 1)
// ============================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_image_generation_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Image) {
    eprintln!("[live] openai image: skipped (YANSHI_LIVE_IMAGE_TESTS not enabled)");
    return;
  }
  let api_key = match std::env::var("OPENAI_API_KEY") {
    Ok(v) if !v.trim().is_empty() => v,
    _ => {
      eprintln!("[live] openai image: skipped (OPENAI_API_KEY missing)");
      return;
    }
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("OPENAI_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  gpt-image-2:
    vendor: openai
    type: text_to_image
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic gpt-image-2 entry");

  let provider = Yanshi::text2image_for("gpt-image-2")
    .await
    .expect("gpt-image-2 resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "openai",
    "gpt-image-2 must route to openai"
  );

  let request = yanshi_llm::Text2ImageRequest {
    model: "gpt-image-2".to_string(),
    prompt: "a tiny red square icon on a white background".to_string(),
    size: Some("1024x1024".to_string()),
    n: Some(1),
    response_format: Some("b64_json".to_string()),
    seed: None,
    steps: None,
    cfg_scale: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(60), provider.generate(request))
    .await
    .unwrap_or_else(|_| panic!("openai image generation timed out after 60s"))
    .unwrap_or_else(|e| panic!("openai image generation failed: {e}"));

  let first = response
    .images
    .first()
    .expect("openai image generation response must contain at least one image");
  assert!(
    first.b64_json.as_deref().is_some_and(|v| !v.is_empty())
      || first.url.as_deref().is_some_and(|v| !v.is_empty()),
    "openai image generation response must contain b64_json or url"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_tts_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Audio) {
    eprintln!("[live] openai tts: skipped (YANSHI_LIVE_AUDIO_TESTS not enabled)");
    return;
  }
  let api_key = match std::env::var("OPENAI_API_KEY") {
    Ok(v) if !v.trim().is_empty() => v,
    _ => {
      eprintln!("[live] openai tts: skipped (OPENAI_API_KEY missing)");
      return;
    }
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("OPENAI_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  gpt-4o-mini-tts:
    vendor: openai
    type: tts
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic gpt-4o-mini-tts entry");

  let provider = Yanshi::tts("gpt-4o-mini-tts")
    .await
    .expect("gpt-4o-mini-tts resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "openai",
    "gpt-4o-mini-tts must route to openai"
  );

  let request = yanshi_llm::TtsRequest {
    model: "gpt-4o-mini-tts".to_string(),
    input: "yanshi modality dispatcher test".to_string(),
    voice: "coral".to_string(),
    response_format: Some("mp3".to_string()),
    speed: None,
    volume: None,
    sample_rate: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(30), provider.synthesize(request))
    .await
    .unwrap_or_else(|_| panic!("openai TTS request timed out after 30s"))
    .unwrap_or_else(|e| panic!("openai TTS request failed: {e}"));

  assert!(
    response.audio.len() > 128,
    "openai TTS response should contain audio bytes"
  );
  assert_eq!(response.mime_type, "audio/mpeg");
}

// ============================================================================
// Google image generation + TTS via the modality dispatcher, both via
// `generateContent` (P-LLM2.3 Batch 2a) — NOT the deprecated Imagen
// `:predict` endpoint.
// ============================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_image_generation_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Image) {
    eprintln!("[live] google image: skipped (YANSHI_LIVE_IMAGE_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_AI_KEY"])
  else {
    eprintln!("[live] google image: skipped (no Gemini API key env var set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("GEMINI_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  gemini-3.1-flash-image:
    vendor: google
    type: text_to_image
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic gemini-3.1-flash-image entry");

  let provider = Yanshi::text2image_for("gemini-3.1-flash-image")
    .await
    .expect("gemini-3.1-flash-image resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "google",
    "gemini-3.1-flash-image must route to google"
  );

  let request = yanshi_llm::Text2ImageRequest {
    model: "gemini-3.1-flash-image".to_string(),
    prompt: "a tiny red square icon on a white background".to_string(),
    size: None,
    n: None,
    response_format: None,
    seed: None,
    steps: None,
    cfg_scale: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(60), provider.generate(request))
    .await
    .unwrap_or_else(|_| panic!("google image generation timed out after 60s"))
    .unwrap_or_else(|e| panic!("google image generation failed: {e}"));

  let first = response
    .images
    .first()
    .expect("google image generation response must contain at least one image");
  assert!(
    first.b64_json.as_deref().is_some_and(|v| !v.is_empty()),
    "google image generation response must contain b64_json"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_tts_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Audio) {
    eprintln!("[live] google tts: skipped (YANSHI_LIVE_AUDIO_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_AI_KEY"])
  else {
    eprintln!("[live] google tts: skipped (no Gemini API key env var set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("GEMINI_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  gemini-3.1-flash-tts-preview:
    vendor: google
    type: tts
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic gemini-3.1-flash-tts-preview entry");

  let provider = Yanshi::tts("gemini-3.1-flash-tts-preview")
    .await
    .expect("gemini-3.1-flash-tts-preview resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "google",
    "gemini-3.1-flash-tts-preview must route to google"
  );

  let request = yanshi_llm::TtsRequest {
    model: "gemini-3.1-flash-tts-preview".to_string(),
    input: "yanshi modality dispatcher test".to_string(),
    voice: "Kore".to_string(),
    response_format: None,
    speed: None,
    volume: None,
    sample_rate: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(30), provider.synthesize(request))
    .await
    .unwrap_or_else(|_| panic!("google TTS request timed out after 30s"))
    .unwrap_or_else(|e| panic!("google TTS request failed: {e}"));

  assert!(
    !response.audio.is_empty(),
    "google TTS response should contain audio bytes"
  );
}

// ============================================================================
// DashScope image generation (Wan, async task) + TTS (Qwen-TTS, sync) via
// the modality dispatcher (P-LLM2.3 Batch 2b) — NOT CosyVoice (WebSocket-
// only) or ASR (Paraformer requires a public URL, unsupported by this
// crate's byte-upload-shaped AsrRequest).
// ============================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashscope_image_generation_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Image) {
    eprintln!("[live] dashscope image: skipped (YANSHI_LIVE_IMAGE_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["DASHSCOPE_API_KEY"]) else {
    eprintln!("[live] dashscope image: skipped (DASHSCOPE_API_KEY not set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("DASHSCOPE_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  wan2.5-t2i-preview:
    vendor: dashscope
    type: text_to_image
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic wan2.5-t2i-preview entry");

  let provider = Yanshi::text2image_for("wan2.5-t2i-preview")
    .await
    .expect("wan2.5-t2i-preview resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "dashscope",
    "wan2.5-t2i-preview must route to dashscope"
  );

  let request = yanshi_llm::Text2ImageRequest {
    model: "wan2.5-t2i-preview".to_string(),
    prompt: "a tiny red square icon on a white background".to_string(),
    size: Some("1024*1024".to_string()),
    n: Some(1),
    response_format: None,
    seed: None,
    steps: None,
    cfg_scale: None,
  };
  // Longer bound than the synchronous-request live tests: this is a
  // real submit+poll loop, not a single request.
  let response = tokio::time::timeout(Duration::from_secs(90), provider.generate(request))
    .await
    .unwrap_or_else(|_| panic!("dashscope image generation timed out after 90s"))
    .unwrap_or_else(|e| panic!("dashscope image generation failed: {e}"));

  let first = response
    .images
    .first()
    .expect("dashscope image generation response must contain at least one image");
  assert!(
    first.url.as_deref().is_some_and(|v| !v.is_empty()),
    "dashscope image generation response must contain a url"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashscope_tts_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Audio) {
    eprintln!("[live] dashscope tts: skipped (YANSHI_LIVE_AUDIO_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["DASHSCOPE_API_KEY"]) else {
    eprintln!("[live] dashscope tts: skipped (DASHSCOPE_API_KEY not set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("DASHSCOPE_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  qwen-tts-2025-05-22:
    vendor: dashscope
    type: tts
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic qwen-tts-2025-05-22 entry");

  let provider = Yanshi::tts("qwen-tts-2025-05-22")
    .await
    .expect("qwen-tts-2025-05-22 resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "dashscope",
    "qwen-tts-2025-05-22 must route to dashscope"
  );

  let request = yanshi_llm::TtsRequest {
    model: "qwen-tts-2025-05-22".to_string(),
    input: "yanshi modality dispatcher test".to_string(),
    voice: "Cherry".to_string(),
    response_format: None,
    speed: None,
    volume: None,
    sample_rate: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(30), provider.synthesize(request))
    .await
    .unwrap_or_else(|_| panic!("dashscope TTS request timed out after 30s"))
    .unwrap_or_else(|e| panic!("dashscope TTS request failed: {e}"));

  assert!(
    !response.audio.is_empty(),
    "dashscope TTS response should contain audio bytes"
  );
}

// ============================================================================
// GLM image generation (CogView) + MiniMax TTS (T2A) via the modality
// dispatcher (P-LLM2.3 Batch 3) — Moonshot has no dedicated
// image/TTS/ASR REST endpoint and is not covered; GLM CogVideo (T2V) is
// deferred as its own follow-up.
// ============================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glm_image_generation_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Image) {
    eprintln!("[live] glm image: skipped (YANSHI_LIVE_IMAGE_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["GLM_API_KEY", "BIGMODEL_API_KEY", "ZHIPU_API_KEY"])
  else {
    eprintln!("[live] glm image: skipped (no GLM API key env var set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("GLM_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  glm-image:
    vendor: glm
    type: text_to_image
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic glm-image entry");

  let provider = Yanshi::text2image_for("glm-image")
    .await
    .expect("glm-image resolves through dispatcher");
  assert_eq!(provider.name(), "glm", "glm-image must route to glm");

  let request = yanshi_llm::Text2ImageRequest {
    model: "glm-image".to_string(),
    prompt: "a tiny red square icon on a white background".to_string(),
    size: Some("1280x1280".to_string()),
    n: None,
    response_format: None,
    seed: None,
    steps: None,
    cfg_scale: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(60), provider.generate(request))
    .await
    .unwrap_or_else(|_| panic!("glm image generation timed out after 60s"))
    .unwrap_or_else(|e| panic!("glm image generation failed: {e}"));

  let first = response
    .images
    .first()
    .expect("glm image generation response must contain at least one image");
  assert!(
    first.url.as_deref().is_some_and(|v| !v.is_empty()),
    "glm image generation response must contain a url"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minimax_tts_via_modality_dispatcher() {
  if !live_gate_enabled(LiveCapability::Audio) {
    eprintln!("[live] minimax tts: skipped (YANSHI_LIVE_AUDIO_TESTS not enabled)");
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["MINIMAX_API_KEY"]) else {
    eprintln!("[live] minimax tts: skipped (MINIMAX_API_KEY not set)");
    return;
  };
  // SAFETY: tests run with their own env scope.
  unsafe {
    std::env::set_var("MINIMAX_API_KEY", api_key);
  }

  let hermetic_registry_yaml = r#"
models:
  speech-2.8-hd:
    vendor: minimax
    type: tts
    accepts: [text]
"#;
  yanshi_llm::ModelRegistry::global()
    .load_config_from_yaml(hermetic_registry_yaml)
    .await
    .expect("seed hermetic speech-2.8-hd entry");

  let provider = Yanshi::tts("speech-2.8-hd")
    .await
    .expect("speech-2.8-hd resolves through dispatcher");
  assert_eq!(
    provider.name(),
    "minimax",
    "speech-2.8-hd must route to minimax"
  );

  let request = yanshi_llm::TtsRequest {
    model: "speech-2.8-hd".to_string(),
    input: "yanshi modality dispatcher test".to_string(),
    voice: "male-qn-qingse".to_string(),
    response_format: None,
    speed: None,
    volume: None,
    sample_rate: None,
  };
  let response = tokio::time::timeout(Duration::from_secs(30), provider.synthesize(request))
    .await
    .unwrap_or_else(|_| panic!("minimax TTS request timed out after 30s"))
    .unwrap_or_else(|e| panic!("minimax TTS request failed: {e}"));

  assert!(
    !response.audio.is_empty(),
    "minimax TTS response should contain audio bytes"
  );
}

fn silent_wav_one_second() -> Vec<u8> {
  // Minimal 1-second mono 8kHz silent WAV. Whisper happily accepts
  // empty audio; the transcript is usually an empty string.
  let mut wav = Vec::with_capacity(44 + 8000 * 2);
  wav.extend_from_slice(b"RIFF");
  wav.extend_from_slice(&(36u32 + 8000u32 * 2).to_le_bytes());
  wav.extend_from_slice(b"WAVE");
  wav.extend_from_slice(b"fmt ");
  wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
  wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
  wav.extend_from_slice(&1u16.to_le_bytes()); // mono
  wav.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
  wav.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
  wav.extend_from_slice(&2u16.to_le_bytes()); // block align
  wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
  wav.extend_from_slice(b"data");
  wav.extend_from_slice(&(8000u32 * 2).to_le_bytes());
  wav.extend(std::iter::repeat_n(0u8, 8000 * 2));
  wav
}

// ── Y6.6: text-to-video, one billed job per vendor ──────────────────────
//
// Gated by YANSHI_LIVE_VIDEO_TESTS (off in the nightly run: each job costs
// real money and takes minutes). Each submits the shortest, lowest
// resolution clip the vendor offers and waits for its URL.

async fn live_video(
  vendor: &str,
  key_env: &str,
  provider: impl FnOnce(&str) -> Box<dyn yanshi_llm::providers::modality::Text2VideoProvider>,
  request: yanshi_llm::providers::modality::Text2VideoRequest,
) {
  if !prepare_live_provider(vendor, LiveCapability::Video).await {
    return;
  }
  let Some((_, api_key)) = pick_api_key(&[key_env]) else {
    eprintln!("[live] {vendor} video: skipped ({key_env} not set)");
    return;
  };
  let provider = provider(&api_key);
  let done = provider
    .generate_and_wait(request, Duration::from_secs(10), Duration::from_secs(600))
    .await
    .unwrap_or_else(|e| panic!("{vendor} video generation failed: {e}"));
  let url = done
    .videos
    .first()
    .and_then(|v| v.url.as_deref())
    .unwrap_or_default();
  assert!(
    url.starts_with("http"),
    "{vendor}: no video url in {done:?}"
  );
}

fn live_video_request(model: &str) -> yanshi_llm::providers::modality::Text2VideoRequest {
  yanshi_llm::providers::modality::Text2VideoRequest {
    model: model.to_string(),
    prompt: "A paper boat drifting on a calm pond, soft morning light".to_string(),
    aspect_ratio: None,
    duration_seconds: None,
    resolution: None,
    seed: None,
    extra: None,
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashscope_wan_text_to_video_live() {
  let mut request = live_video_request("wan2.7-t2v");
  request.resolution = Some("720P".to_string());
  request.duration_seconds = Some(2);
  live_video(
    "dashscope",
    "DASHSCOPE_API_KEY",
    |key| {
      Box::new(
        yanshi_llm::providers::dashscope_media::DashScopeMediaProvider::with_client(
          no_proxy_client(),
          key,
          None,
        )
        .expect("provider"),
      )
    },
    request,
  )
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minimax_hailuo_text_to_video_live() {
  let mut request = live_video_request("MiniMax-Hailuo-2.3");
  request.resolution = Some("768P".to_string());
  request.duration_seconds = Some(6);
  live_video(
    "minimax",
    "MINIMAX_API_KEY",
    |key| {
      Box::new(
        yanshi_llm::providers::minimax_video::MiniMaxVideoProvider::with_client(
          no_proxy_client(),
          key,
          minimax_base_url_sync(),
        )
        .expect("provider"),
      )
    },
    request,
  )
  .await;
}

/// `MINIMAX_VIDEO_BASE_URL` for international accounts
/// (`https://api.minimax.io/v1`); the mainland host otherwise.
fn minimax_base_url_sync() -> Option<String> {
  std::env::var("MINIMAX_VIDEO_BASE_URL")
    .ok()
    .filter(|v| !v.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kling_text_to_video_live() {
  let mut request = live_video_request("kling-v2-5-turbo");
  request.duration_seconds = Some(5);
  live_video(
    "kling",
    "KLING_API_KEY",
    |key| {
      Box::new(
        yanshi_llm::providers::kling_video::KlingVideoProvider::with_client(
          no_proxy_client(),
          key,
          kling_base_url(),
        )
        .expect("provider"),
      )
    },
    request,
  )
  .await;
}

/// `KLING_VIDEO_BASE_URL` for international accounts
/// (`https://api-singapore.klingai.com`); the mainland host otherwise.
fn kling_base_url() -> Option<String> {
  std::env::var("KLING_VIDEO_BASE_URL")
    .ok()
    .filter(|v| !v.trim().is_empty())
}

// ── Y6.6: image-to-image, one call per vendor (YANSHI_LIVE_IMAGE_TESTS) ──

/// A stable, freely licensed photo (Wikimedia Commons) large enough for
/// every vendor's minimum input size.
const LIVE_SOURCE_IMAGE: &str =
  "https://upload.wikimedia.org/wikipedia/commons/thumb/3/3a/Cat03.jpg/640px-Cat03.jpg";

async fn live_image2image(
  vendor: &str,
  key_env: &str,
  provider: impl FnOnce(&str) -> Box<dyn yanshi_llm::providers::modality::Image2ImageProvider>,
  model: &str,
) {
  if !prepare_live_provider(vendor, LiveCapability::Image).await {
    return;
  }
  let Some((_, api_key)) = pick_api_key(&[key_env]) else {
    eprintln!("[live] {vendor} image-to-image: skipped ({key_env} not set)");
    return;
  };
  let request = yanshi_llm::providers::modality::Image2ImageRequest {
    model: model.to_string(),
    prompt: "Turn this photo into a watercolour painting".to_string(),
    source_url: LIVE_SOURCE_IMAGE.to_string(),
    source_weight: 0.6,
    size: None,
    n: Some(1),
    response_format: None,
    seed: None,
    steps: None,
    cfg_scale: None,
  };
  let response = tokio::time::timeout(
    Duration::from_secs(180),
    provider(&api_key).transform(request),
  )
  .await
  .unwrap_or_else(|_| panic!("{vendor} image-to-image timed out"))
  .unwrap_or_else(|e| panic!("{vendor} image-to-image failed: {e}"));
  let first = response.images.first().expect("one image");
  assert!(
    first.url.as_deref().is_some_and(|u| u.starts_with("http"))
      || first.b64_json.as_deref().is_some_and(|b| !b.is_empty()),
    "{vendor}: no image in the response"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_image_to_image_live() {
  live_image2image(
    "openai",
    "OPENAI_API_KEY",
    |key| {
      Box::new(
        yanshi_llm::providers::openai_images::OpenAIImageProvider::with_client(
          no_proxy_client(),
          key,
          None,
        )
        .expect("provider"),
      )
    },
    "gpt-image-1-mini",
  )
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashscope_wan_image_to_image_live() {
  live_image2image(
    "dashscope",
    "DASHSCOPE_API_KEY",
    |key| {
      Box::new(
        yanshi_llm::providers::dashscope_media::DashScopeMediaProvider::with_client(
          no_proxy_client(),
          key,
          None,
        )
        .expect("provider"),
      )
    },
    "wan2.5-i2i-preview",
  )
  .await;
}

// ── Y6.6 (M7): an image in a chat reply (YANSHI_LIVE_IMAGE_TESTS) ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_chat_reply_carries_an_image_live() {
  if !prepare_live_provider("google", LiveCapability::Image).await {
    return;
  }
  let Some((_, api_key)) = pick_api_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]) else {
    eprintln!("[live] google chat image: skipped (GEMINI_API_KEY not set)");
    return;
  };
  let provider = GoogleProvider::with_client(no_proxy_client(), &api_key, None).expect("provider");
  let mut request = provider_request("gemini-2.5-flash-image");
  request.messages = vec![
    json!({"role": "user", "content": "Draw a small red square on white. Reply with the image."}),
  ];
  let response = tokio::time::timeout(Duration::from_secs(120), provider.execute(&request))
    .await
    .expect("google chat image timed out")
    .expect("google chat image failed");
  let (_, media) = response.content.split_text_and_media();
  assert!(
    media.iter().any(
      |b| matches!(b, yanshi_llm::providers::ContentBlock::Image { data, .. } if !data.is_empty())
    ),
    "no image in the reply: {:?}",
    response.content.to_string()
  );
}
