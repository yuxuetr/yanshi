//! Server-backed `workflow` subcommands (P2.5).
//!
//! These delegate to the [`ServerClient`] when the caller passes
//! `--server <url>` (or sets `YANSHI_SERVER_URL`). They never reach
//! into the in-process executor — that path is in `run.rs` and stays
//! the default when no server URL is configured.

use anyhow::Result;

use crate::server_client::{ServerClient, resolve_auth_token, resolve_tenant_id};

/// Build a [`ServerClient`] from `(server_url, auth_token, tenant)` flags
/// after applying the env fallbacks. The caller has already decided to
/// enter server mode (i.e. `resolve_server_url` returned `Some`).
pub fn build_client(
  server_url: &str,
  auth_token: Option<&str>,
  tenant: Option<&str>,
) -> Result<ServerClient> {
  let token = resolve_auth_token(auth_token);
  let tenant_id = resolve_tenant_id(tenant);
  ServerClient::new(server_url.to_string(), token, tenant_id)
}

/// Render the server response — either as the legacy bare JSON
/// body (default) or wrapped in the canonical `CliJsonEnvelope`.
fn print_server_response(command: &str, format: &str, body: &serde_json::Value) -> Result<()> {
  if format == "json-envelope" {
    let envelope = crate::json_envelope::CliJsonEnvelope::ok(command, body);
    println!("{}", serde_json::to_string_pretty(&envelope)?);
  } else {
    println!("{}", serde_json::to_string_pretty(body)?);
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn list(
  server_url: &str,
  auth_token: Option<&str>,
  tenant: Option<&str>,
  limit: Option<i64>,
  offset: Option<i64>,
  status: Option<&str>,
  format: &str,
) -> Result<()> {
  let client = build_client(server_url, auth_token, tenant)?;
  let body = client.list_runs(limit, offset, status).await?;
  print_server_response("workflow list", format, &body)
}

pub async fn cancel(
  server_url: &str,
  auth_token: Option<&str>,
  tenant: Option<&str>,
  run_id: &str,
  format: &str,
) -> Result<()> {
  let client = build_client(server_url, auth_token, tenant)?;
  let body = client.cancel_run(run_id).await?;
  print_server_response("workflow cancel", format, &body)
}

/// P10.11.4: reject `workflow run` flags that are local-only when
/// the operator is dispatching via `--server`. Today the
/// `POST /v1/runs` wire body only accepts `{ workflow, tenant_id }`
/// — every per-run knob the local executor consumes
/// (`--model` / `--execution-mode` / `--max-concurrency` /
/// `--run-dir` / `--watch` / `--output` / `--input` / `--dry-run` /
/// `--timeout` / `--max-retries`) would be silently dropped without
/// this guard, which is exactly the class of bug operators only
/// catch in prod.
///
/// Two categories:
/// - **Always-local**: filesystem-side concerns
///   (`--run-dir`, `--output <path>`) and the in-process flow
///   (`--watch`, `--dry-run`). The server has its own filesystem
///   and event stream, so these never make sense remotely.
/// - **Future API addition**: server-side execution knobs that the
///   wire format could accept but doesn't today (`--model`,
///   `--execution-mode`, `--max-concurrency`, `--input`,
///   `--timeout`, `--max-retries`). Each error names the gap and
///   points at the local-mode workaround.
///
/// `execution_mode_default` and `max_concurrency_default` are the
/// flag defaults — the validation only fires when the operator
/// explicitly chose something else (so passing `--execution-mode
/// serial` with the default is a no-op).
#[allow(clippy::too_many_arguments)]
pub fn reject_local_only_flags(
  model: Option<&str>,
  execution_mode: &str,
  execution_mode_default: &str,
  max_concurrency: usize,
  max_concurrency_default: usize,
  run_dir: Option<&str>,
  watch: bool,
  output: Option<&str>,
  output_dir: Option<&str>,
  input: &[String],
  dry_run: bool,
  timeout: &str,
  timeout_default: &str,
  max_retries: u32,
) -> Result<()> {
  // Always-local first — these are the clearest cases and their
  // remedies are the most concrete.
  if run_dir.is_some() {
    anyhow::bail!(
      "--run-dir is local-only (the server controls its own filesystem layout under its \
       configured YANSHI_RUN_DIR). Drop --run-dir when using --server."
    );
  }
  if let Some(path) = output {
    anyhow::bail!(
      "--output '{path}' is local-only (writes the final workflow output to a local file). \
       In server mode, capture the terminal run row via --format json-envelope or stream \
       events via `yanshi workflow logs <run_id> --follow`."
    );
  }
  if output_dir.is_some() {
    anyhow::bail!(
      "--output-dir is local-only for now. In server mode, list a run's artifacts with \
       GET /v1/runs/{{id}}/artifacts and download each from \
       GET /v1/runs/{{id}}/artifacts/{{artifact_id}}."
    );
  }
  if watch {
    anyhow::bail!(
      "--watch is local-only (polls the in-process run state). In server mode, stream the \
       event log via `yanshi workflow logs <run_id> --follow` after submitting the run."
    );
  }
  if dry_run {
    anyhow::bail!(
      "--dry-run is local-only (validates without execution). In server mode, validate the \
       workflow up-front via `yanshi workflow validate <file>` and then submit only \
       once validation passes."
    );
  }

  // Future API additions — the wire body could accept these once
  // POST /v1/runs is extended; each message says so explicitly.
  if model.is_some() {
    anyhow::bail!(
      "--model is not yet wired to the server (POST /v1/runs body does not accept a per-run \
       model override today; tracked under P10.11.4). The server uses the model declared in \
       each LLM node of the workflow YAML. Drop --model or run locally to override."
    );
  }
  if execution_mode != execution_mode_default {
    anyhow::bail!(
      "--execution-mode '{execution_mode}' is not yet wired to the server (POST /v1/runs body \
       does not accept an execution-mode override today; tracked under P10.11.4). The server \
       runner uses its own configured mode. Drop --execution-mode or run locally."
    );
  }
  if max_concurrency != max_concurrency_default {
    anyhow::bail!(
      "--max-concurrency {max_concurrency} is not yet wired to the server (POST /v1/runs body \
       does not accept this override today; tracked under P10.11.4). Drop --max-concurrency \
       or run locally."
    );
  }
  if !input.is_empty() {
    anyhow::bail!(
      "--input is not yet wired to the server (POST /v1/runs body does not accept initial \
       inputs today; tracked under P10.11.4). Either bake the values into the workflow YAML \
       before submission, or run locally with --input."
    );
  }
  if timeout != timeout_default {
    anyhow::bail!(
      "--timeout '{timeout}' is not yet wired to the server (POST /v1/runs body does not \
       accept a per-run timeout today; tracked under P10.11.4). The server applies its own \
       configured timeout. Drop --timeout or run locally."
    );
  }
  if max_retries != 0 {
    anyhow::bail!(
      "--max-retries {max_retries} is not yet wired to the server (POST /v1/runs body does \
       not accept a per-run retry budget today; tracked under P10.11.4). Drop --max-retries \
       or run locally."
    );
  }
  Ok(())
}

/// `yanshi workflow logs <run_id>` — stream the server's
/// persisted event log for a run. Without `--follow`, fetches the
/// historical events as a single JSON array via
/// `GET /v1/runs/{id}/events/history`. With `--follow`, opens an
/// SSE connection at `GET /v1/runs/{id}/events` and keeps printing
/// events until the server closes (run reaches a terminal state)
/// or the user cancels.
///
/// `format` chooses the stdout shape:
/// - `"text"` (default): one line per event:
///   `[seq] kind ts payload_summary`.
/// - `"json"`: one [`serde_json::Value`] per line (JSONL).
/// - `"json-envelope"`: a single canonical `CliJsonEnvelope`
///   wrapping the events array. Only valid for the non-follow
///   path — envelope mode requires a single bounded output, so
///   combining it with `--follow` is rejected with a clear error.
///
/// `after_seq` lets a reconnecting consumer resume past a known
/// last-seen `seq`, avoiding duplicate prints.
#[allow(clippy::too_many_arguments)]
pub async fn logs(
  server_url: &str,
  auth_token: Option<&str>,
  tenant: Option<&str>,
  run_id: &str,
  follow: bool,
  after_seq: Option<i64>,
  format: &str,
) -> Result<()> {
  let client = build_client(server_url, auth_token, tenant)?;

  if follow && format == "json-envelope" {
    anyhow::bail!(
      "--format json-envelope is incompatible with --follow: an envelope wraps a bounded result, \
       but --follow produces an open-ended stream. Use --format json (JSONL) or --format text."
    );
  }

  if !follow {
    let body = client.list_events_history(run_id, after_seq).await?;
    return print_events_history(format, run_id, &body);
  }

  // Q3.5.3: follow mode wraps `stream_events_sse` in a reconnect
  // loop with bounded exponential backoff. Without this, any
  // mid-stream TCP blip (network reset, server rolling restart,
  // gateway 502) terminated the consumer with a hard error and the
  // operator had to manually rerun with `--after-seq <last_seen>`.
  // We track the highest observed `seq` across reconnects so events
  // already printed are not re-emitted.
  stream_events_with_reconnect(&client, run_id, after_seq, format).await
}

/// Q3.5.3: follow-mode driver — calls `stream_events_sse` in a loop,
/// resuming past the highest seq we've successfully printed. Transport
/// errors trigger an exponential-backoff sleep (1s → 30s cap) before
/// the next reconnect; HTTP-level errors (4xx/5xx from the server)
/// stay fatal because they typically mean misconfiguration, not a
/// transient hiccup.
///
/// Distinguishing a terminal close from a transient drop is the
/// trickiest bit: the SSE wire has no explicit "we're done" marker.
/// The server ends the stream after a terminal event kind
/// ([`is_terminal_run_event_kind`]), so "stream closed after a terminal
/// event" is a normal exit. Some failure paths (a workflow that fails
/// to build, a skill run that errors) end the run without one, so a
/// clean close that delivered nothing new asks for the run's status and
/// exits when it is terminal; any other close is a blip and reconnects.
async fn stream_events_with_reconnect(
  client: &crate::server_client::ServerClient,
  run_id: &str,
  initial_after_seq: Option<i64>,
  format: &str,
) -> Result<()> {
  use std::time::Duration;

  // Highest seq we've already handed to `print_single_event`. The
  // server's `?after_seq=N` filter is strictly `seq > N`, so passing
  // the last printed seq directly resumes without duplicates.
  let mut high_water_mark: Option<i64> = initial_after_seq;
  let mut saw_terminal_event = false;
  // Backoff state — bumped on each consecutive transport failure,
  // reset whenever at least one event flows through cleanly.
  const BACKOFF_FLOOR: Duration = Duration::from_secs(1);
  const BACKOFF_CEILING: Duration = Duration::from_secs(30);
  let mut next_backoff = BACKOFF_FLOOR;

  loop {
    let mut events_since_reconnect: u64 = 0;
    let stream_result = {
      let high_water_mark_ref = &mut high_water_mark;
      let events_since_reconnect_ref = &mut events_since_reconnect;
      let saw_terminal_event_ref = &mut saw_terminal_event;
      client
        .stream_events_sse(run_id, *high_water_mark_ref, |event| {
          if let Some(seq) = event.get("seq").and_then(|s| s.as_i64()) {
            *high_water_mark_ref = Some(match *high_water_mark_ref {
              Some(prev) => prev.max(seq),
              None => seq,
            });
          }
          if let Some(kind) = event.get("kind").and_then(|k| k.as_str())
            && is_terminal_run_event_kind(kind)
          {
            *saw_terminal_event_ref = true;
          }
          *events_since_reconnect_ref += 1;
          if let Err(err) = print_single_event(format, &event) {
            eprintln!("warning: failed to render event: {err}");
          }
        })
        .await
    };

    if events_since_reconnect > 0 {
      next_backoff = BACKOFF_FLOOR;
    }

    match stream_result {
      Ok(()) if saw_terminal_event => {
        // Clean close after a terminal event — the run is genuinely
        // done; exit without reconnecting.
        return Ok(());
      }
      Ok(()) if events_since_reconnect == 0 && run_is_terminal(client, run_id).await? => {
        return Ok(());
      }
      Ok(()) => {
        // Stream closed without a terminal marker — treat as a
        // transient drop and reconnect after the backoff window.
        eprintln!(
          "⚠  workflow logs stream closed mid-run after {events_since_reconnect} events; \
           reconnecting in {next_backoff:?}"
        );
        tokio::time::sleep(next_backoff).await;
        next_backoff = (next_backoff.saturating_mul(2)).min(BACKOFF_CEILING);
      }
      Err(err) => {
        eprintln!(
          "⚠  workflow logs stream dropped after {events_since_reconnect} events; \
           reconnecting in {next_backoff:?}. cause: {err}"
        );
        tokio::time::sleep(next_backoff).await;
        next_backoff = (next_backoff.saturating_mul(2)).min(BACKOFF_CEILING);
      }
    }
  }
}

/// Terminal event kinds the server ends a run's SSE stream after.
/// Mirrors `yanshi_server::events_stream::is_terminal_run_event_kind`;
/// change both together.
fn is_terminal_run_event_kind(kind: &str) -> bool {
  matches!(
    kind,
    "workflow.completed"
      | "workflow.failed"
      | "workflow.cancelled"
      | "skill_run_completed"
      | "run_completed"
      | "run.cancelled"
  )
}

async fn run_is_terminal(
  client: &crate::server_client::ServerClient,
  run_id: &str,
) -> Result<bool> {
  let run = client.get_run(run_id).await?;
  Ok(matches!(
    run["status"].as_str(),
    Some("succeeded" | "failed" | "cancelled")
  ))
}

fn print_events_history(format: &str, run_id: &str, body: &serde_json::Value) -> Result<()> {
  match format {
    "json-envelope" => {
      let envelope = crate::json_envelope::CliJsonEnvelope::ok("workflow logs", body);
      println!("{}", serde_json::to_string_pretty(&envelope)?);
    }
    "json" => {
      // JSONL: one event per line. Bare `Value` rendering keeps
      // downstream `jq -c` filters happy.
      let events = body.as_array().ok_or_else(|| {
        anyhow::anyhow!("server response for {run_id} events was not a JSON array (got: {body:?})")
      })?;
      for event in events {
        println!("{}", serde_json::to_string(event)?);
      }
    }
    _ => {
      // text: human-readable summary, one line per event.
      let events = body.as_array().ok_or_else(|| {
        anyhow::anyhow!("server response for {run_id} events was not a JSON array (got: {body:?})")
      })?;
      if events.is_empty() {
        eprintln!("(no events for run {run_id})");
      }
      for event in events {
        println!("{}", format_event_text(event));
      }
    }
  }
  Ok(())
}

fn print_single_event(format: &str, event: &serde_json::Value) -> Result<()> {
  match format {
    "json" => println!("{}", serde_json::to_string(event)?),
    _ => println!("{}", format_event_text(event)),
  }
  Ok(())
}

/// Format a single `StreamedEvent` JSON value as a one-line human
/// summary. Shape matches `yanshi_server::events_stream::
/// StreamedEvent`: `{ run_id, seq, kind, payload, ts }`.
/// Missing fields render as `?` rather than panicking — the
/// command's job is to surface what the server sent, not validate
/// it.
fn format_event_text(event: &serde_json::Value) -> String {
  let seq = event
    .get("seq")
    .and_then(|s| s.as_i64())
    .map(|s| s.to_string())
    .unwrap_or_else(|| "?".into());
  let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
  let ts = event.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
  // Render payload as compact JSON so the line stays scannable.
  // Truncate to 240 chars to keep `tail -f` legible — full payload
  // is available via `--format json`.
  let payload = event
    .get("payload")
    .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "{}".into()))
    .unwrap_or_else(|| "{}".into());
  const MAX_PAYLOAD_CHARS: usize = 240;
  let payload_display = if payload.chars().count() > MAX_PAYLOAD_CHARS {
    let mut truncated: String = payload.chars().take(MAX_PAYLOAD_CHARS).collect();
    truncated.push('…');
    truncated
  } else {
    payload
  };
  format!("[{seq}] {kind:<32} {ts} {payload_display}")
}

