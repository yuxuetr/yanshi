//! Hook + approval execution layer for [`crate::runtime::HarnessRuntime`].
//!
//! Phase H2 wires the [`PreToolHook`] / [`PostToolHook`] /
//! [`ApprovalProvider`] traits (frozen in Phase H0) into the live
//! tool-dispatch path by decorating every registered [`Tool`] with a
//! [`HookedTool`] wrapper. The wrapper:
//!
//! 1. Builds a [`PendingToolCall`] from the tool metadata + params.
//! 2. Runs every registered [`PreToolHook`] under a bounded timeout.
//!    Hook timeout is fail-closed: the call is denied with a reason
//!    that points at the offending hook.
//! 3. Combines the per-hook decisions (`Deny` wins over
//!    `RequireApproval` wins over `Allow`). When the active
//!    [`HarnessProfile`] is `production` and the call's idempotency
//!    is [`ToolIdempotency::NonIdempotent`], the wrapper escalates
//!    even an unanimous `Allow` to `RequireApproval` so production
//!    runs are fail-closed by default (HARNESS_MODE_EVOLUTION Risk 2).
//! 4. If an approval is required, builds an [`ApprovalRequest`], emits
//!    [`HarnessEventBody::ApprovalRequested`], delegates to the configured
//!    [`ApprovalProvider`], and emits [`HarnessEventBody::ApprovalDecided`]
//!    with the result. The decision is cached per
//!    `(tool_name, scope)` so `Session` / `Run` scoped allows reuse
//!    without re-prompting.
//! 5. Dispatches the inner tool if allowed, or returns a structured
//!    `ToolError::PolicyDenied` otherwise.
//! 6. Runs every registered [`PostToolHook`] (advisory; failures are
//!    logged but never undo the tool invocation).
//!
//! The wrapper does not emit `tool_call_requested` or
//! `tool_call_completed` events — those keep flowing from the
//! `HarnessRuntime` post-hoc translation so existing consumers do not
//! see duplicate tool events when hooks are enabled.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use yanshi_tool::{
  Capability, SandboxStatus, Tool, ToolEffect, ToolError, ToolIdempotency, ToolMetadata,
  ToolOutput, ToolRegistry,
};

use crate::approval::{
  ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalRisk, ApprovalScope,
};
use crate::context::HarnessProfile;
use crate::error::HarnessError;
use crate::event::{ApprovalDecidedPayload, ApprovalRequestedPayload, HarnessEventBody};
use crate::hooks::{
  CompletedToolCall, PendingToolCall, PostToolHook, PreToolDecision, PreToolHook,
};
use crate::persistence::SinkChain;
use crate::seq::SeqAllocator;

/// Default timeout applied to each [`PreToolHook`] / [`PostToolHook`].
/// Operators can tighten or loosen this through
/// [`HookConfig::with_hook_timeout`].
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Default approval timeout. Used when an [`ApprovalRequest`] does not
/// carry an explicit `expires_at` field.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Configuration shared by every [`HookedTool`] wrapped from one
/// registry call. Built with `HookConfig::new(...)` + chained setters.
/// Y7.1: the estimated USD price of one call, from the tool name and its
/// raw parameters; `None` when the call has no price (or its model is
/// unpriced), which leaves it to the ordinary approval rules.
pub type CostEstimator = Arc<dyn Fn(&str, &serde_json::Value) -> Option<f64> + Send + Sync>;

/// Y7.1: approval by budget. A priced call runs without a prompt while the
/// session's spend plus its price stays within `threshold_usd`, overriding
/// the production profile's escalation; past that it requires approval.
/// An explicit pre-hook `Deny` / `RequireApproval` still wins. The price
/// is reserved when the call is allowed and refunded if the tool fails.
struct CostBudget {
  threshold_usd: f64,
  estimate: CostEstimator,
  spent_usd: std::sync::Mutex<f64>,
}

impl CostBudget {
  /// Reserve `price` if it fits; the spend so far when it does not.
  fn try_reserve(&self, price: f64) -> Result<(), f64> {
    let mut spent = self.spent_usd.lock().unwrap_or_else(|e| e.into_inner());
    if *spent + price <= self.threshold_usd + 1e-9 {
      *spent += price;
      Ok(())
    } else {
      Err(*spent)
    }
  }

  fn adjust(&self, delta: f64) {
    let mut spent = self.spent_usd.lock().unwrap_or_else(|e| e.into_inner());
    *spent = (*spent + delta).max(0.0);
  }
}

pub struct HookConfig {
  cost_budget: Option<(f64, CostEstimator)>,
  session_id: String,
  profile: HarnessProfile,
  pre_hooks: Vec<Arc<dyn PreToolHook>>,
  post_hooks: Vec<Arc<dyn PostToolHook>>,
  approval_provider: Arc<dyn ApprovalProvider>,
  sinks: SinkChain,
  hook_timeout: Duration,
  approval_timeout: Duration,
  /// Shared with the parent [`HarnessRuntime`] so hook-emitted events share the
  /// same monotonic `seq` namespace *and emit lock* as the run lifecycle events
  /// (`session_started` / `stopped` / etc). P-A3.4: the shared lock is what
  /// keeps concurrent parallel-tool approval/tool events in wire order.
  seq_allocator: SeqAllocator,
  /// Q3.10.1: monotonic step index handed to each `PendingToolCall`
  /// the hooked registry emits. Pre-Q3.10.1 `build_pending` hardcoded
  /// `step_index: 0`, so every approval / hook event the operator
  /// audit log showed claimed to belong to the first step — making
  /// it impossible to correlate a denied tool call with the agent
  /// step that requested it. Increments per `execute` call. Callers
  /// can plug their own counter via [`HookConfig::with_step_index_counter`]
  /// when an upstream coordinator already owns the step series.
  step_index_counter: Arc<AtomicU64>,
}

impl HookConfig {
  /// Build a config with the minimum required pieces. `seq_counter`
  /// is created fresh; pass a shared counter via
  /// [`HookConfig::with_seq_counter`] when integrating with a parent
  /// [`crate::runtime::HarnessRuntime`] so the emitted approval events share the
  /// session's `seq` namespace.
  ///
  /// ## Approval-gate default is silent-allow (F-A2-12)
  ///
  /// The fresh config starts with [`HarnessProfile::Local`]
  /// (the enum default). Under Local, the approval flow is **opt-in**:
  ///
  /// - With no [`PreToolHook`]s registered, every wrapped tool is
  ///   silently auto-allowed — the [`ApprovalProvider`] is never
  ///   consulted. This is intentional for low-friction local dev.
  /// - To make NonIdempotent tools (shell, file:write, http POST)
  ///   actually trigger the approval prompt, either (a) chain
  ///   [`HookConfig::with_profile`] with
  ///   [`HarnessProfile::Production`] (auto-escalates every
  ///   NonIdempotent call), or (b) register a pre-hook that
  ///   explicitly returns `PreToolDecision::RequireApproval`.
  ///
  /// Reference wiring (binary with write tools):
  ///
  /// ```rust,ignore
  /// let hook_config = HookConfig::new(session_id, approval, sinks)
  ///   .with_profile(HarnessProfile::Production);
  /// let wrapped = wrap_registry(registry, hook_config);
  /// ```
  ///
  /// See `examples/applications/code-reviewer-write/` for an
  /// end-to-end binary that uses this exact pattern.
  pub fn new(
    session_id: impl Into<String>,
    approval_provider: Arc<dyn ApprovalProvider>,
    sinks: SinkChain,
  ) -> Self {
    Self {
      session_id: session_id.into(),
      profile: HarnessProfile::default(),
      pre_hooks: Vec::new(),
      post_hooks: Vec::new(),
      approval_provider,
      sinks,
      hook_timeout: DEFAULT_HOOK_TIMEOUT,
      approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
      seq_allocator: SeqAllocator::new(),
      step_index_counter: Arc::new(AtomicU64::new(0)),
      cost_budget: None,
    }
  }

  /// Y7.1: approve by budget instead of per call; see [`CostBudget`].
  /// `estimate` prices a call (e.g. `yanshi_tools_ai::estimate_cost_usd`
  /// over the operator's pricing table).
  pub fn with_cost_budget(mut self, threshold_usd: f64, estimate: CostEstimator) -> Self {
    self.cost_budget = Some((threshold_usd.max(0.0), estimate));
    self
  }

  /// Set the security profile used by the approval-escalation rules
  /// in [`HookedTool`]. See [`HarnessProfile`] for the per-variant
  /// approval-gate semantics; the short version:
  ///
  /// - `Production` → NonIdempotent calls are auto-escalated to
  ///   `RequireApproval` (the approval prompt always fires for
  ///   mutating tools).
  /// - `Local` / `Dev` → no auto-escalation; only explicit
  ///   pre-hook `RequireApproval` decisions fire the prompt.
  ///
  /// For binaries that ship write-side tools (file:write, shell,
  /// http POST), call this with `HarnessProfile::Production` to
  /// avoid silently auto-allowing mutations (F-A2-12).
  pub fn with_profile(mut self, profile: HarnessProfile) -> Self {
    self.profile = profile;
    self
  }

