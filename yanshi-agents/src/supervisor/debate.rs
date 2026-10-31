//! Debate multi-agent collaboration: N participants propose answers in
//! parallel; over optional further rounds they revise their proposals after
//! seeing each other's; finally a judge agent renders the verdict.
//!
//! # Why?
//!
//! Useful when one model's answer is too risky to trust on its own — fact
//! checking, multi-perspective summarisation, code review with N reviewers,
//! etc. The judge can either pick a winner (`winner = Some(name)`) or
//! synthesise a merged answer (`winner = None`).
//!
//! # Lifecycle
//!
//! 1. The supervisor records the user's input as an `Observe` step.
//! 2. For each round 1..=`rounds`:
//!    - emit `DebateRoundStarted` event.
//!    - run every participant concurrently with the round's prompt
//!      (round 1 = the original input; later rounds include each
//!      participant's prior-round proposal so they can revise).
//!    - record one `DebateProposal` step per participant.
//! 3. Run the judge with all final-round proposals; record a
//!    `DebateVerdict` step + `DebateVerdictRendered` event.
//! 4. The judge's `answer` becomes the supervisor's `answer`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::runtime::{
  AgentContext, AgentEvent, AgentRunResult, AgentRuntime, AgentRuntimeError, AgentStep,
  AgentStepKind, AgentStopReason,
};
use crate::supervisor::common::{ChildBudget, build_child_context};

const DEFAULT_JUDGE_PROMPT: &str = "\
Several specialist agents have independently considered the following user \
request and produced their own answers. As the judge, read each proposal \
carefully, identify points of agreement and disagreement, then produce a \
single best answer. If one proposal is clearly superior you may pick it \
verbatim; otherwise synthesise the strongest combined answer.";

/// One participant's proposal for a single debate round.
#[derive(Debug, Clone)]
struct ProposalRecord {
  agent: String,
  /// `None` means the agent failed to produce an answer this round.
  proposal: Option<String>,
  /// Captured for diagnostic logging in future revisions; intentionally
  /// retained even though the current judge prompt does not surface it.
  #[allow(dead_code)]
  stop_reason: AgentStopReason,
}

#[derive(Debug, thiserror::Error)]
pub enum DebateSupervisorError {
  #[error("DebateSupervisor needs at least one participant")]
  NoParticipants,
  #[error("DebateSupervisor needs a judge agent")]
  NoJudge,
  #[error("Duplicate participant name '{0}'")]
  DuplicateParticipant(String),
  #[error("rounds must be ≥ 1")]
  ZeroRounds,
  #[error("delegation spec registered for unknown participant '{0}'")]
  UnknownDelegationSpecAgent(String),
}

/// A multi-agent runtime where participants propose answers in parallel and
/// a judge produces the final verdict.
///
/// Implements [`AgentRuntime`] so it can be embedded in [`AgentNode`].
///
/// [`AgentNode`]: crate::nodes::AgentNode
pub struct DebateSupervisor {
  /// Participants in registration order so traces are deterministic.
  participants: Vec<(String, super::common::SharedAgentRuntime)>,
  judge: super::common::SharedAgentRuntime,
  rounds: usize,
  judge_prompt_template: String,
  session_id: String,
  /// W3.3: optional per-agent `DelegationSpec` (keyed by participant name,
  /// or `"judge"` for the judge), consulted after each turn to validate
  /// that agent's answer.
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
  last_schema_validations: HashMap<String, crate::delegation::SchemaValidation>,
}

impl std::fmt::Debug for DebateSupervisor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let participants: Vec<&str> = self.participants.iter().map(|(n, _)| n.as_str()).collect();
    f.debug_struct("DebateSupervisor")
      .field("session_id", &self.session_id)
      .field("rounds", &self.rounds)
      .field("participants", &participants)
      .finish()
  }
}

impl DebateSupervisor {
  pub fn session_id(&self) -> &str {
    &self.session_id
  }

  pub fn participant_names(&self) -> Vec<&str> {
    self.participants.iter().map(|(n, _)| n.as_str()).collect()
  }

