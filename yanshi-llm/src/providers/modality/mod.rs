//! Per-modality provider traits for non-chat APIs.
//!
//! Chat-shaped models route through `LLMProvider` (in
//! [`crate::providers`]). The five trait surfaces here cover the
//! non-chat modalities Yanshi ships nodes for:
//!
//! - [`asr::AsrProvider`] — audio → text (Whisper, StepFun ASR)
//! - [`tts::TtsProvider`] — text → audio (StepFun TTS, ElevenLabs)
//! - [`text_to_image::Text2ImageProvider`] — text → image
//!   (DALL-E, Imagen, StepFun T2I)
//! - [`image_to_image::Image2ImageProvider`] — image+text → image
//! - [`image_edit::ImageEditProvider`] — image+text → image
//!   (DALL-E edit, StepFun image edit)
//! - [`text_to_video::Text2VideoProvider`] — text → video (Google Veo).
//!   Unlike the other five, this is an async job API (submit + poll),
//!   not a synchronous request/response.
//!
//! Each trait is intentionally narrow — modality shapes diverge enough
//! that a single "MultimodalProvider" abstraction would leak. Common
//! response shape (URL or base64 image bytes) is shared via
//! [`ImageGenerationResponse`] / [`GeneratedImage`].
//!
//! ## P-LLM.1 contract
//!
//! These traits are the seam that let `yanshi-nodes-ai::{asr, tts,
//! text_to_image, image_to_image, image_edit}` drop their direct
//! StepFun coupling (the pre-P-LLM.3 code called
//! `Yanshi::stepfun_client(...)` and `providers::stepfun::*Request`
//! types directly). P-LLM.3 landed this: every one of those nodes now
//! calls the vendor-agnostic `Yanshi::{asr,tts,text2image_for,
//! image2image,image_edit}` entry points (`crate::modality_dispatch`),
//! which route through these traits to resolve the right vendor by
//! model name — no node references StepFun (or any other vendor) by
//! name.

pub mod asr;
pub mod image_edit;
pub mod image_to_image;
pub mod text_to_image;
pub mod text_to_video;
pub mod tts;

pub use asr::{AsrProvider, AsrRequest, AsrResponse};
pub use image_edit::{ImageEditProvider, ImageEditRequest};
pub use image_to_image::{Image2ImageProvider, Image2ImageRequest};
pub use text_to_image::{Text2ImageProvider, Text2ImageRequest};
pub use text_to_video::{
  GeneratedVideo, Text2VideoProvider, Text2VideoRequest, VideoGenerationResponse,
  VideoGenerationStatus, VideoGenerationTask,
};
pub use tts::{TtsProvider, TtsRequest, TtsResponse};

use serde::{Deserialize, Serialize};

/// Download `url` into memory, refusing to buffer more than `max_bytes`
/// (Y1.1). Vendors hand back time-limited URLs for images and videos;
/// turning those into on-disk artifacts means fetching them inside the
/// node, and an unbounded fetch of an attacker-chosen or mis-sized body
/// is the one thing that must not happen there. The cap is checked
/// against `Content-Length` up front and again while streaming, since
/// the header is optional and untrusted.
pub async fn download_with_cap(
  client: &reqwest::Client,
  url: &str,
  headers: reqwest::header::HeaderMap,
  max_bytes: u64,
) -> crate::Result<Vec<u8>> {
  let mut response = client.get(url).headers(headers).send().await?;
  if !response.status().is_success() {
    let status_code = response.status().as_u16();
    let message = response.text().await.unwrap_or_default();
    return Err(crate::LLMError::HttpError {
      status_code,
      message,
    });
  }
  if let Some(declared) = response.content_length()
    && declared > max_bytes
  {
    return Err(crate::LLMError::ResponseParsingError {
      message: format!(
        "download of {url} declares {declared} bytes, over the {max_bytes}-byte cap"
      ),
    });
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await? {
    if body.len() as u64 + chunk.len() as u64 > max_bytes {
      return Err(crate::LLMError::ResponseParsingError {
        message: format!("download of {url} exceeded the {max_bytes}-byte cap"),
      });
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

/// [`download_with_cap`] with the crate's default HTTP client and no
/// extra headers, returning the response's `Content-Type` alongside the
/// bytes so a caller can label what it stores. For callers (nodes) that
/// do not otherwise depend on `reqwest`.
pub async fn download_with_cap_default(
  url: &str,
  max_bytes: u64,
) -> crate::Result<(Vec<u8>, Option<String>)> {
  let client = crate::providers::default_http_client()?;
  let mut response = client.get(url).send().await?;
  if !response.status().is_success() {
    let status_code = response.status().as_u16();
    let message = response.text().await.unwrap_or_default();
    return Err(crate::LLMError::HttpError {
      status_code,
      message,
    });
  }
  let mime = response
    .headers()
    .get(reqwest::header::CONTENT_TYPE)
    .and_then(|v| v.to_str().ok())
    .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
    .filter(|v| !v.is_empty());
  if let Some(declared) = response.content_length()
    && declared > max_bytes
  {
    return Err(crate::LLMError::ResponseParsingError {
      message: format!(
        "download of {url} declares {declared} bytes, over the {max_bytes}-byte cap"
      ),
    });
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await? {
    if body.len() as u64 + chunk.len() as u64 > max_bytes {
      return Err(crate::LLMError::ResponseParsingError {
        message: format!("download of {url} exceeded the {max_bytes}-byte cap"),
      });
    }
    body.extend_from_slice(&chunk);
  }
  Ok((body, mime))
}

/// Common response envelope for image-generating endpoints.
///
/// Used by `Text2ImageProvider`, `Image2ImageProvider`, and
/// `ImageEditProvider` since vendor responses for these three share a
/// shape (a list of images, each either a URL or a base64 blob, plus
/// a creation timestamp). Vendor-specific bookkeeping that doesn't fit
/// here belongs in `metadata`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationResponse {
  /// Unix timestamp the provider says the image was created.
  pub created: u64,
  /// Generated images. Length depends on the request's `n` field.
  pub images: Vec<GeneratedImage>,
  /// Vendor-specific extras (request id, billing info, etc.) preserved
  /// for caller inspection without forcing every consumer to know
  /// about them.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metadata: Option<serde_json::Value>,
}

/// A single image entry in an [`ImageGenerationResponse`].
///
/// Exactly one of `url` / `b64_json` is populated, mirroring the
/// `response_format: "url" | "b64_json"` choice in the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedImage {
  /// HTTP URL the vendor will serve the image from (time-limited on
  /// most providers).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
  /// Base64-encoded image bytes (no `data:` prefix).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub b64_json: Option<String>,
  /// Seed used to generate this image, when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seed: Option<i32>,
}
