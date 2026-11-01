//! `yanshi harness run` — bootstrap a Harness session.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use yanshi_agents::FileLoopCheckpointer;
use yanshi_agents::checkpoint::AgentLoopCheckpointer as _;
use yanshi_agents::plan_execute::{PlanExecuteAgent, PlanExecuteConfig};
use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_agents::runtime::{
  AgentCancellationToken, AgentContext, AgentStopReason, RuntimeLimits,
};
use yanshi_harness::{
  AgentsMdProvider, ApprovalProvider, AutoAllowApprovalProvider, AutoDenyApprovalProvider,
  CliApprovalProvider, DeterministicContextSummarizer, HarnessEventSink, HarnessRunOptions,
  HarnessRunResult, HarnessRuntime, HarnessRuntimeKind, HookConfig, JsonlEventSink,
  RoadmapMdProvider, SeqAllocator, SinkChain, StdoutEventSink, TodosMdProvider,
  WorkspaceLayoutProvider, default_session_dir, wrap_registry,
};
use yanshi_llm::Yanshi;
use yanshi_memory::SqliteMemory;
use yanshi_skills::{SkillBuilder, SkillLoader};
use yanshi_tools::ToolRegistry;

use super::{OutputFormat, parse_profile, resolve_approve_default, resolve_run_dir};

/// V2.2: `Text` output's only prior live feedback was the `🚀 Harness run
/// starting` header — this renders the in-flight step's `token_delta`
/// stream as a single line that grows in place, TTY-gated the same way
/// `harness chat`'s `ChatProgressSink` is (stderr only, so it never
/// pollutes the authoritative stdout final-answer print in `Text` mode).
struct TypingProgressSink {
  enabled: bool,
  typing: std::sync::Mutex<String>,
}

impl TypingProgressSink {
  fn new() -> Self {
    Self {
      enabled: std::io::IsTerminal::is_terminal(&std::io::stderr()),
      typing: std::sync::Mutex::new(String::new()),
    }
  }
}

#[async_trait::async_trait]
impl HarnessEventSink for TypingProgressSink {
  fn name(&self) -> &str {
    "run-typing-progress"
  }

