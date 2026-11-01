#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

//! `code-reviewer-write` — A2 follow-up validating Harness Mode
//! approval gate end-to-end with a real write-side tool call.
//!
//! ## Why this exists
//!
//! A2's original spec called for a code reviewer that BOTH reads PR
//! diffs AND posts review comments back to GitHub, with the write
//! side gated by Harness Mode's approval flow (`yanshi-harness`'s
//! P-H.2 `HookedTool` + `ApprovalProvider`). The original A2
//! [code-reviewer skill](../code-reviewer/skill.toml) covered only
//! the read side because the write side requires the approval gate.
//!
//! ## Why not `yanshi harness run --skill`?
//!
//! Investigation during this work surfaced a real yanshi gap
//! (**F-A2-9** in EXAMPLES_TODOs.md → **F-A2-11** in dogfooding):
//! `yanshi harness run` CLI builds the agent via `SkillBuilder::build`
//! but does **NOT** call `wrap_registry(...)` to install the
//! `HookedTool` + `ApprovalProvider` pipeline. Only `yanshi-server`'s
//! `LiveHarnessExecutor` wires it. So `harness run` from CLI today
//! gives you the agent + sinks but skips the approval-gate plumbing.
//!
//! Rather than block on a CLI fix, this binary wires the pipeline
//! manually so we can dogfood the approval flow end-to-end. The
//! resulting code is essentially a reduced form of what
//! `yanshi harness run` SHOULD do for skills with write tools —
//! a candidate to promote to first-class CLI support later.
//!
//! ## What the binary does
//!
//! 1. Build a `ToolRegistry` with `ShellTool` (git only) +
//!   `FileTool` (paths under /tmp/).
//! 2. `wrap_registry(registry, HookConfig::new(... CliApprovalProvider
//!   ::stdin() ...))` — every tool call now flows through the approval
//!   gate before execution.
//! 3. Build a `ReActAgent` with a tight persona that:
//!   a. Runs `git show <commit>` (shell → triggers approval prompt #1).
//!   b. Analyses the diff in LLM-side.
//!   c. Writes a single JSON ledger entry to /tmp/pr-review-ledger.json
//!      (file:write → triggers approval prompt #2).
//!   d. Reports a final answer summarising what got written.
//! 4. The operator gets 2 prompts on stdin; on each, they can:
//!   - Allow once / session / run scope
//!   - Deny once / deny+stop
//!
//! When approved, the file content lands at /tmp/pr-review-ledger.json
//! with the agent's findings. When denied, the tool call short-circuits
//! and the agent reports the denial.

use std::path::PathBuf;
use std::sync::Arc;

use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_agents::runtime::RuntimeLimits;
use yanshi_harness::{
  AutoAllowApprovalProvider, CliApprovalProvider, HarnessProfile, HookConfig, SinkChain,
  StdoutEventSink, wrap_registry,
};
use yanshi_llm::Yanshi as LlmInit;
use yanshi_memory::SessionMemory;
use yanshi_tools::ToolRegistry;
use yanshi_tools::builtin::{FileTool, ShellTool};
use yanshi_tools::sandbox::SandboxPolicy;
use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::EnvFilter;

// Persona template with two literal slots: {COMMIT} and {LEDGER}.
// We render at runtime instead of asking the model to parse the user
// prompt — F-A2-13: moonshot-v1-128k repeatedly hallucinated random
// hashes (`abc1234`, `4b4ab6cd0f`) on iteration 1 when the commit
// only appeared in the user prompt, despite "verbatim" instructions.
// Inlining the literal hash into the persona is the deterministic
// fix.
const PERSONA_TEMPLATE: &str = r##"你是 release 工程师的 code-review 助手，针对一个具体的 git commit。

**Run-specific facts (must be used verbatim, do NOT alter)**:
- commit: `{COMMIT}`
- ledger path: `{LEDGER}`

**严格的工具调用脚本（exactly 2 tool calls，按顺序，不可重试）**:

step 1 (恰好一次 shell): 执行字面命令 `git show {COMMIT}`，一字不差。
   - 不要换 hash、不要扩展长度、不要追加任何 git flag。
   - 这一次会触发 approval 请求，approve 后 observation 就是 diff。
   - **shell 全程只调用这一次**。再次 git show 是错误行为，会浪费 budget。
   - **CRITICAL：如果上一条 observation 已经是 `git show` 的输出（含
     "commit "/"Author:"/"Date:" 字样），那么你已经完成 step 1，必须
     立刻进入 step 2 调用 file:write，绝对不可再次调用 shell。**

step 2 (恰好一次 file:write)：把整份 review 序列化成 JSON 对象写到字面
   路径 `{LEDGER}`。JSON 字段：commit (string, 值固定为 `{COMMIT}`),
   reviewed_at (ISO-8601 timestamp), findings (array of {file, line,
   severity in 'critical'/'important'/'minor', title, suggestion},
   长度 0~5), verdict ('approve' | 'approve_with_comments' |
   'request_changes')。
   - 这一次会再触发一次 approval —— 这是 write-side gate 的正常工作方式，
     **不要用 shell `cat >` / `echo >` 绕过**（shell 已经用过一次了）。
   - findings 来自你对 step 1 diff 的 LLM-side 分析，不要再调任何工具
     取额外信息。没有发现就交空数组 + verdict='approve'。

step 3 final_answer：纯文字一句话汇报 commit、findings 数量、verdict、
   ledger path。不再调用任何工具，不要包成 JSON envelope。

**异常处理**：
- 任一 tool call 被 deny：把 deny reason 直接转给用户做 final_answer，
  不要重试任何 tool call。
- step 1 的 shell 报错（例如 commit 不存在）：把错误信息原样转给用户做
  final_answer，**不要换 hash 重试**。
"##;

fn render_persona(commit: &str, ledger: &std::path::Path) -> String {
  PERSONA_TEMPLATE
    .replace("{COMMIT}", commit)
    .replace("{LEDGER}", &ledger.display().to_string())
}

// Persona for --prefetch-diff mode: the diff is already in the user
// prompt, so the agent has exactly one tool call (file:write) to do.
const PREFETCH_PERSONA_TEMPLATE: &str = r##"你是 release 工程师的 code-review 助手。用户已经把 commit `{COMMIT}`
的 diff 完整放进了 user prompt。你的工作只有一步：

**唯一允许的 tool call**: file:write，把整份 review 序列化成 JSON
对象写到字面路径 `{LEDGER}`。

JSON 字段：commit (string, 值固定为 `{COMMIT}`), reviewed_at (ISO-8601
timestamp), findings (array of {file, line, severity in 'critical'/
'important'/'minor', title, suggestion}, 长度 0~5), verdict
('approve' | 'approve_with_comments' | 'request_changes')。

- findings 来自你对 prompt 里 diff 的 LLM-side 分析；没有发现就交空
  数组 + verdict='approve'。**不要再调用任何 shell 或额外工具**。
- file:write 会触发一次 approval 请求 —— 这是 write-side gate 的正常
  工作方式，不要尝试用任何方式绕过。
- file:write 成功后，final_answer 用纯文字一句话汇报 commit、
  findings 数量、verdict、ledger path。不再调用任何工具。

如果 file:write 被 deny，把 deny reason 转给用户做 final_answer，
不要重试。
"##;

fn render_prefetch_persona(commit: &str, ledger: &std::path::Path) -> String {
  PREFETCH_PERSONA_TEMPLATE
    .replace("{COMMIT}", commit)
    .replace("{LEDGER}", &ledger.display().to_string())
}

fn load_yanshi_dotenv() {
  if let Some(home) = std::env::home_dir() {
    let _ = dotenvy::from_path(home.join(".yanshi").join(".env"));
  }
}

#[derive(Debug)]
struct Args {
  commit: String,
  ledger: PathBuf,
  model: String,
  auto_approve: bool,
  /// Run `git show <commit>` outside the agent and inline the diff
  /// into the user prompt; register only FileTool. Isolates the
  /// file:write approval path from F-A2-13's shell-loop pathology.
  prefetch_diff: bool,
}