  /// Schema-validation outcomes (W3.3) from the most recent run, keyed by
  /// participant name (or `"judge"`) — only populated for agents with a
  /// [`DelegationSpec`](crate::delegation::DelegationSpec) registered via
  /// [`DebateSupervisorBuilder::with_delegation_spec`] /
  /// [`DebateSupervisorBuilder::with_judge_delegation_spec`].
  pub fn last_schema_validations(&self) -> &HashMap<String, crate::delegation::SchemaValidation> {
    &self.last_schema_validations
  }

  fn record_delegation_validation(&mut self, agent_name: &str, answer: Option<&str>) {
    super::common::record_delegation_validation(
      &self.delegation_specs,
      &mut self.last_schema_validations,
      agent_name,
      answer,
    );
  }

  /// Convenience: run a one-shot task and return the judge's answer.
  pub async fn run(&mut self, task: &str) -> Result<String, AgentRuntimeError> {
    let context = AgentContext::new(self.session_id.clone(), task, "");
    let result = AgentRuntime::run(self, context).await?;
    result
      .answer
      .ok_or_else(|| AgentRuntimeError::ExecutionFailed {
        message: format!(
          "DebateSupervisor stopped without a final answer: {:?}",
          result.stop_reason
        ),
      })
  }

  fn build_participant_input(&self, original: &str, prior: Option<&[ProposalRecord]>) -> String {
    let Some(prior) = prior else {
      return original.to_string();
    };
    let mut buf = String::from(original);
    buf.push_str("\n\nOther agents previously proposed:\n");
    for record in prior {
      let body = record.proposal.as_deref().unwrap_or("(no answer)");
      buf.push_str(&format!("- {}: {}\n", record.agent, body));
    }
    buf.push_str(
      "\nReview these proposals critically, then produce your own revised answer. \
       Respond with the final improved answer only.",
    );
    buf
  }

  fn build_judge_input(&self, original: &str, finals: &[ProposalRecord]) -> String {
    let mut buf = self.judge_prompt_template.clone();
    buf.push_str("\n\nUser request:\n");
    buf.push_str(original);
    buf.push_str("\n\nProposals:\n");
    for record in finals {
      let body = record.proposal.as_deref().unwrap_or("(no answer)");
      buf.push_str(&format!("- {}: {}\n", record.agent, body));
    }
    buf
  }
}

#[async_trait]
impl AgentRuntime for DebateSupervisor {
  async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
    let session_id = context.session_id.clone();
    let mut steps: Vec<AgentStep> = Vec::new();
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut step_index = 0usize;

    events.push(AgentEvent::RunStarted {
      session_id: session_id.clone(),
      model: format!(
        "multi_agent:debate(participants={},rounds={})",
        self.participants.len(),
        self.rounds
      ),
      timestamp: context.started_at,
    });
    steps.push(AgentStep::new(
      step_index,
      AgentStepKind::Observe {
        input: context.input.clone(),
      },
    ));
    step_index += 1;

