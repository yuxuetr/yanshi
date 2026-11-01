//! gRPC client (`GrpcWorkerProtocol`) + the proto <-> domain conversions and
//! traceparent helpers shared with the server-side service.
//!
//! Moved from `yanshi-server::scheduler::grpc` in P-A2.3. The hand-written
//! tower `Service` (`WorkerControlServer`) + the `WorkerControl` trait stay in
//! `yanshi-server`, which imports the conversions + helpers from here.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tonic::body::BoxBody;
use tonic::client::Grpc;
use tonic::codegen::http;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};
use uuid::Uuid;

// T1.2: re-exported so `yanshi-worker` can build a `ClientTlsConfig`
// from PEM file bytes without taking its own direct dependency on tonic
// (it depends on this crate for the gRPC transport already).
pub use tonic::transport::{Certificate, ClientTlsConfig, Identity};

use crate::pb;
use crate::{
  SchedulerError, WorkerHeartbeat, WorkerId, WorkerProtocol, WorkerTask, WorkerTaskResult,
  WorkerTraceEvent,
};

/// Boxed-`Status` result used by the proto <-> domain conversion helpers.
/// `tonic::Status` is ~176 bytes (clippy::result_large_err), so helpers box it.
pub type BoxedStatusResult<T> = std::result::Result<T, Box<Status>>;

/// gRPC client adapter implementing [`WorkerProtocol`]. Cloning is cheap — the
/// inner channel is an `Arc::clone`.
#[derive(Debug, Clone)]
pub struct GrpcWorkerProtocol {
  channel: Channel,
  /// Q1.6.1: admission token sent as `authorization: Bearer <token>`
  /// gRPC metadata on every call. `None` keeps the legacy "no auth"
  /// path for the `memory://local` and dev profiles.
  admission_token: Option<Arc<str>>,
}

impl GrpcWorkerProtocol {
  pub async fn connect(endpoint: impl AsRef<str>) -> Result<Self, SchedulerError> {
    let channel = Endpoint::from_shared(endpoint.as_ref().to_string())
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?
      .connect()
      .await
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?;
    Ok(Self {
      channel,
      admission_token: None,
    })
  }

  pub fn from_channel(channel: Channel) -> Self {
    Self {
      channel,
      admission_token: None,
    }
  }

  /// T1.2: connect over TLS (or mTLS, when `tls` carries a client
  /// [`Identity`]). `endpoint` must use the `https://` scheme — tonic
  /// rejects a `tls_config` paired with `http://`.
  pub async fn connect_tls(
    endpoint: impl AsRef<str>,
    tls: ClientTlsConfig,
  ) -> Result<Self, SchedulerError> {
    let channel = Endpoint::from_shared(endpoint.as_ref().to_string())
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?
      .tls_config(tls)
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?
      .connect()
      .await
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?;
    Ok(Self {
      channel,
      admission_token: None,
    })
  }

  /// Pin the admission token attached to every outgoing call as
  /// `authorization: Bearer <token>` metadata. Required when the
  /// server side runs `AuthenticatedGrpcWorkerService`; harmless
  /// (just ignored) when the server is the unauthenticated
  /// `GrpcWorkerService`.
  pub fn with_admission_token(mut self, token: impl Into<String>) -> Self {
    self.admission_token = Some(Arc::from(token.into()));
    self
  }

