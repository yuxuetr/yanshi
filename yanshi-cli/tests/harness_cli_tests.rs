//! End-to-end CLI tests for `yanshi harness …`.
//!
//! `run` against a *real* LLM provider is exercised in
//! `yanshi-harness/tests/runtime_react_smoke.rs`. Here we cover the
//! persistence-side subcommands that operate on a JSONL session log
//! without ever calling out to an LLM (`list`, `inspect`, `resume`, plus
//! argument validation on `run`), plus `run`/`chat`/`resume-loop`
//! end-to-end against the offline `mock` provider — including the V2.3
//! `ask_user` interrupt/resume round trip, which needs a real (if
//! canned) turn-by-turn LLM exchange to exercise honestly.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

const RUNTIME_VERSION: &str = "harness/1";

fn write_session(run_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
  let dir = run_dir.join("harness").join("sessions");
  fs::create_dir_all(&dir).unwrap();
  let path = dir.join(format!("{session_id}.jsonl"));
  let started = json!({
    "seq": 0,
    "session_id": session_id,
    "ts": "2026-05-14T12:00:00Z",
    "kind": "session_started",
    "payload": {
      "workspace_root": "/tmp/ws",
      "runtime": "react",
      "profile": "local",
      "model": "mock-model",
      "skills": [],
      "context_item_count": 0,
      "context_token_estimate": 0
    }
  });
  let stopped = json!({
    "seq": 1,
    "session_id": session_id,
    "ts": "2026-05-14T12:00:05Z",
    "kind": "stopped",
    "payload": {
      "reason": "completed",
      "final_answer": "all done"
    }
  });
  fs::write(
    &path,
    format!(
      "{}\n{}\n",
      serde_json::to_string(&started).unwrap(),
      serde_json::to_string(&stopped).unwrap()
    ),
  )
  .unwrap();
  path
}

#[test]
fn harness_run_requires_model_or_skill() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "run", "hello"]);
  cmd.assert().failure().stderr(predicate::str::contains(
    "either --skill or --model is required",
  ));
}

#[test]
fn harness_chat_accepts_approve_cli() {
  // H.2.1: `--approve cli` is now supported in the chat REPL. The approval
  // prompt reads from the REPL's shared stdin line reader instead of racing
  // it, so the combo is accepted: the REPL starts with `approve: cli` in its
  // banner and exits cleanly on `exit`.
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "chat", "--model", "mock", "--approve", "cli"]);
  cmd.write_stdin("exit\n");
  cmd
    .assert()
    .success()
    .stderr(predicate::str::contains("approve: cli"));
}

// U2.3: `--approve` previously hardcoded `"none"` regardless of
// `--profile` — the same "unsupervised by default" gap T1.3 already
// closed for `workflow dynamic` (see
// `workflow_dynamic_tests.rs::local_profile_without_approve_flag_defaults_to_requiring_cli_approval`).
// `resolve_approve_default`'s resolution logic itself is exhaustively
// unit-tested in `commands::harness::tests`; these two prove the CLI
// wiring reaches it (via the printed banner, which reflects the
// resolved value, not the raw flag).

#[test]
fn harness_chat_defaults_to_cli_approval_under_local_profile_without_approve_flag() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  // Neither --approve nor --profile passed: --profile defaults to
  // "local", which must resolve --approve to "cli".
  cmd.args(["harness", "chat", "--model", "mock"]);
  cmd.write_stdin("exit\n");
  cmd
    .assert()
    .success()
    .stderr(predicate::str::contains("approve: cli"));
}

#[test]
fn harness_chat_stays_unsupervised_under_dev_profile_without_approve_flag() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "chat", "--model", "mock", "--profile", "dev"]);
  cmd.write_stdin("exit\n");
  cmd
    .assert()
    .success()
    .stderr(predicate::str::contains("approve: none"));
}

#[test]
fn harness_chat_requires_model_or_skill() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "chat"]);
  cmd.write_stdin("exit\n");
  cmd.assert().failure().stderr(predicate::str::contains(
    "either --skill or --model is required",
  ));
}

