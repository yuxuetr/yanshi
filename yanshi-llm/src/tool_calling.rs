//! # Tool Calling Types
//!
//! Provider-agnostic representation of tool / function calling.
//!
//! Each provider maps these types to its own wire format:
//! - OpenAI: `tools` array + `tool_calls` field
//! - Anthropic: `tools` block + `tool_use` content blocks
//! - Google: `function_declarations` + `functionCall` parts
//! - StepFun / Moonshot: OpenAI-compatible passthrough
//! - Mock: programmatic injection (used by ReAct/Plan-Execute fallback tests)

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Specification of a tool the model can call.
///
/// `parameters` is a JSON Schema describing the tool's arguments. The schema is
/// passed to providers verbatim, so callers must produce a schema the provider
/// accepts (OpenAI / Anthropic / Google all accept JSON Schema, with minor
/// keyword differences).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
  pub name: String,
  #[serde(default, skip_serializing_if = "String::is_empty")]
  pub description: String,
  /// JSON Schema for the tool's arguments.
  pub parameters: Value,
}

impl ToolSpec {
  pub fn new<N: Into<String>, D: Into<String>>(name: N, description: D, parameters: Value) -> Self {
    Self {
      name: name.into(),
      description: description.into(),
      parameters,
    }
  }

  /// Construct a `ToolSpec` from a JSON value in OpenAI tool format:
  /// `{ "type": "function", "function": { "name", "description", "parameters" } }`
  /// or directly `{ "name", "description", "parameters" }`.
  pub fn from_openai_value(value: &Value) -> Result<Self, String> {
    let function = value.get("function").unwrap_or(value);
    let name = function
      .get("name")
      .and_then(Value::as_str)
      .ok_or_else(|| "tool spec missing `name`".to_string())?
      .to_string();
    let description = function
      .get("description")
      .and_then(Value::as_str)
      .unwrap_or("")
      .to_string();
    let parameters = function
      .get("parameters")
      .cloned()
      .unwrap_or_else(|| Value::Object(Map::new()));
    Ok(Self {
      name,
      description,
      parameters,
    })
  }
}

/// Strategy for tool selection passed to the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
  /// Model decides whether to call a tool.
  #[default]
  Auto,
  /// Model must not call a tool.
  None,
  /// Model must call at least one tool (provider-specific support).
  Required,
  /// Model must call exactly this tool.
  Tool { name: String },
}

/// A single tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRequest {
  /// Provider-supplied identifier for correlating with the eventual tool result.
  ///
  /// For providers that don't return an id (e.g. some Google revisions), the
  /// provider adapter should synthesise a stable id like `call_<index>`.
  pub id: String,
  pub name: String,
  /// Arguments as parsed JSON.
  ///
  /// OpenAI returns arguments as a JSON-encoded string; Anthropic returns them
  /// as an object. Provider adapters must parse to a `Value` so callers don't
  /// need to know the wire format.
  pub arguments: Value,
}

/// Reason the model stopped generating.
///
/// Maps OpenAI `finish_reason`, Anthropic `stop_reason`, and Google
/// `finishReason` onto a common enumeration so callers can branch on it
/// without provider awareness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
  /// Natural end of generation (final answer produced).
  Stop,
  /// Reached `max_tokens` / output budget.
  Length,
  /// Model emitted one or more tool calls.
  ToolCalls,
  /// Stopped by content filter / safety.
  ContentFilter,
  /// Anything else; the inner string is the provider's raw stop reason.
  Other(String),
}

impl StopReason {
  /// Map an OpenAI `finish_reason` string onto a `StopReason`.
  pub fn from_openai_finish_reason(reason: &str) -> Self {
    match reason {
      "stop" | "end_turn" => Self::Stop,
      "length" | "max_tokens" => Self::Length,
      "tool_calls" | "function_call" => Self::ToolCalls,
      "content_filter" => Self::ContentFilter,
      other => Self::Other(other.to_string()),
    }
  }

  /// Map an Anthropic `stop_reason` string onto a `StopReason`.
  pub fn from_anthropic_stop_reason(reason: &str) -> Self {
    match reason {
      "end_turn" | "stop_sequence" => Self::Stop,
      "max_tokens" => Self::Length,
      "tool_use" => Self::ToolCalls,
      other => Self::Other(other.to_string()),
    }
  }