  pub fn with_pre_hook(mut self, hook: Arc<dyn PreToolHook>) -> Self {
    self.pre_hooks.push(hook);
    self
  }

  pub fn with_post_hook(mut self, hook: Arc<dyn PostToolHook>) -> Self {
    self.post_hooks.push(hook);
    self
  }

  pub fn with_hook_timeout(mut self, timeout: Duration) -> Self {
    self.hook_timeout = timeout;
    self
  }

  pub fn with_approval_timeout(mut self, timeout: Duration) -> Self {
    self.approval_timeout = timeout;
    self
  }

  /// P-A3.4: share the parent runtime's [`SeqAllocator`] (counter **and** emit
  /// lock) so this hook layer's events stay in wire order with the runtime's
  /// bridge events even under concurrent emission. Prefer this over
  /// [`Self::with_seq_counter`].
  pub fn with_seq_allocator(mut self, allocator: SeqAllocator) -> Self {
    self.seq_allocator = allocator;
    self
  }

  /// Q1.7.1 compatibility: share only the raw seq counter (wrapped in a fresh
  /// emit lock). The parallel-tool race is still fixed among this config's own
  /// writers; for full lock-sharing with the runtime, use
  /// [`Self::with_seq_allocator`].
  pub fn with_seq_counter(mut self, counter: Arc<AtomicU64>) -> Self {
    self.seq_allocator = SeqAllocator::from_counter(counter);
    self
  }

  /// Q3.10.1: inject a pre-existing step counter so the hook layer's
  /// `PendingToolCall.step_index` series shares numbering with any
  /// upstream coordinator (e.g. the live server's per-session step
  /// dispatcher). Default `None` keeps the auto-incrementing
  /// per-config counter behaviour.
  pub fn with_step_index_counter(mut self, counter: Arc<AtomicU64>) -> Self {
    self.step_index_counter = counter;
    self
  }
}

/// Wrap every tool registered in `registry` with a [`HookedTool`] that
/// runs the configured hooks + approval flow on every call. The
/// registry keeps its [`yanshi_tool::ToolPolicy`] + capability state and
/// comes back behind an `Arc`, so nothing can register an unwrapped tool
/// into it afterwards; register every tool before wrapping.
pub fn wrap_registry(mut registry: ToolRegistry, config: HookConfig) -> Arc<ToolRegistry> {
  let shared_config = Arc::new(SharedHookConfig {
    session_id: config.session_id,
    profile: config.profile,
    pre_hooks: config.pre_hooks,
    post_hooks: config.post_hooks,
    approval_provider: config.approval_provider,
    sinks: config.sinks,
    hook_timeout: config.hook_timeout,
    approval_timeout: config.approval_timeout,
    seq_allocator: config.seq_allocator,
    step_index_counter: config.step_index_counter,
    approval_cache: Arc::new(Mutex::new(ApprovalCache::default())),
    budget: config
      .cost_budget
      .map(|(threshold_usd, estimate)| CostBudget {
        threshold_usd,
        estimate,
        spent_usd: std::sync::Mutex::new(0.0),
      }),
  });
  let tools = registry.list();
  for tool in tools {
    let hooked = HookedTool {
      inner: tool,
      config: shared_config.clone(),
    };
    registry.register(Arc::new(hooked));
  }
  Arc::new(registry)
}

struct SharedHookConfig {
  session_id: String,
  profile: HarnessProfile,
  pre_hooks: Vec<Arc<dyn PreToolHook>>,
  post_hooks: Vec<Arc<dyn PostToolHook>>,
  approval_provider: Arc<dyn ApprovalProvider>,
  sinks: SinkChain,
  hook_timeout: Duration,
  approval_timeout: Duration,
  seq_allocator: SeqAllocator,
  /// Q3.10.1: per-call monotonic step index source. See
  /// [`HookConfig::step_index_counter`] for the rationale.
  step_index_counter: Arc<AtomicU64>,
  approval_cache: Arc<Mutex<ApprovalCache>>,
  budget: Option<CostBudget>,
}

#[derive(Default)]
struct ApprovalCache {
  /// Cached outcomes keyed by `(tool_name, params_digest, scope)`. Only
  /// `Session` and `Run` decisions are cached; `Once` decisions are
  /// always re-prompted.
  ///
  /// W1.7: `params_digest` (see [`approval_cache_digest`]) is `None` for
  /// most tools — a session/run-scoped "allow" for e.g. `http` still
  /// blanket-covers every subsequent `http` call, matching the original
  /// coarse `(tool_name, scope)` behavior. For `shell`/`script`/
  /// `code_exec`/`file`(write) it's `Some(sha256)`: an "allow" earned by
  /// one `rm -rf /tmp/build` invocation must not silently cover a later
  /// `rm -rf /` in the same session just because both are `shell` calls.
  cached: HashMap<(String, Option<String>, ApprovalScope), ApprovalOutcome>,
  stop_after_deny: bool,
}

impl ApprovalCache {
  fn lookup(&self, tool: &str, digest: Option<&str>) -> Option<(ApprovalScope, ApprovalOutcome)> {
    for scope in [ApprovalScope::Run, ApprovalScope::Session] {
      let key = (tool.to_string(), digest.map(str::to_string), scope);
      if let Some(outcome) = self.cached.get(&key) {
        return Some((scope, *outcome));
      }
    }
    None
  }

  fn record(
    &mut self,
    tool: &str,
    digest: Option<&str>,
    scope: ApprovalScope,
    outcome: ApprovalOutcome,
  ) {
    if matches!(scope, ApprovalScope::Run | ApprovalScope::Session) {
      self.cached.insert(
        (tool.to_string(), digest.map(str::to_string), scope),
        outcome,
      );
    }
  }
}

/// Params digest folded into the approval cache key for tool classes
/// where the params materially change the call's risk — a session/run
/// "allow" decided for one invocation must not blanket-approve every
/// future invocation of the same tool regardless of what it's asked to
/// do (W1.7). `None` for every other tool, which keeps the original
/// coarse `(tool_name, scope)` caching behavior unchanged.
///
/// The digest is a SHA-256 over the *whole* risk-bearing field(s). An
/// earlier version keyed on a 64-char literal prefix, which is strictly
/// broader than exact matching: `code_exec` payloads routinely share
/// their opening `import` lines, so one session-scoped allow covered any
/// later code with the same preamble (eval 2026-10-07 S3). `script` also
/// folds in `args`, since the same script with different args is a
/// different action.
fn approval_cache_digest(tool: &str, params: &serde_json::Value) -> Option<String> {
  let field = |name: &str| params.get(name).and_then(serde_json::Value::as_str);
  let mut hasher = Sha256::new();
  match tool {
    "shell" => hasher.update(field("command")?),
    "code_exec" => hasher.update(field("code")?),
    "script" => {
      hasher.update(field("script")?);
      // NUL can't appear in a script path, so the boundary is unambiguous.
      hasher.update([0]);
      hasher.update(
        params
          .get("args")
          .map(|args| args.to_string())
          .unwrap_or_default(),
      );
    }
    "file" => {
      if field("operation") != Some("write") {
        return None;
      }
      hasher.update(field("path")?)
    }
    _ => return None,
  }
  Some(format!("{:x}", hasher.finalize()))
}

/// Tool decorator emitted by [`wrap_registry`]. Implements
/// [`Tool`] by delegating metadata to the inner tool and intercepting
/// `execute` to drive the hook / approval pipeline.
pub struct HookedTool {
  inner: Arc<dyn Tool>,
  config: Arc<SharedHookConfig>,
}

#[async_trait]
impl Tool for HookedTool {
  fn name(&self) -> &str {
    self.inner.name()
  }

  fn description(&self) -> &str {
    self.inner.description()
  }

  fn parameters_schema(&self) -> serde_json::Value {
    self.inner.parameters_schema()
  }

  fn metadata(&self) -> ToolMetadata {
    self.inner.metadata()
  }

  fn idempotency(&self, params: &serde_json::Value) -> ToolIdempotency {
    self.inner.idempotency(params)
  }

  fn effect(&self, params: &serde_json::Value) -> ToolEffect {
    self.inner.effect(params)
  }

  fn requires_capabilities(&self) -> Vec<Capability> {
    self.inner.requires_capabilities()
  }

  fn sandbox_status(&self) -> Option<SandboxStatus> {
    self.inner.sandbox_status()
  }

