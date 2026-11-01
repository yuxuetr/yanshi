//! `yanshi harness chat` — interactive multi-turn Harness REPL.
//!
//! Builds the agent + `HarnessRuntime`, then loops: read a line from
//! stdin, run one Harness turn against a **fixed session id** (so the
//! conversation memory accumulates across turns), print the answer. Every
//! turn goes through the full harness layer — context assembly, the
//! approval gate, the event bridge, persistence — unlike `skill chat`
//! which drives the skill's agent directly.
//!
//! Lines beginning with `/` are REPL **commands** (`/help`, `/session`,
//! `/new`, `/clear`, `/model <name>`, `/skill <dir>`); everything else is a
//! message to the agent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use tokio::sync::{mpsc, oneshot};
use yanshi_agents::FileLoopCheckpointer;
use yanshi_agents::checkpoint::AgentLoopCheckpointer as _;
use yanshi_agents::runtime::{AgentCancellationToken, AgentStopReason, RuntimeLimits};
use yanshi_harness::{
  AgentsMdProvider, ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest,
  ApprovalScope, DeterministicContextSummarizer, HarnessError, HarnessEvent, HarnessEventBody,
  HarnessEventSink, HarnessProfile, HarnessRunOptions, HarnessRuntime, HarnessRuntimeKind,
  HookConfig, JsonlEventSink, RoadmapMdProvider, SeqAllocator, SinkChain, TodosMdProvider,
  WorkspaceLayoutProvider, default_session_dir, wrap_registry,
};
use yanshi_llm::Yanshi;
use yanshi_memory::{MemoryStore, SqliteMemory};
use yanshi_tools::ToolRegistry;

use super::run::{ApproveMode, build_agent, parse_runtime_kind};
use super::{parse_profile, resolve_approve_default, resolve_run_dir};
use crate::commands::repl::{LineReader, ReadLine};

/// Per-chat configuration that does not change across REPL commands.
/// Bundled so [`build_chat_runtime`] can be called again to rebuild the
/// runtime when the user switches model / skill, while keeping the same
/// session (so the conversation carries over via persistent memory),
/// event sink, and seq counter.
struct ChatConfig {
  profile: HarnessProfile,
  approve_mode: ApproveMode,
  /// H.2.1: when set (interactive `--approve cli` in chat), used in place of
  /// `approve_mode.provider()` so approvals route through the REPL's stdin.
  approval_override: Option<Arc<dyn ApprovalProvider>>,
  runtime_kind: HarnessRuntimeKind,
  context_budget: Option<usize>,
  context_refresh: bool,
  no_default_context: bool,
  jsonl: Arc<dyn HarnessEventSink>,
  seq_allocator: SeqAllocator,
  memory_db: PathBuf,
  /// V2.3: on by default (mirrors `harness run`'s own posture) so a
  /// turn that pauses on `ask_user` has a checkpoint to resume from —
  /// without this, `AwaitingInput` in chat would have no way back into
  /// the loop at all.
  loop_checkpointer: Arc<FileLoopCheckpointer>,
  session_id: String,
  /// V1.6: passed to `SkillBuilder::build_with_project_root` so a
  /// `[memory.project]`-declaring skill gets its project-memory layer
  /// wired against the chat's real working directory.
  workspace: PathBuf,
}

/// A pending approval handed to the REPL: the request + a one-shot channel to
/// return the user's decision on.
type ApprovalAsk = (ApprovalRequest, oneshot::Sender<ApprovalDecision>);

/// H.3.1: streams concise live progress lines to stderr as the Harness emits
/// tool / memory events *during* a turn, so the user sees activity step-by-step
/// instead of only the final answer appearing after the whole turn completes.
///
/// Gated on a TTY: when stderr is piped (tests, `… | cat`) it stays silent so
/// captured output is just the answer on stdout. Each line first clears the
/// `⏳ thinking…` spinner (or the prior progress line) with CR + erase-to-EOL,
/// so progress lines stack cleanly above the eventual answer.
struct ChatProgressSink {
  enabled: bool,
  /// V2.2: accumulates the in-flight step's `token_delta` text so it
  /// renders as a single line that grows in place (a typing indicator)
  /// instead of one stacked line per chunk. Reset on the next
  /// `step_started` or whenever a discrete progress line is printed.
  typing: std::sync::Mutex<String>,
}

impl ChatProgressSink {
  fn new() -> Self {
    Self {
      enabled: std::io::IsTerminal::is_terminal(&std::io::stderr()),
      typing: std::sync::Mutex::new(String::new()),
    }
  }

  fn line(&self, text: &str) {
    use std::io::Write;
    eprint!("\r\x1b[K{text}\n");
    std::io::stderr().flush().ok();
  }

