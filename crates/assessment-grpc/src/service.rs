//! The tonic implementation of `assessment.v1.AssessmentEngine`.
//!
//! [`AssessmentService`] serves the stateless calls (`Assess`, `AssessBatch`,
//! `AssessStream`, `CompareText`, `GetPolicy`) with a base [`Engine`] and the
//! stateful ones (`PutEntity` … `Acknowledge`) with a shared [`Registry`].
//! Every assessment is CPU-bound and runs on tokio's blocking pool
//! ([`tokio::task::spawn_blocking`]), never on the async workers.

use std::pin::Pin;
use std::sync::Arc;

use assessment_core::engine::{MAX_EMBEDDING_DIM, MAX_TEXT_BYTES};
use assessment_core::{Engine, Error as CoreError, ErrorKind, Registry};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::convert::{self, AssessInput, ConvertError};
use crate::pb;
use crate::pb::assessment_engine_server::AssessmentEngine;

/// Limits on how much work one call may ask for.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceLimits {
    /// Most requests one `AssessBatch` call may carry (larger batches are
    /// rejected with `INVALID_ARGUMENT`). Default 1 000.
    pub max_batch_size: usize,
    /// Most assessments one `AssessBatch` or `AssessStream` call runs at the
    /// same time (results are still returned in request order). On a stream
    /// this also bounds the responses buffered for a slow reader. Default:
    /// the number of CPUs, at least 2.
    pub max_in_flight: usize,
}

impl Default for ServiceLimits {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Self {
            max_batch_size: 1_000,
            max_in_flight: cpus.max(2),
        }
    }
}

impl ServiceLimits {
    /// These limits with `max_batch_size` (at least 1).
    #[must_use]
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = max_batch_size.max(1);
        self
    }

    /// These limits with `max_in_flight` (at least 1).
    #[must_use]
    pub fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight.max(1);
        self
    }
}

/// The gRPC service: a base engine for stateless calls and a registry for
/// stateful ones. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AssessmentService {
    engine: Arc<Engine>,
    registry: Arc<Registry>,
    limits: ServiceLimits,
}

impl AssessmentService {
    /// A service over `engine` (stateless calls; per-request policy overrides
    /// are merged over its policy) and `registry` (stateful calls).
    pub fn new(engine: Engine, registry: Arc<Registry>) -> Self {
        Self {
            engine: Arc::new(engine),
            registry,
            limits: ServiceLimits::default(),
        }
    }

    /// A service whose stateless calls use the registry's own engine.
    pub fn from_registry(registry: Arc<Registry>) -> Self {
        Self::new(registry.engine().clone(), registry)
    }

    /// An in-memory service with the default policy.
    pub fn in_memory() -> Self {
        Self::from_registry(Arc::new(Registry::new(Engine::default())))
    }

    /// This service with other limits.
    #[must_use]
    pub fn with_limits(mut self, limits: ServiceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The limits in force.
    pub fn limits(&self) -> ServiceLimits {
        self.limits
    }

    /// The base engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The gRPC status for a core error.
pub fn status_from_core(err: &CoreError) -> Status {
    match err.kind() {
        ErrorKind::InvalidRequest | ErrorKind::InvalidPolicy => {
            Status::invalid_argument(err.message())
        }
        ErrorKind::NotFound => Status::not_found(err.message()),
        ErrorKind::Conflict => Status::aborted(err.message()),
        // Storage, and any kind added later.
        _ => {
            log_internal(err);
            Status::internal(err.message())
        }
    }
}

fn log_internal(err: &CoreError) {
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    tracing::error!(kind = %err.kind(), error = %err, ?causes, "assessment engine failure");
}

/// Why one assessment failed.
enum Failure {
    Convert(ConvertError),
    Core(CoreError),
    Internal(String),
}

impl Failure {
    fn into_status(self) -> Status {
        match self {
            Failure::Convert(e) => e.into(),
            Failure::Core(e) => status_from_core(&e),
            Failure::Internal(message) => Status::internal(message),
        }
    }

    /// The per-item error of a batch or a stream (`kind` as in the proto).
    fn into_pb(self) -> pb::AssessError {
        let (kind, message) = match self {
            Failure::Convert(e) => ("invalid_request".to_owned(), e.to_string()),
            Failure::Core(e) => {
                let kind = core_kind_name(e.kind());
                if matches!(kind, "storage" | "internal") {
                    log_internal(&e);
                }
                (kind.to_owned(), e.message().to_owned())
            }
            Failure::Internal(message) => ("internal".to_owned(), message),
        };
        pb::AssessError { kind, message }
    }
}

/// The proto's `AssessError.kind` for a core error kind (kinds added to the
/// core later report as `internal`).
fn core_kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest
        | ErrorKind::NotFound
        | ErrorKind::Conflict
        | ErrorKind::InvalidPolicy
        | ErrorKind::Storage => kind.as_str(),
        _ => "internal",
    }
}