/// End-to-end: the chat REPL reads multiple lines from stdin and runs one
/// Harness turn per line against a single session — proving interactive
/// multi-turn. Uses the offline mock provider (own process → race-free).
#[test]
fn harness_chat_repl_runs_multi_turn_with_mock() {
  let tmp = tempfile::tempdir().unwrap();
  let cfg = tmp.path().join("models.yml");
  std::fs::write(
    &cfg,
    "models:\n  mock-chat: { vendor: mock, type: text, model_id: mock-chat }\n\
     providers:\n  mock: { api_key_env: MOCK_API_KEY }\n",
  )
  .unwrap();
  let run_dir = tmp.path().join("runs");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "chat",
    "--model",
    "mock-chat",
    "--approve",
    "none",
    "--no-default-context",
    "--run-dir",
  ])
  .arg(&run_dir)
  .env("YANSHI_MODELS_CONFIG", &cfg)
  .env("MOCK_API_KEY", "x")
  .env(
    "YANSHI_MOCK_RESPONSES",
    r#"["{\"thought\":\"t1\",\"answer\":\"reply one\"}","{\"thought\":\"t2\",\"answer\":\"reply two\"}"]"#,
  )
  .write_stdin("first message\nsecond message\nexit\n");

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("reply one"))
    .stdout(predicate::str::contains("reply two"));
}

/// The chat REPL recognises `/`-prefixed commands (`/help`, `/session`)
/// and bare `exit` (no command reaches the agent, so no mock response is
/// needed).
#[test]
fn harness_chat_supports_slash_commands() {
  let tmp = tempfile::tempdir().unwrap();
  let cfg = tmp.path().join("models.yml");
  std::fs::write(
    &cfg,
    "models:\n  mock-chat: { vendor: mock, type: text, model_id: mock-chat }\n\
     providers:\n  mock: { api_key_env: MOCK_API_KEY }\n",
  )
  .unwrap();

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd
    .args([
      "harness",
      "chat",
      "--model",
      "mock-chat",
      "--approve",
      "none",
      "--no-default-context",
      "--run-dir",
    ])
    .arg(tmp.path().join("runs"))
    .env("YANSHI_MODELS_CONFIG", &cfg)
    .env("MOCK_API_KEY", "x")
    .write_stdin("/help\n/session\nexit\n");

  cmd
    .assert()
    .success()
    .stderr(predicate::str::contains("/model <name>"))
    .stderr(predicate::str::contains("/skill <dir>"))
    .stderr(predicate::str::contains("session:"));
}

#[test]
fn harness_run_rejects_unknown_approve_mode() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "run",
    "hello",
    "--model",
    "mock-model",
    "--approve",
    "bogus",
  ]);
  // clap value_parser rejects with a non-zero exit and a "possible values" hint.
  cmd
    .assert()
    .failure()
    .stderr(predicate::str::contains("possible values"))
    .stderr(predicate::str::contains("auto-allow"));
}

#[test]
fn harness_run_help_lists_approve_flag() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "run", "--help"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("--approve"))
    .stdout(predicate::str::contains("HookedTool"))
    .stdout(predicate::str::contains("F-A2-11"));
}

#[test]
fn harness_run_help_lists_context_engineering_flags() {
  // The Phase 0/2a context-engineering knobs must be reachable from the
  // production CLI: a real-tokenizer context budget that compacts
  // (not drops) over-budget context, and an agent prompt-memory budget.
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "run", "--help"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("--context-budget"))
    .stdout(predicate::str::contains("compacted"))
    .stdout(predicate::str::contains("--token-budget"))
    // §6: harness-driven turn loop with between-turn context refresh.
    .stdout(predicate::str::contains("--context-refresh"))
    .stdout(predicate::str::contains("context_refresh"));
}

// U1.3: `--cost-limit-usd` is the CLI entry point for the
// `RuntimeLimits::cost_limit_usd` runtime enforcement T1.1 wired into
// `ReActAgent`/`PlanExecuteAgent` — see `yanshi-harness/tests/
// runtime_react_smoke.rs::harness_runtime_stops_react_agent_when_cost_limit_usd_is_exceeded`
// for the end-to-end proof that the flag's value actually reaches and
// trips that runtime check.

#[test]
fn harness_run_help_lists_cost_limit_flag() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "run", "--help"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("--cost-limit-usd"));
}

#[test]
fn harness_chat_help_lists_cost_limit_flag() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "chat", "--help"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("--cost-limit-usd"));
}

#[test]
fn harness_list_text_output_lists_sessions_under_run_dir() {
  let run_dir = TempDir::new().unwrap();
  write_session(run_dir.path(), "sess-list-a");
  write_session(run_dir.path(), "sess-list-b");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "list",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
  ]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("sess-list-a"))
    .stdout(predicate::str::contains("sess-list-b"))
    .stdout(predicate::str::contains("SESSION_ID"));
}

