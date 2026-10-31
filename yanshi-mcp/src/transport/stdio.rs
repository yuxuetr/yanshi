//! Stdio transport implementation with buffered I/O
//!
//! This module provides a production-ready stdio transport that communicates
//! with MCP servers via standard input/output, using buffered I/O for performance
//! and proper timeout/health check mechanisms.

use crate::error::{MCPError, MCPResult};
use crate::transport::traits::{Transport, TransportConfig, TransportType};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;

/// Q3.2.1: env vars that are safe to inherit when sandboxing the spawned
/// MCP server. PATH lets it find `node`/`python`/etc.; locale + HOME +
/// USER + SHELL are needed by many CLI tools to behave correctly. Any
/// LC_* var is additionally forwarded by the spawn code. Anything not
/// on this list — API keys, AWS creds, SSH agent sockets, OPENAI_*,
/// ANTHROPIC_*, etc. — is dropped on the floor unless the caller passes
/// it explicitly via `with_env(...)`.
const SAFE_INHERITED_ENV_VARS: &[&str] = &[
  "PATH",
  "HOME",
  "USER",
  "LOGNAME",
  "SHELL",
  "LANG",
  "TZ",
  "TERM",
  "TMPDIR",
  "PWD",
  // Windows equivalents (harmless on unix, no value, just skipped).
  "USERNAME",
  "USERPROFILE",
  "APPDATA",
  "LOCALAPPDATA",
  "SYSTEMROOT",
];

/// Stdio transport for local MCP servers
///
/// This transport spawns a local process and communicates via stdin/stdout
/// using line-delimited JSON-RPC messages.
///
/// # Example
///
/// ```no_run
/// use yanshi_mcp::transport::{StdioTransport, Transport};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut transport = StdioTransport::new(vec![
///   "npx".to_string(),
///   "-y".to_string(),
///   "@modelcontextprotocol/server-everything".to_string(),
/// ]);
///
/// transport.connect().await?;
/// # Ok(())
/// # }
/// ```
pub struct StdioTransport {
  /// Command and arguments to spawn
  command: Vec<String>,
  /// Environment variables to set on the spawned process
  env: HashMap<String, String>,
  /// Q3.2.1: when `false` (the default), the spawned MCP server inherits
  /// only a hardened, opt-in safe-list of env vars from the parent
  /// process (PATH, locale, HOME, USER, etc.) plus whatever `env`
  /// declares explicitly. The parent's `OPENAI_API_KEY`,
  /// `ANTHROPIC_API_KEY`, AWS creds, SSH keys, etc. **do not** leak
  /// to a third-party MCP server. Operators who genuinely need full
  /// parent inheritance can flip this with `with_inherit_parent_env(true)`.
  inherit_parent_env: bool,
  /// Spawned child process — set on connect, taken on disconnect.
  /// Not accessed by send_message / receive_message, so no Mutex.
  process: Option<Child>,
  /// Q3.2.2: writer half held behind an async Mutex so concurrent
  /// `send_message` / `send_notification` callers serialize at the
  /// stdin write barrier (line ordering matters for JSON-RPC).
  /// Empty when not connected.
  writer: Arc<AsyncMutex<Option<BufWriter<ChildStdin>>>>,
  /// Q3.2.2: in-flight `send_message` calls register a oneshot
  /// here keyed by the request's JSON-RPC `id`. The reader task
  /// looks up the sender and delivers the matching response;
  /// drops the entry on send. Stale entries (caller dropped, timed
  /// out) eventually expire on the receiver side without leaking.
  inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>>,
  /// Q3.2.2: queue of server-initiated messages (notifications +
  /// out-of-band JSON-RPC traffic) for `receive_message` consumers.
  /// Receiver lives behind an async Mutex so multiple callers can
  /// share the surface; the producer side is held by the reader
  /// task. V3.5: bounded (was unbounded) — a flooding server can no
  /// longer queue unlimited `Value`s in-process. Once the channel is
  /// full further notifications are dropped (with a warning) rather
  /// than blocking the reader, which also delivers responses.
  /// Capacity is `notification_channel_capacity`.
  notifications_rx: Arc<AsyncMutex<mpsc::Receiver<Value>>>,
  /// Sender end of the notifications channel. Kept on the struct
  /// so disconnect can drop it cleanly even if the reader task
  /// has already exited.
  notifications_tx: mpsc::Sender<Value>,
  /// Q3.2.2: reader task spawned at connect(). Reads each line
  /// from stdout, parses, dispatches to either an inflight oneshot
  /// (by id) or the notifications channel. Aborted on disconnect.
  reader_task: Option<JoinHandle<()>>,
  /// Q2.6.2: handle to the stderr drain task. Set on `connect`,
  /// dropped on `disconnect`. Forwards every stderr line to
  /// `tracing::warn!` so the server's stderr can't fill Linux's
  /// 64 KiB pipe buffer and deadlock the child.
  stderr_drain: Option<JoinHandle<()>>,
  /// Q3.2.2: connection state visible from `&self` paths. Wrapped
  /// in `Arc` so the reader task can flip it false on EOF without
  /// holding a reference back to the struct.
  connected: Arc<AtomicBool>,
  /// Timeout for I/O operations
  timeout: Duration,
  /// Maximum message size (for safety)
  max_message_size: usize,
  /// V3.5: capacity of the notifications channel. Kept on the struct
  /// so `with_notification_channel_capacity` can rebuild the channel
  /// (`mpsc::channel` capacity is fixed at construction).
  notification_channel_capacity: usize,
}

impl StdioTransport {
  /// Default timeout for I/O operations (30 seconds)
  pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

  /// Default maximum message size (10 MB)
  pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

  /// Default notifications channel capacity (V3.5). Bounds how many
  /// server-initiated messages can queue in-process before further
  /// ones are dropped — generous enough that normal notification
  /// bursts fit, small enough that a flooding/misbehaving server can't
  /// grow an unbounded in-process queue.
  pub const DEFAULT_NOTIFICATION_CHANNEL_CAPACITY: usize = 1024;