  /// Map a Google `finishReason` string onto a `StopReason`.
  pub fn from_google_finish_reason(reason: &str) -> Self {
    match reason {
      "STOP" => Self::Stop,
      "MAX_TOKENS" => Self::Length,
      "SAFETY" | "RECITATION" => Self::ContentFilter,
      // Google emits no dedicated tool-call stop reason; presence of
      // functionCall parts is the signal. Surface the raw reason for callers.
      other => Self::Other(other.to_string()),
    }
  }

  pub fn is_tool_calls(&self) -> bool {
    matches!(self, Self::ToolCalls)
  }
}

/// Top-level response returned by the LLM client to consumers (agents,
/// workflows, end users).
///
/// Wraps `ProviderResponse` into a stable, public-facing type. ReAct /
/// Plan-Execute prefer `tool_calls` over prompt parsing; if `tool_calls` is
/// empty they fall back to extracting JSON from `content`.
#[derive(Debug, Clone)]
pub struct LLMResponse {
  /// Model-generated text (may be empty when the model only emits tool calls).
  pub content: String,
  pub tool_calls: Vec<ToolCallRequest>,
  pub stop_reason: Option<StopReason>,
  pub usage: Option<crate::providers::TokenUsage>,
  /// Provider-specific raw payload, useful for logging / replay.
  pub raw_metadata: Option<Value>,
  /// Model-emitted reasoning / "thinking" text when the provider returns
  /// it. Populated for Anthropic `thinking` blocks, DeepSeek-R1
  /// `reasoning_content`, and OpenAI reasoning models when supported.
  /// `None` when the model didn't emit reasoning text or thinking wasn't
  /// requested. See [`crate::ThinkingConfig`] for how to ask for it.
  pub thinking: Option<String>,
  /// Y6.6 (M7): the reply's non-text parts (images, audio) in order, e.g.
  /// from a Gemini image-output model. `content` holds only the text.
  pub media: Vec<crate::providers::ContentBlock>,
}

impl LLMResponse {
  /// True if the model requested at least one tool call.
  pub fn has_tool_calls(&self) -> bool {
    !self.tool_calls.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn tool_spec_roundtrip_openai() {
    let raw = json!({
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Return the weather for a city",
        "parameters": {
          "type": "object",
          "properties": {"city": {"type": "string"}},
          "required": ["city"]
        }
      }
    });
    let spec = ToolSpec::from_openai_value(&raw).unwrap();
    assert_eq!(spec.name, "get_weather");
    assert_eq!(spec.description, "Return the weather for a city");
    assert_eq!(spec.parameters["properties"]["city"]["type"], "string");
  }

  #[test]
  fn tool_choice_default_is_auto() {
    assert_eq!(ToolChoice::default(), ToolChoice::Auto);
  }

  #[test]
  fn stop_reason_openai_mapping() {
    assert_eq!(
      StopReason::from_openai_finish_reason("stop"),
      StopReason::Stop
    );
    assert_eq!(
      StopReason::from_openai_finish_reason("tool_calls"),
      StopReason::ToolCalls
    );
    assert_eq!(
      StopReason::from_openai_finish_reason("length"),
      StopReason::Length
    );
    assert_eq!(
      StopReason::from_openai_finish_reason("custom"),
      StopReason::Other("custom".into())
    );
  }

  #[test]
  fn stop_reason_anthropic_mapping() {
    assert_eq!(
      StopReason::from_anthropic_stop_reason("tool_use"),
      StopReason::ToolCalls
    );
    assert_eq!(
      StopReason::from_anthropic_stop_reason("end_turn"),
      StopReason::Stop
    );
  }

  #[test]
  fn llm_response_has_tool_calls() {
    let resp = LLMResponse {
      content: String::new(),
      tool_calls: vec![ToolCallRequest {
        id: "call_0".into(),
        name: "x".into(),
        arguments: json!({}),
      }],
      stop_reason: Some(StopReason::ToolCalls),
      usage: None,
      raw_metadata: None,
      thinking: None,
      media: Vec::new(),
    };
    assert!(resp.has_tool_calls());
  }
}