  async fn unary<Req, Resp>(
    &self,
    path: &'static str,
    request: Req,
  ) -> Result<Response<Resp>, SchedulerError>
  where
    Req: prost::Message + Default + 'static,
    Resp: prost::Message + Default + 'static,
  {
    // Q3.3.1: fresh `Grpc::new` per call — cloning the underlying
    // `Channel` is an `Arc::clone`, and `Grpc` itself just wraps
    // the channel + codec. The `.ready()` ping is preserved because
    // tonic's `Channel` uses an internal `Buffer<...>` whose
    // `Service::call` contract requires `poll_ready` to have been
    // observed first; the previous serializing mutex hid that
    // requirement from the call site but the constraint is real.
    // `ready()` itself is cheap — it just polls the buffer state.
    let mut inner = Grpc::new(self.channel.clone());
    inner
      .ready()
      .await
      .map_err(|err| SchedulerError::Transport {
        message: err.to_string(),
      })?;
    let path = http::uri::PathAndQuery::from_static(path);
    let codec = tonic::codec::ProstCodec::default();

    // P3.8: cross-hop W3C traceparent propagation. When the caller
    // is running inside an `yanshi_tracing::context::scope`,
    // inject the active value as a gRPC `traceparent` metadata
    // entry. Mirrors the lowercase HTTP-header spelling so OTel
    // tools that already grep "traceparent" match without
    // translation. Outside any scope, the metadata is omitted —
    // consumers can tell apart "no upstream trace" from "upstream
    // trace exists but is malformed".
    let mut req = Request::new(request);
    inject_traceparent_into_grpc_request(&mut req);
    if let Some(token) = self.admission_token.as_deref() {
      // Q1.6.1: every outbound RPC carries the admission credential.
      // We rebuild the value each call instead of storing an
      // AsciiMetadataValue because tonic's metadata map clones
      // cheaply but the value itself isn't `Sync` in all versions.
      let header = format!("Bearer {token}");
      match tonic::metadata::AsciiMetadataValue::try_from(header.as_str()) {
        Ok(value) => {
          req.metadata_mut().insert("authorization", value);
        }
        Err(_) => {
          // A token containing non-ASCII bytes is an operator error;
          // refuse to send instead of letting the server pin the
          // failure on a malformed header.
          return Err(SchedulerError::Transport {
            message: "admission token contains non-ASCII bytes; refusing to send".into(),
          });
        }
      }
    }
    inner
      .unary(req, path, codec)
      .await
      .map_err(scheduler_error_from_status)
  }
}

/// Inject the active `yanshi_tracing::context::current_traceparent`
/// into a tonic `Request`'s gRPC metadata as the `traceparent` key.
/// No-op when there is no active context — see the module-level
/// comment for the rationale behind omitting (not emitting an empty
/// value).
///
/// Exposed as `pub` so cross-hop integration tests (and any
/// downstream consumers wiring custom RPC paths) can reuse the
/// canonical injection logic without re-implementing it.
pub fn inject_traceparent_into_grpc_request<T>(request: &mut Request<T>) {
  let Some(traceparent) = yanshi_tracing::context::current_traceparent() else {
    return;
  };
  match tonic::metadata::AsciiMetadataValue::try_from(traceparent.as_str()) {
    Ok(value) => {
      request.metadata_mut().insert("traceparent", value);
    }
    Err(_err) => {
      // Defensive: a traceparent that contains non-ASCII bytes
      // would be a bug upstream. Drop it silently rather than
      // poisoning the entire RPC — tracing is observability, not
      // a correctness path.
      tracing::warn!("skipping traceparent gRPC injection: value contains non-ASCII bytes");
    }
  }
}

/// Extract the `traceparent` gRPC metadata value from an incoming
/// `Request`. Returns `None` when the metadata key is absent or
/// holds a non-ASCII value. Servers use this to install the parent
/// context via `yanshi_tracing::context::scope` before dispatch.
pub fn extract_traceparent_from_grpc_request<T>(request: &Request<T>) -> Option<String> {
  request
    .metadata()
    .get("traceparent")
    .and_then(|value| value.to_str().ok())
    .map(str::to_owned)
}

/// Run `fut` inside `yanshi_tracing::context::scope` when
/// `traceparent` is `Some`, or plain otherwise. Centralises the
/// scope-vs-no-scope branch the gRPC handler stubs all need.
///
/// Public-in-crate so the four inner unary handler `Svc` structs
/// share one implementation. Generic over the future type so each
/// `WorkerControl` method (which returns a different `Response<R>`)
/// can use it without boxing.
pub async fn run_in_traceparent_scope<F, T>(traceparent: Option<String>, fut: F) -> T
where
  F: std::future::Future<Output = T>,
{
  match traceparent {
    Some(tp) => yanshi_tracing::context::scope(tp, fut).await,
    None => fut.await,
  }
}