  /// Create a new stdio transport
  ///
  /// # Arguments
  ///
  /// * `command` - Command and arguments to spawn (e.g., `["npx", "-y", "server"]`)
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::transport::StdioTransport;
  ///
  /// let transport = StdioTransport::new(vec![
  ///   "node".to_string(),
  ///   "server.js".to_string(),
  /// ]);
  /// ```
  pub fn new(command: Vec<String>) -> Self {
    let notification_channel_capacity = Self::DEFAULT_NOTIFICATION_CHANNEL_CAPACITY;
    let (notifications_tx, notifications_rx) = mpsc::channel(notification_channel_capacity);
    Self {
      command,
      env: HashMap::new(),
      inherit_parent_env: false,
      process: None,
      writer: Arc::new(AsyncMutex::new(None)),
      inflight: Arc::new(std::sync::Mutex::new(HashMap::new())),
      notifications_rx: Arc::new(AsyncMutex::new(notifications_rx)),
      notifications_tx,
      reader_task: None,
      stderr_drain: None,
      connected: Arc::new(AtomicBool::new(false)),
      timeout: Duration::from_millis(Self::DEFAULT_TIMEOUT_MS),
      max_message_size: Self::DEFAULT_MAX_MESSAGE_SIZE,
      notification_channel_capacity,
    }
  }

  /// Set environment variables for the spawned MCP server process.
  pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
    self.env = env;
    self
  }

  /// Q3.2.1: opt back into full parent-environment inheritance for the
  /// spawned MCP server. Default is `false` (sandboxed). Only set this
  /// to `true` for trusted MCP servers where leaking the parent's
  /// secrets (LLM keys, AWS creds, SSH agent socket) is acceptable.
  pub fn with_inherit_parent_env(mut self, inherit: bool) -> Self {
    self.inherit_parent_env = inherit;
    self
  }

  /// Set the I/O timeout
  ///
  /// # Arguments
  ///
  /// * `timeout` - Timeout duration
  ///
  /// # Example
  ///
  /// ```
  /// use yanshi_mcp::transport::StdioTransport;
  /// use std::time::Duration;
  ///
  /// let transport = StdioTransport::new(vec!["node".into(), "server.js".into()])
  ///   .with_timeout(Duration::from_secs(60));
  /// ```
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  /// Set the maximum message size
  pub fn with_max_message_size(mut self, size: usize) -> Self {
    self.max_message_size = size;
    self
  }

  /// V3.5: set the notifications channel capacity. Rebuilds the
  /// channel (must be called before `connect()` — there's nothing
  /// queued yet to lose).
  pub fn with_notification_channel_capacity(mut self, capacity: usize) -> Self {
    self.notification_channel_capacity = capacity;
    let (notifications_tx, notifications_rx) = mpsc::channel(capacity);
    self.notifications_tx = notifications_tx;
    self.notifications_rx = Arc::new(AsyncMutex::new(notifications_rx));
    self
  }

  /// Q3.2.2: shared write path used by `send_message` /
  /// `send_notification`. Takes the writer-mutex briefly per call;
  /// stdin line ordering is preserved because the lock serializes
  /// the write barrier, but lookup + oneshot register happen
  /// outside the lock so multiple senders can have requests in
  /// flight simultaneously.
  async fn write_line(&self, data: &str) -> MCPResult<()> {
    let mut guard = self.writer.lock().await;
    let stdin = guard
      .as_mut()
      .ok_or_else(|| MCPError::connection("Transport not connected (stdin not available)"))?;
    let write_future = async {
      stdin.write_all(data.as_bytes()).await?;
      stdin.write_all(b"\n").await?;
      stdin.flush().await?;
      Ok::<(), std::io::Error>(())
    };
    match tokio::time::timeout(self.timeout, write_future).await {
      Ok(Ok(())) => Ok(()),
      Ok(Err(e)) => Err(MCPError::transport(format!(
        "Failed to write to process stdin: {}",
        e
      ))),
      Err(_) => Err(MCPError::timeout(
        format!("Write timeout after {:?}", self.timeout),
        Some(self.timeout.as_millis() as u64),
      )),
    }
  }
}

/// Q3.2.2: stable key for the inflight HashMap. JSON-RPC `id` is
/// `null | string | number`; serializing to JSON gives a single
/// canonical string we can use as the key. Notifications carry no
/// `id` and never register an inflight entry.
fn request_id_key(value: &Value) -> Option<String> {
  let id = value.get("id")?;
  serde_json::to_string(id).ok()
}

/// V3.5: shared cleanup for every reader-task termination path (EOF,
/// oversized/malformed line, I/O error) — drop the writer half so
/// pending `send_message` calls fail-fast on the writer mutex's
/// `as_mut()` check, flip `connected = false`, and fail every
/// still-in-flight request by dropping its oneshot `Sender` (the
/// receiver gets `RecvError`, which `send_message` translates to a
/// connection error).
async fn terminate_reader(
  inflight: &std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>,
  writer: &AsyncMutex<Option<BufWriter<ChildStdin>>>,
  connected: &AtomicBool,
) {
  connected.store(false, Ordering::SeqCst);
  let mut guard = writer.lock().await;
  *guard = None;
  if let Ok(mut map) = inflight.lock() {
    map.clear();
  }
}