  fn typing(&self, delta: &str) {
    use std::io::Write;
    let mut buf = self.typing.lock().unwrap_or_else(|e| e.into_inner());
    buf.push_str(delta);
    eprint!("\r\x1b[K💭 {buf}");
    std::io::stderr().flush().ok();
  }

  fn reset_typing(&self) {
    self
      .typing
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clear();
  }
}

#[async_trait::async_trait]
impl HarnessEventSink for ChatProgressSink {
  fn name(&self) -> &str {
    "chat-progress"
  }

  async fn write(&self, event: &HarnessEvent) -> Result<(), HarnessError> {
    if !self.enabled {
      return Ok(());
    }
    match &event.body {
      HarnessEventBody::TokenDelta(payload) => self.typing(&payload.delta),
      HarnessEventBody::StepStarted(_) => self.reset_typing(),
      _ => {
        if let Some(text) = progress_line(&event.body) {
          self.reset_typing();
          self.line(&text);
        }
      }
    }
    Ok(())
  }
}

/// Map a Harness event body to the concise progress line the REPL streams, or
/// `None` for events that are not part of the live activity narrative (session
/// lifecycle, approvals, post-hoc `step_started`). Pure so it is unit-testable.
fn progress_line(body: &HarnessEventBody) -> Option<String> {
  match body {
    HarnessEventBody::ToolCallRequested(p) => Some(format!("   🔧 {}…", p.tool)),
    HarnessEventBody::ToolCallCompleted(p) => {
      let mark = if p.is_error { "✗" } else { "✓" };
      Some(format!("   {mark} {} ({} ms)", p.tool, p.duration_ms))
    }
    HarnessEventBody::MemorySummaryAdded(p) => {
      Some(format!("   📝 context compacted ({})", p.layer))
    }
    _ => None,
  }
}

/// Interactive `--approve cli` provider for `harness chat` (H.2.1). The blocking
/// [`yanshi_harness::CliApprovalProvider`] can't be used here because it reads
/// `std::io::stdin` while the REPL owns the async tokio stdin reader — the two
/// race. Instead this forwards each approval request to the REPL loop over a
/// channel and awaits the decision the loop reads from the *same* shared stdin.
struct ChatApprovalProvider {
  tx: mpsc::UnboundedSender<ApprovalAsk>,
}

#[async_trait::async_trait]
impl ApprovalProvider for ChatApprovalProvider {
  fn name(&self) -> &str {
    "chat-cli"
  }

  async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
    let (resp_tx, resp_rx) = oneshot::channel();
    self
      .tx
      .send((request, resp_tx))
      .map_err(|_| HarnessError::Other("chat approval channel closed".into()))?;
    resp_rx
      .await
      .map_err(|_| HarnessError::Other("chat approval response dropped".into()))
  }
}

/// Parse a one-line approval response into an outcome + scope. Mirrors the
/// `CliApprovalProvider` keys (`y`/`s`/`r`/`n`/`q`); anything else (incl. EOF)
/// is a fail-closed deny.
fn parse_approval_response(input: &str) -> (ApprovalOutcome, ApprovalScope, Option<String>) {
  match input.trim().to_ascii_lowercase().as_str() {
    "y" | "yes" | "allow" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Once,
      Some("user allowed once".into()),
    ),
    "s" | "session" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Session,
      Some("user allowed for session".into()),
    ),
    "r" | "run" => (
      ApprovalOutcome::Allow,
      ApprovalScope::Run,
      Some("user allowed for run".into()),
    ),
    "q" | "quit" => (
      ApprovalOutcome::DenyAndStop,
      ApprovalScope::Once,
      Some("user denied and requested stop".into()),
    ),
    other => (
      ApprovalOutcome::Deny,
      ApprovalScope::Once,
      Some(if other.is_empty() || matches!(other, "n" | "no") {
        "user denied".into()
      } else {
        format!("unrecognised input '{other}' — defaulting to deny")
      }),
    ),
  }
}

