//! Binary entry for the Yanshi Gateway. All real boot logic lives
//! in [`yanshi_server::serve`] so the same code path is shared with
//! the `yanshi serve` CLI subcommand (P2.1).
//!
//! Flag surface:
//! - `--check` — non-binding readiness diagnostics; prints a JSON
//!   report and exits with `0` (Ok), `1` (Warn), or `2` (Fail).
//! - any other args are passed through but ignored for now.
//!
//! All other configuration is taken from environment variables so the
//! `yanshi serve` CLI subcommand can drive this binary by setting
//! env vars + arguments without linking the server crate (which would
//! introduce a cycle with `yanshi-cli`).

use std::net::SocketAddr;

use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use yanshi_db::Database;
use yanshi_server::{
  CleanupConfig, DEFAULT_SERVE_BIND, ServeConfig, ServeError, YANSHI_SERVE_BIND_ENV,
  apply_env_overrides, cleanup_expired, run, run_check,
};
use yanshi_tools::{SECURITY_PROFILE_ENV, SecurityProfile};

/// Synchronous so `.env` loading and the config's env overrides mutate the
/// environment before the tokio runtime starts any threads (eval 2026-10-07
/// L8: they used to run inside `#[tokio::main]`).
fn main() -> Result<(), Box<dyn std::error::Error>> {
  tracing_subscriber::registry()
    .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
    .with(tracing_subscriber::fmt::layer())
    .init();

  let _ = dotenvy::dotenv();

  let args: Vec<String> = std::env::args().collect();
  let check_mode = args.iter().any(|arg| arg == "--check");
  let cleanup_mode = args.iter().any(|arg| arg == "--cleanup");
  let dry_run = args.iter().any(|arg| arg == "--dry-run");

  let config = build_config_from_env()?;
  if !check_mode && !cleanup_mode {
    // SAFETY: single-threaded — the runtime is built below.
    unsafe { apply_env_overrides(&config) };
  }
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?;

  if check_mode {
    let report = runtime.block_on(run_check(config))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    std::process::exit(report.readiness.exit_code());
  }

  if cleanup_mode {
    let report = runtime.block_on(run_cleanup_once(&config, dry_run))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    return Ok(());
  }

  match runtime.block_on(run(config)) {
    Ok(()) => Ok(()),
    Err(err) => {
      eprintln!("{err}");
      match &err {
        ServeError::Database(_) | ServeError::MissingDatabaseUrl => Err(err.into()),
        _ => Err(err.into()),
      }
    }
  }
}

fn build_config_from_env() -> Result<ServeConfig, Box<dyn std::error::Error>> {
  let bind = resolve_bind()?;
  let security_profile = SecurityProfile::from_env().map_err(|err| {
    eprintln!("Invalid {SECURITY_PROFILE_ENV}: {err}");
    err
  })?;
  let database_url = std::env::var("DATABASE_URL")
    .ok()
    .filter(|value| !value.trim().is_empty());
  // P10.15.2 read-replica opt-in. `YANSHI_DATABASE_READ_URL`
  // (or the CLI flag `--database-read-url` via the
  // `yanshi serve` shim) points at the replica. Empty / unset
  // → reads fall back to the primary, preserving single-node
  // behavior.
  let read_database_url = std::env::var("YANSHI_DATABASE_READ_URL")
    .ok()
    .filter(|value| !value.trim().is_empty());
  let auth_token_env =
    std::env::var("YANSHI_SERVE_AUTH_TOKEN_ENV").unwrap_or_else(|_| "YANSHI_API_TOKEN".to_string());

  Ok(ServeConfig {
    bind,
    database_url,
    read_database_url,
    run_dir: std::env::var("YANSHI_RUN_DIR").ok().map(Into::into),
    trace_dir: std::env::var("YANSHI_TRACE_DIR").ok().map(Into::into),
    security_profile,
    auth_token_env,
    cors_origins: Vec::new(),
    max_body_mb: None,
    worker_grpc: build_worker_grpc_config_from_env()?,
  })
}