#[test]
fn harness_list_json_output_emits_session_array() {
  let run_dir = TempDir::new().unwrap();
  write_session(run_dir.path(), "sess-json-1");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "list",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
    "--output",
    "json",
  ]);
  let output = cmd.output().unwrap();
  assert!(
    output.status.success(),
    "list --output json failed: {output:?}"
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  let payload: serde_json::Value = serde_json::from_str(&stdout).unwrap();
  let sessions = payload["sessions"].as_array().unwrap();
  assert_eq!(sessions.len(), 1);
  assert_eq!(sessions[0]["session_id"], "sess-json-1");
  assert!(sessions[0]["event_count"].as_u64().unwrap() >= 2);
}

#[test]
fn harness_list_reports_empty_directory_gracefully() {
  let run_dir = TempDir::new().unwrap();
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "list",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
  ]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("no sessions found"));
}

#[test]
fn harness_inspect_text_output_summarises_session() {
  let run_dir = TempDir::new().unwrap();
  write_session(run_dir.path(), "sess-inspect-1");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "inspect",
    "sess-inspect-1",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
  ]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("Session: sess-inspect-1"))
    .stdout(predicate::str::contains("runtime: react"))
    .stdout(predicate::str::contains("session_started: 1"))
    .stdout(predicate::str::contains("stopped: 1"))
    .stdout(predicate::str::contains("final answer: all done"));
}

#[test]
fn harness_inspect_unknown_session_fails_clearly() {
  let run_dir = TempDir::new().unwrap();
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "inspect",
    "ghost",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
  ]);
  cmd
    .assert()
    .failure()
    .stderr(predicate::str::contains("no events found"));
}

#[test]
fn harness_resume_text_output_replays_lines() {
  let run_dir = TempDir::new().unwrap();
  write_session(run_dir.path(), "sess-resume-1");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "resume",
    "sess-resume-1",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
  ]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("Stored events: 2"))
    .stdout(predicate::str::contains("session_started"))
    .stdout(predicate::str::contains("stopped"));
}

#[test]
fn harness_resume_stream_json_emits_per_event_lines() {
  let run_dir = TempDir::new().unwrap();
  write_session(run_dir.path(), "sess-resume-stream");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args([
    "harness",
    "resume",
    "sess-resume-stream",
    "--run-dir",
    run_dir.path().to_str().unwrap(),
    "--output",
    "stream-json",
  ]);
  let output = cmd.output().unwrap();
  assert!(output.status.success());
  let stdout = String::from_utf8(output.stdout).unwrap();
  let lines: Vec<&str> = stdout.lines().collect();
  // Two harness events + one summary trailer.
  assert_eq!(
    lines.len(),
    3,
    "stream-json output should have 3 lines: {stdout}"
  );
  let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
  assert_eq!(first["kind"], "session_started");
  let summary: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
  assert_eq!(summary["type"], "harness_resume_summary");
}