/// Print the approval prompt to stderr and read one decision line from the
/// REPL's shared line reader (H.4.1). EOF / Ctrl-C / read error → fail-closed deny.
async fn prompt_and_read_decision(
  request: &ApprovalRequest,
  reader: &mut LineReader,
) -> ApprovalDecision {
  use std::io::Write;
  eprintln!("\n── Harness approval request ──");
  eprintln!("  tool: {} (step={})", request.tool, request.step_index);
  eprintln!(
    "  risk: {:?}   idempotency: {:?}",
    request.risk, request.idempotency
  );
  eprintln!("  reason: {}", request.reason);
  eprintln!("  params: {}", request.params_summary);
  std::io::stderr().flush().ok();

  let line = match reader
    .read_line("Allow this call? [y]es / [s]ession / [r]un / [n]o / [q]uit: ")
    .await
  {
    Ok(ReadLine::Line(line)) => line,
    // EOF / Ctrl-C / error → fail-closed deny.
    _ => String::new(),
  };
  let (decision, scope, reason) = parse_approval_response(&line);
  ApprovalDecision {
    request_id: request.id.clone(),
    decision,
    scope,
    decided_by: "user".into(),
    decided_at: chrono::Utc::now(),
    reason,
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  skill_dir: Option<String>,
  model_override: Option<String>,
  session: Option<String>,
  workspace: Option<String>,
  profile: String,
  approve: Option<String>,
  runtime_kind: String,
  run_dir_override: Option<String>,
  context_budget: Option<usize>,
  token_budget: Option<u32>,
  cost_limit_usd: Option<f64>,
  context_refresh: bool,
  max_steps: Option<usize>,
  no_default_context: bool,
) -> Result<()> {
  let profile = parse_profile(&profile)?;
  let runtime_kind = parse_runtime_kind(&runtime_kind)?;
  // U2.3: unset --approve now defaults per-profile (mirrors T1.3's
  // `workflow dynamic` fix) instead of always hardcoding "none".
  let approve = resolve_approve_default(approve, profile);
  let approve_mode = ApproveMode::parse(&approve)?;

  if skill_dir.is_none() && model_override.is_none() {
    anyhow::bail!("either --skill or --model is required");
  }

  // H.2.1: interactive `--approve cli` in chat. The REPL owns the async stdin
  // reader, so instead of the blocking `CliApprovalProvider` we install a
  // channel-based `ChatApprovalProvider`; approval requests are answered by the
  // turn loop below, which reads the decision from the same shared reader.
  let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalAsk>();
  let approval_override: Option<Arc<dyn ApprovalProvider>> =
    matches!(approve_mode, ApproveMode::Cli).then(|| {
      Arc::new(ChatApprovalProvider {
        tx: approval_tx.clone(),
      }) as Arc<dyn ApprovalProvider>
    });

  let workspace = workspace
    .map(PathBuf::from)
    .or_else(|| std::env::current_dir().ok())
    .context("could not determine workspace root (pass --workspace or run from a real dir)")?;
  let run_root = resolve_run_dir(run_dir_override)?;
  let session_dir = default_session_dir(&run_root);
  let memory_db = run_root.join("harness").join("memory.sqlite");
  if let Some(parent) = memory_db.parent() {
    std::fs::create_dir_all(parent)
      .with_context(|| format!("could not create harness memory dir {}", parent.display()))?;
  }

  Yanshi::init()
    .await
    .context("failed to initialise Yanshi LLM config — is your API key configured?")?;

  let loop_checkpoint_dir = FileLoopCheckpointer::default_dir(&run_root);
  let loop_checkpointer = Arc::new(
    FileLoopCheckpointer::new(&loop_checkpoint_dir).with_context(|| {
      format!(
        "could not create loop checkpoint dir {}",
        loop_checkpoint_dir.display()
      )
    })?,
  );

  let mut cfg = ChatConfig {
    profile,
    approve_mode,
    approval_override,
    runtime_kind,
    context_budget,
    context_refresh,
    no_default_context,
    jsonl: Arc::new(JsonlEventSink::new(session_dir.clone())),
    // One shared seq allocator (counter + emit lock) for the hook layer and the
    // runtime so events stay monotonic *and in wire order* across the whole chat
    // (Q1.7.1 + P-A3.4) and across rebuilds.
    seq_allocator: SeqAllocator::new(),
    memory_db,
    loop_checkpointer,
    session_id: session.unwrap_or_else(|| format!("chat-{}", uuid::Uuid::new_v4().simple())),
    workspace: workspace.clone(),
  };

  // Current agent source — switched at runtime by /model and /skill.
  let mut cur_skill: Option<String> = skill_dir;
  let mut cur_model: Option<String> = model_override;

  let (mut runtime, mut model, mut skill_name) =
    build_chat_runtime(&cfg, cur_skill.as_deref(), cur_model.as_deref())
      .await
      .context("failed to construct the Harness runtime")?;

  print_banner(&model, skill_name.as_deref(), &cfg, &approve);

  // H.4.1: line editing + up/down history on a TTY; transparent plain-reader
  // fallback when piped (tests). The same reader feeds the H.2.1 approval prompt
  // so input stays consistent.
  let mut reader = LineReader::new();
  // V2.3: set when the previous turn stopped on `ask_user` — the next
  // non-command line is treated as the answer (resume-with-answer)
  // instead of a fresh turn. Cleared whenever a `/` command runs, since
  // none of them are meant to answer a pending question.
  let mut pending_question: Option<String> = None;
  'repl: loop {
    let input = match reader.read_line("› ").await? {
      ReadLine::Line(line) => line,
      // Ctrl-C at the prompt abandons the current line and re-prompts; `exit` /
      // quit / Ctrl-D leave (matches the banner's "Ctrl-D to leave").
      ReadLine::Interrupted => {
        eprintln!();
        continue;
      }
      ReadLine::Eof => {
        eprintln!("\n👋 bye (EOF)");
        break;
      }
    };
    let input = input.trim();
    if input.is_empty() {
      continue;
    }
    if matches!(input, "exit" | "quit") {
      eprintln!("👋 bye");
      break;
    }

    // ── REPL commands (lines starting with `/`) ──
    if let Some(cmd) = input.strip_prefix('/') {
      // A `/` command line was never an answer to a pending question —
      // abandon it rather than leaving stale state.
      pending_question = None;
      let (name, arg) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
      let arg = arg.trim();
      match name {
        "exit" | "quit" | "q" => {
          eprintln!("👋 bye");
          break;
        }
        "help" | "h" | "?" => print_help(),
        "session" => eprintln!("   session: {}", cfg.session_id),
        "model" if !arg.is_empty() => {
          // Commit-on-success (P-A3.6): only mutate `cur_model` / `cur_skill`
          // after the new runtime builds, so a failed switch leaves the prior
          // model + skill intact instead of a dirty half-switched state.
          match build_chat_runtime(&cfg, None, Some(arg)).await {
            Ok((r, m, s)) => {
              cur_skill = None;
              cur_model = Some(arg.to_string());
              runtime = r;
              model = m;
              skill_name = s;
              eprintln!("   ✅ switched to model: {model} (conversation continues)");
            }
            Err(e) => eprintln!("   ⚠️  could not switch model: {e:#} (keeping {model})"),
          }
        }
        "skill" if !arg.is_empty() => {
          if !Path::new(arg).join("SKILL.md").exists()
            && !Path::new(arg).join("skill.toml").exists()
          {
            eprintln!("   ⚠️  no SKILL.md / skill.toml under '{arg}'");
            continue;
          }
          // Commit-on-success (P-A3.6): mutate `cur_skill` only after the build.
          match build_chat_runtime(&cfg, Some(arg), cur_model.as_deref()).await {
            Ok((r, m, s)) => {
              cur_skill = Some(arg.to_string());
              runtime = r;
              model = m;
              skill_name = s;
              eprintln!(
                "   ✅ switched to skill: {} (model: {model})",
                skill_name.as_deref().unwrap_or(arg)
              );
            }
            Err(e) => eprintln!("   ⚠️  could not load skill: {e:#}"),
          }
        }
        "new" | "reset" => {
          // Fresh session id → start a clean conversation. Rebuild so the
          // agent's memory is keyed by the new id; subsequent turns read
          // `cfg.session_id`, which we mutate here.
          cfg.session_id = format!("chat-{}", uuid::Uuid::new_v4().simple());
          match build_chat_runtime(&cfg, cur_skill.as_deref(), cur_model.as_deref()).await {
            Ok((r, m, s)) => {
              runtime = r;
              model = m;
              skill_name = s;
              eprintln!("   ✅ new session: {}", cfg.session_id);
            }
            Err(e) => eprintln!("   ⚠️  could not start new session: {e:#}"),
          }
        }
        "clear" => {
          // Clear the current session's conversation memory *in place* (keep
          // the id), then rebuild so the agent re-reads the now-empty memory.
          // The `--model` path persists to `cfg.memory_db` (SqliteMemory); that
          // is what we clear.
          match clear_session_memory(&cfg.memory_db, &cfg.session_id).await {
            Ok(()) => {
              match build_chat_runtime(&cfg, cur_skill.as_deref(), cur_model.as_deref()).await {
                Ok((r, m, s)) => {
                  runtime = r;
                  model = m;
                  skill_name = s;
                }
                Err(e) => eprintln!("   ⚠️  cleared, but could not rebuild runtime: {e:#}"),
              }
              eprintln!(
                "   ✅ cleared conversation memory for session {} (id kept)",
                cfg.session_id
              );
              if cur_skill.is_some() {
                eprintln!(
                  "   ℹ️  note: a skill that configures its own persistent memory \
                   (memory.type = sqlite) keeps it separately — use /new for a \
                   guaranteed fresh conversation."
                );
              }
            }
            Err(e) => eprintln!("   ⚠️  could not clear memory: {e:#}"),
          }
        }
        "model" | "skill" => eprintln!("   usage: /{name} <value>"),
        other => eprintln!("   unknown command '/{other}' — try /help"),
      }
      continue;
    }

    // ── Otherwise: a message to the agent ──
    let cancel = AgentCancellationToken::new();
    let mut options = HarnessRunOptions::new(input, workspace.clone(), &model)
      .with_runtime_kind(cfg.runtime_kind)
      .with_profile(cfg.profile)
      .with_session_id(cfg.session_id.clone())
      .with_cancellation_token(cancel.clone())
      .with_loop_checkpointer(cfg.loop_checkpointer.clone());
    if let Some(name) = skill_name.as_ref() {
      options = options.with_skill_name(name.clone());
    }
    options = options.with_limits(RuntimeLimits {
      max_steps,
      max_tool_calls: None,
      timeout_ms: None,
      token_budget,
      cost_limit_usd,
    });
    if let Some(budget) = cfg.context_budget {
      options = options.with_context_token_budget(budget);
    }

    // Real-model turns take seconds; show activity (cleared before the
    // answer is printed). Skipped when stderr isn't a TTY (piped/tests).
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if tty {
      use std::io::Write;
      eprint!("⏳ thinking…");
      std::io::stderr().flush().ok();
    }

    // V2.3: if the previous turn paused on `ask_user`, this line is the
    // user's answer — resume the paused loop instead of starting a
    // fresh turn. Otherwise a normal fresh turn.
    let outcome = if let Some(question) = pending_question.take() {
      match cfg.loop_checkpointer.load(&cfg.session_id).await {
        Ok(Some(checkpoint)) => {
          drive_chat_turn(
            runtime.resume_from_interrupt(options, checkpoint, input.to_string()),
            &cancel,
            &mut approval_rx,
            tty,
            &mut reader,
          )
          .await
        }
        Ok(None) => {
          eprintln!(
            "⚠️  no loop checkpoint found for session {} — the paused question ('{question}') \
             can't be answered; starting a fresh turn instead.",
            cfg.session_id
          );
          drive_chat_turn(
            runtime.run(options),
            &cancel,
            &mut approval_rx,
            tty,
            &mut reader,
          )
          .await
        }
        Err(err) => {
          eprintln!("⚠️  failed to load loop checkpoint: {err:#} — starting a fresh turn instead.");
          drive_chat_turn(
            runtime.run(options),
            &cancel,
            &mut approval_rx,
            tty,
            &mut reader,
          )
          .await
        }
      }
    } else {
      drive_chat_turn(
        runtime.run(options),
        &cancel,
        &mut approval_rx,
        tty,
        &mut reader,
      )
      .await
    };

    if tty {
      // Clear the "thinking…" line (CR + erase-to-EOL).
      eprint!("\r\x1b[K");
      use std::io::Write;
      std::io::stderr().flush().ok();
    }

    let result = match outcome {
      ChatTurnOutcome::Cancelled => continue 'repl,
      ChatTurnOutcome::Ready(r) => *r,
    };
    match result {
      Ok(res) => {
        if let AgentStopReason::AwaitingInput { question } = &res.stop_reason {
          eprintln!("\n❓ {question}");
          pending_question = Some(question.clone());
        } else {
          println!("{}", res.answer.as_deref().unwrap_or("(no answer)"));
          if !res.stop_reason.is_success() {
            eprintln!("   (stopped early: {:?})", res.stop_reason);
          }
        }
      }
      Err(err) => eprintln!("⚠️  turn failed: {err:#}"),
    }
    println!();
  }

  Ok(())
}

