//! Notebooks and their sources (Y5.2, `docs/RFC_CONTENT_STUDIO.md` §6).
//!
//! ```text
//! POST /v1/notebooks                      {title}              → notebook
//! GET  /v1/notebooks                                           → {notebooks}
//! GET  /v1/notebooks/{id}                                      → {notebook, sources}
//! POST /v1/notebooks/{id}/sources         {kind, ...}          → source (pending; ingests in the background)
//! GET  /v1/notebooks/{id}/search?q=&top_k=                     → {chunks}
//! ```
//!
//! Ingestion: fetch (`url`, through the http tool's SSRF filter: the URL
//! is tenant-supplied), or take the pasted `text`, or decode an uploaded
//! text-like `file`; extract text (HTML via the RAG loader); chunk by
//! paragraph; store the chunks in `source_chunks`; mark the source
//! `ready` (or `failed`, with the reason). `recording` sources are
//! rejected until transcription is wired in.
//!
//! The index is derived, not stored: a notebook's BM25 index is built
//! from its ready chunks on first search, cached in this process, and
//! dropped whenever one of its sources finishes ingesting. A restart
//! therefore costs one rebuild per notebook, nothing more.

use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

use axum::{
  Extension, Json,
  extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use yanshi_db::{
  NewSource, Notebook, NotebookRepo, Source, SourceChunk, SourceChunkRepo, SourceKind, SourceRepo,
  SourceStatus,
};
use yanshi_rag::{Bm25KnowledgeBackend, KnowledgeBackend};

use crate::{AppState, error::ApiError, tenant::TenantId};

/// Uploaded / pasted sources larger than this are refused at submit.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
/// Paragraph chunking: target size and overlap, in characters.
const CHUNK_SIZE: usize = 1200;
const CHUNK_OVERLAP: usize = 150;

/// Per-notebook BM25 indexes, built on first search.
#[derive(Clone, Default)]
pub struct NotebookIndexCache(Arc<Mutex<HashMap<Uuid, Arc<NotebookIndex>>>>);

/// A notebook's index and the chunks it was built from (search results
/// carry the source id, which BM25's metadata does not round-trip).
pub struct NotebookIndex {
  backend: Bm25KnowledgeBackend,
  chunks: HashMap<String, SourceChunk>,
}

impl NotebookIndexCache {
  fn get(&self, notebook_id: Uuid) -> Option<Arc<NotebookIndex>> {
    self
      .0
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .get(&notebook_id)
      .cloned()
  }

  fn put(&self, notebook_id: Uuid, index: Arc<NotebookIndex>) {
    self
      .0
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .insert(notebook_id, index);
  }

  /// Drop a notebook's index (a source changed).
  pub fn invalidate(&self, notebook_id: Uuid) {
    self
      .0
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .remove(&notebook_id);
  }
}

impl std::fmt::Debug for NotebookIndexCache {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("NotebookIndexCache")
  }
}

#[derive(Debug, Deserialize)]
pub struct CreateNotebookRequest {
  pub title: String,
}

pub async fn create_notebook(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Json(req): Json<CreateNotebookRequest>,
) -> Result<Json<Notebook>, ApiError> {
  let title = req.title.trim();
  if title.is_empty() {
    return Err(ApiError::BadRequest("title must not be empty".into()));
  }
  Ok(Json(
    state.repos.notebooks.create(tenant.as_str(), title).await?,
  ))
}

pub async fn list_notebooks(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
) -> Result<Json<Value>, ApiError> {
  let notebooks = state.repos.notebooks.list(tenant.as_str(), 100).await?;
  Ok(Json(json!({ "notebooks": notebooks })))
}

async fn own_notebook(state: &AppState, tenant: &TenantId, id: Uuid) -> Result<Notebook, ApiError> {
  state
    .repos
    .notebooks
    .get(tenant.as_str(), id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("notebook {id} not found")))
}

pub async fn get_notebook(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
  let notebook = own_notebook(&state, &tenant, id).await?;
  let sources = state
    .repos
    .sources
    .list_for_notebook(tenant.as_str(), id)
    .await?;
  Ok(Json(json!({ "notebook": notebook, "sources": sources })))
}

/// `POST /v1/notebooks/{id}/sources` body.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AddSourceRequest {
  Url {
    url: String,
    #[serde(default)]
    title: Option<String>,
  },
  Text {
    text: String,
    #[serde(default)]
    title: Option<String>,
  },
  /// A text-like file (`.txt`, `.md`, `.html`), base64-encoded.
  File {
    filename: String,
    content_base64: String,
  },
  Recording {},
}

