//! Notebooks, their sources and their generated artifacts (Y5.1,
//! `docs/RFC_CONTENT_STUDIO.md` §6; migration `0013_notebooks.sql`).
//!
//! Every read takes the caller's tenant and filters on it, so an id from
//! another tenant is indistinguishable from a missing one.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::error::DbError;
use crate::models::Artifact;

#[derive(Debug, Clone, PartialEq, Eq, FromRow, Serialize, Deserialize)]
pub struct Notebook {
  pub id: Uuid,
  pub tenant_id: String,
  pub title: String,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

/// What kind of thing a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
  Url,
  File,
  Text,
  Recording,
}

impl SourceKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Url => "url",
      Self::File => "file",
      Self::Text => "text",
      Self::Recording => "recording",
    }
  }
}

/// Ingestion state of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
  Pending,
  Ingesting,
  Ready,
  Failed,
}

impl SourceStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Pending => "pending",
      Self::Ingesting => "ingesting",
      Self::Ready => "ready",
      Self::Failed => "failed",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow, Serialize, Deserialize)]
pub struct Source {
  pub id: Uuid,
  pub notebook_id: Uuid,
  pub tenant_id: String,
  /// `url` / `file` / `text` / `recording` (see [`SourceKind`]).
  pub kind: String,
  pub title: String,
  pub uri: Option<String>,
  /// `pending` / `ingesting` / `ready` / `failed` (see [`SourceStatus`]).
  pub status: String,
  pub error: Option<String>,
  pub sha256: Option<String>,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSource {
  pub notebook_id: Uuid,
  pub tenant_id: String,
  pub kind: SourceKind,
  pub title: String,
  pub uri: Option<String>,
}

/// An artifact attached to a notebook, with the deliverable kind that
/// produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookArtifact {
  pub kind: String,
  #[serde(flatten)]
  pub artifact: Artifact,
}

#[async_trait]
pub trait NotebookRepo: Send + Sync {
  async fn create(&self, tenant_id: &str, title: &str) -> Result<Notebook, DbError>;
  async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Notebook>, DbError>;
  /// Newest first.
  async fn list(&self, tenant_id: &str, limit: i64) -> Result<Vec<Notebook>, DbError>;
  /// Deletes the notebook with its sources and artifact links; `false`
  /// when no such notebook exists for this tenant.
  async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DbError>;
  /// Link a registered artifact to a notebook. Idempotent. `false` when
  /// the notebook or artifact is not this tenant's.
  async fn attach_artifact(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
    artifact_id: Uuid,
    kind: &str,
  ) -> Result<bool, DbError>;
  async fn list_artifacts(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
  ) -> Result<Vec<NotebookArtifact>, DbError>;
}

#[async_trait]
pub trait SourceRepo: Send + Sync {
  /// `None` when the notebook is not the source's tenant's.
  async fn create(&self, source: NewSource) -> Result<Option<Source>, DbError>;
  async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Source>, DbError>;
  /// Oldest first.
  async fn list_for_notebook(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
  ) -> Result<Vec<Source>, DbError>;
  async fn update_status(
    &self,
    id: Uuid,
    status: SourceStatus,
    error: Option<&str>,
    sha256: Option<&str>,
  ) -> Result<(), DbError>;
}

#[derive(Clone, Debug)]
pub struct PgNotebookRepo {
  pub(crate) pool: PgPool,
  pub(crate) read_pool: PgPool,
}

#[derive(Clone, Debug)]
pub struct PgSourceRepo {
  pub(crate) pool: PgPool,
  pub(crate) read_pool: PgPool,
}

const NOTEBOOK_COLUMNS: &str = "id, tenant_id, title, created_at, updated_at";
const SOURCE_COLUMNS: &str =
  "id, notebook_id, tenant_id, kind, title, uri, status, error, sha256, created_at, updated_at";