  async fn write(
    &self,
    event: &yanshi_harness::HarnessEvent,
  ) -> Result<(), yanshi_harness::HarnessError> {
    use std::io::Write;
    if !self.enabled {
      return Ok(());
    }
    match &event.body {
      yanshi_harness::HarnessEventBody::TokenDelta(payload) => {
        let mut buf = self.typing.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_str(&payload.delta);
        eprint!("\r\x1b[K💭 {buf}");
        std::io::stderr().flush().ok();
      }
      yanshi_harness::HarnessEventBody::StepStarted(_) => {
        self
          .typing
          .lock()
          .unwrap_or_else(|e| e.into_inner())
          .clear();
      }
      _ => {}
    }
    Ok(())
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  user_input: String,
  skill_dir: Option<String>,
  model_override: Option<String>,
  session: Option<String>,
  workspace: Option<String>,
  profile: String,
  approve: Option<String>,
  runtime_kind: String,
  output: String,
  run_dir_override: Option<String>,
  max_steps: Option<usize>,
  max_tool_calls: Option<usize>,
  timeout_ms: Option<u64>,
  context_budget: Option<usize>,
  token_budget: Option<u32>,
  cost_limit_usd: Option<f64>,
  context_refresh: bool,
  no_default_context: bool,
) -> Result<()> {
  let profile = parse_profile(&profile)?;
  let output = OutputFormat::parse(&output)?;
  let runtime_kind = parse_runtime_kind(&runtime_kind)?;
  // U2.3: unset --approve now defaults per-profile (mirrors T1.3's
  // `workflow dynamic` fix) instead of always hardcoding "none".
  let approve = resolve_approve_default(approve, profile);
  let approve_mode = ApproveMode::parse(&approve)?;

  if skill_dir.is_none() && model_override.is_none() {
    anyhow::bail!("either --skill or --model is required");
  }

  let workspace = workspace
    .map(PathBuf::from)
    .or_else(|| std::env::current_dir().ok())
    .context("could not determine workspace root (pass --workspace or run from a real dir)")?;
  let run_root = resolve_run_dir(run_dir_override)?;
  let session_dir = default_session_dir(&run_root);
  // Persist conversation memory under the run-dir so `--session <id>`
  // resumes the prior turns (long-lived sessions). One DB keyed by
  // session_id; only the `--model` path uses it (the `--skill` path's
  // memory is configured by the skill manifest).
  let memory_db = run_root.join("harness").join("memory.sqlite");
  if let Some(parent) = memory_db.parent() {
    std::fs::create_dir_all(parent)
      .with_context(|| format!("could not create harness memory dir {}", parent.display()))?;
  }

  Yanshi::init()
    .await
    .context("failed to initialise Yanshi LLM config — is your API key configured?")?;

  // V2.3: PlanExecute has no mid-loop LLM re-entry point at all, so it
  // cannot implement `TurnDrivenRuntime` — `--context-refresh` combined
  // with `--runtime plan_execute` has no meaning. Reject up front rather
  // than silently ignoring the flag (matches how `build_agent` below now
  // rejects `--skill` + `--runtime plan_execute`).
  if context_refresh && matches!(runtime_kind, HarnessRuntimeKind::PlanExecute) {
    anyhow::bail!(
      "--context-refresh is not supported with --runtime plan_execute (PlanExecute has no \
       turn-by-turn re-entry point)"
    );
  }

  let (mut agent, model, skill_name) = build_agent(
    skill_dir.as_deref(),
    model_override.as_deref(),
    &memory_db,
    &workspace,
    runtime_kind,
  )
  .await
  .context("failed to construct the inner Harness agent")?;

  // Persist every session as JSONL. Stream-json mode additionally fans
  // out the same envelope to stdout.
  let jsonl = Arc::new(JsonlEventSink::new(session_dir.clone()));
  let jsonl_sink: Arc<dyn HarnessEventSink> = jsonl.clone();
  let stdout_sink: Option<Arc<dyn HarnessEventSink>> = matches!(output, OutputFormat::StreamJson)
    .then(|| Arc::new(StdoutEventSink::new()) as Arc<dyn HarnessEventSink>);

  // Resolve session id eagerly so HookConfig and HarnessRuntime share
  // the same id namespace. Mirrors the server's `LiveHarnessExecutor`
  // pattern; if the user passed --session we honour that, otherwise
  // generate a fresh one.
  let session_id = session.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4().simple()));

  // W0.6: one shared `SeqAllocator` for both the hook layer and the
  // runtime — pre-fix each built its own independent counter (the hook
  // config via a bare `AtomicU64::new(0)`, the runtime via its own
  // default), so the same JSONL sink saw two series both starting at 0
  // and collided on `(session_id, seq)`. `harness chat`'s
  // `build_chat_runtime` and the server's `LiveHarnessExecutor` already
  // share one allocator this way (Q1.7.1 / P-A3.4); this CLI path had
  // slipped through that fix.
  let seq_allocator = SeqAllocator::new();

  // ── F-A2-11: wrap the agent's tool registry with the approval-gate
  // pipeline if requested. Without this, `yanshi harness run` had
  // no approval flow at all (the bare ReActAgent went straight to the
  // inner tools, even under --profile production), forcing users to
  // hand-roll binaries to dogfood Harness Mode from the CLI.
  if let Some(provider) = approve_mode.provider() {
    let mut hook_sinks = SinkChain::new().push(jsonl_sink.clone());
    if let Some(sink) = stdout_sink.as_ref() {
      hook_sinks = hook_sinks.push(sink.clone());
    }
    let hook_config = HookConfig::new(session_id.clone(), provider, hook_sinks)
      .with_profile(profile)
      .with_seq_allocator(seq_allocator.clone());

    // Snapshot the agent's current registry into a fresh one so
    // `wrap_registry` can decorate each tool. Tools come back as the
    // same `Arc<dyn Tool>` instances so any inner state (sandbox
    // policy, MCP session, etc.) is preserved.
    let mut snapshot = ToolRegistry::new();
    for tool in agent.tools().list() {
      snapshot.register(tool);
    }
    let wrapped = wrap_registry(snapshot, hook_config);
    agent = agent.with_tools(wrapped);
  }

  // §6: `--context-refresh` drives the agent turn-by-turn at the harness
  // layer (ReActAgent implements `TurnDrivenRuntime`) and re-runs the
  // context providers between turns; otherwise the agent owns iteration.
  // `context_refresh && plan_execute` was already rejected above, so
  // reaching the turn-driven branch means `agent` is always `React`.
  let mut runtime = if context_refresh {
    let BuiltHarnessAgent::React(react_agent) = agent else {
      unreachable!("--context-refresh + --runtime plan_execute was rejected above");
    };
    HarnessRuntime::new_turn_driven(react_agent).with_context_refresh()
  } else {
    HarnessRuntime::new(agent.into_runtime_box())
  };
  runtime = runtime
    .with_event_sink(jsonl_sink.clone())
    .with_seq_allocator(seq_allocator.clone());
  if !no_default_context {
    runtime = runtime
      .with_context_provider(Arc::new(AgentsMdProvider::new()))
      .with_context_provider(Arc::new(TodosMdProvider::new()))
      .with_context_provider(Arc::new(RoadmapMdProvider::new()))
      .with_context_provider(Arc::new(WorkspaceLayoutProvider::new()));
  }
  if let Some(sink) = stdout_sink.as_ref() {
    runtime = runtime.with_event_sink(sink.clone());
  }
  if matches!(output, OutputFormat::Text) {
    runtime = runtime.with_event_sink(Arc::new(TypingProgressSink::new()));
  }
  // Phase 2a: when a context budget is set, compact over-budget context
  // into a summary (deterministic, replay-safe) instead of dropping it.
  if context_budget.is_some() {
    runtime = runtime.with_context_summarizer(Arc::new(DeterministicContextSummarizer));
  }

  // Q3.1.2: shared cancellation token so the Ctrl-C / SIGTERM future
  // below can ask the inner agent to stop the loop after the current
  // step instead of the runtime aborting the in-flight LLM call.
  let cancel_token = AgentCancellationToken::new();
  let mut options = HarnessRunOptions::new(user_input, workspace.clone(), &model)
    .with_runtime_kind(runtime_kind)
    .with_profile(profile)
    .with_session_id(session_id)
    .with_cancellation_token(cancel_token.clone());
  if let Some(name) = skill_name.as_ref() {
    options = options.with_skill_name(name.clone());
  }
  options = options.with_limits(RuntimeLimits {
    max_steps,
    max_tool_calls,
    timeout_ms,
    token_budget,
    cost_limit_usd,
  });
  if let Some(budget) = context_budget {
    options = options.with_context_token_budget(budget);
  }
  // V2.4: on by default whenever a run-dir is available — a small JSON
  // write per turn is cheap (matches the DAG-level checkpoint's own
  // unconditional-after-every-node precedent), and most long runs
  // benefit from the safety net without having to know to opt in.
  let loop_checkpoint_dir = FileLoopCheckpointer::default_dir(&run_root);
  let loop_checkpointer = Arc::new(
    FileLoopCheckpointer::new(&loop_checkpoint_dir).with_context(|| {
      format!(
        "could not create loop checkpoint dir {}",
        loop_checkpoint_dir.display()
      )
    })?,
  );
  // Kept alongside `options` (which moves the `Arc` into
  // `HarnessRunOptions`) so the V2.3 awaiting-input loop below can load
  // the checkpoint a pause just wrote without re-parsing `run_root`.
  let checkpointer_handle = loop_checkpointer.clone();
  options = options.with_loop_checkpointer(loop_checkpointer);

  let started = std::time::Instant::now();
  if matches!(output, OutputFormat::Text) {
    eprintln!("🚀 Harness run starting");
    eprintln!("   model: {model}");
    if let Some(name) = &skill_name {
      eprintln!("   skill: {name}");
    }
    eprintln!("   workspace: {}", workspace.display());
    eprintln!("   session log: {}", session_dir.display());
  }

  let mut result = race_with_shutdown(runtime.run(options.clone()), &cancel_token).await?;

  // V2.3: a session can pause on `ask_user` any number of times before
  // reaching a final answer. TTY-gated the same way `CliApprovalProvider`
  // is interactive-only: with a real terminal, prompt inline and keep
  // the process running; otherwise print the question and hand off to
  // `resume-loop --answer` rather than blocking on stdin that will never
  // produce a line.
  while let AgentStopReason::AwaitingInput { question } = &result.stop_reason {
    let question = question.clone();
    let Some(answer) = prompt_for_answer(&question) else {
      eprintln!("❓ {question}");
      eprintln!(
        "Harness session {} is awaiting input (no interactive terminal available).",
        result.session_id
      );
      eprintln!(
        "Resume with: yanshi harness resume-loop {} --answer '<answer>' --runtime {} [--skill <dir> | --model <model>]",
        result.session_id,
        runtime_kind.as_str(),
      );
      return Ok(());
    };
    let checkpoint = checkpointer_handle
      .load(&result.session_id)
      .await
      .context("failed to load loop checkpoint to resume the awaiting-input session")?
      .with_context(|| {
        format!(
          "no loop checkpoint found for session '{}' despite AwaitingInput",
          result.session_id
        )
      })?;
    result = race_with_shutdown(
      runtime.resume_from_interrupt(options.clone(), checkpoint, answer),
      &cancel_token,
    )
    .await?;
  }
  let elapsed = started.elapsed();

  match output {
    OutputFormat::Text => {
      let answer = result.answer.as_deref().unwrap_or("(no answer)");
      println!("{answer}");
      println!();
      println!("Session: {}", result.session_id);
      println!(
        "Stop reason: {} — {} events, {} context items (admitted, {} truncated, {} dropped), elapsed {:.2?}",
        format_stop_reason(&result.stop_reason),
        result.final_event_seq + 1,
        result.context_items_admitted,
        result.context_items_truncated,
        result.context_items_dropped,
        elapsed
      );
    }
    OutputFormat::Json => {
      let payload = serde_json::json!({
        "session_id": result.session_id,
        "answer": result.answer,
        "stop_reason": result.stop_reason,
        "final_event_seq": result.final_event_seq,
        "context_items_admitted": result.context_items_admitted,
        "context_items_truncated": result.context_items_truncated,
        "context_items_dropped": result.context_items_dropped,
        "model": model,
        "skill": skill_name,
        "session_log_path": jsonl.session_path(&result.session_id),
        "elapsed_ms": elapsed.as_millis(),
      });
      println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    OutputFormat::StreamJson => {
      // Events were already streamed by `StdoutEventSink`. Emit a final
      // summary line so consumers can join on a deterministic
      // terminator. The summary itself is not a HarnessEvent envelope;
      // mark it explicitly so parsers can skip it if they only want
      // the closed enum.
      let payload = serde_json::json!({
        "type": "harness_run_summary",
        "session_id": result.session_id,
        "stop_reason": result.stop_reason,
        "elapsed_ms": elapsed.as_millis(),
      });
      println!("{}", serde_json::to_string(&payload)?);
    }
    OutputFormat::JsonEnvelope => {
      // P3.3 migration: wrap the same summary the `json` mode emits
      // in the canonical envelope. `stream-json` keeps its raw
      // per-line event format because envelope-per-line would
      // defeat stream framing — operators wanting the run summary
      // alongside live events can run with stream-json and parse
      // the trailing `harness_run_summary` line.
      let payload = serde_json::json!({
        "session_id": result.session_id,
        "answer": result.answer,
        "stop_reason": result.stop_reason,
        "final_event_seq": result.final_event_seq,
        "context_items_admitted": result.context_items_admitted,
        "context_items_truncated": result.context_items_truncated,
        "context_items_dropped": result.context_items_dropped,
        "model": model,
        "skill": skill_name,
        "session_log_path": jsonl.session_path(&result.session_id),
        "elapsed_ms": elapsed.as_millis(),
      });
      // Surface a non-success stop reason as an actionable error
      // string so shell consumers can branch on `errors.length > 0`
      // without inspecting `result.stop_reason`.
      let errors: Vec<String> = if result.stop_reason.is_success() {
        Vec::new()
      } else {
        vec![format!(
          "Harness session stopped before final answer: {:?}",
          result.stop_reason
        )]
      };
      let envelope =
        crate::json_envelope::CliJsonEnvelope::with_errors("harness run", &payload, errors);
      println!("{}", serde_json::to_string_pretty(&envelope)?);
    }
  }

  if !result.stop_reason.is_success() {
    anyhow::bail!(
      "Harness session stopped before producing a final answer: {:?}",
      result.stop_reason
    );
  }

  Ok(())
}

