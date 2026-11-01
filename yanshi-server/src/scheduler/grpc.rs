//! gRPC transport adapter for distributed workers.
//!
//! The protobuf schema is checked in under
//! `proto/yanshi/scheduler/v1/worker.proto`. As of Q3.3.3 the
//! prost message structs in `pb` are generated from that .proto by
//! `build.rs` (`tonic-build`) so non-Rust language bindings stay
//! byte-compatible with the Rust path. Only the message structs are
//! generated — the tonic `Service` impl + the `WorkerControl` trait
//! stay hand-written below because they wire in custom W3C
//! traceparent scope handling + admission credential extraction
//! that the generated stubs do not model.

use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tonic::body::BoxBody;
use tonic::codegen::{BoxFuture, Service, StdError, http};
use tonic::{Request, Response, Status};

use super::{WorkerControlPlane, WorkerId, WorkerProtocol};
// The generated `pb` messages, the proto <-> domain conversions, the
// `BoxedStatusResult` alias, and the traceparent helpers moved to
// `yanshi-worker-proto` (P-A2.3); the hand-written tower service below
// imports them from there.
use yanshi_worker_proto::grpc::*;
use yanshi_worker_proto::pb;

/// Server-side implementation of the worker gRPC service.
#[derive(Debug, Clone)]
pub struct GrpcWorkerService<P> {
  protocol: P,
}

impl<P> GrpcWorkerService<P> {
  pub fn new(protocol: P) -> Self {
    Self { protocol }
  }
}

/// Q1.6.1: extract the `authorization` metadata from a tonic request as
/// a bare token string. Returns `None` when the header is absent or
/// malformed (caller decides whether absence is fatal). Strips the
/// `Bearer ` prefix when present so callers receive only the PSK value.
fn extract_admission_token<T>(request: &Request<T>) -> Option<String> {
  let header = request.metadata().get("authorization")?;
  let value = header.to_str().ok()?;
  let token = value.strip_prefix("Bearer ").unwrap_or(value);
  if token.is_empty() {
    None
  } else {
    Some(token.to_string())
  }
}

/// Server-side authenticated worker gRPC service. Each call extracts
/// `authorization` metadata, builds a [`WorkerCredential`](super::admission::WorkerCredential), and routes
/// through [`AuthenticatedControlPlane`](super::admission::AuthenticatedControlPlane) which enforces PSK / JWT /
/// fleet-cap admission before delegating to the inner protocol.
///
/// Closes Q1.6.1: the unauthenticated [`GrpcWorkerService`] is still
/// available for the `memory://local` and in-process test paths, but
/// production gRPC deployments wire this struct instead.
#[derive(Debug, Clone)]
pub struct AuthenticatedGrpcWorkerService<P> {
  authenticated: Arc<crate::scheduler::admission::AuthenticatedControlPlane<P>>,
}

impl<P> AuthenticatedGrpcWorkerService<P> {
  pub fn new(
    authenticated: Arc<crate::scheduler::admission::AuthenticatedControlPlane<P>>,
  ) -> Self {
    Self { authenticated }
  }
}

