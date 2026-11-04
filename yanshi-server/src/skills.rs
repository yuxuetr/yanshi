//! Skill catalog + skill-routes (`GET /v1/skills`, `POST /v1/skills/{name}:run`).
//!
//! The catalog wraps `yanshi-skills::SkillRegistryIndex`. "Submit a
//! skill run" creates a `runs` row with the workflow column set to the
//! `@skill:<name>` marker (optionally followed by `\n---\n<user input>`)
//! and dispatches to the same `RunExecutor` that handles `/v1/runs` —
//! `RunContext::skill_dir` carries the resolved manifest directory
//! alongside it so `FlowRunExecutor` (W0.2, `crate::runs::skill_execute`)
//! builds and runs a real `SkillBuilder`-constructed agent instead of
//! trying to parse the marker as a Flow definition.

use axum::{
  Json,
  extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;
use yanshi_skills::{ResolvedSkillRegistryEntry, SkillError, SkillRegistryIndex};

use yanshi_core::FlowCancellationToken;
use yanshi_db::{NewRun, RunRepo, RunStatus};

use crate::AppState;
use crate::error::{ApiError, JsonReq};
use crate::runs::{CreateRunResponse, RunContext};
use crate::tenant::TenantId;
use axum::Extension;

/// Read-only view of a skill registry, suitable for serving over HTTP.
///
/// Holds the index plus the path it was loaded from so `resolve_skill`
/// can re-use the manifest's relative path conventions.
#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
  inner: Arc<SkillCatalogInner>,
}

#[derive(Debug, Default)]
struct SkillCatalogInner {
  index_path: Option<PathBuf>,
  index: Option<SkillRegistryIndex>,
}

impl SkillCatalog {
  /// Empty catalog used when no skills are configured. `GET /v1/skills`
  /// returns `[]` and skill-run requests fail with 404.
  pub fn empty() -> Self {
    Self::default()
  }

  /// Load from `YANSHI_SKILLS_INDEX` (path to `skills.index.toml`).
  /// Returns an empty catalog when the env var is unset; logs a warning
  /// when the env var is set but loading fails so operators can see config
  /// errors without crash-looping the gateway.
  pub fn from_env() -> Self {
    let Ok(raw) = std::env::var("YANSHI_SKILLS_INDEX") else {
      return Self::empty();
    };
    let path = PathBuf::from(raw);
    match SkillRegistryIndex::load(&path) {
      Ok(index) => {
        info!(
          path = %path.display(),
          skill_count = index.entries().len(),
          "loaded skill catalog"
        );
        Self {
          inner: Arc::new(SkillCatalogInner {
            index_path: Some(path),
            index: Some(index),
          }),
        }
      }
      Err(e) => {
        warn!(
          path = %path.display(),
          error = %e,
          "failed to load YANSHI_SKILLS_INDEX; serving empty skill catalog"
        );
        Self::empty()
      }
    }
  }

  /// Construct from an already-parsed index. Used by tests.
  pub fn from_index(index: SkillRegistryIndex, index_path: PathBuf) -> Self {
    Self {
      inner: Arc::new(SkillCatalogInner {
        index_path: Some(index_path),
        index: Some(index),
      }),
    }
  }

  pub fn entries(&self) -> Vec<SkillEntry> {
    self
      .inner
      .index
      .as_ref()
      .map(|idx| idx.entries().iter().map(SkillEntry::from).collect())
      .unwrap_or_default()
  }

  /// Resolve a skill by name or alias. `None` when the catalog is empty
  /// or the name is unknown.
  pub fn resolve(&self, name: &str) -> Option<ResolvedSkillRegistryEntry> {
    let inner = self.inner.as_ref();
    let index = inner.index.as_ref()?;
    let path = inner.index_path.as_deref()?;
    match index.resolve_skill(name, path) {
      Ok(entry) => Some(entry),
      Err(SkillError::ValidationError { .. }) => None,
      Err(e) => {
        warn!(skill = %name, error = %e, "skill resolution failed");
        None
      }
    }
  }
}

/// Public-facing skill metadata. Mirrors `SkillRegistryEntry` but skips
/// implementation detail fields (manifest path, sha256, etc.) so the wire
/// format stays stable across registry implementations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
  pub name: String,
  pub version: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub aliases: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub channel: Option<String>,
}

impl From<&yanshi_skills::SkillRegistryEntry> for SkillEntry {
  fn from(value: &yanshi_skills::SkillRegistryEntry) -> Self {
    Self {
      name: value.name.clone(),
      version: value.version.clone(),
      aliases: value.aliases.clone(),
      channel: value.channel.clone(),
    }
  }
}