/// V2.3: the concrete agent `build_agent` constructed, before it is
/// boxed into the `HarnessRuntime`'s type-erased `Box<dyn AgentRuntime>`.
///
/// Kept concrete (not `Box<dyn AgentRuntime>`) up through the
/// approval-gate wrapping step, since that step needs each agent's
/// concrete `.tools()`/`.with_tools()` accessors (not part of the
/// `AgentRuntime` trait) — mirrors why `HarnessRuntime` itself only
/// erases the type at the very end, in `HarnessRuntime::new`.
pub(super) enum BuiltHarnessAgent {
  React(Box<ReActAgent>),
  PlanExecute(Box<PlanExecuteAgent>),
}

impl BuiltHarnessAgent {
  pub(super) fn tools(&self) -> &Arc<ToolRegistry> {
    match self {
      Self::React(agent) => agent.tools(),
      Self::PlanExecute(agent) => agent.tools(),
    }
  }

  pub(super) fn with_tools(self, tools: Arc<ToolRegistry>) -> Self {
    match self {
      Self::React(agent) => Self::React(Box::new(agent.with_tools(tools))),
      Self::PlanExecute(agent) => Self::PlanExecute(Box::new(agent.with_tools(tools))),
    }
  }

  /// Erase to `Box<dyn AgentRuntime>` for `HarnessRuntime::new`. Not
  /// usable for `HarnessRuntime::new_turn_driven` — only `ReActAgent`
  /// implements `TurnDrivenRuntime` (PlanExecute has no mid-loop LLM
  /// re-entry point), so callers must reject `--context-refresh` +
  /// `--runtime plan_execute` before reaching this, then unwrap the
  /// `React` variant directly for the turn-driven path.
  pub(super) fn into_runtime_box(self) -> Box<dyn yanshi_agents::runtime::AgentRuntime> {
    match self {
      Self::React(agent) => agent,
      Self::PlanExecute(agent) => agent,
    }
  }

