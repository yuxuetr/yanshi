use super::config::*;
use super::core::*;
use super::support::*;
use super::turn_driven::*;

use chrono::Utc;
use yanshi_llm::{ToolCallRequest, Yanshi};
use yanshi_memory::{MemoryStore, Message, Role};
use yanshi_tool::{ToolIdempotency, ToolRegistry};

use crate::react::parser::AgentResponse;
use crate::runtime::{
  AgentCancellationToken, AgentContext, AgentEvent, AgentMemoryHook, AgentRunResult, AgentStep,
  AgentStepKind, AgentStopReason, MemoryHookContext, MemoryHookKind, RuntimeLimits,
};
use crate::verification::{VerificationContext, VerificationOutcome, VerificationStrategy};

use async_trait::async_trait;
use serde_json::{Value, json};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use yanshi_agent_spi::checkpoint::AgentLoopCheckpointer as _;
use yanshi_memory::SessionMemory;
use yanshi_tool::{Tool, ToolError, ToolOutput};

struct EchoTool;

struct CountingTool {
  calls: Arc<AtomicUsize>,
}

#[derive(Default)]
struct RecordingMemoryHook {
  events: Mutex<Vec<MemoryHookContext>>,
}

impl AgentMemoryHook for RecordingMemoryHook {
  fn on_memory_read(&self, context: &MemoryHookContext) {
    self.events.lock().unwrap().push(context.clone());
  }

  fn on_memory_write(&self, context: &MemoryHookContext) {
    self.events.lock().unwrap().push(context.clone());
  }
}

#[derive(Default)]
struct RecordingSummaryBackend {
  contexts: Mutex<Vec<MemorySummaryContext>>,
}

#[async_trait]
impl MemorySummaryBackend for RecordingSummaryBackend {
  fn name(&self) -> &'static str {
    "recording"
  }

  async fn summarize(&self, context: MemorySummaryContext) -> Result<Option<String>, ReActError> {
    self.contexts.lock().unwrap().push(context.clone());
    Ok(Some(format!(
      "[Custom Summary] omitted={} kept={}",
      context.omitted_messages.len(),
      context.kept_messages.len()
    )))
  }
}

#[async_trait]
impl Tool for EchoTool {
  fn name(&self) -> &str {
    "echo"
  }

  fn description(&self) -> &str {
    "Echo test input"
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "text": {"type": "string"}
      },
      "required": ["text"]
    })
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::success(format!(
      "echo: {}",
      params["text"].as_str().unwrap_or_default()
    )))
  }
}

#[async_trait]
impl Tool for CountingTool {
  fn name(&self) -> &str {
    "counting_echo"
  }

  fn description(&self) -> &str {
    "Echo input and count executions"
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "text": {"type": "string"}
      },
      "required": ["text"]
    })
  }

  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    ToolIdempotency::Idempotent
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    Ok(ToolOutput::success(format!(
      "echo: {}",
      params["text"].as_str().unwrap_or_default()
    )))
  }
}

async fn init_mock_model(model: &str) {
  let config_path =
    std::env::temp_dir().join(format!("yanshi-agents-mock-{}.yml", uuid::Uuid::new_v4()));
  fs::write(
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

#[test]
fn truncate_str_at_char_boundary_never_splits_a_multibyte_codepoint() {
  // Each "测" / "试" character is 3 UTF-8 bytes; a naive `&s[..200]` byte
  // slice lands mid-character here (200 is not a multiple of 3 past the
  // ASCII prefix) and would panic pre-fix.
  let s = "测试".repeat(150);
  let truncated = truncate_str_at_char_boundary(&s, 200);
  assert!(truncated.len() <= 200);
  assert!(s.is_char_boundary(truncated.len()));
  assert_eq!(&s[..truncated.len()], truncated);

  // Strings at/under the budget are returned unchanged.
  assert_eq!(truncate_str_at_char_boundary("short", 200), "short");
  assert_eq!(truncate_str_at_char_boundary("", 200), "");
}

/// V0.1 regression: a >200-byte CJK tool observation must not panic the
/// ReAct turn when it's truncated for the "Observation: ..." log preview.
#[tokio::test]
async fn run_with_context_does_not_panic_on_multibyte_utf8_observation() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-utf8-trunc-{}", uuid::Uuid::new_v4());
  let long_cjk = "测试".repeat(150);
  let first_response = json!({
    "thought": "use tool",
    "action": {"tool": "echo", "params": {"text": long_cjk}}
  })
  .to_string();
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        first_response,
        r#"{"thought":"done","answer":"final"}"#.to_string(),
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new("utf8-trunc", "say hi in chinese", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("final"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
}

#[tokio::test]
async fn run_with_context_records_steps_events_and_reflection_with_mock_llm() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-runtime-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"use tool","action":{"tool":"echo","params":{"text":"hi"}}}"#,
        r#"{"thought":"done","answer":"final: echo: hi"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let memory_hook = Arc::new(RecordingMemoryHook::default());
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_memory_hook(memory_hook.clone())
  .with_reflection_strategy(Arc::new(crate::reflection::FinalReflection));

  let result = agent
    .run_with_context(AgentContext::new("session-1", "say hi", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("final: echo: hi"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert!(
    result
      .steps
      .iter()
      .any(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
  );
  assert!(
    result
      .steps
      .iter()
      .any(|step| matches!(step.kind, AgentStepKind::ToolResult { .. }))
  );
  assert!(
    result
      .steps
      .iter()
      .any(|step| matches!(step.kind, AgentStepKind::Reflect { .. }))
  );
  assert!(
    result
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::ToolCallCompleted { .. }))
  );
  assert!(
    result
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::ReflectionAdded { .. }))
  );

  let memory_events = memory_hook.events.lock().unwrap();
  let read_sizes: Vec<usize> = memory_events
    .iter()
    .filter(|event| event.kind == MemoryHookKind::ReadHistory)
    .map(|event| event.messages.len())
    .collect();
  assert_eq!(read_sizes, vec![1, 3]);
  assert_eq!(
    memory_events
      .iter()
      .filter(|event| event.kind == MemoryHookKind::Write)
      .count(),
    4
  );
}

/// V2.4: delegates to a shared `SessionMemory` so a checkpoint-resumed
/// `ReActAgent` (a fresh instance, not the interrupted one) sees the
/// same conversation history the interrupted run wrote — mirroring how
/// a real resume needs a durable, session-id-keyed `MemoryStore`
/// (`SqliteMemory`) rather than each agent owning a private in-process
/// one. `SessionMemory` itself owns its map directly (no internal
/// `Arc`), so two independent instances never see each other's writes —
/// this wrapper is what lets two separate `ReActAgent`s share one.
struct SharedSessionMemory(Arc<SessionMemory>);

#[async_trait]
impl MemoryStore for SharedSessionMemory {
  async fn add_message(&self, message: Message) -> Result<(), yanshi_memory::MemoryError> {
    self.0.add_message(message).await
  }
  async fn get_history(
    &self,
    session_id: &str,
    limit: usize,
  ) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.0.get_history(session_id, limit).await
  }
  async fn get_all(&self, session_id: &str) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.0.get_all(session_id).await
  }
  async fn search(
    &self,
    session_id: &str,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.0.search(session_id, query, limit).await
  }
  async fn clear_session(&self, session_id: &str) -> Result<(), yanshi_memory::MemoryError> {
    self.0.clear_session(session_id).await
  }
  async fn session_token_count(&self, session_id: &str) -> Result<u32, yanshi_memory::MemoryError> {
    self.0.session_token_count(session_id).await
  }
}

/// V2.4: test-double `AgentLoopCheckpointer` backed by an in-process map
/// (no real file I/O — `FileLoopCheckpointer`'s own round-trip is
/// covered separately in `yanshi-agents::checkpoint`'s unit tests).
/// `cancel_after` optionally simulates "the process died mid-loop": once
/// `save` has been called that many times, it fires the paired
/// cancellation token, which `run_one_turn`'s pre-LLM check picks up on
/// the *next* turn — deterministically stopping the loop with
/// `AgentStopReason::Cancelled` before it can consume the next scripted
/// LLM response, without any real process being killed.
#[derive(Clone)]
struct RecordingCheckpointer {
  store: Arc<
    Mutex<std::collections::HashMap<String, yanshi_agent_spi::checkpoint::AgentLoopCheckpoint>>,
  >,
  saves: Arc<AtomicUsize>,
  cancel_after: Option<(usize, AgentCancellationToken)>,
}

impl RecordingCheckpointer {
  fn new() -> Self {
    Self {
      store: Arc::new(Mutex::new(std::collections::HashMap::new())),
      saves: Arc::new(AtomicUsize::new(0)),
      cancel_after: None,
    }
  }

  fn with_cancel_after(mut self, count: usize, token: AgentCancellationToken) -> Self {
    self.cancel_after = Some((count, token));
    self
  }
}

#[async_trait]
impl yanshi_agent_spi::checkpoint::AgentLoopCheckpointer for RecordingCheckpointer {
  async fn save(
    &self,
    checkpoint: &yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
  ) -> Result<(), yanshi_agent_spi::checkpoint::AgentLoopCheckpointError> {
    self
      .store
      .lock()
      .unwrap()
      .insert(checkpoint.session_id.clone(), checkpoint.clone());
    let count = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some((target, token)) = &self.cancel_after
      && count >= *target
    {
      token.cancel();
    }
    Ok(())
  }

  async fn load(
    &self,
    session_id: &str,
  ) -> Result<
    Option<yanshi_agent_spi::checkpoint::AgentLoopCheckpoint>,
    yanshi_agent_spi::checkpoint::AgentLoopCheckpointError,
  > {
    Ok(self.store.lock().unwrap().get(session_id).cloned())
  }

  async fn clear(
    &self,
    session_id: &str,
  ) -> Result<(), yanshi_agent_spi::checkpoint::AgentLoopCheckpointError> {
    self.store.lock().unwrap().remove(session_id);
    Ok(())
  }
}

