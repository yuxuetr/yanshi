use super::line_buffer::LineBuffer;
use crate::{
  LLMError, ResponseFormat, Result,
  client::streaming::{StreamChunk, StreamingResponse, TokenUsage, ToolCallDelta},
  providers::{ContentBlock, ContentType, LLMProvider, ProviderRequest, ProviderResponse},
  thinking::ThinkingConfig,
  tool_calling::{StopReason, ToolCallRequest, ToolChoice, ToolSpec},
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::pin::Pin;
use tokio_stream::Stream;

pub struct GoogleProvider {
  client: Client,
  api_key: String,
  base_url: String,
}

impl GoogleProvider {
  pub fn new(api_key: &str, base_url: Option<String>) -> Result<Self> {
    Self::with_client(super::default_http_client()?, api_key, base_url)
  }

  /// Construct with a caller-supplied [`reqwest::Client`]. See
  /// [`crate::providers::OpenAIProvider::with_client`] for the rationale.
  pub fn with_client(client: Client, api_key: &str, base_url: Option<String>) -> Result<Self> {
    if api_key.is_empty() {
      return Err(LLMError::MissingApiKey {
        provider: "google".to_string(),
      });
    }

    let base_url =
      base_url.unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());

    Ok(Self {
      client,
      api_key: api_key.to_string(),
      base_url,
    })
  }

  fn build_headers(&self) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Q1.8.1: the API key used to live in the URL `?key=...` query
    // string. Any `reqwest::Error::to_string()` carried that URL into
    // `LLMError` messages, logs, and traces. Moving the key into a
    // header (Google publicly documents `x-goog-api-key` as
    // equivalent to the query-string form) keeps it out of the
    // error surface.
    headers.insert(
      "x-goog-api-key",
      HeaderValue::from_str(&self.api_key).map_err(|err| LLMError::ConfigurationError {
        message: format!("Google API key contains non-ASCII bytes: {err}"),
      })?,
    );
    crate::trace_context::inject_into_headers(&mut headers);
    Ok(headers)
  }

  fn build_request_body(&self, request: &ProviderRequest) -> Value {
    // Convert OpenAI-style messages to Gemini format
    let mut system_instruction = None;
    let mut gemini_contents = Vec::new();

    for message in &request.messages {
      if let Some(msg_obj) = message.as_object()
        && let (Some(role), Some(content)) = (msg_obj.get("role"), msg_obj.get("content"))
      {
        match role.as_str() {
          Some("system") => {
            // System messages stay text-only on Gemini; flatten any array parts
            // to their concatenated text rather than dropping non-text content
            // silently.
            let text = super::openai_content_to_text(content);
            if !text.is_empty() {
              system_instruction = Some(json!({"parts": [{"text": text}]}));
            }
          }
          Some("user") => {
            gemini_contents.push(json!({
              "role": "user",
              "parts": openai_content_to_gemini_parts(content),
            }));
          }
          Some("assistant") => {
            gemini_contents.push(json!({
              "role": "model",
              "parts": openai_content_to_gemini_parts(content),
            }));
          }
          _ => {}
        }
      }
    }

    let mut body = json!({
      "contents": gemini_contents
    });

    if let Some(system) = system_instruction {
      body["systemInstruction"] = system;
    }

    // Add generation config
    let mut generation_config = json!({});

    for (key, value) in &request.parameters {
      match key.as_str() {
        "temperature" => generation_config["temperature"] = value.clone(),
        "max_tokens" => generation_config["maxOutputTokens"] = value.clone(),
        "top_p" => generation_config["topP"] = value.clone(),
        "top_k" => generation_config["topK"] = value.clone(),
        _ => {}
      }
    }

    if let Some(thinking) = &request.thinking
      && let Some(block) = thinking_config_to_google_value(thinking)
    {
      generation_config["thinkingConfig"] = block;
    }

    // V2.1: Gemini natively supports structured output via
    // `generationConfig.responseMimeType` + `.responseSchema`, but only
    // when no real `tools` are requested — combining function-calling and
    // structured-output modes in one request isn't a combination this
    // adapter has verified, so (mirroring Anthropic's same caution) skip
    // native wiring and fall back to prompt-only constraint whenever
    // `tools` is present.
    if request.tools.is_none()
      && let Some(format) = &request.response_format
    {
      match format {
        ResponseFormat::JsonObject => {
          generation_config["responseMimeType"] = json!("application/json");
        }
        ResponseFormat::JsonSchema { schema, .. } => {
          generation_config["responseMimeType"] = json!("application/json");
          generation_config["responseSchema"] = schema.clone();
        }
        ResponseFormat::Text => {}
      }
    }

    // `generation_config` is always a JSON object (constructed from
    // `json!({...})` upstream), so `as_object().is_some_and(...)` is the
    // unwrap-free equivalent that survives the Q5.1 sweep.
    if generation_config
      .as_object()
      .is_some_and(|obj| !obj.is_empty())
    {
      body["generationConfig"] = generation_config;
    }

    if let Some(tools) = &request.tools {
      // Gemini wraps every function declaration list in a single `tools`
      // entry — we send one entry containing all functions.
      let declarations: Vec<Value> = tools.iter().map(tool_spec_to_google_value).collect();
      body["tools"] = json!([
        { "functionDeclarations": declarations }
      ]);
    }
    if let Some(choice) = &request.tool_choice {
      body["toolConfig"] = tool_choice_to_google_value(choice, request.tools.as_deref());
    }

    body
  }

  fn get_model_endpoint(&self, model: &str, stream: bool) -> String {
    let method = if stream {
      "streamGenerateContent"
    } else {
      "generateContent"
    };
    // Q1.8.1: no `?key=` here anymore — the API key now travels in
    // the `x-goog-api-key` header so it can't be picked up by a
    // `reqwest::Error::to_string()` URL leak.
    //
    // Without `alt=sse`, `streamGenerateContent` returns one pretty-printed
    // JSON array spread over many lines, which the line-oriented stream
    // parser can't read: every line failed to parse and was dropped, so
    // Google streams came back empty (eval 2026-10-07 L1).
    let query = if stream { "?alt=sse" } else { "" };
    format!(
      "{}/v1beta/models/{}:{}{}",
      self.base_url, model, method, query
    )
  }
}