  /// Resume the agent's own loop checkpoint (V2.4/V2.3), dispatching to
  /// whichever concrete runtime built this agent. Used by
  /// `harness resume-loop`, which bypasses `HarnessRuntime` entirely (see
  /// that command's module doc comment) so it can't go through the
  /// `AgentRuntime` trait object either.
  pub(super) async fn resume_from_loop_checkpoint(
    self,
    context: AgentContext,
    checkpoint: yanshi_agents::checkpoint::AgentLoopCheckpoint,
    answer: Option<String>,
  ) -> Result<HarnessRunResult> {
    let inner_result = match self {
      Self::React(mut agent) => agent
        .resume_from_loop_checkpoint(context, checkpoint, answer)
        .await
        .context("Harness loop resume failed")?,
      Self::PlanExecute(mut agent) => agent
        .resume_from_loop_checkpoint(context, checkpoint, answer)
        .await
        .context("Harness loop resume failed")?,
    };
    Ok(HarnessRunResult {
      session_id: inner_result.session_id.clone(),
      answer: inner_result.answer.clone(),
      stop_reason: inner_result.stop_reason.clone(),
      final_event_seq: 0,
      context_items_admitted: 0,
      context_items_dropped: 0,
      context_items_truncated: 0,
      inner: inner_result,
    })
  }
}