/// V2.4 acceptance scenario: a ReAct loop is "interrupted" (simulated
/// process death via forced cancellation right after a checkpoint save,
/// not a real process kill — see `RecordingCheckpointer`) after 2 tool-
/// call turns; resuming from the saved checkpoint with a brand-new
/// `ReActAgent` instance continues from turn 3 and reaches the same
/// final answer an uninterrupted control run produces, having made
/// exactly one more LLM call (not three) — proving no completed turn is
/// re-executed.
#[tokio::test]
async fn resume_from_loop_checkpoint_continues_a_cancelled_run_to_the_same_answer_as_a_control_run()
{
  let _guard = crate::LLM_TEST_LOCK.lock().await;

  let turn1 = r#"{"thought":"step one","action":{"tool":"echo","params":{"text":"a"}}}"#;
  let turn2 = r#"{"thought":"step two","action":{"tool":"echo","params":{"text":"b"}}}"#;
  let turn3 = r#"{"thought":"done","answer":"control-final-answer"}"#;

  // ── Control run: full 3-turn script, uninterrupted. ──
  let control_model = format!("mock-loop-ckpt-control-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![turn1, turn2, turn3]).unwrap(),
    );
  }
  init_mock_model(&control_model).await;
  let mut control_registry = ToolRegistry::new();
  control_registry.register(Arc::new(EchoTool));
  let mut control_agent = ReActAgent::new(
    ReActConfig::new(&control_model).with_max_iterations(6),
    Box::new(SessionMemory::default_window()),
    Arc::new(control_registry),
  );
  let control_result = control_agent
    .run_with_context(AgentContext::new(
      "loop-ckpt-session",
      "do the two-step task",
      &control_model,
    ))
    .await
    .unwrap();
  assert_eq!(
    control_result.answer.as_deref(),
    Some("control-final-answer")
  );
  assert_eq!(control_result.stop_reason, AgentStopReason::FinalAnswer);

  // ── Interrupted run: only the first 2 turns scripted; a checkpointer
  // wired to trigger cancellation right after the 2nd save. ──
  let interrupted_model = format!("mock-loop-ckpt-interrupted-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![turn1, turn2]).unwrap(),
    );
  }
  init_mock_model(&interrupted_model).await;
  let mut interrupted_registry = ToolRegistry::new();
  interrupted_registry.register(Arc::new(EchoTool));
  // Shared memory stands in for a durable, session-id-keyed store
  // (SqliteMemory in production) that survives the "process restart" —
  // both the interrupted and resumed agent instances read/write it.
  let shared_memory = Arc::new(SessionMemory::default_window());
  let mut interrupted_agent = ReActAgent::new(
    ReActConfig::new(&interrupted_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory.clone())),
    Arc::new(interrupted_registry),
  );

  let cancel_token = AgentCancellationToken::new();
  let checkpointer = RecordingCheckpointer::new().with_cancel_after(2, cancel_token.clone());
  let checkpointer_handle: Arc<dyn yanshi_agent_spi::checkpoint::AgentLoopCheckpointer> =
    Arc::new(checkpointer.clone());

  let interrupted_context = AgentContext::new(
    "loop-ckpt-session",
    "do the two-step task",
    &interrupted_model,
  )
  .with_cancellation_token(cancel_token)
  .with_loop_checkpointer(checkpointer_handle.clone());
  let interrupted_result = interrupted_agent
    .run_with_context(interrupted_context)
    .await
    .unwrap();
  assert!(
    matches!(
      interrupted_result.stop_reason,
      AgentStopReason::Cancelled { .. }
    ),
    "expected Cancelled, got {:?}",
    interrupted_result.stop_reason
  );
  assert_eq!(checkpointer.saves.load(Ordering::SeqCst), 2);

  let checkpoint = checkpointer
    .load("loop-ckpt-session")
    .await
    .unwrap()
    .expect("a checkpoint must have been saved before cancellation");
  assert_eq!(checkpoint.tool_calls, 2);
  assert_eq!(
    checkpoint
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::ToolCall { .. }))
      .count(),
    2
  );

  // ── Resume: a brand-new ReActAgent instance, only the remaining
  // 1-turn script available. If resume incorrectly restarted the loop
  // from scratch, it would need 3 calls against a 1-item queue and
  // fail (or fall back to an unparseable default response) instead of
  // reaching the control run's answer. ──
  let resume_model = format!("mock-loop-ckpt-resume-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![turn3]).unwrap(),
    );
  }
  init_mock_model(&resume_model).await;
  let mut resume_registry = ToolRegistry::new();
  resume_registry.register(Arc::new(EchoTool));
  let mut resumed_agent = ReActAgent::new(
    ReActConfig::new(&resume_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory)),
    Arc::new(resume_registry),
  );
  let resume_context = AgentContext::new("loop-ckpt-session", "", &resume_model)
    .with_loop_checkpointer(checkpointer_handle);
  let resumed_result = resumed_agent
    .resume_from_loop_checkpoint(resume_context, checkpoint.clone(), None)
    .await
    .unwrap();

  assert_eq!(
    resumed_result.answer.as_deref(),
    Some("control-final-answer")
  );
  assert_eq!(resumed_result.stop_reason, AgentStopReason::FinalAnswer);
  assert_eq!(resumed_result.answer, control_result.answer);
  // Continuity: the resumed result's steps/events carry the checkpoint's
  // history forward rather than starting a fresh run at step 0.
  assert!(resumed_result.steps.len() > checkpoint.steps.len());
  assert_eq!(
    resumed_result.steps[..checkpoint.steps.len()],
    checkpoint.steps[..]
  );
  // Successful completion clears the checkpoint (FinalAnswer is in the
  // "keep = false" i.e. clear bucket of `should_clear_checkpoint`).
  assert_eq!(checkpointer.load("loop-ckpt-session").await.unwrap(), None);
}

/// V2.4: the turn-driven seam (`begin_turn_driven`/`ReActLoopSession::
/// next_turn`, used under `--context-refresh`) must get the same
/// checkpoint coverage as `run_with_context` — this pins that a caller
/// simply *not calling* `next_turn` again (no cancellation trickery
/// needed here, unlike the `run_with_context` variant above, since the
/// caller genuinely owns when the next turn happens) leaves a usable
/// checkpoint behind, and that `resume_from_loop_checkpoint` continues
/// from it correctly regardless of which loop-owner produced the
/// interruption.
#[tokio::test]
async fn turn_driven_session_checkpoints_after_each_turn_and_resumes_correctly() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;

  let turn1 = r#"{"thought":"step one","action":{"tool":"echo","params":{"text":"a"}}}"#;
  let turn2 = r#"{"thought":"done","answer":"turn-driven-final-answer"}"#;

  let interrupted_model = format!("mock-loop-ckpt-td-interrupted-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![turn1]).unwrap(),
    );
  }
  init_mock_model(&interrupted_model).await;
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let shared_memory = Arc::new(SessionMemory::default_window());
  let mut interrupted_agent = ReActAgent::new(
    ReActConfig::new(&interrupted_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory.clone())),
    Arc::new(registry),
  );

  let checkpointer = RecordingCheckpointer::new();
  let checkpointer_handle: Arc<dyn yanshi_agent_spi::checkpoint::AgentLoopCheckpointer> =
    Arc::new(checkpointer.clone());
  let context = AgentContext::new(
    "loop-ckpt-td-session",
    "do the one-step task",
    &interrupted_model,
  )
  .with_loop_checkpointer(checkpointer_handle.clone());

  let session = interrupted_agent.begin_turn_driven(context).await.unwrap();
  match session.next_turn().await.unwrap() {
    ReActTurn::Continued(_next_session) => {
      // "Process death": the boxed session (and the one scripted
      // response's tool-call turn it already consumed) is simply
      // dropped here — no further `next_turn` call happens.
    }
    ReActTurn::Finished { .. } => panic!("expected the run to still be mid-loop"),
  }
  assert_eq!(checkpointer.saves.load(Ordering::SeqCst), 1);

  let checkpoint = checkpointer
    .load("loop-ckpt-td-session")
    .await
    .unwrap()
    .expect("a checkpoint must have been saved after the first turn");
  assert_eq!(checkpoint.tool_calls, 1);

  let resume_model = format!("mock-loop-ckpt-td-resume-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![turn2]).unwrap(),
    );
  }
  init_mock_model(&resume_model).await;
  let mut resume_registry = ToolRegistry::new();
  resume_registry.register(Arc::new(EchoTool));
  let mut resumed_agent = ReActAgent::new(
    ReActConfig::new(&resume_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory)),
    Arc::new(resume_registry),
  );
  let resume_context = AgentContext::new("loop-ckpt-td-session", "", &resume_model)
    .with_loop_checkpointer(checkpointer_handle);
  let resumed_result = resumed_agent
    .resume_from_loop_checkpoint(resume_context, checkpoint, None)
    .await
    .unwrap();

  assert_eq!(
    resumed_result.answer.as_deref(),
    Some("turn-driven-final-answer")
  );
  assert_eq!(resumed_result.stop_reason, AgentStopReason::FinalAnswer);
}

// ── V2.3: ask_user / HITL interrupt-resume ───────────────────────────

/// V2.3 acceptance scenario: an `ask_user` native tool call pauses the
/// loop with `AwaitingInput`, the checkpoint records the question, and
/// resuming with a fresh `ReActAgent` instance + the caller's answer
/// reaches the same final answer a control run (which never needed to
/// ask) would.
#[tokio::test]
async fn run_with_context_stops_with_awaiting_input_on_ask_user_and_resumes_with_answer() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;

  // ── Control run: never asks, answers directly. ──
  let control_model = format!("mock-ask-user-control-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"done","answer":"deploy complete: staging"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&control_model).await;
  let mut control_registry = ToolRegistry::new();
  control_registry.register(Arc::new(EchoTool));
  let mut control_agent = ReActAgent::new(
    ReActConfig::new(&control_model).with_max_iterations(6),
    Box::new(SessionMemory::default_window()),
    Arc::new(control_registry),
  );
  let control_result = control_agent
    .run_with_context(AgentContext::new(
      "ask-user-session",
      "deploy the app",
      &control_model,
    ))
    .await
    .unwrap();
  assert_eq!(
    control_result.answer.as_deref(),
    Some("deploy complete: staging")
  );

  // ── Interrupted run: the model asks a question via the ask_user
  // native tool instead of dispatching a real tool or answering. ──
  let interrupted_model = format!("mock-ask-user-interrupted-{}", uuid::Uuid::new_v4());
  let question = "what's the deploy target?";
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![serde_json::json!({
        "id": "call_0",
        "name": ASK_USER_TOOL_NAME,
        "arguments": {"question": question}
      })]])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused — native tool call)"]).unwrap(),
    );
  }
  init_mock_model(&interrupted_model).await;
  let mut interrupted_registry = ToolRegistry::new();
  interrupted_registry.register(Arc::new(EchoTool));
  let shared_memory = Arc::new(SessionMemory::default_window());
  let mut interrupted_agent = ReActAgent::new(
    ReActConfig::new(&interrupted_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory.clone())),
    Arc::new(interrupted_registry),
  );
  let checkpointer = RecordingCheckpointer::new();
  let checkpointer_handle: Arc<dyn yanshi_agent_spi::checkpoint::AgentLoopCheckpointer> =
    Arc::new(checkpointer.clone());
  let interrupted_result = interrupted_agent
    .run_with_context(
      AgentContext::new("ask-user-session", "deploy the app", &interrupted_model)
        .with_loop_checkpointer(checkpointer_handle.clone()),
    )
    .await
    .unwrap();
  assert_eq!(
    interrupted_result.stop_reason,
    AgentStopReason::AwaitingInput {
      question: question.to_string()
    }
  );
  assert!(interrupted_result.steps.iter().any(
    |s| matches!(&s.kind, AgentStepKind::ToolCall { tool, .. } if tool == ASK_USER_TOOL_NAME)
  ));

  let checkpoint = checkpointer
    .load("ask-user-session")
    .await
    .unwrap()
    .expect("a checkpoint must have been saved when the loop paused");
  assert_eq!(checkpoint.pending_question.as_deref(), Some(question));

  // ── Resume: a brand-new ReActAgent instance, answer supplied. ──
  let resume_model = format!("mock-ask-user-resume-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"done","answer":"deploy complete: staging"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&resume_model).await;
  let mut resume_registry = ToolRegistry::new();
  resume_registry.register(Arc::new(EchoTool));
  let mut resumed_agent = ReActAgent::new(
    ReActConfig::new(&resume_model).with_max_iterations(6),
    Box::new(SharedSessionMemory(shared_memory.clone())),
    Arc::new(resume_registry),
  );
  let resume_context = AgentContext::new("ask-user-session", "", &resume_model)
    .with_loop_checkpointer(checkpointer_handle);
  let resumed_result = resumed_agent
    .resume_from_loop_checkpoint(resume_context, checkpoint, Some("staging".to_string()))
    .await
    .unwrap();

  assert_eq!(resumed_result.answer, control_result.answer);
  assert_eq!(resumed_result.stop_reason, AgentStopReason::FinalAnswer);
  // The answer must have been written to memory before the resumed
  // turn, and a ToolResult step pushed for the paused ask_user call.
  let history = shared_memory.get_all("ask-user-session").await.unwrap();
  assert!(history.iter().any(|m| m.content.contains("staging")));
  assert!(
      resumed_result
        .steps
        .iter()
        .any(|s| matches!(&s.kind, AgentStepKind::ToolResult { tool, content, .. } if tool == ASK_USER_TOOL_NAME && content == "staging"))
    );
  // Successful completion clears the checkpoint.
  assert_eq!(checkpointer.load("ask-user-session").await.unwrap(), None);

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

/// Minimal, otherwise-valid React checkpoint for the two validation
/// tests below — only `pending_question` varies between them.
fn bare_react_checkpoint(
  pending_question: Option<String>,
) -> yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
  yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
    schema_version: yanshi_agent_spi::checkpoint::AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
    session_id: "s".into(),
    runtime_kind: yanshi_agent_spi::checkpoint::LoopRuntimeKind::React,
    created_at: Utc::now(),
    steps: vec![],
    events: vec![],
    step_index: 1,
    iteration: 0,
    tool_calls: 0,
    verification_attempts: 0,
    schema_correction_attempts: 0,
    last_tool_call: None,
    recent_tool_calls: std::collections::VecDeque::new(),
    cumulative_cost_usd: 0.0,
    cumulative_tokens: 0,
    system_prompt: String::new(),
    user_input: "hello".into(),
    trace_context: None,
    plan_steps: serde_json::Value::Null,
    plan_position: 0,
    observations: vec![],
    pending_question,
  }
}