#[async_trait]
impl<P> WorkerControl for AuthenticatedGrpcWorkerService<P>
where
  P: WorkerProtocol + Clone + 'static,
{
  /// `submit_task` is gateway → server (not worker → server), so its
  /// wire message carries no `worker_id` to check against
  /// `allowed_workers`/`max_concurrent_tasks_per_worker` individually.
  ///
  /// V0.5: this endpoint used to accept every call unconditionally,
  /// wrongly assuming the standalone worker gRPC listener
  /// (`serve_worker_grpc`) sits behind the HTTP bearer middleware —
  /// it binds its own independent socket and does not. Anyone who
  /// could reach that socket could inject arbitrary `WorkerTask`s into
  /// the queue, regardless of the configured admission policy. Now
  /// routes through [`AuthenticatedControlPlane::admit_any`](super::admission::AuthenticatedControlPlane::admit_any), the same
  /// identity-agnostic check documented for every RPC in
  /// `docs/DISTRIBUTED.md` § Worker Admission.
  async fn submit_task(
    &self,
    request: Request<pb::SubmitTaskRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let token = extract_admission_token(&request);
    self
      .authenticated
      .admit_any(token.as_deref())
      .await
      .map_err(|err| Status::permission_denied(err.to_string()))?;
    let task = request
      .into_inner()
      .task
      .ok_or_else(|| Box::new(Status::invalid_argument("task is required")))
      .and_then(worker_task_from_proto)
      .map_err(|e| *e)?;
    self
      .authenticated
      .inner()
      .schedule_task(task)
      .await
      .map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn claim_task(
    &self,
    request: Request<pb::ClaimTaskRequest>,
  ) -> Result<Response<pb::ClaimTaskResponse>, Status> {
    let token = extract_admission_token(&request);
    let inner = request.into_inner();
    let worker_id = WorkerId::new(inner.worker_id.clone())
      .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let credential = crate::scheduler::admission::WorkerCredential::new(worker_id.clone(), token);
    let task = self
      .authenticated
      .claim_task(credential)
      .await
      .map_err(authn_status_from_error)?
      .map(worker_task_to_proto);
    Ok(Response::new(pb::ClaimTaskResponse { task }))
  }

  async fn report_result(
    &self,
    request: Request<pb::ReportResultRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let token = extract_admission_token(&request);
    let inner = request.into_inner();
    let worker_id =
      WorkerId::new(inner.worker_id).map_err(|err| Status::invalid_argument(err.to_string()))?;
    let task_id = parse_uuid(&inner.task_id, "task_id").map_err(|e| *e)?;
    let result = inner
      .result
      .ok_or_else(|| Box::new(Status::invalid_argument("result is required")))
      .and_then(worker_task_result_from_proto)
      .map_err(|e| *e)?;
    let credential = crate::scheduler::admission::WorkerCredential::new(worker_id, token);
    self
      .authenticated
      .report_result(credential, task_id, result)
      .await
      .map_err(authn_status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn heartbeat(
    &self,
    request: Request<pb::HeartbeatRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let token = extract_admission_token(&request);
    let inner = request.into_inner();
    let worker_id = WorkerId::new(inner.worker_id.clone())
      .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let heartbeat = worker_heartbeat_from_proto(inner).map_err(|e| *e)?;
    let credential = crate::scheduler::admission::WorkerCredential::new(worker_id, token);
    self
      .authenticated
      .heartbeat(credential, heartbeat)
      .await
      .map_err(authn_status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }
}

/// Map [`crate::scheduler::admission::ControlError`] to a tonic Status,
/// surfacing admission failures as `permission_denied` so the client
/// distinguishes auth rejection from transport / state errors.
fn authn_status_from_error(err: crate::scheduler::admission::ControlError) -> Status {
  match err {
    crate::scheduler::admission::ControlError::Admission(adm) => {
      Status::permission_denied(adm.to_string())
    }
    crate::scheduler::admission::ControlError::Scheduler(sched) => status_from_error(sched),
  }
}

#[async_trait]
pub trait WorkerControl: Send + Sync + 'static {
  async fn submit_task(
    &self,
    request: Request<pb::SubmitTaskRequest>,
  ) -> Result<Response<pb::Empty>, Status>;

  async fn claim_task(
    &self,
    request: Request<pb::ClaimTaskRequest>,
  ) -> Result<Response<pb::ClaimTaskResponse>, Status>;

  async fn report_result(
    &self,
    request: Request<pb::ReportResultRequest>,
  ) -> Result<Response<pb::Empty>, Status>;

  async fn heartbeat(
    &self,
    request: Request<pb::HeartbeatRequest>,
  ) -> Result<Response<pb::Empty>, Status>;
}

#[async_trait]
impl<P> WorkerControl for GrpcWorkerService<P>
where
  P: WorkerProtocol + 'static,
{
  async fn submit_task(
    &self,
    request: Request<pb::SubmitTaskRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let task = request
      .into_inner()
      .task
      .ok_or_else(|| Box::new(Status::invalid_argument("task is required")))
      .and_then(worker_task_from_proto)
      .map_err(|e| *e)?;
    self
      .protocol
      .submit_task(task)
      .await
      .map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn claim_task(
    &self,
    request: Request<pb::ClaimTaskRequest>,
  ) -> Result<Response<pb::ClaimTaskResponse>, Status> {
    let request = request.into_inner();
    let worker_id = WorkerId::new(request.worker_id.clone())
      .map_err(|err| Status::invalid_argument(err.to_string()))?;
    // P10.16.2-FU1: route through `claim_task_with_hints` so the
    // worker's advertised capabilities + locality preference
    // filter the queue. Pre-FU1 workers send empty fields, which
    // `ClaimHints::default()` interprets as "no preference" —
    // identical to the bare `claim_task` path.
    let hints = claim_hints_from_proto(&request).map_err(|e| *e)?;
    let task = self
      .protocol
      .claim_task_with_hints(worker_id, &hints)
      .await
      .map_err(status_from_error)?
      .map(worker_task_to_proto);
    Ok(Response::new(pb::ClaimTaskResponse { task }))
  }

  async fn report_result(
    &self,
    request: Request<pb::ReportResultRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let request = request.into_inner();
    let worker_id =
      WorkerId::new(request.worker_id).map_err(|err| Status::invalid_argument(err.to_string()))?;
    let task_id = parse_uuid(&request.task_id, "task_id").map_err(|e| *e)?;
    let result = request
      .result
      .ok_or_else(|| Box::new(Status::invalid_argument("result is required")))
      .and_then(worker_task_result_from_proto)
      .map_err(|e| *e)?;
    self
      .protocol
      .report_result(worker_id, task_id, result)
      .await
      .map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn heartbeat(
    &self,
    request: Request<pb::HeartbeatRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let heartbeat = worker_heartbeat_from_proto(request.into_inner()).map_err(|e| *e)?;
    self
      .protocol
      .heartbeat(heartbeat)
      .await
      .map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }
}

#[async_trait]
impl<P> WorkerControl for WorkerControlPlane<P>
where
  P: WorkerProtocol + 'static,
{
  async fn submit_task(
    &self,
    request: Request<pb::SubmitTaskRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let task = request
      .into_inner()
      .task
      .ok_or_else(|| Box::new(Status::invalid_argument("task is required")))
      .and_then(worker_task_from_proto)
      .map_err(|e| *e)?;
    self.schedule_task(task).await.map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn claim_task(
    &self,
    request: Request<pb::ClaimTaskRequest>,
  ) -> Result<Response<pb::ClaimTaskResponse>, Status> {
    let request = request.into_inner();
    let worker_id = WorkerId::new(request.worker_id.clone())
      .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let hints = claim_hints_from_proto(&request).map_err(|e| *e)?;
    let task = self
      .claim_task_with_hints(worker_id, &hints)
      .await
      .map_err(status_from_error)?
      .map(worker_task_to_proto);
    Ok(Response::new(pb::ClaimTaskResponse { task }))
  }

  async fn report_result(
    &self,
    request: Request<pb::ReportResultRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let request = request.into_inner();
    let worker_id =
      WorkerId::new(request.worker_id).map_err(|err| Status::invalid_argument(err.to_string()))?;
    let task_id = parse_uuid(&request.task_id, "task_id").map_err(|e| *e)?;
    let result = request
      .result
      .ok_or_else(|| Box::new(Status::invalid_argument("result is required")))
      .and_then(worker_task_result_from_proto)
      .map_err(|e| *e)?;
    self
      .report_result(worker_id, task_id, result)
      .await
      .map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }

  async fn heartbeat(
    &self,
    request: Request<pb::HeartbeatRequest>,
  ) -> Result<Response<pb::Empty>, Status> {
    let heartbeat = worker_heartbeat_from_proto(request.into_inner()).map_err(|e| *e)?;
    self.heartbeat(heartbeat).await.map_err(status_from_error)?;
    Ok(Response::new(pb::Empty {}))
  }
}

/// Tonic server wrapper for [`WorkerControl`].
#[derive(Debug, Clone)]
pub struct WorkerControlServer<T> {
  inner: Arc<T>,
}

impl<T> WorkerControlServer<T> {
  pub fn new(inner: T) -> Self {
    Self {
      inner: Arc::new(inner),
    }
  }
}

impl<T, B> Service<http::Request<B>> for WorkerControlServer<T>
where
  T: WorkerControl,
  B: tonic::codegen::Body + Send + 'static,
  B::Error: Into<StdError> + Send + 'static,
{
  type Response = http::Response<BoxBody>;
  type Error = std::convert::Infallible;
  type Future = BoxFuture<Self::Response, Self::Error>;

  fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
  }

  fn call(&mut self, req: http::Request<B>) -> Self::Future {
    let inner = self.inner.clone();
    match req.uri().path() {
      "/yanshi.scheduler.v1.WorkerControl/SubmitTask" => {
        struct SubmitTaskSvc<T>(Arc<T>);
        impl<T> tonic::server::UnaryService<pb::SubmitTaskRequest> for SubmitTaskSvc<T>
        where
          T: WorkerControl,
        {
          type Response = pb::Empty;
          type Future = BoxFuture<Response<Self::Response>, Status>;

          fn call(&mut self, request: Request<pb::SubmitTaskRequest>) -> Self::Future {
            let inner = self.0.clone();
            // P3.8: install upstream traceparent for the duration of
            // the trait dispatch so any tracing/agent code the
            // handler triggers stitches onto the caller's span.
            let traceparent = extract_traceparent_from_grpc_request(&request);
            Box::pin(run_in_traceparent_scope(traceparent, async move {
              inner.submit_task(request).await
            }))
          }
        }
        Box::pin(async move {
          let codec = tonic::codec::ProstCodec::default();
          let mut grpc = tonic::server::Grpc::new(codec);
          Ok(grpc.unary(SubmitTaskSvc(inner), req).await)
        })
      }
      "/yanshi.scheduler.v1.WorkerControl/ClaimTask" => {
        struct ClaimTaskSvc<T>(Arc<T>);
        impl<T> tonic::server::UnaryService<pb::ClaimTaskRequest> for ClaimTaskSvc<T>
        where
          T: WorkerControl,
        {
          type Response = pb::ClaimTaskResponse;
          type Future = BoxFuture<Response<Self::Response>, Status>;

          fn call(&mut self, request: Request<pb::ClaimTaskRequest>) -> Self::Future {
            let inner = self.0.clone();
            let traceparent = extract_traceparent_from_grpc_request(&request);
            Box::pin(run_in_traceparent_scope(traceparent, async move {
              inner.claim_task(request).await
            }))
          }
        }
        Box::pin(async move {
          let codec = tonic::codec::ProstCodec::default();
          let mut grpc = tonic::server::Grpc::new(codec);
          Ok(grpc.unary(ClaimTaskSvc(inner), req).await)
        })
      }
      "/yanshi.scheduler.v1.WorkerControl/ReportResult" => {
        struct ReportResultSvc<T>(Arc<T>);
        impl<T> tonic::server::UnaryService<pb::ReportResultRequest> for ReportResultSvc<T>
        where
          T: WorkerControl,
        {
          type Response = pb::Empty;
          type Future = BoxFuture<Response<Self::Response>, Status>;

          fn call(&mut self, request: Request<pb::ReportResultRequest>) -> Self::Future {
            let inner = self.0.clone();
            let traceparent = extract_traceparent_from_grpc_request(&request);
            Box::pin(run_in_traceparent_scope(traceparent, async move {
              inner.report_result(request).await
            }))
          }
        }
        Box::pin(async move {
          let codec = tonic::codec::ProstCodec::default();
          let mut grpc = tonic::server::Grpc::new(codec);
          Ok(grpc.unary(ReportResultSvc(inner), req).await)
        })
      }
      "/yanshi.scheduler.v1.WorkerControl/Heartbeat" => {
        struct HeartbeatSvc<T>(Arc<T>);
        impl<T> tonic::server::UnaryService<pb::HeartbeatRequest> for HeartbeatSvc<T>
        where
          T: WorkerControl,
        {
          type Response = pb::Empty;
          type Future = BoxFuture<Response<Self::Response>, Status>;

          fn call(&mut self, request: Request<pb::HeartbeatRequest>) -> Self::Future {
            let inner = self.0.clone();
            let traceparent = extract_traceparent_from_grpc_request(&request);
            Box::pin(run_in_traceparent_scope(traceparent, async move {
              inner.heartbeat(request).await
            }))
          }
        }
        Box::pin(async move {
          let codec = tonic::codec::ProstCodec::default();
          let mut grpc = tonic::server::Grpc::new(codec);
          Ok(grpc.unary(HeartbeatSvc(inner), req).await)
        })
      }
      _ => Box::pin(async move {
        Ok(
          http::Response::builder()
            .status(200)
            .header("grpc-status", "12")
            .header("content-type", "application/grpc")
            .body(empty_body())
            .unwrap_or_else(|_| http::Response::new(empty_body())),
        )
      }),
    }
  }
}

impl<T> tonic::server::NamedService for WorkerControlServer<T> {
  const NAME: &'static str = "yanshi.scheduler.v1.WorkerControl";
}

#[cfg(test)]
mod traceparent_tests {
  //! Unit coverage for P3.8 worker gRPC traceparent propagation.
  //!
  //! Inject and extract are pure metadata-manipulation helpers, so
  //! the contract can be verified without spinning up a real
  //! channel. End-to-end coverage (real tonic client + server with
  //! cross-process scope propagation) lives in the integration test
  //! suite under `yanshi-worker/tests/` so it can race a real
  //! channel against admission + heartbeat flows that already exist
  //! there.
  use super::*;
  use tonic::Request;

  #[test]
  fn inject_outside_any_scope_leaves_metadata_empty() {
    let mut req = Request::new(pb::SubmitTaskRequest { task: None });
    inject_traceparent_into_grpc_request(&mut req);
    assert!(
      req.metadata().get("traceparent").is_none(),
      "no scope ⇒ no traceparent key; consumers rely on absence"
    );
  }

  #[tokio::test]
  async fn inject_inside_scope_writes_traceparent_metadata() {
    let mut req = Request::new(pb::ClaimTaskRequest {
      worker_id: "worker-1".into(),
      accepted_node_types: Vec::new(),
      locality_run_id: String::new(),
    });
    yanshi_tracing::context::scope("00-trace-id-span-01".to_string(), async {
      inject_traceparent_into_grpc_request(&mut req);
    })
    .await;
    let value = req
      .metadata()
      .get("traceparent")
      .expect("traceparent metadata populated inside scope");
    assert_eq!(value.to_str().unwrap(), "00-trace-id-span-01");
  }

  #[test]
  fn extract_returns_none_for_missing_traceparent_metadata() {
    let req = Request::new(pb::SubmitTaskRequest { task: None });
    assert!(extract_traceparent_from_grpc_request(&req).is_none());
  }

  #[test]
  fn extract_returns_traceparent_when_metadata_populated() {
    let mut req = Request::new(pb::SubmitTaskRequest { task: None });
    req
      .metadata_mut()
      .insert("traceparent", "00-deadbeef-cafebabe-01".parse().unwrap());
    assert_eq!(
      extract_traceparent_from_grpc_request(&req).as_deref(),
      Some("00-deadbeef-cafebabe-01")
    );
  }

  #[tokio::test]
  async fn run_in_traceparent_scope_with_some_installs_context_for_future() {
    let observed = run_in_traceparent_scope(Some("00-scoped-test-01".to_string()), async {
      yanshi_tracing::context::current_traceparent()
    })
    .await;
    assert_eq!(observed.as_deref(), Some("00-scoped-test-01"));
  }

  #[tokio::test]
  async fn run_in_traceparent_scope_with_none_runs_future_outside_any_scope() {
    let observed = run_in_traceparent_scope(None, async {
      yanshi_tracing::context::current_traceparent()
    })
    .await;
    assert!(observed.is_none());
  }

  #[tokio::test]
  async fn round_trip_inject_then_extract_round_trips_value_under_active_scope() {
    let mut req = Request::new(pb::HeartbeatRequest::default());
    yanshi_tracing::context::scope("00-roundtrip-test-01".to_string(), async {
      inject_traceparent_into_grpc_request(&mut req);
    })
    .await;
    assert_eq!(
      extract_traceparent_from_grpc_request(&req).as_deref(),
      Some("00-roundtrip-test-01")
    );
  }
}

#[cfg(test)]
mod hint_proto_tests {
  //! P10.16.2-FU1: wire-shape conversions for capability +
  //! locality hints. The transport-layer policy-routing test
  //! (capability filter actually filtering, locality preference
  //! actually winning) lives in `scheduler::mod.rs` against
  //! `InMemoryWorkerProtocol`; here we only assert the bytes
  //! round-trip cleanly so the trait method sees the same data
  //! the worker sent.
  use super::*;
  use crate::scheduler::{ClaimHints, WorkerCapabilities, WorkerHeartbeat, WorkerTask};
  use uuid::Uuid;

  #[test]
  fn worker_task_round_trip_preserves_node_type() {
    let task = WorkerTask::new(Uuid::new_v4(), "node-a", serde_json::json!({"k": "v"}))
      .with_node_type("template");
    let wire = worker_task_to_proto(task.clone());
    assert_eq!(wire.node_type, "template");
    let back = worker_task_from_proto(wire).expect("decode");
    assert_eq!(back.node_type.as_deref(), Some("template"));
    assert_eq!(back.task_id, task.task_id);
  }

  #[test]
  fn worker_task_round_trip_preserves_untagged() {
    // The pre-FU1 wire path: a worker that doesn't set
    // `node_type` ends up with `None`, not `Some("")` —
    // important because `WorkerCapabilities::accepts(None)`
    // always returns true (the untagged-task-always-accepted
    // invariant) while `accepts(Some(""))` would unhelpfully
    // never match.
    let task = WorkerTask::new(Uuid::new_v4(), "node-a", serde_json::json!({}));
    let wire = worker_task_to_proto(task);
    assert_eq!(wire.node_type, "", "untagged tasks encode as empty string");
    let back = worker_task_from_proto(wire).expect("decode");
    assert!(
      back.node_type.is_none(),
      "empty string decodes back to None"
    );
  }

  #[test]
  fn claim_hints_round_trip_carries_capabilities_and_locality() {
    let run = Uuid::new_v4();
    let hints = ClaimHints::default()
      .with_capabilities(WorkerCapabilities::for_node_types(["template", "file"]))
      .with_locality(run);
    let wire = pb::ClaimTaskRequest {
      worker_id: "w".into(),
      accepted_node_types: hints.capabilities.node_types.clone(),
      locality_run_id: hints
        .locality_run_id
        .map(|id| id.to_string())
        .unwrap_or_default(),
    };
    let decoded = claim_hints_from_proto(&wire).expect("decode");
    assert_eq!(
      decoded.capabilities.node_types,
      vec!["template".to_string(), "file".to_string()]
    );
    assert_eq!(decoded.locality_run_id, Some(run));
  }

  #[test]
  fn claim_hints_from_proto_default_means_no_hints() {
    // Pre-FU1 worker sends a bare `ClaimTaskRequest { worker_id }`
    // with default values everywhere else. The decoder must
    // produce a `ClaimHints` that's behaviorally identical to
    // `ClaimHints::none()`.
    let wire = pb::ClaimTaskRequest {
      worker_id: "legacy-worker".into(),
      accepted_node_types: Vec::new(),
      locality_run_id: String::new(),
    };
    let decoded = claim_hints_from_proto(&wire).expect("decode");
    assert!(decoded.capabilities.node_types.is_empty());
    assert!(decoded.locality_run_id.is_none());
    // The capability check on the decoded hint accepts any
    // task — same as `ClaimHints::none()` semantics.
    assert!(decoded.capabilities.accepts(Some("anything")));
    assert!(decoded.capabilities.accepts(None));
  }

  #[test]
  fn claim_hints_from_proto_rejects_malformed_locality_uuid() {
    let wire = pb::ClaimTaskRequest {
      worker_id: "w".into(),
      accepted_node_types: Vec::new(),
      locality_run_id: "not-a-uuid".into(),
    };
    let err = claim_hints_from_proto(&wire).expect_err("malformed uuid rejected");
    let status = *err;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("locality_run_id"));
  }

  #[test]
  fn heartbeat_round_trip_preserves_capabilities() {
    let hb = WorkerHeartbeat::now(WorkerId::new("w").unwrap(), None, 4)
      .with_capabilities(WorkerCapabilities::for_node_types(["template"]));
    let wire = worker_heartbeat_to_proto(hb);
    assert_eq!(
      wire.accepted_node_types,
      vec!["template".to_string()],
      "capability advertisement encodes to repeated string"
    );
    let back = worker_heartbeat_from_proto(wire).expect("decode");
    assert_eq!(
      back.capabilities.node_types,
      vec!["template".to_string()],
      "round-trip preserves the capability set"
    );
  }

  #[test]
  fn heartbeat_pre_fu1_default_decodes_as_any_capability() {
    // A heartbeat from a pre-FU1 worker has an empty
    // `accepted_node_types`. The decoder produces a default
    // `WorkerCapabilities` which accepts everything — preserves
    // the pre-FU1 admission semantics.
    let wire = pb::HeartbeatRequest {
      worker_id: "legacy".into(),
      active_task_id: String::new(),
      free_slots: 0,
      timestamp_rfc3339: String::new(),
      accepted_node_types: Vec::new(),
    };
    let back = worker_heartbeat_from_proto(wire).expect("decode");
    assert!(back.capabilities.node_types.is_empty());
    assert!(back.capabilities.accepts(Some("any")));
  }
}

#[cfg(test)]
mod proto_schema_drift_tests {
  //! Q3.3.3: pin `worker.proto` against the prost types so the
  //! audit-found drift (proto missing `node_type` /
  //! `accepted_node_types` / `locality_run_id`) cannot silently
  //! re-occur. Two layers of protection:
  //!
  //! 1. Build-time: `build.rs` generates `pb::*` from the .proto via
  //!    `tonic-build`. A field removed from the .proto fails to
  //!    compile because the rest of `grpc.rs` references it.
  //! 2. Test-time (this module): textually pin the .proto file to
  //!    the expected field/tag triples. Catches the inverse drift —
  //!    someone editing prost-side code by hand (after a future
  //!    refactor away from include_proto!) and forgetting the .proto.
  use super::*;
  use prost::Message as _;

  const WORKER_PROTO: &str =
    include_str!("../../../yanshi-worker-proto/proto/yanshi/scheduler/v1/worker.proto");

  /// Each tuple is `(field_name, tag = "N")`. Order doesn't matter
  /// but every entry must appear as a `... = N;` line in
  /// `worker.proto` for the wire shape to match the prost structs
  /// the rest of the file relies on.
  fn assert_proto_has_field(field: &str, tag: u32) {
    let needle = format!("{field} = {tag};");
    assert!(
      WORKER_PROTO.contains(&needle),
      "worker.proto missing field declaration `{needle}` — \
       drift between .proto and prost::pb detected (Q3.3.3)"
    );
  }

  #[test]
  fn worker_task_fields_pinned_to_proto_tags() {
    // Pre-FU1 fields.
    assert_proto_has_field("string task_id", 1);
    assert_proto_has_field("string run_id", 2);
    assert_proto_has_field("string node_id", 3);
    assert_proto_has_field("uint32 attempt", 4);
    assert_proto_has_field("string payload_json", 5);
    // P10.16.2-FU1 — these are the three the audit found missing
    // from the .proto before Q3.3.3.
    assert_proto_has_field("string node_type", 6);
  }

  #[test]
  fn claim_task_request_carries_capability_and_locality_fields() {
    assert_proto_has_field("string worker_id", 1);
    assert_proto_has_field("repeated string accepted_node_types", 2);
    assert_proto_has_field("string locality_run_id", 3);
  }

  #[test]
  fn heartbeat_request_carries_capability_advertisement() {
    assert_proto_has_field("string worker_id", 1);
    assert_proto_has_field("string active_task_id", 2);
    assert_proto_has_field("uint32 free_slots", 3);
    assert_proto_has_field("string timestamp_rfc3339", 4);
    assert_proto_has_field("repeated string accepted_node_types", 5);
  }

  /// The .proto declares `repeated WorkerTraceEvent events = 5` on
  /// `WorkerTaskResult`. The audit didn't flag this one but it's
  /// load-bearing — `report_result` round-trips event lists across
  /// the wire, so renumbering the field would silently drop events
  /// in mixed-deployment scenarios.
  #[test]
  fn worker_task_result_pins_status_and_events_tags() {
    assert_proto_has_field("Status status", 1);
    assert_proto_has_field("string output_json", 2);
    assert_proto_has_field("string error", 3);
    assert_proto_has_field("bool retryable", 4);
    assert_proto_has_field("repeated WorkerTraceEvent events", 5);
  }

  /// Proto3 + prost interop guarantee: encoding a message with the
  /// new fields populated and decoding into the same struct
  /// round-trips bit-for-bit. The build path uses
  /// `tonic::include_proto!`, so a successful build already implies
  /// the prost derive matches; we still re-prove the contract here
  /// against a representative non-default payload so a future change
  /// (e.g. swapping `string` ↔ `bytes`) would break a focused test
  /// before it broke wire compatibility.
  #[test]
  fn claim_task_request_with_new_fields_round_trips() {
    let original = pb::ClaimTaskRequest {
      worker_id: "worker-q333".into(),
      accepted_node_types: vec!["template".into(), "file".into()],
      locality_run_id: "11111111-1111-1111-1111-111111111111".into(),
    };
    let bytes = original.encode_to_vec();
    let decoded = pb::ClaimTaskRequest::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.worker_id, "worker-q333");
    assert_eq!(decoded.accepted_node_types, vec!["template", "file"]);
    assert_eq!(
      decoded.locality_run_id,
      "11111111-1111-1111-1111-111111111111"
    );
  }

  #[test]
  fn heartbeat_request_with_capabilities_round_trips() {
    let original = pb::HeartbeatRequest {
      worker_id: "worker-q333".into(),
      active_task_id: String::new(),
      free_slots: 4,
      timestamp_rfc3339: "2025-01-01T00:00:00Z".into(),
      accepted_node_types: vec!["llm".into()],
    };
    let bytes = original.encode_to_vec();
    let decoded = pb::HeartbeatRequest::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.free_slots, 4);
    assert_eq!(decoded.accepted_node_types, vec!["llm"]);
  }

  #[test]
  fn worker_task_with_node_type_round_trips() {
    let original = pb::WorkerTask {
      task_id: "00000000-0000-0000-0000-000000000001".into(),
      run_id: "00000000-0000-0000-0000-000000000002".into(),
      node_id: "node-x".into(),
      attempt: 2,
      payload_json: "{\"k\":1}".into(),
      node_type: "template".into(),
    };
    let bytes = original.encode_to_vec();
    let decoded = pb::WorkerTask::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.node_type, "template");
    assert_eq!(decoded.attempt, 2);
  }

  /// Pre-FU1 wire bytes (empty `node_type` / `accepted_node_types` /
  /// `locality_run_id`) must decode to the same default values
  /// `ClaimHints::none()` represents. Proto3 default semantics make
  /// these the zero value, but pinning the assumption with a
  /// regression test keeps a future codec swap honest.
  #[test]
  fn pre_fu1_empty_payload_decodes_to_defaults() {
    let empty_claim: Vec<u8> = pb::ClaimTaskRequest {
      worker_id: "legacy".into(),
      accepted_node_types: Vec::new(),
      locality_run_id: String::new(),
    }
    .encode_to_vec();
    let decoded = pb::ClaimTaskRequest::decode(empty_claim.as_slice()).expect("decode");
    assert_eq!(decoded.worker_id, "legacy");
    assert!(decoded.accepted_node_types.is_empty());
    assert!(decoded.locality_run_id.is_empty());
  }
}

#[cfg(test)]
mod grpc_concurrency_tests {
  //! Q3.3.1: prove that `GrpcWorkerProtocol` actually fires
  //! concurrent unary RPCs over the shared HTTP/2 channel.
  //! Pre-fix the inner `Mutex<Grpc<Channel>>` serialized every
  //! call; the test would see peak in-flight = 1. Post-fix the
  //! channel is cloned per call and tonic muxes the requests
  //! onto the same H/2 connection, so we observe peak ≥ 2.
  use super::*;
  use crate::scheduler::{WorkerHeartbeat, WorkerId, WorkerProtocol};
  use std::sync::atomic::{AtomicU32, Ordering};
  use std::time::Duration;
  use tokio::net::TcpListener;
  use tokio::sync::oneshot;
  use tonic::transport::Server;

  /// Counters held in Arc so the test can read them after handing
  /// `SlowConcurrencyServer` to `WorkerControlServer::new` (which
  /// consumes by value).
  #[derive(Clone, Default)]
  struct ConcurrencyCounters {
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    total: Arc<AtomicU32>,
  }

  #[derive(Clone)]
  struct SlowConcurrencyServer {
    delay: Duration,
    counters: ConcurrencyCounters,
  }

  #[async_trait]
  impl WorkerControl for SlowConcurrencyServer {
    async fn submit_task(
      &self,
      _request: Request<pb::SubmitTaskRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
      // Not exercised by this test; reply Ok so the server type
      // is well-formed even if a stray RPC arrives.
      Ok(Response::new(pb::Empty {}))
    }
    async fn claim_task(
      &self,
      _request: Request<pb::ClaimTaskRequest>,
    ) -> Result<Response<pb::ClaimTaskResponse>, Status> {
      Ok(Response::new(pb::ClaimTaskResponse { task: None }))
    }
    async fn report_result(
      &self,
      _request: Request<pb::ReportResultRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
      Ok(Response::new(pb::Empty {}))
    }
    async fn heartbeat(
      &self,
      _request: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
      let now = self.counters.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
      self.counters.peak.fetch_max(now, Ordering::SeqCst);
      tokio::time::sleep(self.delay).await;
      self.counters.in_flight.fetch_sub(1, Ordering::SeqCst);
      self.counters.total.fetch_add(1, Ordering::SeqCst);
      Ok(Response::new(pb::Empty {}))
    }
  }

  async fn unused_local_addr() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    addr
  }

  async fn connect_with_retry(endpoint: &str) -> GrpcWorkerProtocol {
    for _ in 0..20 {
      if let Ok(protocol) = GrpcWorkerProtocol::connect(endpoint).await {
        return protocol;
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("connect retry exhausted");
  }

  /// Q3.3.1 regression — one `GrpcWorkerProtocol` cloned across 8
  /// async tasks must fire heartbeats concurrently. Pre-fix this
  /// pinned at peak in-flight = 1 because the inner Mutex
  /// serialized every call; post-fix the H/2 channel mux drives
  /// peak above 1 and 8 × 200ms RPCs finish well under the 1.6s
  /// serial bound.
  #[tokio::test]
  async fn grpc_protocol_fires_concurrent_heartbeats() {
    let counters = ConcurrencyCounters::default();
    let server = SlowConcurrencyServer {
      delay: Duration::from_millis(200),
      counters: counters.clone(),
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let addr = unused_local_addr().await;
    let server_task = tokio::spawn(async move {
      Server::builder()
        .add_service(WorkerControlServer::new(server))
        .serve_with_shutdown(addr, async {
          let _ = shutdown_rx.await;
        })
        .await
    });

    let endpoint = format!("http://{addr}");
    let protocol = connect_with_retry(&endpoint).await;
    let worker_id = WorkerId::new("worker-q331").unwrap();

    let started = std::time::Instant::now();
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
      let p = protocol.clone();
      let wid = worker_id.clone();
      handles.push(tokio::spawn(async move {
        p.heartbeat(WorkerHeartbeat::now(wid, None, 0)).await
      }));
    }
    for h in handles {
      h.await.expect("join").expect("heartbeat ok");
    }
    let elapsed = started.elapsed();

    let peak = counters.peak.load(Ordering::SeqCst);
    let total = counters.total.load(Ordering::SeqCst);
    assert_eq!(total, 8, "server must have received every heartbeat");
    assert!(
      peak >= 2,
      "Q3.3.1: peak in-flight must exceed 1 (concurrent RPCs); got {peak}"
    );
    // Serial wall clock bound: 8 × 200ms = 1.6s. Allow generous
    // headroom for handshake + scheduling; assert well below the
    // serial bound to leave no doubt the parallelism is real.
    assert!(
      elapsed < Duration::from_millis(1200),
      "Q3.3.1: 8 concurrent 200ms heartbeats must finish < 1.2s (serial would need 1.6s); took {elapsed:?}"
    );

    let _ = shutdown_tx.send(());
    server_task.await.unwrap().unwrap();
  }
}
