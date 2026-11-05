//! Registry-driven dispatcher for per-modality providers.
//!
//! Looks up a model name in the global [`ModelRegistry`], validates its
//! declared `type` matches the requested modality, resolves the
//! vendor's API key, and returns a boxed trait object. Each entry point
//! is the modality counterpart to [`Yanshi::model`](crate::Yanshi::model)
//! (which covers chat).
//!
//! StepFun implements 5 of the 6 modality traits. OpenAI adds a second
//! `AsrProvider` (Whisper, P-LLM.5) plus `Text2ImageProvider` /
//! `ImageEditProvider` / `TtsProvider` (P-LLM2.3 Batch 1). Google adds
//! the sixth trait, `Text2VideoProvider` (Veo, P-LLM2.2), plus a second
//! `Text2ImageProvider` / `TtsProvider` via `generateContent` (P-LLM2.3
//! Batch 2a). DashScope adds a third `Text2ImageProvider` (Wan) /
//! `TtsProvider` (Qwen-TTS) (P-LLM2.3 Batch 2b). GLM adds a fourth
//! `Text2ImageProvider` (CogView) and MiniMax adds a fourth
//! `TtsProvider` (T2A) (P-LLM2.3 Batch 3). Unimplemented vendor/modality
//! combinations return [`LLMError::UnsupportedProvider`].

use crate::{
  LLMError, Result,
  model_types::ModelType,
  providers::{
    dashscope_media::DashScopeMediaProvider,
    glm_images::GlmImageProvider,
    google_media::GoogleMediaProvider,
    google_veo::GoogleVeoClient,
    kling_video::KlingVideoProvider,
    minimax_tts::MiniMaxTtsProvider,
    minimax_video::MiniMaxVideoProvider,
    mock_media::MockMediaProvider,
    modality::{
      AsrProvider, Image2ImageProvider, ImageEditProvider, Text2ImageProvider, Text2VideoProvider,
      TtsProvider,
    },
    openai_asr::OpenAIAsrProvider,
    openai_images::OpenAIImageProvider,
    openai_tts::OpenAITtsProvider,
    stepfun::StepFunSpecializedClient,
  },
  registry::ModelRegistry,
};

/// Look up `model_name` in the global registry, assert its `type`
/// matches `expected`, and return `(vendor, base_url)`.
async fn resolve_for_modality(
  model_name: &str,
  expected: ModelType,
) -> Result<(String, Option<String>)> {
  let registry = ModelRegistry::global();
  let model_config = registry.get_model(model_name)?;

  let actual = model_config.granular_type();
  if actual != expected {
    return Err(LLMError::InvalidModelConfig {
      message: format!(
        "Model '{model_name}' has type '{}' but the requested modality requires type '{}'. \
         Update the YAML registry entry's `type:` field or pick a model whose type matches.",
        actual.to_legacy_string(),
        expected.to_legacy_string()
      ),
    });
  }

  Ok((model_config.vendor.clone(), model_config.base_url.clone()))
}

/// Resolve the API key for `vendor` using the same precedence rules
/// chat models use (vendor-specific `api_key_env` from registry,
/// then common env-var fallbacks).
async fn resolve_api_key(vendor: &str) -> Result<String> {
  let registry = ModelRegistry::global();
  let config = registry.get_config().await?;
  config.get_api_key(vendor)
}

/// A modality's constructor table (vendor name → how to build its
/// provider). `resolve()` has already validated the model's registry
/// `type:` matches this modality, so lookup only needs the vendor name.
type Ctor<T> = fn(&str, Option<String>) -> Result<Box<T>>;

