//! MiniMax text-to-video (Hailuo, Y6.6).
//!
//! Three calls, per the official API reference: `POST
//! {base_url}/video_generation` → `task_id`; `GET
//! {base_url}/query/video_generation?task_id=` until `status` is
//! `Success` (→ `file_id`) or `Fail`; `GET
//! {base_url}/files/retrieve?file_id=` → `file.download_url` (valid one
//! hour), done inside [`poll`](Text2VideoProvider::poll) so the
//! completed status carries a URL like every other video vendor. Every
//! response carries `base_resp.status_code`, non-zero on error (1002 rate
//! limit, 1004 auth, 1026/1027 sensitive content).
//!
//! The default base URL is the mainland host the registry's `minimax`
//! provider uses (`api.minimaxi.com`); international accounts set the
//! model's `base_url` to `https://api.minimax.io/v1`, as for TTS.

use super::modality::{
  GeneratedVideo, Text2VideoProvider, Text2VideoRequest, VideoGenerationResponse,
  VideoGenerationStatus, VideoGenerationTask,
};
use crate::{LLMError, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

pub struct MiniMaxVideoProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl std::fmt::Debug for MiniMaxVideoProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MiniMaxVideoProvider")
      .field("base_url", &self.base_url)
      .field("api_key", &"<redacted>")
      .finish()
  }
}

impl MiniMaxVideoProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "minimax".to_string(),
      });
    }
    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url: base_url
        .unwrap_or_else(|| "https://api.minimaxi.com/v1".to_string())
        .trim_end_matches('/')
        .to_string(),
    })
  }

  async fn get_json(&self, url: &str, query: &[(&str, &str)]) -> Result<Value> {
    let response = self
      .client
      .get(url)
      .query(query)
      .bearer_auth(&self.api_key)
      .send()
      .await?;
    checked_body(response).await
  }
}

/// The JSON body of a 2xx response whose `base_resp.status_code` is 0.
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
    message: format!("MiniMax video response JSON parse failed: {e}"),
  })?;
  check_base_resp(&body)?;
  Ok(body)
}

/// `base_resp.status_code != 0` is an error even under HTTP 200; 1002
/// (rate limited) is surfaced as HTTP 429 so callers' retry logic treats
/// it as transient.
pub(crate) fn check_base_resp(body: &Value) -> Result<()> {
  let code = body["base_resp"]["status_code"].as_i64().unwrap_or(0);
  if code == 0 {
    return Ok(());
  }
  let message = format!(
    "MiniMax error {code}: {}",
    body["base_resp"]["status_msg"].as_str().unwrap_or("")
  );
  Err(match code {
    1002 => LLMError::HttpError {
      status_code: 429,
      message,
    },
    1004 => LLMError::HttpError {
      status_code: 401,
      message,
    },
    _ => LLMError::ResponseParsingError { message },
  })
}

/// The create request: `duration` / `resolution` pass through
/// (`768P` / `1080P`; MiniMax has no aspect-ratio or seed field), `extra`
/// (an object, e.g. `prompt_optimizer`) is merged in last.
pub(crate) fn request_body(request: &Text2VideoRequest) -> Value {
  let mut body = serde_json::Map::new();
  body.insert("model".to_string(), json!(request.model));
  body.insert("prompt".to_string(), json!(request.prompt));
  if let Some(duration) = request.duration_seconds {
    body.insert("duration".to_string(), json!(duration));
  }
  if let Some(resolution) = &request.resolution {
    body.insert("resolution".to_string(), json!(resolution.to_uppercase()));
  }
  if let Some(Value::Object(extra)) = &request.extra {
    body.extend(extra.clone());
  }
  Value::Object(body)
}

/// What a status query says: still running, failed, or done with a file.
#[derive(Debug, PartialEq)]
pub(crate) enum QueryOutcome {
  Pending,
  Failed(String),
  Done(String),
}