#[async_trait]
impl WorkerProtocol for GrpcWorkerProtocol {
  async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
    self
      .unary::<_, pb::Empty>(
        "/yanshi.scheduler.v1.WorkerControl/SubmitTask",
        pb::SubmitTaskRequest {
          task: Some(worker_task_to_proto(task)),
        },
      )
      .await?;
    Ok(())
  }

  async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
    // Pre-FU1 path: no hints. Equivalent to
    // `claim_task_with_hints(worker_id, &ClaimHints::none())`.
    self
      .claim_task_with_hints(worker_id, &crate::ClaimHints::none())
      .await
  }

  async fn claim_task_with_hints(
    &self,
    worker_id: WorkerId,
    hints: &crate::ClaimHints,
  ) -> Result<Option<WorkerTask>, SchedulerError> {
    let response = self
      .unary::<_, pb::ClaimTaskResponse>(
        "/yanshi.scheduler.v1.WorkerControl/ClaimTask",
        pb::ClaimTaskRequest {
          worker_id: worker_id.0,
          accepted_node_types: hints.capabilities.node_types.clone(),
          locality_run_id: hints
            .locality_run_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        },
      )
      .await?
      .into_inner();
    response
      .task
      .map(worker_task_from_proto)
      .transpose()
      .map_err(|boxed| scheduler_error_from_status(*boxed))
  }

  async fn report_result(
    &self,
    worker_id: WorkerId,
    task_id: Uuid,
    result: WorkerTaskResult,
  ) -> Result<(), SchedulerError> {
    self
      .unary::<_, pb::Empty>(
        "/yanshi.scheduler.v1.WorkerControl/ReportResult",
        pb::ReportResultRequest {
          worker_id: worker_id.0,
          task_id: task_id.to_string(),
          result: Some(worker_task_result_to_proto(result)),
        },
      )
      .await?;
    Ok(())
  }

  async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
    self
      .unary::<_, pb::Empty>(
        "/yanshi.scheduler.v1.WorkerControl/Heartbeat",
        worker_heartbeat_to_proto(heartbeat),
      )
      .await?;
    Ok(())
  }
}

pub fn worker_task_to_proto(task: WorkerTask) -> pb::WorkerTask {
  pb::WorkerTask {
    task_id: task.task_id.to_string(),
    run_id: task.run_id.to_string(),
    node_id: task.node_id,
    attempt: task.attempt,
    payload_json: task.payload.to_string(),
    // P10.16.2-FU1: capability label round-trips across the wire.
    // `None` ↔ empty string so a pre-FU1 worker that ignores the
    // field still sees a coherent task.
    node_type: task.node_type.unwrap_or_default(),
  }
}

pub fn worker_task_from_proto(task: pb::WorkerTask) -> BoxedStatusResult<WorkerTask> {
  Ok(WorkerTask {
    task_id: parse_uuid(&task.task_id, "task_id")?,
    run_id: parse_uuid(&task.run_id, "run_id")?,
    node_id: task.node_id,
    attempt: task.attempt,
    payload: parse_json(&task.payload_json, "payload_json")?,
    // P10.16.2-FU1: empty string → None preserves the
    // "untagged task" semantic that
    // `WorkerCapabilities::accepts` always accepts.
    node_type: if task.node_type.is_empty() {
      None
    } else {
      Some(task.node_type)
    },
  })
}

pub fn worker_trace_event_to_proto(event: WorkerTraceEvent) -> pb::WorkerTraceEvent {
  pb::WorkerTraceEvent {
    seq: event.seq,
    kind: event.kind,
    payload_json: event.payload.to_string(),
  }
}

pub fn worker_trace_event_from_proto(
  event: pb::WorkerTraceEvent,
) -> BoxedStatusResult<WorkerTraceEvent> {
  Ok(WorkerTraceEvent {
    seq: event.seq,
    kind: event.kind,
    payload: parse_json(&event.payload_json, "payload_json")?,
  })
}

pub fn worker_task_result_to_proto(result: WorkerTaskResult) -> pb::WorkerTaskResult {
  match result {
    WorkerTaskResult::Succeeded { output, events } => pb::WorkerTaskResult {
      status: pb::worker_task_result::Status::Succeeded as i32,
      output_json: output.to_string(),
      error: String::new(),
      retryable: false,
      events: events
        .into_iter()
        .map(worker_trace_event_to_proto)
        .collect(),
    },
    WorkerTaskResult::Failed {
      error,
      retryable,
      events,
    } => pb::WorkerTaskResult {
      status: pb::worker_task_result::Status::Failed as i32,
      output_json: String::new(),
      error,
      retryable,
      events: events
        .into_iter()
        .map(worker_trace_event_to_proto)
        .collect(),
    },
  }
}

pub fn worker_task_result_from_proto(
  result: pb::WorkerTaskResult,
) -> BoxedStatusResult<WorkerTaskResult> {
  let events = result
    .events
    .into_iter()
    .map(worker_trace_event_from_proto)
    .collect::<Result<Vec<_>, _>>()?;
  match pb::worker_task_result::Status::try_from(result.status) {
    Ok(pb::worker_task_result::Status::Succeeded) => Ok(WorkerTaskResult::Succeeded {
      output: parse_json(&result.output_json, "output_json")?,
      events,
    }),
    Ok(pb::worker_task_result::Status::Failed) => Ok(WorkerTaskResult::Failed {
      error: result.error,
      retryable: result.retryable,
      events,
    }),
    Ok(pb::worker_task_result::Status::Unspecified) | Err(_) => Err(Box::new(
      Status::invalid_argument("result status is required"),
    )),
  }
}