  async fn execute(&self, params: serde_json::Value) -> Result<ToolOutput, ToolError> {
    let started_at = Utc::now();
    let pending = self.build_pending(&params, started_at);
    // Y7.1: priced from the raw parameters (the hook-facing copy is
    // redacted).
    let price = self
      .config
      .budget
      .as_ref()
      .and_then(|budget| (budget.estimate)(self.inner.name(), &params))
      .filter(|price| price.is_finite() && *price >= 0.0);

    let pre_decision = self
      .run_pre_hooks(&pending)
      .await
      .map_err(hook_to_policy_denied)?;

    let (proceed, reserved) = self
      .resolve_proceed_decision(&pending, pre_decision, price)
      .await
      .map_err(hook_to_policy_denied)?;
    // An over-budget call that was approved still counts toward the spend.
    let charged = match (&proceed, price, &self.config.budget) {
      (Proceed::Allow, Some(price), Some(budget)) => {
        if !reserved {
          budget.adjust(price);
        }
        Some(price)
      }
      _ => None,
    };

    let tool_started = std::time::Instant::now();
    let result = match proceed {
      Proceed::Allow => self.inner.execute(params).await,
      Proceed::Deny { reason } => Err(ToolError::PolicyDenied { message: reason }),
      // W0.5: structurally distinct from `Deny` so `ReActAgent` can stop
      // the whole run instead of treating this like any other failed
      // tool call — see `ApprovalOutcome::DenyAndStop`'s doc comment.
      Proceed::DenyAndStop { reason } => Err(ToolError::PolicyDeniedAndStop { message: reason }),
    };
    let duration_ms = tool_started.elapsed().as_millis() as u64;
    if let (Some(price), Err(_), Some(budget)) = (charged, &result, &self.config.budget) {
      budget.adjust(-price);
    }

    // Post hooks observe both success and failure outcomes.
    let completed = CompletedToolCall {
      session_id: pending.session_id.clone(),
      step_index: pending.step_index,
      tool: pending.tool.clone(),
      source: pending.source.clone(),
      permissions: pending.permissions.clone(),
      is_error: result.is_err(),
      duration_ms,
      output_summary: None,
      completed_at: Utc::now(),
    };
    self.run_post_hooks(&completed).await;

    result
  }
}

#[derive(Debug, Clone)]
enum Proceed {
  Allow,
  Deny {
    reason: String,
  },
  /// W0.5: `Deny` that also carries "stop the agent loop" semantics —
  /// produced by a fresh `ApprovalOutcome::DenyAndStop` decision and by
  /// the stop-after-deny gate for every subsequent call in the session.
  DenyAndStop {
    reason: String,
  },
}

impl HookedTool {
  fn build_pending(
    &self,
    params: &serde_json::Value,
    requested_at: chrono::DateTime<chrono::Utc>,
  ) -> PendingToolCall {
    let metadata = self.inner.metadata();
    // Q3.10.1: increment the shared step counter so each emitted
    // `PendingToolCall` carries a unique monotonic step_index instead
    // of the hardcoded `0` every call used to claim. Operators
    // correlating an approval prompt with an agent step now have a
    // real ordinal to grep on.
    //
    // PendingToolCall.step_index is `usize`; the counter is `u64` so
    // a shared series with the runtime never wraps on 32-bit. `as
    // usize` truncates above 2^32 on 32-bit targets, which is way
    // above any realistic per-session step count — a saturating
    // conversion is the safe choice over `unwrap()`.
    let step_index = usize::try_from(
      self
        .config
        .step_index_counter
        .fetch_add(1, Ordering::SeqCst),
    )
    .unwrap_or(usize::MAX);
    PendingToolCall {
      session_id: self.config.session_id.clone(),
      step_index,
      tool: self.inner.name().to_string(),
      source: Some(metadata.source.clone()),
      permissions: metadata.permissions.permissions.clone(),
      idempotency: self.inner.idempotency(params),
      effect: self.inner.effect(params),
      // D4 (W1.7): `PendingToolCall.params` is documented as MUST-redacted
      // before third-party pre-hooks see it. The raw `params` still gets
      // passed to the actual tool call in `execute` (a separate, unmoved
      // binding) — only this hook-facing copy is masked.
      params: crate::params_summary::redact_only(params.clone()),
      requested_at,
    }
  }

  async fn run_pre_hooks(
    &self,
    pending: &PendingToolCall,
  ) -> Result<PreToolDecision, HarnessError> {
    let mut strictest = PreToolDecision::Allow;
    for hook in &self.config.pre_hooks {
      let hook_name = hook.name().to_string();
      let result = tokio::time::timeout(self.config.hook_timeout, hook.before_tool(pending)).await;
      let decision = match result {
        Ok(Ok(decision)) => decision,
        Ok(Err(err)) => {
          // Hook returned an explicit error → fail-closed.
          return Err(HarnessError::hook(
            hook_name,
            format!("pre-hook failed: {err}"),
          ));
        }
        Err(_) => {
          return Err(HarnessError::hook(
            hook_name,
            format!("pre-hook timed out after {:?}", self.config.hook_timeout),
          ));
        }
      };
      strictest = merge_pre_decisions(strictest, decision);
    }
    Ok(strictest)
  }

  async fn run_post_hooks(&self, completed: &CompletedToolCall) {
    for hook in &self.config.post_hooks {
      let hook_name = hook.name().to_string();
      let outcome =
        tokio::time::timeout(self.config.hook_timeout, hook.after_tool(completed)).await;
      match outcome {
        Err(_) => {
          tracing::warn!(
            target: "harness",
            hook = %hook_name,
            tool = %completed.tool,
            "post-tool hook timed out (advisory; tool result unchanged)"
          );
        }
        Ok(Err(err)) => {
          tracing::warn!(
            target: "harness",
            hook = %hook_name,
            tool = %completed.tool,
            error = %err,
            "post-tool hook returned error (advisory)"
          );
        }
        Ok(Ok(())) => {}
      }
    }
  }

  async fn resolve_proceed_decision(
    &self,
    pending: &PendingToolCall,
    pre_decision: PreToolDecision,
    price: Option<f64>,
  ) -> Result<(Proceed, bool), HarnessError> {
    // Fail-closed escalation: production profile demands explicit
    // approval for any call not *declared* Idempotent, regardless of what
    // the pre-hooks said. `Unknown` is the default for MCP / agent /
    // workflow / modality tools, so treating it as safe would wave through
    // most side-effecting calls.
    let escalation = matches!(self.config.profile, HarnessProfile::Production)
      && !matches!(pending.idempotency, ToolIdempotency::Idempotent);
    // Y7.2: publishing cannot be taken back, so under production it is
    // approved every time: no budget, no cached session/run decision.
    let publish = matches!(self.config.profile, HarnessProfile::Production)
      && matches!(pending.effect, ToolEffect::Publish);

    let need_approval = match pre_decision {
      PreToolDecision::Deny { reason } => {
        return Ok((Proceed::Deny { reason }, false));
      }
      PreToolDecision::Allow | PreToolDecision::RequireApproval { .. } if publish => Some((
        ApprovalRisk::Critical,
        format!(
          "production profile: '{}' publishes to an outside audience; every publish needs approval",
          pending.tool
        ),
      )),
      // Y7.1: a priced call under a budget is decided by the budget.
      PreToolDecision::Allow => match (price, self.config.budget.as_ref()) {
        (Some(price), Some(budget)) => match budget.try_reserve(price) {
          Ok(()) => return Ok((Proceed::Allow, true)),
          Err(spent) => Some((
            ApprovalRisk::High,
            format!(
              "cost budget: ${spent:.2} spent + about ${price:.2} for this call exceeds ${:.2}",
              budget.threshold_usd
            ),
          )),
        },
        _ if escalation => Some((
          ApprovalRisk::Critical,
          "production profile: mutating tool requires explicit approval".to_string(),
        )),
        _ => None,
      },
      PreToolDecision::RequireApproval { risk, reason } => Some((risk, reason)),
    };

    let Some((risk, reason)) = need_approval else {
      return Ok((Proceed::Allow, false));
    };

    // Cached decision wins when scope is Session/Run.
    let digest = approval_cache_digest(&pending.tool, &pending.params);
    if let Some((scope, outcome)) = {
      let cache = self.config.approval_cache.lock().await;
      if cache.stop_after_deny {
        // Q3.10.2: pre-fix this branch silently returned `Deny`
        // without emitting any approval event, so operators
        // tailing the JSONL / SSE log saw the tool call never
        // happen but had no envelope to explain why. Emit a
        // synthetic ApprovalRequested + ApprovalDecided pair
        // (request_id namespaced as `stop-after-deny-<tool>` so
        // it's never confused with a real provider response) so
        // the gate path is fully audit-visible.
        let stop_reason: String =
          "previous approval requested deny-and-stop; aborting further tool calls".into();
        self
          .emit_stop_after_deny_gate(pending, risk, &stop_reason)
          .await?;
        // W0.5: this gate exists *because* an earlier call tripped
        // `DenyAndStop` — every call it intercepts must carry the same
        // stop semantics, not the plain `Deny` this used to return.
        return Ok((
          Proceed::DenyAndStop {
            reason: stop_reason,
          },
          false,
        ));
      }
      if publish {
        None
      } else {
        cache.lookup(&pending.tool, digest.as_deref())
      }
    } {
      self
        .emit_cached_decision(pending, scope, outcome, risk, &reason)
        .await?;
      return Ok((decide_from_outcome(outcome), false));
    }

    // Build a fresh ApprovalRequest.
    // Q1.7.2: redact `params_summary` before it leaves this hook.
    // Pre-fix the raw params (which may include API keys, bearer
    // tokens, passwords) were copied straight into the JSONL / SSE
    // event envelopes the human or upstream UI sees.
    let request_id = format!("req-{}", uuid::Uuid::new_v4());
    let now = Utc::now();
    let expires_at = now
      + chrono::Duration::from_std(self.config.approval_timeout)
        .unwrap_or_else(|_| chrono::Duration::seconds(60));
    let params_summary = crate::params_summary::redact_and_cap(pending.params.clone());
    let request = ApprovalRequest {
      id: request_id.clone(),
      session_id: pending.session_id.clone(),
      step_index: pending.step_index,
      tool: pending.tool.clone(),
      source: pending.source.clone(),
      permissions: pending.permissions.clone(),
      idempotency: pending.idempotency,
      effect: pending.effect,
      params_summary,
      risk,
      reason,
      requested_at: now,
      expires_at: Some(expires_at),
    };

    self
      .emit_event(HarnessEventBody::ApprovalRequested(
        ApprovalRequestedPayload {
          request: request.clone(),
        },
      ))
      .await?;

    let decision = self.config.approval_provider.request(request).await?;

    self
      .emit_event(HarnessEventBody::ApprovalDecided(ApprovalDecidedPayload {
        decision: decision.clone(),
      }))
      .await?;

    {
      let mut cache = self.config.approval_cache.lock().await;
      if !publish {
        cache.record(
          &pending.tool,
          digest.as_deref(),
          decision.scope,
          decision.decision,
        );
      }
      if matches!(decision.decision, ApprovalOutcome::DenyAndStop) {
        cache.stop_after_deny = true;
      }
    }

    Ok((decide_from_outcome(decision.decision), false))
  }