fn join_failure(err: &tokio::task::JoinError) -> Failure {
    tracing::error!(error = %err, "an assessment task failed");
    Failure::Internal("the assessment task failed".to_owned())
}

/// Runs CPU-bound work on the blocking pool.
async fn blocking<T, F>(f: F) -> Result<T, Status>
where
    F: FnOnce() -> Result<T, Status> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) => Err(join_failure(&err).into_status()),
    }
}

// ---------------------------------------------------------------------------
// Assess
// ---------------------------------------------------------------------------

/// The engine for one request: the base one, or it with `overrides` merged.
fn engine_for(
    base: &Arc<Engine>,
    overrides: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<Arc<Engine>, CoreError> {
    match overrides {
        Some(patch) if !patch.is_empty() => base
            .with_policy_patch(&serde_json::Value::Object(patch))
            .map(Arc::new),
        _ => Ok(Arc::clone(base)),
    }
}

/// Decodes, assesses and encodes one request (blocking).
fn assess_one(engine: &Arc<Engine>, input: AssessInput) -> Result<pb::Assessment, Failure> {
    let engine = engine_for(engine, input.policy_overrides).map_err(Failure::Core)?;
    let assessment = engine.assess(&input.request).map_err(Failure::Core)?;
    Ok(assessment.into())
}

/// One request of a batch or a stream: never fails, errors go in the response.
fn assess_item(engine: &Arc<Engine>, request: pb::AssessRequest) -> pb::AssessResponse {
    let request_id = request.request_id.clone();
    let result = AssessInput::try_from(request)
        .map_err(Failure::Convert)
        .and_then(|input| assess_one(engine, input));
    item_response(request_id, result)
}

fn item_response(
    request_id: String,
    result: Result<pb::Assessment, Failure>,
) -> pb::AssessResponse {
    match result {
        Ok(assessment) => pb::AssessResponse {
            request_id,
            assessment: Some(assessment),
            error: None,
        },
        Err(failure) => pb::AssessResponse {
            request_id,
            assessment: None,
            error: Some(failure.into_pb()),
        },
    }
}

/// Starts one item on the blocking pool.
fn spawn_item(
    engine: &Arc<Engine>,
    request: pb::AssessRequest,
) -> (String, tokio::task::JoinHandle<pb::AssessResponse>) {
    let request_id = request.request_id.clone();
    let engine = Arc::clone(engine);
    let handle = tokio::task::spawn_blocking(move || assess_item(&engine, request));
    (request_id, handle)
}

async fn finish_item(
    request_id: String,
    handle: tokio::task::JoinHandle<pb::AssessResponse>,
) -> pb::AssessResponse {
    match handle.await {
        Ok(response) => response,
        Err(err) => item_response(request_id, Err(join_failure(&err))),
    }
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<pb::AssessResponse, Status>> + Send>>;

/// What the stream reader hands the writer, in request order.
enum Pending {
    Item {
        request_id: String,
        handle: tokio::task::JoinHandle<pb::AssessResponse>,
        permit: tokio::sync::OwnedSemaphorePermit,
    },
    Failed(Status),
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

fn into_core_result<T>(result: Result<T, CoreError>) -> Result<T, Status> {
    result.map_err(|e| status_from_core(&e))
}

fn non_empty(id: String) -> Option<String> {
    (!id.trim().is_empty()).then_some(id)
}

fn check_text(name: &'static str, text: &str) -> Result<(), Status> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(Status::invalid_argument(format!(
            "{name}: is longer than {MAX_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_embedding(name: &'static str, embedding: &[f32]) -> Result<(), Status> {
    if embedding.len() > MAX_EMBEDDING_DIM {
        return Err(Status::invalid_argument(format!(
            "{name}: has more than {MAX_EMBEDDING_DIM} dimensions"
        )));
    }
    if embedding.iter().any(|x| !x.is_finite()) {
        return Err(Status::invalid_argument(format!("{name}: is not finite")));
    }
    Ok(())
}

#[tonic::async_trait]
impl AssessmentEngine for AssessmentService {
    async fn assess(
        &self,
        request: Request<pb::AssessRequest>,
    ) -> Result<Response<pb::AssessResponse>, Status> {
        let request = request.into_inner();
        let request_id = request.request_id.clone();
        let engine = Arc::clone(&self.engine);
        let assessment = blocking(move || {
            AssessInput::try_from(request)
                .map_err(Failure::Convert)
                .and_then(|input| assess_one(&engine, input))
                .map_err(Failure::into_status)
        })
        .await?;
        Ok(Response::new(pb::AssessResponse {
            request_id,
            assessment: Some(assessment),
            error: None,
        }))
    }

    async fn assess_batch(
        &self,
        request: Request<pb::AssessBatchRequest>,
    ) -> Result<Response<pb::AssessBatchResponse>, Status> {
        let requests = request.into_inner().requests;
        if requests.len() > self.limits.max_batch_size {
            return Err(Status::invalid_argument(format!(
                "requests: the batch has {} requests, more than the {} allowed",
                requests.len(),
                self.limits.max_batch_size
            )));
        }
        let permits = Arc::new(Semaphore::new(self.limits.max_in_flight));
        let mut running = Vec::with_capacity(requests.len());
        for request in requests {
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .map_err(|_| Status::internal("the batch was cancelled"))?;
            let request_id = request.request_id.clone();
            let engine = Arc::clone(&self.engine);
            let handle = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                assess_item(&engine, request)
            });
            running.push((request_id, handle));
        }
        let mut responses = Vec::with_capacity(running.len());
        for (request_id, handle) in running {
            responses.push(finish_item(request_id, handle).await);
        }
        Ok(Response::new(pb::AssessBatchResponse { responses }))
    }

    type AssessStreamStream = ResponseStream;

    async fn assess_stream(
        &self,
        request: Request<Streaming<pb::AssessRequest>>,
    ) -> Result<Response<Self::AssessStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let in_flight = self.limits.max_in_flight;
        let permits = Arc::new(Semaphore::new(in_flight));
        let (pending_tx, mut pending_rx) = mpsc::channel::<Pending>(in_flight);
        let (out_tx, out_rx) = mpsc::channel::<Result<pb::AssessResponse, Status>>(in_flight);
        let engine = Arc::clone(&self.engine);

        // Reader: starts each request as it arrives, at most `in_flight` at once.
        tokio::spawn(async move {
            loop {
                let next = inbound.message().await;
                let pending = match next {
                    Ok(Some(request)) => {
                        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                            break;
                        };
                        let (request_id, handle) = spawn_item(&engine, request);
                        Pending::Item {
                            request_id,
                            handle,
                            permit,
                        }
                    }
                    Ok(None) => break,
                    Err(status) => Pending::Failed(status),
                };
                let failed = matches!(pending, Pending::Failed(_));
                if pending_tx.send(pending).await.is_err() || failed {
                    break;
                }
            }
        });

        // Writer: forwards results in request order. A permit is released
        // once its response is handed to the transport.
        tokio::spawn(async move {
            while let Some(pending) = pending_rx.recv().await {
                let (item, permit) = match pending {
                    Pending::Item {
                        request_id,
                        handle,
                        permit,
                    } => (Ok(finish_item(request_id, handle).await), Some(permit)),
                    Pending::Failed(status) => (Err(status), None),
                };
                let sent = out_tx.send(item).await;
                drop(permit);
                if sent.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn compare_text(
        &self,
        request: Request<pb::CompareTextRequest>,
    ) -> Result<Response<pb::CompareTextResponse>, Status> {
        let r = request.into_inner();
        check_text("before", &r.before)?;
        check_text("after", &r.after)?;
        check_embedding("before_embedding", &r.before_embedding)?;
        check_embedding("after_embedding", &r.after_embedding)?;
        let engine = Arc::clone(&self.engine);
        let diff = blocking(move || {
            let embeddings = (!r.before_embedding.is_empty() && !r.after_embedding.is_empty())
                .then_some((r.before_embedding.as_slice(), r.after_embedding.as_slice()));
            engine
                .compare_text(&r.before, &r.after, embeddings)
                .map_err(|e| status_from_core(&e))
        })
        .await?;
        Ok(Response::new(diff.into()))
    }

    async fn get_policy(
        &self,
        request: Request<pb::GetPolicyRequest>,
    ) -> Result<Response<pb::GetPolicyResponse>, Status> {
        let overrides = request
            .into_inner()
            .overrides
            .map(convert::struct_to_map)
            .transpose()
            .map_err(|e| Status::from(e.field("overrides")))?;
        let engine = Arc::clone(&self.engine);
        let response = blocking(move || {
            let engine = into_core_result(engine_for(&engine, overrides))?;
            let policy = engine.policy();
            let policy_json = serde_json::to_string(policy).map_err(|e| {
                Status::internal(format!("the policy could not be serialized: {e}"))
            })?;
            let policy =
                convert::policy_to_struct(policy).map_err(|e| Status::internal(e.to_string()))?;
            Ok(pb::GetPolicyResponse {
                policy: Some(policy),
                policy_json,
            })
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn put_entity(
        &self,
        request: Request<pb::PutEntityRequest>,
    ) -> Result<Response<pb::PutEntityResponse>, Status> {
        let (entity, options) = convert::put_request_from_pb(request.into_inner())?;
        let registry = Arc::clone(&self.registry);
        let outcome = blocking(move || into_core_result(registry.put(entity, options))).await?;
        Ok(Response::new(outcome.into()))
    }

    async fn delete_entity(
        &self,
        request: Request<pb::DeleteEntityRequest>,
    ) -> Result<Response<pb::DeleteEntityResponse>, Status> {
        let r = request.into_inner();
        let now = convert::decode_time(r.now, "now")?;
        let expected = r.expected_version.map(Into::into);
        let dry_run = r.dry_run;
        let registry = Arc::clone(&self.registry);
        let assessment =
            blocking(move || into_core_result(registry.delete(&r.id, dry_run, expected, now)))
                .await?;
        Ok(Response::new(pb::DeleteEntityResponse {
            assessment: Some(assessment.into()),
            deleted: !dry_run,
        }))
    }

    async fn record_activity(
        &self,
        request: Request<pb::RecordActivityRequest>,
    ) -> Result<Response<pb::RecordActivityResponse>, Status> {
        let r = request.into_inner();
        let events = convert::decode_list(r.events, "events", TryFrom::try_from)?;
        let now = convert::decode_time(r.now, "now")?;
        let registry = Arc::clone(&self.registry);
        let assessment =
            blocking(move || into_core_result(registry.record_activity(&r.entity_id, events, now)))
                .await?;
        Ok(Response::new(pb::RecordActivityResponse {
            assessment: Some(assessment.into()),
        }))
    }

    async fn get_entity(
        &self,
        request: Request<pb::GetEntityRequest>,
    ) -> Result<Response<pb::GetEntityResponse>, Status> {
        let r = request.into_inner();
        let version = r.version.map(Into::into);
        let registry = Arc::clone(&self.registry);
        let entity = blocking(move || into_core_result(registry.get(&r.id, version))).await?;
        Ok(Response::new(pb::GetEntityResponse {
            entity: Some(entity.into()),
        }))
    }

    async fn get_history(
        &self,
        request: Request<pb::GetHistoryRequest>,
    ) -> Result<Response<pb::GetHistoryResponse>, Status> {
        let r = request.into_inner();
        let limit = usize::try_from(r.limit).unwrap_or(usize::MAX);
        let registry = Arc::clone(&self.registry);
        let records = blocking(move || into_core_result(registry.history(&r.id, limit))).await?;
        Ok(Response::new(pb::GetHistoryResponse {
            records: records.into_iter().map(Into::into).collect(),
        }))
    }

    async fn compare_versions(
        &self,
        request: Request<pb::CompareVersionsRequest>,
    ) -> Result<Response<pb::CompareVersionsResponse>, Status> {
        let r = request.into_inner();
        let from = convert::decode_version(r.from, "from")?;
        let to = convert::decode_version(r.to, "to")?;
        let now = convert::decode_time(r.now, "now")?;
        let registry = Arc::clone(&self.registry);
        let assessment =
            blocking(move || into_core_result(registry.compare(&r.id, from, to, now))).await?;
        Ok(Response::new(pb::CompareVersionsResponse {
            assessment: Some(assessment.into()),
        }))
    }

    async fn list_stale(
        &self,
        request: Request<pb::ListStaleRequest>,
    ) -> Result<Response<pb::ListStaleResponse>, Status> {
        let root = non_empty(request.into_inner().root_id);
        let registry = Arc::clone(&self.registry);
        let entries = blocking(move || into_core_result(registry.stale(root.as_deref()))).await?;
        Ok(Response::new(pb::ListStaleResponse {
            entries: entries.into_iter().map(Into::into).collect(),
        }))
    }

    async fn acknowledge(
        &self,
        request: Request<pb::AcknowledgeRequest>,
    ) -> Result<Response<pb::AcknowledgeResponse>, Status> {
        let r = request.into_inner();
        let now = convert::decode_time(r.now, "now")?;
        let registry = Arc::clone(&self.registry);
        let entity = blocking(move || into_core_result(registry.acknowledge(&r.id, now))).await?;
        Ok(Response::new(pb::AcknowledgeResponse {
            entity: Some(entity.into()),
        }))
    }
}
