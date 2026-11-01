//! `yanshi workflow dynamic` — LLM-authored, governed dynamic workflow.
//!
//! The dynamic-workflow paradigm (see `docs/ARCHITECTURE.md` § Four Execution
//! Paradigms): instead of a fixed DAG file or an agent looping tool-by-tool, an
//! LLM makes ONE up-front planning call that emits a declarative `WorkflowPlan`,
//! which `compile_plan_to_flow` turns into an `yanshi-graph` `Flow` the core
//! executor runs deterministically — and, under `FlowExecutionMode::Concurrent`,
//! in parallel wherever the plan's `depends_on` edges allow.
//!
//! Because the plan is LLM-authored and then *executed*, every tool call is
//! governed:
//!
//! - the built-in tools carry a **restrictive** [`SandboxPolicy`] — file paths
//!   and HTTP domains must be granted explicitly via `--allow-path` /
//!   `--allow-domain`, and the shell tool is never registered;
//! - `--allow-modalities` opts in the `yanshi-tools-ai` modality Tool
//!   adapters (tts/asr/text_to_image/image_to_image/image_edit/
//!   image_understand/text_to_video) — off by default, same rationale as
//!   shell: they're billed vendor API calls an adversarial-by-construction
//!   plan shouldn't reach without an explicit operator grant;
//! - `--dry-run` prints the plan without executing it, so an operator can audit
//!   what the model intends before any tool runs;
//! - `--approve none|cli|auto-allow|auto-deny` routes every call through the
//!   Harness [`wrap_registry`] approval pipeline (the same `Arc<ToolRegistry>`
//!   is shared by the planner and the compiler, so governance is not
//!   bypassed); [`RequireApprovalForEveryCall`] makes that true regardless of
//!   `--profile`, since `HookConfig`'s own escalation only auto-requires
//!   approval for `production`. T1.3: leaving `--approve` unset defaults to
//!   `cli` under `local`/`production` — an LLM-authored plan is adversarial
//!   by construction — and to `none` (unsupervised) only under `dev`. Pass
//!   `--approve none` explicitly to opt out on any profile.
//! - `--replan <n>` (W2.1) recovers from step failures instead of failing the
//!   whole run: [`DynamicWorkflowAgent::run_with_replan`] reuses already-
//!   succeeded step results and asks the planner for a revised plan covering
//!   only what's still missing, up to `n` rounds. Every round's new tool
//!   calls flow through the same governed (and, when `--approve` is set,
//!   approval-wrapped) registry as round 0 — no separate re-approval wiring
//!   needed. Defaults to `0` (disabled, a failed step fails the run, matching
//!   pre-W2.1 behavior); has no effect under `--dry-run`, since nothing
//!   executes there for a step to fail and trigger a revision.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result};

use async_trait::async_trait;

use serde_json::{Value, json};
use yanshi_agents::dynamic::{
  DynamicWorkflowAgent, DynamicWorkflowRunOutcome, WorkflowPlan, compile_plan_to_flow,
  plan_step_results,
};
use yanshi_core::async_node::AsyncNodeResult;
use yanshi_core::{FlowExecutionConfig, FlowExt, FlowValue};
use yanshi_harness::{
  ApprovalProvider, ApprovalRisk, AutoAllowApprovalProvider, AutoDenyApprovalProvider,
  CliApprovalProvider, HarnessError, HarnessEventSink, HookConfig, PendingToolCall,
  PreToolDecision, PreToolHook, SinkChain, StdoutEventSink, wrap_registry,
};
use yanshi_llm::Yanshi;
use yanshi_tools::builtin::{FileTool, HttpTool};
use yanshi_tools::{SandboxPolicy, ToolRegistry};

use crate::commands::harness::{parse_profile, resolve_approve_default};