/// Outcome of one [`drive_chat_turn`] call: either the turn ran to
/// completion (or errored), or SIGINT/SIGTERM cancelled it mid-flight —
/// distinguished so the REPL loop can `continue 'repl` on cancellation
/// without re-checking a sentinel `Result` value.
enum ChatTurnOutcome {
  Ready(Box<Result<yanshi_harness::HarnessRunResult>>),
  Cancelled,
}

/// Drive one Harness turn — a fresh `runtime.run(options)` or a V2.3
/// `runtime.resume_from_interrupt(...)`, both of which produce the same
/// `Result<HarnessRunResult, HarnessError>` future shape — servicing
/// interactive approval requests (H.2.1) as they arrive on `approval_rx`
/// and racing SIGINT/SIGTERM. Generic over the future so both the "fresh
/// turn" and "resume with answer" call sites share the exact same
/// approval-and-cancellation plumbing.
async fn drive_chat_turn(
  fut: impl std::future::Future<Output = Result<yanshi_harness::HarnessRunResult, HarnessError>>,
  cancel: &AgentCancellationToken,
  approval_rx: &mut mpsc::UnboundedReceiver<ApprovalAsk>,
  tty: bool,
  reader: &mut LineReader,
) -> ChatTurnOutcome {
  tokio::pin!(fut);
  loop {
    tokio::select! {
      biased;
      r = &mut fut => return ChatTurnOutcome::Ready(Box::new(r.context("Harness turn failed"))),
      Some((req, resp_tx)) = approval_rx.recv() => {
        if tty {
          use std::io::Write;
          eprint!("\r\x1b[K");
          std::io::stderr().flush().ok();
        }
        let decision = prompt_and_read_decision(&req, reader).await;
        let _ = resp_tx.send(decision);
      }
      _ = crate::shutdown::shutdown_signal() => {
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut fut).await;
        eprintln!("\n🛑 turn cancelled");
        return ChatTurnOutcome::Cancelled;
      }
    }
  }
}