#[tokio::test]
async fn resume_from_loop_checkpoint_rejects_answer_without_pending_question() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-ask-user-validate-{}", uuid::Uuid::new_v4());
  init_mock_model(&model).await;
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let err = agent
    .resume_from_loop_checkpoint(
      AgentContext::new("s", "", &model),
      bare_react_checkpoint(None),
      Some("unsolicited answer".to_string()),
    )
    .await
    .unwrap_err();
  assert!(matches!(err, ReActError::InvalidCheckpoint { .. }));
}

#[tokio::test]
async fn resume_from_loop_checkpoint_rejects_missing_answer_when_pending_question_set() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-ask-user-validate2-{}", uuid::Uuid::new_v4());
  init_mock_model(&model).await;
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let err = agent
    .resume_from_loop_checkpoint(
      AgentContext::new("s", "", &model),
      bare_react_checkpoint(Some("what now?".to_string())),
      None,
    )
    .await
    .unwrap_err();
  assert!(matches!(err, ReActError::InvalidCheckpoint { .. }));
}

#[tokio::test]
async fn run_with_context_retries_after_verification_rejection_then_approves() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-verify-retry-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"draft","answer":"answer v1"}"#,
        r#"{"thought":"revised","answer":"answer v2"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  struct RejectOnce;

  #[async_trait]
  impl VerificationStrategy for RejectOnce {
    fn name(&self) -> &'static str {
      "reject-once"
    }

    async fn verify(
      &self,
      context: &VerificationContext,
    ) -> Result<VerificationOutcome, crate::verification::VerificationError> {
      if context.attempt == 1 {
        Ok(VerificationOutcome::Rejected {
          feedback: "cite your sources".to_string(),
        })
      } else {
        Ok(VerificationOutcome::Approved)
      }
    }
  }

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_verification_strategy(Arc::new(RejectOnce));

  let result = agent
    .run_with_context(AgentContext::new("session-verify-1", "research X", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("answer v2"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);

  let verify_steps: Vec<(bool, usize)> = result
    .steps
    .iter()
    .filter_map(|step| match &step.kind {
      AgentStepKind::Verify {
        approved, attempt, ..
      } => Some((*approved, *attempt)),
      _ => None,
    })
    .collect();
  assert_eq!(verify_steps, vec![(false, 1), (true, 2)]);

  assert_eq!(
    result
      .steps
      .iter()
      .filter(|step| matches!(step.kind, AgentStepKind::FinalAnswer { .. }))
      .count(),
    2
  );
  assert_eq!(
    result
      .events
      .iter()
      .filter(|event| matches!(event, AgentEvent::VerificationCompleted { .. }))
      .count(),
    2
  );
}

/// L4.4 end-to-end: a `rag_search` result is cited by the final answer,
/// but the answer's claim (garbage collection) has nothing to do with
/// the cited passage (Rust ownership) — the citation checker must catch
/// this and downgrade the answer to a citation-free version, recorded
/// as a failed `Verify` step.
#[tokio::test]
async fn run_with_context_downgrades_answer_with_unsupported_citation() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-citation-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          r#"{"thought":"search","action":{"tool":"rag_search","params":{"query":"rust ownership"}}}"#,
          r#"{"thought":"done","answer":"Python uses garbage collection [1]."}"#,
        ])
        .unwrap(),
      );
  }
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  struct FakeRagSearchTool;

  #[async_trait]
  impl Tool for FakeRagSearchTool {
    fn name(&self) -> &str {
      "rag_search"
    }
    fn description(&self) -> &str {
      "search"
    }
    fn parameters_schema(&self) -> Value {
      json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
      })
    }
    async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
      Ok(ToolOutput::success(
        "[1] (source: docs/rust.md, score: 0.900)\nRust ownership moves values on assignment."
          .to_string(),
      ))
    }
  }

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(FakeRagSearchTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_citation_checker(Arc::new(
    crate::citation::KeywordOverlapCitationChecker::default(),
  ));

  let result = agent
    .run_with_context(AgentContext::new(
      "session-citation",
      "explain memory management",
      &model,
    ))
    .await
    .unwrap();

  let answer = result.answer.as_deref().expect("final answer present");
  assert!(
    !answer.contains('['),
    "citation marker must be stripped after downgrade: {answer}"
  );
  assert!(answer.contains("garbage collection"));
  assert!(
    result.steps.iter().any(|step| matches!(
      &step.kind,
      AgentStepKind::Verify {
        approved: false,
        ..
      }
    )),
    "downgrade must be recorded as a failed Verify step"
  );
  assert!(
    result.events.iter().any(|event| matches!(
      event,
      AgentEvent::VerificationCompleted {
        approved: false,
        ..
      }
    )),
    "downgrade must emit a VerificationCompleted(approved=false) event"
  );
}

/// A supported citation must NOT be stripped or trigger a downgrade —
/// only unsupported citations change the answer.
#[tokio::test]
async fn run_with_context_keeps_answer_unchanged_when_citation_is_supported() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-citation-ok-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          r#"{"thought":"search","action":{"tool":"rag_search","params":{"query":"rust ownership"}}}"#,
          r#"{"thought":"done","answer":"Rust ownership moves values on assignment [1]."}"#,
        ])
        .unwrap(),
      );
  }
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  struct FakeRagSearchTool;

  #[async_trait]
  impl Tool for FakeRagSearchTool {
    fn name(&self) -> &str {
      "rag_search"
    }
    fn description(&self) -> &str {
      "search"
    }
    fn parameters_schema(&self) -> Value {
      json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
      })
    }
    async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
      Ok(ToolOutput::success(
        "[1] (source: docs/rust.md, score: 0.900)\nRust ownership moves values on assignment."
          .to_string(),
      ))
    }
  }

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(FakeRagSearchTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_citation_checker(Arc::new(
    crate::citation::KeywordOverlapCitationChecker::default(),
  ));

  let result = agent
    .run_with_context(AgentContext::new(
      "session-citation-ok",
      "explain ownership",
      &model,
    ))
    .await
    .unwrap();

  assert_eq!(
    result.answer.as_deref(),
    Some("Rust ownership moves values on assignment [1].")
  );
  assert!(
    !result.steps.iter().any(|step| matches!(
      &step.kind,
      AgentStepKind::Verify {
        approved: false,
        ..
      }
    )),
    "a supported citation must not trigger a downgrade Verify step"
  );
}

#[tokio::test]
async fn run_with_context_force_accepts_after_exhausting_verification_attempts() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-verify-exhaust-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"draft","answer":"answer v1"}"#,
        r#"{"thought":"still not great","answer":"answer v2"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  struct AlwaysReject;

  #[async_trait]
  impl VerificationStrategy for AlwaysReject {
    fn name(&self) -> &'static str {
      "always-reject"
    }

    async fn verify(
      &self,
      _context: &VerificationContext,
    ) -> Result<VerificationOutcome, crate::verification::VerificationError> {
      Ok(VerificationOutcome::Rejected {
        feedback: "never good enough".to_string(),
      })
    }
  }

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(4)
      .with_max_verification_attempts(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_verification_strategy(Arc::new(AlwaysReject));

  let result = agent
    .run_with_context(AgentContext::new("session-verify-2", "research X", &model))
    .await
    .unwrap();

  // Exhausting max_verification_attempts force-accepts rather than
  // erroring — the run degrades gracefully instead of getting stuck.
  assert_eq!(result.answer.as_deref(), Some("answer v2"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert_eq!(
    result
      .steps
      .iter()
      .filter(|step| matches!(step.kind, AgentStepKind::Verify { .. }))
      .count(),
    2
  );
}

#[tokio::test]
async fn run_with_context_skips_verification_when_disabled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-verify-disabled-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![r#"{"thought":"draft","answer":"answer v1"}"#]).unwrap(),
    );
  }
  init_mock_model(&model).await;

  struct AlwaysReject;

  #[async_trait]
  impl VerificationStrategy for AlwaysReject {
    fn name(&self) -> &'static str {
      "always-reject"
    }

    async fn verify(
      &self,
      _context: &VerificationContext,
    ) -> Result<VerificationOutcome, crate::verification::VerificationError> {
      Ok(VerificationOutcome::Rejected {
        feedback: "never good enough".to_string(),
      })
    }
  }

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(4)
      .with_verification_enabled(false),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_verification_strategy(Arc::new(AlwaysReject));

  let result = agent
    .run_with_context(AgentContext::new("session-verify-3", "research X", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("answer v1"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert!(
    !result
      .steps
      .iter()
      .any(|step| matches!(step.kind, AgentStepKind::Verify { .. }))
  );
}

#[tokio::test]
async fn run_with_context_consumes_native_tool_calls_when_available() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-native-tool-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  //
  // Drives the ReAct loop through the native tool-calling path: iteration
  // 0 emits a tool call (via YANSHI_MOCK_TOOL_CALLS), iteration 1 emits
  // an empty batch and a JSON answer (via YANSHI_MOCK_RESPONSES). The
  // first response would be malformed JSON for the prompt parser, so a
  // successful tool call here proves the native path was actually taken.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![serde_json::json!({
          "id": "call_0",
          "name": "echo",
          "arguments": {"text": "hi"}
        })],
        Vec::<serde_json::Value>::new(),
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        // Iteration 0 content is irrelevant; tool_calls drive the loop.
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"final: echo: hi"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_reflection_strategy(Arc::new(crate::reflection::FinalReflection));

  let result = agent
    .run_with_context(AgentContext::new("session-native-tool", "say hi", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("final: echo: hi"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  let tool_call_count = result
    .steps
    .iter()
    .filter(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
    .count();
  assert_eq!(tool_call_count, 1, "expected exactly one ToolCall step");

  // SAFETY: cleanup of the dedicated mock env vars after the test read.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

/// W0.5 regression: a tool call denied with `ToolError::PolicyDeniedAndStop`
/// (what the harness hook layer emits for `ApprovalOutcome::DenyAndStop`
/// and for every subsequent call once that gate has tripped) must stop
/// the run immediately with `AgentStopReason::ApprovalDenied` — not get
/// folded into an `[ERROR] ...` observation the LLM sees and keeps
/// looping past. Only one `YANSHI_MOCK_TOOL_CALLS`/`YANSHI_MOCK_RESPONSES`
/// entry is queued; if the loop wrongly continued to a second turn, the
/// mock provider would have nothing left to return and the test would
/// fail on that empty-queue panic instead of the stop_reason assertion —
/// proof the fix stops after exactly one turn, not eventually via
/// `MaxSteps`/`MaxToolCalls`.
#[tokio::test]
async fn policy_denied_and_stop_ends_the_run_immediately() {
  struct DenyAndStopTool;

  #[async_trait]
  impl Tool for DenyAndStopTool {
    fn name(&self) -> &str {
      "guarded"
    }
    fn description(&self) -> &str {
      "A tool the approval layer refuses with stop semantics"
    }
    fn parameters_schema(&self) -> Value {
      json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _params: Value) -> Result<yanshi_tool::ToolOutput, ToolError> {
      Err(ToolError::PolicyDeniedAndStop {
        message: "previous approval requested deny-and-stop; aborting further tool calls"
          .to_string(),
      })
    }
  }

  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-deny-and-stop-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![serde_json::json!({
        "id": "call_0",
        "name": "guarded",
        "arguments": {}
      })]])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused — native tool call)"]).unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(DenyAndStopTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new(
      "session-deny-and-stop",
      "do the guarded thing",
      &model,
    ))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::ApprovalDenied {
      message: "previous approval requested deny-and-stop; aborting further tool calls".to_string(),
    }
  );
  assert!(result.answer.is_none());

  // SAFETY: cleanup of the dedicated mock env vars after the test read.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