pub(super) async fn build_agent(
  skill_dir: Option<&str>,
  model_override: Option<&str>,
  memory_db: &std::path::Path,
  workspace: &std::path::Path,
  runtime_kind: HarnessRuntimeKind,
) -> Result<(BuiltHarnessAgent, String, Option<String>)> {
  if let Some(dir) = skill_dir {
    // V2.3: `SkillBuilder` only ever constructs a `ReActAgent` — skill
    // manifests declare persona/tools/knowledge but not a runtime
    // pattern. Rather than silently ignoring `--runtime plan_execute`
    // (the pre-existing bug this fixes), reject the combination
    // explicitly.
    if !matches!(runtime_kind, HarnessRuntimeKind::React) {
      anyhow::bail!(
        "--skill only supports the react runtime today (skill manifests don't declare a \
         runtime pattern) — pass --model instead of --skill to use --runtime {runtime_kind:?}"
      );
    }
    let dir_path = std::path::Path::new(dir);
    let mut manifest =
      SkillLoader::load(dir_path).with_context(|| format!("failed to load skill from '{dir}'"))?;
    if let Some(model) = model_override {
      manifest.model.name = Some(model.to_owned());
    }
    let _warnings =
      SkillLoader::validate(&manifest, dir_path).with_context(|| "skill validation failed")?;
    let model = manifest.model.resolved_model().to_owned();
    let skill_name = manifest.skill.name.clone();
    // V1.6: `harness run`/`chat` are the only `SkillBuilder` call sites
    // with a real "what directory is this agent working in" concept
    // (`--workspace`, defaulting to CWD) — wires `[memory.project]`
    // against it when the skill declares that table. See
    // `SkillBuilder::build_with_project_root`'s doc comment for why the
    // other call sites stay on plain `build`.
    let agent = SkillBuilder::build_with_project_root(&manifest, dir_path, Some(workspace))
      .await
      .with_context(|| "failed to build agent from skill manifest")?;
    Ok((
      BuiltHarnessAgent::React(Box::new(agent)),
      model,
      Some(skill_name),
    ))
  } else {
    let model = model_override
      .context("--model is required when no --skill is provided")?
      .to_owned();
    // Persistent conversation memory keyed by session_id: a `harness run
    // --session <id>` that reuses an id reads the prior turns back, so
    // long-lived sessions continue across processes.
    let memory = SqliteMemory::open(memory_db).await.with_context(|| {
      format!(
        "failed to open harness memory db at {}",
        memory_db.display()
      )
    })?;
    // W0.1: no `--skill` means no manifest to build tools from, but an
    // always-empty registry leaves the approval/hook pipeline with
    // nothing to ever govern. Default to a safe registry instead:
    // read-only file access scoped to `--workspace`, plus outbound HTTP.
    let default_tools = Arc::new(
      yanshi_tools::default_governed_registry(workspace)
        .with_context(|| "failed to build default tool registry")?,
    );
    let agent = match runtime_kind {
      HarnessRuntimeKind::PlanExecute => {
        BuiltHarnessAgent::PlanExecute(Box::new(PlanExecuteAgent::new(
          PlanExecuteConfig::new(&model),
          Box::new(memory),
          default_tools,
        )))
      }
      // V2.3: `handoff`/`blackboard`/`debate`/`flow` have no
      // `build_agent`-constructible equivalent yet (multi-agent
      // supervisors and Flow governance are separate execution paths
      // entirely) — unchanged pre-existing behaviour of defaulting to
      // react for those, only `react` vs `plan_execute` dispatch is new.
      _ => BuiltHarnessAgent::React(Box::new(ReActAgent::new(
        ReActConfig::new(&model),
        Box::new(memory),
        default_tools,
      ))),
    };
    Ok((agent, model, None))
  }
}

