//! Eval runner: walks a [`Dataset`], drives a [`crate::AgentRuntime`]
//! per case, evaluates assertions, and emits a structured report.
//!
//! The runner is **synchronous over cases** in this slice: each case
//! builds a fresh runtime via the supplied [`AgentRuntimeFactory`] and
//! runs it to completion before moving on to the next. Parallelism via
//! `yanshi_core::Flow` is tracked separately under P4.4 slice 3.
//!
//! The runner is also intentionally agnostic of how runtimes are built —
//! tests pass a stub factory that returns pre-canned [`crate::runtime::AgentRunResult`]s;
//! the CLI builds real [`crate::react::ReActAgent`]s. The harness's job is
//! only to glue the dataset, runtime, and assertions together.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::runtime::{
  AgentContext, AgentRuntime, AgentRuntimeError, AgentStopReason, RuntimeLimits,
};

use super::assertion::{AssertionContext, AssertionOutcome, SkillValidator, SkillValidatorVerdict};
use super::dataset::{Dataset, EvalCase};
use super::pricing::PricingTable;

/// Boxed skill validator closure returned by
/// [`AgentRuntimeFactory::skill_validator`]. Closure returns the rich
/// [`SkillValidatorVerdict`] so the assertion layer can surface
/// per-verdict reason text (see `docs/SKILL_VALIDATOR_PROTOCOL.md`).
pub type BoxedSkillValidator<'a> = Box<dyn Fn(&str) -> SkillValidatorVerdict + Send + Sync + 'a>;

/// Errors surfaced by the runner. Dataset / assertion errors are kept
/// distinct from runtime errors so a CI gate can tell "this dataset was
/// malformed" from "this run actually executed and failed".
#[derive(Debug, Error)]
pub enum EvalRunnerError {
  /// The factory could not produce a runtime for the case.
  #[error("runtime factory failed for case '{case_id}': {message}")]
  FactoryFailed { case_id: String, message: String },

  /// The runtime itself returned an `AgentRuntimeError`.
  #[error("runtime error in case '{case_id}': {source}")]
  RuntimeFailed {
    case_id: String,
    #[source]
    source: AgentRuntimeError,
  },
}

/// Final per-case status; matches the `status` field documented in
/// `docs/AGENT_EVAL_FORMAT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
  /// Every assertion passed AND `stop_reason.is_success()`.
  Passed,
  /// At least one assertion failed, OR the run produced a non-success
  /// stop reason.
  Failed,
  /// The case was filtered out by `EvalRunner::with_filter` / similar.
  /// Skipped cases do not count toward pass/fail.
  Skipped,
}

impl CaseStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Passed => "passed",
      Self::Failed => "failed",
      Self::Skipped => "skipped",
    }
  }
}

/// Per-case report. Mirrors the JSON envelope in
/// `docs/AGENT_EVAL_FORMAT.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseReport {
  pub id: String,
  pub status: CaseStatus,
  /// Trace id reused as the agent's session_id. Operators feed this to
  /// `yanshi trace replay <trace_id>` for failure debugging.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub trace_id: Option<String>,
  pub started_at: DateTime<Utc>,
  pub finished_at: DateTime<Utc>,
  pub duration_ms: u64,
  pub cost_usd_actual: f64,
  /// String tag matching the `AgentStopReason` `reason` discriminant.
  /// `"skipped"` for filtered cases.
  pub stop_reason: String,
  pub step_count: usize,
  pub tool_call_count: usize,
  #[serde(default)]
  pub assertions: Vec<AssertionOutcome>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub notes: Option<String>,
  /// Free-form runner-level error description when `status == Failed`
  /// because the runtime itself errored (vs an assertion failure).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub runtime_error: Option<String>,
}

/// Aggregate summary across all cases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalSummary {
  pub total: usize,
  pub passed: usize,
  pub failed: usize,
  pub skipped: usize,
  pub cost_usd_total: f64,
  pub latency_ms_p50: u64,
  pub latency_ms_p95: u64,
}

/// Top-level report envelope. Matches the JSON shape in
/// `docs/AGENT_EVAL_FORMAT.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
  pub schema_version: u32,
  pub dataset: String,
  pub dataset_version: String,
  pub started_at: DateTime<Utc>,
  pub finished_at: DateTime<Utc>,
  pub summary: EvalSummary,
  pub cases: Vec<CaseReport>,
}

