use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::HashMap;
use tera::{Result as TeraResult, Tera, Value as TeraValue};
use yanshi_core::value::FlowValue;

/// Convert FlowValue to Tera-compatible value
pub fn flow_value_to_tera_value(value: &FlowValue) -> TeraValue {
  match value {
    FlowValue::Json(json) => json_to_tera_value(json),
    FlowValue::File { path, .. } => TeraValue::String(path.to_string_lossy().to_string()),
    FlowValue::Url { url, .. } => TeraValue::String(url.clone()),
  }
}

/// Convert serde_json::Value to tera::Value
fn json_to_tera_value(json: &JsonValue) -> TeraValue {
  match json {
    JsonValue::Null => TeraValue::Null,
    JsonValue::Bool(b) => TeraValue::Bool(*b),
    JsonValue::Number(n) => {
      if let Some(i) = n.as_i64() {
        TeraValue::Number(i.into())
      } else if let Some(u) = n.as_u64() {
        TeraValue::Number(u.into())
      } else if let Some(f) = n.as_f64() {
        // `Number::from_f64` returns `None` for NaN/±Inf. JSON itself
        // disallows those, but defensively degrade to Null rather than
        // panicking (Q5.1).
        serde_json::Number::from_f64(f)
          .map(TeraValue::Number)
          .unwrap_or(TeraValue::Null)
      } else {
        TeraValue::Null
      }
    }
    JsonValue::String(s) => TeraValue::String(s.clone()),
    JsonValue::Array(arr) => {
      let tera_arr: Vec<TeraValue> = arr.iter().map(json_to_tera_value).collect();
      TeraValue::Array(tera_arr)
    }
    JsonValue::Object(obj) => {
      let mut tera_obj = JsonMap::new();
      for (k, v) in obj {
        tera_obj.insert(k.clone(), json_to_tera_value(v));
      }
      TeraValue::Object(tera_obj)
    }
  }
}

/// Register custom filters for Yanshi
pub fn register_custom_filters(tera: &mut Tera) {
  // Filter to get file path from FlowValue
  tera.register_filter(
    "flow_path",
    |value: &TeraValue, _args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      if let TeraValue::String(s) = value {
        Ok(TeraValue::String(s.clone()))
      } else {
        Ok(TeraValue::String(format!("{:?}", value)))
      }
    },
  );

  // Filter to format JSON beautifully
  tera.register_filter(
    "json_pretty",
    |value: &TeraValue, _args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let json_str = serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{:?}", value));
      Ok(TeraValue::String(json_str))
    },
  );

  // Y1.2: a `FlowValue::File` renders as its path string (see
  // `flow_value_to_tera_value`); these two filters expose the rest of
  // what a template needs to describe an artifact without reading it
  // into the prompt. `{{ audio | mime }}` → "audio/wav" (by extension,
  // which is how artifacts are named); `{{ audio | file_size }}` → bytes.
  tera.register_filter(
    "mime",
    |value: &TeraValue, _args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let TeraValue::String(path) = value else {
        return Err(tera::Error::msg("`mime` filter expects a file path string"));
      };
      let mime = mime_guess::from_path(path)
        .first()
        .map(|m| m.essence_str().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
      Ok(TeraValue::String(mime))
    },
  );
  tera.register_filter(
    "file_size",
    |value: &TeraValue, _args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let TeraValue::String(path) = value else {
        return Err(tera::Error::msg(
          "`file_size` filter expects a file path string",
        ));
      };
      let len = std::fs::metadata(path)
        .map_err(|e| tera::Error::msg(format!("`file_size`: cannot stat '{path}': {e}")))?
        .len();
      Ok(TeraValue::Number(len.into()))
    },
  );

  // Filter to convert to JSON string
  tera.register_filter(
    "to_json",
    |value: &TeraValue, _args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let json_str = serde_json::to_string(value).unwrap_or_else(|_| format!("{:?}", value));
      Ok(TeraValue::String(json_str))
    },
  );
}

/// Register custom functions for Yanshi
pub fn register_custom_functions(tera: &mut Tera) {
  // Function to get current timestamp
  tera.register_function(
    "now",
    |_args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let now = chrono::Utc::now();
      Ok(TeraValue::String(now.to_rfc3339()))
    },
  );

  // Function to generate UUID
  tera.register_function(
    "uuid",
    |_args: &HashMap<String, TeraValue>| -> TeraResult<TeraValue> {
      let id = uuid::Uuid::new_v4();
      Ok(TeraValue::String(id.to_string()))
    },
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_json_to_tera_value_primitives() {
    assert_eq!(json_to_tera_value(&json!(null)), TeraValue::Null);
    assert_eq!(json_to_tera_value(&json!(true)), TeraValue::Bool(true));
    assert_eq!(json_to_tera_value(&json!(42)), TeraValue::Number(42.into()));
    assert_eq!(
      json_to_tera_value(&json!("hello")),
      TeraValue::String("hello".to_string())
    );
  }

  #[test]
  fn test_json_to_tera_value_array() {
    let json_arr = json!([1, 2, 3]);
    let tera_val = json_to_tera_value(&json_arr);

    if let TeraValue::Array(arr) = tera_val {
      assert_eq!(arr.len(), 3);
      assert_eq!(arr[0], TeraValue::Number(1.into()));
    } else {
      panic!("Expected array");
    }
  }

  #[test]
  fn test_json_to_tera_value_object() {
    let json_obj = json!({"name": "Alice", "age": 30});
    let tera_val = json_to_tera_value(&json_obj);

    if let TeraValue::Object(obj) = tera_val {
      assert_eq!(obj.len(), 2);
      assert_eq!(
        obj.get("name"),
        Some(&TeraValue::String("Alice".to_string()))
      );
      assert_eq!(obj.get("age"), Some(&TeraValue::Number(30.into())));
    } else {
      panic!("Expected object");
    }
  }

  #[test]
  fn test_flow_value_to_tera_value() {
    let flow_val = FlowValue::Json(json!({"test": "value"}));
    let tera_val = flow_value_to_tera_value(&flow_val);

    if let TeraValue::Object(obj) = tera_val {
      assert_eq!(
        obj.get("test"),
        Some(&TeraValue::String("value".to_string()))
      );
    } else {
      panic!("Expected object");
    }
  }

  #[test]
  fn test_custom_filters() {
    let mut tera = Tera::default();
    register_custom_filters(&mut tera);

    // Test that filters are registered
    assert!(tera.get_filter("flow_path").is_ok());
    assert!(tera.get_filter("json_pretty").is_ok());
    assert!(tera.get_filter("to_json").is_ok());
  }

  #[test]
  fn test_custom_functions() {
    let mut tera = Tera::default();
    register_custom_functions(&mut tera);

    // Test that functions are registered
    assert!(tera.get_function("now").is_ok());
    assert!(tera.get_function("uuid").is_ok());
  }
  #[test]
  fn file_values_render_as_paths_and_expose_mime_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("1.wav");
    std::fs::write(&audio, vec![0u8; 44]).unwrap();

    let mut tera = Tera::default();
    register_custom_filters(&mut tera);
    let mut ctx = tera::Context::new();
    ctx.insert(
      "audio",
      &flow_value_to_tera_value(&FlowValue::File {
        path: audio.clone(),
        mime_type: Some("audio/wav".to_string()),
      }),
    );
    let rendered = tera
      .render_str(
        "{{ audio }}|{{ audio | mime }}|{{ audio | file_size }}",
        &ctx,
      )
      .unwrap();
    assert_eq!(rendered, format!("{}|audio/wav|44", audio.display()));
  }
}