#[test]
fn harness_command_help_is_listed() {
  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd.args(["harness", "--help"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("run"))
    .stdout(predicate::str::contains("resume"))
    .stdout(predicate::str::contains("list"))
    .stdout(predicate::str::contains("inspect"));
  // Sanity: keep schema version referenced anywhere in the help/source
  // so we never accidentally rename the constant without picking it up.
  assert_eq!(RUNTIME_VERSION, "harness/1");
}

// ── P-A2.2: `harness run-flow` (no LLM — a pure template DAG) ────────────────

/// A two-node template workflow runs under harness governance: the command
/// succeeds, reports completion, and persists a JSONL session log with the
/// `session_started` → `step_started`(×2) → `stopped` envelope.
#[test]
fn harness_run_flow_governs_a_template_dag() {
  let tmp = TempDir::new().unwrap();
  let wf = tmp.path().join("flow.yml");
  fs::write(
    &wf,
    r#"
name: "demo"
description: "two template nodes"
nodes:
  - id: prepare
    type: template
    parameters:
      template: "Topic: {{ topic | default(value='Yanshi') }}"
  - id: summarize
    type: template
    dependencies: ["prepare"]
    parameters:
      template: "done"
"#,
  )
  .unwrap();

  let run_dir = tmp.path().join("run");
  Command::cargo_bin("yanshi")
    .unwrap()
    .args([
      "harness",
      "run-flow",
      wf.to_str().unwrap(),
      "--run-dir",
      run_dir.to_str().unwrap(),
      "--output",
      "text",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("flow completed"));

  // The governed event stream was persisted as JSONL.
  let sessions = run_dir.join("harness").join("sessions");
  let log = fs::read_dir(&sessions)
    .unwrap()
    .filter_map(|e| e.ok())
    .map(|e| e.path())
    .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
    .expect("a session jsonl log");
  let body = fs::read_to_string(log).unwrap();
  assert!(body.contains("session_started"));
  assert!(body.contains("stopped"));
  assert_eq!(
    body.matches("step_started").count(),
    2,
    "one step_started per node"
  );
}

/// Eval 2026-10-07 A6: config-built nodes embed their own tools, so
/// `--profile production` governed none of them while appearing to.
/// A production run-flow now refuses tool-running nodes, nested ones
/// included, and a pure template DAG still runs.
#[test]
fn harness_run_flow_production_refuses_ungoverned_tool_nodes() {
  let tmp = TempDir::new().unwrap();
  let marker = tmp.path().join("breach");
  let wf = tmp.path().join("flow.yml");
  fs::write(
    &wf,
    format!(
      r#"
name: "nested-shell"
nodes:
  - id: fan
    type: map
    parameters:
      input_list: [1]
      template:
        - id: touch
          type: shell
          parameters:
            command: "touch {marker}"
            allowed_commands: [touch]
            allowed_paths: ["{dir}"]
"#,
      marker = marker.display(),
      dir = tmp.path().display()
    ),
  )
  .unwrap();

  let run_dir = tmp.path().join("run");
  Command::cargo_bin("yanshi")
    .unwrap()
    .args([
      "harness",
      "run-flow",
      wf.to_str().unwrap(),
      "--profile",
      "production",
      "--run-dir",
      run_dir.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("node 'touch'"))
    .stderr(predicate::str::contains("production"));
  assert!(!marker.exists(), "the shell node must not have run");

  let template = tmp.path().join("template.yml");
  fs::write(
    &template,
    "name: t\nnodes:\n  - id: only\n    type: template\n    parameters:\n      template: done\n",
  )
  .unwrap();
  Command::cargo_bin("yanshi")
    .unwrap()
    .args([
      "harness",
      "run-flow",
      template.to_str().unwrap(),
      "--profile",
      "production",
      "--run-dir",
      run_dir.to_str().unwrap(),
    ])
    .assert()
    .success();
}

// ── V2.3: ask_user interrupt / resume, end to end against `mock` ──────

/// `--model mock` alone resolves nothing — `mock` isn't a built-in
/// registry entry (unlike what a bare `--model mock` flag might
/// suggest), it has to be declared in a `~/.yanshi/models.yml`-shaped
/// config, same as `skill_cli_tests.rs::write_mock_models_config`. Point
/// `HOME` at a tempdir carrying this file so `Yanshi::init()` picks
/// it up without touching the real `~/.yanshi`.
fn write_mock_models_config(home: &std::path::Path) {
  let config_dir = home.join(".yanshi");
  fs::create_dir_all(&config_dir).unwrap();
  fs::write(
    config_dir.join("models.yml"),
    r#"
models:
  mock-model:
    vendor: mock
    type: text
    model_id: mock-model
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#,
  )
  .unwrap();
}

/// Queues for the `mock` LLM provider: first call requests `ask_user`,
/// second call (post-resume) answers plainly with no tool calls — the
/// ordinary ReAct final-answer shape. `YANSHI_MOCK_RESPONSES` and
/// `YANSHI_MOCK_TOOL_CALLS` both advance one entry per LLM call, so
/// both queues need an entry per call even though only one of the two
/// matters each time (mock.rs pops both unconditionally).
fn mock_ask_user_then_final_answer(question: &str, final_answer: &str) -> (String, String) {
  let tool_calls = json!([
    [{ "id": "call_1", "name": "ask_user", "arguments": { "question": question } }],
    [],
  ]);
  let responses = json!(["", final_answer]);
  (tool_calls.to_string(), responses.to_string())
}

/// `harness chat`'s REPL: a turn that pauses on `ask_user` prints the
/// question and treats the *next* line the user types as the answer,
/// resuming through `HarnessRuntime::resume_from_interrupt` instead of
/// starting a fresh turn — this is the inline interactive path from
/// V2.3 step 8.
#[test]
fn harness_chat_pauses_on_ask_user_and_resumes_with_the_next_repl_line() {
  let home = TempDir::new().unwrap();
  write_mock_models_config(home.path());
  let tmp = TempDir::new().unwrap();
  let run_dir = tmp.path().join("run");
  let (tool_calls, responses) =
    mock_ask_user_then_final_answer("Which file should I edit?", "Editing src/main.rs. Done!");

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd
    .args(["harness", "chat", "--model", "mock-model", "--run-dir"])
    .arg(&run_dir)
    .env("HOME", home.path())
    .env("YANSHI_MOCK_TOOL_CALLS", tool_calls)
    .env("YANSHI_MOCK_RESPONSES", responses)
    .write_stdin("please make a change\nsrc/main.rs\nexit\n");

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("Editing src/main.rs. Done!"))
    .stderr(predicate::str::contains("Which file should I edit?"));
}

/// `harness run`'s non-interactive fallback: under `assert_cmd`, stdin
/// and stdout are pipes, never a TTY, so a paused session must print the
/// question and exit 0 (not block forever on a stdin that will never
/// produce a line) — then `resume-loop --answer` picks the session back
/// up from its checkpoint and reaches the same final answer a
/// non-interrupted run would.
#[test]
fn harness_run_prints_question_when_non_interactive_then_resume_loop_answer_continues() {
  let home = TempDir::new().unwrap();
  write_mock_models_config(home.path());
  let tmp = TempDir::new().unwrap();
  let run_dir = tmp.path().join("run");
  let session_id = "v2-3-smoke-session";

  // `harness run` is its own fresh process and makes exactly one LLM
  // call before pausing on `ask_user` — a single-entry queue, not the
  // two-call sequence a single continuous process (like the chat test
  // above) would consume.
  let mut first = Command::cargo_bin("yanshi").unwrap();
  first
    .args([
      "harness",
      "run",
      "please make a change",
      "--model",
      "mock-model",
      "--session",
      session_id,
      "--run-dir",
    ])
    .arg(&run_dir)
    .env("HOME", home.path())
    .env(
      "YANSHI_MOCK_TOOL_CALLS",
      json!([[
        { "id": "call_1", "name": "ask_user", "arguments": { "question": "Which file should I edit?" } },
      ]])
      .to_string(),
    )
    .env("YANSHI_MOCK_RESPONSES", json!([""]).to_string());
  first.assert().success().stderr(
    predicate::str::contains("awaiting input")
      .and(predicate::str::contains("resume-loop"))
      .and(predicate::str::contains("Which file should I edit?")),
  );

  // `resume-loop` is a fresh process — its `mock` provider queues start
  // over at index 0, and this run makes exactly one more LLM call (the
  // continuation after the answer is injected into memory), so the
  // queues here hold only that one entry, not the original two-call
  // sequence `first` consumed.
  let mut resume = Command::cargo_bin("yanshi").unwrap();
  resume
    .args([
      "harness",
      "resume-loop",
      session_id,
      "--model",
      "mock-model",
      "--run-dir",
    ])
    .arg(&run_dir)
    .arg("--answer")
    .arg("src/main.rs")
    .env("HOME", home.path())
    .env("YANSHI_MOCK_TOOL_CALLS", json!([[]]).to_string())
    .env(
      "YANSHI_MOCK_RESPONSES",
      json!(["Editing src/main.rs. Done!"]).to_string(),
    );
  resume
    .assert()
    .success()
    .stdout(predicate::str::contains("Editing src/main.rs. Done!"));
}

/// W0.1 regression: `harness run --model` (no `--skill`) used to build the
/// agent around an always-empty `ToolRegistry::new()`, so any tool call
/// the model attempted would fail with "tool not found" regardless of
/// what was on disk. Drives a real `file` `read` tool call through the
/// default registry and asserts the content it read back reaches the
/// final answer — proof the registry is populated, not empty.
#[test]
fn harness_run_without_skill_exercises_the_default_file_tool() {
  let home = TempDir::new().unwrap();
  write_mock_models_config(home.path());
  let tmp = TempDir::new().unwrap();
  let run_dir = tmp.path().join("run");
  let workspace = tmp.path().join("workspace");
  fs::create_dir_all(&workspace).unwrap();
  fs::write(workspace.join("hello.txt"), "hi from disk").unwrap();

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd
    .args([
      "harness",
      "run",
      "read hello.txt",
      "--model",
      "mock-model",
      "--workspace",
    ])
    .arg(&workspace)
    .arg("--run-dir")
    .arg(&run_dir)
    .arg("--approve")
    .arg("none")
    .env("HOME", home.path())
    .env(
      "YANSHI_MOCK_TOOL_CALLS",
      json!([
        [{ "id": "call_1", "name": "file", "arguments": { "operation": "read", "path": workspace.join("hello.txt") } }],
        [],
      ])
      .to_string(),
    )
    .env(
      "YANSHI_MOCK_RESPONSES",
      json!([
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"file said: hi from disk"}"#,
      ])
      .to_string(),
    );

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("file said: hi from disk"));
}

