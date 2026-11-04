//! End-to-end smoke test: a real `yanshi_agents::ReActAgent` driven
//! by the mock LLM provider, wrapped inside the Harness runtime.
//!
//! This complements the scripted unit tests in `runtime.rs` by proving
//! the `Box<dyn AgentRuntime>` boundary actually delivers a working
//! agent through the Harness wrapper. The mock provider is configured
//! via env vars, so a static `Mutex` serializes mutation across tests
//! inside this binary.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Mutex;
use yanshi_agents::eval::{ModelPricing, PricingTable};
use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_agents::runtime::RuntimeLimits;
use yanshi_harness::{
  AgentsMdProvider, HarnessEventBody, HarnessEventSink, HarnessRunOptions, HarnessRuntime,
  InMemoryEventSink, StopReason,
};
use yanshi_llm::Yanshi;
use yanshi_memory::SessionMemory;
use yanshi_tool::{Tool, ToolError, ToolIdempotency, ToolOutput};
use yanshi_tools::ToolRegistry;

fn env_lock() -> &'static Mutex<()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| Mutex::new(()))
}

async fn init_mock_model(model: &str) {
  let config_path =
    std::env::temp_dir().join(format!("yanshi-harness-mock-{}.yml", uuid::Uuid::new_v4()));
  std::fs::write(
    &config_path,
    format!(
      r#"
models:
  {model}:
    vendor: mock
    type: text
    model_id: {model}
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#
    ),
  )
  .unwrap();

  Yanshi::init_with_config(config_path.to_str().unwrap())
    .await
    .unwrap();
}

#[tokio::test]
async fn harness_runtime_drives_react_agent_with_mock_provider() {
  let _guard = env_lock().lock().await;
  let model = format!("mock-harness-{}", uuid::Uuid::new_v4());

  // SAFETY: env_lock() serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"answer directly","answer":"hi from harness"}"#,
      ])
      .unwrap(),
    );
  }

  init_mock_model(&model).await;

  let agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  let dir = tempfile::tempdir().unwrap();
  tokio::fs::write(
    dir.path().join("AGENTS.md"),
    "keep answers short; mention harness in the reply.\n",
  )
  .await
  .unwrap();

  let sink = Arc::new(InMemoryEventSink::new());
  let mut runtime = HarnessRuntime::new(Box::new(agent))
    .with_context_provider(Arc::new(AgentsMdProvider::new()))
    .with_event_sink(sink.clone() as Arc<dyn HarnessEventSink>);

  let result = runtime
    .run(HarnessRunOptions::new("hi", dir.path(), &model))
    .await
    .expect("harness run succeeds");

  assert_eq!(result.answer.as_deref(), Some("hi from harness"));
  assert_eq!(result.context_items_admitted, 1);
  assert!(!result.session_id.is_empty());

  let events = sink.snapshot().await;
  assert!(
    events.len() >= 3,
    "expected ≥3 events, got {}",
    events.len()
  );
  let first = &events[0];
  assert!(matches!(first.body, HarnessEventBody::SessionStarted(_)));
  assert_eq!(first.seq, 0, "first event must have seq 0");
  let last = events.last().unwrap();
  match &last.body {
    HarnessEventBody::Stopped(payload) => {
      assert_eq!(payload.reason, StopReason::Completed);
      assert_eq!(payload.final_answer.as_deref(), Some("hi from harness"));
    }
    other => panic!("expected stopped, got {other:?}"),
  }

  // SAFETY: cleanup of dedicated mock env vars after read.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

struct CountingEchoTool {
  calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingEchoTool {
  fn name(&self) -> &str {
    "counting_echo"
  }

  fn description(&self) -> &str {
    "Echo input and count executions"
  }

  fn parameters_schema(&self) -> serde_json::Value {
    serde_json::json!({
      "type": "object",
      "properties": {"text": {"type": "string"}},
      "required": ["text"]
    })
  }

  fn idempotency(&self, _params: &serde_json::Value) -> ToolIdempotency {
    ToolIdempotency::Idempotent
  }

  async fn execute(&self, params: serde_json::Value) -> Result<ToolOutput, ToolError> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    Ok(ToolOutput::success(format!(
      "echo: {}",
      params["text"].as_str().unwrap_or_default()
    )))
  }
}

/// $1.00 per call regardless of response content: the mock provider always
/// reports `prompt_tokens: 50`, so pricing entirely off `input_per_1k`
/// (with `output_per_1k: 0.0`) makes each call's cost a fixed, deterministic
/// amount. Mirrors `yanshi_agents::react::agent`'s own
/// `flat_dollar_per_call_pricing` test helper.
fn flat_dollar_per_call_pricing() -> PricingTable {
  PricingTable::default().with_default(ModelPricing {
    input_per_1k: 20.0,
    output_per_1k: 0.0,
    ..Default::default()
  })
}

/// U1.3 regression: T1.1 already proved `RuntimeLimits::cost_limit_usd`
/// stops a bare `ReActAgent::run_with_context` call
/// (`yanshi_agents::react::agent`'s own test suite) — but nothing
/// proved the limit actually survives the *full* path the CLI/server now
/// expose it through: `HarnessRunOptions::with_limits(...)` →
/// `HarnessRuntime::run(...)` → `AgentContext.limits` →
/// `ReActAgent::run_with_context`. This test drives that full path with
/// the mock provider (no live credentials needed) and asserts the harness
/// event stream's terminal `Stopped` event reports the cost-limit reason.
#[tokio::test]
async fn harness_runtime_stops_react_agent_when_cost_limit_usd_is_exceeded() {
  let _guard = env_lock().lock().await;
  let model = format!("mock-harness-cost-limit-{}", uuid::Uuid::new_v4());

  // SAFETY: env_lock() serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![serde_json::json!({"id": "c1", "name": "counting_echo", "arguments": {"text": "a"}})],
        vec![serde_json::json!({"id": "c2", "name": "counting_echo", "arguments": {"text": "b"}})],
        vec![serde_json::json!({"id": "c3", "name": "counting_echo", "arguments": {"text": "c"}})],
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }

  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingEchoTool {
    calls: calls.clone(),
  }));

  let agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(10)
      .without_loop_detection()
      .with_pricing_table(flat_dollar_per_call_pricing()),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let dir = tempfile::tempdir().unwrap();
  let sink = Arc::new(InMemoryEventSink::new());
  let mut runtime =
    HarnessRuntime::new(Box::new(agent)).with_event_sink(sink.clone() as Arc<dyn HarnessEventSink>);

  let options = HarnessRunOptions::new("go", dir.path(), &model).with_limits(RuntimeLimits {
    cost_limit_usd: Some(1.5),
    ..Default::default()
  });
  let result = runtime.run(options).await.expect("harness run completes");

  assert!(result.answer.is_none(), "no final answer was ever reached");

  let events = sink.snapshot().await;
  let last = events.last().unwrap();
  match &last.body {
    HarnessEventBody::Stopped(payload) => {
      assert_eq!(payload.reason, StopReason::LimitReached);
      let error = payload.error.as_deref().unwrap_or_default();
      assert!(
        error.contains("cost_limit_usd exceeded"),
        "expected a cost_limit_usd error, got: {error}"
      );
    }
    other => panic!("expected stopped, got {other:?}"),
  }
  // Two calls at $1.00 each = $2.00 exceeds the $1.50 budget; the guard
  // reacts at the next turn boundary, so the 3rd queued tool call never runs.
  assert_eq!(calls.load(Ordering::SeqCst), 2);

  // SAFETY: cleanup of dedicated mock env vars after read.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}