/// Q3.2.2: the reader task spawned at `connect()`. Reads each line
/// from stdout, parses, and routes:
/// - Response (has `id`): pop the matching oneshot from `inflight`
///   and deliver. If no oneshot is waiting (stale request, caller
///   timed out), drop the message on the floor.
/// - Notification (no `id`): forward to the `notifications` channel
///   so `receive_message` can drain it.
///
/// Malformed JSON is logged at `warn` and skipped; EOF terminates
/// the task and is communicated by closing the writer side (so the
/// next `send_message` will fail-fast with a connection error
/// instead of waiting the full timeout).
///
/// V3.5: the read itself is capped via `AsyncReadExt::take`, not just
/// discarded after the fact — a bare `read_line` on an unbounded
/// `BufReader` will keep growing its buffer for as long as the child
/// keeps writing with no `\n`, before any post-hoc size check gets a
/// chance to run. Once `Take` cuts a read off mid-line (limit hit, no
/// terminator) the remainder of that line is still sitting unread in
/// the pipe, so we can no longer safely resync and keep parsing —
/// that (and the symmetric "child exited mid-line, no terminator"
/// case) terminates the connection, same as any other fatal read
/// condition.
async fn run_reader_task(
  stdout: BufReader<ChildStdout>,
  inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>>,
  notifications_tx: mpsc::Sender<Value>,
  writer: Arc<AsyncMutex<Option<BufWriter<ChildStdin>>>>,
  connected: Arc<AtomicBool>,
  max_message_size: usize,
) {
  let limit = max_message_size as u64;
  let mut stdout = stdout.take(limit);
  let mut buf: Vec<u8> = Vec::new();
  loop {
    buf.clear();
    stdout.set_limit(limit);
    match stdout.read_until(b'\n', &mut buf).await {
      Ok(0) => {
        // True EOF — child process exited cleanly between lines.
        terminate_reader(&inflight, &writer, &connected).await;
        return;
      }
      Ok(n) => {
        if buf.last() != Some(&b'\n') {
          // Either the `Take` limit was hit with no terminator (line
          // too large) or the child exited mid-line (partial write,
          // no `\n`). Both leave the stream unsafe to keep reading.
          tracing::warn!(
            target = "yanshi_mcp::stdio",
            bytes = n,
            max = max_message_size,
            "stdio line exceeded max_message_size or ended without a terminator; \
             terminating connection"
          );
          terminate_reader(&inflight, &writer, &connected).await;
          return;
        }
        let trimmed = match std::str::from_utf8(&buf) {
          Ok(s) => s.trim(),
          Err(err) => {
            tracing::warn!(
              target = "yanshi_mcp::stdio",
              error = %err,
              "stdio line was not valid UTF-8; terminating connection"
            );
            terminate_reader(&inflight, &writer, &connected).await;
            return;
          }
        };
        if trimmed.is_empty() {
          continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
          Ok(v) => v,
          Err(err) => {
            tracing::warn!(
              target = "yanshi_mcp::stdio",
              error = %err,
              "malformed stdio JSON-RPC line; skipping"
            );
            continue;
          }
        };
        if let Some(key) = request_id_key(&value) {
          // Response — deliver to the matching oneshot.
          let sender = inflight.lock().ok().and_then(|mut map| map.remove(&key));
          if let Some(sender) = sender {
            // If the receiver was dropped (caller timed out), the
            // send fails silently — we don't care.
            let _ = sender.send(value);
          } else {
            tracing::debug!(
              target = "yanshi_mcp::stdio",
              id = ?value.get("id"),
              "stdio response with no matching inflight request; dropping"
            );
          }
        } else {
          // Notification — forward to the receive_message channel.
          // Never await a full channel: this task also delivers every
          // response, so blocking here stalls all in-flight requests
          // whenever nobody drains notifications (eval 2026-10-07 L5).
          crate::transport::queue_notification(&notifications_tx, value, "stdio");
        }
      }
      Err(err) => {
        tracing::warn!(
          target = "yanshi_mcp::stdio",
          error = %err,
          "stdio read failed; terminating reader task"
        );
        terminate_reader(&inflight, &writer, &connected).await;
        return;
      }
    }
  }
}

#[async_trait]
impl Transport for StdioTransport {
  async fn connect(&mut self) -> MCPResult<()> {
    if self.connected.load(Ordering::SeqCst) {
      return Ok(());
    }

    // Validate command
    if self.command.is_empty() {
      return Err(MCPError::configuration("Command cannot be empty"));
    }

    // Spawn the process
    let mut cmd = Command::new(&self.command[0]);
    if self.command.len() > 1 {
      cmd.args(&self.command[1..]);
    }

    // Q3.2.1: sandboxed env by default. See struct comment.
    if !self.inherit_parent_env {
      cmd.env_clear();
      for safe_var in SAFE_INHERITED_ENV_VARS {
        if let Ok(value) = std::env::var(safe_var) {
          cmd.env(safe_var, value);
        }
      }
      for (key, value) in std::env::vars() {
        if key.starts_with("LC_") {
          cmd.env(key, value);
        }
      }
    }
    if !self.env.is_empty() {
      cmd.envs(&self.env);
    }

    let mut child = cmd
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .kill_on_drop(true)
      .spawn()
      .map_err(|e| MCPError::connection(format!("Failed to spawn MCP server process: {}", e)))?;

    let stdin = child
      .stdin
      .take()
      .ok_or_else(|| MCPError::connection("Failed to capture stdin"))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| MCPError::connection("Failed to capture stdout"))?;

    // Q2.6.2: stderr drain so the server can log without blocking
    // on a full pipe.
    if let Some(stderr) = child.stderr.take() {
      self.stderr_drain = Some(spawn_stderr_drain(stderr));
    }

    // Q3.2.2: install the writer half + spawn the reader task.
    // The reader owns stdout and demuxes responses by JSON-RPC `id`.
    {
      let mut guard = self.writer.lock().await;
      *guard = Some(BufWriter::new(stdin));
    }
    // Q3.2.2: SHARED `Arc<AtomicBool>` between this struct's
    // `connected` field and the reader task. When the reader sees
    // EOF / read error it flips `false`, which `is_connected()` and
    // every `send_message` checks see immediately. Pre-fix the
    // struct had its own atomic separate from the one passed into
    // the task, so the EOF-detected disconnect never propagated.
    self.connected.store(true, Ordering::SeqCst);
    let reader_task = tokio::spawn(run_reader_task(
      BufReader::new(stdout),
      self.inflight.clone(),
      self.notifications_tx.clone(),
      self.writer.clone(),
      self.connected.clone(),
      self.max_message_size,
    ));
    self.reader_task = Some(reader_task);
    self.process = Some(child);

