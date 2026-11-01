use async_trait::async_trait;
use serde_json::Value;
use yanshi_memory::Message;

use crate::eval::PricingTable;

use super::support::compact_memory_summary;

/// Error type for ReAct agent operations
#[derive(Debug, thiserror::Error)]
pub enum ReActError {
  #[error("LLM error: {0}")]
  LlmError(#[from] yanshi_llm::LLMError),

  #[error("Memory error: {0}")]
  MemoryError(#[from] yanshi_memory::MemoryError),

  #[error("Tool error: {tool} — {message}")]
  ToolError { tool: String, message: String },

  #[error("Max iterations ({0}) reached without a final answer")]
  MaxIterationsReached(usize),

  #[error("Token budget exceeded: {used} / {budget}")]
  BudgetExceeded { used: u32, budget: u32 },

  #[error("Agent run cancelled: {reason}")]
  Cancelled { reason: String },

  #[error("Memory summary error: {message}")]
  MemorySummary { message: String },

  /// V2.1: `output_schema` was configured but the candidate final answer
  /// still failed validation after `max_schema_correction_attempts`
  /// retries. Unlike `VerificationStrategy` rejection (which force-accepts
  /// on exhaustion), a schema is a caller-declared hard contract — hard
  /// error rather than silently returning non-conformant output.
  #[error("Final answer did not match output_schema after {attempts} attempt(s): {errors:?}")]
  SchemaValidationFailed {
    errors: Vec<String>,
    attempts: usize,
  },

  /// V2.4: [`ReActAgent::resume_from_loop_checkpoint`](crate::react::ReActAgent::resume_from_loop_checkpoint) was handed a
  /// checkpoint that isn't a valid resume target for this agent — e.g. a
  /// `PlanExecute` checkpoint, or one with an unsupported schema version.
  #[error("cannot resume from loop checkpoint: {message}")]
  InvalidCheckpoint { message: String },
}

/// Input passed to a pluggable memory summary backend.
///
/// The runtime hands the backend the messages it had to drop in order to
/// fit the prompt budget along with the messages that were kept. Backends
/// can use either or both to produce a single string summary that the
/// runtime then prepends to the prompt as a synthetic system message.
#[derive(Debug, Clone)]
pub struct MemorySummaryContext {
  /// Session id of the run requesting the summary.
  pub session_id: String,
  /// Configured prompt-memory budget in approximate tokens.
  pub budget_tokens: u32,
  /// Approximate token count of the dropped messages.
  pub omitted_tokens: u32,
  /// Messages that did not fit and need summarising.
  pub omitted_messages: Vec<Message>,
  /// Messages that were kept verbatim in the prompt.
  pub kept_messages: Vec<Message>,
}

/// Pluggable backend for summarising prompt memory that exceeds a budget.
///
/// A backend receives a [`MemorySummaryContext`] describing what was kept
/// vs. dropped and returns:
///
/// - `Ok(Some(summary))` to inject `summary` as a synthetic system message
///   ahead of the kept messages.
/// - `Ok(None)` to skip the summary entirely (the runtime will silently
///   continue with truncated history).
/// - `Err(ReActError::MemorySummary { .. })` to surface a real failure.
///
/// Backends can be deterministic (rule-based) or LLM-backed; both should
/// stay on the synchronous side of the ReAct loop, so heavy work belongs
/// behind a separate task with a tight timeout.
#[async_trait]
pub trait MemorySummaryBackend: Send + Sync {
  /// Stable backend name (e.g. `"recent_only"`, `"compact"`).
  fn name(&self) -> &'static str;

  /// Produce an optional summary string for the omitted slice of memory.
  async fn summarize(&self, context: MemorySummaryContext) -> Result<Option<String>, ReActError>;
}

/// Summary backend that only records how much history was omitted.
#[derive(Debug, Default, Clone)]
pub struct RecentOnlyMemorySummary;

#[async_trait]
impl MemorySummaryBackend for RecentOnlyMemorySummary {
  fn name(&self) -> &'static str {
    "recent_only"
  }

  async fn summarize(&self, context: MemorySummaryContext) -> Result<Option<String>, ReActError> {
    Ok(Some(format!(
      "[Memory Summary]\n{} older messages omitted to fit the prompt memory budget (approx {} tokens).",
      context.omitted_messages.len(),
      context.omitted_tokens
    )))
  }
}

/// Deterministic rule-based summary backend for older prompt memory.
#[derive(Debug, Default, Clone)]
pub struct CompactMemorySummary;

#[async_trait]
impl MemorySummaryBackend for CompactMemorySummary {
  fn name(&self) -> &'static str {
    "compact"
  }

  async fn summarize(&self, context: MemorySummaryContext) -> Result<Option<String>, ReActError> {
    Ok(Some(compact_memory_summary(
      &context.omitted_messages,
      context.omitted_tokens,
    )))
  }
}

