//! Y2.3 acceptance: DAG cost accounting for modality nodes.
//!
//! Three chained `text_to_image` calls under `vendor: mock`, priced at
//! $0.04 per image, with the workflow's `cost_limit_usd: 0.05`: the first
//! two run ($0.08 spent, crossing the limit on the second), the third is
//! refused before it calls the vendor, and the events carry each node's
//! cost plus exactly one `CostLimitExceeded`. A control run without the
//! limit runs all three, so the limit is what stopped the first run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use yanshi_config::executor::{build_flow_from_definition, parse_workflow_definition};
use yanshi_core::{
  EventListener, FlowExecutionConfig, FlowExt, WorkflowEvent, error::YanshiError, value::FlowValue,
};

const MOCK_REGISTRY: &str = r#"
models:
  mock-t2i:
    vendor: mock
    type: text_to_image
    model_id: mock-t2i
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#;

const PRICING: &str = r#"
models:
  mock-t2i:
    per_image: 0.04
"#;

/// Registry and pricing table are process-wide (the nodes read
/// `YANSHI_MODELS_CONFIG`; `PricingTable::shared` reads
/// `YANSHI_PRICING_TABLE` once), so both are installed once.
async fn install_env() {
  static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-cost-limit-{}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("dir");
      let models = dir.join("models.yml");
      std::fs::write(&models, MOCK_REGISTRY).expect("registry");
      let pricing = dir.join("pricing.yml");
      std::fs::write(&pricing, PRICING).expect("pricing");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe {
        std::env::set_var("YANSHI_MODELS_CONFIG", &models);
        std::env::set_var("YANSHI_PRICING_TABLE", &pricing);
      }
      yanshi_llm::Yanshi::init_with_config(models.to_str().expect("utf-8"))
        .await
        .expect("mock registry loads");
    })
    .await;
}

fn workflow(limit: Option<f64>) -> String {
  let limit = limit
    .map(|l| format!("cost_limit_usd: {l}\n"))
    .unwrap_or_default();
  format!(
    r#"
name: "three images"
{limit}nodes:
  - id: draw1
    type: text_to_image
    parameters: {{ model: mock-t2i, prompt: "one" }}
  - id: draw2
    type: text_to_image
    dependencies: ["draw1"]
    parameters: {{ model: mock-t2i, prompt: "two" }}
  - id: draw3
    type: text_to_image
    dependencies: ["draw2"]
    parameters: {{ model: mock-t2i, prompt: "three" }}
"#
  )
}

#[derive(Default)]
struct Captured(Mutex<Vec<WorkflowEvent>>);

impl EventListener for Captured {
  fn on_event(&self, event: &WorkflowEvent) {
    self.0.lock().expect("lock").push(event.clone());
  }
}

async fn run(
  limit: Option<f64>,
) -> (
  HashMap<String, yanshi_core::async_node::AsyncNodeResult>,
  Vec<WorkflowEvent>,
  tempfile::TempDir,
) {
  install_env().await;
  let runs = tempfile::TempDir::new().expect("tempdir");
  let definition = parse_workflow_definition(&workflow(limit)).expect("parses");
  let captured = Arc::new(Captured::default());
  let flow = build_flow_from_definition(&definition, None)
    .expect("builds")
    .with_event_listener(captured.clone());
  let mut config = FlowExecutionConfig::serial().with_run_base_dir(runs.path());
  if let Some(limit) = definition.effective_cost_limit(None).expect("valid limit") {
    config = config.with_cost_limit_usd(limit);
  }
  let state = flow
    .execute_from_inputs_with_config(HashMap::new(), config)
    .await
    .expect("run returns its state");
  let events = captured.0.lock().expect("lock").clone();
  (state, events, runs)
}

fn costs(events: &[WorkflowEvent]) -> Vec<(String, f64, f64)> {
  events
    .iter()
    .filter_map(|event| match event {
      WorkflowEvent::NodeCostRecorded {
        node_id,
        cost_usd,
        total_cost_usd,
        ..
      } => Some((node_id.clone(), *cost_usd, *total_cost_usd)),
      _ => None,
    })
    .collect()
}