/// Resolved value of the `--approve` flag.
///
/// `None` means the legacy "no wrapping" behaviour: the inner agent
/// drives tools directly, the `ApprovalProvider` is never instantiated,
/// and `--profile` is only used for `HarnessContext`. Any other mode
/// installs a [`HookConfig`] with the matching provider so every
/// NonIdempotent tool call passes through the approval gate (and is
/// auto-escalated to `RequireApproval` under `--profile production`).
#[derive(Debug, Clone, Copy)]
pub(super) enum ApproveMode {
  None,
  Cli,
  AutoAllow,
  AutoDeny,
}

impl ApproveMode {
  pub(super) fn parse(value: &str) -> Result<Self> {
    match value {
      "none" => Ok(Self::None),
      "cli" => Ok(Self::Cli),
      "auto-allow" => Ok(Self::AutoAllow),
      "auto-deny" => Ok(Self::AutoDeny),
      other => anyhow::bail!(
        "unsupported --approve '{other}', expected none | cli | auto-allow | auto-deny"
      ),
    }
  }

  pub(super) fn provider(self) -> Option<Arc<dyn ApprovalProvider>> {
    match self {
      Self::None => None,
      Self::Cli => Some(Arc::new(CliApprovalProvider::stdin())),
      Self::AutoAllow => Some(Arc::new(AutoAllowApprovalProvider::new())),
      Self::AutoDeny => Some(Arc::new(
        AutoDenyApprovalProvider::new().with_stop_on_deny(true),
      )),
    }
  }
}

pub(super) fn parse_runtime_kind(value: &str) -> Result<HarnessRuntimeKind> {
  match value {
    "react" => Ok(HarnessRuntimeKind::React),
    "plan_execute" | "plan-execute" => Ok(HarnessRuntimeKind::PlanExecute),
    "handoff" => Ok(HarnessRuntimeKind::Handoff),
    "blackboard" => Ok(HarnessRuntimeKind::Blackboard),
    "debate" => Ok(HarnessRuntimeKind::Debate),
    "flow" => Ok(HarnessRuntimeKind::Flow),
    other => {
      anyhow::bail!(
        "unsupported --runtime '{other}', expected react | plan_execute | handoff | blackboard | debate | flow"
      )
    }
  }
}

fn format_stop_reason(reason: &yanshi_agents::runtime::AgentStopReason) -> String {
  use yanshi_agents::runtime::AgentStopReason::*;
  match reason {
    FinalAnswer => "final_answer".into(),
    StopCondition { condition } => format!("stop_condition({condition})"),
    MaxSteps { max_steps } => format!("max_steps({max_steps})"),
    MaxToolCalls { max_tool_calls } => format!("max_tool_calls({max_tool_calls})"),
    Timeout { timeout_ms } => format!("timeout({timeout_ms}ms)"),
    Cancelled { message } => format!("cancelled({message})"),
    TokenBudgetExceeded { used, budget } => format!("token_budget({used}/{budget})"),
    CostLimitExceeded {
      used_usd,
      budget_usd,
    } => format!("cost_limit(${used_usd:.4}/${budget_usd:.4})"),
    LoopDetected { tool, repeats } => format!("loop_detected({tool}x{repeats})"),
    Error { message } => format!("error({message})"),
    AwaitingInput { question } => format!("awaiting_input: {question}"),
    ApprovalDenied { message } => format!("approval_denied({message})"),
  }
}

