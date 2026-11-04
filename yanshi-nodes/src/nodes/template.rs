use crate::common::tera_helpers::{
  flow_value_to_tera_value, register_custom_filters, register_custom_functions,
};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tera::Tera;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};

/// Global Tera instance with custom filters and functions
static TERA_INSTANCE: OnceLock<Mutex<Tera>> = OnceLock::new();

fn get_tera() -> &'static Mutex<Tera> {
  TERA_INSTANCE.get_or_init(|| {
    let mut tera = Tera::default();
    register_custom_filters(&mut tera);
    register_custom_functions(&mut tera);
    Mutex::new(tera)
  })
}

pub struct TemplateNode {
  pub name: String,
  pub template: String,
  pub output_key: String,
  pub output_format: String,
  pub variables: HashMap<String, String>,
}

impl TemplateNode {
  pub fn new(name: &str, template: &str) -> Self {
    Self {
      name: name.to_string(),
      template: template.to_string(),
      output_key: "output".to_string(),
      output_format: "text".to_string(),
      variables: HashMap::new(),
    }
  }

  pub fn with_output_key(mut self, key: &str) -> Self {
    self.output_key = key.to_string();
    self
  }

  pub fn with_format(mut self, format: &str) -> Self {
    self.output_format = format.to_string();
    self
  }

  pub fn with_variables(mut self, variables: HashMap<String, String>) -> Self {
    self.variables = variables;
    self
  }

  pub fn with_variable(mut self, key: &str, value: &str) -> Self {
    self.variables.insert(key.to_string(), value.to_string());
    self
  }
}

