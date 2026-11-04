//! Y2.5 acceptance: a billed node's `max_retries` repeats only failures
//! a retry can fix. Under `vendor: mock` a `mock:http=<status>` prompt
//! makes text-to-image fail with that status; the mock counts calls.
//!
//! - HTTP 400 (a content rejection): exactly one vendor call.
//! - HTTP 503: every allowed attempt is used (1 + `max_retries`).

use std::collections::HashMap;

use yanshi_config::executor::build_flow_from_yaml;
use yanshi_core::{FlowExecutionConfig, FlowExt, error::YanshiError};
use yanshi_llm::providers::mock_text2image_calls;

const MOCK_REGISTRY: &str = r#"
models:
  mock-t2i:
    vendor: mock
    type: text_to_image
    model_id: mock-t2i
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#;

async fn install_mock_registry() {
  static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir =
        std::env::temp_dir().join(format!("yanshi-retry-discipline-{}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("dir");
      let path = dir.join("models.yml");
      std::fs::write(&path, MOCK_REGISTRY).expect("registry");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe { std::env::set_var("YANSHI_MODELS_CONFIG", &path) };
      yanshi_llm::Yanshi::init_with_config(path.to_str().expect("utf-8"))
        .await
        .expect("mock registry loads");
    })
    .await;
}

/// Run one `text_to_image` node with `max_retries: 3`; return the node's
/// error and how many vendor calls it made.
async fn run_failing(status: u16) -> (YanshiError, usize) {
  let yaml = format!(
    r#"
name: retry discipline
nodes:
  - id: draw
    type: text_to_image
    max_retries: 3
    parameters:
      model: mock-t2i
      prompt: "a circuit mock:http={status}"
"#
  );
  let runs = tempfile::TempDir::new().expect("tempdir");
  let before = mock_text2image_calls();
  let state = build_flow_from_yaml(&yaml, None)
    .expect("builds")
    .execute_from_inputs_with_config(
      HashMap::new(),
      FlowExecutionConfig::serial().with_run_base_dir(runs.path()),
    )
    .await
    .expect("run returns its state");
  let error = match &state["draw"] {
    Err(error) => error.clone(),
    Ok(outputs) => panic!("HTTP {status} must fail the node, got {outputs:?}"),
  };
  (error, mock_text2image_calls() - before)
}

#[tokio::test]
async fn a_rejection_is_not_retried_but_a_server_error_is() {
  install_mock_registry().await;

  let (error, calls) = run_failing(400).await;
  assert_eq!(calls, 1, "a 4xx must not be retried: {error}");
  assert!(
    matches!(error, YanshiError::NodeExecutionFailed { .. }),
    "{error:?}"
  );
  assert!(error.to_string().contains("400"), "{error}");

  let (error, calls) = run_failing(503).await;
  // `max_retries: 3` is the first attempt plus three retries.
  assert_eq!(calls, 4, "a 5xx uses every attempt: {error}");
}