impl EvalReport {
  /// Convenience helper for callers wiring `--fail-on-status`. Returns
  /// `true` when any case is `Failed` (skipped does not count).
  pub fn has_failures(&self) -> bool {
    self.summary.failed > 0
  }
}

/// Build an [`AgentRuntime`] for one [`EvalCase`]. Implementations vary
/// from "always returns a canned result" (tests) to "construct a fresh
/// `ReActAgent` with the right registry / memory / model" (real CLI).
#[async_trait]
pub trait AgentRuntimeFactory: Send + Sync {
  /// Return an owned runtime instance ready to execute the case.
  ///
  /// Implementations should not call `runtime.run(...)` themselves —
  /// the runner does that. Returning an `Err` aborts evaluation of
  /// the current case with a [`CaseStatus::Failed`] and a
  /// `runtime_error` in the report.
  async fn build(&self, case: &EvalCase) -> Result<Box<dyn AgentRuntime>, EvalRunnerError>;

  /// Optional per-case skill validator wired into the
  /// `final_answer_matches_skill` assertion. `None` (the default)
  /// means "skill declares no validator" — the assertion will fail
  /// for that case if it is used.
  fn skill_validator<'a>(&'a self, _case: &'a EvalCase) -> Option<BoxedSkillValidator<'a>> {
    None
  }
}

/// Optional filter applied per-case. Returning `true` keeps the case;
/// `false` flags it as skipped (still emitted in the report).
pub type CaseFilter = dyn Fn(&EvalCase) -> bool + Send + Sync;

/// Main entry point. Build with [`Self::new`], optionally chain
/// [`Self::with_filter`], then call [`Self::run`].
pub struct EvalRunner<'a> {
  dataset: &'a Dataset,
  factory: &'a dyn AgentRuntimeFactory,
  filter: Option<Box<CaseFilter>>,
  pricing: PricingTable,
}

impl<'a> EvalRunner<'a> {
  pub fn new(dataset: &'a Dataset, factory: &'a dyn AgentRuntimeFactory) -> Self {
    Self {
      dataset,
      factory,
      filter: None,
      pricing: PricingTable::empty(),
    }
  }

  /// Apply a filter; cases for which `predicate` returns `false` are
  /// reported as `Skipped`.
  pub fn with_filter<F>(mut self, predicate: F) -> Self
  where
    F: Fn(&EvalCase) -> bool + Send + Sync + 'static,
  {
    self.filter = Some(Box::new(predicate));
    self
  }

  /// Attach a [`PricingTable`] so per-case `cost_usd_actual` reflects
  /// the configured per-model rates. Defaults to an empty table
  /// (everything costs $0). Used by `yanshi eval run` to plug
  /// `~/.yanshi/pricing.yml`.
  pub fn with_pricing(mut self, pricing: PricingTable) -> Self {
    self.pricing = pricing;
    self
  }

  /// Execute every case sequentially and return the aggregated report.
  pub async fn run(&self) -> EvalReport {
    let started_at = Utc::now();
    let mut cases: Vec<CaseReport> = Vec::with_capacity(self.dataset.cases.len());
    let mut latencies: Vec<u64> = Vec::with_capacity(self.dataset.cases.len());
    let mut cost_total: f64 = 0.0;

    for case in &self.dataset.cases {
      if let Some(filter) = self.filter.as_ref()
        && !filter(case)
      {
        cases.push(CaseReport {
          id: case.id.clone(),
          status: CaseStatus::Skipped,
          trace_id: None,
          started_at: Utc::now(),
          finished_at: Utc::now(),
          duration_ms: 0,
          cost_usd_actual: 0.0,
          stop_reason: "skipped".to_string(),
          step_count: 0,
          tool_call_count: 0,
          assertions: Vec::new(),
          notes: case.notes.clone(),
          runtime_error: None,
        });
        continue;
      }

      let report = self.run_one(case).await;
      latencies.push(report.duration_ms);
      cost_total += report.cost_usd_actual;
      cases.push(report);
    }

    let finished_at = Utc::now();
    let summary = aggregate(&cases, cost_total, &mut latencies);

    EvalReport {
      schema_version: 1,
      dataset: self.dataset.manifest.name.clone(),
      dataset_version: self.dataset.manifest.version.clone(),
      started_at,
      finished_at,
      summary,
      cases,
    }
  }