// ── V2.1: output_schema ──────────────────────────────────────────────

fn answer_schema() -> Value {
  json!({
    "type": "object",
    "properties": {"answer": {"type": "string"}},
    "required": ["answer"]
  })
}

/// `collect_tool_specs` must offer the synthetic `final_answer` tool
/// alongside real tools whenever `output_schema` is configured, and must
/// not offer it at all otherwise.
#[test]
fn collect_tool_specs_includes_final_answer_tool_only_when_output_schema_set() {
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));

  let without_schema = ReActAgent::new(
    ReActConfig::new("gpt-4o"),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  assert!(
    !without_schema
      .collect_tool_specs()
      .iter()
      .any(|spec| spec.name == FINAL_ANSWER_TOOL_NAME)
  );

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let with_schema = ReActAgent::new(
    ReActConfig::new("gpt-4o").with_output_schema(answer_schema()),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let specs = with_schema.collect_tool_specs();
  let final_answer_spec = specs
    .iter()
    .find(|spec| spec.name == FINAL_ANSWER_TOOL_NAME)
    .expect("final_answer tool must be offered when output_schema is set");
  assert_eq!(final_answer_spec.parameters, answer_schema());
  // The real tool must still be offered alongside it.
  assert!(specs.iter().any(|spec| spec.name == "echo"));
}

/// The exact V2.1 test bar: a schema mismatch on the first candidate
/// answer triggers a correction turn (the loop continues, feeding the
/// validation errors back), and the run eventually produces a compliant
/// output once the model self-corrects.
#[tokio::test]
async fn output_schema_mismatch_triggers_correction_turn_and_eventually_succeeds() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-schema-correct-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        // Iteration 0: violates the schema (`answer` is missing).
        vec![serde_json::json!({
          "id": "call_0",
          "name": FINAL_ANSWER_TOOL_NAME,
          "arguments": {"wrong_field": "oops"}
        })],
        // Iteration 1: conforms.
        vec![serde_json::json!({
          "id": "call_1",
          "name": FINAL_ANSWER_TOOL_NAME,
          "arguments": {"answer": "42"}
        })],
      ])
      .unwrap(),
    );
  }
  let _tool_calls_guard = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  init_mock_model(&model).await;

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(4)
      .with_output_schema(answer_schema()),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  let result = agent
    .run_with_context(AgentContext::new(
      "session-schema-correct",
      "answer please",
      &model,
    ))
    .await
    .unwrap();

  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert_eq!(result.answer.as_deref(), Some(r#"{"answer":"42"}"#));
  let verify_steps: Vec<_> = result
    .steps
    .iter()
    .filter_map(|step| match &step.kind {
      AgentStepKind::Verify { approved, .. } => Some(*approved),
      _ => None,
    })
    .collect();
  assert_eq!(
    verify_steps,
    vec![false, true],
    "expected one rejected schema check then one approved, got: {verify_steps:?}"
  );
}

/// When the model never produces a conforming answer within the
/// correction budget, the run must hard-error rather than silently
/// returning a non-conformant answer labelled as final (unlike
/// `VerificationStrategy`'s force-accept-on-exhaustion).
#[tokio::test]
async fn output_schema_exhausted_attempts_returns_hard_error() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-schema-exhaust-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![serde_json::json!({"id": "c0", "name": FINAL_ANSWER_TOOL_NAME, "arguments": {}})],
        vec![serde_json::json!({"id": "c1", "name": FINAL_ANSWER_TOOL_NAME, "arguments": {}})],
        vec![serde_json::json!({"id": "c2", "name": FINAL_ANSWER_TOOL_NAME, "arguments": {}})],
      ])
      .unwrap(),
    );
  }
  let _tool_calls_guard = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  init_mock_model(&model).await;

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(6)
      .with_output_schema(answer_schema())
      .with_max_schema_correction_attempts(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  let err = agent
    .run_with_context(AgentContext::new(
      "session-schema-exhaust",
      "answer please",
      &model,
    ))
    .await
    .expect_err("schema-exhausted run must hard-error");
  match err {
    ReActError::SchemaValidationFailed { attempts, .. } => assert_eq!(attempts, 3),
    other => panic!("expected SchemaValidationFailed, got {other:?}"),
  }
}

/// Phase 2b: the between-turn hook fires once at the top of every turn,
/// before that turn's LLM call, with the 0-based turn index — the
/// control point a loop owner uses for between-turn context engineering.
#[tokio::test]
async fn between_turn_hook_fires_before_each_turn() {
  use crate::runtime::BetweenTurnHook;

  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-turn-hook-{}", uuid::Uuid::new_v4());
  // Two turns: iteration 0 emits a tool call; iteration 1 the answer.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![serde_json::json!({"id":"call_0","name":"echo","arguments":{"text":"hi"}})],
        Vec::<serde_json::Value>::new(),
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"final"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  struct CountingHook {
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
  }
  #[async_trait]
  impl BetweenTurnHook for CountingHook {
    async fn before_turn(&self, turn_index: usize, _session_id: &str, _memory: &dyn MemoryStore) {
      self.seen.lock().unwrap().push(turn_index);
    }
  }

  let seen = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
  let hook = Arc::new(CountingHook { seen: seen.clone() });

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_reflection_strategy(Arc::new(crate::reflection::FinalReflection));

  let result = agent
    .run_with_context(
      AgentContext::new("turn-hook-session", "say hi", &model).with_between_turn_hook(hook),
    )
    .await
    .unwrap();
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }

  assert_eq!(
    *seen.lock().unwrap(),
    vec![0, 1],
    "hook must fire before each turn with the 0-based turn index"
  );
}

/// RFC §6 step 6: the turn-driven session pumps one turn at a time —
/// `Continued` while the agent works, `Finished(result)` at the end —
/// exposes memory between turns, and rejects use after completion.
#[tokio::test]
async fn turn_driven_session_advances_then_finishes() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-turn-driven-{}", uuid::Uuid::new_v4());
  // Turn 0: a tool call; turn 1: the final answer.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![serde_json::json!({"id":"call_0","name":"echo","arguments":{"text":"hi"}})],
        Vec::<serde_json::Value>::new(),
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"final: ok"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(EchoTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_reflection_strategy(Arc::new(crate::reflection::FinalReflection));

  let session = agent
    .begin_turn_driven(AgentContext::new("turn-driven-session", "say hi", &model))
    .await
    .unwrap();
  assert_eq!(session.turn_index(), 0);

  // Turn 0: tool call → Continued; the consuming `next_turn` hands back a
  // fresh live session, and memory is observable between turns.
  let session = match session.next_turn().await.unwrap() {
    ReActTurn::Continued(active) => *active,
    ReActTurn::Finished { .. } => panic!("expected Continued on the first turn"),
  };
  assert_eq!(session.turn_index(), 1);
  let history = session
    .memory()
    .get_all("turn-driven-session")
    .await
    .unwrap();
  assert!(!history.is_empty(), "memory accessible mid-run");

  // Turn 1: final answer → Finished. `next_turn` consumes the session and
  // returns the result + the agent borrow — there is no session value left.
  let result = match session.next_turn().await.unwrap() {
    ReActTurn::Finished { result, .. } => result,
    ReActTurn::Continued(_) => panic!("expected Finished on the second turn"),
  };
  assert_eq!(result.answer.as_deref(), Some("final: ok"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);

  // P-A3.3: "use after finish" is now a *compile* error, not a runtime
  // `SessionFinished`. The line below does not compile — `session` was moved
  // into the consuming `next_turn` above, so there is nothing left to drive:
  //
  //   let _ = session.next_turn().await; // error[E0382]: use of moved value
  //
  // The object-safe `LoopSession` path keeps a runtime guard
  // (`ReActTurnDriver`); that is covered by the harness runtime tests.

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[tokio::test]
async fn batch_path_runs_multiple_idempotent_tool_calls_in_order() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-batch-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  //
  // Iteration 0 emits three native tool calls in one turn; iteration 1
  // emits an empty batch and the final answer.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![
          serde_json::json!({"id": "call_a", "name": "counting_echo", "arguments": {"text": "a"}}),
          serde_json::json!({"id": "call_b", "name": "counting_echo", "arguments": {"text": "b"}}),
          serde_json::json!({"id": "call_c", "name": "counting_echo", "arguments": {"text": "c"}}),
        ],
        Vec::<serde_json::Value>::new(),
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        "(unused — native batch)",
        r#"{"thought":"done","answer":"batch complete"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(AgentContext::new("session-batch", "go", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("batch complete"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  // All three idempotent calls executed.
  assert_eq!(
    calls.load(Ordering::SeqCst),
    3,
    "all three calls in the batch should run"
  );

  // ToolCallStarted events must appear in LLM-returned (a, b, c) order.
  let started: Vec<String> = result
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ToolCallStarted { params, .. } => params["text"].as_str().map(|s| s.to_string()),
      _ => None,
    })
    .collect();
  assert_eq!(
    started,
    vec!["a".to_string(), "b".to_string(), "c".to_string()]
  );

  // ToolCall steps must also be in LLM order.
  let step_order: Vec<String> = result
    .steps
    .iter()
    .filter_map(|step| match &step.kind {
      AgentStepKind::ToolCall { params, .. } => params["text"].as_str().map(|s| s.to_string()),
      _ => None,
    })
    .collect();
  assert_eq!(
    step_order,
    vec!["a".to_string(), "b".to_string(), "c".to_string()]
  );

  // ToolCallCompleted matches LLM order via step_index.
  let started_indexes: Vec<usize> = result
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ToolCallStarted { step_index, .. } => Some(*step_index),
      _ => None,
    })
    .collect();
  let completed_indexes: Vec<usize> = result
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ToolCallCompleted { step_index, .. } => Some(*step_index),
      _ => None,
    })
    .collect();
  assert_eq!(started_indexes, completed_indexes);

  // SAFETY: cleanup the dedicated mock env vars after the test read.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[tokio::test]