    Ok(())
  }

  async fn send_message(&self, request: Value) -> MCPResult<Value> {
    if !self.connected.load(Ordering::SeqCst) {
      return Err(MCPError::connection("Transport not connected"));
    }

    // Q3.2.2: per-request oneshot for response correlation. The
    // reader task pops the matching entry from `inflight` when the
    // response line arrives and forwards through the oneshot.
    let id_key = request_id_key(&request).ok_or_else(|| {
      MCPError::transport(
        "send_message called with a request that has no JSON-RPC `id` field; \
         use send_notification for fire-and-forget messages",
      )
    })?;

    let request_str = serde_json::to_string(&request)
      .map_err(|e| MCPError::from(e).context("Failed to serialize JSON-RPC request"))?;

    let (response_tx, response_rx) = oneshot::channel();
    // Register the oneshot BEFORE writing so a fast server response
    // can't beat us to the dispatch.
    {
      let mut map = self
        .inflight
        .lock()
        .map_err(|_| MCPError::transport("inflight map poisoned"))?;
      if map.contains_key(&id_key) {
        return Err(MCPError::transport(format!(
          "duplicate JSON-RPC request id {id_key}; previous call still in flight"
        )));
      }
      map.insert(id_key.clone(), response_tx);
    }

    // Write the request line; if write fails, clear the inflight
    // entry so we don't leak it.
    if let Err(write_err) = self.write_line(&request_str).await {
      if let Ok(mut map) = self.inflight.lock() {
        map.remove(&id_key);
      }
      return Err(write_err.context("Failed to write JSON-RPC request"));
    }

    // Wait for the response, capped by the transport timeout. On
    // timeout or sender drop, clean up the inflight entry.
    match tokio::time::timeout(self.timeout, response_rx).await {
      Ok(Ok(response)) => Ok(response),
      Ok(Err(_recv_err)) => {
        if let Ok(mut map) = self.inflight.lock() {
          map.remove(&id_key);
        }
        Err(MCPError::connection(
          "transport closed before JSON-RPC response arrived",
        ))
      }
      Err(_) => {
        if let Ok(mut map) = self.inflight.lock() {
          map.remove(&id_key);
        }
        Err(MCPError::timeout(
          format!("Request timeout after {:?}", self.timeout),
          Some(self.timeout.as_millis() as u64),
        ))
      }
    }
  }

  async fn send_notification(&self, notification: Value) -> MCPResult<()> {
    if !self.connected.load(Ordering::SeqCst) {
      return Err(MCPError::connection("Transport not connected"));
    }
    let notification_str = serde_json::to_string(&notification)
      .map_err(|e| MCPError::from(e).context("Failed to serialize JSON-RPC notification"))?;
    self
      .write_line(&notification_str)
      .await
      .map_err(|e| e.context("Failed to write JSON-RPC notification"))
  }

  async fn receive_message(&self) -> MCPResult<Option<Value>> {
    let mut rx = self.notifications_rx.lock().await;
    match tokio::time::timeout(self.timeout, rx.recv()).await {
      Ok(Some(value)) => Ok(Some(value)),
      Ok(None) => Ok(None), // channel closed (disconnected)
      Err(_) => Ok(None),   // timeout — no message available
    }
  }

  async fn disconnect(&mut self) -> MCPResult<()> {
    self.connected.store(false, Ordering::SeqCst);

    // Drop the writer first to signal EOF to the child's stdin.
    // The reader task observes EOF on stdout when the child exits.
    {
      let mut guard = self.writer.lock().await;
      *guard = None;
    }

    // Q2.6.2: stop the stderr drain.
    if let Some(handle) = self.stderr_drain.take() {
      handle.abort();
    }

    // Q3.2.2: stop the reader task. EOF on the closed stdin should
    // cause the child to exit naturally; the reader exits when its
    // stdout returns 0 bytes. We `.abort()` as a belt-and-braces
    // catch for hung children.
    if let Some(handle) = self.reader_task.take() {
      handle.abort();
    }

    // Clear any still-pending oneshots so their senders drop —
    // pending send_message callers will see `RecvError`.
    if let Ok(mut map) = self.inflight.lock() {
      map.clear();
    }

    // Kill and wait for process
    if let Some(mut process) = self.process.take() {
      match tokio::time::timeout(Duration::from_secs(2), process.wait()).await {
        Ok(Ok(_)) => {}
        _ => {
          let _ = process.kill().await;
          let _ = process.wait().await;
        }
      }
    }

    Ok(())
  }

  fn is_connected(&self) -> bool {
    self.connected.load(Ordering::SeqCst) && self.process.is_some()
  }

  fn transport_type(&self) -> TransportType {
    TransportType::Stdio
  }
}

impl TransportConfig for StdioTransport {
  fn timeout_ms(&self) -> Option<u64> {
    Some(self.timeout.as_millis() as u64)
  }

  fn set_timeout_ms(&mut self, timeout: u64) {
    self.timeout = Duration::from_millis(timeout);
  }

  fn max_message_size(&self) -> Option<usize> {
    Some(self.max_message_size)
  }

  fn set_max_message_size(&mut self, size: usize) {
    self.max_message_size = size;
  }
}

impl Drop for StdioTransport {
  fn drop(&mut self) {
    // Best effort cleanup. We deliberately do NOT spin up our own executor
    // here — running `futures::executor::block_on(process.kill().await)`
    // inside a tokio runtime context deadlocks, because the inner
    // `tokio::process::Child::kill()` future needs the tokio reactor that
    // is currently parked under the outer `block_on`. The CI hang in
    // `test_drop_cleans_up_process` was exactly this.
    //
    // Cleanup happens via two paths instead:
    //   1. Callers `await disconnect()` for the graceful path (preferred).
    //   2. `Command::kill_on_drop(true)` (set at spawn time) makes tokio
    //      SIGKILL the child when the `Child` is dropped, as long as the
    //      runtime is still alive.
    if let Some(handle) = self.stderr_drain.take() {
      handle.abort();
    }
    if let Some(handle) = self.reader_task.take() {
      handle.abort();
    }
    drop(self.process.take());
  }
}