/// Run the `workflow dynamic` command.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
  goal: String,
  model: Option<String>,
  allow_path: Vec<String>,
  allow_domain: Vec<String>,
  allow_modalities: bool,
  approve: Option<String>,
  profile: String,
  dry_run: bool,
  max_concurrency: usize,
  replan: usize,
  output: String,
) -> Result<()> {
  let model = model.context(
    "workflow dynamic requires --model <model> to author the plan with (the LLM that plans)",
  )?;
  let as_json = match output.as_str() {
    "text" => false,
    "json" => true,
    other => anyhow::bail!("unsupported --output '{other}', expected text | json"),
  };
  let profile = parse_profile(&profile)?;
  let approve = resolve_approve_default(approve, profile);

  Yanshi::init()
    .await
    .context("failed to initialise Yanshi LLM config — is your API key configured?")?;

  // Built-in tools under a restrictive sandbox: the LLM authors the plan, so the
  // operator — not the model — decides which paths/domains are reachable.
  let policy = Arc::new(build_policy(&allow_path, &allow_domain));
  let mut registry = ToolRegistry::new();
  registry.register(Arc::new(FileTool::new(policy.clone())));
  registry.register(Arc::new(
    HttpTool::new(policy.clone()).context("failed to build the built-in HTTP tool")?,
  ));
  // `--allow-modalities`: opt the LLM-authored plan into the modality Tool
  // adapters (tts/asr/text_to_image/...). Off by default — same rationale
  // as shell never being registered here: these are billed vendor API
  // calls, so an adversarial-by-construction plan shouldn't reach them
  // without the operator explicitly granting it.
  if allow_modalities {
    yanshi_tools_ai::register_all(&mut registry);
  }

  // Optionally decorate every tool with the Harness approval/audit pipeline.
  let registry = match approve_provider(&approve)? {
    None => Arc::new(registry),
    Some(provider) => {
      let sinks =
        SinkChain::new().push(Arc::new(StdoutEventSink::new()) as Arc<dyn HarnessEventSink>);
      let session_id = format!("dynamic-{}", uuid::Uuid::new_v4().simple());
      // T1.3: `HookConfig`'s own profile-based escalation only auto-requires
      // approval for `NonIdempotent` calls under `HarnessProfile::Production`
      // — under `Dev`/`Local` it would otherwise auto-allow every call even
      // with a provider wired, silently defeating `--approve`. Attaching
      // `RequireApprovalForEveryCall` makes "a provider is configured" and
      // "every call is actually gated" mean the same thing on every profile,
      // matching this module's own doc claim that `--approve` "routes every
      // call through the Harness approval pipeline."
      let hook_config = HookConfig::new(session_id, provider, sinks)
        .with_profile(profile)
        .with_pre_hook(Arc::new(RequireApprovalForEveryCall))
        .with_seq_counter(Arc::new(AtomicU64::new(0)));
      wrap_registry(registry, hook_config)
    }
  };

  // One up-front planning call against the (governed) tool table. The agent
  // requires a `FlowRunner` for its own `run()`, though this command compiles +
  // executes the plan itself below (to support `--dry-run`).
  let agent = DynamicWorkflowAgent::new(
    &model,
    Arc::clone(&registry),
    Arc::new(yanshi_core::CoreFlowRunner::concurrent(max_concurrency)),
  );
  if dry_run {
    let plan = agent
      .plan(&goal)
      .await
      .map_err(|err| anyhow::anyhow!("dynamic workflow planning failed: {err}"))?;
    if as_json {
      println!("{}", serde_json::to_string_pretty(&plan_to_json(&plan))?);
    } else {
      print!("{}", render_plan_text(&plan));
      let note = if replan > 0 {
        "\n(dry run — plan not executed; --replan has no effect without execution)"
      } else {
        "\n(dry run — plan not executed)"
      };
      println!("{note}");
    }
    return Ok(());
  }

  // replan > 0: recover from step failures by asking the planner for a
  // revised plan (L1.1's `run_with_replan`) instead of failing outright.
  // The eventual `plan` can differ from round 0 (steps may be replaced
  // across revisions), so unlike the no-replan path below there is no
  // single up-front plan to show before execution starts.
  let (plan, state, revisions) = if replan > 0 {
    if !as_json {
      println!("Executing (up to {replan} replan round(s) on failure)...");
    }
    let outcome: DynamicWorkflowRunOutcome = agent
      .run_with_replan(&goal, replan)
      .await
      .map_err(|err| anyhow::anyhow!("dynamic workflow execution with replanning failed: {err}"))?;
    (outcome.plan, outcome.state, outcome.revisions)
  } else {
    let plan = agent
      .plan(&goal)
      .await
      .map_err(|err| anyhow::anyhow!("dynamic workflow planning failed: {err}"))?;
    // Show the authored plan up front so the executor's per-node progress
    // lines follow it in reading order.
    if !as_json {
      print!("{}", render_plan_text(&plan));
      println!("\nExecuting...");
    }
    // Compile the plan to a Flow and execute it via the core engine,
    // sharing the same governed registry the planner saw.
    let flow = compile_plan_to_flow(&plan, Arc::clone(&registry))
      .context("compiling the dynamic plan into a Flow failed")?;
    let state = flow
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::concurrent(max_concurrency),
      )
      .await
      .context("executing the compiled dynamic workflow failed")?;
    let state = plan_step_results(&plan, state);
    (plan, state, 0usize)
  };

  if as_json {
    println!(
      "{}",
      serde_json::to_string_pretty(&json!({
        "plan": plan_to_json(&plan),
        "revisions": revisions,
        "results": results_to_json(&state),
      }))?
    );
  } else {
    if replan > 0 {
      // The plan shown here is the final one that actually ran (merged
      // across every revision) — printed after execution since it wasn't
      // known up front.
      print!("{}", render_plan_text(&plan));
      println!("Revisions: {revisions}\n");
    }
    println!("Results:");
    print!("{}", render_results_text(&state));
  }

  // Surface a non-zero exit if any node failed, so shell consumers can branch.
  // A `run_if` skip is the plan's own decision, not a failure.
  if state
    .values()
    .any(|r| matches!(r, Err(err) if !matches!(err, yanshi_core::YanshiError::NodeSkipped)))
  {
    anyhow::bail!("one or more workflow steps failed (see results above)");
  }
  Ok(())
}