/// V2.3: race a Harness run/resume future against SIGINT/SIGTERM.
/// Shared by the initial `runtime.run(...)` call and every subsequent
/// `runtime.resume_from_interrupt(...)` call in the awaiting-input loop
/// below — the cancellation behaviour (trip the token, give the agent a
/// bounded drain window to surface the terminal `stopped` event, exit
/// 130) is identical either way.
pub(super) async fn race_with_shutdown(
  fut: impl std::future::Future<Output = Result<HarnessRunResult, yanshi_harness::HarnessError>>,
  cancel_token: &AgentCancellationToken,
) -> Result<HarnessRunResult> {
  tokio::pin!(fut);
  tokio::select! {
    biased;
    res = &mut fut => res.context("Harness session failed"),
    _ = crate::shutdown::shutdown_signal() => {
      eprintln!("\n🛑 Cancelled (received SIGINT/SIGTERM)");
      cancel_token.cancel();
      const CANCEL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
      let _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, &mut fut).await;
      std::process::exit(crate::shutdown::SIGINT_EXIT_CODE);
    }
  }
}

/// V2.3: prompt for an `ask_user` answer on stderr and read one non-empty
/// line from stdin. TTY-gated on both stdin and stdout (mirrors
/// `CliApprovalProvider`'s interactivity assumption) — a piped/non-TTY
/// process has no one to prompt, so callers should fall back to printing
/// the question and pointing at `resume-loop --answer` instead of
/// blocking on a stdin that will never produce a line. Returns `None` on
/// EOF as well as on a non-interactive stdin/stdout.
pub(super) fn prompt_for_answer(question: &str) -> Option<String> {
  use std::io::Write;
  if !std::io::IsTerminal::is_terminal(&std::io::stdin())
    || !std::io::IsTerminal::is_terminal(&std::io::stdout())
  {
    return None;
  }
  loop {
    eprintln!("\n❓ {question}");
    eprint!("> ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
      return None;
    }
    let trimmed = line.trim();
    if !trimmed.is_empty() {
      return Some(trimmed.to_string());
    }
    eprintln!("   (answer must not be empty)");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use yanshi_memory::{Message, ProjectMemoryStore};

  /// Resume contract: `build_agent` (the `--model` path) backs the agent
  /// with a persistent SQLite store at the run-dir path, keyed by
  /// session_id. A second `build_agent` against the same DB therefore
  /// reads the prior conversation back — which is what makes
  /// `harness run --session <id>` continue a long-lived session across
  /// processes.
  #[tokio::test]
  async fn build_agent_persists_memory_for_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("memory.sqlite");
    let workspace = tmp.path();

    // First "run": construct the agent and record a turn for a session.
    let (agent1, _model, _skill) = build_agent(
      None,
      Some("mock"),
      &db,
      workspace,
      HarnessRuntimeKind::React,
    )
    .await
    .unwrap();
    let BuiltHarnessAgent::React(agent1) = agent1 else {
      unreachable!("--model path with HarnessRuntimeKind::React always builds React");
    };
    agent1
      .memory_ref()
      .add_message(Message::user("sess-resume", "remember the secret token"))
      .await
      .unwrap();
    drop(agent1);

    // Second "run" (resume): same DB + same session id sees the message.
    let (agent2, _model, _skill) = build_agent(
      None,
      Some("mock"),
      &db,
      workspace,
      HarnessRuntimeKind::React,
    )
    .await
    .unwrap();
    let BuiltHarnessAgent::React(agent2) = agent2 else {
      unreachable!("--model path with HarnessRuntimeKind::React always builds React");
    };
    let history = agent2.memory_ref().get_all("sess-resume").await.unwrap();
    assert!(
      history
        .iter()
        .any(|m| m.content.contains("remember the secret token")),
      "resume must restore the prior conversation from the persistent store"
    );
  }

  /// V2.3 regression test for the pre-existing bug this work fixes:
  /// `harness run --runtime plan_execute` used to silently still build a
  /// `ReActAgent` (`build_agent` never consulted `runtime_kind` at all).
  /// Asserts the `--model` path now genuinely dispatches to
  /// `PlanExecuteAgent` when asked.
  #[tokio::test]
  async fn build_agent_dispatches_plan_execute_runtime_kind_to_plan_execute_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("memory.sqlite");
    let workspace = tmp.path();

    let (agent, _model, _skill) = build_agent(
      None,
      Some("mock"),
      &db,
      workspace,
      HarnessRuntimeKind::PlanExecute,
    )
    .await
    .unwrap();
    assert!(
      matches!(agent, BuiltHarnessAgent::PlanExecute(_)),
      "--runtime plan_execute must build a PlanExecuteAgent, not silently fall back to React"
    );
  }

  /// The `--model` path still defaults to React when no `--runtime` is
  /// given (or an unimplemented multi-agent kind is requested) — only
  /// `plan_execute` gets real dispatch, matching the scoped fix.
  #[tokio::test]
  async fn build_agent_defaults_to_react_for_react_runtime_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("memory.sqlite");
    let workspace = tmp.path();

    let (agent, _model, _skill) = build_agent(
      None,
      Some("mock"),
      &db,
      workspace,
      HarnessRuntimeKind::React,
    )
    .await
    .unwrap();
    assert!(matches!(agent, BuiltHarnessAgent::React(_)));
  }

  /// `SkillBuilder` only ever constructs a `ReActAgent` — skill manifests
  /// don't declare a runtime pattern. `--skill` + `--runtime
  /// plan_execute` must be rejected with a clear error instead of
  /// silently building React anyway (the same bug class this fix
  /// closes, just for the skill path).
  #[tokio::test]
  async fn build_agent_rejects_skill_dir_with_plan_execute_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
      skill_dir.join("skill.toml"),
      r#"
