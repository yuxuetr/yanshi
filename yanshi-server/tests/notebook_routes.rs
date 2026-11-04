//! Y5.2 acceptance: notebook sources are ingested in the background,
//! searchable, and still searchable after a restart (a fresh `AppState`
//! on the same database: the index is rebuilt from `source_chunks`).
//!
//! Skipped without `YANSHI_DATABASE_TEST_URL`.

use std::time::Duration;

use axum::{
  body::Body,
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::Database;
use yanshi_server::{AppState, create_router, tenant::TENANT_HEADER};

async fn state() -> Option<AppState> {
  let url = std::env::var("YANSHI_DATABASE_TEST_URL").ok()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  Some(AppState::new(db))
}

async fn call(
  state: &AppState,
  method: &str,
  uri: &str,
  tenant: &str,
  body: Option<Value>,
) -> (StatusCode, Value) {
  let request = Request::builder()
    .method(method)
    .uri(uri)
    .header(TENANT_HEADER, tenant)
    .header(CONTENT_TYPE, "application/json")
    .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
    .expect("request");
  let response = create_router(state.clone())
    .oneshot(request)
    .await
    .expect("response");
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .expect("body");
  (
    status,
    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
  )
}

/// Poll until every source of the notebook has left pending / ingesting.
async fn settled_sources(state: &AppState, tenant: &str, notebook: &str) -> Vec<Value> {
  for _ in 0..200 {
    let (_, body) = call(
      state,
      "GET",
      &format!("/v1/notebooks/{notebook}"),
      tenant,
      None,
    )
    .await;
    let sources = body["sources"].as_array().cloned().unwrap_or_default();
    if !sources.is_empty()
      && sources
        .iter()
        .all(|s| matches!(s["status"].as_str(), Some("ready" | "failed")))
    {
      return sources;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  panic!("sources did not settle");
}

#[tokio::test]
async fn sources_are_ingested_searchable_and_survive_a_restart() {
  let Some(state) = state().await else {
    eprintln!("skipping notebook routes — set YANSHI_DATABASE_TEST_URL");
    return;
  };
  let tenant = format!("tenant-nb-routes-{}", Uuid::new_v4());
  let other = format!("tenant-nb-other-{}", Uuid::new_v4());

  let (code, notebook) = call(
    &state,
    "POST",
    "/v1/notebooks",
    &tenant,
    Some(json!({"title": "Tides"})),
  )
  .await;
  assert_eq!(code, StatusCode::OK, "{notebook}");
  let id = notebook["id"].as_str().expect("id").to_string();

  // Text, an HTML file, and a URL the SSRF filter must refuse.
  let text = "The tides are caused by the gravitational pull of the moon.\n\nSpring tides happen when the sun and moon align.";
  let (code, _) = call(
    &state,
    "POST",
    &format!("/v1/notebooks/{id}/sources"),
    &tenant,
    Some(json!({"kind": "text", "text": text})),
  )
  .await;
  assert_eq!(code, StatusCode::OK);
  use base64::Engine as _;
  let html = "<html><body><h1>Neap tides</h1><p>Neap tides occur at quarter moon.</p><script>noise()</script></body></html>";
  let (code, _) = call(
    &state,
    "POST",
    &format!("/v1/notebooks/{id}/sources"),
    &tenant,
    Some(json!({
      "kind": "file", "filename": "neap.html",
      "content_base64": base64::engine::general_purpose::STANDARD.encode(html)
    })),
  )
  .await;
  assert_eq!(code, StatusCode::OK);
  let (code, _) = call(
    &state,
    "POST",
    &format!("/v1/notebooks/{id}/sources"),
    &tenant,
    Some(json!({"kind": "url", "url": "http://127.0.0.1:9/private"})),
  )
  .await;
  assert_eq!(code, StatusCode::OK, "accepted, then refused at fetch");
  let (code, _) = call(
    &state,
    "POST",
    &format!("/v1/notebooks/{id}/sources"),
    &tenant,
    Some(json!({"kind": "recording"})),
  )
  .await;
  assert_eq!(code, StatusCode::BAD_REQUEST);

  let sources = settled_sources(&state, &tenant, &id).await;
  assert_eq!(sources.len(), 3);
  let by_kind = |kind: &str| {
    sources
      .iter()
      .find(|s| s["kind"] == kind)
      .expect(kind)
      .clone()
  };
  assert_eq!(by_kind("text")["status"], "ready");
  assert_eq!(by_kind("text")["sha256"].as_str().map(str::len), Some(64));
  assert_eq!(by_kind("file")["status"], "ready");
  let url = by_kind("url");
  assert_eq!(url["status"], "failed", "loopback must be refused: {url}");
  assert!(url["error"].as_str().is_some_and(|e| !e.is_empty()));

  // Search finds the right chunk, with its source and chunk id.
  let search = |state: AppState, q: &'static str| {
    let tenant = tenant.clone();
    let id = id.clone();
    async move {
      call(
        &state,
        "GET",
        &format!("/v1/notebooks/{id}/search?q={q}"),
        &tenant,
        None,
      )
      .await
    }
  };
  let (code, hits) = search(state.clone(), "moon").await;
  assert_eq!(code, StatusCode::OK, "{hits}");
  let first = &hits["chunks"][0];
  assert!(
    first["content"]
      .as_str()
      .is_some_and(|c| c.contains("moon")),
    "{hits}"
  );
  assert_eq!(first["source_id"], by_kind("text")["id"]);
  assert!(
    first["chunk_id"]
      .as_str()
      .is_some_and(|c| c.starts_with(by_kind("text")["id"].as_str().unwrap_or("?")))
  );
  let (_, hits) = search(state.clone(), "quarter").await;
  assert_eq!(
    hits["chunks"][0]["source_id"],
    by_kind("file")["id"],
    "HTML text is searchable: {hits}"
  );
  assert!(!hits.to_string().contains("noise()"), "scripts are dropped");

  // "Restart": a new AppState (empty index cache) on the same database.
  let restarted = AppState::new(state.db.clone());
  let (code, hits) = search(restarted, "moon").await;
  assert_eq!(code, StatusCode::OK);
  assert!(
    hits["chunks"].as_array().is_some_and(|c| !c.is_empty()),
    "the index is rebuilt from source_chunks: {hits}"
  );

  // Another tenant sees nothing.
  for uri in [
    format!("/v1/notebooks/{id}"),
    format!("/v1/notebooks/{id}/search?q=moon"),
  ] {
    let (code, _) = call(&state, "GET", &uri, &other, None).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{uri}");
  }
  let (code, _) = call(
    &state,
    "POST",
    &format!("/v1/notebooks/{id}/sources"),
    &other,
    Some(json!({"kind": "text", "text": "x"})),
  )
  .await;
  assert_eq!(code, StatusCode::NOT_FOUND);
}