  /// Q3.10.3: emit BOTH halves of the approval pair when reusing a
  /// cached decision. Pre-fix this method emitted only the
  /// `ApprovalDecided` envelope, so an operator tailing the SSE /
  /// JSONL log saw `Decided` events with no matching `Requested` —
  /// the UI's "pending approval" widget had nothing to render and
  /// audit log replay broke its `request_id` correlation. The
  /// synthetic `ApprovalRequested` carries the same `cached-*` id
  /// the decision references, so the pair joins cleanly. The
  /// `request_id` includes a uuid suffix so consecutive cached hits
  /// for the same tool are still distinguishable.
  async fn emit_cached_decision(
    &self,
    pending: &PendingToolCall,
    scope: ApprovalScope,
    outcome: ApprovalOutcome,
    risk: ApprovalRisk,
    reason: &str,
  ) -> Result<(), HarnessError> {
    let request_id = format!("cached-{}-{}", pending.tool, uuid::Uuid::new_v4());
    let now = Utc::now();
    // Same redaction rules as the live request path so the synthetic
    // event cannot leak header values / API keys either.
    let params_summary = crate::params_summary::redact_and_cap(pending.params.clone());
    let request = ApprovalRequest {
      id: request_id.clone(),
      session_id: pending.session_id.clone(),
      step_index: pending.step_index,
      tool: pending.tool.clone(),
      source: pending.source.clone(),
      permissions: pending.permissions.clone(),
      idempotency: pending.idempotency,
      effect: pending.effect,
      params_summary,
      risk,
      reason: reason.to_string(),
      requested_at: now,
      expires_at: None,
    };
    self
      .emit_event(HarnessEventBody::ApprovalRequested(
        ApprovalRequestedPayload { request },
      ))
      .await?;
    let decision = ApprovalDecision {
      request_id,
      decision: outcome,
      scope,
      decided_by: "cache".into(),
      decided_at: now,
      reason: Some(format!(
        "reused {:?}-scope decision from earlier call",
        scope
      )),
    };
    self
      .emit_event(HarnessEventBody::ApprovalDecided(ApprovalDecidedPayload {
        decision,
      }))
      .await
  }

  /// Q3.10.2: emit a synthetic ApprovalRequested + ApprovalDecided
  /// pair so the stop-after-deny gate is visible in JSONL / SSE
  /// instead of being silently dropped. The `request_id` is
  /// namespaced as `stop-after-deny-<tool>-<uuid>` so a downstream
  /// parser can distinguish synthetic gate events from real
  /// provider-issued ones.
  async fn emit_stop_after_deny_gate(
    &self,
    pending: &PendingToolCall,
    risk: ApprovalRisk,
    stop_reason: &str,
  ) -> Result<(), HarnessError> {
    let request_id = format!("stop-after-deny-{}-{}", pending.tool, uuid::Uuid::new_v4());
    let now = Utc::now();
    // Same redaction rules as the live request path — synthetic
    // events still flow through the operator's log so we cannot
    // leak unredacted params here either.
    let params_summary = crate::params_summary::redact_and_cap(pending.params.clone());
    let request = ApprovalRequest {
      id: request_id.clone(),
      session_id: pending.session_id.clone(),
      step_index: pending.step_index,
      tool: pending.tool.clone(),
      source: pending.source.clone(),
      permissions: pending.permissions.clone(),
      idempotency: pending.idempotency,
      effect: pending.effect,
      params_summary,
      risk,
      reason: stop_reason.to_string(),
      requested_at: now,
      expires_at: None,
    };
    self
      .emit_event(HarnessEventBody::ApprovalRequested(
        ApprovalRequestedPayload { request },
      ))
      .await?;
    let decision = ApprovalDecision {
      request_id,
      decision: ApprovalOutcome::DenyAndStop,
      scope: ApprovalScope::Run,
      decided_by: "stop-after-deny-gate".into(),
      decided_at: now,
      reason: Some(stop_reason.to_string()),
    };
    self
      .emit_event(HarnessEventBody::ApprovalDecided(ApprovalDecidedPayload {
        decision,
      }))
      .await
  }

  async fn emit_event(&self, body: HarnessEventBody) -> Result<(), HarnessError> {
    // P-A3.4: stamp under the shared emit lock. Parallel tool calls reach this
    // path concurrently (each HookedTool shares this allocator), so without the
    // lock a later-allocated approval/tool event could win the dispatch race
    // and land on the wire before an earlier one.
    self
      .config
      .seq_allocator
      .stamp(
        &self.config.sinks,
        &self.config.session_id,
        Utc::now(),
        body,
      )
      .await
      .map(|_| ())
  }
}

fn decide_from_outcome(outcome: ApprovalOutcome) -> Proceed {
  match outcome {
    ApprovalOutcome::Allow => Proceed::Allow,
    ApprovalOutcome::Deny => Proceed::Deny {
      reason: "approval denied by approver".into(),
    },
    ApprovalOutcome::DenyAndStop => Proceed::DenyAndStop {
      reason: "approval denied with stop request".into(),
    },
  }
}

fn merge_pre_decisions(current: PreToolDecision, incoming: PreToolDecision) -> PreToolDecision {
  match (current, incoming) {
    (PreToolDecision::Deny { reason }, _) => PreToolDecision::Deny { reason },
    (_, PreToolDecision::Deny { reason }) => PreToolDecision::Deny { reason },
    (PreToolDecision::RequireApproval { risk, reason }, _)
    | (_, PreToolDecision::RequireApproval { risk, reason }) => {
      PreToolDecision::RequireApproval { risk, reason }
    }
    (PreToolDecision::Allow, PreToolDecision::Allow) => PreToolDecision::Allow,
  }
}