pub fn worker_heartbeat_to_proto(heartbeat: WorkerHeartbeat) -> pb::HeartbeatRequest {
  pb::HeartbeatRequest {
    worker_id: heartbeat.worker_id.0,
    active_task_id: heartbeat
      .active_task
      .map(|task_id| task_id.to_string())
      .unwrap_or_default(),
    free_slots: heartbeat.free_slots,
    timestamp_rfc3339: heartbeat.ts.to_rfc3339(),
    // P10.16.2-FU1: heartbeats round-trip capability
    // advertisements. Empty `node_types` ↔ empty repeated
    // string, so pre-FU1 workers (which never set this field)
    // produce the same wire bytes as a worker explicitly
    // advertising "any task."
    accepted_node_types: heartbeat.capabilities.node_types,
  }
}

pub fn worker_heartbeat_from_proto(
  heartbeat: pb::HeartbeatRequest,
) -> BoxedStatusResult<WorkerHeartbeat> {
  let worker_id = WorkerId::new(heartbeat.worker_id)
    .map_err(|err| Box::new(Status::invalid_argument(err.to_string())))?;
  let active_task = if heartbeat.active_task_id.is_empty() {
    None
  } else {
    Some(parse_uuid(&heartbeat.active_task_id, "active_task_id")?)
  };
  let ts = if heartbeat.timestamp_rfc3339.is_empty() {
    Utc::now()
  } else {
    DateTime::parse_from_rfc3339(&heartbeat.timestamp_rfc3339)
      .map_err(|err| {
        Box::new(Status::invalid_argument(format!(
          "invalid timestamp_rfc3339: {err}"
        )))
      })?
      .with_timezone(&Utc)
  };
  Ok(WorkerHeartbeat {
    worker_id,
    active_task,
    free_slots: heartbeat.free_slots,
    ts,
    // P10.16.2-FU1: capability advertisement reads from the
    // wire. An empty list means "any task" (the pre-FU1 default)
    // — the policy layer in `InMemoryWorkerProtocol` and
    // `WorkerCapabilities::accepts` already handle that case.
    capabilities: crate::WorkerCapabilities {
      node_types: heartbeat.accepted_node_types,
    },
  })
}

/// Convert a `pb::ClaimTaskRequest` into a [`ClaimHints`](crate::protocol::ClaimHints)
/// (P10.16.2-FU1). Empty values map cleanly to "no preference"
/// so pre-FU1 workers continue to get FIFO dispatch.
pub fn claim_hints_from_proto(
  request: &pb::ClaimTaskRequest,
) -> BoxedStatusResult<crate::ClaimHints> {
  let locality_run_id = if request.locality_run_id.is_empty() {
    None
  } else {
    Some(parse_uuid(&request.locality_run_id, "locality_run_id")?)
  };
  Ok(crate::ClaimHints {
    capabilities: crate::WorkerCapabilities {
      node_types: request.accepted_node_types.clone(),
    },
    locality_run_id,
  })
}

pub fn parse_uuid(value: &str, field: &str) -> BoxedStatusResult<Uuid> {
  Uuid::parse_str(value)
    .map_err(|err| Box::new(Status::invalid_argument(format!("invalid {field}: {err}"))))
}

pub fn parse_json(value: &str, field: &str) -> BoxedStatusResult<serde_json::Value> {
  if value.is_empty() {
    return Ok(serde_json::Value::Null);
  }
  serde_json::from_str(value)
    .map_err(|err| Box::new(Status::invalid_argument(format!("invalid {field}: {err}"))))
}

pub fn status_from_error(error: SchedulerError) -> Status {
  match error {
    SchedulerError::InvalidWorkerId => Status::invalid_argument(error.to_string()),
    SchedulerError::TaskNotClaimed { .. } | SchedulerError::WorkerMismatch { .. } => {
      Status::failed_precondition(error.to_string())
    }
    SchedulerError::Transport { .. } => Status::unavailable(error.to_string()),
  }
}

pub fn scheduler_error_from_status(status: Status) -> SchedulerError {
  SchedulerError::Transport {
    message: status.to_string(),
  }
}

pub fn empty_body() -> BoxBody {
  tonic::body::empty_body()
}