/// What ingestion starts from, resolved at submit time.
enum Payload {
  Url(String),
  Bytes { bytes: Vec<u8>, html: bool },
}

pub async fn add_source(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
  Json(req): Json<AddSourceRequest>,
) -> Result<Json<Source>, ApiError> {
  own_notebook(&state, &tenant, id).await?;
  let (kind, title, uri, payload) = match req {
    AddSourceRequest::Url { url, title } => {
      // The http tool parses and SSRF-checks the URL at fetch time;
      // here only the scheme is checked so a typo fails at submit.
      let lowered = url.trim().to_ascii_lowercase();
      if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
        return Err(ApiError::BadRequest(
          "url must start with http:// or https://".into(),
        ));
      }
      (
        SourceKind::Url,
        title.unwrap_or_else(|| url.clone()),
        Some(url.clone()),
        Payload::Url(url),
      )
    }
    AddSourceRequest::Text { text, title } => {
      if text.trim().is_empty() {
        return Err(ApiError::BadRequest("text must not be empty".into()));
      }
      if text.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::PayloadTooLarge(format!(
          "text is larger than {MAX_SOURCE_BYTES} bytes"
        )));
      }
      (
        SourceKind::Text,
        title.unwrap_or_else(|| text.chars().take(60).collect()),
        None,
        Payload::Bytes {
          bytes: text.into_bytes(),
          html: false,
        },
      )
    }
    AddSourceRequest::File {
      filename,
      content_base64,
    } => {
      use base64::Engine as _;
      let extension = FsPath::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
      let html = match extension.as_str() {
        "html" | "htm" => true,
        "txt" | "md" | "markdown" => false,
        other => {
          return Err(ApiError::BadRequest(format!(
            "unsupported file type '.{other}': upload .txt, .md or .html"
          )));
        }
      };
      let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_base64.as_bytes())
        .map_err(|e| ApiError::BadRequest(format!("content_base64: {e}")))?;
      if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::PayloadTooLarge(format!(
          "file is larger than {MAX_SOURCE_BYTES} bytes"
        )));
      }
      (
        SourceKind::File,
        filename.clone(),
        Some(filename),
        Payload::Bytes { bytes, html },
      )
    }
    AddSourceRequest::Recording {} => {
      return Err(ApiError::BadRequest(
        "recording sources are not supported yet; transcribe first and add the text".into(),
      ));
    }
  };
  let source = state
    .repos
    .sources
    .create(NewSource {
      notebook_id: id,
      tenant_id: tenant.as_str().to_string(),
      kind,
      title,
      uri,
    })
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("notebook {id} not found")))?;
  tokio::spawn(ingest(state.clone(), source.clone(), payload));
  Ok(Json(source))
}

/// Fetch / decode → text → chunks → `source_chunks`, then mark the
/// source ready (or failed with the reason) and drop the notebook's
/// cached index.
async fn ingest(state: AppState, source: Source, payload: Payload) {
  let sources = &state.repos.sources;
  let _ = sources
    .update_status(source.id, SourceStatus::Ingesting, None, None)
    .await;
  let outcome = async {
    let (bytes, html) = match payload {
      Payload::Url(url) => fetch(&url).await?,
      Payload::Bytes { bytes, html } => (bytes, html),
    };
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let text = if html {
      html_text(&bytes).await?
    } else {
      String::from_utf8(bytes).map_err(|_| "the source is not UTF-8 text".to_string())?
    };
    let chunks = chunk(&text)?;
    if chunks.is_empty() {
      return Err("the source has no text".to_string());
    }
    state
      .repos
      .chunks
      .replace_for_source(&source, chunks)
      .await
      .map_err(|e| format!("storing chunks failed: {e}"))?;
    Ok::<_, String>(digest)
  }
  .await;
  let _ = match outcome {
    Ok(digest) => {
      sources
        .update_status(source.id, SourceStatus::Ready, None, Some(&digest))
        .await
    }
    Err(reason) => {
      tracing::warn!(source = %source.id, %reason, "source ingestion failed");
      sources
        .update_status(source.id, SourceStatus::Failed, Some(&reason), None)
        .await
    }
  };
  state.notebook_indexes.invalidate(source.notebook_id);
}

/// GET `url` through the http tool's default policy, which refuses
/// loopback, private and cloud-metadata addresses.
async fn fetch(url: &str) -> Result<(Vec<u8>, bool), String> {
  let tool = yanshi_tools::builtin::HttpTool::new(Arc::new(yanshi_tools::SandboxPolicy::default()))
    .map_err(|e| format!("http client: {e}"))?;
  let response = tool
    .send(&json!({ "url": url, "method": "GET" }))
    .await
    .map_err(|e| format!("fetching {url} failed: {e}"))?;
  if !response.status.is_success() {
    return Err(format!("fetching {url} returned HTTP {}", response.status));
  }
  let body = response.body;
  let lowered = body
    .trim_start()
    .get(..512)
    .unwrap_or(body.trim_start())
    .to_ascii_lowercase();
  let html = lowered.starts_with("<!doctype html") || lowered.contains("<html");
  Ok((body.into_bytes(), html))
}

