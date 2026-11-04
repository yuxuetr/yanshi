use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;
use tracing::{debug, info, warn};
use yanshi_async_util::{RaceOutcome, race_with_limits};
use yanshi_llm::{LLMResponse, MultimodalMessage, Yanshi, prompt_fingerprint};
use yanshi_memory::{MemoryStore, Message};
use yanshi_tool::ToolRegistry;

use crate::citation::CitationChecker;
use crate::react::parser::AgentResponse;
use crate::reflection::{ReflectionContext, ReflectionStrategy};
use crate::runtime::{
  AgentCancellationToken, AgentContext, AgentEvent, AgentMemoryHook, AgentRunResult, AgentStep,
  AgentStepKind, AgentStopReason,
};
use crate::verification::VerificationStrategy;

use super::batch::BatchOutcome;
use super::config::{
  ASK_USER_TOOL_NAME, FINAL_ANSWER_TOOL_NAME, LoopDetectionConfig, MemorySummaryBackend,
  ReActConfig, ReActError,
};
use super::support::{
  has_unresolved_tool_call, is_cancelled, is_resume_safe_tool_call, merge_resumed_result,
  most_repeated_signature, native_tool_call_to_agent_response, remaining_timeout,
  strip_yanshi_metadata, timed_out,
};
use super::turn_driven::{LlmTurnOutcome, LoopState, ReActLoopSession, TurnStep};

/// Phase 1 (RFC_HARNESS_LOOP_OWNERSHIP): emit an `AgentEvent` to the
/// optional live sink (if any), then push it into the run's event
/// accumulator. The live emission is inline `.await` at the event's
/// production point so an observer (the Harness bridge) sees it on the
/// same logical clock as governance side-effects that fire during tool
/// execution. With `self.live_sink == None` this is exactly
/// `$events.push(ev)` — byte-identical to the pre-Phase-1 behaviour.
macro_rules! emit_and_push {
  ($sink:expr, $events:expr, $event:expr) => {{
    let ev = $event;
    if let Some(handle) = ($sink).as_ref() {
      handle.0.emit(&ev).await;
    }
    $events.push(ev);
  }};
}
pub(crate) use emit_and_push;

/// Record a step *and* emit its `AgentEvent::StepStarted` live (H.1.1).
///
/// Every recorded `AgentStep` gets a matching `step_started`. Pre-H.1.1 those
/// were reconstructed post-hoc by the Harness after the whole run; emitting them
/// live here lets the bridge interleave them with the tool / approval events
/// that fire during the same step, instead of batching them at the end. The
/// `step_type` comes from the shared [`AgentStepKind::kind_name`] so it always
/// matches the post-hoc path used for non-live runtimes. With `$sink == None`
/// the live emit is a no-op and this is just the step + event push.
macro_rules! push_step {
  ($sink:expr, $steps:expr, $events:expr, $session:expr, $index:expr, $kind:expr) => {{
    let index = $index;
    let kind = $kind;
    let started = AgentEvent::StepStarted {
      session_id: $session.clone(),
      step_index: index,
      step_type: kind.kind_name().to_string(),
      timestamp: Utc::now(),
    };
    if let Some(handle) = ($sink).as_ref() {
      handle.0.emit(&started).await;
    }
    $events.push(started);
    $steps.push(AgentStep::new(index, kind));
  }};
}
pub(crate) use push_step;

/// An autonomous ReAct (Reasoning + Acting) agent.
///
/// On each call to [`ReActAgent::run`], the agent:
/// 1. Stores the user message in memory.
/// 2. Iterates: builds a prompt from memory, calls the LLM, parses the response.
/// 3. If the LLM returns a tool call, executes it and appends the result to memory.
/// 4. If the LLM returns a final answer, stores it and returns.
///
/// ## Example
/// ```rust,no_run
/// use yanshi_agents::react::{ReActAgent, ReActConfig};
/// use yanshi_memory::SessionMemory;
/// use yanshi_tool::ToolRegistry;
/// use yanshi_tools::builtin::ShellTool;
/// use yanshi_tools::sandbox::SandboxPolicy;
/// use std::sync::Arc;
///
/// #[tokio::main]
/// async fn main() {
///     yanshi_llm::Yanshi::init().await.unwrap();
///
///     let policy = Arc::new(SandboxPolicy::default());
///     let mut registry = ToolRegistry::new();
///     registry.register(Arc::new(ShellTool::new(policy)));
///
///     let mut agent = ReActAgent::new(
///         ReActConfig::new("gpt-4o"),
///         Box::new(SessionMemory::default_window()),
///         Arc::new(registry),
///     );
///
///     let answer = agent.run("What is today's date?").await.unwrap();
///     println!("{}", answer);
/// }
/// ```
pub struct ReActAgent {
  pub(crate) config: ReActConfig,
  pub(crate) memory: Box<dyn MemoryStore>,
  pub(crate) tools: Arc<ToolRegistry>,
  pub(crate) reflection: Option<Arc<dyn ReflectionStrategy>>,
  pub(crate) verification: Option<Arc<dyn VerificationStrategy>>,
  /// L4.4: optional citation-consistency check applied to an approved
  /// candidate final answer, right before the run stops. `None` (the
  /// default) disables it entirely — a run with no `rag_search` tool
  /// calls is unaffected either way, since `verify_citations` is a no-op
  /// when there's no `rag_search` result to check citations against.
  pub(crate) citation_checker: Option<Arc<dyn CitationChecker>>,
  pub(crate) memory_hook: Option<Arc<dyn AgentMemoryHook>>,
  pub(crate) memory_summary_backend: Option<Arc<dyn MemorySummaryBackend>>,
  /// L2.1: persisted structured task-narrative checkpoint. `None` (the
  /// default) disables the feature entirely — no reads, no writes, byte
  /// -identical behaviour to before this existed.
  pub(crate) task_summary_store: Option<Arc<dyn yanshi_memory::TaskSummaryStore>>,
  pub(crate) task_summary_generator: Arc<dyn crate::task_summary::TaskSummaryGenerator>,
  /// L3.1: durable, cross-session project facts (e.g. commands observed
  /// to have been run). `None` (the default) disables the feature
  /// entirely. Unlike `task_summary_store` (keyed by `session_id`), this
  /// is keyed by a caller-supplied `project_key` — see
  /// `yanshi_memory::project_key_for_path`.
  pub(crate) project_memory_store: Option<Arc<dyn yanshi_memory::ProjectMemoryStore>>,
  pub(crate) project_key: Option<String>,
  pub(crate) project_fact_generator: Arc<dyn crate::project_memory::ProjectFactGenerator>,
  /// U2.2: durable per-user preferences (tone, language, opt-outs), read
  /// fresh every turn and injected into the persona — see
  /// `docs/MEMORY_LAYERING.md` § Precedence at prompt-assembly time.
  /// `None` (the default) disables the feature entirely. U2.6 redesigned
  /// `PreferenceStore` to `&self` (matching `task_summary_store`/
  /// `project_memory_store`), so this is a bare `Arc<dyn Trait>` too —
  /// no `Mutex` wrapper needed.
  pub(crate) preference_store: Option<Arc<dyn yanshi_memory::PreferenceStore>>,
  pub(crate) preference_scope: Option<yanshi_memory::PreferenceScope>,
  /// Stable identifier for this agent's conversation session
  pub session_id: String,
  /// Token counter used for every `Message::*_with_counter` call
  /// in the run loop (P10.3.3-FU1). Initialised lazily in
  /// `apply_context` from `context.model` so the per-message
  /// `token_count` reflects the real tokenizer for the target
  /// provider — `apply_memory_prompt_budget` then enforces the
  /// budget against the same numbers the LLM will actually bill.
  /// Defaults to the heuristic until the first context arrives.
  pub(crate) message_counter: Box<dyn yanshi_memory::TokenCounter>,
  /// Phase 1 (RFC_HARNESS_LOOP_OWNERSHIP): optional live event observer
  /// captured from `AgentContext::event_sink` at the start of
  /// `run_with_context`. When set, the loop emits each `AgentEvent` to it
  /// as it is produced (in addition to accumulating it into the result),
  /// so the Harness bridge sees tool events on the same logical clock as
  /// the governance events that fire during tool execution. `None` keeps
  /// behavior byte-identical to a runtime with no observer.
  pub(crate) live_sink: Option<crate::runtime::EventSinkHandle>,
  /// V2.4: optional agent-loop checkpointer captured from
  /// `AgentContext::loop_checkpointer` at the start of a run. When set,
  /// the loop saves an `AgentLoopCheckpoint` after every completed turn
  /// so a process restart can resume mid-loop via
  /// [`ReActAgent::resume_from_loop_checkpoint`] instead of restarting.
  pub(crate) live_checkpointer: Option<yanshi_agent_spi::checkpoint::LoopCheckpointerHandle>,
}

