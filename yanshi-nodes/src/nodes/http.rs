use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Value, json};
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_tools::SandboxPolicy;
use yanshi_tools::builtin::{HttpResponse, HttpTool};

/// Workflow node that performs an HTTP request.
///
/// `HttpNode` delegates to [`yanshi_tools::builtin::HttpTool`] so the
/// workflow node surface inherits the same SSRF defenses (private-IP /
/// cloud-metadata / link-local blocking), redirect cap (max 10 hops with
/// per-hop re-validation), 30-second timeout, and host allowlist that
/// the agent tool already enforces (Q1.3.2 — closes yanshi-nodes.md
/// C2). The default policy is permissive on domain allow-listing but
/// strict on private/loopback IP classes; pin via [`HttpNode::with_policy`]
/// to lock down further.
#[derive(Clone)]
pub struct HttpNode {
  policy: Arc<SandboxPolicy>,
}

impl std::fmt::Debug for HttpNode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HttpNode").finish_non_exhaustive()
  }
}

impl HttpNode {
  pub fn new(policy: Arc<SandboxPolicy>) -> Self {
    Self { policy }
  }

  pub fn with_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
    self.policy = policy;
    self
  }
}

impl Default for HttpNode {
  fn default() -> Self {
    // The default policy denies private IPs / cloud metadata / loopback
    // but allows arbitrary public domains (allowed_domains empty == permissive).
    Self {
      policy: Arc::new(SandboxPolicy::default()),
    }
  }
}

#[async_trait]
impl AsyncNode for HttpNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let url = get_string_input(inputs, "url")?;
    let method = get_optional_string_input(inputs, "method")?.unwrap_or("GET");
    let headers = get_optional_map_input(inputs, "headers")?;
    let body = get_optional_string_input(inputs, "body")?;

    // `send` returns the full body; HttpTool's 8 KB truncation only
    // applies to its agent-facing `execute`.
    let mut params = json!({
        "url": url,
        "method": method,
    });
    if let Some(b) = body {
      params["body"] = json!(b);
    }
    if let Some(h) = headers {
      params["headers"] = json!(h);
    }

    let tool =
      HttpTool::new(self.policy.clone()).map_err(|err| YanshiError::AsyncExecutionError {
        message: format!("HttpNode failed to build HTTP client: {err}"),
      })?;
    let response = tool
      .send(&params)
      .await
      .map_err(|err| YanshiError::AsyncExecutionError {
        message: err.to_string(),
      })?;
    response_outputs(response)
  }
}

/// Longest body excerpt carried in a non-2xx error message.
const ERROR_BODY_CHARS: usize = 2048;

/// A 2xx response becomes `status` (the real code) + `body`; any other
/// status fails the node, so retries and the run status see the failure
/// instead of downstream nodes consuming an error page as data.
fn response_outputs(response: HttpResponse) -> AsyncNodeResult {
  let HttpResponse { status, body } = response;
  if !status.is_success() {
    let excerpt: String = body.chars().take(ERROR_BODY_CHARS).collect();
    let message = format!("HTTP {status}: {excerpt}");
    // 5xx and 429 may succeed on a later attempt; other statuses won't,
    // so they use the variant the distributed scheduler never retries.
    return Err(
      if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        YanshiError::AsyncExecutionError { message }
      } else {
        YanshiError::NodeExecutionFailed { message }
      },
    );
  }
  let mut outputs = HashMap::new();
  outputs.insert(
    "status".to_string(),
    FlowValue::Json(json!(status.as_u16())),
  );
  outputs.insert("body".to_string(), FlowValue::Json(json!(body)));
  Ok(outputs)
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

fn get_optional_map_input(
  inputs: &AsyncNodeInputs,
  key: &str,
) -> Result<Option<HashMap<String, String>>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(FlowValue::Json(Value::Object(map))) => {
      let mut result = HashMap::new();
      for (k, v) in map {
        if let Value::String(s) = v {
          result.insert(k.clone(), s.clone());
        }
      }
      Ok(Some(result))
    }
    _ => Err(YanshiError::NodeInputError {
      message: format!("Input '{}' has wrong type, expected a map", key),
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn response(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
      status: StatusCode::from_u16(status).unwrap(),
      body: body.to_string(),
    }
  }

  #[test]
  fn non_success_status_fails_the_node() {
    for (code, retryable) in [(404, false), (302, false), (500, true), (429, true)] {
      let err = response_outputs(response(code, "nope")).unwrap_err();
      assert_eq!(
        matches!(err, YanshiError::AsyncExecutionError { .. }),
        retryable,
        "{code}"
      );
      assert!(
        err.to_string().contains(&format!("HTTP {code}")) && err.to_string().contains("nope"),
        "{code}: {err}"
      );
    }
    let long = "x".repeat(ERROR_BODY_CHARS * 2);
    let err = response_outputs(response(500, &long))
      .unwrap_err()
      .to_string();
    assert!(
      err.len() < ERROR_BODY_CHARS + 100,
      "body excerpt is bounded"
    );
  }

  #[test]
  fn success_reports_the_real_status_and_full_body() {
    let outputs = response_outputs(response(201, "created")).unwrap();
    assert_eq!(outputs["status"], FlowValue::Json(json!(201)));
    assert_eq!(outputs["body"], FlowValue::Json(json!("created")));
  }

  /// Q1.3.2 regression: a freshly-constructed `HttpNode` must refuse
  /// SSRF attempts at the cloud-metadata IP without the operator
  /// having to wire any policy — the default policy denies the
  /// LinkLocal / CloudMetadata classes automatically.
  #[tokio::test]
  async fn default_policy_rejects_cloud_metadata_ssrf() {
    let node = HttpNode::default();
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert(
      "url".to_string(),
      FlowValue::Json(json!("http://169.254.169.254/latest/meta-data/")),
    );

    let err = node.execute(&inputs).await.unwrap_err();
    let message = err.to_string();
    assert!(
      message.contains("CloudMetadata")
        || message.contains("LinkLocal")
        || message.contains("Sandbox"),
      "expected SSRF rejection message, got: {message}"
    );
  }

  #[tokio::test]
  async fn default_policy_rejects_private_ip() {
    let node = HttpNode::default();
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("url".to_string(), FlowValue::Json(json!("http://10.0.0.1")));

    let err = node.execute(&inputs).await.unwrap_err();
    assert!(err.to_string().contains("Private") || err.to_string().contains("Sandbox"));
  }

  #[tokio::test]
  async fn explicit_policy_allows_loopback_for_tests() {
    // Confirms the policy wiring carries through. We can't easily run a
    // mock HTTP server here without re-creating one, but proving the
    // URL passes the validator (instead of being denied) is enough for
    // the wiring regression; the actual GET happens in
    // `yanshi-tools::builtin::http::tests` which has full coverage.
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let policy = Arc::new(SandboxPolicy {
      allow_loopback_network_access: true,
      ..SandboxPolicy::default()
    });
    let node = HttpNode::new(policy);
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert(
      "url".to_string(),
      FlowValue::Json(json!(format!("http://127.0.0.1:{port}"))),
    );
    // The server is gone so we expect a connection error, not a
    // sandbox denial — that proves the URL passed the validator.
    let err = node.execute(&inputs).await.unwrap_err();
    let message = err.to_string();
    assert!(
      !message.contains("Loopback"),
      "loopback should be allowed under explicit policy, but got: {message}"
    );
  }
}