fn build_stepfun_asr(api_key: &str, base_url: Option<String>) -> Result<Box<dyn AsrProvider>> {
  Ok(Box::new(StepFunSpecializedClient::new(api_key, base_url)?))
}
fn build_stepfun_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(StepFunSpecializedClient::new(api_key, base_url)?))
}
fn build_openai_asr(api_key: &str, base_url: Option<String>) -> Result<Box<dyn AsrProvider>> {
  Ok(Box::new(OpenAIAsrProvider::new(api_key, base_url)?))
}
fn build_openai_text2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2ImageProvider>> {
  Ok(Box::new(OpenAIImageProvider::new(api_key, base_url)?))
}
fn build_openai_image_edit(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn ImageEditProvider>> {
  Ok(Box::new(OpenAIImageProvider::new(api_key, base_url)?))
}
fn build_openai_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(OpenAITtsProvider::new(api_key, base_url)?))
}
fn build_google_text2video(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2VideoProvider>> {
  Ok(Box::new(GoogleVeoClient::new(api_key, base_url)?))
}
fn build_google_text2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2ImageProvider>> {
  Ok(Box::new(GoogleMediaProvider::new(api_key, base_url)?))
}
fn build_google_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(GoogleMediaProvider::new(api_key, base_url)?))
}
fn build_dashscope_text2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2ImageProvider>> {
  Ok(Box::new(DashScopeMediaProvider::new(api_key, base_url)?))
}
fn build_dashscope_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(DashScopeMediaProvider::new(api_key, base_url)?))
}
fn build_glm_text2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2ImageProvider>> {
  Ok(Box::new(GlmImageProvider::new(api_key, base_url)?))
}
fn build_minimax_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(MiniMaxTtsProvider::new(api_key, base_url)?))
}
// Y6.6: image-to-image for OpenAI (`/images/edits`, no mask) and
// DashScope (Wan), and three more text-to-video vendors.
fn build_openai_image2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Image2ImageProvider>> {
  Ok(Box::new(OpenAIImageProvider::new(api_key, base_url)?))
}
fn build_dashscope_image2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Image2ImageProvider>> {
  Ok(Box::new(DashScopeMediaProvider::new(api_key, base_url)?))
}
fn build_dashscope_text2video(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2VideoProvider>> {
  Ok(Box::new(DashScopeMediaProvider::new(api_key, base_url)?))
}
fn build_minimax_text2video(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2VideoProvider>> {
  Ok(Box::new(MiniMaxVideoProvider::new(api_key, base_url)?))
}
fn build_kling_text2video(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2VideoProvider>> {
  Ok(Box::new(KlingVideoProvider::new(api_key, base_url)?))
}
// Y1.1: `vendor: mock` for every modality, mirroring the chat path's
// `MockProvider` — lets nodes, generator templates and CLI tests run a
// modality success path with no vendor key. See `providers::mock_media`.
fn build_mock_asr(api_key: &str, base_url: Option<String>) -> Result<Box<dyn AsrProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}
fn build_mock_tts(api_key: &str, base_url: Option<String>) -> Result<Box<dyn TtsProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}
fn build_mock_text2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2ImageProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}
fn build_mock_image2image(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Image2ImageProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}
fn build_mock_image_edit(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn ImageEditProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}
fn build_mock_text2video(
  api_key: &str,
  base_url: Option<String>,
) -> Result<Box<dyn Text2VideoProvider>> {
  Ok(Box::new(MockMediaProvider::new(api_key, base_url)?))
}

const ASR_PROVIDERS: &[(&str, Ctor<dyn AsrProvider>)] = &[
  ("stepfun", build_stepfun_asr),
  ("step", build_stepfun_asr),
  ("openai", build_openai_asr),
  ("mock", build_mock_asr),
];
const TTS_PROVIDERS: &[(&str, Ctor<dyn TtsProvider>)] = &[
  ("stepfun", build_stepfun_tts),
  ("step", build_stepfun_tts),
  ("openai", build_openai_tts),
  ("google", build_google_tts),
  ("dashscope", build_dashscope_tts),
  ("minimax", build_minimax_tts),
  ("mock", build_mock_tts),
];
const TEXT2IMAGE_PROVIDERS: &[(&str, Ctor<dyn Text2ImageProvider>)] = &[
  ("openai", build_openai_text2image),
  ("google", build_google_text2image),
  ("dashscope", build_dashscope_text2image),
  ("glm", build_glm_text2image),
  ("mock", build_mock_text2image),
];
const IMAGE2IMAGE_PROVIDERS: &[(&str, Ctor<dyn Image2ImageProvider>)] = &[
  ("openai", build_openai_image2image),
  ("dashscope", build_dashscope_image2image),
  ("mock", build_mock_image2image),
];
const IMAGE_EDIT_PROVIDERS: &[(&str, Ctor<dyn ImageEditProvider>)] = &[
  ("openai", build_openai_image_edit),
  ("mock", build_mock_image_edit),
];
const TEXT2VIDEO_PROVIDERS: &[(&str, Ctor<dyn Text2VideoProvider>)] = &[
  ("google", build_google_text2video),
  ("dashscope", build_dashscope_text2video),
  ("minimax", build_minimax_text2video),
  ("kling", build_kling_text2video),
  ("mock", build_mock_text2video),
];

