//! Y5.3: generators fed notebook `chunks` cite those chunks. Every
//! report section carries at least one citation whose `chunk_id` is one
//! of the given passages, with that passage's `source_id`; the mind map
//! cites the passages its branches summarise.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use yanshi_config::content::Deliverable;
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, parse_workflow_definition,
};
use yanshi_core::{FlowExecutionConfig, FlowExt, value::FlowValue};

fn chunks() -> Value {
  json!([
    {"chunk_id": "src-a:0", "source_id": "src-a", "content": "Tides are caused by the gravitational pull of the moon."},
    {"chunk_id": "src-a:1", "source_id": "src-a", "content": "Spring tides happen when the sun and the moon align."},
    {"chunk_id": "src-b:0", "source_id": "src-b", "content": "Neap tides occur at the quarter moon."}
  ])
}

async fn run(
  name: &str,
  extra: &[(&str, Value)],
) -> HashMap<String, yanshi_core::async_node::AsyncNodeResult> {
  let template = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../examples/generators")
      .join(name),
  )
  .expect("template");
  let definition = parse_workflow_definition(&template).expect("template validates");
  let mut inputs: HashMap<String, FlowValue> =
    HashMap::from([("chunks".to_string(), FlowValue::Json(chunks()))]);
  for (k, v) in extra {
    inputs.insert(k.to_string(), FlowValue::Json(v.clone()));
  }
  apply_declared_inputs(&definition, &mut inputs).expect("defaults");
  let runs = tempfile::TempDir::new().expect("tempdir").keep();
  build_flow_from_definition(&definition, Some("mock-model"))
    .expect("builds")
    .execute_from_inputs_with_config(
      inputs,
      FlowExecutionConfig::serial().with_run_base_dir(runs),
    )
    .await
    .expect("runs")
}

fn deliverable(state: &HashMap<String, yanshi_core::async_node::AsyncNodeResult>) -> Deliverable {
  let FlowValue::Json(value) = &state["deliverable"].as_ref().expect("deliverable")["deliverable"]
  else {
    panic!("deliverable must be JSON");
  };
  Deliverable::from_value(value.clone()).expect("matches the contract")
}

#[tokio::test]
async fn generators_fed_chunks_cite_existing_chunk_ids() {
  let dir = std::env::temp_dir().join(format!("yanshi-gen-cite-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("dir");
  let registry = dir.join("models.yml");
  std::fs::write(
    &registry,
    "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n",
  )
  .expect("registry");
  // Model calls in order: report outline, two sections (parallel, same
  // shape), then the mind map's tree.
  let section = |text: &str, id: &str, quote: &str| {
    json!({"markdown": text, "quotes": [{"chunk_id": id, "quote": quote}]}).to_string()
  };
  let replies = json!([
    json!({"title": "Tides", "sections": [{"heading": "Cause", "brief": "why"}, {"heading": "Kinds", "brief": "which"}]}).to_string(),
    section("The moon pulls the sea.", "src-a:0", "Tides are caused by the gravitational pull of the moon."),
    section("Spring and neap tides differ.", "src-b:0", "Neap tides occur at the quarter moon."),
    json!({"label": "Tides", "children": [
      {"label": "Cause: the moon", "chunk_id": "src-a:0"},
      {"label": "Kinds", "chunk_id": "src-a:1", "children": [{"label": "Neap", "chunk_id": "src-b:0"}]}
    ]}).to_string(),
  ]);
  // SAFETY: the only env writers in this binary, before any model call.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &registry);
    std::env::set_var("YANSHI_MOCK_RESPONSES", replies.to_string());
  }
  yanshi_llm::Yanshi::init_with_config(registry.to_str().expect("utf-8"))
    .await
    .expect("mock registry");
  let known: HashSet<String> = chunks()
    .as_array()
    .expect("array")
    .iter()
    .map(|c| c["chunk_id"].as_str().expect("id").to_string())
    .collect();

  // Report: every section cites, every cited chunk exists, with its source.
  let state = run(
    "report.yml",
    &[("sections", json!(2)), ("pdf", json!(false))],
  )
  .await;
  let FlowValue::Json(source) = &state["source"].as_ref().expect("source")["output"] else {
    panic!("source text");
  };
  assert!(
    source
      .as_str()
      .is_some_and(|s| s.contains("[src-b:0] Neap tides")),
    "{source}"
  );
  let report = deliverable(&state);
  assert_eq!(report.citations.len(), 2, "one citation per section here");
  for citation in &report.citations {
    assert!(known.contains(&citation.chunk_id), "{citation:?}");
    assert_eq!(
      citation.source_id,
      citation.chunk_id.split(':').next().unwrap_or_default(),
      "source id comes from the chunk"
    );
  }
  let sections = report.content["sections"].as_array().expect("sections");
  assert_eq!(sections.len(), 2);
  let FlowValue::File { path, .. } = &state["report_md"].as_ref().expect("md")["report_md_output"]
  else {
    panic!("md file");
  };
  let markdown = std::fs::read_to_string(path).expect("md");
  assert!(
    markdown.contains("[src-a:0]") && markdown.contains("[src-b:0]"),
    "footnotes name the passages: {markdown}"
  );

  // Mind map: branches cite the passages they summarise.
  let state = run("mindmap.yml", &[]).await;
  let map = deliverable(&state);
  let cited: Vec<&str> = map.citations.iter().map(|c| c.chunk_id.as_str()).collect();
  assert_eq!(
    cited,
    vec!["src-a:0", "src-a:1", "src-b:0"],
    "{:?}",
    map.citations
  );
  assert!(
    map
      .citations
      .iter()
      .all(|c| !c.quote.is_empty() && known.contains(&c.chunk_id))
  );
}