  async fn run_one(&self, case: &EvalCase) -> CaseReport {
    let case_started = Utc::now();
    let started_instant = Instant::now();
    let trace_id = generate_trace_id(&case.id);

    let mut runtime = match self.factory.build(case).await {
      Ok(rt) => rt,
      Err(e) => {
        let elapsed = started_instant.elapsed();
        return CaseReport {
          id: case.id.clone(),
          status: CaseStatus::Failed,
          trace_id: Some(trace_id),
          started_at: case_started,
          finished_at: Utc::now(),
          duration_ms: elapsed.as_millis() as u64,
          cost_usd_actual: 0.0,
          stop_reason: "factory_error".to_string(),
          step_count: 0,
          tool_call_count: 0,
          assertions: Vec::new(),
          notes: case.notes.clone(),
          runtime_error: Some(e.to_string()),
        };
      }
    };

    let limits = limits_from_case(case);
    let model = case
      .model
      .clone()
      .unwrap_or_else(|| "mock-model".to_string());
    let context =
      AgentContext::new(trace_id.clone(), case.prompt.clone(), model).with_limits(limits);

    let run_result = runtime.run(context).await;
    let run_outcome = match run_result {
      Ok(result) => result,
      Err(e) => {
        let elapsed = started_instant.elapsed();
        return CaseReport {
          id: case.id.clone(),
          status: CaseStatus::Failed,
          trace_id: Some(trace_id),
          started_at: case_started,
          finished_at: Utc::now(),
          duration_ms: elapsed.as_millis() as u64,
          cost_usd_actual: 0.0,
          stop_reason: "runtime_error".to_string(),
          step_count: 0,
          tool_call_count: 0,
          assertions: Vec::new(),
          notes: case.notes.clone(),
          runtime_error: Some(e.to_string()),
        };
      }
    };

    let elapsed = started_instant.elapsed();
    let validator = self.factory.skill_validator(case);
    let validator_ref: Option<&SkillValidator<'_>> = validator
      .as_deref()
      .map(|boxed| boxed as &SkillValidator<'_>);

    let assertion_ctx = AssertionContext {
      steps: &run_outcome.steps,
      final_answer: run_outcome.answer.as_deref(),
      skill_validator: validator_ref,
    };
    let assertion_outcomes: Vec<AssertionOutcome> = case
      .expected_assertions
      .iter()
      .map(|a| a.evaluate(&assertion_ctx))
      .collect();

    let assertions_passed = assertion_outcomes.iter().all(|o| o.passed);
    let stop_success = run_outcome.stop_reason.is_success();
    let tool_call_count = run_outcome
      .steps
      .iter()
      .filter(|s| matches!(s.kind, crate::runtime::AgentStepKind::ToolCall { .. }))
      .count();

    // Aggregate per-call token usage from the run's events. Providers
    // that don't report usage contribute zero (None → 0); the pricing
    // table similarly returns zero when neither the model id nor the
    // table's `default` row is set.
    let cost_usd_actual = aggregate_cost(&run_outcome.events, &self.pricing);

    // Per-case cost limit enforcement. The agent runtime itself does
    // not (yet) emit `AgentStopReason::CostLimitExceeded` — the eval
    // harness re-stamps the case status when the post-run cost crosses
    // the budget. The agent's actual stop_reason stays in the report
    // for trace replay; `runtime_error` carries the cost-limit
    // explanation so the failure row is self-explanatory.
    let cost_exceeded = case
      .cost_limit_usd
      .is_some_and(|budget| cost_usd_actual > budget);

    let status = if cost_exceeded || !assertions_passed || !stop_success {
      CaseStatus::Failed
    } else {
      CaseStatus::Passed
    };

    let cost_limit_reason = if cost_exceeded {
      Some(format!(
        "cost_limit_usd exceeded: ${:.4} > ${:.4}",
        cost_usd_actual,
        case.cost_limit_usd.unwrap_or(0.0)
      ))
    } else {
      None
    };

    CaseReport {
      id: case.id.clone(),
      status,
      trace_id: Some(trace_id),
      started_at: case_started,
      finished_at: Utc::now(),
      duration_ms: elapsed.as_millis() as u64,
      cost_usd_actual,
      stop_reason: if cost_exceeded {
        "cost_limit_exceeded".to_string()
      } else {
        stop_reason_label(&run_outcome.stop_reason)
      },
      step_count: run_outcome.steps.len(),
      tool_call_count,
      assertions: assertion_outcomes,
      notes: case.notes.clone(),
      runtime_error: cost_limit_reason.or_else(|| match &run_outcome.stop_reason {
        AgentStopReason::Error { message } => Some(message.clone()),
        _ => None,
      }),
    }
  }
}

/// Aggregate the total USD cost of a finished run by walking its
/// `LlmCallCompleted` events and multiplying each by the table's
/// pricing for the call's model.
fn aggregate_cost(events: &[crate::runtime::AgentEvent], pricing: &PricingTable) -> f64 {
  let mut total = 0.0_f64;
  for event in events {
    if let crate::runtime::AgentEvent::LlmCallCompleted {
      model,
      prompt_tokens,
      completion_tokens,
      ..
    } = event
    {
      let p = pricing.lookup(model);
      total += p.cost_for_call(*prompt_tokens, *completion_tokens);
    }
  }
  total
}

fn aggregate(cases: &[CaseReport], cost_total: f64, latencies: &mut [u64]) -> EvalSummary {
  let total = cases.len();
  let passed = cases
    .iter()
    .filter(|c| c.status == CaseStatus::Passed)
    .count();
  let failed = cases
    .iter()
    .filter(|c| c.status == CaseStatus::Failed)
    .count();
  let skipped = cases
    .iter()
    .filter(|c| c.status == CaseStatus::Skipped)
    .count();

  latencies.sort();
  let p50 = percentile(latencies, 50);
  let p95 = percentile(latencies, 95);

  EvalSummary {
    total,
    passed,
    failed,
    skipped,
    cost_usd_total: cost_total,
    latency_ms_p50: p50,
    latency_ms_p95: p95,
  }
}

fn percentile(sorted: &[u64], pct: u32) -> u64 {
  if sorted.is_empty() {
    return 0;
  }
  // `pct` is in [0, 100]; idx = ceil(len * pct / 100) - 1, clamped to
  // [0, len-1]. Nearest-rank method — good enough for an eval summary.
  let len = sorted.len();
  let raw = (len as u64 * pct as u64).div_ceil(100);
  let idx = (raw.max(1) - 1) as usize;
  sorted[idx.min(len - 1)]
}

fn limits_from_case(case: &EvalCase) -> RuntimeLimits {
  let defaults = RuntimeLimits::default();
  RuntimeLimits {
    max_steps: case.max_steps.or(defaults.max_steps),
    max_tool_calls: case.max_tool_calls.or(defaults.max_tool_calls),
    timeout_ms: case.latency_limit_ms.or(defaults.timeout_ms),
    token_budget: case.token_budget.or(defaults.token_budget),
    // T1.1: a factory-built real ReActAgent/PlanExecuteAgent now enforces
    // this in-loop; the post-hoc `cost_exceeded` re-stamp below stays as
    // a harness-level safety net for runtimes that don't (e.g. custom
    // `AgentRuntime` test doubles), not as the only enforcement point.
    cost_limit_usd: case.cost_limit_usd.or(defaults.cost_limit_usd),
  }
}

fn generate_trace_id(case_id: &str) -> String {
  // Use the case id plus a millisecond timestamp so reruns in the same
  // dataset directory don't collide. The hex padding keeps the id
  // sortable by start time.
  let ts = chrono::Utc::now().timestamp_millis();
  format!("eval-{case_id}-{ts:013x}")
}

fn stop_reason_label(reason: &AgentStopReason) -> String {
  match reason {
    AgentStopReason::FinalAnswer => "final_answer".to_string(),
    AgentStopReason::StopCondition { .. } => "stop_condition".to_string(),
    AgentStopReason::MaxSteps { .. } => "max_steps".to_string(),
    AgentStopReason::MaxToolCalls { .. } => "max_tool_calls".to_string(),
    AgentStopReason::Timeout { .. } => "timeout".to_string(),
    AgentStopReason::Cancelled { .. } => "cancelled".to_string(),
    AgentStopReason::TokenBudgetExceeded { .. } => "token_budget_exceeded".to_string(),
    AgentStopReason::CostLimitExceeded { .. } => "cost_limit_exceeded".to_string(),
    AgentStopReason::LoopDetected { .. } => "loop_detected".to_string(),
    AgentStopReason::Error { .. } => "error".to_string(),
    AgentStopReason::AwaitingInput { .. } => "awaiting_input".to_string(),
    AgentStopReason::ApprovalDenied { .. } => "approval_denied".to_string(),
  }
}

/// Time helper used by callers that want to compute their own latency
/// bucket. Exposed so the CLI can format report rows consistently with
/// the runner.
pub fn duration_to_ms(d: Duration) -> u64 {
  d.as_millis() as u64
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::eval::assertion::{Assertion, AssertionTarget};
  use crate::eval::dataset::{DatasetManifest, EvalCaseDefaults};
  use crate::runtime::{AgentRunResult, AgentStep, AgentStepKind};
  use serde_json::json;
  use std::collections::BTreeMap;

  // ── Stub runtime + factory ─────────────────────────────────────────────

  /// A fake `AgentRuntime` that returns whatever result the factory
  /// stashed for the case id. Lets the runner be tested without touching
  /// the LLM provider or tool registry.
  struct StubRuntime {
    result: AgentRunResult,
  }

  #[async_trait]
  impl AgentRuntime for StubRuntime {
    fn runtime_name(&self) -> &'static str {
      "stub"
    }

    async fn run(&mut self, _context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
      Ok(self.result.clone())
    }
  }