#[async_trait]
impl AsyncNode for TemplateNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let tera = get_tera();
    let mut context = tera::Context::new();

    // First, add node-defined variables to context
    for (key, value) in &self.variables {
      context.insert(key, value);
    }

    // Then, add inputs from the flow (can override variables)
    for (key, value) in inputs {
      let tera_value = flow_value_to_tera_value(value);
      context.insert(key, &tera_value);
    }

    tracing::debug!(name = %self.name, "rendering template");

    // Q3.8.2: avoid the `.lock().unwrap()` panic on a poisoned mutex.
    // A poisoned Tera mutex means a previous render panicked mid-flight;
    // the engine's internal state may be inconsistent. Rather than
    // crashing the workflow, surface the poison as an
    // `AsyncExecutionError` so the caller's retry / error-handling
    // logic gets a chance. We recover the guard via `into_inner` so
    // subsequent renders proceed (Tera::render_str takes `&mut self`
    // only because Tera caches parsed templates internally; the cache
    // is best-effort and a stale entry is acceptable).
    let rendered = {
      let mut tera_guard = tera.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(
          "Tera template engine mutex was poisoned by a prior panic; recovering and continuing"
        );
        poisoned.into_inner()
      });
      tera_guard
        .render_str(&self.template, &context)
        .map_err(|e| {
          // Tera's top-level message is just "Failed to render
          // '__tera_one_off'"; the cause (unknown variable, bad filter
          // argument, ...) is further down the source chain.
          let mut message = format!("Template rendering failed: {e}");
          let mut cause = std::error::Error::source(&e);
          while let Some(inner) = cause {
            message.push_str(&format!(": {inner}"));
            cause = inner.source();
          }
          YanshiError::NodeExecutionFailed { message }
        })?
    };

    // Parse result based on output format
    match self.output_format.as_str() {
      "json" => {
        tracing::debug!(rendered = %rendered, "attempting to parse rendered template as JSON");
        match serde_json::from_str::<Value>(&rendered) {
          Ok(Value::Object(map)) => {
            // When output format is JSON and it's an object, unpack fields into separate outputs
            tracing::debug!(
              field_count = map.len(),
              "template rendered successfully (JSON object unpacked)"
            );
            let mut outputs = HashMap::new();
            for (key, value) in map {
              outputs.insert(key, FlowValue::Json(value));
            }
            Ok(outputs)
          }
          Ok(json) => {
            // Non-object JSON, keep as single output
            tracing::debug!("template rendered successfully (non-object JSON)");
            let mut outputs = HashMap::new();
            outputs.insert(self.output_key.clone(), FlowValue::Json(json));
            Ok(outputs)
          }
          Err(e) => {
            // Invalid JSON, treat as string
            tracing::warn!(
              error = %e,
              rendered = %rendered,
              "template rendered but JSON parsing failed"
            );
            let mut outputs = HashMap::new();
            outputs.insert(
              self.output_key.clone(),
              FlowValue::Json(Value::String(rendered)),
            );
            Ok(outputs)
          }
        }
      }
      _ => {
        // F-A6-7: opportunistic JSON auto-detect for the default
        // (text / unset) format. When the rendered output starts
        // with `[` or `{` we attempt `serde_json::from_str` first
        // so downstream consumers (e.g. a map node's `input_list`,
        // a tool's `params`) receive a structured `FlowValue::
        // Json(Array | Object)` instead of a `FlowValue::Json(String)`.
        // Parse failures fall back to the legacy String wrap so
        // prose templates that happen to start with `{` (rare but
        // possible) stay safe.
        //
        // The explicit `output_format: "json"` path above remains
        // the strict-mode override: it logs the rendered content
        // and warns on parse failure, which is what you want when
        // the workflow author intends JSON and a bug would
        // otherwise hide as a downstream "wrong type" error far
        // from the producing template.
        let trimmed = rendered.trim_start();
        // Capture the first char once so the log line below can reuse it
        // without a second `chars().next().unwrap()` (Q5.1).
        let first_char = trimmed.chars().next();
        if matches!(first_char, Some('[') | Some('{'))
          && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
        {
          tracing::debug!(
            kind = if parsed.is_array() { "array" } else { "object" },
            leading_char = %first_char.unwrap_or('?'),
            "template rendered successfully (auto-detected JSON)"
          );
          let mut outputs = HashMap::new();
          outputs.insert(self.output_key.clone(), FlowValue::Json(parsed));
          return Ok(outputs);
        }
        tracing::debug!("template rendered successfully");
        let mut outputs = HashMap::new();
        outputs.insert(
          self.output_key.clone(),
          FlowValue::Json(Value::String(rendered)),
        );
        Ok(outputs)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[tokio::test]
  async fn test_template_node_simple_rendering() {
    let node = TemplateNode::new("simple_test", "Hello {{name}}, welcome to {{place}}!");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("name".to_string(), FlowValue::Json(json!("Alice")));
    inputs.insert("place".to_string(), FlowValue::Json(json!("Yanshi")));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(
      output,
      &FlowValue::Json(json!("Hello Alice, welcome to Yanshi!"))
    );
  }

  #[tokio::test]
  async fn test_template_node_with_variables() {
    let node = TemplateNode::new("var_test", "{{greeting}} {{name}}{{punctuation}}")
      .with_variable("greeting", "Hi")
      .with_variable("punctuation", "!");

    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("name".to_string(), FlowValue::Json(json!("Bob")));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("Hi Bob!")));
  }

  #[tokio::test]
  async fn test_template_node_json_output_format() {
    let node =
      TemplateNode::new("json_test", r#"{"message": "Hello {{name}}"}"#).with_format("json");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("name".to_string(), FlowValue::Json(json!("World")));

    let result = node.execute(&inputs).await.unwrap();
    // When format is "json" and result is an object, fields are unpacked
    // So we should have a "message" key, not "output"
    let message = result.get("message").unwrap();
    assert_eq!(message, &FlowValue::Json(json!("Hello World")));
  }

  // New Tera-specific tests

  #[tokio::test]
  async fn test_tera_conditional() {
    let node = TemplateNode::new(
      "conditional_test",
      "{% if show %}Hello {{name}}{% else %}Goodbye{% endif %}",
    );
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("show".to_string(), FlowValue::Json(json!(true)));
    inputs.insert("name".to_string(), FlowValue::Json(json!("Alice")));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("Hello Alice")));
  }

  #[tokio::test]
  async fn test_tera_conditional_false() {
    let node = TemplateNode::new(
      "conditional_test",
      "{% if show %}Hello{% else %}Goodbye{% endif %}",
    );
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("show".to_string(), FlowValue::Json(json!(false)));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("Goodbye")));
  }

  #[tokio::test]
  async fn test_tera_loop() {
    let node = TemplateNode::new(
      "loop_test",
      "{% for item in items %}{{item}}{% if not loop.last %},{% endif %}{% endfor %}",
    );
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("items".to_string(), FlowValue::Json(json!([1, 2, 3])));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("1,2,3")));
  }

  #[tokio::test]
  async fn test_tera_filters() {
    let node = TemplateNode::new("filter_test", "{{ name | upper }}");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("name".to_string(), FlowValue::Json(json!("alice")));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("ALICE")));
  }

  #[tokio::test]
  async fn test_tera_length_filter() {
    let node = TemplateNode::new("length_test", "{{ items | length }}");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("items".to_string(), FlowValue::Json(json!([1, 2, 3, 4, 5])));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("5")));
  }

  #[tokio::test]
  async fn test_tera_object_access() {
    let node = TemplateNode::new("object_test", "{{ user.name }} is {{ user.age }} years old");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert(
      "user".to_string(),
      FlowValue::Json(json!({
          "name": "Alice",
          "age": 30
      })),
    );

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("Alice is 30 years old")));
  }

  #[tokio::test]
  async fn test_tera_array_access() {
    let node = TemplateNode::new("array_test", "First: {{items.0}}, Last: {{items.2}}");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("items".to_string(), FlowValue::Json(json!(["a", "b", "c"])));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("First: a, Last: c")));
  }

  #[tokio::test]
  async fn test_tera_default_filter() {
    let node = TemplateNode::new("default_test", "{{ name | default(value='Anonymous') }}");
    let inputs = AsyncNodeInputs::new();

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("Anonymous")));
  }

  #[tokio::test]
  async fn test_tera_math() {
    let node = TemplateNode::new("math_test", "{{ count + 10 }}");
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("count".to_string(), FlowValue::Json(json!(5)));

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();
    assert_eq!(output, &FlowValue::Json(json!("15")));
  }

  #[tokio::test]
  async fn test_tera_complex_template() {
    let template = r#"
# {{ project_name }}

{% if tasks %}
## Tasks ({{ tasks | length }})
{% for task in tasks %}
- [{% if task.done %}x{% else %} {% endif %}] {{ task.name }}
{% endfor %}
{% else %}
No tasks found.
{% endif %}
"#;
    let node = TemplateNode::new("complex_test", template);
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("project_name".to_string(), FlowValue::Json(json!("Yanshi")));
    inputs.insert(
      "tasks".to_string(),
      FlowValue::Json(json!([
          {"name": "Task 1", "done": true},
          {"name": "Task 2", "done": false}
      ])),
    );

    let result = node.execute(&inputs).await.unwrap();
    let output = result.get("output").unwrap();

    if let FlowValue::Json(Value::String(s)) = output {
      assert!(s.contains("# Yanshi"));
      assert!(s.contains("## Tasks (2)"));
      assert!(s.contains("[x] Task 1"));
      assert!(s.contains("[ ] Task 2"));
    } else {
      panic!("Expected string output");
    }
  }
}