async fn batch_path_continues_when_one_tool_fails() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-batch-partial-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![
        vec![
          serde_json::json!({"id": "ok1", "name": "counting_echo", "arguments": {"text": "ok"}}),
          serde_json::json!({"id": "boom", "name": "exploding", "arguments": {}}),
          serde_json::json!({"id": "ok2", "name": "counting_echo", "arguments": {"text": "ok2"}}),
        ],
        Vec::<serde_json::Value>::new(),
      ])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        "(unused)",
        r#"{"thought":"done","answer":"partial done"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  struct Exploding;
  #[async_trait]
  impl Tool for Exploding {
    fn name(&self) -> &str {
      "exploding"
    }
    fn description(&self) -> &str {
      "always errors"
    }
    fn parameters_schema(&self) -> Value {
      json!({"type": "object"})
    }
    fn idempotency(&self, _params: &Value) -> ToolIdempotency {
      ToolIdempotency::Idempotent
    }
    async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
      Err(ToolError::ExecutionFailed {
        message: "exploded".into(),
      })
    }
  }

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  registry.register(Arc::new(Exploding));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new("session-partial", "go", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("partial done"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  // Both ok calls still ran despite the middle one erroring.
  assert_eq!(calls.load(Ordering::SeqCst), 2);

  // Verify one ToolCallCompleted is_error=true and two are false.
  let (errors, successes) = result.events.iter().fold((0, 0), |(e, s), event| {
    if let AgentEvent::ToolCallCompleted { is_error, .. } = event {
      if *is_error { (e + 1, s) } else { (e, s + 1) }
    } else {
      (e, s)
    }
  });
  assert_eq!(errors, 1);
  assert_eq!(successes, 2);

  // Step trace has three ToolResult entries.
  let result_steps = result
    .steps
    .iter()
    .filter(|step| matches!(step.kind, AgentStepKind::ToolResult { .. }))
    .count();
  assert_eq!(result_steps, 3);

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[tokio::test]
async fn batch_path_returns_cancelled_when_token_already_signalled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-batch-cancel-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![
        serde_json::json!({"id": "c1", "name": "counting_echo", "arguments": {"text": "a"}}),
        serde_json::json!({"id": "c2", "name": "counting_echo", "arguments": {"text": "b"}}),
      ]])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  init_mock_model(&model).await;

  let token = AgentCancellationToken::new();
  token.cancel(); // pre-cancelled

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(
      AgentContext::new("session-cancel", "go", &model).with_cancellation_token(token),
    )
    .await
    .unwrap();
  assert!(
    matches!(result.stop_reason, AgentStopReason::Cancelled { .. }),
    "expected Cancelled, got {:?}",
    result.stop_reason
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[tokio::test]
async fn batch_path_blocks_when_max_tool_calls_would_be_exceeded() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-batch-max-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![
        serde_json::json!({"id": "c1", "name": "counting_echo", "arguments": {"text": "a"}}),
        serde_json::json!({"id": "c2", "name": "counting_echo", "arguments": {"text": "b"}}),
        serde_json::json!({"id": "c3", "name": "counting_echo", "arguments": {"text": "c"}}),
      ]])
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
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let limits = RuntimeLimits {
    max_tool_calls: Some(2),
    ..Default::default()
  };
  let result = agent
    .run_with_context(AgentContext::new("session-max", "go", &model).with_limits(limits))
    .await
    .unwrap();
  assert!(
    matches!(
      result.stop_reason,
      AgentStopReason::MaxToolCalls { max_tool_calls: 2 }
    ),
    "expected MaxToolCalls, got {:?}",
    result.stop_reason
  );
  assert_eq!(
    calls.load(Ordering::SeqCst),
    0,
    "batch must reject atomically; no inner tool runs"
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

// ── T1.1: production cost-limit enforcement ───────────────────────────

/// $1.00 per call regardless of response content: the mock provider
/// always reports `prompt_tokens: 50`, so pricing entirely off
/// `input_per_1k` (with `output_per_1k: 0.0`) makes each call's cost a
/// fixed, deterministic amount independent of response word count.
fn flat_dollar_per_call_pricing() -> crate::eval::PricingTable {
  crate::eval::PricingTable::default().with_default(crate::eval::ModelPricing {
    input_per_1k: 20.0,
    output_per_1k: 0.0,
  })
}

#[tokio::test]
async fn cost_limit_stops_run_before_next_llm_call_once_exceeded() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-cost-limit-{}", uuid::Uuid::new_v4());
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
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(10)
      .without_loop_detection()
      .with_pricing_table(flat_dollar_per_call_pricing()),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let limits = RuntimeLimits {
    cost_limit_usd: Some(1.5),
    ..Default::default()
  };
  let result = agent
    .run_with_context(AgentContext::new("session-cost", "go", &model).with_limits(limits))
    .await
    .unwrap();

  match result.stop_reason {
    AgentStopReason::CostLimitExceeded {
      used_usd,
      budget_usd,
    } => {
      assert_eq!(budget_usd, 1.5);
      // Two calls at $1.00 each = $2.00: over budget, but not wildly so
      // (the guard reacts at the next turn boundary, not mid-call).
      assert!(
        (used_usd - 2.0).abs() < 1e-9,
        "expected used_usd == 2.0 (2 calls x $1.00), got {used_usd}"
      );
    }
    other => panic!("expected CostLimitExceeded, got {other:?}"),
  }
  // Exactly 2 tool calls ran before the 3rd turn's top-of-loop check
  // stopped the run — the 3rd queued tool-call batch is never reached.
  assert_eq!(calls.load(Ordering::SeqCst), 2);

  unsafe {
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[tokio::test]
async fn cost_limit_does_not_interrupt_a_run_that_stays_within_budget() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-cost-ok-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![r#"{"thought":"done","answer":"all good"}"#]).unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(10)
      .with_pricing_table(flat_dollar_per_call_pricing()),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let limits = RuntimeLimits {
    // A single $1.00 call comfortably fits a $100 budget.
    cost_limit_usd: Some(100.0),
    ..Default::default()
  };
  let result = agent
    .run_with_context(AgentContext::new("session-cost-ok", "go", &model).with_limits(limits))
    .await
    .unwrap();

  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert_eq!(result.answer.as_deref(), Some("all good"));

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

#[test]
fn native_tool_call_to_agent_response_preserves_name_and_args() {
  let call = ToolCallRequest {
    id: "call_0".into(),
    name: "echo".into(),
    arguments: serde_json::json!({"text": "hi"}),
  };
  match native_tool_call_to_agent_response(&call) {
    AgentResponse::Action {
      thought,
      tool,
      params,
    } => {
      assert!(thought.is_empty());
      assert_eq!(tool, "echo");
      assert_eq!(params["text"], "hi");
    }
    other => panic!("expected Action, got {:?}", other),
  }
}

#[test]
fn tool_params_annotation_maps_idempotency_to_resume_metadata() {
  let params = annotate_tool_params_for_resume(
    json!({"url": "https://example.test"}),
    Some(ToolIdempotency::Idempotent),
  );

  assert_eq!(params["_yanshi"]["side_effect_class"], json!("idempotent"));
}

#[tokio::test]
async fn resume_with_context_reuses_recorded_tool_result_without_replay() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-resume-runtime-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      r#"{"thought":"use recovered observation","answer":"final: echo: hi"}"#,
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let prior = AgentRunResult {
    session_id: "resume-session".to_string(),
    answer: None,
    stop_reason: AgentStopReason::Cancelled {
      message: "shutdown".to_string(),
    },
    steps: vec![
      AgentStep::new(
        0,
        AgentStepKind::Observe {
          input: "say hi".to_string(),
        },
      ),
      AgentStep::new(
        1,
        AgentStepKind::ToolCall {
          tool: "counting_echo".to_string(),
          params: json!({"text": "hi"}),
        },
      ),
      AgentStep::new(
        2,
        AgentStepKind::ToolResult {
          tool: "counting_echo".to_string(),
          content: "echo: hi".to_string(),
          is_error: false,
          parts: vec![],
        },
      ),
    ],
    events: vec![],
  };

  let result = agent
    .resume_with_context(
      AgentContext::new("resume-session", "finish the task", &model),
      prior,
    )
    .await
    .unwrap();

  assert_eq!(calls.load(Ordering::SeqCst), 0);
  assert_eq!(result.answer.as_deref(), Some("final: echo: hi"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  assert!(result.steps.len() > 3);
}

#[tokio::test]
async fn resume_with_context_replays_unresolved_idempotent_tool_call() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-resume-replay-{}", uuid::Uuid::new_v4());
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      r#"{"thought":"use recovered replay","answer":"final: echo: hi"}"#,
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let prior = AgentRunResult {
    session_id: "resume-replay-session".to_string(),
    answer: None,
    stop_reason: AgentStopReason::Cancelled {
      message: "shutdown".to_string(),
    },
    steps: vec![AgentStep::new(
      1,
      AgentStepKind::ToolCall {
        tool: "counting_echo".to_string(),
        params: json!({
          "text": "hi",
          "_yanshi": {
            "side_effect_class": "idempotent"
          }
        }),
      },
    )],
    events: vec![],
  };

  let result = agent
    .resume_with_context(
      AgentContext::new("resume-replay-session", "finish", &model),
      prior,
    )
    .await
    .unwrap();

  assert_eq!(calls.load(Ordering::SeqCst), 1);
  assert!(result.steps.iter().any(|step| {
    matches!(
      &step.kind,
      AgentStepKind::ToolResult { tool, .. } if tool == "counting_echo"
    )
  }));
  assert_eq!(result.answer.as_deref(), Some("final: echo: hi"));
}

#[tokio::test]
async fn record_reflection_can_be_disabled_even_with_strategy() {
  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_max_iterations(4)
      .with_reflection_enabled(false),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("session-no-reflection")
  .with_reflection_strategy(Arc::new(crate::reflection::FinalReflection));
  let mut step_index = 1;
  let mut steps = vec![];
  let mut events = vec![];

  agent
    .record_reflection(
      crate::reflection::ReflectionContext::final_answer(
        "session-no-reflection",
        step_index,
        "done",
      ),
      &mut step_index,
      &mut steps,
      &mut events,
    )
    .await
    .unwrap();

  assert_eq!(step_index, 1);
  assert!(steps.is_empty());
  assert!(events.is_empty());
}

#[tokio::test]
async fn query_memory_uses_backing_memory_search_for_current_session() {
  let memory = SessionMemory::default_window();
  memory
    .add_message(Message::user(
      "memory-session",
      "Remember that semantic search belongs to runtime memory.",
    ))
    .await
    .unwrap();
  memory
    .add_message(Message::assistant("other-session", "semantic but isolated"))
    .await
    .unwrap();

  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(memory),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("memory-session");

  let hits = agent.query_memory("semantic search", 5).await.unwrap();

  assert_eq!(hits.len(), 1);
  assert_eq!(hits[0].session_id, "memory-session");
  assert!(hits[0].content.contains("semantic search"));
}

#[tokio::test]
async fn memory_hook_observes_loop_reads_searches_and_writes() {
  let hook = Arc::new(RecordingMemoryHook::default());
  let mut agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("hook-session")
  .with_memory_hook(hook.clone());

  agent
    .add_memory_message(Message::user("hook-session", "semantic hook memory"))
    .await
    .unwrap();
  let _messages = agent
    .build_llm_messages(&agent.build_system_prompt())
    .await
    .unwrap();
  let hits = agent.query_memory("semantic", 3).await.unwrap();

  assert_eq!(hits.len(), 1);
  let events = hook.events.lock().unwrap();
  assert!(
    events
      .iter()
      .any(|event| event.kind == MemoryHookKind::Write && event.messages.len() == 1)
  );
  assert!(
    events
      .iter()
      .any(|event| event.kind == MemoryHookKind::ReadHistory && event.messages.len() == 1)
  );
  assert!(events.iter().any(|event| {
    event.kind == MemoryHookKind::Search
      && event.query.as_deref() == Some("semantic")
      && event.limit == Some(3)
      && event.messages.len() == 1
  }));
}

#[tokio::test]
async fn run_with_context_returns_cancelled_when_token_already_signalled() {
  let token = AgentCancellationToken::new();
  token.cancel();
  let mut agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  let result = agent
    .run_with_context(
      AgentContext::new("cancel-session", "do work", "mock-runtime").with_cancellation_token(token),
    )
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::Cancelled {
      message: "cancellation token signalled".to_string(),
    }
  );
  assert!(result.answer.is_none());
  assert_eq!(result.steps.len(), 1);
  assert!(matches!(
    result.events.last(),
    Some(AgentEvent::RunStopped {
      reason: AgentStopReason::Cancelled { .. },
      ..
    })
  ));
}

// ── P-A3.1: characterize the timeout / cancellation *racing* paths ─────────
//
// The existing tests above cover *pre-signalled* cancellation and the batch
// max-tool-calls gate. These pin the behaviour when a deadline or a
// cancellation wins a race against an *in-flight* call — the four `select!`
// arms in `run_turn_llm_call` / `run_turn_tool_call` that P-A3.2 consolidates
// into `async-util::race_with_limits`. They are deterministic because the
// racing operation never completes within the test window (a 10 s sleep
// dwarfs the ~50 ms deadline), so the outcome does not depend on scheduler
// timing.

/// Removes a process-global env var on drop so a panicking test can't leak it
/// into the next test in the (lock-serialized) suite — `YANSHI_MOCK_DELAY_MS`
/// in particular would otherwise make unrelated LLM tests hang.
struct EnvVarGuard(&'static str);
impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    // SAFETY: the LLM_TEST_LOCK guard serializes these process-wide mutations.
    unsafe {
      std::env::remove_var(self.0);
    }
  }
}

/// A tool that signals it has started, then blocks far longer than any test
/// deadline, so a racing timeout or cancellation deterministically wins. Its
/// idempotency selects the batch path: `Idempotent` calls run concurrently
/// (the `join_all` matrix), `NonIdempotent` calls run serially.
struct SleepingTool {
  started: Arc<std::sync::atomic::AtomicBool>,
  idempotent: bool,
}

impl SleepingTool {
  /// Idempotent — drives the concurrent batch path.
  fn concurrent(started: Arc<std::sync::atomic::AtomicBool>) -> Self {
    Self {
      started,
      idempotent: true,
    }
  }
  /// NonIdempotent — drives the serial batch path.
  fn serial(started: Arc<std::sync::atomic::AtomicBool>) -> Self {
    Self {
      started,
      idempotent: false,
    }
  }
}

#[async_trait]
impl Tool for SleepingTool {
  fn name(&self) -> &str {
    "sleeper"
  }
  fn description(&self) -> &str {
    "signals start, then blocks so a racing timeout/cancellation wins"
  }
  fn parameters_schema(&self) -> Value {
    json!({ "type": "object" })
  }
  fn idempotency(&self, _params: &Value) -> ToolIdempotency {
    if self.idempotent {
      ToolIdempotency::Idempotent
    } else {
      ToolIdempotency::NonIdempotent
    }
  }
  async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
    self.started.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    Ok(ToolOutput::success("done"))
  }
}