/// Convert an OpenAI-shaped `content` field (string, or an array of typed
/// parts) into a Gemini `parts` array.
///
/// Supported part types: `text`, `image_url`, and `video_url` (P-LLM2.4
/// follow-up — Gemini's `generateContent` accepts video the same way it
/// accepts images: `inline_data` for base64, `file_data` for a remote
/// reference, including YouTube links passed directly without the Files
/// API). An `image_url` value can be either a string or an object
/// `{ "url": "..." }`; a `video_url` value is always an object
/// `{ "url": "...", "media_type": "..." }` (`media_type` optional — see
/// [`crate::multimodal::VideoUrl`]). Data URLs of the form
/// `data:<mime>;base64,<payload>` are decoded into Gemini's `inline_data`
/// shape; remote `http(s)` URLs are passed through as `file_data`
/// references. Unknown part shapes are dropped — multimodal flows should
/// not crash on a single unrecognised part.
pub(crate) fn openai_content_to_gemini_parts(content: &Value) -> Vec<Value> {
  if let Some(text) = content.as_str() {
    return vec![json!({"text": text})];
  }
  let Some(items) = content.as_array() else {
    // Non-string, non-array content (e.g. number, null) becomes empty parts.
    // Gemini rejects empty `parts`, so callers should ensure the content is
    // populated; we don't synthesise placeholder text.
    return Vec::new();
  };
  let mut parts = Vec::with_capacity(items.len());
  for item in items {
    let Some(obj) = item.as_object() else {
      continue;
    };
    let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
      "text" => {
        if let Some(text) = obj.get("text").and_then(Value::as_str) {
          parts.push(json!({"text": text}));
        }
      }
      "image_url" => {
        let url = obj
          .get("image_url")
          .and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(map) => map.get("url").and_then(Value::as_str),
            _ => None,
          })
          .unwrap_or("");
        if url.is_empty() {
          continue;
        }
        if let Some((mime_type, data)) = super::parse_data_url(url) {
          parts.push(json!({
            "inline_data": {
              "mime_type": mime_type,
              "data": data,
            }
          }));
        } else {
          // Pass remote URLs through as `file_data`. Gemini's REST API accepts
          // either `inline_data` (base64 payload) or `file_data` (uri+mime).
          parts.push(json!({
            "file_data": {
              "mime_type": "image/jpeg",
              "file_uri": url,
            }
          }));
        }
      }
      // P-LLM2.4 follow-up: video input. `MultimodalMessage::
      // to_openai_format` collapses `VideoData` (base64) into the same
      // `video_url` kind with a `data:` URI, same as image_url/image_data.
      "video_url" => {
        let video_url_obj = obj.get("video_url").and_then(Value::as_object);
        let url = video_url_obj
          .and_then(|m| m.get("url"))
          .and_then(Value::as_str)
          .unwrap_or("");
        if url.is_empty() {
          continue;
        }
        let media_type_hint = video_url_obj
          .and_then(|m| m.get("media_type"))
          .and_then(Value::as_str);
        if let Some((mime_type, data)) = super::parse_data_url(url) {
          parts.push(json!({
            "inline_data": {
              "mime_type": mime_type,
              "data": data,
            }
          }));
        } else if is_youtube_url(url) && media_type_hint.is_none() {
          // YouTube links are passed via `file_data` with no `mime_type` —
          // per the Gemini video-understanding docs, Gemini resolves the
          // format itself; sending a guessed `mime_type` alongside a
          // YouTube `file_uri` is not part of the documented shape.
          parts.push(json!({
            "file_data": { "file_uri": url }
          }));
        } else {
          // Any other remote reference (a Files API `file_uri`, or a plain
          // external URL) — Gemini's `file_data` requires `mime_type`
          // alongside `file_uri`. Callers should supply an explicit
          // `media_type` (`VideoUrl::media_type` /
          // `.add_video_url_with_media_type()`) since it can't be inferred
          // from the URL; `video/mp4` is used as a last-resort default,
          // matching this file's existing `image/jpeg` fallback for
          // remote images above.
          let mime_type = media_type_hint.unwrap_or("video/mp4");
          parts.push(json!({
            "file_data": {
              "mime_type": mime_type,
              "file_uri": url,
            }
          }));
        }
      }
      _ => {}
    }
  }
  parts
}

/// Whether `url` is a YouTube video link (`youtube.com/watch...` or
/// `youtu.be/...`) — Gemini accepts these directly via `file_data` without
/// requiring the Files API, and without a `mime_type` field.
fn is_youtube_url(url: &str) -> bool {
  let Some(rest) = url
    .strip_prefix("https://")
    .or_else(|| url.strip_prefix("http://"))
  else {
    return false;
  };
  let host = rest.split(['/', '?', '#']).next().unwrap_or("");
  matches!(
    host,
    "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be"
  )
}

/// Encode a `ToolSpec` as a Gemini `functionDeclaration` entry.
pub(crate) fn tool_spec_to_google_value(spec: &ToolSpec) -> Value {
  json!({
    "name": spec.name,
    "description": spec.description,
    "parameters": spec.parameters,
  })
}

/// Encode `ToolChoice` as Gemini's `toolConfig.functionCallingConfig` block.
///
/// Specific-tool selection requires `allowedFunctionNames` to contain the
/// target name (mode is `ANY` so the model is forced to use a tool).
pub(crate) fn tool_choice_to_google_value(
  choice: &ToolChoice,
  _tools: Option<&[ToolSpec]>,
) -> Value {
  match choice {
    ToolChoice::Auto => json!({"functionCallingConfig": {"mode": "AUTO"}}),
    ToolChoice::None => json!({"functionCallingConfig": {"mode": "NONE"}}),
    ToolChoice::Required => json!({"functionCallingConfig": {"mode": "ANY"}}),
    ToolChoice::Tool { name } => json!({
      "functionCallingConfig": {
        "mode": "ANY",
        "allowedFunctionNames": [name],
      }
    }),
  }
}

/// Encode a [`ThinkingConfig`] as Gemini's `generationConfig.thinkingConfig`.
///
/// Gemini 2.5+ accepts `thinkingBudget: N` (negative `-1` for "dynamic"
/// auto-budget). `Auto` maps to `thinkingBudget: -1`; explicit qualitative
/// levels and `Budget(n)` map to integer token counts. `Disabled` emits
/// `thinkingBudget: 0` per Google's documented "thinking off" form.
/// `includeThoughts: true` asks for the reasoning summary, which comes
/// back as parts marked `thought: true` (Y10.3).
pub(crate) fn thinking_config_to_google_value(config: &ThinkingConfig) -> Option<Value> {
  if config.is_disabled() {
    return Some(json!({ "thinkingBudget": 0 }));
  }
  let budget = config.to_token_budget().map_or(-1, i64::from);
  Some(json!({ "thinkingBudget": budget, "includeThoughts": true }))
}