/// Build (or rebuild) the chat's `HarnessRuntime` for the given agent
/// source. Reuses the chat's sink + seq counter + session id so the
/// conversation continues across model/skill switches (the agent reads
/// the same persistent memory keyed by `session_id`).
async fn build_chat_runtime(
  cfg: &ChatConfig,
  skill_dir: Option<&str>,
  model_override: Option<&str>,
) -> Result<(HarnessRuntime, String, Option<String>)> {
  // V2.3: PlanExecute has no mid-loop LLM re-entry point at all, so it
  // cannot implement `TurnDrivenRuntime` — mirrors the same guard in
  // `harness run`.
  if cfg.context_refresh && matches!(cfg.runtime_kind, HarnessRuntimeKind::PlanExecute) {
    anyhow::bail!(
      "--context-refresh is not supported with --runtime plan_execute (PlanExecute has no \
       turn-by-turn re-entry point)"
    );
  }

  let (mut agent, model, skill_name) = build_agent(
    skill_dir,
    model_override,
    &cfg.memory_db,
    &cfg.workspace,
    cfg.runtime_kind,
  )
  .await
  .context("failed to construct the inner Harness agent")?;

  // H.2.1: prefer the REPL-routed approval provider when present (interactive
  // `--approve cli` in chat); otherwise fall back to the mode's default.
  if let Some(provider) = cfg
    .approval_override
    .clone()
    .or_else(|| cfg.approve_mode.provider())
  {
    let hook_config = HookConfig::new(
      cfg.session_id.clone(),
      provider,
      SinkChain::new().push(cfg.jsonl.clone()),
    )
    .with_profile(cfg.profile)
    .with_seq_allocator(cfg.seq_allocator.clone());
    let mut snapshot = ToolRegistry::new();
    for tool in agent.tools().list() {
      snapshot.register(tool);
    }
    agent = agent.with_tools(wrap_registry(snapshot, hook_config));
  }

  // `context_refresh && plan_execute` was already rejected above, so
  // reaching the turn-driven branch means `agent` is always `React`.
  let mut runtime = if cfg.context_refresh {
    let super::run::BuiltHarnessAgent::React(react_agent) = agent else {
      unreachable!("--context-refresh + --runtime plan_execute was rejected above");
    };
    HarnessRuntime::new_turn_driven(react_agent).with_context_refresh()
  } else {
    HarnessRuntime::new(agent.into_runtime_box())
  };
  runtime = runtime
    .with_event_sink(cfg.jsonl.clone())
    // H.3.1: stream live tool/memory progress to stderr (self-gates on a TTY).
    .with_event_sink(Arc::new(ChatProgressSink::new()))
    .with_seq_allocator(cfg.seq_allocator.clone());
  if !cfg.no_default_context {
    runtime = runtime
      .with_context_provider(Arc::new(AgentsMdProvider::new()))
      .with_context_provider(Arc::new(TodosMdProvider::new()))
      .with_context_provider(Arc::new(RoadmapMdProvider::new()))
      .with_context_provider(Arc::new(WorkspaceLayoutProvider::new()));
  }
  if cfg.context_budget.is_some() {
    runtime = runtime.with_context_summarizer(Arc::new(DeterministicContextSummarizer));
  }
  Ok((runtime, model, skill_name))
}