fn close(a: f64, b: f64) -> bool {
  (a - b).abs() < 1e-9
}

#[tokio::test]
async fn the_third_image_is_refused_once_spend_passes_the_limit() {
  let (state, events, runs) = run(Some(0.05)).await;

  for id in ["draw1", "draw2"] {
    let outputs = state[id].as_ref().expect("first two run");
    assert!(matches!(
      outputs[&format!("{id}_image")],
      FlowValue::File { .. }
    ));
  }
  match &state["draw3"] {
    Err(YanshiError::CostLimitExceeded {
      spent_usd,
      limit_usd,
    }) => {
      assert!(close(*spent_usd, 0.08), "spent {spent_usd}");
      assert!(close(*limit_usd, 0.05), "limit {limit_usd}");
    }
    other => panic!("draw3 must be refused for cost, got {other:?}"),
  }
  // Refused before the vendor call: no third artifact anywhere.
  let third = walk(runs.path())
    .into_iter()
    .filter(|p| p.components().any(|c| c.as_os_str() == "draw3"))
    .count();
  assert_eq!(third, 0, "draw3 must not have produced an artifact");

  let recorded = costs(&events);
  assert_eq!(recorded.len(), 2, "{recorded:?}");
  assert_eq!(recorded[0].0, "draw1");
  assert!(close(recorded[0].1, 0.04) && close(recorded[0].2, 0.04));
  assert_eq!(recorded[1].0, "draw2");
  assert!(close(recorded[1].1, 0.04) && close(recorded[1].2, 0.08));

  let exceeded: Vec<&WorkflowEvent> = events
    .iter()
    .filter(|e| matches!(e, WorkflowEvent::CostLimitExceeded { .. }))
    .collect();
  assert_eq!(exceeded.len(), 1);
  let WorkflowEvent::CostLimitExceeded { node_id, .. } = exceeded[0] else {
    unreachable!("filtered to CostLimitExceeded above");
  };
  assert_eq!(node_id, "draw3");
}

#[tokio::test]
async fn without_a_limit_all_three_images_run_and_are_billed() {
  let (state, events, _runs) = run(None).await;
  for id in ["draw1", "draw2", "draw3"] {
    assert!(state[id].is_ok(), "{id}: {:?}", state[id]);
  }
  let recorded = costs(&events);
  assert_eq!(recorded.len(), 3);
  assert!(close(recorded[2].2, 0.12), "{recorded:?}");
  assert!(
    !events
      .iter()
      .any(|e| matches!(e, WorkflowEvent::CostLimitExceeded { .. }))
  );
}

#[test]
fn invalid_limits_are_rejected() {
  for bad in ["0", "-1", ".nan"] {
    let yaml = format!(
      "name: x\ncost_limit_usd: {bad}\nnodes:\n  - id: a\n    type: template\n    parameters: {{ template: hi }}\n"
    );
    let err = parse_workflow_definition(&yaml).expect_err("schema rejects it");
    assert!(err.to_string().contains("cost_limit_usd"), "{bad}: {err}");
  }
  let yaml = "name: x\ncost_limit_usd: 2.0\nnodes:\n  - id: a\n    type: template\n    parameters: { template: hi }\n";
  let definition = parse_workflow_definition(yaml).expect("parses");
  assert_eq!(definition.effective_cost_limit(Some(0.5)), Ok(Some(0.5)));
  assert!(definition.effective_cost_limit(Some(-1.0)).is_err());
  assert_eq!(definition.effective_cost_limit(None), Ok(Some(2.0)));
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
  let mut found = Vec::new();
  if let Ok(entries) = std::fs::read_dir(dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path.is_dir() {
        found.extend(walk(&path));
      }
      found.push(path);
    }
  }
  found
}