fn parse_args() -> Result<Args> {
  let mut commit: Option<String> = None;
  let mut ledger: Option<PathBuf> = None;
  let mut model: String = "kimi-k2.6".to_string();
  let mut auto_approve = false;
  let mut prefetch_diff = false;
  let mut it = std::env::args().skip(1);
  while let Some(flag) = it.next() {
    match flag.as_str() {
      "--commit" => commit = Some(it.next().context("--commit expects a value")?),
      "--ledger" => ledger = Some(it.next().context("--ledger expects a path")?.into()),
      "--model" => model = it.next().context("--model expects a model name")?,
      "--auto-approve" => auto_approve = true,
      "--prefetch-diff" => prefetch_diff = true,
      "-h" | "--help" => {
        print_help();
        std::process::exit(0);
      }
      other => anyhow::bail!("unknown flag `{other}`"),
    }
  }
  Ok(Args {
    commit: commit.context("--commit is required")?,
    ledger: ledger.unwrap_or_else(|| PathBuf::from("/tmp/pr-review-ledger.json")),
    model,
    auto_approve,
    prefetch_diff,
  })
}

fn print_help() {
  println!(
    "code-reviewer-write — A2 follow-up Harness approval validation\n\
     \n\
     USAGE:\n  \
       code-reviewer-write --commit <git-ref> [--ledger <path>] [--model <name>] [--auto-approve]\n\
     \n\
     FLAGS:\n  \
       --commit <ref>      git commit / ref to review (required)\n  \
       --ledger <path>     where the review JSON gets written (default: /tmp/pr-review-ledger.json)\n  \
       --model <name>      LLM model (default: kimi-k2.6)\n  \
       --auto-approve      bypass interactive approval (CI smoke; defaults to interactive CLI prompt)\n  \
       --prefetch-diff     run `git show` outside the agent, inline the diff into the prompt,\n  \
                           and register only FileTool. Isolates the file:write approval path\n  \
                           from F-A2-13's shell-loop pathology so the happy path is reliably\n  \
                           reached on moonshot-v1-128k.\n  \
       -h, --help          show this help\n\
     \n\
     ENV:\n  \
       MOONSHOT_API_KEY   required by default model; auto-loaded from ~/.yanshi/.env\n"
  );
}