/// Q2.6.2: stream the child's stderr into `tracing::warn!` so it
/// never blocks on a full pipe. We deliberately ignore I/O errors
/// — once stderr closes (child exits / disconnect aborts the task)
/// the loop terminates.
fn spawn_stderr_drain(stderr: ChildStderr) -> JoinHandle<()> {
  tokio::spawn(async move {
    // V3.5: cap the read the same way `run_reader_task` does, so a
    // server that floods stderr with a huge, unterminated chunk can't
    // grow this buffer unbounded either. Unlike the JSON-RPC stdout
    // path, hitting the cap here isn't fatal — stderr isn't a
    // structured protocol, it's free text for human consumption via
    // `tracing::warn!`, so a huge "line" just gets logged in
    // `DEFAULT_MAX_MESSAGE_SIZE`-sized chunks instead of one giant
    // one. Lossy UTF-8 conversion (vs. the strict stdout path) is
    // fine for the same reason — this is diagnostic output, not
    // something downstream parses.
    let limit = StdioTransport::DEFAULT_MAX_MESSAGE_SIZE as u64;
    let mut reader = BufReader::new(stderr).take(limit);
    let mut buf: Vec<u8> = Vec::new();
    loop {
      buf.clear();
      reader.set_limit(limit);
      match reader.read_until(b'\n', &mut buf).await {
        Ok(0) => break, // EOF
        Ok(_) => {
          let text = String::from_utf8_lossy(&buf);
          let trimmed = text.trim_end_matches(['\r', '\n']);
          if !trimmed.is_empty() {
            tracing::warn!(target = "yanshi_mcp::stdio::stderr", "{trimmed}");
          }
        }
        Err(_) => break,
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  // ============================================================================
  // Configuration Tests
  // ============================================================================

  #[test]
  fn test_stdio_transport_creation() {
    let transport = StdioTransport::new(vec!["echo".to_string(), "test".to_string()]);
    assert!(!transport.is_connected());
    assert_eq!(transport.transport_type(), TransportType::Stdio);
    assert_eq!(
      transport.timeout_ms(),
      Some(StdioTransport::DEFAULT_TIMEOUT_MS)
    );
    assert_eq!(
      transport.max_message_size(),
      Some(StdioTransport::DEFAULT_MAX_MESSAGE_SIZE)
    );
  }

  #[test]
  fn test_stdio_transport_with_timeout() {
    let transport =
      StdioTransport::new(vec!["test".to_string()]).with_timeout(Duration::from_secs(60));
    assert_eq!(transport.timeout_ms(), Some(60_000));
  }

  #[test]
  fn test_stdio_transport_with_max_message_size() {
    let transport = StdioTransport::new(vec!["test".to_string()]).with_max_message_size(1024);
    assert_eq!(transport.max_message_size(), Some(1024));
  }

  #[test]
  fn test_stdio_transport_config() {
    let mut transport = StdioTransport::new(vec!["test".to_string()]);
    transport.set_timeout_ms(5000);
    assert_eq!(transport.timeout_ms(), Some(5000));

    transport.set_max_message_size(1024 * 1024);
    assert_eq!(transport.max_message_size(), Some(1024 * 1024));
  }

  #[test]
  fn test_stdio_transport_builder_pattern() {
    let transport = StdioTransport::new(vec!["node".to_string(), "server.js".to_string()])
      .with_timeout(Duration::from_secs(10))
      .with_max_message_size(2 * 1024 * 1024);

    assert_eq!(transport.timeout_ms(), Some(10_000));
    assert_eq!(transport.max_message_size(), Some(2 * 1024 * 1024));
    assert!(!transport.is_connected());
  }

  // Q3.2.1 + Q3.2.2: by default a third-party MCP server must NOT see
  // the parent's secrets. Q3.2.2 took the direct `transport.stdout`
  // access path away (stdout is now owned by the reader task) — so the
  // tests use `sh -c "env > FILE"` and read FILE back, which proves
  // the spawn path applied the right env_clear policy without poking
  // any private transport internals.
  #[cfg(unix)]
  #[tokio::test]
  async fn spawn_sandboxes_parent_env_by_default() {
    let secret_key = "Q3_2_1_LEAK_TEST_OPENAI_API_KEY";
    let secret_value = "sk-must-not-leak";
    // SAFETY: test single-threaded w.r.t. this var.
    unsafe {
      std::env::set_var(secret_key, secret_value);
    }

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp); // close handle; child rewrites the path
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      format!("env > {}", path.display()),
    ])
    .with_timeout(Duration::from_secs(5));

    let _ = transport.connect().await;
    // Give the child a moment to write + exit.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = transport.disconnect().await;

    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);

    unsafe {
      std::env::remove_var(secret_key);
    }

    assert!(
      !body.contains(secret_value),
      "parent secret leaked to spawned MCP child by default; got env:\n{body}"
    );
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn spawn_inherits_parent_env_when_explicitly_opted_in() {
    let secret_key = "Q3_2_1_OPTIN_TEST_VAR";
    let secret_value = "opted-in-on-purpose";
    // SAFETY: test single-threaded w.r.t. this var.
    unsafe {
      std::env::set_var(secret_key, secret_value);
    }

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      format!("env > {}", path.display()),
    ])
    .with_timeout(Duration::from_secs(5))
    .with_inherit_parent_env(true);

    let _ = transport.connect().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = transport.disconnect().await;

    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);

    unsafe {
      std::env::remove_var(secret_key);
    }

    assert!(
      body.contains(secret_value),
      "with_inherit_parent_env(true) must restore parent env; got:\n{body}"
    );
  }

  // ============================================================================
  // Connection Tests
  // ============================================================================

  #[tokio::test]
  async fn test_connect_empty_command() {
    let mut transport = StdioTransport::new(vec![]);
    let result = transport.connect().await;
    assert!(result.is_err());
    assert!(matches!(
      result.unwrap_err(),
      MCPError::Configuration { .. }
    ));
  }

  #[tokio::test]
  async fn test_connect_invalid_command() {
    let mut transport = StdioTransport::new(vec!["nonexistent_command_xyz123".to_string()]);
    let result = transport.connect().await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), MCPError::Connection { .. }));
  }

  #[tokio::test]
  async fn test_connect_already_connected() {
    // Use 'cat' which will wait for input (works on Unix-like systems)
    let mut transport =
      StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(100));

    transport.connect().await.unwrap();
    assert!(transport.is_connected());

    // Second connect should succeed (idempotent)
    let result = transport.connect().await;
    assert!(result.is_ok());
    assert!(transport.is_connected());

    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  async fn test_is_connected_state() {
    let mut transport = StdioTransport::new(vec!["cat".to_string()]);
    assert!(!transport.is_connected());

    transport.connect().await.unwrap();
    assert!(transport.is_connected());

    transport.disconnect().await.unwrap();
    assert!(!transport.is_connected());
  }

  // ============================================================================
  // Disconnection Tests
  // ============================================================================

  #[tokio::test]
  async fn test_disconnect_cleans_up_process() {
    let mut transport = StdioTransport::new(vec!["cat".to_string()]);
    transport.connect().await.unwrap();
    assert!(transport.is_connected());

    transport.disconnect().await.unwrap();
    assert!(!transport.is_connected());
    assert!(transport.process.is_none());
    // Q3.2.2: writer is now an Arc<AsyncMutex<Option<...>>> guarded
    // by the transport — after disconnect the inner Option is None.
    assert!(transport.writer.lock().await.is_none());
  }

  #[tokio::test]
  async fn test_disconnect_when_not_connected() {
    let mut transport = StdioTransport::new(vec!["cat".to_string()]);
    let result = transport.disconnect().await;
    assert!(result.is_ok());
  }

  // ============================================================================
  // Process Health Check Tests
  // ============================================================================
  //
  // Q3.2.2: the explicit `check_process_health` helper is gone — the
  // reader task spawned at `connect()` flips `is_connected()` to
  // false when stdout returns 0 bytes (child exit) or when read
  // errors. Reframe the old health-check tests as `is_connected()`
  // observations instead.

  #[tokio::test]
  async fn test_is_connected_before_connect_is_false() {
    let transport = StdioTransport::new(vec!["cat".to_string()]);
    assert!(!transport.is_connected());
  }

  #[tokio::test]
  async fn test_is_connected_after_connect_is_true() {
    let mut transport = StdioTransport::new(vec!["cat".to_string()]);
    transport.connect().await.unwrap();
    assert!(transport.is_connected());
    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  async fn test_is_connected_flips_false_after_child_exits() {
    // `true` command exits immediately; the reader task observes EOF
    // and stores `connected = false`.
    let mut transport = StdioTransport::new(vec!["true".to_string()]);
    transport.connect().await.unwrap();
    let mut flipped = false;
    for _ in 0..20 {
      tokio::time::sleep(Duration::from_millis(50)).await;
      if !transport.is_connected() {
        flipped = true;
        break;
      }
    }
    assert!(
      flipped,
      "is_connected() must flip false within ~1s after child exits"
    );
  }

  // ============================================================================
  // Message Send/Receive Tests
  // ============================================================================

  #[tokio::test]
  async fn test_send_message_not_connected() {
    let transport = StdioTransport::new(vec!["cat".to_string()]);
    let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
    let result = transport.send_message(request).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_send_notification_not_connected() {
    let transport = StdioTransport::new(vec!["cat".to_string()]);
    let notification = json!({"jsonrpc": "2.0", "method": "test"});
    let result = transport.send_notification(notification).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_receive_message_not_connected_returns_none() {
    // Q3.2.2: receive_message no longer errors when not connected —
    // the notifications mpsc receiver returns None when the
    // sender is dropped, which we forward as `Ok(None)`. Same
    // semantics as a timeout (no message available).
    let transport =
      StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(50));
    let result = transport.receive_message().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None);
  }

  // ============================================================================
  // Timeout Tests
  // ============================================================================

  #[tokio::test]
  async fn test_receive_message_timeout_returns_none() {
    let mut transport =
      StdioTransport::new(vec!["cat".to_string()]).with_timeout(Duration::from_millis(100));

    transport.connect().await.unwrap();

    // Receive should return None on timeout (not an error)
    let result = transport.receive_message().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None);

    transport.disconnect().await.unwrap();
  }

  // ============================================================================
  // Echo Process Integration Tests
  // ============================================================================
  // These tests use simple Unix commands to test the full transport flow

  /// Q2.6.1 regression: a server that emits a notification before
  /// the matching response no longer permanently desyncs the
  /// session. Pre-fix `send_message` returned the notification as
  /// the response and the actual response had to be consumed via a
  /// separate read; here we craft a shell session that ships
  /// `notif → response` for every line, send a request, and verify
  /// the returned message has the matching id.
  #[tokio::test]
  #[cfg(unix)]
  async fn send_message_skips_notifications_until_matching_id() {
    // The shell script:
    //  - reads one line (the request)
    //  - emits a notification (no id)
    //  - echoes the request back unchanged as the response
    // Without Q2.6.1, the client would read the notification line
    // and treat it as the response.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "while read line; do \
         echo '{\"jsonrpc\":\"2.0\",\"method\":\"server/notify\",\"params\":{\"hi\":1}}'; \
         echo \"$line\"; \
       done"
        .to_string(),
    ])
    .with_timeout(Duration::from_secs(2));
    transport.connect().await.unwrap();

    let request = json!({"jsonrpc": "2.0", "method": "ping", "id": 42});
    let response = transport.send_message(request.clone()).await.unwrap();
    assert_eq!(response.get("id"), Some(&json!(42)));
    assert_eq!(response, request, "response payload must round-trip");

    // The notification we buffered must be observable via
    // `receive_message`.
    let buffered = transport.receive_message().await.unwrap().unwrap();
    assert_eq!(
      buffered.get("method").and_then(|v| v.as_str()),
      Some("server/notify"),
      "queued notification not delivered: {buffered:?}"
    );

    transport.disconnect().await.unwrap();
  }

  /// Q2.6.2 regression: a server writing > 64 KiB of stderr stays
  /// healthy because the drain task keeps the pipe empty. Without
  /// the drain, the child would block in `write(2)` to stderr the
  /// moment the pipe fills and never send a response.
  #[tokio::test]
  #[cfg(unix)]
  async fn stderr_does_not_deadlock_when_server_floods_it() {
    // Print 128 KiB of stderr in 1 KiB chunks before echoing each
    // request. 128 KiB is comfortably above Linux's default 64 KiB
    // pipe buffer, so an undrained pipe would deadlock here.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "for i in $(seq 1 128); do \
         printf 'stderr noise %d %s\\n' \"$i\" \"$(printf '%.0sx' {1..980})\" >&2; \
       done; \
       while read line; do echo \"$line\"; done"
        .to_string(),
    ])
    .with_timeout(Duration::from_secs(5));
    transport.connect().await.unwrap();

    // Give the script a beat to push the stderr noise.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let request = json!({"jsonrpc": "2.0", "method": "ping", "id": 7});
    let response = transport.send_message(request.clone()).await.unwrap();
    assert_eq!(response, request);

    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  #[cfg(unix)]
  async fn stdout_flood_with_no_newline_terminates_the_connection() {
    // V3.5: write well past a small max_message_size with no
    // trailing newline. The bounded read must cut this off instead
    // of growing an unbounded buffer, and the connection must
    // terminate rather than hang or silently resync mid-stream.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "yes x | tr -d '\\n' | head -c 200000".to_string(),
    ])
    .with_max_message_size(64 * 1024)
    .with_timeout(Duration::from_secs(5));
    transport.connect().await.unwrap();

    // Give the flood + termination a beat to land.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
      !transport.is_connected(),
      "transport must disconnect after an oversized/unterminated line"
    );
    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  #[cfg(unix)]
  async fn unconsumed_notifications_never_block_responses() {
    // Eval 2026-10-07 L5: nothing in the workspace drains
    // `receive_message`, so a reader that awaited a full notifications
    // channel stopped delivering responses after `capacity`
    // notifications and every later request timed out. Overflow is now
    // dropped; the queue stays bounded and responses keep flowing.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "for i in $(seq 1 50); do \
         printf '{\"jsonrpc\":\"2.0\",\"method\":\"tick\",\"params\":{\"i\":%d}}\\n' \"$i\"; \
       done; \
       while read line; do echo \"$line\"; done"
        .to_string(),
    ])
    .with_notification_channel_capacity(4)
    .with_timeout(Duration::from_secs(5));
    transport.connect().await.unwrap();

    // Nobody drains the 50-notification burst before this request.
    let request = json!({"jsonrpc": "2.0", "method": "ping", "id": 1});
    let response = tokio::time::timeout(
      Duration::from_secs(3),
      transport.send_message(request.clone()),
    )
    .await
    .expect("a full notifications queue must not block responses")
    .unwrap();
    assert_eq!(response, request);

    let mut received = Vec::new();
    while let Ok(Ok(Some(message))) =
      tokio::time::timeout(Duration::from_millis(200), transport.receive_message()).await
    {
      received.push(message["params"]["i"].as_i64());
    }
    assert_eq!(
      received,
      vec![Some(1), Some(2), Some(3), Some(4)],
      "the queue keeps the first `capacity` notifications and drops the overflow"
    );

    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  #[cfg(unix)] // These tests rely on Unix utilities
  async fn test_echo_json_roundtrip() {
    // Use a shell command that echoes back JSON
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "while read line; do echo \"$line\"; done".to_string(),
    ])
    .with_timeout(Duration::from_secs(1));

    transport.connect().await.unwrap();

    let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
    let response = transport.send_message(request.clone()).await.unwrap();

    assert_eq!(response, request); // Echo should return same message

    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  #[cfg(unix)]
  async fn test_multiple_messages() {
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "while read line; do echo \"$line\"; done".to_string(),
    ])
    .with_timeout(Duration::from_secs(1));

    transport.connect().await.unwrap();

    // Send multiple messages
    for i in 0..3 {
      let request = json!({"jsonrpc": "2.0", "method": "test", "id": i});
      let response = transport.send_message(request.clone()).await.unwrap();
      assert_eq!(response, request);
    }

    transport.disconnect().await.unwrap();
  }

  // ============================================================================
  // Error Handling Tests
  // ============================================================================

  #[tokio::test]
  async fn test_invalid_json_response() {
    // Q3.2.2 behavior change: malformed JSON lines from the child
    // are dropped by the reader task (logged at `warn`) instead of
    // being surfaced as a per-call parse error — there's no way to
    // attribute a malformed line to any specific inflight request.
    // The caller sees a timeout once the per-call deadline elapses
    // with no matching response. That's a strictly better failure
    // mode (the previous behavior tied the next caller's fate to
    // an unrelated server bug). The test now asserts the timeout
    // path; "JSON parse error" was a wire-level surface that no
    // longer exists.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "echo 'invalid json'; sleep 5".to_string(),
    ])
    .with_timeout(Duration::from_millis(300));

    transport.connect().await.unwrap();

    let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
    let result = transport.send_message(request).await;
    assert!(result.is_err());
    let error_msg = result.unwrap_err().to_string();
    assert!(
      error_msg.contains("timeout") || error_msg.contains("Timeout"),
      "expected timeout error after invalid JSON line; got: {error_msg}"
    );

    transport.disconnect().await.unwrap();
  }

  #[tokio::test]
  async fn test_process_exit_during_operation() {
    // Use 'true' which exits immediately
    let mut transport =
      StdioTransport::new(vec!["true".to_string()]).with_timeout(Duration::from_millis(500));

    transport.connect().await.unwrap();

    // Wait for process to exit
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Try to send message - should fail
    let request = json!({"jsonrpc": "2.0", "method": "test", "id": 1});
    let result = transport.send_message(request).await;

    assert!(result.is_err());
    assert!(!transport.is_connected());
  }

  // ============================================================================
  // Drop Tests
  // ============================================================================

  #[tokio::test]
  async fn test_drop_cleans_up_process() {
    {
      let mut transport = StdioTransport::new(vec!["cat".to_string()]);
      transport.connect().await.unwrap();
      // Transport dropped here
    }
    // If we get here without hanging, drop worked.
  }

  // ============================================================================
  // Property-Based Tests
  // ============================================================================

  mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
      /// Property: Timeout configuration is preserved
      #[test]
      fn prop_timeout_config_preserved(timeout_ms in 1u64..300_000u64) {
        let transport = StdioTransport::new(vec!["test".to_string()])
          .with_timeout(Duration::from_millis(timeout_ms));

        use crate::transport::TransportConfig;
        prop_assert_eq!(transport.timeout_ms(), Some(timeout_ms));
      }

      /// Property: Max message size configuration is preserved
      #[test]
      fn prop_max_message_size_preserved(size in 1usize..100_000_000usize) {
        let transport = StdioTransport::new(vec!["test".to_string()])
          .with_max_message_size(size);

        use crate::transport::TransportConfig;
        prop_assert_eq!(transport.max_message_size(), Some(size));
      }

      /// Property: set_timeout_ms updates timeout correctly
      #[test]
      fn prop_set_timeout_ms_works(
        initial_ms in 1u64..10_000u64,
        new_ms in 10_000u64..100_000u64
      ) {
        let mut transport = StdioTransport::new(vec!["test".to_string()])
          .with_timeout(Duration::from_millis(initial_ms));

        use crate::transport::TransportConfig;
        prop_assert_eq!(transport.timeout_ms(), Some(initial_ms));

        transport.set_timeout_ms(new_ms);
        prop_assert_eq!(transport.timeout_ms(), Some(new_ms));
      }

      /// Property: set_max_message_size updates size correctly
      #[test]
      fn prop_set_max_message_size_works(
        initial_size in 1usize..1_000_000usize,
        new_size in 1_000_000usize..10_000_000usize
      ) {
        let mut transport = StdioTransport::new(vec!["test".to_string()])
          .with_max_message_size(initial_size);

        use crate::transport::TransportConfig;
        prop_assert_eq!(transport.max_message_size(), Some(initial_size));

        transport.set_max_message_size(new_size);
        prop_assert_eq!(transport.max_message_size(), Some(new_size));
      }

      /// Property: Command vec is preserved (non-empty)
      #[test]
      fn prop_command_preserved(cmd_count in 1usize..5usize) {
        let commands: Vec<String> = (0..cmd_count)
          .map(|i| format!("cmd{}", i))
          .collect();

        let transport = StdioTransport::new(commands.clone());

        // Transport should be created successfully
        prop_assert_eq!(transport.transport_type(), TransportType::Stdio);
      }

      /// Property: New transport is not connected
      #[test]
      fn prop_new_transport_not_connected(
        timeout_ms in 1u64..60_000u64,
        max_size in 1usize..10_000_000usize
      ) {
        let transport = StdioTransport::new(vec!["test".to_string()])
          .with_timeout(Duration::from_millis(timeout_ms))
          .with_max_message_size(max_size);

        prop_assert!(!transport.is_connected());
      }

      /// Property: Transport type is always Stdio
      #[test]
      fn prop_transport_type_always_stdio(
        timeout_ms in 1u64..60_000u64
      ) {
        let transport = StdioTransport::new(vec!["test".to_string()])
          .with_timeout(Duration::from_millis(timeout_ms));

        prop_assert_eq!(transport.transport_type(), TransportType::Stdio);
      }

      /// Property: Builder pattern chains correctly
      #[test]
      fn prop_builder_pattern_chains(
        timeout_ms in 1u64..60_000u64,
        max_size in 1usize..10_000_000usize
      ) {
        let transport = StdioTransport::new(vec!["test".to_string()])
          .with_timeout(Duration::from_millis(timeout_ms))
          .with_max_message_size(max_size);

        use crate::transport::TransportConfig;
        prop_assert_eq!(transport.timeout_ms(), Some(timeout_ms));
        prop_assert_eq!(transport.max_message_size(), Some(max_size));
        prop_assert_eq!(transport.transport_type(), TransportType::Stdio);
      }
    }
  }

  // Note: Additional integration tests with real MCP servers are in tests/ directory

  // ============================================================================
  // Q3.2.2 — Per-request demux concurrency regression
  // ============================================================================

  /// Q3.2.2 — `StdioTransport::send_message` must support concurrent
  /// in-flight requests over the SAME transport. Pre-fix every call
  /// serialized behind the inner `Mutex<Box<dyn Transport>>` /
  /// blocking read loop; post-fix the reader task demuxes responses
  /// by JSON-RPC id and each `send_message` waits on its own
  /// oneshot, so N concurrent calls fan out in parallel over the
  /// single child process pipe.
  ///
  /// The mock server is a sh-script JSON-RPC echo: read a line,
  /// parse the id, echo it back as a response. We dispatch 8
  /// concurrent calls through the SAME `Arc<StdioTransport>` and
  /// assert every call receives its matching response (so the
  /// demux really routes by id) without timing out.
  #[cfg(unix)]
  #[tokio::test]
  async fn stdio_transport_supports_concurrent_send_message() {
    use std::sync::Arc;

    // sh echo loop: while-read line, extract `"id":N`, emit a JSON-RPC
    // response with the same id. Cheap enough to test demux without
    // pulling a real MCP server in. Uses portable POSIX sed
    // (`[0-9][0-9]*` instead of GNU `\+`) so the test passes on
    // macOS BSD sed too.
    let script = r#"
      while IFS= read -r line; do
        id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":%s}}\n' "$id" "$id"
      done
    "#;
    let mut transport =
      StdioTransport::new(vec!["sh".to_string(), "-c".to_string(), script.to_string()])
        .with_timeout(Duration::from_secs(5));
    transport.connect().await.expect("connect");
    let shared = Arc::new(transport);

    let mut handles = Vec::with_capacity(8);
    for i in 1..=8i64 {
      let t = shared.clone();
      handles.push(tokio::spawn(async move {
        let req = json!({"jsonrpc": "2.0", "method": "echo", "id": i});
        let resp = t.send_message(req).await.expect("send_message");
        assert_eq!(
          resp["id"].as_i64(),
          Some(i),
          "Q3.2.2: response id must match the request id (demux works)"
        );
        assert_eq!(resp["result"]["echo"].as_i64(), Some(i));
      }));
    }
    for h in handles {
      h.await.expect("join");
    }

    // Drop the shared Arc by extracting (only one ref left) so we
    // can call disconnect on &mut.
    let mut transport = Arc::try_unwrap(shared)
      .ok()
      .expect("only one Arc clone should remain after handles complete");
    transport.disconnect().await.expect("disconnect");
  }

  /// Q3.2.2 — duplicate JSON-RPC ids in flight must be rejected
  /// loudly. The demux uses the id as the inflight HashMap key,
  /// so two callers using the same id would clobber each other's
  /// oneshots; surfacing it as an error is strictly better than
  /// silently dropping one response.
  #[cfg(unix)]
  #[tokio::test]
  async fn stdio_transport_rejects_duplicate_inflight_request_id() {
    // sh sleeps forever so the first request stays in flight;
    // the second concurrent send_message with the same id should
    // surface a "duplicate id" error from the inflight register.
    let mut transport = StdioTransport::new(vec![
      "sh".to_string(),
      "-c".to_string(),
      "sleep 30".to_string(),
    ])
    .with_timeout(Duration::from_millis(500));
    transport.connect().await.expect("connect");
    let shared = Arc::new(transport);

    let t1 = shared.clone();
    let first = tokio::spawn(async move {
      let req = json!({"jsonrpc": "2.0", "method": "test", "id": 42});
      t1.send_message(req).await
    });

    // Give the first request a moment to register its inflight entry.
    tokio::time::sleep(Duration::from_millis(50)).await;

    {
      let t2 = shared.clone();
      let second_result = t2
        .send_message(json!({"jsonrpc": "2.0", "method": "test", "id": 42}))
        .await;
      assert!(
        second_result.is_err(),
        "Q3.2.2: second send_message with same id must error; got {second_result:?}"
      );
      let msg = second_result.unwrap_err().to_string();
      assert!(
        msg.contains("duplicate"),
        "error must explain duplicate id; got: {msg}"
      );
      // `t2` drops at end of this block so the Arc clone count
      // returns to 1 (plus the in-flight `first` task's clone).
    }

    // The first send_message times out (sleep keeps server alive
    // with no response), but it's a clean timeout error not a
    // collision.
    let first_result = first.await.expect("join");
    assert!(first_result.is_err());

    let mut transport = Arc::try_unwrap(shared).ok().expect("one Arc remains");
    transport.disconnect().await.expect("disconnect");
  }
}