  struct StubFactory {
    /// case_id → canned result.
    results: std::sync::Mutex<BTreeMap<String, AgentRunResult>>,
    /// case_id → factory error to inject.
    factory_errors: std::sync::Mutex<BTreeMap<String, String>>,
    /// case_id → validator closure (Some(true|false) | None).
    validators: std::sync::Mutex<BTreeMap<String, Option<bool>>>,
  }

  impl StubFactory {
    fn new() -> Self {
      Self {
        results: std::sync::Mutex::new(BTreeMap::new()),
        factory_errors: std::sync::Mutex::new(BTreeMap::new()),
        validators: std::sync::Mutex::new(BTreeMap::new()),
      }
    }

    fn with_result(self, case_id: &str, result: AgentRunResult) -> Self {
      self
        .results
        .lock()
        .unwrap()
        .insert(case_id.to_string(), result);
      self
    }

    fn with_factory_error(self, case_id: &str, message: &str) -> Self {
      self
        .factory_errors
        .lock()
        .unwrap()
        .insert(case_id.to_string(), message.to_string());
      self
    }
  }

  #[async_trait]
  impl AgentRuntimeFactory for StubFactory {
    async fn build(&self, case: &EvalCase) -> Result<Box<dyn AgentRuntime>, EvalRunnerError> {
      if let Some(msg) = self.factory_errors.lock().unwrap().get(&case.id) {
        return Err(EvalRunnerError::FactoryFailed {
          case_id: case.id.clone(),
          message: msg.clone(),
        });
      }
      let result = self
        .results
        .lock()
        .unwrap()
        .get(&case.id)
        .cloned()
        .unwrap_or_else(|| AgentRunResult::final_answer(format!("trace-{}", case.id), ""));
      Ok(Box::new(StubRuntime { result }))
    }