/// W0.6 regression: `harness run`'s hook layer used to build its own
/// independent `AtomicU64::new(0)` seq counter instead of sharing the
/// runtime's, so the same session JSONL log ended up with two series
/// both starting at 0 — colliding `(session_id, seq)` pairs the moment
/// both an approval event and a runtime event landed in one session.
/// `--profile production` escalates the `http` tool's `POST` method
/// (classified `NonIdempotent`) to require approval, so this run
/// produces both `approval_requested`/`approval_decided` (hook layer)
/// and `tool_call_requested`/`tool_call_completed` (runtime) events in
/// the same log — exactly the cross-source scenario the pre-fix code
/// never exercised (existing unit tests only covered a hand-rolled
/// shared-seq case, not this real dual-emitter path). Targets a
/// loopback URL the default `SandboxPolicy` denies by default, so the
/// eventual `http` execute() fails fast with no real network call —
/// irrelevant here since approval fires before that regardless of the
/// outcome.
#[test]
fn harness_run_shares_one_seq_series_between_hook_and_runtime_events() {
  let home = TempDir::new().unwrap();
  write_mock_models_config(home.path());
  let tmp = TempDir::new().unwrap();
  let run_dir = tmp.path().join("run");
  let workspace = tmp.path().join("workspace");
  fs::create_dir_all(&workspace).unwrap();
  let session_id = "w0-6-seq-session";

  let mut cmd = Command::cargo_bin("yanshi").unwrap();
  cmd
    .args([
      "harness",
      "run",
      "post something",
      "--model",
      "mock-model",
      "--workspace",
    ])
    .arg(&workspace)
    .arg("--run-dir")
    .arg(&run_dir)
    .arg("--session")
    .arg(session_id)
    .arg("--profile")
    .arg("production")
    .arg("--approve")
    .arg("auto-allow")
    .env("HOME", home.path())
    .env(
      "YANSHI_MOCK_TOOL_CALLS",
      json!([
        [{ "id": "call_1", "name": "http", "arguments": { "url": "http://127.0.0.1:1/", "method": "POST", "body": "x" } }],
        [],
      ])
      .to_string(),
    )
    .env(
      "YANSHI_MOCK_RESPONSES",
      json!([
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"done"}"#,
      ])
      .to_string(),
    );

  // The POST is denied by the sandbox policy (loopback network access
  // is off by default) regardless of approval — irrelevant to this
  // test, which only cares that both event sources fired into one
  // shared, collision-free seq series.
  let _ = cmd.assert();

  let log_path = run_dir
    .join("harness")
    .join("sessions")
    .join(format!("{session_id}.jsonl"));
  let body = fs::read_to_string(&log_path)
    .unwrap_or_else(|e| panic!("failed to read session log at {}: {e}", log_path.display()));
  let mut seqs = Vec::new();
  let mut kinds = Vec::new();
  for line in body.lines().filter(|l| !l.trim().is_empty()) {
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    seqs.push(v["seq"].as_u64().expect("seq field present"));
    kinds.push(v["kind"].as_str().unwrap().to_string());
  }

  assert!(
    kinds.contains(&"approval_requested".to_string()),
    "expected a hook-layer approval_requested event; kinds: {kinds:?}"
  );
  assert!(
    kinds.contains(&"approval_decided".to_string()),
    "expected a hook-layer approval_decided event; kinds: {kinds:?}"
  );
  assert!(
    kinds.contains(&"tool_call_requested".to_string())
      || kinds.contains(&"tool_call_completed".to_string()),
    "expected a runtime-layer tool call event; kinds: {kinds:?}"
  );

  let mut deduped = seqs.clone();
  deduped.sort_unstable();
  deduped.dedup();
  assert_eq!(
    deduped.len(),
    seqs.len(),
    "hook and runtime events must share one seq series with no collisions; got {seqs:?} (kinds: {kinds:?})"
  );
  let mut sorted = seqs.clone();
  sorted.sort_unstable();
  assert_eq!(
    seqs, sorted,
    "seq must be monotonically increasing in file-write order"
  );
}