#[async_trait]
impl NotebookRepo for PgNotebookRepo {
  async fn create(&self, tenant_id: &str, title: &str) -> Result<Notebook, DbError> {
    let row = sqlx::query_as::<_, Notebook>(&format!(
      "INSERT INTO notebooks (id, tenant_id, title) VALUES ($1, $2, $3) RETURNING {NOTEBOOK_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(title)
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Notebook>, DbError> {
    Ok(
      sqlx::query_as::<_, Notebook>(&format!(
        "SELECT {NOTEBOOK_COLUMNS} FROM notebooks WHERE id = $1 AND tenant_id = $2"
      ))
      .bind(id)
      .bind(tenant_id)
      .fetch_optional(&self.read_pool)
      .await?,
    )
  }

  async fn list(&self, tenant_id: &str, limit: i64) -> Result<Vec<Notebook>, DbError> {
    Ok(
      sqlx::query_as::<_, Notebook>(&format!(
        "SELECT {NOTEBOOK_COLUMNS} FROM notebooks WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2"
      ))
      .bind(tenant_id)
      .bind(limit)
      .fetch_all(&self.read_pool)
      .await?,
    )
  }

  async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DbError> {
    let deleted = sqlx::query("DELETE FROM notebooks WHERE id = $1 AND tenant_id = $2")
      .bind(id)
      .bind(tenant_id)
      .execute(&self.pool)
      .await?
      .rows_affected();
    Ok(deleted == 1)
  }

  async fn attach_artifact(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
    artifact_id: Uuid,
    kind: &str,
  ) -> Result<bool, DbError> {
    // Both sides must belong to the tenant; the INSERT ... SELECT only
    // produces a row when they do.
    let inserted = sqlx::query(
      r#"INSERT INTO notebook_artifacts (notebook_id, artifact_id, kind)
         SELECT n.id, a.id, $4
         FROM notebooks n, artifacts a
         WHERE n.id = $1 AND n.tenant_id = $3 AND a.id = $2 AND a.tenant_id = $3
         ON CONFLICT (notebook_id, artifact_id) DO NOTHING"#,
    )
    .bind(notebook_id)
    .bind(artifact_id)
    .bind(tenant_id)
    .bind(kind)
    .execute(&self.pool)
    .await?
    .rows_affected();
    if inserted == 1 {
      return Ok(true);
    }
    // Already linked counts as success.
    let linked: Option<(i32,)> = sqlx::query_as(
      r#"SELECT 1 FROM notebook_artifacts na JOIN notebooks n ON n.id = na.notebook_id
         WHERE na.notebook_id = $1 AND na.artifact_id = $2 AND n.tenant_id = $3"#,
    )
    .bind(notebook_id)
    .bind(artifact_id)
    .bind(tenant_id)
    .fetch_optional(&self.pool)
    .await?;
    Ok(linked.is_some())
  }

  async fn list_artifacts(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
  ) -> Result<Vec<NotebookArtifact>, DbError> {
    #[derive(FromRow)]
    struct Row {
      kind: String,
      #[sqlx(flatten)]
      artifact: Artifact,
    }
    let rows = sqlx::query_as::<_, Row>(
      r#"SELECT na.kind, a.id, a.run_id, a.node_id, a.name, a.path_or_url, a.mime_type,
                a.created_at, a.tenant_id, a.size_bytes, a.sha256
         FROM notebook_artifacts na
         JOIN notebooks n ON n.id = na.notebook_id
         JOIN artifacts a ON a.id = na.artifact_id
         WHERE na.notebook_id = $1 AND n.tenant_id = $2
         ORDER BY na.created_at ASC, a.node_id ASC, a.name ASC"#,
    )
    .bind(notebook_id)
    .bind(tenant_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(
      rows
        .into_iter()
        .map(|row| NotebookArtifact {
          kind: row.kind,
          artifact: row.artifact,
        })
        .collect(),
    )
  }
}

#[async_trait]
impl SourceRepo for PgSourceRepo {
  async fn create(&self, source: NewSource) -> Result<Option<Source>, DbError> {
    Ok(
      sqlx::query_as::<_, Source>(&format!(
        r#"INSERT INTO sources (id, notebook_id, tenant_id, kind, title, uri)
           SELECT $1, n.id, n.tenant_id, $4, $5, $6 FROM notebooks n
           WHERE n.id = $2 AND n.tenant_id = $3
           RETURNING {SOURCE_COLUMNS}"#
      ))
      .bind(Uuid::new_v4())
      .bind(source.notebook_id)
      .bind(&source.tenant_id)
      .bind(source.kind.as_str())
      .bind(&source.title)
      .bind(source.uri.as_deref())
      .fetch_optional(&self.pool)
      .await?,
    )
  }

  async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Source>, DbError> {
    Ok(
      sqlx::query_as::<_, Source>(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources WHERE id = $1 AND tenant_id = $2"
      ))
      .bind(id)
      .bind(tenant_id)
      .fetch_optional(&self.read_pool)
      .await?,
    )
  }

  async fn list_for_notebook(
    &self,
    tenant_id: &str,
    notebook_id: Uuid,
  ) -> Result<Vec<Source>, DbError> {
    Ok(
      sqlx::query_as::<_, Source>(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources WHERE notebook_id = $1 AND tenant_id = $2 ORDER BY created_at ASC"
      ))
      .bind(notebook_id)
      .bind(tenant_id)
      .fetch_all(&self.read_pool)
      .await?,
    )
  }

  async fn update_status(
    &self,
    id: Uuid,
    status: SourceStatus,
    error: Option<&str>,
    sha256: Option<&str>,
  ) -> Result<(), DbError> {
    let updated = sqlx::query(
      r#"UPDATE sources SET status = $2, error = $3, sha256 = COALESCE($4, sha256), updated_at = NOW()
         WHERE id = $1"#,
    )
    .bind(id)
    .bind(status.as_str())
    .bind(error)
    .bind(sha256)
    .execute(&self.pool)
    .await?
    .rows_affected();
    if updated == 0 {
      return Err(DbError::NotFound {
        entity_type: "source",
        id: id.to_string(),
      });
    }
    Ok(())
  }
}