#[tokio::main]
async fn main() -> Result<()> {
  load_yanshi_dotenv();
  // Approval prompts go to stderr through tracing; keep stdout for the
  // final answer + JSON outputs so callers can pipe.
  tracing_subscriber::fmt()
    .with_writer(std::io::stderr)
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
    .init();

  let args = parse_args()?;
  info!(?args, "starting code-reviewer-write");

  LlmInit::init()
    .await
    .context("failed to initialise yanshi-llm")?;

  // Pre-fetch the diff outside the agent if requested. This bypasses
  // step 1 (the shell call) entirely and isolates the file:write
  // approval path. Useful for moonshot-v1-128k which loops on
  // identical shell calls (F-A2-13).
  let prefetched_diff = if args.prefetch_diff {
    let out = std::process::Command::new("git")
      .arg("show")
      .arg(&args.commit)
      .output()
      .context("failed to spawn `git show` for --prefetch-diff")?;
    if !out.status.success() {
      anyhow::bail!(
        "`git show {}` exited {}: {}",
        args.commit,
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
      );
    }
    let diff = String::from_utf8(out.stdout).context("git show output is not UTF-8")?;
    info!(diff_bytes = diff.len(), "pre-fetched diff outside agent");
    Some(diff)
  } else {
    None
  };

  // ── 1. Build the unprotected tool registry ─────────────────────────
  let policy = Arc::new(SandboxPolicy {
    allowed_commands: vec!["git".to_string()],
    allowed_paths: vec![PathBuf::from("/tmp")],
    ..SandboxPolicy::default()
  });
  let mut registry = ToolRegistry::new();
  if !args.prefetch_diff {
    registry.register(Arc::new(ShellTool::new(policy.clone())));
  }
  registry.register(Arc::new(FileTool::new(policy.clone())));

  // ── 2. Wrap with HookedTool + CliApprovalProvider ──────────────────
  let session_id = format!(
    "review-{}",
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or(0)
  );
  let sinks = SinkChain::new().push(Arc::new(StdoutEventSink::new()));
  let approval: Arc<dyn yanshi_harness::ApprovalProvider> = if args.auto_approve {
    info!("--auto-approve set: bypassing interactive approval");
    Arc::new(AutoAllowApprovalProvider::new())
  } else {
    Arc::new(CliApprovalProvider::stdin())
  };
  // Use Production profile so HookedTool escalates every NonIdempotent
  // call to RequireApproval. Default (Local) profile only fires approval
  // when a pre-hook explicitly demands it; since we have no pre-hooks,
  // Local would auto-allow every call and the approval prompt would
  // never appear. This is finding F-A2-12 — H2 design choice: approval
  // gate is opt-in via profile or pre-hook, not on-by-default for any
  // NonIdempotent tool.
  let hook_config =
    HookConfig::new(session_id.clone(), approval, sinks).with_profile(HarnessProfile::Production);
  let wrapped_registry = wrap_registry(registry, hook_config);

  // ── 3. Build ReActAgent with the wrapped registry ───────────────────
  let persona = if args.prefetch_diff {
    render_prefetch_persona(&args.commit, &args.ledger)
  } else {
    render_persona(&args.commit, &args.ledger)
  };
  let memory = Box::new(SessionMemory::default_window());
  let config = ReActConfig::new(&args.model)
    .with_persona(&persona)
    .with_max_iterations(4)
    .with_budget_tokens(60_000);
  let mut agent =
    ReActAgent::new(config, memory, wrapped_registry).with_session_id(session_id);

  // ── 4. Drive the agent ─────────────────────────────────────────────
  let prompt = if let Some(diff) = &prefetched_diff {
    format!(
      "Pre-fetched diff for commit {commit} is below. Analyse it and \
       write the structured review JSON to {ledger}. You have exactly \
       one tool call: file:write. Do not attempt shell.\n\n\
       ===== BEGIN DIFF =====\n{diff}\n===== END DIFF =====",
      commit = args.commit,
      ledger = args.ledger.display(),
    )
  } else {
    format!(
      "Please review commit {commit} per the script and write the ledger to {ledger}.",
      commit = args.commit,
      ledger = args.ledger.display()
    )
  };
  info!(prompt_bytes = prompt.len(), "agent input");

  let max_tool_calls = if args.prefetch_diff { 1 } else { 4 };
  let context =
    yanshi_agents::runtime::AgentContext::new(agent.session_id.clone(), &prompt, &args.model)
      .with_persona(&persona)
      .with_limits(RuntimeLimits {
        // Happy path: 2 (shell + file:write) in non-prefetch mode; 1
        // (file:write only) in prefetch mode. Soft ceiling at 4 in
        // non-prefetch to give moonshot some slack if it loops on
        // shell (F-A2-13).
        max_steps: Some(6),
        max_tool_calls: Some(max_tool_calls),
        timeout_ms: None,
        token_budget: Some(60_000),
        ..Default::default()
      });
  let result = agent
    .run_with_context(context)
    .await
    .context("agent run failed")?;

  eprintln!();
  eprintln!("=== Agent finished ===");
  eprintln!("Session: {}", result.session_id);
  eprintln!("Stop reason: {:?}", result.stop_reason);
  if let Some(answer) = &result.answer {
    println!("{answer}");
  } else {
    println!("(no answer)");
  }

  // Sanity check: was the ledger actually written?
  if args.ledger.exists() {
    eprintln!("\n✅ Ledger written: {}", args.ledger.display());
  } else {
    eprintln!(
      "\n⚠️  Ledger NOT written at {} (approval denied?)",
      args.ledger.display()
    );
  }

  Ok(())
}