    fn skill_validator<'a>(&'a self, case: &'a EvalCase) -> Option<BoxedSkillValidator<'a>> {
      let verdict = self.validators.lock().unwrap().get(&case.id).copied();
      verdict.map(|v| -> BoxedSkillValidator<'a> {
        Box::new(move |_| match v {
          Some(true) => SkillValidatorVerdict::Pass,
          Some(false) => SkillValidatorVerdict::Fail {
            reason: "stub validator rejected".to_string(),
          },
          None => SkillValidatorVerdict::Unrunnable {
            reason: "stub validator unrunnable".to_string(),
          },
        })
      })
    }
  }

  // ── Fixtures ──────────────────────────────────────────────────────────

  fn make_dataset(cases: Vec<EvalCase>) -> Dataset {
    Dataset {
      manifest: DatasetManifest {
        schema_version: 1,
        name: "fixture".to_string(),
        version: "0.0.1".to_string(),
        description: None,
        source: None,
        license: None,
        defaults: EvalCaseDefaults::default(),
      },
      root: std::path::PathBuf::from("."),
      cases,
    }
  }

  fn case(id: &str, prompt: &str, assertions: Vec<Assertion>) -> EvalCase {
    EvalCase {
      id: id.to_string(),
      prompt: prompt.to_string(),
      skill: None,
      model: Some("mock-model".to_string()),
      tools_allowed: Vec::new(),
      tools_denied: Vec::new(),
      inputs: BTreeMap::new(),
      max_steps: Some(4),
      max_tool_calls: None,
      cost_limit_usd: None,
      latency_limit_ms: None,
      token_budget: None,
      expected_assertions: assertions,
      notes: None,
    }
  }

  fn run_result_with(answer: &str, steps: Vec<AgentStep>) -> AgentRunResult {
    let mut r = AgentRunResult::final_answer("trace", answer);
    r.steps = steps;
    r
  }

  // ── Tests ─────────────────────────────────────────────────────────────

  #[tokio::test]
  async fn passes_when_assertions_match_and_run_succeeded() {
    let dataset = make_dataset(vec![case(
      "c1",
      "say hi",
      vec![Assertion::Contains {
        needle: "hi".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let factory = StubFactory::new().with_result(
      "c1",
      run_result_with(
        "hi there",
        vec![AgentStep::new(
          0,
          AgentStepKind::FinalAnswer {
            answer: "hi there".to_string(),
          },
        )],
      ),
    );
    let runner = EvalRunner::new(&dataset, &factory);
    let report = runner.run().await;
    assert_eq!(report.summary.total, 1);
    assert_eq!(report.summary.passed, 1);
    assert_eq!(report.summary.failed, 0);
    assert_eq!(report.cases[0].status, CaseStatus::Passed);
    assert_eq!(report.cases[0].stop_reason, "final_answer");
    assert!(report.cases[0].trace_id.is_some());
  }

  #[tokio::test]
  async fn fails_when_assertion_misses_even_if_run_succeeded() {
    let dataset = make_dataset(vec![case(
      "c1",
      "say hi",
      vec![Assertion::Contains {
        needle: "bye".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let factory = StubFactory::new().with_result("c1", run_result_with("hi there", vec![]));
    let report = EvalRunner::new(&dataset, &factory).run().await;
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.cases[0].status, CaseStatus::Failed);
    let outcome = &report.cases[0].assertions[0];
    assert!(!outcome.passed);
    assert!(outcome.reason.as_ref().unwrap().contains("bye"));
  }

  #[tokio::test]
  async fn fails_when_stop_reason_is_non_success_even_if_assertions_pass() {
    let dataset = make_dataset(vec![case(
      "c1",
      "loop",
      vec![Assertion::StepCountBelow { max_steps: 100 }], // trivially true
    )]);
    let mut result = run_result_with("", vec![]);
    result.stop_reason = AgentStopReason::MaxSteps { max_steps: 5 };
    let factory = StubFactory::new().with_result("c1", result);
    let report = EvalRunner::new(&dataset, &factory).run().await;
    assert_eq!(report.cases[0].status, CaseStatus::Failed);
    assert_eq!(report.cases[0].stop_reason, "max_steps");
  }

  #[tokio::test]
  async fn factory_error_reports_runtime_error_field() {
    let dataset = make_dataset(vec![case(
      "broken",
      "x",
      vec![Assertion::Contains {
        needle: "x".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let factory = StubFactory::new().with_factory_error("broken", "registry not initialized");
    let report = EvalRunner::new(&dataset, &factory).run().await;
    assert_eq!(report.cases[0].status, CaseStatus::Failed);
    assert_eq!(report.cases[0].stop_reason, "factory_error");
    assert!(
      report.cases[0]
        .runtime_error
        .as_ref()
        .unwrap()
        .contains("registry not initialized")
    );
  }

  #[tokio::test]
  async fn filter_marks_unselected_cases_as_skipped() {
    let dataset = make_dataset(vec![
      case(
        "keep",
        "x",
        vec![Assertion::Contains {
          needle: "x".to_string(),
          target: AssertionTarget::FinalAnswer,
          case_insensitive: false,
        }],
      ),
      case(
        "skip",
        "y",
        vec![Assertion::Contains {
          needle: "y".to_string(),
          target: AssertionTarget::FinalAnswer,
          case_insensitive: false,
        }],
      ),
    ]);
    let factory = StubFactory::new()
      .with_result("keep", run_result_with("x is here", vec![]))
      .with_result("skip", run_result_with("y is here", vec![]));
    let runner = EvalRunner::new(&dataset, &factory).with_filter(|c| c.id == "keep");
    let report = runner.run().await;
    assert_eq!(report.summary.total, 2);
    assert_eq!(report.summary.passed, 1);
    assert_eq!(report.summary.skipped, 1);
    assert_eq!(report.cases[1].status, CaseStatus::Skipped);
    assert_eq!(report.cases[1].stop_reason, "skipped");
  }

  #[tokio::test]
  async fn cost_limit_exceeded_maps_to_failed_with_label() {
    let dataset = make_dataset(vec![case(
      "expensive",
      "do thing",
      vec![Assertion::Contains {
        needle: "thing".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let mut result = run_result_with("thing happens", vec![]);
    result.stop_reason = AgentStopReason::CostLimitExceeded {
      used_usd: 0.50,
      budget_usd: 0.10,
    };
    let factory = StubFactory::new().with_result("expensive", result);
    let report = EvalRunner::new(&dataset, &factory).run().await;
    assert_eq!(report.cases[0].status, CaseStatus::Failed);
    assert_eq!(report.cases[0].stop_reason, "cost_limit_exceeded");
  }

  #[tokio::test]
  async fn tool_call_count_counts_only_tool_call_steps() {
    let dataset = make_dataset(vec![case(
      "c1",
      "x",
      vec![Assertion::ToolCalled {
        tool: "search".to_string(),
        min_count: 1,
        max_count: usize::MAX,
        with_params: None,
      }],
    )]);
    let steps = vec![
      AgentStep::new(
        0,
        AgentStepKind::Plan {
          thought: "plan".to_string(),
        },
      ),
      AgentStep::new(
        1,
        AgentStepKind::ToolCall {
          tool: "search".to_string(),
          params: json!({}),
        },
      ),
      AgentStep::new(
        2,
        AgentStepKind::ToolResult {
          tool: "search".to_string(),
          content: "ok".to_string(),
          is_error: false,
          parts: vec![],
        },
      ),
      AgentStep::new(
        3,
        AgentStepKind::FinalAnswer {
          answer: "done".to_string(),
        },
      ),
    ];
    let factory = StubFactory::new().with_result("c1", run_result_with("done", steps));
    let report = EvalRunner::new(&dataset, &factory).run().await;
    assert_eq!(report.cases[0].step_count, 4);
    assert_eq!(report.cases[0].tool_call_count, 1);
    assert!(report.cases[0].assertions[0].passed);
  }

  #[test]
  fn percentile_handles_empty_and_single_element() {
    assert_eq!(percentile(&[], 50), 0);
    assert_eq!(percentile(&[42], 50), 42);
    assert_eq!(percentile(&[42], 95), 42);
  }

  #[test]
  fn percentile_nearest_rank_matches_documented_behavior() {
    // For [10, 20, 30, 40, 50]:
    //   p50 = ceil(5 * 50 / 100) = 3 → idx 2 → 30
    //   p95 = ceil(5 * 95 / 100) = 5 → idx 4 → 50
    let sorted = vec![10u64, 20, 30, 40, 50];
    assert_eq!(percentile(&sorted, 50), 30);
    assert_eq!(percentile(&sorted, 95), 50);
  }

  #[tokio::test]
  async fn cost_usd_actual_aggregates_llm_call_events_against_pricing_table() {
    let dataset = make_dataset(vec![case(
      "c1",
      "x",
      vec![Assertion::Contains {
        needle: "x".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let mut result = run_result_with("x here", vec![]);
    result.events = vec![
      crate::runtime::AgentEvent::LlmCallCompleted {
        session_id: "trace".to_string(),
        step_index: 0,
        model: "mock-model".to_string(),
        prompt_tokens: Some(2_000),
        completion_tokens: Some(500),
        total_tokens: Some(2_500),
        duration_ms: 100,
        timestamp: chrono::Utc::now(),
      },
      // A second call to verify the runner sums per-call costs.
      crate::runtime::AgentEvent::LlmCallCompleted {
        session_id: "trace".to_string(),
        step_index: 1,
        model: "mock-model".to_string(),
        prompt_tokens: Some(1_000),
        completion_tokens: Some(0),
        total_tokens: Some(1_000),
        duration_ms: 80,
        timestamp: chrono::Utc::now(),
      },
    ];
    let factory = StubFactory::new().with_result("c1", result);
    let pricing = crate::eval::PricingTable::default().with_model(
      "mock-model",
      crate::eval::ModelPricing {
        input_per_1k: 1.0,
        output_per_1k: 2.0,
        ..Default::default()
      },
    );
    let runner = EvalRunner::new(&dataset, &factory).with_pricing(pricing);
    let report = runner.run().await;
    // First call:  2.0 input + 1.0 output = $3.0
    // Second call: 1.0 input + 0.0 output = $1.0
    // Total:                                 $4.0
    let cost = report.cases[0].cost_usd_actual;
    assert!((cost - 4.0).abs() < 1e-9, "cost = {cost}");
    // Summary aggregates per-case costs.
    assert!((report.summary.cost_usd_total - 4.0).abs() < 1e-9);
  }

  #[tokio::test]
  async fn cost_over_limit_flips_status_to_failed_with_runtime_error() {
    let mut c = case(
      "c1",
      "x",
      vec![Assertion::Contains {
        needle: "x".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    );
    c.cost_limit_usd = Some(0.005); // $0.005 budget
    let dataset = make_dataset(vec![c]);
    let mut result = run_result_with("x here", vec![]);
    result.events = vec![crate::runtime::AgentEvent::LlmCallCompleted {
      session_id: "trace".to_string(),
      step_index: 0,
      model: "mock-model".to_string(),
      prompt_tokens: Some(10_000), // 10k input × $0.001 = $0.010 > $0.005
      completion_tokens: Some(0),
      total_tokens: Some(10_000),
      duration_ms: 100,
      timestamp: chrono::Utc::now(),
    }];
    let factory = StubFactory::new().with_result("c1", result);
    let pricing = crate::eval::PricingTable::default().with_model(
      "mock-model",
      crate::eval::ModelPricing {
        input_per_1k: 0.001,
        output_per_1k: 0.001,
        ..Default::default()
      },
    );
    let report = EvalRunner::new(&dataset, &factory)
      .with_pricing(pricing)
      .run()
      .await;
    let case = &report.cases[0];
    assert_eq!(case.status, CaseStatus::Failed);
    assert_eq!(case.stop_reason, "cost_limit_exceeded");
    assert!(
      case
        .runtime_error
        .as_ref()
        .unwrap()
        .contains("cost_limit_usd"),
      "runtime_error: {:?}",
      case.runtime_error
    );
    assert!((case.cost_usd_actual - 0.010).abs() < 1e-9);
  }

  #[tokio::test]
  async fn cost_usd_actual_zero_when_no_llm_events() {
    let dataset = make_dataset(vec![case(
      "c1",
      "x",
      vec![Assertion::Contains {
        needle: "x".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    )]);
    let factory = StubFactory::new().with_result("c1", run_result_with("x here", vec![]));
    let pricing = crate::eval::PricingTable::default().with_default(crate::eval::ModelPricing {
      input_per_1k: 100.0,
      output_per_1k: 100.0,
      ..Default::default()
    });
    let report = EvalRunner::new(&dataset, &factory)
      .with_pricing(pricing)
      .run()
      .await;
    // No LlmCallCompleted events on the canned result → $0.0
    // regardless of how expensive the pricing table claims the model is.
    assert_eq!(report.cases[0].cost_usd_actual, 0.0);
  }

  #[test]
  fn limits_from_case_passes_case_fields_through() {
    let mut c = case(
      "c1",
      "x",
      vec![Assertion::Contains {
        needle: "x".to_string(),
        target: AssertionTarget::FinalAnswer,
        case_insensitive: false,
      }],
    );
    c.token_budget = Some(8_000);
    c.latency_limit_ms = Some(5_000);
    let limits = limits_from_case(&c);
    assert_eq!(limits.max_steps, Some(4));
    assert_eq!(limits.token_budget, Some(8_000));
    assert_eq!(limits.timeout_ms, Some(5_000));
    assert_eq!(limits.max_tool_calls, None);
  }
}