/// Y10.3: the text of the `thought: true` parts — Gemini's reasoning
/// summary, kept out of the answer.
fn google_thought_text(parts: &[GooglePart]) -> String {
  parts
    .iter()
    .filter(|part| part.thought)
    .filter_map(|part| part.text.as_deref())
    .collect()
}

/// Pull `functionCall` parts out of the first candidate and convert them to
/// typed `ToolCallRequest`s. Gemini does not include ids — we synthesise
/// stable `call_<index>` ids so downstream tool-result correlation works.
pub(crate) fn parse_google_function_calls(parts: &[GooglePart]) -> Vec<ToolCallRequest> {
  parts
    .iter()
    .enumerate()
    .filter_map(|(idx, part)| {
      let call = part.function_call.as_ref()?;
      let name = call.get("name").and_then(Value::as_str)?.to_string();
      let arguments = call
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
      Some(ToolCallRequest {
        id: format!("call_{}", idx),
        name,
        arguments,
      })
    })
    .collect()
}

#[async_trait]
impl LLMProvider for GoogleProvider {
  fn name(&self) -> &str {
    "google"
  }

  async fn execute(&self, request: &ProviderRequest) -> Result<ProviderResponse> {
    if request.stream {
      return Err(LLMError::InternalError {
        message: "Use execute_streaming for streaming requests".to_string(),
      });
    }

    let url = self.get_model_endpoint(&request.model, false);
    let body = self.build_request_body(request);

    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let headers = response.headers().clone();
      let error_text = response.text().await.unwrap_or_default();
      return Err(super::chat_http_error(status_code, error_text, &headers));
    }

    let google_response: GoogleResponse = response.json().await?;

    let first_candidate = google_response.candidates.first();
    // Text and inline images, in order; functionCall parts are surfaced
    // via `tool_calls` instead of being stringified into content.
    let content = match first_candidate {
      Some(candidate) => google_parts_to_content(&candidate.content.parts)?,
      None => ContentType::Text(String::new()),
    };

    let usage = google_response
      .usage_metadata
      .clone()
      .map(|u| crate::providers::TokenUsage {
        prompt_tokens: Some(u.prompt_token_count),
        completion_tokens: Some(u.candidates_token_count),
        total_tokens: Some(u.total_token_count),
      });

    let tool_calls = first_candidate
      .map(|c| parse_google_function_calls(&c.content.parts))
      .unwrap_or_default();

    // Gemini emits no dedicated tool-call finish reason; presence of
    // functionCall parts is the signal. Override `STOP` to `ToolCalls` when
    // tool calls are present so callers branch correctly.
    let stop_reason = first_candidate.and_then(|c| {
      let raw = c.finish_reason.as_deref()?;
      let mapped = StopReason::from_google_finish_reason(raw);
      if !tool_calls.is_empty() && matches!(mapped, StopReason::Stop) {
        Some(StopReason::ToolCalls)
      } else {
        Some(mapped)
      }
    });

    Ok(ProviderResponse {
      content,
      usage,
      metadata: Some(serde_json::to_value(&google_response)?),
      tool_calls,
      stop_reason,
      thinking: first_candidate
        .map(|c| google_thought_text(&c.content.parts))
        .filter(|text| !text.is_empty()),
    })
  }

  async fn execute_streaming(
    &self,
    request: &ProviderRequest,
  ) -> Result<Box<dyn StreamingResponse>> {
    if !request.stream {
      return Err(LLMError::InternalError {
        message: "Streaming not enabled in request".to_string(),
      });
    }

    let url = self.get_model_endpoint(&request.model, true);
    let body = self.build_request_body(request);

    let response = self
      .client
      .post(&url)
      .headers(self.build_headers()?)
      .json(&body)
      .send()
      .await?;

    if !response.status().is_success() {
      let status_code = response.status().as_u16();
      let headers = response.headers().clone();
      let error_text = response.text().await.unwrap_or_default();
      return Err(super::chat_http_error(status_code, error_text, &headers));
    }

    Ok(Box::new(GoogleStreamingResponse::new(response)))
  }

  async fn validate_config(&self) -> Result<()> {
    // Test with a simple model list request
    // Q1.8.1: same treatment as `get_model_endpoint` — no `?key=` in
    // the URL, the API key rides along in `x-goog-api-key`.
    let url = format!("{}/v1beta/models", self.base_url);

    let response = self
      .client
      .get(&url)
      .headers(self.build_headers()?)
      .send()
      .await?;

    if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
      return Err(LLMError::AuthenticationError {
        provider: "google".to_string(),
        message: "Invalid API key".to_string(),
      });
    }

    Ok(())
  }

  fn base_url(&self) -> &str {
    &self.base_url
  }

  fn supported_models(&self) -> Vec<String> {
    vec![
      "gemini-1.5-pro".to_string(),
      "gemini-1.5-pro-002".to_string(),
      "gemini-1.5-flash".to_string(),
      "gemini-1.5-flash-002".to_string(),
      "gemini-1.0-pro".to_string(),
    ]
  }
}