/// Clear the conversation memory for `session_id` in the harness chat SQLite
/// store (the `--model` path's persistent memory), keeping the session id so
/// `/clear` resets continuity in place. A not-yet-created DB is already empty.
async fn clear_session_memory(memory_db: &Path, session_id: &str) -> Result<()> {
  if !memory_db.exists() {
    return Ok(());
  }
  let memory = SqliteMemory::open(memory_db)
    .await
    .with_context(|| format!("could not open chat memory db {}", memory_db.display()))?;
  memory
    .clear_session(session_id)
    .await
    .with_context(|| format!("could not clear session '{session_id}'"))?;
  Ok(())
}

fn print_banner(model: &str, skill: Option<&str>, cfg: &ChatConfig, approve: &str) {
  eprintln!(
    "💬 Harness chat — model: {model}{}",
    skill.map(|s| format!(", skill: {s}")).unwrap_or_default()
  );
  eprintln!(
    "   session: {}  (memory persists; resume later with --session {})",
    cfg.session_id, cfg.session_id
  );
  eprintln!(
    "   profile: {}  ·  approve: {approve}",
    cfg.profile.as_str()
  );
  eprintln!("   type a message, or /help for commands; 'exit' / Ctrl-D to leave.\n");
}

fn print_help() {
  eprintln!("   commands:");
  eprintln!("     /help              show this");
  eprintln!("     /session           show the current session id");
  eprintln!("     /new  (/reset)     start a fresh session (clears continuity)");
  eprintln!("     /clear             clear this session's memory (keep the id)");
  eprintln!("     /model <name>      switch model (conversation continues)");
  eprintln!("     /skill <dir>       switch to a skill (conversation continues)");
  eprintln!("     /exit (/quit)      leave");
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;
  use yanshi_harness::{
    ApprovalRisk, MemorySummaryAddedPayload, SessionStartedPayload, ToolCallCompletedPayload,
    ToolCallRequestedPayload,
  };
  use yanshi_memory::Message;
  use yanshi_tools::ToolIdempotency;

  #[test]
  fn progress_line_renders_tool_and_memory_events() {
    let req = progress_line(&HarnessEventBody::ToolCallRequested(
      ToolCallRequestedPayload {
        step_index: 1,
        tool: "search".into(),
        source: None,
        permissions: vec![],
        idempotency: None,
        params_summary: serde_json::Value::Null,
      },
    ));
    assert_eq!(req.as_deref(), Some("   🔧 search…"));

    let ok = progress_line(&HarnessEventBody::ToolCallCompleted(
      ToolCallCompletedPayload {
        step_index: 1,
        tool: "search".into(),
        is_error: false,
        duration_ms: 12,
        source: None,
        output_summary: None,
      },
    ));
    assert_eq!(ok.as_deref(), Some("   ✓ search (12 ms)"));

    let err = progress_line(&HarnessEventBody::ToolCallCompleted(
      ToolCallCompletedPayload {
        step_index: 2,
        tool: "shell".into(),
        is_error: true,
        duration_ms: 3,
        source: None,
        output_summary: None,
      },
    ));
    assert_eq!(err.as_deref(), Some("   ✗ shell (3 ms)"));

    let mem = progress_line(&HarnessEventBody::MemorySummaryAdded(
      MemorySummaryAddedPayload {
        layer: "context_refresh".into(),
        summary: "x".into(),
        token_estimate: 1,
      },
    ));
    assert_eq!(
      mem.as_deref(),
      Some("   📝 context compacted (context_refresh)")
    );
  }

  #[test]
  fn progress_line_ignores_non_activity_events() {
    // Session lifecycle is not part of the live activity narrative.
    let started = progress_line(&HarnessEventBody::SessionStarted(SessionStartedPayload {
      workspace_root: "/tmp".into(),
      runtime: HarnessRuntimeKind::React,
      profile: HarnessProfile::Local,
      model: "mock".into(),
      skills: vec![],
      context_item_count: 0,
      context_token_estimate: 0,
    }));
    assert_eq!(started, None);
  }

  fn sample_event(body: HarnessEventBody) -> HarnessEvent {
    HarnessEvent::new(0, "sess-1", body)
  }

  #[tokio::test]
  async fn chat_progress_sink_accumulates_token_deltas_and_resets_on_step_boundary() {
    let sink = ChatProgressSink {
      enabled: true,
      typing: std::sync::Mutex::new(String::new()),
    };

    sink
      .write(&sample_event(HarnessEventBody::TokenDelta(
        yanshi_harness::TokenDeltaPayload {
          step_index: 0,
          delta: "Hel".into(),
        },
      )))
      .await
      .unwrap();
    sink
      .write(&sample_event(HarnessEventBody::TokenDelta(
        yanshi_harness::TokenDeltaPayload {
          step_index: 0,
          delta: "lo".into(),
        },
      )))
      .await
      .unwrap();
    assert_eq!(*sink.typing.lock().unwrap(), "Hello");

    sink
      .write(&sample_event(HarnessEventBody::StepStarted(
        yanshi_harness::StepStartedPayload {
          step_index: 1,
          step_type: "plan".into(),
        },
      )))
      .await
      .unwrap();
    assert_eq!(*sink.typing.lock().unwrap(), "");

    sink
      .write(&sample_event(HarnessEventBody::TokenDelta(
        yanshi_harness::TokenDeltaPayload {
          step_index: 1,
          delta: "next".into(),
        },
      )))
      .await
      .unwrap();
    assert_eq!(*sink.typing.lock().unwrap(), "next");

    // A discrete progress-line event also clears the in-flight typing buffer.
    sink
      .write(&sample_event(HarnessEventBody::ToolCallRequested(
        ToolCallRequestedPayload {
          step_index: 1,
          tool: "search".into(),
          source: None,
          permissions: vec![],
          idempotency: None,
          params_summary: serde_json::Value::Null,
        },
      )))
      .await
      .unwrap();
    assert_eq!(*sink.typing.lock().unwrap(), "");
  }

  fn sample_request(id: &str) -> ApprovalRequest {
    ApprovalRequest {
      id: id.into(),
      session_id: "s".into(),
      step_index: 0,
      tool: "shell".into(),
      source: None,
      permissions: vec![],
      idempotency: ToolIdempotency::NonIdempotent,
      params_summary: serde_json::Value::Null,
      risk: ApprovalRisk::High,
      reason: "test".into(),
      requested_at: chrono::Utc::now(),
      expires_at: None,
    }
  }

  #[test]
  fn parse_approval_response_maps_each_key() {
    use ApprovalOutcome::*;
    use ApprovalScope::*;
    assert!(matches!(parse_approval_response("y"), (Allow, Once, _)));
    assert!(matches!(parse_approval_response("yes"), (Allow, Once, _)));
    assert!(matches!(parse_approval_response("s"), (Allow, Session, _)));
    assert!(matches!(parse_approval_response("r"), (Allow, Run, _)));
    assert!(matches!(parse_approval_response("n"), (Deny, Once, _)));
    assert!(matches!(parse_approval_response(""), (Deny, Once, _)));
    assert!(matches!(
      parse_approval_response("q"),
      (DenyAndStop, Once, _)
    ));
    // Unknown input is a fail-closed deny.
    assert!(matches!(
      parse_approval_response("garbage"),
      (Deny, Once, _)
    ));
  }

  /// The provider forwards a request to the REPL channel and returns whatever
  /// decision the REPL sends back.
  #[tokio::test]
  async fn chat_approval_provider_round_trips_a_decision() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ApprovalAsk>();
    let provider = ChatApprovalProvider { tx };
    let responder = tokio::spawn(async move {
      let (req, resp_tx) = rx.recv().await.unwrap();
      let (decision, scope, reason) = parse_approval_response("y");
      resp_tx
        .send(ApprovalDecision {
          request_id: req.id,
          decision,
          scope,
          decided_by: "user".into(),
          decided_at: chrono::Utc::now(),
          reason,
        })
        .unwrap();
    });
    let decided = provider.request(sample_request("req-1")).await.unwrap();
    assert_eq!(decided.request_id, "req-1");
    assert!(matches!(decided.decision, ApprovalOutcome::Allow));
    responder.await.unwrap();
  }

  /// If the REPL is gone (receiver dropped), `request` errors — the hook layer
  /// treats a provider error as a fail-closed deny.
  #[tokio::test]
  async fn chat_approval_provider_errors_when_repl_gone() {
    let (tx, rx) = mpsc::unbounded_channel::<ApprovalAsk>();
    drop(rx);
    let provider = ChatApprovalProvider { tx };
    assert!(provider.request(sample_request("req-2")).await.is_err());
  }

  #[tokio::test]
  async fn clear_session_memory_empties_the_session_but_keeps_others() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("memory.sqlite");
    let memory = SqliteMemory::open(&db).await.unwrap();
    memory.add_message(Message::user("s1", "hi")).await.unwrap();
    memory
      .add_message(Message::user("s2", "keep me"))
      .await
      .unwrap();

    clear_session_memory(&db, "s1").await.unwrap();

    let reopened = SqliteMemory::open(&db).await.unwrap();
    assert!(
      reopened.get_all("s1").await.unwrap().is_empty(),
      "cleared session must be empty"
    );
    assert!(
      !reopened.get_all("s2").await.unwrap().is_empty(),
      "other sessions must be untouched"
    );
  }

  #[tokio::test]
  async fn clear_session_memory_is_ok_when_db_missing() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("does-not-exist.sqlite");
    // A not-yet-created store is already empty — no error.
    clear_session_memory(&db, "s1").await.unwrap();
    assert!(!db.exists(), "must not create the db just to clear it");
  }
}