#[tokio::test]
async fn llm_call_times_out_mid_flight_stops_with_timeout() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-llm-timeout-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_DELAY_MS", "10000");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![r#"{"answer":"done"}"#]).unwrap(),
    );
  }
  let _delay = EnvVarGuard("YANSHI_MOCK_DELAY_MS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let mut limits = RuntimeLimits::react_defaults();
  limits.timeout_ms = Some(50);
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let result = agent
    .run_with_context(AgentContext::new("llm-timeout", "go", &model).with_limits(limits))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::Timeout { timeout_ms: 50 },
    "the deadline should win against the 10s-delayed LLM call"
  );
}

#[tokio::test]
async fn llm_call_cancelled_mid_flight_stops_with_cancelled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-llm-cancel-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_DELAY_MS", "10000");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![r#"{"answer":"done"}"#]).unwrap(),
    );
  }
  let _delay = EnvVarGuard("YANSHI_MOCK_DELAY_MS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let token = AgentCancellationToken::new();
  // The LLM call blocks 10s, so cancelling shortly after the run starts
  // deterministically lands while the call is in flight.
  let cancel_token = token.clone();
  tokio::spawn(async move {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel_token.cancel();
  });

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let result = agent
    .run_with_context(AgentContext::new("llm-cancel", "go", &model).with_cancellation_token(token))
    .await
    .unwrap();

  assert!(
    matches!(result.stop_reason, AgentStopReason::Cancelled { .. }),
    "cancellation should win against the in-flight LLM call; got {:?}",
    result.stop_reason
  );
}

#[tokio::test]
async fn tool_call_times_out_mid_flight_stops_with_timeout() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-tool-timeout-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![
        json!({"id":"t1","name":"sleeper","arguments":{}}),
      ]])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::concurrent(started.clone())));

  // Generous enough for the instant LLM call to return its tool call; the 10s
  // tool then overruns the wall-clock budget.
  let mut limits = RuntimeLimits::react_defaults();
  limits.timeout_ms = Some(300);
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(AgentContext::new("tool-timeout", "go", &model).with_limits(limits))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::Timeout { timeout_ms: 300 }
  );
  assert!(
    started.load(Ordering::SeqCst),
    "the tool must have entered execution — confirms the tool-call timeout arm"
  );
}

#[tokio::test]
async fn tool_call_cancelled_mid_flight_stops_with_cancelled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-tool-cancel-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      serde_json::to_string(&vec![vec![
        json!({"id":"t1","name":"sleeper","arguments":{}}),
      ]])
      .unwrap(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::concurrent(started.clone())));

  let token = AgentCancellationToken::new();
  // Cancel only once the tool is actually in flight, so the *tool-call*
  // cancellation arm fires (not the LLM one).
  let cancel_token = token.clone();
  let started_probe = started.clone();
  tokio::spawn(async move {
    while !started_probe.load(Ordering::SeqCst) {
      tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    cancel_token.cancel();
  });

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(AgentContext::new("tool-cancel", "go", &model).with_cancellation_token(token))
    .await
    .unwrap();

  assert!(
    matches!(result.stop_reason, AgentStopReason::Cancelled { .. }),
    "cancellation should win against the in-flight tool call; got {:?}",
    result.stop_reason
  );
  assert!(started.load(Ordering::SeqCst));
}

// ── P-A3.2b: characterize the *batch-dispatch* racing paths ────────────────
//
// When the LLM returns >= 2 tool calls, the batch dispatcher races the
// concurrent (`join_all`) group and each serial call against the same
// timeout / cancellation limits, via two more `select!` matrices. These pin
// those arms so the matrices can be repointed onto `race_with_limits` without
// behaviour drift. Idempotency selects the path: `concurrent` → `join_all`,
// `serial` → the per-call loop.

/// Two tool calls in one LLM turn → the batch dispatcher.
fn two_sleeper_calls() -> String {
  serde_json::to_string(&vec![vec![
    json!({"id":"c1","name":"sleeper","arguments":{}}),
    json!({"id":"c2","name":"sleeper","arguments":{}}),
  ]])
  .unwrap()
}

#[tokio::test]
async fn concurrent_batch_times_out_stops_with_timeout() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-cbatch-timeout-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_TOOL_CALLS", two_sleeper_calls());
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::concurrent(started.clone())));

  let mut limits = RuntimeLimits::react_defaults();
  limits.timeout_ms = Some(300);
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(AgentContext::new("cbatch-timeout", "go", &model).with_limits(limits))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::Timeout { timeout_ms: 300 }
  );
  assert!(
    started.load(Ordering::SeqCst),
    "the concurrent batch must have begun executing"
  );
}

#[tokio::test]
async fn concurrent_batch_cancelled_mid_flight_stops_with_cancelled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-cbatch-cancel-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_TOOL_CALLS", two_sleeper_calls());
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::concurrent(started.clone())));

  let token = AgentCancellationToken::new();
  let cancel_token = token.clone();
  let started_probe = started.clone();
  tokio::spawn(async move {
    while !started_probe.load(Ordering::SeqCst) {
      tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    cancel_token.cancel();
  });

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(
      AgentContext::new("cbatch-cancel", "go", &model).with_cancellation_token(token),
    )
    .await
    .unwrap();

  assert!(
    matches!(result.stop_reason, AgentStopReason::Cancelled { .. }),
    "cancellation should win against the in-flight concurrent batch; got {:?}",
    result.stop_reason
  );
  assert!(started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn serial_batch_times_out_stops_with_timeout() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-sbatch-timeout-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_TOOL_CALLS", two_sleeper_calls());
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::serial(started.clone())));

  let mut limits = RuntimeLimits::react_defaults();
  limits.timeout_ms = Some(300);
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(AgentContext::new("sbatch-timeout", "go", &model).with_limits(limits))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::Timeout { timeout_ms: 300 }
  );
  assert!(
    started.load(Ordering::SeqCst),
    "the first serial call must have begun executing"
  );
}

#[tokio::test]
async fn serial_batch_cancelled_mid_flight_stops_with_cancelled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-sbatch-cancel-{}", uuid::Uuid::new_v4());
  // SAFETY: serialized by LLM_TEST_LOCK; both vars are removed by the guards.
  unsafe {
    std::env::set_var("YANSHI_MOCK_TOOL_CALLS", two_sleeper_calls());
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec!["(unused)"]).unwrap(),
    );
  }
  let _calls = EnvVarGuard("YANSHI_MOCK_TOOL_CALLS");
  let _responses = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(SleepingTool::serial(started.clone())));

  let token = AgentCancellationToken::new();
  let cancel_token = token.clone();
  let started_probe = started.clone();
  tokio::spawn(async move {
    while !started_probe.load(Ordering::SeqCst) {
      tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    cancel_token.cancel();
  });

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );
  let result = agent
    .run_with_context(
      AgentContext::new("sbatch-cancel", "go", &model).with_cancellation_token(token),
    )
    .await
    .unwrap();

  assert!(
    matches!(result.stop_reason, AgentStopReason::Cancelled { .. }),
    "cancellation should win against the in-flight serial call; got {:?}",
    result.stop_reason
  );
  assert!(started.load(Ordering::SeqCst));
}

#[test]
fn compact_memory_summary_formats_older_messages() {
  let mut older = Message::user("budget-session", "older context about project goals");
  older.token_count = 10;

  let summary = compact_memory_summary(&[older], 10);

  assert!(summary.contains("1 older messages compacted"));
  assert!(summary.contains("older context about project goals"));
}

/// V0.1 regression: a >160-byte CJK message must not panic
/// `compact_memory_summary`'s per-line truncation.
#[test]
fn compact_memory_summary_truncates_multibyte_utf8_without_panicking() {
  let mut older = Message::user("budget-session", "测试".repeat(100));
  older.token_count = 10;

  let summary = compact_memory_summary(&[older], 10);

  assert!(summary.contains("..."));
}

#[tokio::test]
async fn memory_prompt_budget_compacts_older_messages() {
  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );
  let mut older = Message::user("budget-session", "older context about project goals");
  older.token_count = 10;
  let mut recent = Message::assistant("budget-session", "recent answer");
  recent.token_count = 4;

  let (summary, kept) = agent
    .apply_memory_prompt_budget(vec![older, recent.clone()])
    .await
    .unwrap();

  let summary = summary.unwrap();
  assert!(summary.contains("1 older messages compacted"));
  assert!(summary.contains("older context about project goals"));
  assert_eq!(kept.len(), 1);
  assert_eq!(kept[0].content, recent.content);
}

/// Phase 2b: when the agent compacts prompt memory mid-run, it emits a
/// `MemorySummaryAdded` event to the live sink so the Harness bridge can
/// surface the between-turn context engineering. Pre-2b this compaction
/// was invisible.
#[tokio::test]
async fn build_llm_messages_emits_memory_summary_added_when_compacting() {
  use crate::runtime::{AgentEventSink, EventSinkHandle};
  use std::sync::Mutex as StdMutex;

  struct RecordingSink {
    events: Arc<StdMutex<Vec<AgentEvent>>>,
  }
  #[async_trait]
  impl AgentEventSink for RecordingSink {
    async fn emit(&self, event: &AgentEvent) {
      self.events.lock().unwrap().push(event.clone());
    }
  }

  let recorded = Arc::new(StdMutex::new(Vec::new()));
  let sink = Arc::new(RecordingSink {
    events: recorded.clone(),
  });
  let mut agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("emit-session");
  agent.live_sink = Some(EventSinkHandle(sink as Arc<dyn AgentEventSink>));

  // Populate memory over the 8-token budget so compaction fires.
  let mut older = Message::user("emit-session", "older context about project goals");
  older.token_count = 10;
  let mut recent = Message::assistant("emit-session", "recent answer");
  recent.token_count = 4;
  agent.add_memory_message(older).await.unwrap();
  agent.add_memory_message(recent).await.unwrap();

  let _ = agent.build_llm_messages("system prompt").await.unwrap();

  let events = recorded.lock().unwrap();
  assert!(
    events
      .iter()
      .any(|e| matches!(e, AgentEvent::MemorySummaryAdded { .. })),
    "mid-run compaction must emit MemorySummaryAdded; got {events:?}"
  );
}