impl ReActAgent {
  pub fn new(config: ReActConfig, memory: Box<dyn MemoryStore>, tools: Arc<ToolRegistry>) -> Self {
    let session_id = uuid::Uuid::new_v4().to_string();
    // Build the initial counter from the configured model so
    // agents created without a context (e.g. construction-time
    // dogfooding tools) still produce sane counts. `apply_context`
    // updates this if the run's context overrides the model.
    let message_counter = crate::token_counter_adapter::build_message_counter(&config.model);
    Self {
      config,
      memory,
      tools,
      reflection: None,
      verification: None,
      citation_checker: None,
      memory_hook: None,
      memory_summary_backend: None,
      task_summary_store: None,
      task_summary_generator: Arc::new(crate::task_summary::DeterministicTaskSummaryGenerator),
      project_memory_store: None,
      project_key: None,
      project_fact_generator: Arc::new(crate::project_memory::DeterministicProjectFactGenerator),
      preference_store: None,
      preference_scope: None,
      session_id,
      message_counter,
      live_sink: None,
      live_checkpointer: None,
    }
  }

  /// Continue an existing session by reusing a known `session_id`.
  pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
    self.session_id = session_id.into();
    self
  }

  /// Read-only view of the agent's tool registry.
  ///
  /// Useful for callers that want to introspect which tools are admitted —
  /// for example, the eval harness asserting that admission filters were
  /// applied, or `yanshi skill inspect` rendering the resolved set.
  pub fn tools(&self) -> &Arc<ToolRegistry> {
    &self.tools
  }

  /// Replace the agent's tool registry (builder-style setter).
  ///
  /// Used by callers that need to wrap or transform the registry after
  /// the agent has been constructed — for example, the
  /// `yanshi harness run` CLI uses this to install
  /// `yanshi_harness::wrap_registry`'s approval-gate pipeline
  /// around tools that came from `SkillBuilder::build`, without having
  /// to duplicate the manifest/persona/memory wiring.
  ///
  /// The provided `Arc` becomes the canonical registry for the rest of
  /// the agent's lifetime; subsequent `tools()` calls return this new
  /// `Arc`.
  pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
    self.tools = tools;
    self
  }

  /// Replace the agent's memory store (builder-style). Useful for
  /// injecting a persistent store (e.g. `SqliteMemory` keyed by
  /// session_id) after construction, so a resumed session reads back the
  /// prior conversation — the long-lived-session resume contract.
  pub fn with_memory(mut self, memory: Box<dyn MemoryStore>) -> Self {
    self.memory = memory;
    self
  }

  /// Attach a reflection strategy to the runtime trace.
  pub fn with_reflection_strategy(mut self, strategy: Arc<dyn ReflectionStrategy>) -> Self {
    self.reflection = Some(strategy);
    self
  }

  /// Attach a verification strategy that gates candidate final answers.
  ///
  /// Unlike a reflection strategy, a verification's verdict can change
  /// control flow: a rejection sends the loop back around for another
  /// attempt (bounded by `ReActConfig::max_verification_attempts`)
  /// instead of terminating with `AgentStopReason::FinalAnswer`.
  pub fn with_verification_strategy(mut self, strategy: Arc<dyn VerificationStrategy>) -> Self {
    self.verification = Some(strategy);
    self
  }

  /// Attach a citation-consistency checker (L4.4). Runs once an answer
  /// clears verification (or when verification is disabled/absent),
  /// right before the run stops: if the answer's citations point at a
  /// `rag_search` result, checks whether each one is actually supported.
  /// Unlike a verification rejection, this never loops the run back
  /// around — an unsupported citation set downgrades the answer to a
  /// citation-free version (`crate::citation::downgrade_answer`) and
  /// records the outcome as a `Verify` step / `VerificationCompleted`
  /// event, same as a verification-strategy rejection would.
  pub fn with_citation_checker(mut self, checker: Arc<dyn CitationChecker>) -> Self {
    self.citation_checker = Some(checker);
    self
  }

  /// Attach a memory hook that observes loop reads, searches, and writes.
  pub fn with_memory_hook(mut self, hook: Arc<dyn AgentMemoryHook>) -> Self {
    self.memory_hook = Some(hook);
    self
  }

  /// Attach a custom memory summary backend used when prompt memory exceeds budget.
  pub fn with_memory_summary_backend(mut self, backend: Arc<dyn MemorySummaryBackend>) -> Self {
    self.memory_summary_backend = Some(backend);
    self
  }

  /// Enable L2.1 task-summary checkpointing: persist a structured
  /// [`yanshi_memory::TaskSummary`] whenever compaction drops messages
  /// from the prompt, and inject it back into the prompt on every turn
  /// (covering resumed runs and a fresh run reusing the same session id
  /// alike, since both read the same store). Requires
  /// `memory_prompt_token_budget` + a non-`Disabled` summary strategy to
  /// actually trigger — this only persists/injects, it doesn't decide
  /// when compaction happens.
  pub fn with_task_summary_store(
    mut self,
    store: Arc<dyn yanshi_memory::TaskSummaryStore>,
  ) -> Self {
    self.task_summary_store = Some(store);
    self
  }

  /// Override the default [`crate::task_summary::DeterministicTaskSummaryGenerator`]
  /// with a custom generator (e.g. LLM-backed). No effect unless
  /// [`Self::with_task_summary_store`] is also configured.
  pub fn with_task_summary_generator(
    mut self,
    generator: Arc<dyn crate::task_summary::TaskSummaryGenerator>,
  ) -> Self {
    self.task_summary_generator = generator;
    self
  }

  /// Enable L3.1 project-memory checkpointing: at the end of every
  /// `run_with_context` call, extract durable facts (by default, commands
  /// observed via the `shell`/`script` tools) from the completed run's
  /// steps and persist them under `project_key`, then inject the
  /// accumulated facts back into every subsequent turn's prompt (in this
  /// run and any future run/session that reuses the same store +
  /// `project_key`). `project_key` is caller-supplied — see
  /// [`yanshi_memory::project_key_for_path`] for the recommended
  /// derivation from a project root path.
  ///
  /// Only fires from `run_with_context` (and therefore `run`/
  /// `run_with_trace`); the caller-driven turn-by-turn `LoopSession` path
  /// doesn't go through that chokepoint, so it doesn't get this hook —
  /// callers driving turns manually would need to call the same
  /// extraction themselves.
  pub fn with_project_memory(
    mut self,
    store: Arc<dyn yanshi_memory::ProjectMemoryStore>,
    project_key: impl Into<String>,
  ) -> Self {
    self.project_memory_store = Some(store);
    self.project_key = Some(project_key.into());
    self
  }

  /// Enable U2.2 preference injection: read every `(key, value)` under
  /// `scope` fresh each turn and surface it in the persona. `store` is
  /// shared (not owned) so a caller can also register a
  /// `yanshi_memory::RememberPreferenceTool` wrapping the same `Arc`
  /// — writes from that tool are visible on the agent's very next turn.
  pub fn with_preference_store(
    mut self,
    store: Arc<dyn yanshi_memory::PreferenceStore>,
    scope: yanshi_memory::PreferenceScope,
  ) -> Self {
    self.preference_store = Some(store);
    self.preference_scope = Some(scope);
    self
  }

  /// Override the default [`crate::project_memory::DeterministicProjectFactGenerator`].
  /// No effect unless [`Self::with_project_memory`] is also configured.
  pub fn with_project_fact_generator(
    mut self,
    generator: Arc<dyn crate::project_memory::ProjectFactGenerator>,
  ) -> Self {
    self.project_fact_generator = generator;
    self
  }

  /// Build the LLM message list without calling a model.
  ///
  /// This is useful for prompt debugging and prompt assembly benchmarks.
  pub async fn preview_llm_messages(&self) -> Result<Vec<MultimodalMessage>, ReActError> {
    let system_prompt = self.build_system_prompt();
    self.build_llm_messages(&system_prompt).await
  }

  /// Run the agent on a new user message and return the final answer.
  pub async fn run(&mut self, user_input: &str) -> Result<String, ReActError> {
    let result = self
      .run_with_context(self.context_for_input(user_input))
      .await?;
    Self::answer_from_result(result)
  }

  /// Run the agent on a new user message and return structured runtime output.
  pub async fn run_with_trace(&mut self, user_input: &str) -> Result<AgentRunResult, ReActError> {
    self
      .run_with_context(self.context_for_input(user_input))
      .await
  }

  /// Resume a partial run from a previously serialized runtime trace.
  ///
  /// This first-stage resume support restores durable observations into memory
  /// and continues the loop from a fresh prompt. It deliberately refuses traces
  /// that end with an unresolved tool call, because resuming those would require
  /// re-running a tool whose side effects are unknown.
  pub async fn resume_with_context(
    &mut self,
    context: AgentContext,
    mut prior: AgentRunResult,
  ) -> Result<AgentRunResult, ReActError> {
    if prior.stop_reason.is_success() {
      return Ok(prior);
    }
    self
      .replay_resume_safe_unresolved_tool_calls(&mut prior)
      .await?;
    if has_unresolved_tool_call(&prior) {
      return Err(ReActError::ToolError {
        tool: "runtime".to_string(),
        message: "cannot resume trace with unresolved non-idempotent or unknown tool call"
          .to_string(),
      });
    }

    self.apply_context(&context);
    self.restore_trace_memory(&prior).await?;

    let continuation = if context.input.trim().is_empty() {
      "Continue from the recovered tool observations and produce the next action or final answer."
        .to_string()
    } else {
      format!(
        "{}\n\nContinue from the recovered tool observations. Do not repeat tool calls whose results are already present unless new information is required.",
        context.input
      )
    };
    let resumed = self
      .run_with_context(AgentContext {
        input: continuation,
        ..context
      })
      .await?;

    Ok(merge_resumed_result(prior, resumed))
  }

  /// V2.4: resume a loop interrupted by a process restart from a saved
  /// [`yanshi_agent_spi::checkpoint::AgentLoopCheckpoint`], continuing
  /// from the checkpointed step instead of restarting the loop from
  /// scratch.
  ///
  /// Distinct from [`Self::resume_with_context`], which answers a
  /// different question: "the run stopped with one unresolved tool call —
  /// replay it, then start a *fresh* loop." This method answers "the
  /// process died with no clean stop at all — skip `init_run` entirely and
  /// splice a *restored* loop state directly back into the turn loop."
  ///
  /// Does **not** re-add the user message to memory (unlike `init_run`) —
  /// the memory store is expected to already hold it from the
  /// pre-interruption run. This means loop-checkpoint resume requires the
  /// underlying `MemoryStore` to also be durable and keyed by the same
  /// `session_id` (e.g. `SqliteMemory`, not the in-process `SessionMemory`)
  /// — an in-memory store won't have survived the same process restart the
  /// checkpoint did.
  ///
  /// V2.3: `answer` carries the user's reply when
  /// `checkpoint.pending_question` is set (the run stopped with
  /// [`AgentStopReason::AwaitingInput`]). Exactly one of
  /// `checkpoint.pending_question` / `answer` being `Some` is a hard
  /// error — resuming past an unanswered question, or supplying an
  /// answer nothing asked for, would silently corrupt loop semantics.
  /// When both are `Some`, the answer is written to memory as a user
  /// turn (formalizing the seam `init_run` uses for the original user
  /// message, via the same [`Self::memory_ref`] the caller could already
  /// reach — this makes it a correct-by-construction parameter instead
  /// of a caller-must-know-the-trick precondition) before the turn loop
  /// resumes, so the next LLM call sees it as context. A `ToolResult`
  /// step for the paused `ask_user` call is also pushed, keeping the
  /// `ToolCall`/`ToolResult` pairing trace replay expects even though no
  /// real tool executed.
  pub async fn resume_from_loop_checkpoint(
    &mut self,
    context: AgentContext,
    checkpoint: yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
    answer: Option<String>,
  ) -> Result<AgentRunResult, ReActError> {
    if checkpoint.runtime_kind != yanshi_agent_spi::checkpoint::LoopRuntimeKind::React {
      return Err(ReActError::InvalidCheckpoint {
        message: format!(
          "expected a React loop checkpoint, found {:?}",
          checkpoint.runtime_kind
        ),
      });
    }
    match (&checkpoint.pending_question, &answer) {
      (Some(_), None) => {
        return Err(ReActError::InvalidCheckpoint {
          message: "checkpoint is paused on a question but no answer was supplied".to_string(),
        });
      }
      (None, Some(_)) => {
        return Err(ReActError::InvalidCheckpoint {
          message: "an answer was supplied but the checkpoint has no pending question".to_string(),
        });
      }
      _ => {}
    }
    self.apply_context(&context);
    self.live_sink = context.event_sink.clone();
    self.live_checkpointer = context.loop_checkpointer.clone();

    let mut st = LoopState::from_checkpoint(&context, &self.config, &checkpoint);
    if let Some(answer) = answer {
      push_step!(
        self.live_sink,
        st.steps,
        st.events,
        self.session_id,
        st.step_index,
        AgentStepKind::ToolResult {
          tool: ASK_USER_TOOL_NAME.to_string(),
          content: answer.clone(),
          is_error: false,
          parts: Vec::new(),
        }
      );
      st.step_index += 1;
      self
        .add_memory_message(Message::user_with_counter(
          &self.session_id,
          &answer,
          &*self.message_counter,
        ))
        .await?;
    }
    loop {
      match self.run_one_turn(&mut st).await? {
        TurnStep::Continue => {
          self.save_loop_checkpoint(&st).await;
        }
        TurnStep::Stop(result) => {
          self
            .clear_loop_checkpoint_if_terminal(&result.stop_reason)
            .await;
          self.record_project_facts(&result.steps).await?;
          return Ok(result);
        }
      }
    }
  }

  async fn replay_resume_safe_unresolved_tool_calls(
    &self,
    prior: &mut AgentRunResult,
  ) -> Result<(), ReActError> {
    let unresolved_calls: Vec<(usize, String, Value)> = prior
      .steps
      .iter()
      .filter_map(|step| {
        let AgentStepKind::ToolCall { tool, params } = &step.kind else {
          return None;
        };
        let has_result = prior.steps.iter().any(|candidate| {
          matches!(
            &candidate.kind,
            AgentStepKind::ToolResult {
              tool: result_tool,
              ..
            } if result_tool == tool && candidate.index > step.index
          )
        });
        if has_result {
          None
        } else {
          Some((step.index, tool.clone(), params.clone()))
        }
      })
      .collect();

    let mut next_index = prior.steps.iter().map(|step| step.index).max().unwrap_or(0) + 1;

    for (_step_index, tool, params) in unresolved_calls {
      if !is_resume_safe_tool_call(&params) {
        continue;
      }
      let execute_params = strip_yanshi_metadata(params);
      let output = match self.tools.execute(&tool, execute_params).await {
        Ok(output) => output,
        Err(error) => yanshi_tool::ToolOutput::error(error.to_string()),
      };
      prior.steps.push(AgentStep::new(
        next_index,
        AgentStepKind::ToolResult {
          tool,
          content: output.content,
          is_error: output.is_error,
          parts: output.parts,
        },
      ));
      next_index += 1;
    }

    Ok(())
  }

  /// Query memory for this agent's current session.
  ///
  /// The backing [`MemoryStore`] decides retrieval behavior. With
  /// `yanshi_memory::SemanticMemory`, this performs semantic vector search
  /// with keyword fallback; simpler stores may use keyword matching.
  pub async fn query_memory(&self, query: &str, limit: usize) -> Result<Vec<Message>, ReActError> {
    self
      .query_session_memory(&self.session_id, query, limit)
      .await
  }

  /// Query memory for an explicit session id.
  pub async fn query_session_memory(
    &self,
    session_id: &str,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Message>, ReActError> {
    self.search_memory(session_id, query, limit).await
  }

  /// Run the agent and return structured runtime steps and events.
  pub async fn run_with_context(
    &mut self,
    context: AgentContext,
  ) -> Result<AgentRunResult, ReActError> {
    let mut st = self.init_run(&context).await?;
    loop {
      match self.run_one_turn(&mut st).await? {
        TurnStep::Continue => {
          self.save_loop_checkpoint(&st).await;
        }
        TurnStep::Stop(result) => {
          self
            .clear_loop_checkpoint_if_terminal(&result.stop_reason)
            .await;
          self.record_project_facts(&result.steps).await?;
          return Ok(result);
        }
      }
    }
  }

  /// Set up a run — apply context, capture the live sink, store the user
  /// message, build the system prompt — and return the initial
  /// [`LoopState`]. Shared by [`Self::run_with_context`] (the
  /// batteries-included driver) and [`Self::begin_turn_driven`] (the
  /// caller-owned turn-driven driver). RFC_HARNESS_LOOP_OWNERSHIP §6.
  ///
  /// F-A2-13 anti-loop steering: `last_tool_call` starts `None`; a repeat
  /// single-tool call with identical params later gets a steering note
  /// (see `dispatch_single_tool_call`).
  async fn init_run(&mut self, context: &AgentContext) -> Result<LoopState, ReActError> {
    self.apply_context(context);
    // Phase 1: capture the optional live event observer for this run.
    self.live_sink = context.event_sink.clone();
    // V2.4: capture the optional loop checkpointer for this run.
    self.live_checkpointer = context.loop_checkpointer.clone();
    info!(
        session = %self.session_id,
        model = %self.config.model,
        "ReActAgent starting"
    );

    self
      .add_memory_message(Message::user_with_counter(
        &self.session_id,
        &context.input,
        &*self.message_counter,
      ))
      .await?;

    let system_prompt = self.build_system_prompt();

    let mut state = LoopState {
      steps: vec![AgentStep::new(
        0,
        AgentStepKind::Observe {
          input: context.input.clone(),
        },
      )],
      events: vec![AgentEvent::RunStarted {
        session_id: self.session_id.clone(),
        model: self.config.model.clone(),
        timestamp: context.started_at,
      }],
      step_index: 1,
      iteration: 0,
      tool_calls: 0,
      verification_attempts: 0,
      schema_correction_attempts: 0,
      last_tool_call: None,
      recent_tool_calls: std::collections::VecDeque::new(),
      loop_detection: self.config.loop_detection,
      max_iterations: context
        .limits
        .max_steps
        .unwrap_or(self.config.max_iterations),
      max_tool_calls: context.limits.max_tool_calls,
      timeout_ms: context.limits.timeout_ms,
      budget_tokens: context.limits.token_budget.or(self.config.budget_tokens),
      cost_limit_usd: context.limits.cost_limit_usd.or(self.config.cost_limit_usd),
      cumulative_cost_usd: 0.0,
      cumulative_tokens: 0,
      cancellation_token: context.cancellation_token.clone(),
      run_started_at: Instant::now(),
      system_prompt,
      trace_context: context.trace_context.clone(),
      between_turn_hook: context.between_turn_hook.clone(),
      user_input: context.input.clone(),
    };
    // H.1.1: the first step (`observe`) is recorded in the initial vec above, so
    // emit its `step_started` live here too — every other step does so via
    // `push_step!`. Reuse the recorded step's kind/index so the `step_type`
    // never drifts. Without it, a live-aware runtime would lose the observe
    // boundary (the post-hoc path is skipped once step_started goes live).
    let observe_started = AgentEvent::StepStarted {
      session_id: self.session_id.clone(),
      step_index: state.steps[0].index,
      step_type: state.steps[0].kind.kind_name().to_string(),
      timestamp: context.started_at,
    };
    if let Some(handle) = self.live_sink.as_ref() {
      handle.0.emit(&observe_started).await;
    }
    state.events.push(observe_started);
    Ok(state)
  }

  /// Begin a **turn-driven** run: set up the session and hand back a
  /// [`ReActLoopSession`] the caller pumps one turn at a time via
  /// [`ReActLoopSession::next_turn`], performing its own context
  /// engineering (e.g. memory compaction) between turns. This is the
  /// loop-ownership seam of RFC_HARNESS_LOOP_OWNERSHIP §6 — the same per
  /// turn machinery as [`Self::run_with_context`], but the caller owns
  /// the loop.
  pub async fn begin_turn_driven(
    &mut self,
    context: AgentContext,
  ) -> Result<ReActLoopSession<'_>, ReActError> {
    let state = self.init_run(&context).await?;
    Ok(ReActLoopSession { agent: self, state })
  }

  /// Borrow the run's conversation memory (used by a turn-driven driver
  /// to compact/inspect context between turns).
  pub fn memory_ref(&self) -> &dyn MemoryStore {
    &*self.memory
  }

  /// Execute exactly one turn of the ReAct loop against `st`, returning
  /// `TurnStep::Continue` to advance or `TurnStep::Stop` with the
  /// terminal result. This is the loop body lifted whole out of
  /// `run_with_context` (RFC_HARNESS_LOOP_OWNERSHIP §6 step 5); the
  /// `run_with_context` loop is now just `loop { match run_one_turn … }`.
  /// Callable in isolation, it is the seam a `LoopSession` (step 6) drives.
  pub(crate) async fn run_one_turn(&mut self, st: &mut LoopState) -> Result<TurnStep, ReActError> {
    // Top-of-turn limit guards (cancel / timeout / max-steps / budget).
    if let Some(result) = self
      .check_turn_limits(
        &mut st.steps,
        &mut st.events,
        &mut st.step_index,
        st.iteration,
        st.run_started_at,
        st.timeout_ms,
        st.max_iterations,
        st.budget_tokens,
        st.cost_limit_usd,
        st.cumulative_cost_usd,
        st.cumulative_tokens,
        &st.cancellation_token,
        &st.recent_tool_calls,
        st.loop_detection,
      )
      .await?
    {
      return Ok(TurnStep::Stop(result));
    }

    // LLM call (the Phase 2b between-turn hook fires inside).
    let (llm_response, raw_response) = match self
      .run_turn_llm_call(
        &mut st.steps,
        &mut st.events,
        &mut st.step_index,
        st.iteration,
        &st.system_prompt,
        st.trace_context.clone(),
        &st.between_turn_hook,
        st.run_started_at,
        st.timeout_ms,
        &st.cancellation_token,
        &mut st.cumulative_cost_usd,
        &mut st.cumulative_tokens,
      )
      .await?
    {
      LlmTurnOutcome::Proceed {
        llm_response,
        raw_response,
      } => (llm_response, raw_response),
      LlmTurnOutcome::Stop(result) => return Ok(TurnStep::Stop(result)),
    };

    // Stop conditions.
    if let Some(result) = self
      .check_stop_conditions(
        &mut st.steps,
        &mut st.events,
        &mut st.step_index,
        &raw_response,
      )
      .await?
    {
      return Ok(TurnStep::Stop(result));
    }

    // V2.3: an `ask_user` native tool call pauses the loop, asking the
    // user a question — checked before the `final_answer` scan so a
    // model batching both in one response stops cleanly (asking wins).
    if let Some(call) = llm_response
      .tool_calls
      .iter()
      .find(|call| call.name == ASK_USER_TOOL_NAME)
    {
      let question = call
        .arguments
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
      self
        .add_memory_message(Message::assistant_with_counter(
          &self.session_id,
          &raw_response,
          &*self.message_counter,
        ))
        .await?;
      let ask_step_index = st.step_index;
      // Recorded as a `ToolCall` step (reusing the existing variant per
      // `AgentStepKind`'s doc comment — it IS a native tool call from
      // the model's perspective) with no matching `ToolResult` until
      // resume supplies the answer.
      push_step!(
        self.live_sink,
        st.steps,
        st.events,
        self.session_id,
        st.step_index,
        AgentStepKind::ToolCall {
          tool: ASK_USER_TOOL_NAME.to_string(),
          params: call.arguments.clone(),
        }
      );
      st.step_index += 1;
      let interrupt_event = AgentEvent::InterruptRequested {
        session_id: self.session_id.clone(),
        step_index: ask_step_index,
        question: question.clone(),
        timestamp: Utc::now(),
      };
      if let Some(handle) = self.live_sink.as_ref() {
        handle.0.emit(&interrupt_event).await;
      }
      st.events.push(interrupt_event);
      // Explicit save (not the ordinary per-turn `save_loop_checkpoint`
      // called by this turn's caller on `TurnStep::Continue`) — that
      // checkpoint would be one full turn stale and wouldn't contain
      // this question or the `ToolCall` step just pushed above.
      if let Some(checkpointer) = self.live_checkpointer.as_ref() {
        let checkpoint = st.to_checkpoint(&self.session_id, Some(question.clone()));
        if let Err(e) = checkpointer.0.save(&checkpoint).await {
          warn!(session = %self.session_id, error = %e, "agent loop checkpoint save failed");
        }
      }
      return Ok(TurnStep::Stop(Self::stopped_result(
        &self.session_id,
        None,
        AgentStopReason::AwaitingInput { question },
        std::mem::take(&mut st.steps),
        std::mem::take(&mut st.events),
      )));
    }

    // V2.1: a `final_answer` native tool call is the agent's schema-
    // constrained final answer, not a real tool dispatch — intercept
    // before the batch/single dispatch paths below so it's recognised as
    // an answer even if the model (incorrectly) batches it alongside real
    // tool calls.
    let parsed = if self.config.output_schema.is_some()
      && let Some(call) = llm_response
        .tool_calls
        .iter()
        .find(|call| call.name == FINAL_ANSWER_TOOL_NAME)
    {
      AgentResponse::Answer {
        thought: String::new(),
        answer: serde_json::to_string(&call.arguments).unwrap_or_default(),
      }
    } else {
      // Multi-call batch path (P-H.3): >=2 native tool calls in one
      // response dispatch as a batch (concurrent for idempotent, serial
      // otherwise) in LLM-returned order.
      if llm_response.tool_calls.len() >= 2 {
        match self
          .dispatch_native_tool_calls_batch(
            &llm_response.tool_calls,
            &raw_response,
            &mut st.steps,
            &mut st.events,
            &mut st.step_index,
            &mut st.tool_calls,
            &mut st.recent_tool_calls,
            st.loop_detection,
            st.max_tool_calls,
            st.run_started_at,
            st.timeout_ms,
            st.cancellation_token.as_ref(),
          )
          .await?
        {
          BatchOutcome::Continue => {
            st.iteration += 1;
            return Ok(TurnStep::Continue);
          }
          BatchOutcome::Stop(result) => return Ok(TurnStep::Stop(*result)),
        }
      }

      // Parse response: prefer native tool_calls when present.
      if let Some(call) = llm_response.tool_calls.first() {
        native_tool_call_to_agent_response(call)
      } else {
        AgentResponse::parse(&raw_response)
      }
    };

    // Store the assistant turn.
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        &raw_response,
        &*self.message_counter,
      ))
      .await?;

    match parsed {
      AgentResponse::Action {
        thought,
        tool,
        params,
      } => match self
        .dispatch_single_tool_call(
          thought,
          tool,
          params,
          &mut st.steps,
          &mut st.events,
          &mut st.step_index,
          &mut st.tool_calls,
          &mut st.last_tool_call,
          &mut st.recent_tool_calls,
          st.loop_detection,
          st.iteration,
          st.max_tool_calls,
          st.run_started_at,
          st.timeout_ms,
          &st.cancellation_token,
        )
        .await?
      {
        TurnStep::Continue => {
          st.iteration += 1;
          Ok(TurnStep::Continue)
        }
        TurnStep::Stop(result) => Ok(TurnStep::Stop(result)),
      },

      AgentResponse::Answer { thought, answer } => {
        // Q5.2: `thought` routinely contains user input verbatim —
        // fingerprint + length only at INFO; full text at TRACE.
        info!(
          thought_len = thought.len(),
          thought_sha = %prompt_fingerprint(&thought),
          "Final answer reached"
        );
        tracing::trace!(thought = %thought, "Final answer thought body");
        if !thought.trim().is_empty() {
          push_step!(
            self.live_sink,
            st.steps,
            st.events,
            self.session_id,
            st.step_index,
            AgentStepKind::Plan { thought }
          );
          st.step_index += 1;
        }
        push_step!(
          self.live_sink,
          st.steps,
          st.events,
          self.session_id,
          st.step_index,
          AgentStepKind::FinalAnswer {
            answer: answer.clone(),
          }
        );
        st.step_index += 1;
        self
          .record_reflection(
            ReflectionContext::final_answer(&self.session_id, st.step_index, &answer),
            &mut st.step_index,
            &mut st.steps,
            &mut st.events,
          )
          .await?;
        if self.gate_schema_answer(&answer, st).await? {
          return Ok(TurnStep::Continue);
        }
        if self.gate_candidate_answer(&answer, st).await? {
          return Ok(TurnStep::Continue);
        }
        let answer = self.apply_citation_check(answer, st).await;
        Ok(TurnStep::Stop(Self::stopped_result(
          &self.session_id,
          Some(answer),
          AgentStopReason::FinalAnswer,
          std::mem::take(&mut st.steps),
          std::mem::take(&mut st.events),
        )))
      }

      AgentResponse::Malformed(text) => {
        warn!("LLM returned non-JSON text; treating as final answer");
        push_step!(
          self.live_sink,
          st.steps,
          st.events,
          self.session_id,
          st.step_index,
          AgentStepKind::FinalAnswer {
            answer: text.clone(),
          }
        );
        st.step_index += 1;
        self
          .record_reflection(
            ReflectionContext::final_answer(&self.session_id, st.step_index, &text),
            &mut st.step_index,
            &mut st.steps,
            &mut st.events,
          )
          .await?;
        if self.gate_schema_answer(&text, st).await? {
          return Ok(TurnStep::Continue);
        }
        if self.gate_candidate_answer(&text, st).await? {
          return Ok(TurnStep::Continue);
        }
        let text = self.apply_citation_check(text, st).await;
        Ok(TurnStep::Stop(Self::stopped_result(
          &self.session_id,
          Some(text),
          AgentStopReason::FinalAnswer,
          std::mem::take(&mut st.steps),
          std::mem::take(&mut st.events),
        )))
      }
    }
  }

  pub(crate) fn stopped_result(
    session_id: &str,
    answer: Option<String>,
    reason: AgentStopReason,
    steps: Vec<AgentStep>,
    mut events: Vec<AgentEvent>,
  ) -> AgentRunResult {
    events.push(AgentEvent::RunStopped {
      session_id: session_id.to_string(),
      reason: reason.clone(),
      timestamp: Utc::now(),
    });
    AgentRunResult {
      session_id: session_id.to_string(),
      answer,
      stop_reason: reason,
      steps,
      events,
    }
  }

  pub(crate) fn cancelled_result(
    session_id: &str,
    reason: impl Into<String>,
    steps: Vec<AgentStep>,
    events: Vec<AgentEvent>,
  ) -> AgentRunResult {
    Self::stopped_result(
      session_id,
      None,
      AgentStopReason::Cancelled {
        message: reason.into(),
      },
      steps,
      events,
    )
  }

  /// Run one turn's LLM call: between-turn hook, prompt assembly, the
  /// model round-trip (with timeout/cancellation racing), and the
  /// `LlmCallCompleted` event. Returns the parsed-ready response, or a
  /// terminal result when the turn must stop (cancel / timeout).
  ///
  /// Turn-driven extraction (RFC_HARNESS_LOOP_OWNERSHIP §6, series step
  /// 2): pure relocation out of the `run_with_context` loop; `steps` /
  /// `events` are consumed (via `mem::take`) only on the stop paths.
  #[allow(clippy::too_many_arguments)]
  async fn run_turn_llm_call(
    &self,
    steps: &mut Vec<AgentStep>,
    events: &mut Vec<AgentEvent>,
    step_index: &mut usize,
    iteration: usize,
    system_prompt: &str,
    trace_context: Option<yanshi_llm::LlmTraceContext>,
    between_turn_hook: &Option<crate::runtime::BetweenTurnHookHandle>,
    run_started_at: Instant,
    timeout_ms: Option<u64>,
    cancellation_token: &Option<AgentCancellationToken>,
    cumulative_cost_usd: &mut f64,
    cumulative_tokens: &mut u64,
  ) -> Result<LlmTurnOutcome, ReActError> {
    // Phase 2b between-turn control point.
    if let Some(hook) = between_turn_hook {
      hook
        .0
        .before_turn(iteration, &self.session_id, &*self.memory)
        .await;
    }
    let messages = self.build_llm_messages(system_prompt).await?;

    if is_cancelled(cancellation_token) {
      return Ok(LlmTurnOutcome::Stop(Self::cancelled_result(
        &self.session_id,
        "cancellation token signalled",
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    debug!(iteration, "Calling LLM");
    let tool_specs = self.collect_tool_specs();
    let mut builder = Yanshi::model(&self.config.model)
      .multimodal_messages(messages)
      .trace_context(trace_context);
    if !tool_specs.is_empty() {
      builder = builder.tools(tool_specs);
    }
    let llm_call_started = std::time::Instant::now();
    // V2.2: every ReActAgent LLM call streams now (not just when a Harness
    // caller wants token-level events) — one call path to reason about and
    // test, and it closes a latent gap where a model config with
    // `requires_streaming: true` (no non-streaming mode) could never
    // actually be used via `execute_full`. Each chunk is forwarded live as
    // `AgentEvent::TokenDelta` (see that variant's doc comment for why it's
    // live-only, not accumulated into `events`); `collect_streaming_response`
    // still reconstructs the same aggregate `LLMResponse` shape
    // `execute_full` used to hand back, so everything downstream of this
    // call (tool-call dispatch, JSON parsing, usage/cost accounting) is
    // unchanged.
    let delta_session_id = self.session_id.clone();
    let delta_live_sink = self.live_sink.clone();
    let delta_step_index = *step_index;
    let llm_call = builder.execute_streaming_collected(move |chunk| {
      let delta = chunk.content.clone();
      let is_final = chunk.is_final;
      let session_id = delta_session_id.clone();
      let live_sink = delta_live_sink.clone();
      async move {
        if delta.is_empty() {
          return;
        }
        if let Some(handle) = &live_sink {
          handle
            .0
            .emit(&AgentEvent::TokenDelta {
              session_id,
              step_index: delta_step_index,
              delta,
              is_final,
              timestamp: chrono::Utc::now(),
            })
            .await;
        }
      }
    });
    // Race the model round-trip against the wall-clock budget and the
    // cancellation token (the per-outcome handling — reflection on timeout,
    // cancelled-result on cancel — is what differs per call site).
    let cancel = cancellation_token.as_ref().map(|token| token.cancelled());
    let llm_response: LLMResponse = match race_with_limits(
      llm_call,
      remaining_timeout(run_started_at, timeout_ms),
      cancel,
    )
    .await
    {
      RaceOutcome::Completed(result) => result?,
      RaceOutcome::TimedOut => {
        self
          .record_reflection(
            ReflectionContext::failure(
              &self.session_id,
              *step_index,
              format!(
                "runtime timed out after {}ms",
                timeout_ms.unwrap_or_default()
              ),
            ),
            step_index,
            steps,
            events,
          )
          .await?;
        return Ok(LlmTurnOutcome::Stop(Self::stopped_result(
          &self.session_id,
          None,
          AgentStopReason::Timeout {
            timeout_ms: timeout_ms.unwrap_or_default(),
          },
          std::mem::take(steps),
          std::mem::take(events),
        )));
      }
      RaceOutcome::Cancelled => {
        return Ok(LlmTurnOutcome::Stop(Self::cancelled_result(
          &self.session_id,
          "cancellation token signalled",
          std::mem::take(steps),
          std::mem::take(events),
        )));
      }
    };

    let usage = llm_response.usage.as_ref();
    events.push(AgentEvent::LlmCallCompleted {
      session_id: self.session_id.clone(),
      step_index: *step_index,
      model: self.config.model.clone(),
      prompt_tokens: usage.and_then(|u| u.prompt_tokens),
      completion_tokens: usage.and_then(|u| u.completion_tokens),
      total_tokens: usage.and_then(|u| u.total_tokens),
      duration_ms: llm_call_started.elapsed().as_millis() as u64,
      timestamp: chrono::Utc::now(),
    });

    // Y2.2 (COR-2): accrue what the provider says this call actually
    // consumed. `total_tokens` when reported, else prompt + completion.
    if let Some(u) = usage {
      let call_tokens = u.total_tokens.map(u64::from).unwrap_or_else(|| {
        u64::from(u.prompt_tokens.unwrap_or(0)) + u64::from(u.completion_tokens.unwrap_or(0))
      });
      *cumulative_tokens = cumulative_tokens.saturating_add(call_tokens);
    }

    // T1.1: accrue this call's estimated cost against the run's cost
    // budget. `pricing_table` defaults to all-zero, so this is a no-op
    // unless the caller configured real per-model prices.
    *cumulative_cost_usd += self
      .config
      .pricing_table
      .lookup(&self.config.model)
      .cost_for_call(
        usage.and_then(|u| u.prompt_tokens),
        usage.and_then(|u| u.completion_tokens),
      );

    let raw_response = llm_response.content.clone();
    debug!(
      response_len = raw_response.len(),
      response_sha = %prompt_fingerprint(&raw_response),
      "LLM responded"
    );
    tracing::trace!(response = %raw_response, "LLM response body");

    Ok(LlmTurnOutcome::Proceed {
      llm_response,
      raw_response,
    })
  }

  /// After the LLM call, stop the run if the response contains any
  /// configured stop string. Returns `Some(result)` to stop, `None` to
  /// continue to the parse/dispatch phase.
  ///
  /// Turn-driven extraction (RFC_HARNESS_LOOP_OWNERSHIP §6, series step
  /// 3): pure relocation; `steps`/`events` are consumed via `mem::take`
  /// only on the stop path.
  async fn check_stop_conditions(
    &mut self,
    steps: &mut Vec<AgentStep>,
    events: &mut Vec<AgentEvent>,
    step_index: &mut usize,
    raw_response: &str,
  ) -> Result<Option<AgentRunResult>, ReActError> {
    let Some(condition) = self
      .config
      .stop_conditions
      .iter()
      .find(|cond| raw_response.contains(cond.as_str()))
      .cloned()
    else {
      return Ok(None);
    };
    info!("Stop condition matched: '{}'", condition);
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        raw_response,
        &*self.message_counter,
      ))
      .await?;
    self
      .record_reflection(
        ReflectionContext::final_answer(&self.session_id, *step_index, raw_response),
        step_index,
        steps,
        events,
      )
      .await?;
    Ok(Some(Self::stopped_result(
      &self.session_id,
      Some(raw_response.to_string()),
      AgentStopReason::StopCondition { condition },
      std::mem::take(steps),
      std::mem::take(events),
    )))
  }

  /// Top-of-turn limit guards (cancel / timeout / max-steps / token
  /// budget). Returns `Some(result)` when the run must stop this turn,
  /// `None` to proceed with the LLM call.
  ///
  /// Turn-driven extraction (RFC_HARNESS_LOOP_OWNERSHIP §6, series step
  /// 1): pulling the guards out of the monolithic `run_with_context`
  /// loop is the first move toward a resumable `LoopSession`. Behaviour
  /// is identical — this is a pure relocation; `steps`/`events` are only
  /// consumed (via `mem::take`) on the stop paths, where the caller
  /// returns immediately.
  #[allow(clippy::too_many_arguments)]
  async fn check_turn_limits(
    &self,
    steps: &mut Vec<AgentStep>,
    events: &mut Vec<AgentEvent>,
    step_index: &mut usize,
    iteration: usize,
    run_started_at: Instant,
    timeout_ms: Option<u64>,
    max_iterations: usize,
    budget_tokens: Option<u32>,
    cost_limit_usd: Option<f64>,
    cumulative_cost_usd: f64,
    cumulative_tokens: u64,
    cancellation_token: &Option<AgentCancellationToken>,
    recent_tool_calls: &std::collections::VecDeque<(String, serde_json::Value)>,
    loop_detection: Option<LoopDetectionConfig>,
  ) -> Result<Option<AgentRunResult>, ReActError> {
    if is_cancelled(cancellation_token) {
      return Ok(Some(Self::cancelled_result(
        &self.session_id,
        "cancellation token signalled",
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    if timed_out(run_started_at, timeout_ms) {
      self
        .record_reflection(
          ReflectionContext::failure(
            &self.session_id,
            *step_index,
            format!(
              "runtime timed out after {}ms",
              timeout_ms.unwrap_or_default()
            ),
          ),
          step_index,
          steps,
          events,
        )
        .await?;
      return Ok(Some(Self::stopped_result(
        &self.session_id,
        None,
        AgentStopReason::Timeout {
          timeout_ms: timeout_ms.unwrap_or_default(),
        },
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    if iteration >= max_iterations {
      self
        .record_reflection(
          ReflectionContext::failure(
            &self.session_id,
            *step_index,
            format!("max steps ({}) reached", max_iterations),
          ),
          step_index,
          steps,
          events,
        )
        .await?;
      return Ok(Some(Self::stopped_result(
        &self.session_id,
        None,
        AgentStopReason::MaxSteps {
          max_steps: max_iterations,
        },
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    if let Some(budget) = budget_tokens {
      // Y2.2 (COR-2): the budget is spend, not context size. The memory
      // window's token count is capped by summarisation / trimming, so a
      // budget above the window size could never trip on it. It stays as
      // a floor for providers that report no usage at all.
      let window = self.memory.session_token_count(&self.session_id).await?;
      let used = u32::try_from(cumulative_tokens)
        .unwrap_or(u32::MAX)
        .max(window);
      if used > budget {
        self
          .record_reflection(
            ReflectionContext::failure(
              &self.session_id,
              *step_index,
              format!("token budget exceeded: {} / {}", used, budget),
            ),
            step_index,
            steps,
            events,
          )
          .await?;
        return Ok(Some(Self::stopped_result(
          &self.session_id,
          None,
          AgentStopReason::TokenBudgetExceeded { used, budget },
          std::mem::take(steps),
          std::mem::take(events),
        )));
      }
    }

    // T1.1: cumulative-cost guard. Checked at the top of the turn (like
    // every other bound above) so a call that tips the run over budget
    // is allowed to finish and be recorded, then the *next* LLM call is
    // what actually gets stopped — mirrors the token-budget check's
    // one-turn-delayed reaction rather than trying to abort mid-call.
    if let Some(budget) = cost_limit_usd
      && cumulative_cost_usd > budget
    {
      self
        .record_reflection(
          ReflectionContext::failure(
            &self.session_id,
            *step_index,
            format!(
              "cost limit exceeded: ${:.4} / ${:.4}",
              cumulative_cost_usd, budget
            ),
          ),
          step_index,
          steps,
          events,
        )
        .await?;
      return Ok(Some(Self::stopped_result(
        &self.session_id,
        None,
        AgentStopReason::CostLimitExceeded {
          used_usd: cumulative_cost_usd,
          budget_usd: budget,
        },
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    // L1.2: sliding-window loop detection — catches a stuck loop (whether
    // strictly consecutive or alternating, e.g. A, B, A, B, ...) before it
    // exhausts the step/tool-call/token budget above. Checked against calls
    // accumulated during prior turns, so it fires with a one-turn delay
    // relative to the call that actually tipped it over the threshold.
    if let Some(cfg) = loop_detection
      && let Some((tool, repeats)) = most_repeated_signature(recent_tool_calls)
      && repeats >= cfg.threshold
    {
      self
        .record_reflection(
          ReflectionContext::failure(
            &self.session_id,
            *step_index,
            format!(
              "loop detected: tool `{tool}` called with identical params {repeats} times \
               within the last {} calls",
              recent_tool_calls.len()
            ),
          ),
          step_index,
          steps,
          events,
        )
        .await?;
      return Ok(Some(Self::stopped_result(
        &self.session_id,
        None,
        AgentStopReason::LoopDetected { tool, repeats },
        std::mem::take(steps),
        std::mem::take(events),
      )));
    }

    Ok(None)
  }

  pub(crate) async fn record_reflection(
    &self,
    context: ReflectionContext,
    step_index: &mut usize,
    steps: &mut Vec<AgentStep>,
    events: &mut Vec<AgentEvent>,
  ) -> Result<(), ReActError> {
    if !self.config.reflection_enabled {
      return Ok(());
    }
    let Some(strategy) = &self.reflection else {
      return Ok(());
    };
    let reflection = strategy
      .reflect(&context)
      .await
      .map_err(|err| ReActError::ToolError {
        tool: "reflection".to_string(),
        message: err.to_string(),
      })?;
    let Some(reflection) = reflection else {
      return Ok(());
    };

    let current_step = *step_index;
    push_step!(
      self.live_sink,
      steps,
      events,
      self.session_id,
      current_step,
      AgentStepKind::Reflect {
        content: reflection.content,
      }
    );
    events.push(AgentEvent::ReflectionAdded {
      session_id: self.session_id.clone(),
      step_index: current_step,
      timestamp: reflection.timestamp,
    });
    *step_index += 1;
    Ok(())
  }
}
