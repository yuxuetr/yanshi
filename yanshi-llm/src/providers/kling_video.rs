//! Kling text-to-video (Y6.6).
//!
//! `POST {base_url}/v1/videos/text2video` → `data.task_id`; `GET
//! {base_url}/v1/videos/text2video/{task_id}` until `data.task_status`
//! is `succeed` (videos at `data.task_result.videos[].url`) or `failed`
//! (`data.task_status_msg`). Responses carry `code`, non-zero on error.
//!
//! Credentials: Kling issues either an API key, sent as the bearer token,
//! or a legacy access key / secret key pair, which signs a 30-minute
//! HS256 JWT (`iss` = access key). The registry has one key per vendor,
//! so a pair is written as `ACCESS_KEY:SECRET_KEY` in `KLING_API_KEY`.
//! A fresh token is signed per request: tokens expire, and signing is
//! cheap.
//!
//! The default base URL is the mainland host (`api-beijing.klingai.com`),
//! as for MiniMax; a key issued by the international site is rejected
//! there with 401 `api key not found`, so international accounts set the
//! model's `base_url` to `https://api-singapore.klingai.com`.

use super::modality::{
  GeneratedVideo, Text2VideoProvider, Text2VideoRequest, VideoGenerationResponse,
  VideoGenerationStatus, VideoGenerationTask,
};
use crate::{LLMError, Result};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::Sha256;

const ENDPOINT: &str = "/v1/videos/text2video";

/// How the provider authenticates.
enum Credentials {
  ApiKey(String),
  KeyPair { access: String, secret: String },
}

pub struct KlingVideoProvider {
  client: Client,
  credentials: Credentials,
  base_url: String,
}

impl std::fmt::Debug for KlingVideoProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("KlingVideoProvider")
      .field("base_url", &self.base_url)
      .field("credentials", &"<redacted>")
      .finish()
  }
}

/// A Kling JWT for `access` signed with `secret`, valid from five seconds
/// ago (clock skew) for thirty minutes.
pub(crate) fn sign_jwt(access: &str, secret: &str, now: u64) -> Result<String> {
  let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
  // Written out so the claim order is fixed whatever serde_json's map is.
  let claims = format!(
    r#"{{"iss":{},"exp":{},"nbf":{}}}"#,
    json!(access),
    now + 1800,
    now.saturating_sub(5)
  );
  let payload = URL_SAFE_NO_PAD.encode(claims);
  let signing_input = format!("{header}.{payload}");
  let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|e| {
    LLMError::ConfigurationError {
      message: format!("Kling secret key unusable for HS256: {e}"),
    }
  })?;
  mac.update(signing_input.as_bytes());
  let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
  Ok(format!("{signing_input}.{signature}"))
}

impl KlingVideoProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    let api_key = api_key.trim();
    let credentials = match api_key.split_once(':') {
      _ if api_key.is_empty() => {
        return Err(LLMError::MissingApiKey {
          provider: "kling".to_string(),
        });
      }
      Some((access, secret)) if !access.is_empty() && !secret.is_empty() => Credentials::KeyPair {
        access: access.to_string(),
        secret: secret.to_string(),
      },
      _ => Credentials::ApiKey(api_key.to_string()),
    };
    Ok(Self {
      client,
      credentials,
      base_url: base_url
        .unwrap_or_else(|| "https://api-beijing.klingai.com".to_string())
        .trim_end_matches('/')
        .to_string(),
    })
  }

  fn token(&self) -> Result<String> {
    match &self.credentials {
      Credentials::ApiKey(key) => Ok(key.clone()),
      Credentials::KeyPair { access, secret } => {
        let now = std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or_default();
        sign_jwt(access, secret, now)
      }
    }
  }
}

/// The JSON body of a 2xx response whose `code` is 0.
async fn checked_body(response: reqwest::Response) -> Result<Value> {
  let status = response.status();
  let text = response.text().await?;
  if !status.is_success() {
    return Err(LLMError::HttpError {
      status_code: status.as_u16(),
      message: text,
    });
  }
  let body: Value = serde_json::from_str(&text).map_err(|e| LLMError::ResponseParsingError {
    message: format!("Kling response JSON parse failed: {e}"),
  })?;
  match body["code"].as_i64() {
    Some(0) | None => Ok(body),
    Some(code) => Err(LLMError::ResponseParsingError {
      message: format!(
        "Kling error {code}: {}",
        body["message"].as_str().unwrap_or("")
      ),
    }),
  }
}

/// The create request. `duration` is a string in Kling's API; `extra`
/// (an object: `negative_prompt`, `mode`, `cfg_scale`, `sound`, …) is
/// merged in last. Kling has no resolution or seed field.
pub(crate) fn request_body(request: &Text2VideoRequest) -> Value {
  let mut body = serde_json::Map::new();
  body.insert("model_name".to_string(), json!(request.model));
  body.insert("prompt".to_string(), json!(request.prompt));
  if let Some(ratio) = &request.aspect_ratio {
    body.insert("aspect_ratio".to_string(), json!(ratio));
  }
  if let Some(duration) = request.duration_seconds {
    body.insert("duration".to_string(), json!(duration.to_string()));
  }
  if let Some(Value::Object(extra)) = &request.extra {
    body.extend(extra.clone());
  }
  Value::Object(body)
}