/// V2.2 test bar: every delta the mock streaming provider produces is
/// forwarded live, in order — concatenating them reconstructs the raw
/// model output exactly. Also confirms `TokenDelta` is live-only: it
/// must never appear in the recorded `AgentRunResult.events` (unlike
/// every other live-emitted event, which is also pushed there).
#[tokio::test]
async fn run_with_context_emits_token_delta_events_in_order_matching_the_response() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-token-delta-{}", uuid::Uuid::new_v4());
  let raw_response = r#"{"thought":"done","answer":"the quick brown fox jumps"}"#;
  // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![raw_response]).unwrap(),
    );
  }
  let _responses_guard = EnvVarGuard("YANSHI_MOCK_RESPONSES");
  init_mock_model(&model).await;

  use crate::runtime::AgentEventSink;
  use std::sync::Mutex as StdMutex;

  struct RecordingSink {
    events: Arc<StdMutex<Vec<AgentEvent>>>,
  }
  #[async_trait]
  impl AgentEventSink for RecordingSink {
    async fn emit(&self, event: &AgentEvent) {
      self.events.lock().unwrap().push(event.clone());
    }
  }

  let recorded = Arc::new(StdMutex::new(Vec::new()));
  let sink = Arc::new(RecordingSink {
    events: recorded.clone(),
  });
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(2),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  // `init_run` overwrites `self.live_sink` from `context.event_sink` at
  // the top of every run, so the sink must be attached via the context
  // (not by pre-setting the field, which only works for tests that call
  // a lower-level helper directly, bypassing `init_run`).
  let context = AgentContext::new("token-delta-session", "hi", &model)
    .with_event_sink(sink as Arc<dyn AgentEventSink>);
  let result = agent.run_with_context(context).await.unwrap();
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);

  let deltas: Vec<String> = recorded
    .lock()
    .unwrap()
    .iter()
    .filter_map(|e| match e {
      AgentEvent::TokenDelta { delta, .. } => Some(delta.clone()),
      _ => None,
    })
    .collect();

  assert!(
    deltas.len() > 1,
    "expected a genuine multi-delta sequence, got {} delta(s)",
    deltas.len()
  );
  assert_eq!(
    deltas.concat(),
    raw_response,
    "concatenating every forwarded delta in order must reconstruct the raw model output exactly"
  );

  assert!(
    !result
      .events
      .iter()
      .any(|e| matches!(e, AgentEvent::TokenDelta { .. })),
    "TokenDelta must be live-only, never accumulated into AgentRunResult.events"
  );
}

#[tokio::test]
async fn memory_prompt_budget_uses_custom_summary_backend() {
  let backend = Arc::new(RecordingSummaryBackend::default());
  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("summary-session")
  .with_memory_summary_backend(backend.clone());
  let mut older = Message::user("summary-session", "older context");
  older.token_count = 10;
  let mut recent = Message::assistant("summary-session", "recent answer");
  recent.token_count = 4;

  let (summary, kept) = agent
    .apply_memory_prompt_budget(vec![older.clone(), recent.clone()])
    .await
    .unwrap();

  assert_eq!(
    summary.as_deref(),
    Some("[Custom Summary] omitted=1 kept=1")
  );
  assert_eq!(kept.len(), 1);
  assert_eq!(kept[0].content, recent.content);
  let contexts = backend.contexts.lock().unwrap();
  assert_eq!(contexts.len(), 1);
  assert_eq!(contexts[0].session_id, "summary-session");
  assert_eq!(contexts[0].budget_tokens, 8);
  assert_eq!(contexts[0].omitted_tokens, 10);
  assert_eq!(contexts[0].omitted_messages[0].content, older.content);
}

/// F-A2-13: When the LLM returns the same `(tool, params)` two
/// iterations in a row, the second tool result that lands in the
/// agent's working memory MUST carry a steering note nudging the
/// model to advance instead of looping. The tool itself still
/// runs both times (the steering is advisory, not a hard block),
/// and the trace-side `AgentStepKind::ToolResult` step keeps the
/// raw observation unchanged so replay/audit stay faithful.
#[tokio::test]
async fn repeat_tool_call_appends_steering_note_to_memory() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-repeat-{}", uuid::Uuid::new_v4());

  // Same action twice, then a final answer. Identical params is
  // the trigger — F-A2-13 must detect it on iteration 2. Env var
  // MUST be set BEFORE init_mock_model so the mock provider reads
  // the queue at registration time.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"first try","action":{"tool":"counting_echo","params":{"text":"hi"}}}"#,
        r#"{"thought":"again","action":{"tool":"counting_echo","params":{"text":"hi"}}}"#,
        r#"{"thought":"done","answer":"OK"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));

  let memory_hook = Arc::new(RecordingMemoryHook::default());
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(4),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_memory_hook(memory_hook.clone());

  let result = agent
    .run_with_context(AgentContext::new("session-repeat", "go", &model))
    .await
    .unwrap();

  assert_eq!(result.answer.as_deref(), Some("OK"));
  assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  // Steering is advisory — the tool MUST run both times so a
  // legitimate retry (e.g. polling) isn't broken by F-A2-13.
  assert_eq!(
    calls.load(Ordering::SeqCst),
    2,
    "tool should run both times; steering is a nudge, not a block"
  );

  // Inspect every memory write event the hook saw. The
  // tool-result messages (role=Tool) are the ones that matter
  // for steering; their content is what the model sees on its
  // next turn.
  let events = memory_hook.events.lock().unwrap().clone();
  let tool_result_messages: Vec<Message> = events
    .iter()
    .filter(|c| matches!(c.kind, MemoryHookKind::Write))
    .flat_map(|c| c.messages.iter().cloned())
    .filter(|m| matches!(m.role, Role::Tool))
    .collect();

  assert_eq!(
    tool_result_messages.len(),
    2,
    "expected exactly 2 tool results in memory, got {}",
    tool_result_messages.len()
  );
  assert!(
    !tool_result_messages[0].content.contains("steering note"),
    "first call must NOT carry the steering note: {}",
    tool_result_messages[0].content
  );
  assert!(
    tool_result_messages[1].content.contains("F-A2-13"),
    "second call MUST carry the F-A2-13 steering note: {}",
    tool_result_messages[1].content
  );
  assert!(
    tool_result_messages[1].content.contains("counting_echo"),
    "steering note must name the looping tool: {}",
    tool_result_messages[1].content
  );

  // ToolResult steps (the trace surface) carry the raw
  // observation unchanged — F-A2-13 only touches the memory
  // copy, not the trace.
  let tool_result_steps: Vec<&AgentStepKind> = result
    .steps
    .iter()
    .map(|s| &s.kind)
    .filter(|k| matches!(k, AgentStepKind::ToolResult { .. }))
    .collect();
  assert_eq!(tool_result_steps.len(), 2);
  for step in tool_result_steps {
    if let AgentStepKind::ToolResult { content, .. } = step {
      assert!(
        !content.contains("steering note"),
        "trace-side ToolResult must stay clean of F-A2-13 nudges: {content}"
      );
    }
  }

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

// ── L1.2: sliding-window loop detection ─────────────────────────────────

/// The agent keeps calling the same tool with identical params forever
/// (the mock LLM never varies its response) — loop detection must stop
/// the run well before `max_iterations`, not let it grind to the budget.
#[tokio::test]
async fn loop_detection_stops_before_budget_exhausted_on_repeated_identical_calls() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-loop-{}", uuid::Uuid::new_v4());
  // Unbounded singular response (not the FIFO _RESPONSES queue) so
  // every turn sees the exact same action — proves detection fires on
  // its own, not because the mock ran out of canned responses.
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      r#"{"thought":"again","action":{"tool":"counting_echo","params":{"text":"x"}}}"#,
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(20),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new("session-loop", "go", &model))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::LoopDetected {
      tool: "counting_echo".to_string(),
      repeats: 3,
    }
  );
  let call_count = calls.load(Ordering::SeqCst);
  assert!(
    call_count < 20,
    "must stop well before max_iterations (20), called {call_count} times"
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSE");
  }
}

/// Alternating tool calls (A, B, A, B, ...) must also trip detection —
/// not just strictly consecutive repeats, which F-A2-13 already
/// (weakly) covers.
#[tokio::test]
async fn loop_detection_catches_alternating_signature_pattern() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-loop-alt-{}", uuid::Uuid::new_v4());
  let action_a = r#"{"thought":"a","action":{"tool":"counting_echo","params":{"text":"a"}}}"#;
  let action_b = r#"{"thought":"b","action":{"tool":"counting_echo","params":{"text":"b"}}}"#;
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        action_a, action_b, action_a, action_b, action_a, action_b, action_a, action_b,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let calls = Arc::new(AtomicUsize::new(0));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: calls.clone(),
  }));

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model).with_max_iterations(20),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new("session-loop-alt", "go", &model))
    .await
    .unwrap();

  assert!(
    matches!(result.stop_reason, AgentStopReason::LoopDetected { .. }),
    "alternating A/B calls must trip loop detection too, got {:?}",
    result.stop_reason
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

/// `without_loop_detection()` must disable the feature entirely — the
/// run falls through to whatever other limit trips first (here,
/// `max_iterations`), matching pre-L1.2 behaviour.
#[tokio::test]
async fn loop_detection_can_be_disabled() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-loop-disabled-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      r#"{"thought":"again","action":{"tool":"counting_echo","params":{"text":"x"}}}"#,
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: Arc::new(AtomicUsize::new(0)),
  }));

  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(4)
      .without_loop_detection(),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result = agent
    .run_with_context(AgentContext::new("session-loop-disabled", "go", &model))
    .await
    .unwrap();

  assert_eq!(
    result.stop_reason,
    AgentStopReason::MaxSteps { max_steps: 4 },
    "with loop detection disabled, max_iterations must be what stops the run"
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSE");
  }
}

// ── L2.1: task-summary checkpoint ───────────────────────────────────────

/// Compaction that drops messages must persist a `TaskSummary` capturing
/// what was in them.
#[tokio::test]
async fn compaction_persists_a_task_summary() {
  let store: Arc<dyn yanshi_memory::TaskSummaryStore> =
    Arc::new(yanshi_memory::InMemoryTaskSummaryStore::new());

  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_task_summary_store(store.clone());

  let mut goal = Message::user("budget-session", "find the config file location");
  goal.token_count = 1;
  let mut tool_result = Message::tool_result(
    "budget-session",
    "file",
    "config file is at /etc/yanshi/config.toml",
  );
  tool_result.token_count = 10;
  let mut recent = Message::assistant("budget-session", "recent thought");
  recent.token_count = 4;

  agent
    .apply_memory_prompt_budget(vec![goal, tool_result, recent])
    .await
    .unwrap();

  let persisted = store
    .get_task_summary(&agent.session_id)
    .await
    .unwrap()
    .expect("a task summary must have been persisted");
  assert!(
    persisted
      .key_results
      .iter()
      .any(|r| r.contains("/etc/yanshi/config.toml")),
    "expected the dropped tool result's fact to survive into key_results, got {:?}",
    persisted.key_results
  );
}

/// The regression L2.1 exists for: a fact established before messages
/// were dropped from the prompt must still be visible to a *different*
/// agent instance — same session id, same task-summary store, but
/// otherwise-empty memory (simulating the raw history being gone: a
/// resumed process, or a fresh run reusing the session id after the
/// original process exited).
#[tokio::test]
async fn resumed_agent_still_sees_facts_established_before_truncation() {
  let store: Arc<dyn yanshi_memory::TaskSummaryStore> =
    Arc::new(yanshi_memory::InMemoryTaskSummaryStore::new());
  let session_id = "resume-session";

  // First "process": establishes a fact, then compacts it out of the
  // live prompt window.
  let first_agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id(session_id)
  .with_task_summary_store(store.clone());

  let mut tool_result =
    Message::tool_result(session_id, "file", "the deploy target is prod-us-east-1");
  tool_result.token_count = 10;
  let mut recent = Message::assistant(session_id, "recent thought");
  recent.token_count = 4;
  first_agent
    .apply_memory_prompt_budget(vec![tool_result, recent])
    .await
    .unwrap();

  // Second "process": brand-new agent, same session id, same
  // task-summary store — but a fresh, empty MemoryStore, so the raw
  // message history is genuinely gone, not just compacted.
  let resumed_agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id(session_id)
  .with_task_summary_store(store.clone());

  let messages = resumed_agent.preview_llm_messages().await.unwrap();
  let rendered = format!("{messages:?}");
  assert!(
    rendered.contains("prod-us-east-1"),
    "the resumed agent's prompt must still carry the pre-truncation fact, got: {rendered}"
  );
}