/// Configuration for a [`ReActAgent`](crate::react::ReActAgent).
#[derive(Debug, Clone)]
pub struct ReActConfig {
  /// LLM model identifier (e.g. `"gpt-4o"`, `"claude-3-5-sonnet"`)
  pub model: String,

  /// Optional persona / task description prepended to the system prompt.
  pub persona: Option<String>,

  /// Maximum number of tool-call iterations before giving up.
  pub max_iterations: usize,

  /// Stop after the session accumulates more than this many estimated tokens.
  /// `None` disables the token budget guard.
  pub budget_tokens: Option<u32>,

  /// Terminate if any of these strings appear in the LLM response.
  pub stop_conditions: Vec<String>,

  /// Enable reflection strategy execution when a strategy is attached.
  pub reflection_enabled: bool,

  /// Enable verification strategy execution when a strategy is attached.
  pub verification_enabled: bool,

  /// Maximum number of times a `VerificationStrategy` may reject a
  /// candidate final answer before the runtime force-accepts it and stops
  /// anyway. Bounds the verification loop so a strategy that never
  /// approves can't run forever. Only relevant when a strategy is
  /// attached and `verification_enabled` is `true`.
  pub max_verification_attempts: usize,

  /// Optional token budget for memory included in each LLM prompt.
  pub memory_prompt_token_budget: Option<u32>,

  /// Strategy used when prompt memory exceeds `memory_prompt_token_budget`.
  pub memory_summary_strategy: MemorySummaryStrategy,

  /// L1.2: sliding-window loop detection. `None` disables it. `Some(cfg)`
  /// stops the run with `AgentStopReason::LoopDetected` if the same
  /// `(tool, params)` signature appears `cfg.threshold` or more times
  /// within the last `cfg.window` tool calls — a safety net against a
  /// stuck loop burning through the step/tool-call/token budget instead
  /// of tripping any of them. This checks a wider window than F-A2-13
  /// (which only ever compares against the immediately prior call) and
  /// catches non-consecutive patterns (e.g. A, B, A, B, ...) too. Fires
  /// after `record_reflection` gets a chance to react, same as every
  /// other limit check.
  pub loop_detection: Option<LoopDetectionConfig>,

  /// T1.1: stop the run once cumulative LLM spend (computed from
  /// [`Self::pricing_table`]) crosses this many USD. `None` disables the
  /// guard. Overridable per-run via `AgentContext::limits.cost_limit_usd`
  /// (the context value wins when both are set, mirroring
  /// `Self::budget_tokens`/`RuntimeLimits::token_budget`).
  pub cost_limit_usd: Option<f64>,

  /// T1.1: pricing table used to translate each LLM call's token usage
  /// into a USD cost estimate, checked against [`Self::cost_limit_usd`]
  /// at the top of every turn. Defaults to an empty table (every call
  /// costs $0), so cost tracking is inert unless the caller configures
  /// real per-model prices — reuses `yanshi-agents::eval::pricing`
  /// rather than a second pricing representation.
  pub pricing_table: PricingTable,

  /// V2.1: JSON Schema the final answer must validate against once parsed
  /// as JSON. `None` (the default) disables structured-output enforcement
  /// entirely — byte-identical behaviour to before this existed.
  ///
  /// When set: (1) the LLM call additionally offers a synthetic
  /// `final_answer` tool (name: [`FINAL_ANSWER_TOOL_NAME`]) whose
  /// `input_schema` is this schema, so providers with native tool calling
  /// (all six today) can enforce the shape directly instead of relying on
  /// prompt-only constraint — calling it is recognised as the agent's final
  /// answer rather than a real tool dispatch; (2) a candidate answer that
  /// fails to parse as JSON or fails schema validation is rejected with the
  /// validation errors fed back into memory and the loop continues for
  /// another attempt, bounded by [`Self::max_schema_correction_attempts`].
  /// Unlike [`VerificationStrategy`](crate::verification::VerificationStrategy) rejection (which force-accepts once
  /// [`Self::max_verification_attempts`] is exhausted), exhausting the
  /// schema-correction budget is a hard [`ReActError::SchemaValidationFailed`]
  /// — a schema is a caller-declared hard contract, not an advisory
  /// critique, so returning a non-conformant answer labelled as final would
  /// silently break that contract.
  pub output_schema: Option<Value>,

  /// Maximum number of times a candidate final answer may fail
  /// [`Self::output_schema`] validation before the run gives up with
  /// [`ReActError::SchemaValidationFailed`]. Only relevant when
  /// `output_schema` is `Some`. Mirrors [`Self::max_verification_attempts`]'s
  /// shape but is tracked independently — schema correction and domain
  /// verification are separate gates with separate budgets.
  pub max_schema_correction_attempts: usize,
}

/// See [`ReActConfig::loop_detection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopDetectionConfig {
  /// How many of the most recent tool calls to consider.
  pub window: usize,
  /// How many times the same signature must appear within `window` to
  /// trip a stop.
  pub threshold: usize,
}