[skill]
name = "plain-skill"
version = "0.1"
description = "test"

[persona]
role = "You are a test agent."

[model]
name = "mock"
"#,
    )
    .unwrap();
    let db = tmp.path().join("memory.sqlite");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let err = match build_agent(
      Some(skill_dir.to_str().unwrap()),
      None,
      &db,
      &workspace,
      HarnessRuntimeKind::PlanExecute,
    )
    .await
    {
      Ok(_) => panic!("expected --skill + --runtime plan_execute to be rejected"),
      Err(err) => err,
    };
    assert!(
      format!("{err:#}").contains("only supports the react runtime"),
      "unexpected error message: {err:#}"
    );
  }

  /// V1.6: `build_agent`'s `--skill` branch passes `workspace` through as
  /// `[memory.project]`'s `project_root` — a fact recorded for that
  /// workspace before the agent is built must already be visible in its
  /// first prompt (confirms the CLI-layer plumbing, not just the
  /// `SkillBuilder`-layer wiring covered by
  /// `yanshi_skills::builder::tests::project_fact_recorded_for_one_root_is_read_by_a_second_agent_for_the_same_root`).
  #[tokio::test]
  async fn build_agent_wires_project_memory_against_the_workspace_for_skill_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let project_db = tmp.path().join("project.sqlite");
    std::fs::write(
      skill_dir.join("skill.toml"),
      format!(
        r#"
[skill]
name = "project-memory-skill"
version = "0.1"
description = "test"

[persona]
role = "You are a test agent."

[model]
name = "mock"

[memory]
type = "none"

[memory.project]
enabled = true
db_path = "{}"
"#,
        project_db.to_string_lossy().replace('\\', "\\\\")
      ),
    )
    .unwrap();

    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let memory_db = tmp.path().join("memory.sqlite");

    let store = yanshi_memory::SqliteProjectMemoryStore::open(&project_db)
      .await
      .unwrap();
    let project_key = yanshi_memory::project_key_for_path(&workspace);
    store
      .record_project_fact(&project_key, "shell", "cargo build --workspace")
      .await
      .unwrap();

    let (agent, _model, skill_name) = build_agent(
      Some(skill_dir.to_str().unwrap()),
      None,
      &memory_db,
      &workspace,
      HarnessRuntimeKind::React,
    )
    .await
    .unwrap();
    assert_eq!(skill_name.as_deref(), Some("project-memory-skill"));
    let BuiltHarnessAgent::React(agent) = agent else {
      unreachable!("--skill path always builds React");
    };

    let messages = agent.preview_llm_messages().await.unwrap();
    let rendered = format!("{messages:?}");
    assert!(
      rendered.contains("cargo build --workspace"),
      "expected the project fact recorded for `workspace` to appear in the \
       agent's first prompt, got: {rendered}"
    );
  }
}