#[derive(Debug, Serialize)]
pub struct ListSkillsResponse {
  pub skills: Vec<SkillEntry>,
}

/// `GET /v1/skills` — list installed skills (with version + aliases).
pub async fn list_skills(State(state): State<AppState>) -> Json<ListSkillsResponse> {
  Json(ListSkillsResponse {
    skills: state.skills.entries(),
  })
}

#[derive(Debug, Deserialize)]
pub struct RunSkillRequest {
  /// Free-form input forwarded to the skill agent. Optional so callers
  /// can fire-and-forget skills that don't need user input.
  #[serde(default)]
  pub input: Option<String>,
  #[serde(default)]
  pub tenant_id: Option<String>,
}

/// `POST /v1/skills/{name}:run` — submit a skill execution as a run.
///
/// Resolution is best-effort: the path matches `:run` literally so the
/// route conflicts with no existing skill name. If the catalog has no
/// entry for `name`, return 404. Otherwise persist a queued `runs` row
/// with `workflow = "@skill:<name>"` (+ optional `\n---\n<input>`) and
/// dispatch via the same executor that handles `/v1/runs`, carrying the
/// resolved skill directory on `RunContext::skill_dir` so the executor
/// can build the agent without re-resolving the name.
pub async fn run_skill(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(name): Path<String>,
  JsonReq(req): JsonReq<RunSkillRequest>,
) -> Result<Json<CreateRunResponse>, ApiError> {
  // Path is registered as `/v1/skills/:name_run`; strip the trailing
  // `:run` suffix to recover the bare skill name. Failing the suffix
  // check is a 400 because the route literal can't actually mismatch
  // here — but defensive code is cheap.
  let skill_name = name
    .strip_suffix(":run")
    .ok_or_else(|| ApiError::BadRequest("skill route must end with :run".into()))?;

  let resolved = state.skills.resolve(skill_name).ok_or_else(|| {
    ApiError::NotFound(format!(
      "skill '{}' not installed (configure YANSHI_SKILLS_INDEX)",
      skill_name
    ))
  })?;

  let workflow = match req.input.as_deref() {
    Some(input) if !input.is_empty() => format!("@skill:{}\n---\n{}", resolved.name, input),
    _ => format!("@skill:{}", resolved.name),
  };
  let run_id = Uuid::new_v4();
  let tenant_id = tenant.as_str().to_string();
  // Q1.4.3: body tenant_id is no longer authoritative; if present it
  // must match the auth-bound tenant.
  if let Some(body_tenant) = &req.tenant_id
    && body_tenant != &tenant_id
  {
    return Err(ApiError::TenantMismatch(format!(
      "request body tenant_id '{body_tenant}' does not match authenticated tenant '{tenant_id}'"
    )));
  }

  let run = state
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: workflow.clone(),
      status: RunStatus::Queued,
      run_dir: None,
      tenant_id: tenant_id.clone(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await?;

  let executor = state.executor.clone();
  let repos = state.repos.clone();
  let broker = state.event_broker.clone();
  let cancellation_registry = state.cancellation_registry.clone();
  let live_state_registry = state.live_state_registry.clone();
  let approval_registry = state.approval_registry.clone();
  let run_max_concurrency = state.run_max_concurrency;
  let cancellation_token = FlowCancellationToken::new();
  let task_token = cancellation_token.clone();
  // W0.2: the resolved skill's directory travels alongside the
  // `@skill:<name>` marker so `FlowRunExecutor` can build the agent
  // without re-resolving the name against the catalog.
  let skill_dir = Some(resolved.path.clone());
  let handle = tokio::spawn(async move {
    executor
      .execute(RunContext {
        run_id,
        workflow,
        repos,
        run_base_dir: None,
        cancellation_token: task_token,
        broker,
        tenant_id,
        live_state_registry: Some(live_state_registry),
        skill_dir,
        approval_registry,
        approval_timeout: crate::serve::HARNESS_APPROVAL_TIMEOUT,
        run_max_concurrency,
        cost_limit_usd: None,
        resume: false,
      })
      .await;
    cancellation_registry.complete(run_id);
  });
  state
    .cancellation_registry
    .register(run_id, cancellation_token, handle.abort_handle());
  if handle.is_finished() {
    state.cancellation_registry.complete(run_id);
  }

  Ok(Json(CreateRunResponse {
    run_id: run.id,
    status: "queued",
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_catalog_lists_no_skills() {
    let catalog = SkillCatalog::empty();
    assert!(catalog.entries().is_empty());
    assert!(catalog.resolve("anything").is_none());
  }
}