    let cancellation = context.cancellation_token.clone();
    if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
      return Ok(stopped(
        session_id,
        None,
        AgentStopReason::Cancelled {
          message: "cancellation token signalled".into(),
        },
        steps,
        events,
      ));
    }

    let mut last_round_proposals: Vec<ProposalRecord> = Vec::new();
    let mut budget = ChildBudget::start(&context.limits);

    for round in 1..=self.rounds {
      let limits = match budget.limits_for(&context.limits, self.participants.len()) {
        Ok(limits) => limits,
        Err(spent) => return Ok(stopped(session_id, None, spent, steps, events)),
      };
      let participant_names: Vec<String> =
        self.participants.iter().map(|(n, _)| n.clone()).collect();

      events.push(AgentEvent::DebateRoundStarted {
        session_id: session_id.clone(),
        round,
        participants: participant_names.clone(),
        timestamp: Utc::now(),
      });

      let prior_for_input = if round == 1 {
        None
      } else {
        Some(last_round_proposals.as_slice())
      };
      let round_input = self.build_participant_input(&context.input, prior_for_input);

      // Spawn all participants concurrently.
      let mut handles: Vec<(
        String,
        tokio::task::JoinHandle<Result<AgentRunResult, AgentRuntimeError>>,
      )> = Vec::with_capacity(self.participants.len());
      for (name, handle) in &self.participants {
        let agent_handle = handle.clone();
        let child_ctx = build_child_context(&context, name, &round_input, limits.clone());
        let task = tokio::spawn(async move {
          let mut guard = agent_handle.lock().await;
          guard.run(child_ctx).await
        });
        handles.push((name.clone(), task));
      }

      // Collect results in registration order so the trace is deterministic.
      let mut round_proposals: Vec<ProposalRecord> = Vec::with_capacity(handles.len());
      let mut handles = handles.into_iter();
      while let Some((name, handle)) = handles.next() {
        match handle.await {
          Ok(Ok(child_result)) => {
            budget.charge(&child_result);
            step_index =
              merge_child_into(&mut steps, &mut events, step_index, child_result.clone());
            if super::common::halts_supervisor(&child_result.stop_reason) {
              for (_, rest) in handles {
                rest.abort();
              }
              return Ok(stopped(
                session_id,
                None,
                child_result.stop_reason,
                steps,
                events,
              ));
            }
            let proposal = child_result.answer.clone();
            self.record_delegation_validation(&name, proposal.as_deref());
            let proposal_step_index = step_index;
            steps.push(AgentStep::new(
              proposal_step_index,
              AgentStepKind::DebateProposal {
                round,
                agent: name.clone(),
                proposal: proposal.clone().unwrap_or_default(),
              },
            ));
            step_index += 1;
            round_proposals.push(ProposalRecord {
              agent: name,
              proposal,
              stop_reason: child_result.stop_reason,
            });
          }
          Ok(Err(e)) => {
            // Surfaced agent error: record an empty proposal, keep debating.
            round_proposals.push(ProposalRecord {
              agent: name.clone(),
              proposal: None,
              stop_reason: AgentStopReason::Error {
                message: e.to_string(),
              },
            });
            let proposal_step_index = step_index;
            steps.push(AgentStep::new(
              proposal_step_index,
              AgentStepKind::DebateProposal {
                round,
                agent: name,
                proposal: String::new(),
              },
            ));
            step_index += 1;
          }
          Err(join_err) => {
            return Err(AgentRuntimeError::ExecutionFailed {
              message: format!("DebateSupervisor: participant join failed: {join_err}"),
            });
          }
        }
      }

      last_round_proposals = round_proposals;

      if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
        return Ok(stopped(
          session_id,
          None,
          AgentStopReason::Cancelled {
            message: "cancellation token signalled".into(),
          },
          steps,
          events,
        ));
      }
    }

    // Run the judge with the final-round proposals.
    let judge_input = self.build_judge_input(&context.input, &last_round_proposals);
    let limits = match budget.limits_for(&context.limits, 1) {
      Ok(limits) => limits,
      Err(spent) => return Ok(stopped(session_id, None, spent, steps, events)),
    };
    let judge_ctx = build_child_context(&context, "judge", &judge_input, limits);
    let judge_result = {
      let mut guard = self.judge.lock().await;
      guard.run(judge_ctx).await?
    };
    self.record_delegation_validation("judge", judge_result.answer.as_deref());
    step_index = merge_child_into(&mut steps, &mut events, step_index, judge_result.clone());

    let verdict_index = step_index;
    let answer = judge_result.answer.clone();
    let rationale = answer.clone().unwrap_or_else(|| {
      format!(
        "judge stopped without an answer: {:?}",
        judge_result.stop_reason
      )
    });
    steps.push(AgentStep::new(
      verdict_index,
      AgentStepKind::DebateVerdict {
        winner: None, // judge synthesises rather than voting
        rationale,
      },
    ));
    events.push(AgentEvent::DebateVerdictRendered {
      session_id: session_id.clone(),
      step_index: verdict_index,
      winner: None,
      timestamp: Utc::now(),
    });
    step_index += 1;
    let _ = step_index;

    // The judge's own stop reason is the debate's: a judge that ran out of
    // steps or was denied did not deliver a final answer.
    Ok(stopped(
      session_id,
      answer,
      judge_result.stop_reason,
      steps,
      events,
    ))
  }

  fn runtime_name(&self) -> &'static str {
    "debate"
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn merge_child_into(
  steps: &mut Vec<AgentStep>,
  events: &mut Vec<AgentEvent>,
  mut next_index: usize,
  child: AgentRunResult,
) -> usize {
  let mut index_map: HashMap<usize, usize> = HashMap::new();
  for mut step in child.steps {
    let original = step.index;
    step.index = next_index;
    index_map.insert(original, next_index);
    steps.push(step);
    next_index += 1;
  }
  for mut event in child.events {
    rewrite_event_step_index(&mut event, &index_map);
    events.push(event);
  }
  next_index
}

