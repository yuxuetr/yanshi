//! Kling text-to-video (Y6.6).
//!
//! Kling runs two APIs, chosen here by model name:
//!
//! - The legacy one, for the `kling-v*` models: `POST
//!   {base_url}/v1/videos/text2video` with `model_name` → `data.task_id`;
//!   `GET {base_url}/v1/videos/text2video/{task_id}` until
//!   `data.task_status` is `succeed` (videos at
//!   `data.task_result.videos[].url`) or `failed` (`data.task_status_msg`).
//! - The current one, for everything else (`kling-4.0`,
//!   `kling-4.0-flash`): `POST {base_url}/text-to-video/{model}` with
//!   `prompt` + `settings` → `data.id`; `GET {base_url}/tasks?task_ids=`
//!   until `data[0].status` is `succeeded` (videos are the `outputs` of
//!   type `video`) or `failed` (`message`). Resource packs bought for
//!   these models are only drawn on by this API. Its task ids are handed
//!   out as `tasks/{id}` so [`poll`](Text2VideoProvider::poll) knows which
//!   API to ask.
//!
//! Responses of both carry `code`, non-zero on error.
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
/// Prefix of the task ids of the current API.
const TASKS_PREFIX: &str = "tasks/";

/// Whether `model` is served by the legacy API.
fn is_legacy(model: &str) -> bool {
  model.starts_with("kling-v")
}

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

/// The create request of the current API. `resolution` is lower-cased
/// (`720p`); `extra` (`options`, …) is merged in last.
pub(crate) fn request_body_v4(request: &Text2VideoRequest) -> Value {
  let mut settings = serde_json::Map::new();
  if let Some(duration) = request.duration_seconds {
    settings.insert("duration".to_string(), json!(duration));
  }
  if let Some(resolution) = &request.resolution {
    settings.insert("resolution".to_string(), json!(resolution.to_lowercase()));
  }
  if let Some(ratio) = &request.aspect_ratio {
    settings.insert("aspect_ratio".to_string(), json!(ratio));
  }
  let mut body = serde_json::Map::new();
  body.insert("prompt".to_string(), json!(request.prompt));
  if !settings.is_empty() {
    body.insert("settings".to_string(), Value::Object(settings));
  }
  if let Some(Value::Object(extra)) = &request.extra {
    body.extend(extra.clone());
  }
  Value::Object(body)
}

/// The status of task `id` in a `GET /tasks` response.
pub(crate) fn parse_status_v4(body: &Value, id: &str) -> Result<VideoGenerationStatus> {
  let task = body["data"]
    .as_array()
    .and_then(|tasks| tasks.iter().find(|task| task["id"].as_str() == Some(id)))
    .ok_or_else(|| LLMError::ResponseParsingError {
      message: format!("Kling task {id} missing from the query response: {body}"),
    })?;
  match task["status"].as_str().unwrap_or_default() {
    "succeeded" => {
      let videos: Vec<GeneratedVideo> = task["outputs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|output| output["type"] == "video")
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
        created: task["update_time"].as_u64().unwrap_or(0) / 1000,
        videos,
        metadata: Some(json!({
          "task_id": id,
          "request_id": body["request_id"],
          "usage": task["usage"],
          "billing": task["billing"],
        })),
      }))
    }
    "failed" => Ok(VideoGenerationStatus::Failed {
      message: task["message"]
        .as_str()
        .filter(|m| !m.is_empty())
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
    if !is_legacy(&request.model) {
      let response = self
        .client
        .post(format!("{}/text-to-video/{}", self.base_url, request.model))
        .bearer_auth(self.token()?)
        .json(&request_body_v4(&request))
        .send()
        .await?;
      let body = checked_body(response).await?;
      let id = body["data"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| LLMError::ResponseParsingError {
          message: format!("Kling submission returned no id: {body}"),
        })?;
      return Ok(VideoGenerationTask {
        task_id: format!("{TASKS_PREFIX}{id}"),
        metadata: None,
      });
    }
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
    if let Some(id) = task.task_id.strip_prefix(TASKS_PREFIX) {
      let response = self
        .client
        .get(format!("{}/tasks", self.base_url))
        .query(&[("task_ids", id)])
        .bearer_auth(self.token()?)
        .send()
        .await?;
      return parse_status_v4(&checked_body(response).await?, id);
    }
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

  #[test]
  fn the_current_api_serves_kling_4_and_reports_its_task_status() {
    assert!(is_legacy("kling-v2-5-turbo"));
    assert!(!is_legacy("kling-4.0-flash"));
    let body = request_body_v4(&Text2VideoRequest {
      model: "kling-4.0-flash".to_string(),
      prompt: "a fox".to_string(),
      aspect_ratio: Some("9:16".to_string()),
      duration_seconds: Some(3),
      resolution: Some("720P".to_string()),
      seed: None,
      extra: Some(json!({"options": {"watermark_info": {"enabled": false}}})),
    });
    assert_eq!(
      body,
      json!({"prompt": "a fox", "settings": {"duration": 3, "resolution": "720p", "aspect_ratio": "9:16"}, "options": {"watermark_info": {"enabled": false}}})
    );
    let tasks = |status: &str| {
      json!({"code": 0, "request_id": "r", "data": [{"id": "893", "status": status, "message": "risk control", "update_time": 1_781_080_794_151u64, "outputs": [
        {"type": "image", "url": "https://x/cover.png"},
        {"type": "video", "id": "v", "url": "https://x/v.mp4", "duration": "3.04"}
      ]}]})
    };
    assert!(matches!(
      parse_status_v4(&tasks("processing"), "893").expect("ok"),
      VideoGenerationStatus::Pending
    ));
    assert!(matches!(
      parse_status_v4(&tasks("failed"), "893").expect("ok"),
      VideoGenerationStatus::Failed { message } if message == "risk control"
    ));
    match parse_status_v4(&tasks("succeeded"), "893").expect("ok") {
      VideoGenerationStatus::Completed(done) => {
        assert_eq!(done.videos.len(), 1);
        assert_eq!(done.videos[0].url.as_deref(), Some("https://x/v.mp4"));
        assert_eq!(done.videos[0].duration_seconds, Some(3));
        assert_eq!(done.created, 1_781_080_794);
      }
      other => panic!("{other:?}"),
    }
    assert!(parse_status_v4(&tasks("succeeded"), "other").is_err());
  }

  const REFERENCE_SIGNATURE: &str = "tMljlG8Xy1G8AKj5WkEZRcRIADvbH0FP6wgzpwHAfs4";
}