/// T1.2: `YANSHI_WORKER_GRPC_BIND` (e.g. `0.0.0.0:50051`) turns the
/// worker gRPC control-plane listener on; its absence keeps pre-T1.2
/// behavior (`worker_grpc: None`, no socket bound). TLS activates when
/// both `YANSHI_WORKER_GRPC_TLS_CERT` and `..._TLS_KEY` are set;
/// `YANSHI_WORKER_GRPC_CLIENT_CA` on top of that additionally
/// requires and verifies a client certificate (mTLS).
/// `YANSHI_WORKER_IDS` (comma-separated) + `YANSHI_WORKER_PSK`
/// configure a single shared pre-shared-key admission policy — see
/// `yanshi_server::worker_grpc::WorkerGrpcServeConfig` for the
/// per-worker-PSK / JWT alternatives available to direct library
/// callers.
fn build_worker_grpc_config_from_env()
-> Result<Option<yanshi_server::worker_grpc::WorkerGrpcServeConfig>, Box<dyn std::error::Error>> {
  let Some(bind) = std::env::var("YANSHI_WORKER_GRPC_BIND").ok() else {
    return Ok(None);
  };
  let bind: SocketAddr = bind.parse()?;

  let tls_cert = std::env::var("YANSHI_WORKER_GRPC_TLS_CERT").ok();
  let tls_key = std::env::var("YANSHI_WORKER_GRPC_TLS_KEY").ok();
  let client_ca = std::env::var("YANSHI_WORKER_GRPC_CLIENT_CA")
    .ok()
    .map(PathBuf::from);
  let tls = match (tls_cert, tls_key) {
    (Some(cert), Some(key)) => Some(yanshi_server::worker_grpc::WorkerGrpcTlsConfig {
      cert_pem_path: PathBuf::from(cert),
      key_pem_path: PathBuf::from(key),
      client_ca_pem_path: client_ca,
    }),
    (None, None) => None,
    _ => {
      return Err(
        "YANSHI_WORKER_GRPC_TLS_CERT and YANSHI_WORKER_GRPC_TLS_KEY must both be set, or both unset"
          .into(),
      );
    }
  };

  let allowed_worker_ids = std::env::var("YANSHI_WORKER_IDS")
    .ok()
    .map(|raw| {
      raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  let shared_psk = std::env::var("YANSHI_WORKER_PSK").ok();

  Ok(Some(yanshi_server::worker_grpc::WorkerGrpcServeConfig {
    bind,
    tls,
    allowed_worker_ids,
    shared_psk,
  }))
}

async fn run_cleanup_once(
  config: &ServeConfig,
  dry_run: bool,
) -> Result<yanshi_server::CleanupReport, Box<dyn std::error::Error>> {
  let cleanup_cfg = CleanupConfig::for_profile(config.security_profile).with_dry_run(dry_run);
  // V4.4: fall back to the same `~/.yanshi/{runs,traces}` defaults
  // `yanshi workflow run`/`yanshi trace *` use when the CLI's own
  // `--run-dir`/`--trace-dir`/env vars are unset — otherwise `yanshi
  // cleanup` with no flags silently sweeps nothing on exactly the
  // no-configuration setup it's meant to help (a long-running machine
  // that only ever ran `yanshi workflow run` locally and never set
  // YANSHI_RUN_DIR).
  let run_root: Option<PathBuf> = config
    .run_dir
    .clone()
    .or_else(default_yanshi_subdir("runs"));
  let trace_root: Option<PathBuf> = config
    .trace_dir
    .clone()
    .or_else(default_yanshi_subdir("traces"));

  match config.database_url.as_ref() {
    Some(db_url) => {
      let db = Database::connect_and_migrate(db_url, 4).await?;
      let report = cleanup_expired(
        &db,
        run_root.as_deref(),
        trace_root.as_deref(),
        &cleanup_cfg,
      )
      .await?;
      Ok(report)
    }
    // V4.4: no DATABASE_URL configured — this used to hard-fail before
    // touching the filesystem at all, so a purely local CLI user (never
    // ran `yanshi serve`, no Postgres anywhere) had no way to reap
    // `~/.yanshi/runs`/`~/.yanshi/traces` short of standing up a
    // database just to run a cleanup sweep. Fall back to the DB-free
    // filesystem-only sweep instead — it skips the `runs`/`events`/
    // `artifacts` table sweeps (nothing to skip when there's no DB) but
    // still reaps stale run/trace directories by mtime.
    None => {
      let report = yanshi_server::cleanup_expired_local(
        run_root.as_deref(),
        trace_root.as_deref(),
        &cleanup_cfg,
      )
      .await?;
      Ok(report)
    }
  }
}

/// `Some(~/.yanshi/<tail>)`, or `None` when the home directory can't
/// be resolved (matches the CLI's own `dirs::home_dir()`-based fallback
/// convention for `--run-dir`/`--trace-dir`).
fn default_yanshi_subdir(tail: &str) -> impl FnOnce() -> Option<PathBuf> + '_ {
  move || dirs::home_dir().map(|home| home.join(".yanshi").join(tail))
}

fn resolve_bind() -> Result<SocketAddr, std::net::AddrParseError> {
  // Backwards compatibility: prefer the historical `PORT` env, then the
  // new `YANSHI_SERVE_BIND`, then the documented default.
  if let Ok(port) = std::env::var("PORT") {
    return format!("0.0.0.0:{port}").parse();
  }
  if let Ok(addr) = std::env::var(YANSHI_SERVE_BIND_ENV) {
    return addr.parse();
  }
  DEFAULT_SERVE_BIND.parse()
}