// Google AI API response structures
#[derive(Debug, Deserialize, Serialize)]
struct GoogleResponse {
  candidates: Vec<GoogleCandidate>,
  #[serde(rename = "usageMetadata")]
  usage_metadata: Option<GoogleUsage>,
  #[serde(rename = "promptFeedback")]
  prompt_feedback: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GoogleCandidate {
  content: GoogleContent,
  #[serde(rename = "finishReason")]
  finish_reason: Option<String>,
  index: Option<u32>,
  #[serde(rename = "safetyRatings")]
  safety_ratings: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GoogleContent {
  parts: Vec<GooglePart>,
  role: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GooglePart {
  pub text: Option<String>,
  /// Y10.3: set on reasoning-summary parts when `includeThoughts` is on.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub thought: bool,
  /// Y6.6 (M7): inline media in a reply, e.g. the image of a Gemini
  /// image-output model. Not re-serialized into the response metadata:
  /// the decoded bytes are already in `content`.
  #[serde(rename = "inlineData", default, skip_serializing)]
  pub inline_data: Option<GoogleInlineData>,
  /// Native function call payload: `{ "name": "...", "args": { ... } }`.
  #[serde(
    rename = "functionCall",
    default,
    skip_serializing_if = "Option::is_none"
  )]
  pub function_call: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GoogleInlineData {
  #[serde(rename = "mimeType")]
  pub mime_type: String,
  /// Base64.
  pub data: String,
}

/// Decode one `inlineData` part: audio MIME types become audio, anything
/// else an image. Shared by the whole-reply and streaming parsers.
fn inline_data_block(inline: &GoogleInlineData) -> Result<ContentBlock> {
  use base64::{Engine as _, engine::general_purpose::STANDARD};
  let data = STANDARD
    .decode(inline.data.trim())
    .map_err(|e| LLMError::ResponseParsingError {
      message: format!(
        "Gemini inlineData ({}) is not valid base64: {e}",
        inline.mime_type
      ),
    })?;
  let media_type = inline.mime_type.clone();
  Ok(if media_type.starts_with("audio/") {
    ContentBlock::Audio { data, media_type }
  } else {
    ContentBlock::Image { data, media_type }
  })
}

/// Y6.6 (M7): a reply's parts as content, in order. Text only (the usual
/// case) stays `ContentType::Text`; one image alone is
/// `ContentType::Image`; text and images together are `Mixed`. Function
/// calls are not content (they become `tool_calls`).
pub(crate) fn google_parts_to_content(parts: &[GooglePart]) -> Result<ContentType> {
  let mut blocks: Vec<ContentBlock> = Vec::new();
  for part in parts.iter().filter(|part| !part.thought) {
    if let Some(text) = &part.text {
      match blocks.last_mut() {
        Some(ContentBlock::Text(previous)) => previous.push_str(text),
        _ => blocks.push(ContentBlock::Text(text.clone())),
      }
    } else if let Some(inline) = &part.inline_data {
      blocks.push(inline_data_block(inline)?);
    }
  }
  Ok(match blocks.len() {
    0 => ContentType::Text(String::new()),
    1 => match blocks.remove(0) {
      ContentBlock::Text(text) => ContentType::Text(text),
      ContentBlock::Image { data, media_type } => ContentType::Image { data, media_type },
      ContentBlock::Audio { data, media_type } => ContentType::Audio { data, media_type },
    },
    _ => ContentType::Mixed(blocks),
  })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GoogleUsage {
  #[serde(rename = "promptTokenCount")]
  prompt_token_count: u32,
  #[serde(rename = "candidatesTokenCount")]
  candidates_token_count: u32,
  #[serde(rename = "totalTokenCount")]
  total_token_count: u32,
}

pub struct GoogleStreamingResponse {
  stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
  buffer: LineBuffer,
  finished: bool,
  /// P-LLM2.5: Gemini gives `functionCall` parts no id at all (same as the
  /// non-streaming path — see `parse_google_function_calls`'s doc comment),
  /// so we synthesise one. Unlike a per-chunk-local counter, this persists
  /// across the whole stream: Gemini's real wire behavior for *when*
  /// parallel function-call parts land (all in one chunk vs. spread across
  /// several) isn't documented as a hard guarantee, and a per-chunk-local
  /// index would silently collide two distinct tool calls onto the same
  /// `ToolCallDelta.index` if they ever arrived in separate chunks —
  /// `collect_streaming_response` groups deltas by `index`, so a collision
  /// would merge two unrelated calls into one garbled reconstruction.
  next_tool_call_index: u32,
}

// Make it Send + Sync
// Q2.5.4: `unsafe impl Send + Sync` removed (trait no longer needs Sync).

impl GoogleStreamingResponse {
  fn new(response: reqwest::Response) -> Self {
    let byte_stream = response.bytes_stream();
    let string_stream = byte_stream.map(|chunk_result| {
      chunk_result
        .map_err(|e| LLMError::StreamingError {
          message: e.to_string(),
        })
        .map(|chunk| chunk.to_vec())
    });

    Self {
      stream: Box::pin(string_stream),
      buffer: LineBuffer::default(),
      finished: false,
      next_tool_call_index: 0,
    }
  }

  /// P-LLM2.5: unlike OpenAI's `delta.tool_calls[]` / Anthropic's
  /// `input_json_delta`, Gemini never fragments a function call's
  /// arguments across chunks — a `functionCall` part always carries the
  /// complete `{"name": ..., "args": {...}}` object in one shot (mirrors
  /// `parse_google_function_calls`, the non-streaming equivalent, which
  /// never needs cross-part accumulation either). So each `functionCall`
  /// part observed here becomes exactly one `ToolCallDelta` with its full
  /// arguments already serialized into `arguments_delta` — no follow-up
  /// delta will ever arrive for that same index.
  ///
  /// `&mut self` (not a free function, unlike the sibling OpenAI/Moonshot/
  /// StepFun parsers) because `next_tool_call_index` must persist across
  /// calls — see that field's doc comment.
  fn parse_json_chunk(&mut self, line: &str) -> Result<Option<StreamChunk>> {
    // SSE framing (`alt=sse`); a bare JSON line is still accepted.
    let line = line.strip_prefix("data:").unwrap_or(line).trim();
    if line.is_empty() {
      return Ok(None);
    }

    let Ok(response) = serde_json::from_str::<GoogleResponse>(line) else {
      return Ok(None);
    };
    let Some(candidate) = response.candidates.first() else {
      return Ok(None);
    };

    // Previously this only ever inspected `parts.first()` and only handled
    // a `text` field — a `functionCall` part (whether alone or alongside a
    // text part in the same `parts` array) was invisible to this function
    // entirely, silently dropping every streamed tool call.
    let mut content_text = String::new();
    let mut tool_call_deltas = Vec::new();
    let mut media = Vec::new();
    let thinking = google_thought_text(&candidate.content.parts);
    for part in &candidate.content.parts {
      if let Some(text) = &part.text
        && !part.thought
      {
        content_text.push_str(text);
      }
      // Y8.2: an image arrives as one complete part; a broken one fails
      // the stream like it fails a whole reply.
      if let Some(inline) = &part.inline_data {
        media.push(inline_data_block(inline)?);
      }
      if let Some(call) = &part.function_call {
        let name = call.get("name").and_then(Value::as_str).map(str::to_string);
        let arguments = call
          .get("args")
          .cloned()
          .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let index = self.next_tool_call_index;
        self.next_tool_call_index += 1;
        tool_call_deltas.push(ToolCallDelta {
          index,
          id: Some(format!("call_{index}")),
          name,
          arguments_delta: serde_json::to_string(&arguments).ok(),
        });
      }
    }

    let is_final = candidate.finish_reason.is_some();
    // Emit a chunk when there is *any* signal — text, a tool-call delta, or
    // the final marker — so a tool-call-only chunk (no text) doesn't get
    // silently dropped, mirroring the same fix already applied to OpenAI's
    // `parse_sse_chunk` for the identical class of bug.
    if content_text.is_empty()
      && thinking.is_empty()
      && tool_call_deltas.is_empty()
      && media.is_empty()
      && !is_final
    {
      return Ok(None);
    }

    let Ok(metadata) = serde_json::to_value(&response) else {
      return Ok(None);
    };
    Ok(Some(StreamChunk {
      thinking,
      media,
      content: content_text,
      is_final,
      metadata: Some(metadata),
      usage: response.usage_metadata.map(|u| TokenUsage {
        prompt_tokens: Some(u.prompt_token_count),
        completion_tokens: Some(u.candidates_token_count),
        total_tokens: Some(u.total_token_count),
      }),
      content_type: Some("text".to_string()),
      tool_call_deltas,
    }))
  }
}

#[async_trait]
impl StreamingResponse for GoogleStreamingResponse {
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
    if self.finished {
      return Ok(None);
    }

    loop {
      // W0.3: drain any complete lines already sitting in the buffer
      // *before* pulling more bytes off the network — see `openai.rs`'s
      // `OpenAIStreamingResponse::next_chunk` (same bug, same fix) and
      // `stepfun.rs` for the original reference fix. Draining only
      // inside the `Some(Ok(data))` arm and returning on the first
      // parsed line stranded every subsequent buffered line until
      // another network read happened to re-trigger the drain; if the
      // stream had already ended, that line was silently dropped and
      // `is_final` was never observed from a real finish_reason chunk.
      // With `alt=sse` Gemini sends one `data: {json}` line per event.
      while let Some(line) = self.buffer.next_line() {
        if !line.is_empty()
          && let Some(chunk) = self.parse_json_chunk(&line)?
        {
          if chunk.is_final {
            self.finished = true;
          }
          return Ok(Some(chunk));
        }
      }

      match self.stream.next().await {
        Some(Ok(data)) => {
          self.buffer.push(&data);
        }
        Some(Err(e)) => return Err(e),
        None => {
          self.finished = true;
          return Ok(None);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Eval 2026-10-07 L1: without `alt=sse` the streaming endpoint returns
  /// a multi-line JSON array the line parser can't read.
  #[test]
  fn streaming_endpoint_requests_sse_framing() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    assert!(
      provider
        .get_model_endpoint("gemini-2.5-pro", true)
        .ends_with("/models/gemini-2.5-pro:streamGenerateContent?alt=sse")
    );
    assert!(
      provider
        .get_model_endpoint("gemini-2.5-pro", false)
        .ends_with("/models/gemini-2.5-pro:generateContent")
    );
  }

  /// Gemini 2.5+ accepts `thinkingConfig.thinkingBudget` under
  /// `generationConfig`. `ThinkingConfig::Medium` must land there
  /// (not on a top-level `thinking` field).
  #[test]
  fn build_request_body_emits_thinking_config_under_generation_config() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-2.5-pro".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Medium),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(
      body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
      4096
    );
  }

  #[test]
  fn build_request_body_thinking_auto_uses_dynamic_budget() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-2.5-flash".to_string(),
      messages: vec![json!({"role": "user", "content": "reason"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Auto),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    // Google uses -1 as the "auto / dynamic" budget signal.
    assert_eq!(
      body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
      -1
    );
  }

  #[test]
  fn build_request_body_thinking_disabled_emits_zero_budget() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-2.5-pro".to_string(),
      messages: vec![json!({"role": "user", "content": "no thinking"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: Some(ThinkingConfig::Disabled),
      response_format: None,
    };
    let body = provider.build_request_body(&request);
    assert_eq!(
      body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
      0
    );
  }

  /// V2.1: a `JsonSchema` format with no real tools requested maps
  /// natively to `generationConfig.responseMimeType` + `.responseSchema`.
  #[test]
  fn build_request_body_emits_response_schema_for_json_schema_without_tools() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-2.5-flash".to_string(),
      messages: vec![json!({"role": "user", "content": "answer please"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: Some(ResponseFormat::JsonSchema {
        name: "final_answer".to_string(),
        schema: json!({
          "type": "object",
          "properties": {"answer": {"type": "string"}},
          "required": ["answer"]
        }),
        strict: Some(true),
      }),
    };
    let body = provider.build_request_body(&request);
    assert_eq!(
      body["generationConfig"]["responseMimeType"],
      "application/json"
    );
    assert_eq!(body["generationConfig"]["responseSchema"]["type"], "object");
  }

  /// `JsonObject` maps to `responseMimeType` alone (Gemini has no separate
  /// "any valid JSON, no schema" knob beyond the mime type).
  #[test]
  fn build_request_body_emits_response_mime_type_for_json_object() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-2.5-flash".to_string(),
      messages: vec![json!({"role": "user", "content": "answer please"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: Some(ResponseFormat::JsonObject),
    };
    let body = provider.build_request_body(&request);
    assert_eq!(
      body["generationConfig"]["responseMimeType"],
      "application/json"
    );
    assert!(body["generationConfig"].get("responseSchema").is_none());
  }

  /// When real tools are already requested, native structured-output
  /// wiring is skipped entirely — falls back to prompt-only constraint
  /// rather than sending an unverified tools+responseSchema combination.
  #[test]
  fn build_request_body_skips_response_schema_when_tools_present() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new(
      "get_weather",
      "Return the weather",
      json!({"type": "object"}),
    );
    let request = ProviderRequest {
      model: "gemini-2.5-flash".to_string(),
      messages: vec![json!({"role": "user", "content": "weather?"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: None,
      thinking: None,
      response_format: Some(ResponseFormat::JsonSchema {
        name: "final_answer".to_string(),
        schema: json!({"type": "object"}),
        strict: Some(true),
      }),
    };
    let body = provider.build_request_body(&request);
    assert!(body["generationConfig"].get("responseMimeType").is_none());
    assert!(body["generationConfig"].get("responseSchema").is_none());
    assert!(body.get("tools").is_some());
  }

  #[test]
  fn test_google_provider_creation() {
    let provider = GoogleProvider::new("test-key", None);
    assert!(provider.is_ok());

    let provider = GoogleProvider::new("", None);
    assert!(provider.is_err());
  }

  #[tokio::test]
  async fn build_headers_injects_traceparent_when_scope_active() {
    use crate::trace_context::{LlmTraceContext, scope};

    let provider = GoogleProvider::new("test-key", None).unwrap();
    let ctx = LlmTraceContext::new("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331").unwrap();

    let headers = scope(ctx.clone(), async { provider.build_headers() })
      .await
      .expect("headers must build under a well-formed ASCII API key");
    assert_eq!(
      headers.get("traceparent").and_then(|v| v.to_str().ok()),
      Some(ctx.to_traceparent().as_str()),
    );
    // Q1.8.1: the API key now lives in `x-goog-api-key` instead of
    // the URL query string.
    assert_eq!(
      headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
      Some("test-key"),
    );
  }

  /// Q1.8.1 regression: the model endpoint URL must not contain
  /// `key=`. Pre-fix any `reqwest::Error::to_string()` would carry
  /// the URL (and therefore the API key) straight into `LLMError`
  /// messages and tracing.
  #[test]
  fn google_model_endpoint_url_no_longer_carries_api_key() {
    let provider = GoogleProvider::new("secret-key-do-not-leak", None).unwrap();
    let url = provider.get_model_endpoint("gemini-1.5-pro", false);
    assert!(
      !url.contains("key="),
      "endpoint URL still embeds the API key: {url}"
    );
    assert!(
      !url.contains("secret-key-do-not-leak"),
      "endpoint URL still leaks the API key value: {url}"
    );
  }

  #[test]
  fn test_build_request_body() {
    let provider = GoogleProvider::new("test-key", None).unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("temperature".to_string(), json!(0.7));
    params.insert("max_tokens".to_string(), json!(100));

    let request = ProviderRequest {
      model: "gemini-1.5-pro".to_string(),
      messages: vec![
        json!({"role": "system", "content": "You are helpful"}),
        json!({"role": "user", "content": "test"}),
      ],
      stream: false,
      parameters: params,
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    assert!(body.get("systemInstruction").is_some());
    assert_eq!(body["contents"].as_array().unwrap().len(), 1); // Only user message in contents
    assert!(body.get("generationConfig").is_some());
  }

  #[test]
  fn test_model_endpoint() {
    let provider = GoogleProvider::new("test-key", None).unwrap();

    let endpoint = provider.get_model_endpoint("gemini-1.5-pro", false);
    assert!(endpoint.contains("generateContent"));
    // Q1.8.1: the API key is no longer in the URL — it lives in the
    // `x-goog-api-key` header now (see
    // `google_model_endpoint_url_no_longer_carries_api_key`).
    assert!(!endpoint.contains("test-key"));

    let streaming_endpoint = provider.get_model_endpoint("gemini-1.5-pro", true);
    assert!(streaming_endpoint.contains("streamGenerateContent"));
  }

  /// V4.4 regression: every prior endpoint test constructed `GoogleProvider`
  /// with `base_url: None` (the correct hardcoded fallback) or a synthetic
  /// mock-server URL — never the real value shipped in
  /// `templates/default_models.yml`. That shipped value used to be
  /// `https://generativelanguage.googleapis.com/v1beta/openai` (the
  /// OpenAI-compatibility endpoint prefix), which `get_model_endpoint`'s
  /// native-format concatenation (`{base_url}/v1beta/models/{model}:{method}`)
  /// turned into a malformed double-`/v1beta/` URL with a stray `/openai`
  /// segment — invisible to every existing test because none of them
  /// exercised the real config value. Parse the actual embedded YAML (the
  /// same `include_str!` production code reads) and build a real
  /// `GoogleProvider` from it, so a future re-introduction of this bug in
  /// either the config value or the endpoint-building code fails here.
  #[test]
  fn model_endpoint_from_the_real_shipped_config_is_well_formed() {
    let config = crate::config::model_config::LLMConfig::from_yaml(include_str!(
      "../../templates/default_models.yml"
    ))
    .expect("default_models.yml parses");
    let base_url = config
      .get_provider("google")
      .and_then(|p| p.base_url.clone())
      .expect("google provider has a base_url in the shipped config");

    let provider = GoogleProvider::new("test-key", Some(base_url)).unwrap();
    let endpoint = provider.get_model_endpoint("gemini-1.5-pro", false);

    assert_eq!(
      endpoint,
      "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent"
    );
    assert!(
      !endpoint.contains("/v1beta/openai"),
      "endpoint carries a stray OpenAI-compat path segment: {endpoint}"
    );
    assert_eq!(
      endpoint.matches("/v1beta/").count(),
      1,
      "endpoint has a duplicated /v1beta/ segment: {endpoint}"
    );
  }

  #[test]
  fn build_request_body_serialises_tools() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let tool = ToolSpec::new(
      "get_weather",
      "Return the weather for a city",
      json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"]
      }),
    );
    let request = ProviderRequest {
      model: "gemini-1.5-pro".to_string(),
      messages: vec![json!({"role": "user", "content": "weather?"})],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: Some(vec![tool]),
      tool_choice: Some(ToolChoice::Required),
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    let decls = tools[0]["functionDeclarations"]
      .as_array()
      .expect("functionDeclarations");
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
  }

  #[test]
  fn tool_choice_specific_lists_allowed_function() {
    let body = tool_choice_to_google_value(
      &ToolChoice::Tool {
        name: "x".to_string(),
      },
      None,
    );
    assert_eq!(body["functionCallingConfig"]["mode"], "ANY");
    assert_eq!(
      body["functionCallingConfig"]["allowedFunctionNames"][0],
      "x"
    );
  }

  #[test]
  fn parse_google_function_calls_extracts_calls() {
    let raw = json!({
      "candidates": [
        {
          "content": {
            "parts": [
              {"text": "I'll check"},
              {"functionCall": {"name": "get_weather", "args": {"city": "Tokyo"}}}
            ],
            "role": "model"
          },
          "finishReason": "STOP"
        }
      ],
      "usageMetadata": {
        "promptTokenCount": 5,
        "candidatesTokenCount": 3,
        "totalTokenCount": 8
      }
    });
    let parsed: GoogleResponse = serde_json::from_value(raw).unwrap();
    let candidate = &parsed.candidates[0];
    let calls = parse_google_function_calls(&candidate.content.parts);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments["city"], "Tokyo");
    // Synthesised id when Gemini doesn't provide one.
    assert_eq!(calls[0].id, "call_1");
  }

  #[test]
  fn parse_google_function_calls_text_only_returns_empty() {
    let parts = vec![GooglePart {
      text: Some("hi".to_string()),
      thought: false,
      inline_data: None,
      function_call: None,
    }];
    assert!(parse_google_function_calls(&parts).is_empty());
  }

  #[test]
  fn reply_images_become_image_content_in_order() {
    let parts: Vec<GooglePart> = serde_json::from_value(json!([
      {"text": "Here is "},
      {"text": "your cat:"},
      {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
      {"functionCall": {"name": "f", "args": {}}}
    ]))
    .expect("parts");
    match google_parts_to_content(&parts).expect("content") {
      ContentType::Mixed(blocks) => {
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t == "Here is your cat:"));
        assert!(
          matches!(&blocks[1], ContentBlock::Image { data, media_type }
          if media_type == "image/png" && data.starts_with(b"\x89PNG"))
        );
        assert_eq!(blocks.len(), 2);
      }
      other => panic!("{other:?}"),
    }
    let image_only: Vec<GooglePart> = serde_json::from_value(json!([
      {"inlineData": {"mimeType": "image/jpeg", "data": "/9j/"}}
    ]))
    .expect("parts");
    assert!(
      google_parts_to_content(&image_only)
        .expect("content")
        .has_images()
    );
    let text_only: Vec<GooglePart> =
      serde_json::from_value(json!([{"text": "a"}, {"text": "b"}])).expect("parts");
    assert!(
      matches!(google_parts_to_content(&text_only).expect("content"), ContentType::Text(t) if t == "ab")
    );
    let broken: Vec<GooglePart> = serde_json::from_value(json!([
      {"inlineData": {"mimeType": "image/png", "data": "***"}}
    ]))
    .expect("parts");
    assert!(google_parts_to_content(&broken).is_err());
    // The image bytes are not serialized back into the response metadata.
    assert!(
      !serde_json::to_string(&parts)
        .expect("json")
        .contains("iVBOR")
    );
  }

  #[test]
  fn openai_content_to_gemini_parts_handles_string() {
    let parts = openai_content_to_gemini_parts(&json!("hello"));
    assert_eq!(parts, vec![json!({"text": "hello"})]);
  }

  #[test]
  fn openai_content_to_gemini_parts_handles_text_and_image_data_url() {
    let content = json!([
      {"type": "text", "text": "describe"},
      {
        "type": "image_url",
        "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
      }
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], json!({"text": "describe"}));
    assert_eq!(
      parts[1],
      json!({
        "inline_data": {
          "mime_type": "image/png",
          "data": "iVBORw0KGgo="
        }
      })
    );
  }

  #[test]
  fn openai_content_to_gemini_parts_handles_remote_image_url() {
    let content = json!([
      {"type": "image_url", "image_url": "https://example.com/cat.jpg"}
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(
      parts[0]["file_data"]["file_uri"],
      "https://example.com/cat.jpg"
    );
  }

  #[test]
  fn openai_content_to_gemini_parts_drops_unknown_part_kinds() {
    let content = json!([
      {"type": "text", "text": "ok"},
      {"type": "voice_note", "audio": "..."}
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0], json!({"text": "ok"}));
  }

  /// P-LLM2.4 follow-up: base64 video routes to `inline_data`, same as images.
  #[test]
  fn openai_content_to_gemini_parts_handles_base64_video_data_url() {
    let content = json!([
      {
        "type": "video_url",
        "video_url": {"url": "data:video/mp4;base64,AAAA"}
      }
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(
      parts[0],
      json!({
        "inline_data": {
          "mime_type": "video/mp4",
          "data": "AAAA",
        }
      })
    );
  }

  /// A YouTube `file_uri` omits `mime_type` entirely, matching the Gemini
  /// video-understanding docs' documented shape for YouTube links.
  #[test]
  fn openai_content_to_gemini_parts_youtube_url_omits_mime_type() {
    let content = json!([
      {
        "type": "video_url",
        "video_url": {"url": "https://www.youtube.com/watch?v=9hE5-98ZeCg"}
      }
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(
      parts[0],
      json!({
        "file_data": { "file_uri": "https://www.youtube.com/watch?v=9hE5-98ZeCg" }
      })
    );
  }

  /// A non-YouTube remote reference (e.g. a Files API URI) requires
  /// `mime_type` alongside `file_uri`; an explicit `media_type` hint is
  /// honored when supplied.
  #[test]
  fn openai_content_to_gemini_parts_remote_video_url_uses_media_type_hint() {
    let content = json!([
      {
        "type": "video_url",
        "video_url": {
          "url": "https://generativelanguage.googleapis.com/v1beta/files/abc123",
          "media_type": "video/webm",
        }
      }
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(
      parts[0],
      json!({
        "file_data": {
          "mime_type": "video/webm",
          "file_uri": "https://generativelanguage.googleapis.com/v1beta/files/abc123",
        }
      })
    );
  }

  /// Without a `media_type` hint, a non-YouTube remote video URL falls back
  /// to `video/mp4` rather than being dropped.
  #[test]
  fn openai_content_to_gemini_parts_remote_video_url_defaults_mime_type() {
    let content = json!([
      {
        "type": "video_url",
        "video_url": {"url": "https://example.com/clip.mov"}
      }
    ]);
    let parts = openai_content_to_gemini_parts(&content);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["file_data"]["mime_type"], "video/mp4");
    assert_eq!(
      parts[0]["file_data"]["file_uri"],
      "https://example.com/clip.mov"
    );
  }

  #[test]
  fn build_request_body_routes_multimodal_user_content_to_inline_data() {
    let provider = GoogleProvider::new("test-key", None).unwrap();
    let request = ProviderRequest {
      model: "gemini-1.5-pro".to_string(),
      messages: vec![json!({
        "role": "user",
        "content": [
          {"type": "text", "text": "describe"},
          {
            "type": "image_url",
            "image_url": {"url": "data:image/png;base64,AAAA"}
          }
        ]
      })],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let parts = body["contents"][0]["parts"].as_array().expect("parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "describe");
    assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
    assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
  }

  /// P-LLM2.4 follow-up: exercises the real public-API path — a
  /// `MultimodalMessage` built via `.add_video_url()` — end to end through
  /// `build_request_body`, not just the lower-level content converter.
  #[test]
  fn build_request_body_routes_multimodal_video_to_file_data() {
    use crate::multimodal::MultimodalMessage;

    let provider = GoogleProvider::new("test-key", None).unwrap();
    let message = MultimodalMessage::user()
      .add_text("summarize this video")
      .add_video_url("https://www.youtube.com/watch?v=9hE5-98ZeCg")
      .build()
      .to_openai_format();

    let request = ProviderRequest {
      model: "gemini-1.5-pro".to_string(),
      messages: vec![message],
      stream: false,
      parameters: std::collections::HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    };

    let body = provider.build_request_body(&request);
    let parts = body["contents"][0]["parts"].as_array().expect("parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "summarize this video");
    assert_eq!(
      parts[1]["file_data"]["file_uri"],
      "https://www.youtube.com/watch?v=9hE5-98ZeCg"
    );
    assert!(parts[1]["file_data"].get("mime_type").is_none());
  }

  #[test]
  fn is_youtube_url_recognizes_standard_and_short_forms() {
    assert!(is_youtube_url("https://www.youtube.com/watch?v=abc123"));
    assert!(is_youtube_url("https://youtube.com/watch?v=abc123"));
    assert!(is_youtube_url("https://m.youtube.com/watch?v=abc123"));
    assert!(is_youtube_url("https://youtu.be/abc123"));
    assert!(!is_youtube_url("https://example.com/watch?v=abc123"));
    assert!(!is_youtube_url(
      "https://generativelanguage.googleapis.com/v1beta/files/abc"
    ));
    assert!(!is_youtube_url("not a url"));
  }

  /// A `GoogleStreamingResponse` with no real network stream backing it —
  /// `parse_json_chunk`/`drain_next_line` never touch `self.stream`, so an
  /// empty stub is enough to exercise them directly without a live HTTP
  /// round trip.
  fn test_streaming_response() -> GoogleStreamingResponse {
    GoogleStreamingResponse {
      stream: Box::pin(futures::stream::empty::<Result<Vec<u8>>>()),
      buffer: LineBuffer::default(),
      finished: false,
      next_tool_call_index: 0,
    }
  }

  /// P-LLM2.5 regression: `parse_json_chunk` previously only ever inspected
  /// `parts.first()` and only handled a `text` field — a `functionCall`
  /// part was invisible to it, and `tool_call_deltas` was hardcoded to
  /// `Vec::new()`. Gemini gives function calls no id, so one is synthesised
  /// (`call_<index>`); the full `args` object is serialized whole into
  /// `arguments_delta` since Gemini never fragments it across chunks.
  /// Y10.3: `thought: true` parts are reasoning, not answer text — in a
  /// whole reply and in a streamed chunk alike.
  #[test]
  fn thought_parts_become_thinking_not_content() {
    let parts: Vec<GooglePart> = serde_json::from_value(json!([
      {"text": "weighing it", "thought": true},
      {"text": "391"}
    ]))
    .unwrap();
    assert_eq!(google_thought_text(&parts), "weighing it");
    match google_parts_to_content(&parts).unwrap() {
      ContentType::Text(text) => assert_eq!(text, "391"),
      other => panic!("expected text, got {other:?}"),
    }

    let mut response = test_streaming_response();
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"weighing it","thought":true}],"role":"model"}}]}"#;
    let chunk = response
      .parse_json_chunk(line)
      .unwrap()
      .expect("thought chunk");
    assert_eq!(chunk.thinking, "weighing it");
    assert!(chunk.content.is_empty());
  }

  #[test]
  fn thinking_config_asks_for_thoughts_unless_disabled() {
    let on = thinking_config_to_google_value(&ThinkingConfig::Low).unwrap();
    assert_eq!(on["includeThoughts"], true);
    assert_eq!(on["thinkingBudget"], 1024);
    let off = thinking_config_to_google_value(&ThinkingConfig::Disabled).unwrap();
    assert!(off.get("includeThoughts").is_none());
  }

  #[test]
  fn parse_json_chunk_extracts_function_call_part() {
    let mut response = test_streaming_response();
    let line = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"}}}],"role":"model"},"finishReason":null}]}"#;
    let chunk = response
      .parse_json_chunk(line)
      .unwrap()
      .expect("must produce a chunk");
    assert!(chunk.content.is_empty());
    assert!(!chunk.is_final);
    assert_eq!(chunk.tool_call_deltas.len(), 1);
    let delta = &chunk.tool_call_deltas[0];
    assert_eq!(delta.index, 0);
    assert_eq!(delta.id.as_deref(), Some("call_0"));
    assert_eq!(delta.name.as_deref(), Some("get_weather"));
    let args: Value = serde_json::from_str(delta.arguments_delta.as_deref().unwrap()).unwrap();
    assert_eq!(args, json!({"city": "Tokyo"}));
  }

  /// A tool-call-only chunk (no text) must not be silently dropped —
  /// mirrors the same `has_signal` fix already applied to OpenAI.
  #[test]
  fn parse_json_chunk_does_not_drop_tool_call_only_chunk() {
    let mut response = test_streaming_response();
    let line = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"noop","args":{}}}],"role":"model"}}]}"#;
    let chunk = response.parse_json_chunk(line).unwrap();
    assert!(
      chunk.is_some(),
      "a function-call-only chunk must not be dropped"
    );
  }

  /// A part carrying both `text` and a `functionCall` (or two separate
  /// `functionCall` parts, for parallel tool calls) must surface both —
  /// not just the first part, like the pre-fix code did.
  #[test]
  fn parse_json_chunk_handles_text_and_multiple_function_calls_together() {
    let mut response = test_streaming_response();
    let line = r#"{"candidates":[{"content":{"parts":[{"text":"checking..."},{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"}}},{"functionCall":{"name":"get_time","args":{"tz":"JST"}}}],"role":"model"}}]}"#;
    let chunk = response.parse_json_chunk(line).unwrap().unwrap();
    assert_eq!(chunk.content, "checking...");
    assert_eq!(chunk.tool_call_deltas.len(), 2);
    assert_eq!(chunk.tool_call_deltas[0].index, 0);
    assert_eq!(
      chunk.tool_call_deltas[0].name.as_deref(),
      Some("get_weather")
    );
    assert_eq!(chunk.tool_call_deltas[1].index, 1);
    assert_eq!(chunk.tool_call_deltas[1].name.as_deref(), Some("get_time"));
  }

  /// `next_tool_call_index` must persist *across* calls to
  /// `parse_json_chunk`, not reset per chunk — otherwise two function
  /// calls arriving in separate stream chunks would collide onto the same
  /// `ToolCallDelta.index` and `collect_streaming_response` would merge
  /// them into one garbled reconstruction.
  #[test]
  fn tool_call_index_persists_across_chunks() {
    let mut response = test_streaming_response();
    let first = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{}}}],"role":"model"}}]}"#;
    let second = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_time","args":{}}}],"role":"model"}}]}"#;
    let chunk1 = response.parse_json_chunk(first).unwrap().unwrap();
    let chunk2 = response.parse_json_chunk(second).unwrap().unwrap();
    assert_eq!(chunk1.tool_call_deltas[0].index, 0);
    assert_eq!(chunk2.tool_call_deltas[0].index, 1);
  }
}