/// Submit a workflow body via the server and poll until terminal.
/// Returns the final run JSON from `GET /v1/runs/{id}`.
///
/// `format` chooses the stdout shape:
/// - `"text"` (default): emoji submit line + final pretty JSON on stdout.
/// - `"json-envelope"`: submit / poll progress lines go to **stderr** so
///   stdout stays a single parseable `CliJsonEnvelope`. Non-success
///   terminal status surfaces in `envelope.errors[]`.
pub async fn run_via_server(
  server_url: &str,
  auth_token: Option<&str>,
  tenant: Option<&str>,
  workflow_text: &str,
  format: &str,
) -> Result<()> {
  let is_envelope = format == "json-envelope";
  let client = build_client(server_url, auth_token, tenant)?;
  let submission = client.submit_run(workflow_text).await?;
  let run_id = submission["run_id"]
    .as_str()
    .ok_or_else(|| anyhow::anyhow!("server response missing run_id: {submission}"))?
    .to_string();

  // Progress goes to stderr in envelope mode so stdout stays clean
  // for `jq` consumers. Text mode keeps the original stdout
  // behaviour for back-compat with shell logs that captured it.
  let progress_line = format!(
    "📋 Submitted run {run_id}; status: {}",
    submission["status"]
  );
  if is_envelope {
    eprintln!("{progress_line}");
  } else {
    println!("{progress_line}");
  }

  // Poll the run until it reaches a terminal status. The server flips
  // the row to `succeeded` / `failed` / `cancelled` once the inline
  // executor finishes. Bounded so a stuck server doesn't hang the CLI.
  const POLL_TIMEOUT_MS: u64 = 60_000;
  const POLL_INTERVAL_MS: u64 = 250;
  let mut waited = 0u64;
  loop {
    let row = client.get_run(&run_id).await?;
    let status = row["status"].as_str().unwrap_or("unknown");
    if matches!(status, "succeeded" | "failed" | "cancelled") {
      if is_envelope {
        // P3.3 migration: wrap the terminal run row in the canonical
        // envelope. Non-success statuses populate `errors[]` so
        // shell tooling can branch on `errors.length > 0` without
        // walking `result.status`. The bare run row stays available
        // as `envelope.result` so consumers who pinned to the legacy
        // shape migrate by reading `envelope.result.<field>`.
        let errors: Vec<String> = if status == "succeeded" {
          Vec::new()
        } else {
          vec![format!("run {run_id} ended with status '{status}'")]
        };
        let envelope =
          crate::json_envelope::CliJsonEnvelope::with_errors("workflow run", &row, errors);
        println!("{}", serde_json::to_string_pretty(&envelope)?);
      } else {
        println!("{}", serde_json::to_string_pretty(&row)?);
      }
      return Ok(());
    }
    if waited >= POLL_TIMEOUT_MS {
      anyhow::bail!(
        "run {run_id} did not reach a terminal status within {POLL_TIMEOUT_MS} ms; last status: {status}"
      );
    }
    tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    waited += POLL_INTERVAL_MS;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn format_event_text_renders_all_fields() {
    let event = json!({
      "run_id": "00000000-0000-0000-0000-000000000001",
      "seq": 12,
      "kind": "step_started",
      "payload": { "node_id": "render" },
      "ts": "2026-05-20T10:00:00Z",
    });
    let line = format_event_text(&event);
    assert!(line.contains("[12]"));
    assert!(line.contains("step_started"));
    assert!(line.contains("2026-05-20T10:00:00Z"));
    assert!(line.contains(r#"{"node_id":"render"}"#));
  }

  #[test]
  fn format_event_text_handles_missing_fields_gracefully() {
    let event = json!({});
    let line = format_event_text(&event);
    // Missing seq → "?"; missing kind → "?"; missing ts → "?";
    // missing payload → "{}". Must never panic on partial input.
    assert!(line.contains("[?]"));
    assert!(line.contains('?'));
  }

  #[test]
  fn format_event_text_truncates_long_payloads() {
    let big_string: String = std::iter::repeat_n('x', 1000).collect();
    let event = json!({
      "seq": 1,
      "kind": "noisy",
      "ts": "now",
      "payload": { "blob": big_string },
    });
    let line = format_event_text(&event);
    // Truncation appends a `…` glyph; cap is 240 chars of payload.
    assert!(line.contains('…'), "long payloads must be truncated");
    // The wrapping format `[seq] kind ts payload` makes the line
    // somewhat longer than 240; ensure we did NOT include the full
    // 1000-char blob.
    assert!(
      !line.contains(&"x".repeat(1000)),
      "full blob must not appear in the truncated line"
    );
  }

  // ── P10.11.4 reject_local_only_flags tests ─────────────────────────
  //
  // Each test isolates one flag so a regression that loosens the
  // guard for one knob doesn't silently land. The defaults passed
  // to the validator must mirror the clap definitions in main.rs.

  /// Test seam over [`reject_local_only_flags`]: every test fills
  /// in the defaults from the clap definitions in `main.rs`, then
  /// overrides exactly one flag, then asserts the per-flag message.
  /// The `#[allow]` keeps clippy from flagging the long arg list —
  /// the wrapped function is the one that needs it; the helper
  /// inherits the shape.
  #[allow(clippy::too_many_arguments)]
  fn run_validator(
    model: Option<&str>,
    execution_mode: &str,
    max_concurrency: usize,
    run_dir: Option<&str>,
    watch: bool,
    output: Option<&str>,
    input: &[String],
    dry_run: bool,
    timeout: &str,
    max_retries: u32,
  ) -> Result<()> {
    reject_local_only_flags(
      model,
      execution_mode,
      "serial",
      max_concurrency,
      4,
      run_dir,
      watch,
      output,
      None,
      input,
      dry_run,
      timeout,
      "60s",
      max_retries,
    )
  }

  #[test]
  fn workflow_run_server_baseline_passes() {
    // Defaults across the board must yield Ok — otherwise every
    // operator's first `workflow run --server <url> <file>` would
    // get rejected.
    run_validator(None, "serial", 4, None, false, None, &[], false, "60s", 0)
      .expect("defaults must pass");
  }

  #[test]
  fn workflow_run_server_rejects_run_dir_as_local_only() {
    let err = run_validator(
      None,
      "serial",
      4,
      Some("/tmp/x"),
      false,
      None,
      &[],
      false,
      "60s",
      0,
    )
    .expect_err("--run-dir must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--run-dir is local-only"), "{msg}");
    assert!(
      msg.contains("YANSHI_RUN_DIR"),
      "should name the server-side env var: {msg}"
    );
  }

  #[test]
  fn workflow_run_server_rejects_output_path_with_alternative_hint() {
    let err = run_validator(
      None,
      "serial",
      4,
      None,
      false,
      Some("out.json"),
      &[],
      false,
      "60s",
      0,
    )
    .expect_err("--output must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--output 'out.json' is local-only"), "{msg}");
    // The error must point the operator at the in-band alternatives.
    assert!(
      msg.contains("json-envelope") || msg.contains("workflow logs"),
      "{msg}"
    );
  }

  #[test]
  fn workflow_run_server_rejects_watch_with_logs_alternative() {
    let err = run_validator(None, "serial", 4, None, true, None, &[], false, "60s", 0)
      .expect_err("--watch must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--watch is local-only"), "{msg}");
    // Operators get the workflow logs --follow alternative explicitly.
    assert!(msg.contains("workflow logs"), "{msg}");
    assert!(msg.contains("--follow"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_dry_run_with_validate_alternative() {
    let err = run_validator(None, "serial", 4, None, false, None, &[], true, "60s", 0)
      .expect_err("--dry-run must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--dry-run is local-only"), "{msg}");
    // The clean alternative for a dry-run-then-submit pattern is
    // `workflow validate`; the error names it.
    assert!(msg.contains("workflow validate"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_model_with_future_api_note() {
    let err = run_validator(
      Some("gpt-4o"),
      "serial",
      4,
      None,
      false,
      None,
      &[],
      false,
      "60s",
      0,
    )
    .expect_err("--model must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--model is not yet wired"), "{msg}");
    // The "future API addition" caveat names P10.11.4 so curious
    // operators can trace it back.
    assert!(msg.contains("P10.11.4"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_execution_mode_when_explicitly_set() {
    // Defaults pass; only the explicit override trips the guard.
    let err = run_validator(
      None,
      "concurrent",
      4,
      None,
      false,
      None,
      &[],
      false,
      "60s",
      0,
    )
    .expect_err("--execution-mode concurrent must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--execution-mode 'concurrent'"), "{msg}");
    assert!(msg.contains("P10.11.4"), "{msg}");
  }

  #[test]
  fn workflow_run_server_accepts_execution_mode_at_default_value() {
    // Passing `--execution-mode serial` explicitly is the same as
    // the default; the validator must not trip.
    run_validator(None, "serial", 4, None, false, None, &[], false, "60s", 0)
      .expect("explicit default value must pass");
  }

  #[test]
  fn workflow_run_server_rejects_max_concurrency_when_changed() {
    let err = run_validator(None, "serial", 16, None, false, None, &[], false, "60s", 0)
      .expect_err("--max-concurrency 16 must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--max-concurrency 16"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_input_pairs_with_yaml_inline_hint() {
    let inputs = vec!["k".to_string(), "v".to_string()];
    let err = run_validator(
      None, "serial", 4, None, false, None, &inputs, false, "60s", 0,
    )
    .expect_err("--input must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--input is not yet wired"), "{msg}");
    // Operators get the "bake into YAML" workaround explicitly.
    assert!(msg.contains("workflow YAML"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_timeout_when_changed() {
    let err = run_validator(None, "serial", 4, None, false, None, &[], false, "120s", 0)
      .expect_err("--timeout 120s must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--timeout '120s'"), "{msg}");
  }

  #[test]
  fn workflow_run_server_rejects_max_retries_when_nonzero() {
    let err = run_validator(None, "serial", 4, None, false, None, &[], false, "60s", 3)
      .expect_err("--max-retries 3 must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("--max-retries 3"), "{msg}");
  }

  /// Guard ordering invariant: when multiple local-only flags are
  /// set at once, the always-local category fires first (run_dir
  /// before model). Pin this so a future refactor doesn't silently
  /// flip the order and start surfacing a less-actionable error
  /// when both are set.
  #[test]
  fn workflow_run_server_rejects_always_local_before_future_api() {
    let err = run_validator(
      Some("gpt-4o"),
      "serial",
      4,
      Some("/tmp/x"),
      false,
      None,
      &[],
      false,
      "60s",
      0,
    )
    .expect_err("both flags set must still err");
    let msg = err.to_string();
    assert!(
      msg.contains("--run-dir"),
      "always-local --run-dir must surface first when both set: {msg}",
    );
  }

  /// P10.11.1: --follow + --format json-envelope is rejected with
  /// a clear error. The contract: an envelope is bounded; a follow
  /// stream is unbounded — they're mutually exclusive.
  #[tokio::test]
  async fn logs_rejects_follow_with_json_envelope_format() {
    // No need for a real server — the validation runs before any
    // HTTP call. Using a junk URL exercises the early-bail path.
    let err = logs(
      "http://127.0.0.1:1",
      None,
      None,
      "any-run-id",
      true,            // follow
      None,            // after_seq
      "json-envelope", // incompatible
    )
    .await
    .expect_err("must reject the combination");
    let msg = err.to_string();
    assert!(
      msg.contains("incompatible with --follow"),
      "error must explain the incompatibility, got: {msg}"
    );
  }
}