/// Visible text of an HTML document (scripts and styles dropped).
async fn html_text(bytes: &[u8]) -> Result<String, String> {
  use yanshi_rag::sources::{DocumentLoader, html::HtmlLoader};
  let path = std::env::temp_dir().join(format!("yanshi-ingest-{}.html", Uuid::new_v4()));
  tokio::fs::write(&path, bytes)
    .await
    .map_err(|e| format!("staging the page: {e}"))?;
  let loaded = HtmlLoader::new().load(&path).await;
  let _ = tokio::fs::remove_file(&path).await;
  loaded
    .map(|document| document.content)
    .map_err(|e| format!("extracting text: {e}"))
}

fn chunk(text: &str) -> Result<Vec<(String, Value)>, String> {
  use yanshi_rag::chunking::{ChunkingStrategy, ParagraphChunker};
  let chunks = ParagraphChunker::new(CHUNK_SIZE, CHUNK_OVERLAP)
    .chunk(text)
    .map_err(|e| format!("chunking failed: {e}"))?;
  Ok(
    chunks
      .into_iter()
      .filter(|c| !c.content.trim().is_empty())
      // ParagraphChunker's indices are source line numbers.
      .map(|c| {
        (
          c.content,
          json!({ "start_line": c.start_idx, "end_line": c.end_idx }),
        )
      })
      .collect(),
  )
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
  pub q: String,
  #[serde(default)]
  pub top_k: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
  pub source_id: Uuid,
  pub chunk_id: String,
  pub content: String,
  pub score: f32,
}

pub async fn search_notebook(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
  Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
  own_notebook(&state, &tenant, id).await?;
  if query.q.trim().is_empty() {
    return Err(ApiError::BadRequest("q must not be empty".into()));
  }
  let index = match state.notebook_indexes.get(id) {
    Some(index) => index,
    None => {
      let chunks = state
        .repos
        .chunks
        .list_ready_for_notebook(tenant.as_str(), id)
        .await?;
      let backend = Bm25KnowledgeBackend::from_documents(
        chunks
          .iter()
          .map(|c| (c.chunk_id.clone(), c.content.clone())),
      )
      .with_name(format!("notebook:{id}"));
      let index = Arc::new(NotebookIndex {
        backend,
        chunks: chunks
          .into_iter()
          .map(|c| (c.chunk_id.clone(), c))
          .collect(),
      });
      state.notebook_indexes.put(id, index.clone());
      index
    }
  };
  if index.chunks.is_empty() {
    return Ok(Json(json!({ "chunks": [] })));
  }
  let top_k = query.top_k.unwrap_or(5).clamp(1, 50);
  let hits: Vec<SearchHit> = index
    .backend
    .search(&query.q, top_k)
    .await
    .map_err(|e| ApiError::Internal(format!("search failed: {e}")))?
    .into_iter()
    .filter_map(|hit| {
      let chunk = index.chunks.get(&hit.id)?;
      Some(SearchHit {
        source_id: chunk.source_id,
        chunk_id: chunk.chunk_id.clone(),
        content: chunk.content.clone(),
        score: hit.score,
      })
    })
    .collect();
  Ok(Json(json!({ "chunks": hits })))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn chunks_carry_their_line_range() {
    let text = format!("{}\n\n{}", "a".repeat(800), "b".repeat(800));
    let chunks = chunk(&text).expect("chunks");
    assert!(
      chunks.len() >= 2,
      "two paragraphs over the size limit split"
    );
    assert!(chunks.iter().all(|(content, meta)| {
      !content.is_empty() && meta["end_line"].as_u64() >= meta["start_line"].as_u64()
    }));
    assert!(
      chunks[1].1["start_line"].as_u64() > chunks[0].1["start_line"].as_u64(),
      "later chunks start on later lines"
    );
  }

  #[tokio::test]
  async fn html_is_reduced_to_its_visible_text() {
    let text = html_text(b"<html><head><script>var x=1;</script></head><body><h1>Tides</h1><p>The moon pulls.</p></body></html>")
      .await
      .expect("text");
    assert!(
      text.contains("Tides") && text.contains("The moon pulls."),
      "{text}"
    );
    assert!(!text.contains("var x"), "{text}");
  }
}