impl Default for LoopDetectionConfig {
  /// `threshold: 3` deliberately leaves room for F-A2-13's steering nudge
  /// (fired on the 2nd identical call) to work first — a legitimate retry
  /// pattern that repeats twice must not be treated as a stuck loop.
  fn default() -> Self {
    Self {
      window: 6,
      threshold: 3,
    }
  }
}

/// Strategy used to fit conversation memory into a prompt budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySummaryStrategy {
  /// Keep legacy behavior and include full memory.
  Disabled,
  /// Drop older messages and keep only the newest messages that fit.
  RecentOnly,
  /// Replace older messages with a deterministic compact summary.
  Compact,
}

impl Default for ReActConfig {
  fn default() -> Self {
    Self {
      model: "gpt-4o".to_string(),
      persona: None,
      max_iterations: 15,
      budget_tokens: Some(50_000),
      stop_conditions: vec![],
      reflection_enabled: true,
      verification_enabled: true,
      max_verification_attempts: 2,
      memory_prompt_token_budget: None,
      memory_summary_strategy: MemorySummaryStrategy::Disabled,
      loop_detection: Some(LoopDetectionConfig::default()),
      cost_limit_usd: None,
      pricing_table: PricingTable::default(),
      output_schema: None,
      max_schema_correction_attempts: 2,
    }
  }
}

impl ReActConfig {
  pub fn new(model: impl Into<String>) -> Self {
    Self {
      model: model.into(),
      ..Default::default()
    }
  }

  pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
    self.persona = Some(persona.into());
    self
  }

  pub fn with_max_iterations(mut self, n: usize) -> Self {
    self.max_iterations = n;
    self
  }

  pub fn with_budget_tokens(mut self, tokens: u32) -> Self {
    self.budget_tokens = Some(tokens);
    self
  }

  pub fn with_stop_conditions(mut self, conditions: Vec<String>) -> Self {
    self.stop_conditions = conditions;
    self
  }

  pub fn with_reflection_enabled(mut self, enabled: bool) -> Self {
    self.reflection_enabled = enabled;
    self
  }

  pub fn with_verification_enabled(mut self, enabled: bool) -> Self {
    self.verification_enabled = enabled;
    self
  }

  pub fn with_max_verification_attempts(mut self, attempts: usize) -> Self {
    self.max_verification_attempts = attempts;
    self
  }

  pub fn with_memory_prompt_token_budget(mut self, tokens: u32) -> Self {
    self.memory_prompt_token_budget = Some(tokens);
    self
  }

  pub fn with_memory_summary_strategy(mut self, strategy: MemorySummaryStrategy) -> Self {
    self.memory_summary_strategy = strategy;
    self
  }

  /// Configure loop detection (L1.2). See [`ReActConfig::loop_detection`].
  pub fn with_loop_detection(mut self, window: usize, threshold: usize) -> Self {
    self.loop_detection = Some(LoopDetectionConfig { window, threshold });
    self
  }

  /// Disable loop detection entirely.
  pub fn without_loop_detection(mut self) -> Self {
    self.loop_detection = None;
    self
  }

  /// Configure the USD spend cap. See [`ReActConfig::cost_limit_usd`].
  pub fn with_cost_limit_usd(mut self, budget_usd: f64) -> Self {
    self.cost_limit_usd = Some(budget_usd);
    self
  }

  /// Configure the pricing table used to cost each LLM call. See
  /// [`ReActConfig::pricing_table`].
  pub fn with_pricing_table(mut self, table: PricingTable) -> Self {
    self.pricing_table = table;
    self
  }

  /// Require the final answer to validate against `schema` (V2.1). See
  /// [`ReActConfig::output_schema`].
  pub fn with_output_schema(mut self, schema: Value) -> Self {
    self.output_schema = Some(schema);
    self
  }

  /// Configure the schema-correction retry budget. See
  /// [`ReActConfig::max_schema_correction_attempts`].
  pub fn with_max_schema_correction_attempts(mut self, attempts: usize) -> Self {
    self.max_schema_correction_attempts = attempts;
    self
  }
}

/// Name of the synthetic native tool a [`ReActConfig::output_schema`]-
/// configured agent offers alongside its real tools. Calling it is
/// recognised as the agent's final answer (its arguments become the
/// answer, validated against `output_schema`) rather than a real tool
/// dispatch — see `run_one_turn`'s tool-call parsing.
pub const FINAL_ANSWER_TOOL_NAME: &str = "final_answer";

/// V2.3: name of the synthetic native tool every `ReActAgent` offers
/// (unconditionally, unlike [`FINAL_ANSWER_TOOL_NAME`] which needs
/// `output_schema` to know what shape to validate — `ask_user` needs no
/// config to be safe, it's one inert extra tool spec until the model
/// chooses to call it). Calling it pauses the loop with
/// [`AgentStopReason::AwaitingInput`](crate::runtime::AgentStopReason::AwaitingInput) instead of dispatching a real
/// tool — see `run_one_turn`'s interception, checked *before* the
/// `final_answer` scan so a model batching both stops cleanly.
pub const ASK_USER_TOOL_NAME: &str = "ask_user";