/// Build the built-in tool sandbox from the operator-granted allow-lists, layered
/// on top of the restrictive [`SandboxPolicy::default`] baseline.
fn build_policy(allow_path: &[String], allow_domain: &[String]) -> SandboxPolicy {
  let mut policy = SandboxPolicy::default();
  if !allow_path.is_empty() {
    policy.allowed_paths = allow_path.iter().map(std::path::PathBuf::from).collect();
  }
  if !allow_domain.is_empty() {
    policy.allowed_domains = allow_domain.to_vec();
  }
  policy
}

/// T1.3: forces every tool call through the approval provider whenever
/// one is configured. See the wiring comment at the `wrap_registry` call
/// site for why `HookConfig`'s own profile-based escalation isn't enough
/// on its own.
struct RequireApprovalForEveryCall;

#[async_trait]
impl PreToolHook for RequireApprovalForEveryCall {
  fn name(&self) -> &str {
    "workflow_dynamic_require_approval"
  }

  async fn before_tool(&self, call: &PendingToolCall) -> Result<PreToolDecision, HarnessError> {
    Ok(PreToolDecision::RequireApproval {
      risk: ApprovalRisk::High,
      reason: format!(
        "workflow dynamic: LLM-authored plan requires explicit approval before executing '{}'",
        call.tool
      ),
    })
  }
}

/// Map the `--approve` flag to an approval provider, or `None` for "no wrapping"
/// (tools still carry their sandbox policy; only the approval gate is skipped).
fn approve_provider(value: &str) -> Result<Option<Arc<dyn ApprovalProvider>>> {
  match value {
    "none" => Ok(None),
    "cli" => Ok(Some(Arc::new(CliApprovalProvider::stdin()))),
    "auto-allow" => Ok(Some(Arc::new(AutoAllowApprovalProvider::new()))),
    "auto-deny" => Ok(Some(Arc::new(
      AutoDenyApprovalProvider::new().with_stop_on_deny(true),
    ))),
    other => {
      anyhow::bail!("unsupported --approve '{other}', expected none | cli | auto-allow | auto-deny")
    }
  }
}

/// One step's outcome, ready to print: `Ok(content)` or `Err(message)`.
fn node_outcome(res: &AsyncNodeResult) -> Result<String, String> {
  match res {
    Ok(map) => match map.get("result") {
      Some(FlowValue::Json(Value::String(s))) => Ok(s.clone()),
      Some(FlowValue::Json(v)) => Ok(v.to_string()),
      Some(other) => Ok(format!("{other:?}")),
      None => Ok(String::new()),
    },
    Err(err) => Err(err.to_string()),
  }
}

/// Human-readable plan listing: one line per step with its tool and dependencies.
fn render_plan_text(plan: &WorkflowPlan) -> String {
  let noun = if plan.steps.len() == 1 {
    "step"
  } else {
    "steps"
  };
  let mut out = format!("Plan ({} {noun}):\n", plan.steps.len());
  for step in &plan.steps {
    if step.depends_on.is_empty() {
      out.push_str(&format!("  {} [{}]\n", step.id, step.tool));
    } else {
      out.push_str(&format!(
        "  {} [{}] <- {}\n",
        step.id,
        step.tool,
        step.depends_on.join(", ")
      ));
    }
  }
  out
}