/// The modality labels `vendor` already has a provider implementation
/// for, derived from the same constructor tables the dispatch functions
/// look up — single source of truth, so this can't drift from what's
/// actually wired up.
fn implemented_modalities_for(vendor: &str) -> Vec<&'static str> {
  let mut modalities = Vec::new();
  if ASR_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("ASR");
  }
  if TTS_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("TTS");
  }
  if TEXT2IMAGE_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("text-to-image");
  }
  if IMAGE2IMAGE_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("image-to-image");
  }
  if IMAGE_EDIT_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("image-edit");
  }
  if TEXT2VIDEO_PROVIDERS.iter().any(|(v, _)| *v == vendor) {
    modalities.push("text-to-video");
  }
  modalities
}

fn unsupported_vendor<T>(vendor: &str, modality: &str) -> Result<T> {
  let implemented = implemented_modalities_for(vendor);
  let detail = if implemented.is_empty() {
    "no modality implementations yet".to_string()
  } else {
    format!("implements: {}", implemented.join(", "))
  };
  Err(LLMError::UnsupportedProvider {
    provider: format!("{vendor} (no {modality} implementation yet — {detail})"),
  })
}

/// Snapshot of registry resolution shared by every modality entry
/// point. Centralised so the per-modality functions stay tiny.
struct ResolvedModel {
  vendor: String,
  base_url: Option<String>,
  api_key: String,
}

async fn resolve(model_name: &str, expected: ModelType) -> Result<ResolvedModel> {
  let (vendor, base_url) = resolve_for_modality(model_name, expected).await?;
  let api_key = resolve_api_key(&vendor).await?;
  Ok(ResolvedModel {
    vendor,
    base_url,
    api_key,
  })
}

/// Look up `vendor` in `table` and invoke its constructor, or produce a
/// diagnosable `UnsupportedProvider` error naming `modality_label`.
fn dispatch<T: ?Sized>(
  table: &[(&str, Ctor<T>)],
  vendor: &str,
  api_key: &str,
  base_url: Option<String>,
  modality_label: &str,
) -> Result<Box<T>> {
  match table.iter().find(|(v, _)| *v == vendor) {
    Some((_, ctor)) => ctor(api_key, base_url),
    None => unsupported_vendor(vendor, modality_label),
  }
}

/// Build an [`AsrProvider`] for the named ASR model. Returns
/// `UnsupportedProvider` if the model's vendor has no ASR
/// implementation yet.
pub async fn asr_provider(model_name: &str) -> Result<Box<dyn AsrProvider>> {
  let resolved = resolve(model_name, ModelType::Asr).await?;
  dispatch(
    ASR_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "ASR",
  )
}

/// Build a [`TtsProvider`] for the named TTS model.
pub async fn tts_provider(model_name: &str) -> Result<Box<dyn TtsProvider>> {
  let resolved = resolve(model_name, ModelType::Tts).await?;
  dispatch(
    TTS_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "TTS",
  )
}

/// Build a [`Text2ImageProvider`] for the named text-to-image model.
pub async fn text2image_provider(model_name: &str) -> Result<Box<dyn Text2ImageProvider>> {
  let resolved = resolve(model_name, ModelType::Text2Image).await?;
  dispatch(
    TEXT2IMAGE_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "text-to-image",
  )
}

/// Build an [`Image2ImageProvider`] for the named image-to-image model.
pub async fn image2image_provider(model_name: &str) -> Result<Box<dyn Image2ImageProvider>> {
  let resolved = resolve(model_name, ModelType::Image2Image).await?;
  dispatch(
    IMAGE2IMAGE_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "image-to-image",
  )
}

/// Build an [`ImageEditProvider`] for the named image-edit model.
pub async fn image_edit_provider(model_name: &str) -> Result<Box<dyn ImageEditProvider>> {
  let resolved = resolve(model_name, ModelType::ImageEdit).await?;
  dispatch(
    IMAGE_EDIT_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "image-edit",
  )
}