fn hook_to_policy_denied(err: HarnessError) -> ToolError {
  ToolError::PolicyDenied {
    message: format!("harness hook denied tool: {err}"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::approval_providers::{AutoAllowApprovalProvider, AutoDenyApprovalProvider};
  use crate::persistence::{HarnessEventSink, InMemoryEventSink};
  use yanshi_tool::{ToolMetadata, ToolOutput, ToolPermissionSet, ToolSource};

  // ── Test fixtures ────────────────────────────────────────────────

  type LastParams = Arc<std::sync::Mutex<Option<serde_json::Value>>>;

  struct ProbeTool {
    name: &'static str,
    idempotency: ToolIdempotency,
    invocations: Arc<std::sync::Mutex<usize>>,
    last_params: LastParams,
  }

  impl ProbeTool {
    fn new(
      name: &'static str,
      idempotency: ToolIdempotency,
    ) -> (Self, Arc<std::sync::Mutex<usize>>) {
      let (tool, counter, _last_params) = Self::new_capturing(name, idempotency);
      (tool, counter)
    }

    /// Like [`Self::new`] but also exposes every call's raw `params`, so
    /// a test can assert the *tool* still receives the unredacted value
    /// even though `PendingToolCall.params` (seen by pre-hooks) is
    /// redacted (D4 / W1.7).
    fn new_capturing(
      name: &'static str,
      idempotency: ToolIdempotency,
    ) -> (Self, Arc<std::sync::Mutex<usize>>, LastParams) {
      let counter = Arc::new(std::sync::Mutex::new(0));
      let last_params = Arc::new(std::sync::Mutex::new(None));
      (
        Self {
          name,
          idempotency,
          invocations: counter.clone(),
          last_params: last_params.clone(),
        },
        counter,
        last_params,
      )
    }
  }

  #[async_trait]
  impl Tool for ProbeTool {
    fn name(&self) -> &str {
      self.name
    }
    fn description(&self) -> &str {
      "probe tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
      serde_json::json!({})
    }
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata {
        source: ToolSource::Builtin,
        permissions: ToolPermissionSet::default(),
        idempotency: self.idempotency,
        mcp_server_name: None,
        mcp_tool_name: None,
      }
    }
    fn idempotency(&self, _params: &serde_json::Value) -> ToolIdempotency {
      self.idempotency
    }
    async fn execute(&self, params: serde_json::Value) -> Result<ToolOutput, ToolError> {
      *self.invocations.lock().unwrap() += 1;
      let fail = params["fail"] == serde_json::json!(true);
      *self.last_params.lock().unwrap() = Some(params);
      if fail {
        return Err(ToolError::ExecutionFailed {
          message: "probe failed".to_string(),
        });
      }
      Ok(ToolOutput::success("ok"))
    }
  }

  struct DenyHook {
    reason: String,
  }

  #[async_trait]
  impl PreToolHook for DenyHook {
    fn name(&self) -> &str {
      "deny_hook"
    }
    async fn before_tool(&self, _call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
      Ok(PreToolDecision::Deny {
        reason: self.reason.clone(),
      })
    }
  }

  struct RequireApprovalHook;

  #[async_trait]
  impl PreToolHook for RequireApprovalHook {
    fn name(&self) -> &str {
      "require_approval_hook"
    }
    async fn before_tool(&self, _call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
      Ok(PreToolDecision::RequireApproval {
        risk: ApprovalRisk::High,
        reason: "hook flagged risky".into(),
      })
    }
  }

  struct RecordingHook {
    seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
  }

  #[async_trait]
  impl PreToolHook for RecordingHook {
    fn name(&self) -> &str {
      "recording_hook"
    }
    async fn before_tool(&self, call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
      self.seen.lock().unwrap().push(call.params.clone());
      Ok(PreToolDecision::Allow)
    }
  }

  struct SlowHook {
    delay: Duration,
  }

  #[async_trait]
  impl PreToolHook for SlowHook {
    fn name(&self) -> &str {
      "slow_hook"
    }
    async fn before_tool(&self, _call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
      tokio::time::sleep(self.delay).await;
      Ok(PreToolDecision::Allow)
    }
  }

  fn build_registry(tool: ProbeTool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(tool));
    registry
  }

  // ── Tests ────────────────────────────────────────────────────────

  fn approvals_requested(events: &[crate::event::HarnessEvent]) -> usize {
    events
      .iter()
      .filter(|event| matches!(event.body, HarnessEventBody::ApprovalRequested(_)))
      .count()
  }

  /// Y7.2: a mock blog tool that publishes only for `status: "published"`
  /// (drafts are local), declared idempotent and priced, so neither of
  /// those can excuse a publish from approval.
  struct PublishBlog {
    posts: Arc<std::sync::Mutex<usize>>,
  }

  #[async_trait]
  impl Tool for PublishBlog {
    fn name(&self) -> &str {
      "publish_blog"
    }
    fn description(&self) -> &str {
      "post to the blog"
    }
    fn parameters_schema(&self) -> serde_json::Value {
      serde_json::json!({})
    }
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata {
        source: ToolSource::Builtin,
        permissions: ToolPermissionSet::default(),
        idempotency: ToolIdempotency::Idempotent,
        mcp_server_name: None,
        mcp_tool_name: None,
      }
    }
    fn effect(&self, params: &serde_json::Value) -> ToolEffect {
      if params["status"] == "published" {
        ToolEffect::Publish
      } else {
        ToolEffect::Local
      }
    }
    async fn execute(&self, _params: serde_json::Value) -> Result<ToolOutput, ToolError> {
      *self.posts.lock().unwrap() += 1;
      Ok(ToolOutput::success("posted"))
    }
  }

  /// Approves for the whole session, which would normally cache.
  struct SessionAllow;

  #[async_trait]
  impl ApprovalProvider for SessionAllow {
    fn name(&self) -> &str {
      "session_allow"
    }
    async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
      Ok(ApprovalDecision {
        request_id: request.id,
        decision: ApprovalOutcome::Allow,
        scope: ApprovalScope::Session,
        decided_by: "test".into(),
        decided_at: Utc::now(),
        reason: None,
      })
    }
  }

  #[tokio::test]
  async fn production_approves_every_publish_and_marks_it_in_the_event() {
    let posts = Arc::new(std::sync::Mutex::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PublishBlog {
      posts: posts.clone(),
    }));
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(SessionAllow), sinks)
      .with_profile(HarnessProfile::Production)
      .with_cost_budget(100.0, Arc::new(|_, _| Some(0.0)));
    let registry = wrap_registry(registry, config);
    let post = serde_json::json!({"title": "Tides", "status": "published"});
    // Session-scoped approval, an idempotent declaration and a budget
    // with room to spare: each publish still asks.
    for _ in 0..2 {
      registry
        .execute("publish_blog", post.clone())
        .await
        .expect("approved");
    }
    // A draft is local: idempotent + under budget, so no prompt.
    registry
      .execute("publish_blog", serde_json::json!({"status": "draft"}))
      .await
      .expect("draft");
    let events = sink.snapshot().await;
    let requests: Vec<&ApprovalRequest> = events
      .iter()
      .filter_map(|event| match &event.body {
        HarnessEventBody::ApprovalRequested(payload) => Some(&payload.request),
        _ => None,
      })
      .collect();
    assert_eq!(requests.len(), 2, "one per publish, none for the draft");
    assert!(requests.iter().all(|r| r.effect == ToolEffect::Publish));
    assert!(requests.iter().all(|r| r.risk == ApprovalRisk::Critical));
    // Filterable on the wire: publish requests say so, others omit it.
    let wire = serde_json::to_value(requests[0]).expect("json");
    assert_eq!(wire["effect"], "publish");
    assert_eq!(*posts.lock().unwrap(), 3);

    // Under the local profile publishing is not forced through approval.
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PublishBlog {
      posts: posts.clone(),
    }));
    let local_sink = Arc::new(InMemoryEventSink::new());
    let config = HookConfig::new(
      "sess-2",
      Arc::new(AutoDenyApprovalProvider::new()),
      SinkChain::new().push(local_sink.clone() as Arc<dyn HarnessEventSink>),
    );
    wrap_registry(registry, config)
      .execute("publish_blog", post)
      .await
      .expect("local allows");
    assert_eq!(approvals_requested(&local_sink.snapshot().await), 0);
  }

  /// Y7.1 acceptance: threshold 1.00, each call 0.40 → calls 1 and 2 run
  /// unprompted (0.40, 0.80), call 3 (1.20) raises `approval_requested`.
  /// The approved over-budget call still counts, so call 4 asks again.
  #[tokio::test]
  async fn budget_approval_prompts_once_the_threshold_would_be_crossed() {
    let (tool, counter) = ProbeTool::new("paid", ToolIdempotency::NonIdempotent);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_profile(HarnessProfile::Production)
      .with_cost_budget(1.0, Arc::new(|tool, _| (tool == "paid").then_some(0.40)));
    let registry = wrap_registry(build_registry(tool), config);
    for (call, expected) in [(1, 0), (2, 0), (3, 1), (4, 2)] {
      registry
        .execute("paid", serde_json::json!({ "call": call }))
        .await
        .expect("allowed or approved");
      assert_eq!(
        approvals_requested(&sink.snapshot().await),
        expected,
        "after call {call}"
      );
    }
    assert_eq!(*counter.lock().unwrap(), 4);
  }

  #[tokio::test]
  async fn a_failed_call_refunds_its_budget_and_unpriced_calls_keep_escalating() {
    let (tool, _) = ProbeTool::new("paid", ToolIdempotency::NonIdempotent);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoDenyApprovalProvider::new()), sinks)
      .with_profile(HarnessProfile::Production)
      .with_cost_budget(
        0.5,
        Arc::new(|_, params: &serde_json::Value| {
          (params["priced"] != serde_json::json!(false)).then_some(0.40)
        }),
      );
    let registry = wrap_registry(build_registry(tool), config);
    // Fails: its 0.40 is refunded, so the next 0.40 still fits under 0.50.
    assert!(
      registry
        .execute("paid", serde_json::json!({ "fail": true }))
        .await
        .is_err()
    );
    registry
      .execute("paid", serde_json::json!({}))
      .await
      .expect("fits after the refund");
    assert_eq!(approvals_requested(&sink.snapshot().await), 0);
    // An unpriced call falls back to production escalation (denied here).
    let result = registry
      .execute("paid", serde_json::json!({ "priced": false }))
      .await;
    assert!(
      matches!(result, Err(ToolError::PolicyDenied { .. })),
      "{result:?}"
    );
    assert_eq!(approvals_requested(&sink.snapshot().await), 1);
  }

  #[tokio::test]
  async fn allow_path_invokes_inner_tool() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks);
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    assert!(result.is_ok());
    assert_eq!(*counter.lock().unwrap(), 1);
    // No approval was needed; sink should be empty.
    assert!(sink.snapshot().await.is_empty());
  }

  #[tokio::test]
  async fn deny_pre_hook_short_circuits_call() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(DenyHook {
        reason: "test deny".into(),
      }));
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
    assert_eq!(*counter.lock().unwrap(), 0);
  }

  #[tokio::test]
  async fn require_approval_hook_routes_through_provider() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    assert_eq!(*counter.lock().unwrap(), 1);
    let events = sink.snapshot().await;
    let kinds: Vec<&str> = events
      .iter()
      .map(|event| match &event.body {
        HarnessEventBody::ApprovalRequested(_) => "approval_requested",
        HarnessEventBody::ApprovalDecided(_) => "approval_decided",
        _ => "other",
      })
      .collect();
    assert_eq!(kinds, vec!["approval_requested", "approval_decided"]);
  }

  #[tokio::test]
  async fn auto_deny_provider_returns_policy_denied() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoDenyApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
    assert_eq!(*counter.lock().unwrap(), 0);
  }

  #[tokio::test]
  async fn production_profile_escalates_non_idempotent_tools() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    // No pre-hook would normally raise approval, but the production
    // profile escalation should still kick in.
    let config = HookConfig::new("sess-1", Arc::new(AutoDenyApprovalProvider::new()), sinks)
      .with_profile(HarnessProfile::Production);
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
    assert_eq!(*counter.lock().unwrap(), 0);
    let events = sink.snapshot().await;
    assert!(
      events
        .iter()
        .any(|event| matches!(event.body, HarnessEventBody::ApprovalRequested(_)))
    );
  }

  /// Eval 2026-10-07 S4: `Unknown` is the default for MCP / agent /
  /// workflow / modality tools, so the production profile must treat it
  /// like `NonIdempotent`; only a declared `Idempotent` call skips approval.
  #[tokio::test]
  async fn production_profile_escalates_everything_but_declared_idempotent() {
    for (idempotency, expect_escalation) in [
      (ToolIdempotency::Unknown, true),
      (ToolIdempotency::NonIdempotent, true),
      (ToolIdempotency::Idempotent, false),
    ] {
      let (tool, counter) = ProbeTool::new("probe", idempotency);
      let registry = build_registry(tool);
      let sinks =
        SinkChain::new().push(Arc::new(InMemoryEventSink::new()) as Arc<dyn HarnessEventSink>);
      let config = HookConfig::new("sess-1", Arc::new(AutoDenyApprovalProvider::new()), sinks)
        .with_profile(HarnessProfile::Production);
      let registry = wrap_registry(registry, config);
      let result = registry.execute("probe", serde_json::json!({})).await;
      assert_eq!(
        matches!(result, Err(ToolError::PolicyDenied { .. })),
        expect_escalation,
        "{idempotency:?}: {result:?}"
      );
      assert_eq!(*counter.lock().unwrap(), usize::from(!expect_escalation));
    }
  }

  /// S4.2 follow-up: `production_profile_escalates_non_idempotent_tools`
  /// above proves the *mechanism* works for any `NonIdempotent` tool via
  /// `ProbeTool`, a hand-rolled test double. This proves it for the real,
  /// shipped `code_exec` tool specifically — the thing actually missing
  /// after S4.2 first claimed to be done. Uses `AutoDenyApprovalProvider`
  /// so the assertion holds regardless of whether this host has a
  /// container engine on PATH: a denied approval short-circuits before
  /// `CodeExecTool::execute` ever runs its own enforcing-backend check or
  /// tries to spawn anything, so this test needs no skip-guard.
  #[tokio::test]
  async fn production_profile_escalates_code_exec() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(yanshi_tools::builtin::CodeExecTool::new()));
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoDenyApprovalProvider::new()), sinks)
      .with_profile(HarnessProfile::Production);
    let registry = wrap_registry(registry, config);
    let result = registry
      .execute("code_exec", serde_json::json!({"code": "print(1)"}))
      .await;
    assert!(
      matches!(result, Err(ToolError::PolicyDenied { .. })),
      "expected the production-profile escalation to deny code_exec before it ran, got {result:?}"
    );
    let events = sink.snapshot().await;
    assert!(
      events
        .iter()
        .any(|event| matches!(event.body, HarnessEventBody::ApprovalRequested(_))),
      "expected an approval_requested event for code_exec"
    );
  }

  struct ScopedAllowOnceProvider {
    scope: ApprovalScope,
    calls: Arc<std::sync::Mutex<usize>>,
  }

  #[async_trait]
  impl ApprovalProvider for ScopedAllowOnceProvider {
    fn name(&self) -> &str {
      "scoped_allow"
    }
    async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
      *self.calls.lock().unwrap() += 1;
      Ok(ApprovalDecision {
        request_id: request.id,
        decision: ApprovalOutcome::Allow,
        scope: self.scope,
        decided_by: "scripted".into(),
        decided_at: Utc::now(),
        reason: Some(format!("scoped allow for {:?}", self.scope)),
      })
    }
  }

  #[tokio::test]
  async fn session_scope_decisions_are_cached_across_calls() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let approval_calls = Arc::new(std::sync::Mutex::new(0_usize));
    let approval = Arc::new(ScopedAllowOnceProvider {
      scope: ApprovalScope::Session,
      calls: approval_calls.clone(),
    });
    let config =
      HookConfig::new("sess-1", approval, sinks).with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    assert_eq!(*counter.lock().unwrap(), 3);
    assert_eq!(
      *approval_calls.lock().unwrap(),
      1,
      "approval provider should be hit once"
    );
    let events = sink.snapshot().await;
    // Q3.10.3: pre-fix only the first call emitted an
    // ApprovalRequested; cached hits emitted a bare ApprovalDecided
    // with no matching `request_id`. Now each cached hit also
    // emits its own synthetic ApprovalRequested (id `cached-*`) so
    // the UI's pending-approval pairing and replay correlation
    // both work. 3 calls → 3 Requested + 3 Decided.
    let requested = events
      .iter()
      .filter(|event| matches!(event.body, HarnessEventBody::ApprovalRequested(_)))
      .count();
    let decided = events
      .iter()
      .filter(|event| matches!(event.body, HarnessEventBody::ApprovalDecided(_)))
      .count();
    assert_eq!(requested, 3);
    assert_eq!(decided, 3);
    // Pair up by request_id: every Decided must reference a Requested.
    let request_ids: std::collections::HashSet<_> = events
      .iter()
      .filter_map(|e| match &e.body {
        HarnessEventBody::ApprovalRequested(p) => Some(p.request.id.clone()),
        _ => None,
      })
      .collect();
    for event in &events {
      if let HarnessEventBody::ApprovalDecided(p) = &event.body {
        assert!(
          request_ids.contains(&p.decision.request_id),
          "Q3.10.3: Decided id {} must correlate to a Requested in the same stream",
          p.decision.request_id
        );
      }
    }
  }

  #[tokio::test]
  async fn once_scope_decisions_are_not_cached() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let approval_calls = Arc::new(std::sync::Mutex::new(0_usize));
    let approval = Arc::new(ScopedAllowOnceProvider {
      scope: ApprovalScope::Once,
      calls: approval_calls.clone(),
    });
    let config =
      HookConfig::new("sess-1", approval, sinks).with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    assert_eq!(*counter.lock().unwrap(), 2);
    assert_eq!(
      *approval_calls.lock().unwrap(),
      2,
      "approval provider should run each time"
    );
  }

  /// W1.7: pre-fix the approval cache was keyed only on `(tool, scope)`,
  /// so a session-scoped "allow" earned by one `shell` command
  /// blanket-approved every future `shell` command in the session
  /// regardless of what it actually was. The cache must now re-prompt
  /// for a materially different command.
  #[tokio::test]
  async fn shell_approval_cache_is_scoped_by_command_prefix() {
    let (tool, counter) = ProbeTool::new("shell", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let approval_calls = Arc::new(std::sync::Mutex::new(0_usize));
    let approval = Arc::new(ScopedAllowOnceProvider {
      scope: ApprovalScope::Session,
      calls: approval_calls.clone(),
    });
    let config =
      HookConfig::new("sess-1", approval, sinks).with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);

    registry
      .execute("shell", serde_json::json!({"command": "ls -la"}))
      .await
      .unwrap();
    registry
      .execute("shell", serde_json::json!({"command": "ls -la"}))
      .await
      .unwrap();
    assert_eq!(
      *approval_calls.lock().unwrap(),
      1,
      "an identical command should reuse the cached decision"
    );

    registry
      .execute("shell", serde_json::json!({"command": "rm -rf /"}))
      .await
      .unwrap();
    assert_eq!(
      *approval_calls.lock().unwrap(),
      2,
      "a materially different shell command must not reuse another command's cached approval"
    );

    assert_eq!(*counter.lock().unwrap(), 3);
  }

  /// Eval 2026-10-07 S3: the digest used to be a 64-char literal prefix,
  /// so two `code_exec` payloads sharing their import preamble shared one
  /// session-scoped approval. Likewise `script` ignored `args`.
  #[tokio::test]
  async fn approval_cache_does_not_collide_on_shared_prefix_or_args() {
    let preamble = "import os\nimport sys\nimport json\nimport pathlib\nimport subprocess\n";
    assert!(preamble.len() > 64);
    for (tool, first, second) in [
      (
        "code_exec",
        serde_json::json!({"code": format!("{preamble}print('hi')")}),
        serde_json::json!({"code": format!("{preamble}subprocess.run(['rm', '-rf', '/'])")}),
      ),
      (
        "script",
        serde_json::json!({"script": "cleanup.py", "args": {"dir": "/tmp/build"}}),
        serde_json::json!({"script": "cleanup.py", "args": {"dir": "/"}}),
      ),
    ] {
      let (probe, counter) = ProbeTool::new(tool, ToolIdempotency::NonIdempotent);
      let registry = build_registry(probe);
      let sinks =
        SinkChain::new().push(Arc::new(InMemoryEventSink::new()) as Arc<dyn HarnessEventSink>);
      let approval_calls = Arc::new(std::sync::Mutex::new(0_usize));
      let approval = Arc::new(ScopedAllowOnceProvider {
        scope: ApprovalScope::Session,
        calls: approval_calls.clone(),
      });
      let config =
        HookConfig::new("sess-1", approval, sinks).with_pre_hook(Arc::new(RequireApprovalHook));
      let registry = wrap_registry(registry, config);

      registry.execute(tool, first.clone()).await.unwrap();
      registry.execute(tool, first).await.unwrap();
      assert_eq!(
        *approval_calls.lock().unwrap(),
        1,
        "{tool}: identical call reuses approval"
      );
      registry.execute(tool, second).await.unwrap();
      assert_eq!(
        *approval_calls.lock().unwrap(),
        2,
        "{tool}: different call must re-prompt"
      );
      assert_eq!(*counter.lock().unwrap(), 3);
    }
  }

  /// W1.7: same coarse-cache bug as shell, for `file` write calls — a
  /// path digest scopes the cache; non-write operations (lower risk)
  /// keep sharing the original blanket per-tool bucket.
  #[tokio::test]
  async fn file_write_approval_cache_is_scoped_by_path() {
    let (tool, _counter) = ProbeTool::new("file", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let approval_calls = Arc::new(std::sync::Mutex::new(0_usize));
    let approval = Arc::new(ScopedAllowOnceProvider {
      scope: ApprovalScope::Session,
      calls: approval_calls.clone(),
    });
    let config =
      HookConfig::new("sess-1", approval, sinks).with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);

    registry
      .execute(
        "file",
        serde_json::json!({"operation": "write", "path": "/tmp/a.txt"}),
      )
      .await
      .unwrap();
    registry
      .execute(
        "file",
        serde_json::json!({"operation": "write", "path": "/tmp/a.txt"}),
      )
      .await
      .unwrap();
    assert_eq!(
      *approval_calls.lock().unwrap(),
      1,
      "the same write path should reuse the cached decision"
    );

    registry
      .execute(
        "file",
        serde_json::json!({"operation": "write", "path": "/tmp/b.txt"}),
      )
      .await
      .unwrap();
    assert_eq!(
      *approval_calls.lock().unwrap(),
      2,
      "a different write path must not reuse another path's cached approval"
    );

    registry
      .execute(
        "file",
        serde_json::json!({"operation": "read", "path": "/tmp/a.txt"}),
      )
      .await
      .unwrap();
    assert_eq!(
      *approval_calls.lock().unwrap(),
      3,
      "reads share the coarse blanket bucket, distinct from any write path's bucket"
    );
  }

  /// D4 (W1.7): `PendingToolCall.params` is documented "the runtime MUST
  /// redact secrets before constructing this struct" — pre-fix the raw,
  /// unredacted params reached third-party pre-hooks regardless.
  #[tokio::test]
  async fn pre_hooks_receive_redacted_params_not_raw_secrets() {
    let (tool, _counter, tool_saw) = ProbeTool::new_capturing("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(RecordingHook { seen: seen.clone() }));
    let registry = wrap_registry(registry, config);

    registry
      .execute(
        "probe",
        serde_json::json!({"api_key": "sk-live-should-not-leak", "url": "https://example.test"}),
      )
      .await
      .unwrap();

    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    let rendered = captured[0].to_string();
    assert!(
      !rendered.contains("sk-live-should-not-leak"),
      "PendingToolCall.params reached a pre-hook unredacted: {rendered}"
    );
    assert!(
      rendered.contains("example.test"),
      "redaction must not touch non-sensitive fields: {rendered}"
    );

    // The tool itself must still get the raw, unredacted params — only
    // the hook-facing PendingToolCall copy is masked.
    let tool_params = tool_saw
      .lock()
      .unwrap()
      .clone()
      .expect("tool must have run");
    assert_eq!(
      tool_params["api_key"], "sk-live-should-not-leak",
      "redacting the hook-facing copy must not redact what the tool actually receives"
    );
  }

  #[tokio::test]
  async fn slow_pre_hook_times_out_and_denies() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(SlowHook {
        delay: Duration::from_millis(200),
      }))
      .with_hook_timeout(Duration::from_millis(20));
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    let err = match result {
      Err(ToolError::PolicyDenied { message }) => message,
      other => panic!("expected PolicyDenied, got {other:?}"),
    };
    assert!(err.contains("timed out"), "actual: {err}");
    assert_eq!(*counter.lock().unwrap(), 0);
  }

  struct ErrApprovalProvider;

  #[async_trait]
  impl ApprovalProvider for ErrApprovalProvider {
    fn name(&self) -> &str {
      "err_provider"
    }
    async fn request(&self, _request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
      Err(HarnessError::ApprovalDenied("operator cancelled".into()))
    }
  }

  #[tokio::test]
  async fn approval_provider_error_is_treated_as_deny() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(ErrApprovalProvider), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    let result = registry.execute("probe", serde_json::json!({})).await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
    assert_eq!(*counter.lock().unwrap(), 0);
  }

  struct DenyAndStopProvider;

  #[async_trait]
  impl ApprovalProvider for DenyAndStopProvider {
    fn name(&self) -> &str {
      "deny_and_stop"
    }
    async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError> {
      Ok(ApprovalDecision {
        request_id: request.id,
        decision: ApprovalOutcome::DenyAndStop,
        scope: ApprovalScope::Run,
        decided_by: "scripted".into(),
        decided_at: Utc::now(),
        reason: Some("scripted deny_and_stop".into()),
      })
    }
  }

  #[tokio::test]
  async fn deny_and_stop_blocks_subsequent_calls_without_reprompt() {
    let (tool, counter) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(DenyAndStopProvider), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    let first = registry.execute("probe", serde_json::json!({})).await;
    // W0.5: `DenyAndStop` now surfaces as the structurally distinct
    // `PolicyDeniedAndStop`, not `PolicyDenied` — that's what lets
    // `ReActAgent` actually stop the run instead of treating it like
    // any other failed tool call.
    assert!(matches!(first, Err(ToolError::PolicyDeniedAndStop { .. })));
    let second = registry.execute("probe", serde_json::json!({})).await;
    assert!(matches!(second, Err(ToolError::PolicyDeniedAndStop { .. })));
    assert_eq!(*counter.lock().unwrap(), 0);
    // First call → real provider ApprovalRequested + ApprovalDecided.
    // Second call → Q3.10.2 synthetic `stop-after-deny-*` gate pair
    // so the audit log shows operators *why* the second call never
    // hit the tool, instead of silently dropping it.
    let events = sink.snapshot().await;
    let requested = events
      .iter()
      .filter(|event| matches!(event.body, HarnessEventBody::ApprovalRequested(_)))
      .count();
    assert_eq!(requested, 2, "expect one real + one synthetic gate request");
    let decided = events
      .iter()
      .filter(|event| matches!(event.body, HarnessEventBody::ApprovalDecided(_)))
      .count();
    assert_eq!(
      decided, 2,
      "expect a Decided event paired with each Requested"
    );
  }

  /// Q3.10.1 regression — `PendingToolCall.step_index` must increment
  /// per execute, not be hardcoded to 0. Pre-fix every approval /
  /// hook event the operator saw claimed `step_index: 0`, making it
  /// impossible to grep the audit log for the step that requested a
  /// specific tool call.
  #[tokio::test]
  async fn pending_step_index_increments_per_call() {
    use std::sync::Mutex as StdMutex;

    // Pre-hook that captures the step_index it sees so the test can
    // assert each successive call observed a distinct value. Always
    // returns Allow so the call flow runs to completion.
    struct StepIndexProbe {
      seen: Arc<StdMutex<Vec<usize>>>,
    }
    #[async_trait]
    impl PreToolHook for StepIndexProbe {
      fn name(&self) -> &str {
        "step_index_probe"
      }
      async fn before_tool(&self, call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
        self.seen.lock().unwrap().push(call.step_index);
        Ok(PreToolDecision::Allow)
      }
    }

    let (tool, _) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let seen = Arc::new(StdMutex::new(Vec::<usize>::new()));
    let config = HookConfig::new(
      "sess-step-index",
      Arc::new(AutoAllowApprovalProvider::new()),
      sinks,
    )
    .with_pre_hook(Arc::new(StepIndexProbe { seen: seen.clone() }));
    let registry = wrap_registry(registry, config);

    for _ in 0..3 {
      registry
        .execute("probe", serde_json::json!({}))
        .await
        .unwrap();
    }

    let observed = seen.lock().unwrap().clone();
    assert_eq!(
      observed,
      vec![0, 1, 2],
      "Q3.10.1: step_index must increment monotonically per execute, got {observed:?}"
    );
  }

  /// Q3.10.1 — `HookConfig::with_step_index_counter` lets an upstream
  /// coordinator (e.g. a server-side dispatcher) share a single step
  /// series across multiple wrapped registries.
  #[tokio::test]
  async fn shared_step_index_counter_is_honored() {
    use std::sync::Mutex as StdMutex;

    struct StepIndexProbe {
      seen: Arc<StdMutex<Vec<usize>>>,
    }
    #[async_trait]
    impl PreToolHook for StepIndexProbe {
      fn name(&self) -> &str {
        "step_index_probe"
      }
      async fn before_tool(&self, call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
        self.seen.lock().unwrap().push(call.step_index);
        Ok(PreToolDecision::Allow)
      }
    }

    let counter = Arc::new(AtomicU64::new(42));
    let (tool, _) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let seen = Arc::new(StdMutex::new(Vec::<usize>::new()));
    let config = HookConfig::new(
      "sess-shared-step",
      Arc::new(AutoAllowApprovalProvider::new()),
      sinks,
    )
    .with_step_index_counter(counter.clone())
    .with_pre_hook(Arc::new(StepIndexProbe { seen: seen.clone() }));
    let registry = wrap_registry(registry, config);

    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();

    let observed = seen.lock().unwrap().clone();
    assert_eq!(
      observed,
      vec![42, 43],
      "Q3.10.1: injected counter must seed the step_index series"
    );
    // The counter is also bumped by the hook layer, so subsequent
    // increments from the upstream coordinator continue from 44.
    assert_eq!(counter.load(Ordering::SeqCst), 44);
  }

  /// Q3.10.2 regression — the synthetic gate events must carry the
  /// `stop-after-deny-<tool>-*` request_id namespace so downstream
  /// parsers can distinguish them from real provider responses, and
  /// must be `decided_by = "stop-after-deny-gate"` so the audit log
  /// makes the gate path obvious.
  #[tokio::test]
  async fn stop_after_deny_gate_emits_namespaced_events_with_redacted_params() {
    let (tool, _) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(DenyAndStopProvider), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);

    // First call trips deny-and-stop; second call hits the gate.
    let _ = registry.execute("probe", serde_json::json!({})).await;
    let _ = registry
      .execute(
        "probe",
        serde_json::json!({
          "url": "https://api.example.com",
          "headers": {"Authorization": "Bearer sk-leak-me"}
        }),
      )
      .await;

    let events = sink.snapshot().await;
    let gate_request = events
      .iter()
      .find_map(|e| match &e.body {
        HarnessEventBody::ApprovalRequested(payload)
          if payload.request.id.starts_with("stop-after-deny-") =>
        {
          Some(&payload.request)
        }
        _ => None,
      })
      .expect("Q3.10.2: synthetic ApprovalRequested for the gated second call must exist");
    assert!(
      gate_request.id.starts_with("stop-after-deny-probe-"),
      "request id must namespace tool name; got {}",
      gate_request.id
    );
    assert!(
      gate_request.reason.contains("deny-and-stop"),
      "reason must reference the originating decision; got {}",
      gate_request.reason
    );
    // Redaction must still apply on the synthetic path — sensitive
    // header values should not appear verbatim.
    let summary = serde_json::to_string(&gate_request.params_summary).unwrap();
    assert!(
      !summary.contains("sk-leak-me"),
      "synthetic gate event must not leak Authorization values; got {summary}"
    );

    let gate_decision = events
      .iter()
      .find_map(|e| match &e.body {
        HarnessEventBody::ApprovalDecided(payload)
          if payload.decision.request_id.starts_with("stop-after-deny-") =>
        {
          Some(&payload.decision)
        }
        _ => None,
      })
      .expect("Q3.10.2: synthetic ApprovalDecided for the gated call must exist");
    assert_eq!(gate_decision.decision, ApprovalOutcome::DenyAndStop);
    assert_eq!(gate_decision.decided_by, "stop-after-deny-gate");
  }

  struct CountingPostHook {
    counter: Arc<std::sync::Mutex<usize>>,
  }

  #[async_trait]
  impl PostToolHook for CountingPostHook {
    fn name(&self) -> &str {
      "counting_post_hook"
    }
    async fn after_tool(&self, _call: &CompletedToolCall) -> Result<(), HarnessError> {
      *self.counter.lock().unwrap() += 1;
      Ok(())
    }
  }

  /// Q1.7.2 regression: `ApprovalRequest.params_summary` emitted via
  /// the `ApprovalRequested` event must have sensitive fields
  /// redacted. Pre-fix the raw `params.clone()` flowed through to
  /// the JSONL / SSE sinks and an operator's prompt log could leak
  /// API keys or bearer tokens.
  #[tokio::test]
  async fn approval_request_redacts_sensitive_params_before_emit() {
    let (tool, _) = ProbeTool::new("probe", ToolIdempotency::NonIdempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_pre_hook(Arc::new(RequireApprovalHook));
    let registry = wrap_registry(registry, config);
    registry
      .execute(
        "probe",
        serde_json::json!({
          "url": "https://api.example.com/login",
          "headers": {"Authorization": "Bearer sk-live-abc123"},
          "api_key": "ant-api03-leak-me-not"
        }),
      )
      .await
      .unwrap();

    let events = sink.snapshot().await;
    let request = events
      .iter()
      .find_map(|e| match &e.body {
        HarnessEventBody::ApprovalRequested(payload) => Some(&payload.request),
        _ => None,
      })
      .expect("ApprovalRequested must fire");
    let rendered = serde_json::to_string(&request.params_summary).unwrap();
    assert!(
      !rendered.contains("sk-live-abc123"),
      "bearer token leaked to operator approval prompt: {rendered}"
    );
    assert!(
      !rendered.contains("ant-api03-leak-me-not"),
      "api_key leaked: {rendered}"
    );
    assert!(rendered.contains("api.example.com"));
  }

  #[tokio::test]
  async fn post_hook_runs_for_both_success_and_failure() {
    let (tool, _) = ProbeTool::new("probe", ToolIdempotency::Idempotent);
    let registry = build_registry(tool);
    let sink = Arc::new(InMemoryEventSink::new());
    let sinks = SinkChain::new().push(sink.clone() as Arc<dyn HarnessEventSink>);
    let post_counter = Arc::new(std::sync::Mutex::new(0_usize));
    let config = HookConfig::new("sess-1", Arc::new(AutoAllowApprovalProvider::new()), sinks)
      .with_post_hook(Arc::new(CountingPostHook {
        counter: post_counter.clone(),
      }));
    let registry = wrap_registry(registry, config);
    registry
      .execute("probe", serde_json::json!({}))
      .await
      .unwrap();
    assert_eq!(*post_counter.lock().unwrap(), 1);
  }
}