pub(crate) fn parse_status(body: &Value) -> Result<VideoGenerationStatus> {
  let data = &body["data"];
  match data["task_status"].as_str().unwrap_or_default() {
    "succeed" => {
      let videos: Vec<GeneratedVideo> = data["task_result"]["videos"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|video| {
          Some(GeneratedVideo {
            url: Some(video["url"].as_str()?.to_string()),
            b64_data: None,
            duration_seconds: video["duration"]
              .as_str()
              .and_then(|d| d.parse::<f64>().ok())
              .map(|d| d.round() as u32),
          })
        })
        .collect();
      if videos.is_empty() {
        return Err(LLMError::ResponseParsingError {
          message: format!("Kling task succeeded without videos: {body}"),
        });
      }
      Ok(VideoGenerationStatus::Completed(VideoGenerationResponse {
        created: data["updated_at"].as_u64().unwrap_or(0) / 1000,
        videos,
        metadata: Some(json!({ "task_id": data["task_id"], "request_id": body["request_id"] })),
      }))
    }
    "failed" => Ok(VideoGenerationStatus::Failed {
      message: data["task_status_msg"]
        .as_str()
        .unwrap_or("video generation failed")
        .to_string(),
    }),
    _ => Ok(VideoGenerationStatus::Pending),
  }
}

#[async_trait]
impl Text2VideoProvider for KlingVideoProvider {
  fn name(&self) -> &str {
    "kling"
  }

  async fn submit(&self, request: Text2VideoRequest) -> Result<VideoGenerationTask> {
    let response = self
      .client
      .post(format!("{}{ENDPOINT}", self.base_url))
      .bearer_auth(self.token()?)
      .json(&request_body(&request))
      .send()
      .await?;
    let body = checked_body(response).await?;
    let task_id = body["data"]["task_id"]
      .as_str()
      .filter(|id| !id.is_empty())
      .ok_or_else(|| LLMError::ResponseParsingError {
        message: format!("Kling submission returned no task_id: {body}"),
      })?;
    Ok(VideoGenerationTask {
      task_id: task_id.to_string(),
      metadata: None,
    })
  }

  async fn poll(&self, task: &VideoGenerationTask) -> Result<VideoGenerationStatus> {
    let response = self
      .client
      .get(format!("{}{ENDPOINT}/{}", self.base_url, task.task_id))
      .bearer_auth(self.token()?)
      .send()
      .await?;
    parse_status(&checked_body(response).await?)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn jwt_matches_an_independent_hs256_signature() {
    // Reference token computed with Python's hmac/hashlib for the same
    // header, claims and secret.
    let token = sign_jwt("ak-test", "sk-test", 1_700_000_000).expect("token");
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    let claims: Value =
      serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("b64")).expect("json");
    assert_eq!(
      claims,
      json!({"iss": "ak-test", "exp": 1_700_001_800u64, "nbf": 1_699_999_995u64})
    );
    assert_eq!(parts[2], REFERENCE_SIGNATURE);
  }

  #[test]
  fn statuses_and_request_shape() {
    assert!(matches!(
      parse_status(&json!({"code": 0, "data": {"task_status": "processing"}})).expect("ok"),
      VideoGenerationStatus::Pending
    ));
    assert!(matches!(
      parse_status(&json!({"data": {"task_status": "failed", "task_status_msg": "risk control"}})).expect("ok"),
      VideoGenerationStatus::Failed { message } if message == "risk control"
    ));
    match parse_status(&json!({"data": {"task_status": "succeed", "task_result": {"videos": [{"id": "v", "url": "https://x/v.mp4", "duration": "5.1"}]}}}))
      .expect("ok")
    {
      VideoGenerationStatus::Completed(done) => {
        assert_eq!(done.videos[0].url.as_deref(), Some("https://x/v.mp4"));
        assert_eq!(done.videos[0].duration_seconds, Some(5));
      }
      other => panic!("{other:?}"),
    }
    assert!(
      parse_status(&json!({"data": {"task_status": "succeed", "task_result": {"videos": []}}}))
        .is_err()
    );
    let body = request_body(&Text2VideoRequest {
      model: "kling-v2-1-master".to_string(),
      prompt: "a fox".to_string(),
      aspect_ratio: Some("16:9".to_string()),
      duration_seconds: Some(5),
      resolution: None,
      seed: None,
      extra: Some(json!({"mode": "pro"})),
    });
    assert_eq!(
      body,
      json!({"model_name": "kling-v2-1-master", "prompt": "a fox", "aspect_ratio": "16:9", "duration": "5", "mode": "pro"})
    );
  }

  const REFERENCE_SIGNATURE: &str = "tMljlG8Xy1G8AKj5WkEZRcRIADvbH0FP6wgzpwHAfs4";
}