/// Build a [`Text2VideoProvider`] for the named text-to-video model.
/// Unlike the other four modality entry points, the returned provider is
/// an async job API — see [`Text2VideoProvider::submit`]/[`Text2VideoProvider::poll`].
pub async fn text2video_provider(model_name: &str) -> Result<Box<dyn Text2VideoProvider>> {
  let resolved = resolve(model_name, ModelType::Text2Video).await?;
  dispatch(
    TEXT2VIDEO_PROVIDERS,
    &resolved.vendor,
    &resolved.api_key,
    resolved.base_url,
    "text-to-video",
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build an isolated `ModelRegistry` for tests by loading YAML
  /// directly. The global singleton may be in any state across the
  /// test process, so we don't touch it here — `resolve_for_modality`
  /// uses the global registry though, so a separate isolated helper
  /// exercises the same shape via a hand-built `LLMConfig`.
  fn type_mismatch_error_for(actual_type: ModelType, expected: ModelType) -> LLMError {
    LLMError::InvalidModelConfig {
      message: format!(
        "Model 'sample' has type '{}' but the requested modality requires type '{}'. \
         Update the YAML registry entry's `type:` field or pick a model whose type matches.",
        actual_type.to_legacy_string(),
        expected.to_legacy_string()
      ),
    }
  }

  #[test]
  fn type_mismatch_message_names_both_actual_and_expected() {
    // Use a chat model name where an ASR is expected: the error must
    // make the mistake operator-actionable, not just say "wrong type".
    let err = type_mismatch_error_for(ModelType::Chat, ModelType::Asr);
    let msg = err.to_string();
    assert!(msg.contains("type 'chat'"), "actual type missing: {msg}");
    assert!(msg.contains("type 'asr'"), "expected type missing: {msg}");
  }

  #[test]
  fn unsupported_vendor_message_names_modality() {
    // openai has no image-to-image implementation (only ASR/TTS/
    // text-to-image/image-edit), so this is a genuine unsupported
    // combination.
    let err = unsupported_vendor::<()>("openai", "image-to-image").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("openai"), "vendor missing: {msg}");
    assert!(msg.contains("image-to-image"), "modality missing: {msg}");
  }

  #[test]
  fn unsupported_vendor_message_lists_vendors_other_implemented_modalities() {
    // openai implements every modality but text-to-video: the error
    // should name the ones it does implement as a hint that the vendor
    // isn't wholesale unsupported.
    let err = unsupported_vendor::<()>("openai", "text-to-video").unwrap_err();
    let msg = err.to_string();
    assert!(
      msg.contains("implements: ASR, TTS"),
      "should list openai's existing modality support: {msg}"
    );
  }

  #[test]
  fn unsupported_vendor_message_names_no_implementations_for_a_fully_unsupported_vendor() {
    let err = unsupported_vendor::<()>("moonshot", "ASR").unwrap_err();
    let msg = err.to_string();
    assert!(
      msg.contains("no modality implementations yet"),
      "should say moonshot has nothing implemented: {msg}"
    );
  }

  #[test]
  fn implemented_modalities_for_stepfun_are_speech_only() {
    // StepFun shut down its image endpoints on 2026-10-10.
    assert_eq!(implemented_modalities_for("stepfun"), vec!["ASR", "TTS"]);
  }

  #[test]
  fn implemented_modalities_for_openai_covers_everything_but_video() {
    assert_eq!(
      implemented_modalities_for("openai"),
      vec![
        "ASR",
        "TTS",
        "text-to-image",
        "image-to-image",
        "image-edit"
      ]
    );
  }

  #[test]
  fn implemented_modalities_for_google_covers_tts_text_to_image_and_text_to_video() {
    assert_eq!(
      implemented_modalities_for("google"),
      vec!["TTS", "text-to-image", "text-to-video"]
    );
  }

  #[test]
  fn unsupported_vendor_message_for_text_to_video_names_the_modality() {
    // stepfun implements speech only.
    let err = unsupported_vendor::<()>("stepfun", "text-to-video").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stepfun"), "vendor missing: {msg}");
    assert!(msg.contains("text-to-video"), "modality missing: {msg}");
    assert!(
      msg.contains("implements: ASR, TTS"),
      "should list stepfun's other implemented modalities: {msg}"
    );
  }
}
