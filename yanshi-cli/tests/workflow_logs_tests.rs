//! Hermetic round-trip coverage for `yanshi workflow logs <run_id>`
//! (P10.11.1).
//!
//! Spins up a tiny axum mock server that implements the two routes the
//! CLI talks to:
//! - `GET /v1/runs/{id}/events/history` returns a canned JSON array.
//! - `GET /v1/runs/{id}/events` returns a finite SSE stream that closes
//!   itself after a couple of events so the CLI's follow loop exits.
//!
//! No Postgres required — this stays hermetic so workspace
//! `cargo test` runs it on every dev machine.

use std::time::Duration;

use assert_cmd::Command;
use axum::{
  Json, Router,
  extract::Path,
  response::sse::{Event, KeepAlive, Sse},
  routing::get,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn cli_bin() -> Command {
  Command::cargo_bin("yanshi").expect("yanshi binary built")
}

/// Canned 3-event history that `/v1/runs/<id>/events/history` returns.
/// Wire shape mirrors `yanshi_server::events_stream::StreamedEvent`
/// so the CLI's parser exercises the same JSON the real server emits.
fn canned_history() -> Vec<Value> {
  vec![
    json!({
      "run_id": "00000000-0000-0000-0000-000000000001",
      "seq": 0,
      "kind": "run_started",
      "payload": { "workflow_name": "demo" },
      "ts": "2026-05-20T10:00:00Z",
    }),
    json!({
      "run_id": "00000000-0000-0000-0000-000000000001",
      "seq": 1,
      "kind": "step_started",
      "payload": { "node_id": "render" },
      "ts": "2026-05-20T10:00:01Z",
    }),
    json!({
      "run_id": "00000000-0000-0000-0000-000000000001",
      "seq": 2,
      "kind": "workflow.completed",
      "payload": { "status": "succeeded" },
      "ts": "2026-05-20T10:00:02Z",
    }),
  ]
}

async fn history_handler(Path(_run_id): Path<String>) -> Json<Vec<Value>> {
  Json(canned_history())
}

/// SSE stream that emits the same 3 events, then closes. Closing the
/// stream is what makes the CLI follow loop terminate — without it
/// the test would hang forever waiting for the next event.
async fn events_sse_handler(
  Path(_run_id): Path<String>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
  use futures::stream;
  let events = canned_history()
    .into_iter()
    .map(|v| Ok(Event::default().data(v.to_string())))
    .collect::<Vec<_>>();
  // `stream::iter` drains its source then closes the stream — that's
  // the signal the CLI uses to break out of its follow loop.
  Sse::new(stream::iter(events)).keep_alive(
    KeepAlive::new()
      .interval(Duration::from_secs(60))
      .text("keep-alive"),
  )
}

/// Spawn the mock server on an ephemeral port. Returns the base URL.
async fn spawn_mock_server() -> String {
  let router = Router::new()
    .route("/v1/runs/:id/events/history", get(history_handler))
    .route("/v1/runs/:id/events", get(events_sse_handler));
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("local addr");
  tokio::spawn(async move {
    let _ = axum::serve(listener, router.into_make_service()).await;
  });
  // Brief settle so the OS finishes the listen state before the CLI
  // subprocess tries to connect. 80 ms mirrors the existing P2.5
  // test infra cadence.
  tokio::time::sleep(Duration::from_millis(80)).await;
  format!("http://{addr}")
}

#[tokio::test]
async fn cli_workflow_logs_history_renders_text_lines() {
  let url = spawn_mock_server().await;
  let run_id = "00000000-0000-0000-0000-000000000001";

  let url_for_cli = url.clone();
  let assert = tokio::task::spawn_blocking(move || {
    cli_bin()
      .args([
        "workflow",
        "logs",
        run_id,
        "--server",
        &url_for_cli,
        "--format",
        "text",
      ])
      .assert()
      .success()
  })
  .await
  .expect("join");

  let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
  // text format: one line per event, format `[seq] kind ts payload`.
  // All 3 canned event kinds + their seqs must surface.
  assert!(
    stdout.contains("[0] run_started"),
    "missing run_started: {stdout}"
  );
  assert!(
    stdout.contains("[1] step_started"),
    "missing step_started: {stdout}"
  );
  assert!(
    stdout.contains("[2] workflow.completed"),
    "missing workflow.completed: {stdout}"
  );
  // The payload must render in compact JSON form.
  assert!(stdout.contains(r#"{"workflow_name":"demo"}"#));
  assert!(stdout.contains(r#"{"node_id":"render"}"#));
}

#[tokio::test]
async fn cli_workflow_logs_history_renders_jsonl() {
  let url = spawn_mock_server().await;
  let run_id = "00000000-0000-0000-0000-000000000001";

  let url_for_cli = url.clone();
  let assert = tokio::task::spawn_blocking(move || {
    cli_bin()
      .args([
        "workflow",
        "logs",
        run_id,
        "--server",
        &url_for_cli,
        "--format",
        "json",
      ])
      .assert()
      .success()
  })
  .await
  .expect("join");

  let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
  // JSONL: each line is a complete, parseable JSON object. Locking
  // this shape catches accidental pretty-printing or `[ ... ]` array
  // wrapping that would break `jq -c` pipelines.
  let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
  assert_eq!(lines.len(), 3, "expected 3 JSONL lines, got: {stdout}");
  for line in lines {
    let parsed: Value =
      serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON line `{line}`: {e}"));
    assert!(parsed.get("seq").is_some());
    assert!(parsed.get("kind").is_some());
  }
}

#[tokio::test]
async fn cli_workflow_logs_history_envelope_wraps_array() {
  let url = spawn_mock_server().await;
  let run_id = "00000000-0000-0000-0000-000000000001";

  let url_for_cli = url.clone();
  let assert = tokio::task::spawn_blocking(move || {
    cli_bin()
      .args([
        "workflow",
        "logs",
        run_id,
        "--server",
        &url_for_cli,
        "--format",
        "json-envelope",
      ])
      .assert()
      .success()
  })
  .await
  .expect("join");

  let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
  let parsed: Value = serde_json::from_str(&stdout).expect("envelope must be valid JSON");
  // Canonical envelope shape (see yanshi-cli/src/json_envelope.rs):
  // `{ version: "yanshi.cli/1", command, result, errors: [] }`.
  // The events array lands under `result`; success means `errors`
  // is an empty list.
  assert_eq!(parsed["version"], "yanshi.cli/1");
  assert_eq!(parsed["command"], "workflow logs");
  let errors = parsed["errors"].as_array().expect("errors is array");
  assert!(errors.is_empty(), "successful run must have no errors");
  let events = parsed["result"].as_array().expect("result is array");
  assert_eq!(events.len(), 3);
}

#[tokio::test]
async fn cli_workflow_logs_follow_streams_and_exits_when_server_closes() {
  let url = spawn_mock_server().await;
  let run_id = "00000000-0000-0000-0000-000000000001";

  let url_for_cli = url.clone();
  // Wrap in a timeout to surface a hung follow loop as a test
  // failure rather than a CI deadlock. The mock SSE stream closes
  // itself after 3 events so this should finish in well under a
  // second.
  let assert = tokio::time::timeout(
    Duration::from_secs(10),
    tokio::task::spawn_blocking(move || {
      cli_bin()
        .args([
          "workflow",
          "logs",
          run_id,
          "--server",
          &url_for_cli,
          "--follow",
          "--format",
          "json",
        ])
        .assert()
        .success()
    }),
  )
  .await
  .expect("CLI must exit when the SSE server closes the stream")
  .expect("join");

  let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
  let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
  assert_eq!(
    lines.len(),
    3,
    "follow mode must surface all 3 streamed events as JSONL, got: {stdout}"
  );
  for line in lines {
    let parsed: Value = serde_json::from_str(line).expect("each follow line is JSON");
    assert!(parsed.get("kind").is_some());
  }
}

/// Q3.5.3 regression — `workflow logs --follow` must transparently
/// reconnect after a mid-stream TCP drop instead of bailing out with
/// a hard error. The mock SSE handler tracks attempts: the FIRST
/// request emits events 0+1 and then closes the stream early to
/// simulate a network blip; the SECOND request (sent with
/// `?after_seq=1` by the reconnect path) emits events 2+3 and closes
/// cleanly. The CLI must surface all 4 events without duplicates and
/// exit 0.
#[tokio::test]
async fn cli_workflow_logs_follow_reconnects_after_mid_stream_drop() {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicU32, Ordering};

  let run_id = "00000000-0000-0000-0000-0000000000aa";
  let attempts = Arc::new(AtomicU32::new(0));

  async fn history_handler(Path(_run_id): Path<String>) -> Json<Vec<Value>> {
    Json(vec![])
  }

  // Two-phase SSE handler: attempt #1 returns events 0+1 then closes;
  // attempt #2 returns events 2+3 then closes. The `after_seq` query
  // param the CLI sends on reconnect controls which slice is sent —
  // a misbehaving consumer that didn't bump after_seq would re-fetch
  // 0+1 and the assertion below would catch the duplicate.
  async fn flaky_sse_handler(
    Path(_run_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State(attempts): axum::extract::State<Arc<AtomicU32>>,
  ) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures::stream;
    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
    let after_seq: Option<i64> = params.get("after_seq").and_then(|v| v.parse().ok());
    let slice = if attempt == 0 {
      // First attempt — emit seqs 0+1, then close (simulates drop).
      vec![
        json!({"seq": 0, "kind": "run_started", "payload": {}, "ts": "2026-05-20T10:00:00Z"}),
        json!({"seq": 1, "kind": "step_started", "payload": {}, "ts": "2026-05-20T10:00:01Z"}),
      ]
    } else {
      // Reconnect — CLI must pass `after_seq = 1` (the last seq it
      // observed). Filter accordingly so a buggy "always resend
      // everything" client would surface as duplicates the assert
      // catches below.
      let start = after_seq.map(|s| s + 1).unwrap_or(0);
      let later = vec![
        json!({"seq": 2, "kind": "step_completed", "payload": {}, "ts": "2026-05-20T10:00:02Z"}),
        json!({"seq": 3, "kind": "workflow.completed", "payload": {"status": "succeeded"}, "ts": "2026-05-20T10:00:03Z"}),
      ];
      later
        .into_iter()
        .filter(|e| e["seq"].as_i64().unwrap() >= start)
        .collect()
    };
    let events: Vec<_> = slice
      .into_iter()
      .map(|v| Ok(Event::default().data(v.to_string())))
      .collect();
    Sse::new(stream::iter(events)).keep_alive(
      KeepAlive::new()
        .interval(Duration::from_secs(60))
        .text("keep-alive"),
    )
  }

  let router: Router = Router::new()
    .route("/v1/runs/:id/events/history", get(history_handler))
    .route(
      "/v1/runs/:id/events",
      get(flaky_sse_handler).with_state(attempts.clone()),
    );
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("local addr");
  tokio::spawn(async move {
    let _ = axum::serve(listener, router.into_make_service()).await;
  });
  tokio::time::sleep(Duration::from_millis(80)).await;
  let url = format!("http://{addr}");

  let assert = tokio::time::timeout(
    Duration::from_secs(15),
    tokio::task::spawn_blocking(move || {
      cli_bin()
        .args([
          "workflow", "logs", run_id, "--server", &url, "--follow", "--format", "json",
        ])
        .assert()
        .success()
    }),
  )
  .await
  .expect("CLI must finish within 15s after reconnect succeeds")
  .expect("join");

  let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
  let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
  let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

  // 4 distinct seqs, no duplicates — proof the reconnect resumed
  // past the prior high-water mark instead of replaying from 0.
  assert_eq!(
    lines.len(),
    4,
    "follow mode must emit 4 events across the reconnect, got {}:\n{}",
    lines.len(),
    stdout
  );
  let mut seen_seqs = Vec::new();
  for line in &lines {
    let parsed: Value = serde_json::from_str(line).expect("each line is JSON");
    seen_seqs.push(parsed["seq"].as_i64().unwrap());
  }
  assert_eq!(
    seen_seqs,
    vec![0, 1, 2, 3],
    "events must arrive in order with no duplicates after reconnect; got {seen_seqs:?}"
  );

  // The reconnect notice goes to stderr so it doesn't corrupt the
  // JSONL stream on stdout. Confirming the warning fires proves the
  // backoff path actually ran (versus the server happening to emit
  // all 4 events in one transaction).
  assert!(
    stderr.contains("reconnecting in"),
    "reconnect warning must reach stderr; got: {stderr}"
  );
  // The server should have seen exactly two GET /events attempts —
  // one for the initial open + one for the reconnect.
  assert_eq!(
    attempts.load(Ordering::SeqCst),
    2,
    "Q3.5.3: server must have received exactly two SSE attempts"
  );
}

#[tokio::test]
async fn cli_workflow_logs_follow_rejects_envelope_format() {
  // The mock server is not even reached — `--follow` +
  // `--format json-envelope` is rejected by the CLI before any
  // network call. Use an obviously-unreachable URL to prove the
  // early-bail path: if the validation didn't fire, the CLI would
  // hang trying to connect.
  let assert = tokio::task::spawn_blocking(move || {
    cli_bin()
      .args([
        "workflow",
        "logs",
        "any-run-id",
        "--server",
        "http://127.0.0.1:1",
        "--follow",
        "--format",
        "json-envelope",
      ])
      .assert()
      .failure()
  })
  .await
  .expect("join");

  let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
  assert!(
    stderr.contains("incompatible with --follow"),
    "stderr must explain why the combination was rejected, got: {stderr}"
  );
}
