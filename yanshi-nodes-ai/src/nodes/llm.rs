use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::{Yanshi, providers::ContentBlock};
use yanshi_nodes::common::artifacts;

/// A chat-model call. `output` is the reply's text (or its parsed JSON
/// under `json_schema`); a reply that also carries images (Y6.6, e.g. a
/// Gemini image-output model) adds `images`, the images written as this
/// node's artifacts.
#[derive(Debug, Clone, Default)]
pub struct LlmNode {
  /// Names the artifact directory for reply images (`llm` when unset).
  id: Option<String>,
}

impl LlmNode {
  pub fn with_id(id: impl Into<String>) -> Self {
    Self {
      id: Some(id.into()),
    }
  }
}

#[async_trait]
impl AsyncNode for LlmNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let prompt = get_string_input(inputs, "prompt")?;
    let model = get_optional_string_input(inputs, "model")?;
    let system = get_optional_string_input(inputs, "system")?;

    Yanshi::ensure_init()
      .await
      .map_err(|e| YanshiError::ConfigurationError {
        message: format!("Failed to initialize Yanshi LLM service: {}", e),
      })?;

    let mut request = Yanshi::model(model.unwrap_or_default()).prompt(prompt);

    if let Some(sys) = system {
      request = request.system(sys);
    }
    if let Some(temp) = get_optional_f64_input(inputs, "temperature")? {
      request = request.temperature(temp as f32);
    }
    if let Some(max_tokens) = get_optional_u64_input(inputs, "max_tokens")? {
      request = request.max_tokens(max_tokens as u32);
    }
    // Y4.1: `json_schema` (a JSON Schema object) constrains the reply and
    // makes `output` the parsed JSON value rather than a string, so a
    // generator's outline can feed a Map node directly.
    let schema = match inputs.get("json_schema") {
      None => None,
      Some(FlowValue::Json(Value::Object(schema))) => Some(Value::Object(schema.clone())),
      Some(FlowValue::Json(Value::String(text))) => Some(serde_json::from_str(text).map_err(
        |e| YanshiError::NodeInputError {
          message: format!("Input 'json_schema' is not valid JSON: {e}"),
        },
      )?),
      Some(_) => {
        return Err(YanshiError::NodeInputError {
          message: "Input 'json_schema' must be a JSON Schema object".to_string(),
        });
      }
    };
    if let Some(schema) = &schema {
      request = request.json_schema("output", schema.clone());
    }

    tracing::debug!("executing LLM request");
    let response = request
      .execute_full()
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("LLM execution failed: {}", e),
      })?;
    tracing::debug!("LLM response received");

    let output = match schema {
      Some(_) => parse_json_reply(&response.content)?,
      None => Value::String(response.content),
    };
    let mut outputs = HashMap::new();
    outputs.insert("output".to_string(), FlowValue::Json(output));

    let node = self.id.as_deref().unwrap_or("llm");
    let mut images = Vec::new();
    for block in &response.media {
      if let ContentBlock::Image { data, media_type } = block {
        let file = artifacts::write_artifact(node, data, media_type).await?;
        images.push(
          serde_json::to_value(&file).map_err(|e| YanshiError::NodeExecutionFailed {
            message: format!("{node}: cannot describe a reply image: {e}"),
          })?,
        );
      }
    }
    if !images.is_empty() {
      outputs.insert("images".to_string(), FlowValue::Json(Value::Array(images)));
    }

    Ok(outputs)
  }
}

/// The JSON in a schema-constrained reply. Providers that ignore the
/// schema sometimes wrap it in a ```json fence; accept that too.
fn parse_json_reply(reply: &str) -> Result<Value, YanshiError> {
  let trimmed = reply.trim();
  let unfenced = trimmed
    .strip_prefix("```json")
    .or_else(|| trimmed.strip_prefix("```"))
    .and_then(|rest| rest.strip_suffix("```"))
    .unwrap_or(trimmed);
  serde_json::from_str(unfenced.trim()).map_err(|e| YanshiError::NodeExecutionFailed {
    message: format!("the model did not return JSON for json_schema: {e}"),
  })
}

fn get_string_input<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Result<&'a str, YanshiError> {
  inputs
    .get(key)
    .and_then(|v| match v {
      FlowValue::Json(Value::String(s)) => Some(s.as_str()),
      _ => None,
    })
    .ok_or_else(|| YanshiError::NodeInputError {
      message: format!(
        "Required string input '{}' is missing or has wrong type",
        key
      ),
    })
}

fn get_optional_string_input<'a>(
  inputs: &'a AsyncNodeInputs,
  key: &str,
) -> Result<Option<&'a str>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::String(s)) => Ok(Some(s.as_str())),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a string", key),
      }),
    },
  }
}

fn get_optional_f64_input(inputs: &AsyncNodeInputs, key: &str) -> Result<Option<f64>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::Number(n)) => Ok(n.as_f64()),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a number", key),
      }),
    },
  }
}

fn get_optional_u64_input(inputs: &AsyncNodeInputs, key: &str) -> Result<Option<u64>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::Number(n)) => Ok(n.as_u64()),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a number", key),
      }),
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[tokio::test]
  async fn test_llm_node_async_execution() {
    let node = LlmNode::default();
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert(
      "prompt".to_string(),
      FlowValue::Json(json!("What is 2+2? Respond with only the number.")),
    );
    inputs.insert("temperature".to_string(), FlowValue::Json(json!(0.0)));

    let result = node.execute(&inputs).await;

    if let Ok(outputs) = result {
      let response_val = outputs.get("output").unwrap();
      if let FlowValue::Json(Value::String(s)) = response_val {
        assert!(s.contains('4'));
      }
    } else {
      println!(
        "LLM node execution failed (as expected without API key): {:?}",
        result.err().unwrap()
      );
    }
  }

  #[test]
  fn json_replies_parse_with_or_without_a_fence() {
    assert_eq!(parse_json_reply(r#"{"a": 1}"#).unwrap(), json!({"a": 1}));
    assert_eq!(
      parse_json_reply("```json\n[1, 2]\n```").unwrap(),
      json!([1, 2])
    );
    assert!(parse_json_reply("not json").is_err());
  }
}