/// Without `with_task_summary_store`, behaviour is unchanged: no
/// persistence, no injection, no extra store reads.
#[tokio::test]
async fn task_summary_is_a_no_op_when_not_configured() {
  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime")
      .with_memory_prompt_token_budget(8)
      .with_memory_summary_strategy(MemorySummaryStrategy::Compact),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  let mut older = Message::user("budget-session", "older context");
  older.token_count = 10;
  let mut recent = Message::assistant("budget-session", "recent");
  recent.token_count = 4;

  // Must not panic or error just because no store is configured.
  let (summary, kept) = agent
    .apply_memory_prompt_budget(vec![older, recent])
    .await
    .unwrap();
  assert!(summary.is_some());
  assert_eq!(kept.len(), 1);
}

// ── L3.1: project-memory checkpoint ─────────────────────────────────────

struct MockShellTool;

#[async_trait]
impl Tool for MockShellTool {
  fn name(&self) -> &str {
    "shell"
  }
  fn description(&self) -> &str {
    "mock shell for tests"
  }
  fn parameters_schema(&self) -> Value {
    json!({ "type": "object" })
  }
  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::success(format!(
      "ran: {}",
      params.get("command").and_then(|v| v.as_str()).unwrap_or("")
    )))
  }
}

/// The regression L3.1 exists for: a second, brand-new agent instance
/// (same project_key + store, but no session in common with the first)
/// can see a fact the first agent's *real run* established, without
/// re-exploring — mirrors L2.1's
/// `resumed_agent_still_sees_facts_established_before_truncation`, one
/// layer up (project-scoped instead of session-scoped).
#[tokio::test]
async fn second_agent_sees_project_facts_established_by_first_run() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-project-memory-{}", uuid::Uuid::new_v4());
  let store: Arc<dyn yanshi_memory::ProjectMemoryStore> =
    Arc::new(yanshi_memory::InMemoryProjectMemoryStore::new());
  let project_key = "proj-l3-1";

  unsafe {
    std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          r#"{"thought":"build it","action":{"tool":"shell","params":{"command":"cargo build --release"}}}"#,
          r#"{"thought":"done","answer":"built"}"#,
        ])
        .unwrap(),
      );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(MockShellTool));

  let mut first_agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_project_memory(store.clone(), project_key);

  let result = first_agent
    .run_with_context(AgentContext::new("session-1", "build the project", &model))
    .await
    .unwrap();
  assert_eq!(result.answer.as_deref(), Some("built"));

  // A second, unrelated agent instance — different session, but the
  // same project_key + store — must see the fact without ever running
  // the command itself.
  let second_agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("session-2")
  .with_project_memory(store.clone(), project_key);

  let messages = second_agent.preview_llm_messages().await.unwrap();
  let rendered = format!("{messages:?}");
  assert!(
    rendered.contains("cargo build --release"),
    "the second agent's prompt must carry the project fact established by the \
       first agent's run, got: {rendered}"
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

/// Facts are scoped to `project_key` — a different project must not see
/// them.
#[tokio::test]
async fn project_facts_are_isolated_by_project_key() {
  let store: Arc<dyn yanshi_memory::ProjectMemoryStore> =
    Arc::new(yanshi_memory::InMemoryProjectMemoryStore::new());
  store
    .record_project_fact("proj-a", "shell", "cargo build")
    .await
    .unwrap();

  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_project_memory(store, "proj-b");

  let messages = agent.preview_llm_messages().await.unwrap();
  let rendered = format!("{messages:?}");
  assert!(
    !rendered.contains("cargo build"),
    "a different project_key must not see proj-a's facts, got: {rendered}"
  );
}

/// Without `with_project_memory`, behaviour is unchanged: no
/// persistence, no injection, no extra store reads.
#[tokio::test]
async fn project_memory_is_a_no_op_when_not_configured() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-no-project-memory-{}", uuid::Uuid::new_v4());
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      serde_json::to_string(&vec![
        r#"{"thought":"build it","action":{"tool":"shell","params":{"command":"cargo build"}}}"#,
        r#"{"thought":"done","answer":"built"}"#,
      ])
      .unwrap(),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(MockShellTool));
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  // Must not panic or error just because no project-memory store is
  // configured.
  let result = agent
    .run_with_context(AgentContext::new("session-none", "build", &model))
    .await
    .unwrap();
  assert_eq!(result.answer.as_deref(), Some("built"));

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

// ── U2.2: preference injection ──────────────────────────────────────────

#[tokio::test]
async fn preference_injection_surfaces_a_directly_seeded_preference() {
  let store: Arc<dyn yanshi_memory::PreferenceStore> = Arc::new(
    yanshi_memory::SqlitePreferenceStore::in_memory()
      .await
      .unwrap(),
  );
  let scope = yanshi_memory::PreferenceScope::local("default");
  store
    .put_preference(&scope, "language", json!("en-GB"))
    .await
    .unwrap();

  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_preference_store(store, scope);

  let messages = agent.preview_llm_messages().await.unwrap();
  let rendered = format!("{messages:?}");
  assert!(
    rendered.contains("en-GB"),
    "the agent's prompt must carry the seeded preference, got: {rendered}"
  );
}

/// Without `with_preference_store`, behaviour is unchanged: no
/// injection, no extra store reads.
#[tokio::test]
async fn preference_is_a_no_op_when_not_configured() {
  let agent = ReActAgent::new(
    ReActConfig::new("mock-runtime"),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  );

  // Must not panic or error just because no preference store is
  // configured.
  let messages = agent.preview_llm_messages().await.unwrap();
  assert!(!format!("{messages:?}").contains("User Preferences"));
}

/// The regression U2.2 exists for: `SqlitePreferenceStore` was
/// previously usable only "standalone" — nothing in a real
/// conversation could ever write to it, so a configured
/// `[memory.preference]` had no product-visible effect. This proves
/// the full round trip through the actual product surface (an LLM
/// tool call, not a direct store write): a first agent's real run
/// calls `remember_preference`, and a second, brand-new agent instance
/// (same store, no session in common) sees it on its very next turn —
/// mirrors L3.1's `second_agent_sees_project_facts_established_by_first_run`.
#[tokio::test]
async fn remember_preference_tool_write_is_visible_to_a_second_agent_instance() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-preference-{}", uuid::Uuid::new_v4());
  let store: Arc<dyn yanshi_memory::PreferenceStore> = Arc::new(
    yanshi_memory::SqlitePreferenceStore::in_memory()
      .await
      .unwrap(),
  );
  let scope = yanshi_memory::PreferenceScope::local("default");

  unsafe {
    std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          r#"{"thought":"remember it","action":{"tool":"remember_preference","params":{"key":"language","value":"en-GB"}}}"#,
          r#"{"thought":"done","answer":"got it"}"#,
        ])
        .unwrap(),
      );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(yanshi_memory::RememberPreferenceTool::new(
    store.clone(),
    scope.clone(),
  )));

  let mut first_agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  )
  .with_preference_store(store.clone(), scope.clone());

  let result = first_agent
    .run_with_context(AgentContext::new(
      "session-1",
      "remember I prefer British English",
      &model,
    ))
    .await
    .unwrap();
  assert_eq!(result.answer.as_deref(), Some("got it"));

  // A second, unrelated agent instance — different session, same
  // store + scope — must see the preference without ever calling
  // `remember_preference` itself.
  let second_agent = ReActAgent::new(
    ReActConfig::new(&model),
    Box::new(SessionMemory::default_window()),
    Arc::new(ToolRegistry::new()),
  )
  .with_session_id("session-2")
  .with_preference_store(store, scope);

  let messages = second_agent.preview_llm_messages().await.unwrap();
  let rendered = format!("{messages:?}");
  assert!(
    rendered.contains("en-GB"),
    "the second agent's prompt must carry the preference the first agent's tool call \
       persisted, got: {rendered}"
  );

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
}

// ── Y2.2 (COR-2): token budget is cumulative spend ─────────────────────

/// A memory whose token count never exceeds a context window, like any
/// summarising / trimming store. The pre-Y2.2 budget check read this
/// number, so a budget above the window could never trip.
struct WindowCappedMemory {
  inner: SessionMemory,
  window: u32,
}

#[async_trait]
impl MemoryStore for WindowCappedMemory {
  async fn add_message(&self, message: Message) -> Result<(), yanshi_memory::MemoryError> {
    self.inner.add_message(message).await
  }
  async fn get_history(
    &self,
    session_id: &str,
    limit: usize,
  ) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.inner.get_history(session_id, limit).await
  }
  async fn get_all(&self, session_id: &str) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.inner.get_all(session_id).await
  }
  async fn search(
    &self,
    session_id: &str,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Message>, yanshi_memory::MemoryError> {
    self.inner.search(session_id, query, limit).await
  }
  async fn clear_session(&self, session_id: &str) -> Result<(), yanshi_memory::MemoryError> {
    self.inner.clear_session(session_id).await
  }
  async fn session_token_count(&self, session_id: &str) -> Result<u32, yanshi_memory::MemoryError> {
    Ok(
      self
        .inner
        .session_token_count(session_id)
        .await?
        .min(self.window),
    )
  }
}

/// Budget 20 000 with an 8 000-token window and ~2 000 tokens per call:
/// the run must stop with `TokenBudgetExceeded` on the first turn after
/// cumulative spend passes 20 000. Before Y2.2 the check read the capped
/// window (≤ 8 000) and the run went on to `max_iterations`.
#[tokio::test]
async fn token_budget_stops_on_cumulative_spend_not_the_memory_window() {
  let _guard = crate::LLM_TEST_LOCK.lock().await;
  let model = format!("mock-token-budget-{}", uuid::Uuid::new_v4());
  // The mock bills 50 prompt tokens + one completion token per
  // whitespace-separated word of its reply.
  let padding = "w ".repeat(1_950);
  unsafe {
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      format!(
        r#"{{"thought":"{padding}","action":{{"tool":"counting_echo","params":{{"text":"x"}}}}}}"#
      ),
    );
  }
  init_mock_model(&model).await;

  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(CountingTool {
    calls: Arc::new(AtomicUsize::new(0)),
  }));
  let budget = 20_000;
  let mut agent = ReActAgent::new(
    ReActConfig::new(&model)
      .with_max_iterations(50)
      .without_loop_detection(),
    Box::new(WindowCappedMemory {
      inner: SessionMemory::default_window(),
      window: 8_000,
    }),
    Arc::new(registry),
  );
  let mut context = AgentContext::new("session-token-budget", "go", &model);
  context.limits.token_budget = Some(budget);
  let result = agent.run_with_context(context).await.unwrap();

  unsafe {
    std::env::remove_var("YANSHI_MOCK_RESPONSE");
  }

  let per_call: Vec<u32> = result
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::LlmCallCompleted { total_tokens, .. } => *total_tokens,
      _ => None,
    })
    .collect();
  let first = *per_call.first().expect("at least one LLM call");
  assert!(
    first > 1_900,
    "each call should bill ~2 000 tokens, got {first}"
  );
  let spent: u32 = per_call.iter().sum();
  let expected_calls = (budget / first + 1) as usize;
  assert_eq!(
    result.stop_reason,
    AgentStopReason::TokenBudgetExceeded {
      used: spent,
      budget
    },
    "must stop on cumulative spend"
  );
  assert_eq!(
    per_call.len(),
    expected_calls,
    "stop on the first turn after spend passes the budget"
  );
}