/// JSON projection of the plan (its `Deserialize`-only types rendered by hand).
fn plan_to_json(plan: &WorkflowPlan) -> Value {
  Value::Array(
    plan
      .steps
      .iter()
      .map(|step| {
        json!({
          "id": step.id,
          "tool": step.tool,
          "params": step.params,
          "depends_on": step.depends_on,
        })
      })
      .collect(),
  )
}

/// Human-readable per-node results, sorted by node id for deterministic output.
fn render_results_text(state: &HashMap<String, AsyncNodeResult>) -> String {
  let mut ids: Vec<&String> = state.keys().collect();
  ids.sort();
  let mut out = String::new();
  for id in ids {
    match node_outcome(&state[id]) {
      Ok(content) => out.push_str(&format!("  {id} => {content}\n")),
      Err(err) => out.push_str(&format!("  {id} !! {err}\n")),
    }
  }
  out
}

/// JSON projection of the executed state pool (node id → result or error).
fn results_to_json(state: &HashMap<String, AsyncNodeResult>) -> Value {
  let mut map = serde_json::Map::new();
  for (id, res) in state {
    let entry = match node_outcome(res) {
      Ok(content) => json!({ "ok": content }),
      Err(err) => json!({ "error": err }),
    };
    map.insert(id.clone(), entry);
  }
  Value::Object(map)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use yanshi_core::YanshiError;

  #[test]
  fn build_policy_defaults_to_restrictive() {
    let policy = build_policy(&[], &[]);
    assert!(!policy.allow_all_paths, "paths must not be open by default");
    assert!(policy.allowed_paths.is_empty());
    assert!(policy.allowed_domains.is_empty());
  }

  #[test]
  fn build_policy_widens_only_what_is_granted() {
    let policy = build_policy(&["/tmp/wf".to_string()], &["api.example.com".to_string()]);
    assert_eq!(
      policy.allowed_paths,
      vec![std::path::PathBuf::from("/tmp/wf")]
    );
    assert_eq!(policy.allowed_domains, vec!["api.example.com".to_string()]);
    // Still not blanket-open — only the grant widened it.
    assert!(!policy.allow_all_paths);
  }

  #[test]
  fn approve_provider_parses_modes() {
    assert!(approve_provider("none").unwrap().is_none());
    assert!(approve_provider("cli").unwrap().is_some());
    assert!(approve_provider("auto-allow").unwrap().is_some());
    assert!(approve_provider("auto-deny").unwrap().is_some());
    assert!(approve_provider("bogus").is_err());
  }

  // `resolve_approve_default`'s own tests moved to `commands::harness`
  // (U2.3) alongside the function itself, which now backs `harness
  // run`/`chat` too, not just this command.

  fn ok_result(content: &str) -> AsyncNodeResult {
    let mut map = HashMap::new();
    map.insert(
      "result".to_string(),
      FlowValue::Json(Value::String(content.to_string())),
    );
    Ok(map)
  }

  #[test]
  fn node_outcome_extracts_result_string() {
    assert_eq!(node_outcome(&ok_result("hello")), Ok("hello".to_string()));
  }

  #[test]
  fn node_outcome_reports_errors() {
    let failed: AsyncNodeResult = Err(YanshiError::NodeExecutionFailed {
      message: "boom".to_string(),
    });
    assert!(node_outcome(&failed).is_err());
  }

  #[test]
  fn render_plan_text_lists_dependencies() {
    let plan: WorkflowPlan = serde_json::from_value(json!({
      "steps": [
        {"id": "a", "tool": "http", "params": {}},
        {"id": "b", "tool": "file", "params": {}, "depends_on": ["a"]}
      ]
    }))
    .unwrap();
    let text = render_plan_text(&plan);
    assert!(text.contains("Plan (2 steps)"));
    assert!(text.contains("a [http]"));
    assert!(text.contains("b [file] <- a"));
  }

  #[test]
  fn results_render_sorted_and_jsonable() {
    let mut state: HashMap<String, AsyncNodeResult> = HashMap::new();
    state.insert("z".to_string(), ok_result("last"));
    state.insert("a".to_string(), ok_result("first"));
    let text = render_results_text(&state);
    // 'a' sorts before 'z' regardless of insertion order.
    assert!(text.find("a =>").unwrap() < text.find("z =>").unwrap());
    let value = results_to_json(&state);
    assert_eq!(value["a"]["ok"], json!("first"));
    assert_eq!(value["z"]["ok"], json!("last"));
  }
}