fn rewrite_event_step_index(event: &mut AgentEvent, map: &HashMap<usize, usize>) {
  match event {
    AgentEvent::StepStarted { step_index, .. }
    | AgentEvent::ToolCallStarted { step_index, .. }
    | AgentEvent::ToolPolicyDecision { step_index, .. }
    | AgentEvent::ToolCapabilityDecision { step_index, .. }
    | AgentEvent::ToolCallCompleted { step_index, .. }
    | AgentEvent::LlmCallCompleted { step_index, .. }
    | AgentEvent::ReflectionAdded { step_index, .. }
    | AgentEvent::HandoffOccurred { step_index, .. }
    | AgentEvent::BlackboardWritten { step_index, .. }
    | AgentEvent::DebateVerdictRendered { step_index, .. } => {
      if let Some(remapped) = map.get(step_index) {
        *step_index = *remapped;
      }
    }
    AgentEvent::StepCompleted { step, .. } => {
      if let Some(remapped) = map.get(&step.index) {
        step.index = *remapped;
      }
    }
    // Events without a `step_index` need no remap — and neither does any
    // future variant, since `AgentEvent` is `#[non_exhaustive]`.
    _ => {}
  }
}

fn stopped(
  session_id: String,
  answer: Option<String>,
  reason: AgentStopReason,
  steps: Vec<AgentStep>,
  mut events: Vec<AgentEvent>,
) -> AgentRunResult {
  events.push(AgentEvent::RunStopped {
    session_id: session_id.clone(),
    reason: reason.clone(),
    timestamp: Utc::now(),
  });
  AgentRunResult {
    session_id,
    answer,
    stop_reason: reason,
    steps,
    events,
  }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Builder for [`DebateSupervisor`].
pub struct DebateSupervisorBuilder {
  participants: Vec<DebateAgentSpec>,
  judge: Option<Box<dyn AgentRuntime>>,
  rounds: usize,
  judge_prompt_template: String,
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
}

struct DebateAgentSpec {
  name: String,
  agent: Box<dyn AgentRuntime>,
}

impl Default for DebateSupervisorBuilder {
  fn default() -> Self {
    Self {
      participants: Vec::new(),
      judge: None,
      rounds: 1,
      judge_prompt_template: DEFAULT_JUDGE_PROMPT.to_string(),
      delegation_specs: HashMap::new(),
    }
  }
}

impl DebateSupervisorBuilder {
  pub fn new() -> Self {
    Self::default()
  }

  /// Register a participant. Names must be unique. Accepts any
  /// [`AgentRuntime`] (W3.2), not just `ReActAgent`.
  pub fn add_participant<A: AgentRuntime + 'static>(
    mut self,
    name: impl Into<String>,
    agent: A,
  ) -> Self {
    self.participants.push(DebateAgentSpec {
      name: name.into(),
      agent: Box::new(agent),
    });
    self
  }

  /// Set the judge agent. Required. Accepts any [`AgentRuntime`] (W3.2).
  pub fn judge<A: AgentRuntime + 'static>(mut self, agent: A) -> Self {
    self.judge = Some(Box::new(agent));
    self
  }

  /// Number of proposal rounds. Defaults to 1.
  pub fn rounds(mut self, rounds: usize) -> Self {
    self.rounds = rounds;
    self
  }

  /// Override the judge's system prompt template.
  pub fn judge_prompt(mut self, template: impl Into<String>) -> Self {
    self.judge_prompt_template = template.into();
    self
  }

  /// Register a [`DelegationSpec`](crate::delegation::DelegationSpec) for
  /// an already-registered participant (W3.3): after each of its turns,
  /// the supervisor validates its answer against the spec's
  /// `expected_output_schema` (when set) and records the outcome, readable
  /// via [`DebateSupervisor::last_schema_validations`]. `build()` rejects
  /// a spec registered for a name that was never passed to
  /// [`Self::add_participant`].
  pub fn with_delegation_spec(
    mut self,
    name: impl Into<String>,
    spec: crate::delegation::DelegationSpec,
  ) -> Self {
    self.delegation_specs.insert(name.into(), spec);
    self
  }

  /// Register a [`DelegationSpec`](crate::delegation::DelegationSpec) for
  /// the judge (W3.3), validated the same way as a participant's, under
  /// the fixed key `"judge"` in [`DebateSupervisor::last_schema_validations`].
  pub fn with_judge_delegation_spec(mut self, spec: crate::delegation::DelegationSpec) -> Self {
    self.delegation_specs.insert("judge".to_string(), spec);
    self
  }

  pub fn build(self) -> Result<DebateSupervisor, DebateSupervisorError> {
    if self.participants.is_empty() {
      return Err(DebateSupervisorError::NoParticipants);
    }
    if self.rounds == 0 {
      return Err(DebateSupervisorError::ZeroRounds);
    }
    let mut seen = std::collections::HashSet::new();
    for spec in &self.participants {
      if !seen.insert(spec.name.clone()) {
        return Err(DebateSupervisorError::DuplicateParticipant(
          spec.name.clone(),
        ));
      }
    }
    let judge = self.judge.ok_or(DebateSupervisorError::NoJudge)?;

    for name in self.delegation_specs.keys() {
      if name != "judge" && !seen.contains(name) {
        return Err(DebateSupervisorError::UnknownDelegationSpecAgent(
          name.clone(),
        ));
      }
    }

    let participants: Vec<(String, super::common::SharedAgentRuntime)> = self
      .participants
      .into_iter()
      .map(|s| (s.name, Arc::new(AsyncMutex::new(s.agent))))
      .collect();

    Ok(DebateSupervisor {
      participants,
      judge: Arc::new(AsyncMutex::new(judge)),
      rounds: self.rounds,
      judge_prompt_template: self.judge_prompt_template,
      session_id: Uuid::new_v4().to_string(),
      delegation_specs: self.delegation_specs,
      last_schema_validations: HashMap::new(),
    })
  }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  use serde_json::json;
  use yanshi_llm::Yanshi;
  use yanshi_memory::SessionMemory;
  use yanshi_tool::ToolRegistry;

  use crate::react::{ReActAgent, ReActConfig};

  fn solo_agent(model: &str) -> ReActAgent {
    ReActAgent::new(
      ReActConfig::new(model).with_max_iterations(2),
      Box::new(SessionMemory::default_window()),
      Arc::new(ToolRegistry::new()),
    )
  }

  async fn init_mock_model(model: &str) {
    let path =
      std::env::temp_dir().join(format!("yanshi-debate-mock-{}.yml", uuid::Uuid::new_v4()));
    std::fs::write(
      &path,
      format!(
        r#"
models:
  {model}:
    vendor: mock
    type: text
    model_id: {model}
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#
      ),
    )
    .unwrap();
    Yanshi::init_with_config(path.to_str().unwrap())
      .await
      .unwrap();
  }

  fn set_mock_responses(responses: Vec<&str>) {
    let s = serde_json::to_string(&responses).unwrap();
    // SAFETY: callers hold crate::LLM_TEST_LOCK to serialise env mutation.
    unsafe {
      std::env::set_var("YANSHI_MOCK_RESPONSES", s);
      std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    }
  }

  // ── Builder validation ────────────────────────────────────────────────────

  #[tokio::test]
  async fn builder_rejects_empty_participants() {
    let err = DebateSupervisorBuilder::new()
      .judge(solo_agent("mock"))
      .build()
      .unwrap_err();
    assert!(matches!(err, DebateSupervisorError::NoParticipants));
  }

  #[tokio::test]
  async fn builder_rejects_missing_judge() {
    let err = DebateSupervisorBuilder::new()
      .add_participant("a", solo_agent("mock"))
      .build()
      .unwrap_err();
    assert!(matches!(err, DebateSupervisorError::NoJudge));
  }

  #[tokio::test]
  async fn builder_rejects_duplicate_participant_names() {
    let err = DebateSupervisorBuilder::new()
      .add_participant("a", solo_agent("mock"))
      .add_participant("a", solo_agent("mock"))
      .judge(solo_agent("mock"))
      .build()
      .unwrap_err();
    assert!(matches!(
      err,
      DebateSupervisorError::DuplicateParticipant(_)
    ));
  }

  #[tokio::test]
  async fn builder_rejects_zero_rounds() {
    let err = DebateSupervisorBuilder::new()
      .add_participant("a", solo_agent("mock"))
      .judge(solo_agent("mock"))
      .rounds(0)
      .build()
      .unwrap_err();
    assert!(matches!(err, DebateSupervisorError::ZeroRounds));
  }

  // ── Helper formatting ────────────────────────────────────────────────────

  #[test]
  fn judge_input_includes_user_request_and_all_proposals() {
    let supervisor = DebateSupervisorBuilder::new()
      .add_participant("a", solo_agent("mock"))
      .add_participant("b", solo_agent("mock"))
      .judge(solo_agent("mock"))
      .build()
      .unwrap();
    let proposals = vec![
      ProposalRecord {
        agent: "a".into(),
        proposal: Some("answer-a".into()),
        stop_reason: AgentStopReason::FinalAnswer,
      },
      ProposalRecord {
        agent: "b".into(),
        proposal: None,
        stop_reason: AgentStopReason::Error {
          message: "boom".into(),
        },
      },
    ];
    let prompt = supervisor.build_judge_input("explain rust", &proposals);
    assert!(prompt.contains("explain rust"));
    assert!(prompt.contains("a: answer-a"));
    assert!(prompt.contains("b: (no answer)"));
  }

  // ── End-to-end via mock LLM ───────────────────────────────────────────────

  #[tokio::test]
  async fn one_round_two_participants_and_judge_returns_judge_answer() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-debate-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // The mock provider serves responses in FIFO order; both participants
      // run concurrently so their consumption order is non-deterministic.
      // We make all participant responses interchangeable so tests are stable.
      r#"{"thought":"propose","answer":"proposal-1"}"#,
      r#"{"thought":"propose","answer":"proposal-2"}"#,
      // Judge:
      r#"{"thought":"verdict","answer":"final synthesised answer"}"#,
    ]);
    init_mock_model(&model).await;

    let mut supervisor = DebateSupervisorBuilder::new()
      .add_participant("alpha", solo_agent(&model))
      .add_participant("beta", solo_agent(&model))
      .judge(solo_agent(&model))
      .build()
      .unwrap();

    let context = AgentContext::new("session-1", "what is rust?", &model);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    assert_eq!(result.answer.as_deref(), Some("final synthesised answer"));
    assert!(matches!(result.stop_reason, AgentStopReason::FinalAnswer));

    // Two DebateProposal steps + one DebateVerdict step expected.
    let proposals: Vec<&AgentStep> = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::DebateProposal { .. }))
      .collect();
    assert_eq!(proposals.len(), 2);

    let verdicts: Vec<&AgentStep> = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::DebateVerdict { .. }))
      .collect();
    assert_eq!(verdicts.len(), 1);
    if let AgentStepKind::DebateVerdict { rationale, .. } = &verdicts[0].kind {
      assert_eq!(rationale, "final synthesised answer");
    }

    // Round-started + verdict-rendered events both present.
    assert!(
      result
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::DebateRoundStarted { round: 1, .. })),
      "DebateRoundStarted for round 1 must be present"
    );
    assert!(
      result
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::DebateVerdictRendered { .. })),
      "DebateVerdictRendered must be present"
    );
  }

  #[tokio::test]
  async fn two_rounds_emit_two_round_started_events() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-debate-2r-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // Round 1: 2 proposals
      r#"{"thought":"p1","answer":"p1-r1"}"#,
      r#"{"thought":"p2","answer":"p2-r1"}"#,
      // Round 2: 2 revised proposals
      r#"{"thought":"p1r","answer":"p1-r2"}"#,
      r#"{"thought":"p2r","answer":"p2-r2"}"#,
      // Judge
      r#"{"thought":"final","answer":"verdict"}"#,
    ]);
    init_mock_model(&model).await;

    let mut supervisor = DebateSupervisorBuilder::new()
      .add_participant("alpha", solo_agent(&model))
      .add_participant("beta", solo_agent(&model))
      .judge(solo_agent(&model))
      .rounds(2)
      .build()
      .unwrap();

    let context = AgentContext::new("session-1", "topic", &model);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    let round_starts: usize = result
      .events
      .iter()
      .filter(|e| matches!(e, AgentEvent::DebateRoundStarted { .. }))
      .count();
    assert_eq!(round_starts, 2);

    // 2 rounds × 2 participants = 4 proposal steps.
    let proposals = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::DebateProposal { .. }))
      .count();
    assert_eq!(proposals, 4);

    assert_eq!(result.answer.as_deref(), Some("verdict"));
  }

  #[tokio::test]
  async fn pre_cancelled_token_short_circuits_debate() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-debate-cancel-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![]);
    init_mock_model(&model).await;

    let mut supervisor = DebateSupervisorBuilder::new()
      .add_participant("a", solo_agent(&model))
      .judge(solo_agent(&model))
      .build()
      .unwrap();

    let token = crate::runtime::AgentCancellationToken::new();
    token.cancel();
    let context = AgentContext::new("session-1", "x", &model).with_cancellation_token(token);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();
    assert!(matches!(
      result.stop_reason,
      AgentStopReason::Cancelled { .. }
    ));
  }

  /// W3.2 regression: pre-fix, `add_participant`/`judge` required a
  /// concrete `ReActAgent`. This stub needs no LLM at all, proving any
  /// `AgentRuntime` impl can now serve as a participant or judge.
  struct StubRuntime {
    answer: String,
  }
  #[async_trait]
  impl AgentRuntime for StubRuntime {
    async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
      Ok(AgentRunResult {
        session_id: context.session_id,
        answer: Some(self.answer.clone()),
        stop_reason: AgentStopReason::FinalAnswer,
        steps: Vec::new(),
        events: Vec::new(),
      })
    }
    fn runtime_name(&self) -> &'static str {
      "stub"
    }
  }

  #[tokio::test]
  async fn add_participant_and_judge_accept_non_react_agent_runtimes() {
    let mut supervisor = DebateSupervisorBuilder::new()
      .add_participant(
        "alpha",
        StubRuntime {
          answer: "alpha proposal".to_string(),
        },
      )
      .judge(StubRuntime {
        answer: "judge verdict".to_string(),
      })
      .build()
      .unwrap();
    let answer = supervisor.run("hello").await.unwrap();
    assert_eq!(answer, "judge verdict");
  }

  /// W3.3 regression: a `DelegationSpec` registered via
  /// `with_delegation_spec`/`with_judge_delegation_spec` must actually be
  /// consulted by the run loop for both participants and the judge.
  #[tokio::test]
  async fn delegation_specs_validate_participant_and_judge_answers() {
    let participant_spec = crate::delegation::DelegationSpec::new("propose")
      .with_expected_output_schema(json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"]
      }));
    let judge_spec =
      crate::delegation::DelegationSpec::new("judge").with_expected_output_schema(json!({
        "type": "object",
        "properties": { "verdict": { "type": "string" } },
        "required": ["verdict"]
      }));
    let mut supervisor = DebateSupervisorBuilder::new()
      .add_participant(
        "alpha",
        StubRuntime {
          answer: r#"{"ok":true}"#.to_string(),
        },
      )
      .judge(StubRuntime {
        answer: "not json".to_string(),
      })
      .with_delegation_spec("alpha", participant_spec)
      .with_judge_delegation_spec(judge_spec)
      .build()
      .unwrap();
    supervisor.run("hello").await.unwrap();

    let validations = supervisor.last_schema_validations();
    assert_eq!(
      validations.get("alpha"),
      Some(&crate::delegation::SchemaValidation::Valid)
    );
    assert!(
      !validations.get("judge").unwrap().is_valid(),
      "judge's non-JSON answer must fail schema validation"
    );
  }

  #[test]
  fn build_rejects_delegation_spec_for_unknown_participant() {
    let result = DebateSupervisorBuilder::new()
      .add_participant(
        "alpha",
        StubRuntime {
          answer: "x".to_string(),
        },
      )
      .judge(StubRuntime {
        answer: "y".to_string(),
      })
      .with_delegation_spec(
        "does-not-exist",
        crate::delegation::DelegationSpec::new("goal"),
      )
      .build();
    assert!(matches!(
      result,
      Err(DebateSupervisorError::UnknownDelegationSpecAgent(_))
    ));
  }
}