pub(crate) fn parse_query(body: &Value) -> Result<QueryOutcome> {
  match body["status"].as_str().unwrap_or_default() {
    "Success" => {
      // `file_id` is documented as a string but has shipped as a number.
      let file_id = match &body["file_id"] {
        Value::String(id) if !id.is_empty() => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => {
          return Err(LLMError::ResponseParsingError {
            message: "MiniMax video task succeeded without a file_id".to_string(),
          });
        }
      };
      Ok(QueryOutcome::Done(file_id))
    }
    "Fail" => Ok(QueryOutcome::Failed(
      body["base_resp"]["status_msg"]
        .as_str()
        .unwrap_or("video generation failed")
        .to_string(),
    )),
    _ => Ok(QueryOutcome::Pending),
  }
}

#[async_trait]
impl Text2VideoProvider for MiniMaxVideoProvider {
  fn name(&self) -> &str {
    "minimax"
  }

  async fn submit(&self, request: Text2VideoRequest) -> Result<VideoGenerationTask> {
    let response = self
      .client
      .post(format!("{}/video_generation", self.base_url))
      .bearer_auth(&self.api_key)
      .json(&request_body(&request))
      .send()
      .await?;
    let body = checked_body(response).await?;
    let task_id = match &body["task_id"] {
      Value::String(id) if !id.is_empty() => id.clone(),
      Value::Number(id) => id.to_string(),
      _ => {
        return Err(LLMError::ResponseParsingError {
          message: format!("MiniMax video submission returned no task_id: {body}"),
        });
      }
    };
    Ok(VideoGenerationTask {
      task_id,
      metadata: None,
    })
  }

  async fn poll(&self, task: &VideoGenerationTask) -> Result<VideoGenerationStatus> {
    let body = self
      .get_json(
        &format!("{}/query/video_generation", self.base_url),
        &[("task_id", task.task_id.as_str())],
      )
      .await?;
    let file_id = match parse_query(&body)? {
      QueryOutcome::Pending => return Ok(VideoGenerationStatus::Pending),
      QueryOutcome::Failed(message) => return Ok(VideoGenerationStatus::Failed { message }),
      QueryOutcome::Done(file_id) => file_id,
    };
    let file = self
      .get_json(
        &format!("{}/files/retrieve", self.base_url),
        &[("file_id", file_id.as_str())],
      )
      .await?;
    let url =
      file["file"]["download_url"]
        .as_str()
        .ok_or_else(|| LLMError::ResponseParsingError {
          message: format!("MiniMax file {file_id} has no download_url"),
        })?;
    Ok(VideoGenerationStatus::Completed(VideoGenerationResponse {
      created: file["file"]["created_at"].as_u64().unwrap_or(0),
      videos: vec![GeneratedVideo {
        url: Some(url.to_string()),
        b64_data: None,
        duration_seconds: None,
      }],
      metadata: Some(json!({
        "file_id": file_id,
        "width": body["video_width"],
        "height": body["video_height"],
      })),
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn query_states_and_error_codes() {
    assert_eq!(
      parse_query(&json!({"status": "Queueing"})).expect("ok"),
      QueryOutcome::Pending
    );
    assert_eq!(
      parse_query(&json!({"status": "Success", "file_id": 176844028768320u64})).expect("ok"),
      QueryOutcome::Done("176844028768320".to_string())
    );
    assert!(matches!(
      parse_query(&json!({"status": "Fail", "base_resp": {"status_code": 1027, "status_msg": "sensitive"}})).expect("ok"),
      QueryOutcome::Failed(m) if m == "sensitive"
    ));
    assert!(parse_query(&json!({"status": "Success"})).is_err());
    assert!(matches!(
      check_base_resp(&json!({"base_resp": {"status_code": 1002, "status_msg": "rate"}})),
      Err(LLMError::HttpError {
        status_code: 429,
        ..
      })
    ));
    assert!(check_base_resp(&json!({"base_resp": {"status_code": 0}})).is_ok());
  }

  #[test]
  fn request_maps_duration_and_resolution() {
    let body = request_body(&Text2VideoRequest {
      model: "MiniMax-Hailuo-2.3".to_string(),
      prompt: "a fox".to_string(),
      aspect_ratio: Some("16:9".to_string()),
      duration_seconds: Some(6),
      resolution: Some("1080p".to_string()),
      seed: Some(1),
      extra: Some(json!({"prompt_optimizer": false})),
    });
    assert_eq!(
      body,
      json!({"model": "MiniMax-Hailuo-2.3", "prompt": "a fox", "duration": 6, "resolution": "1080P", "prompt_optimizer": false})
    );
  }
}
