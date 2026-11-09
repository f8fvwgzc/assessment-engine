//! The tonic implementation of `assessment.v1.AssessmentEngine`.
//!
//! [`AssessmentService`] serves the stateless calls (`Assess`, `AssessBatch`,
//! `AssessStream`, `CompareText`, `GetPolicy`) with a base [`Engine`] and the
//! stateful ones (`PutEntity` … `Acknowledge`) with a shared [`Registry`].
//! Every assessment is CPU-bound and runs on tokio's blocking pool
//! ([`tokio::task::spawn_blocking`]), never on the async workers.
//!
//! Two slot pools of the same size cap the work
//! ([`ServiceLimits::max_concurrent_assessments`]). Stateless assessments
//! (`Assess`, `CompareText`, every batch and stream item) use one pool.
//! Registry calls that assess (`PutEntity`, `DeleteEntity`,
//! `RecordActivity`, `CompareVersions`) use the other. A registry call can
//! wait on the registry lock or on a journal sync while it holds its slot;
//! with its own pool, that wait never takes a slot from a stateless call.

use std::pin::Pin;
use std::sync::Arc;

use assessment_core::engine::{MAX_EMBEDDING_DIM, MAX_TEXT_BYTES};
use assessment_core::{Engine, Error as CoreError, ErrorKind, Registry};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
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
    /// Most items one `AssessBatch` or `AssessStream` call has started at
    /// the same time (results are still returned in request order). Each
    /// started item waits for its own assessment slot, so a call holds up
    /// to this many places in the slot queue. On a stream this also bounds
    /// the responses buffered for a slow reader. Default: the number of
    /// CPUs, at least 2. [`AssessmentService::with_limits`] keeps it within
    /// 1 and [`MAX_SLOTS`].
    pub max_in_flight: usize,
    /// The size of each of the service's two slot pools.
    ///
    /// * The stateless pool: unary `Assess` and `CompareText`, and every
    ///   `AssessBatch` and `AssessStream` item, across all calls.
    /// * The registry pool: `PutEntity`, `DeleteEntity`, `RecordActivity`
    ///   and `CompareVersions`. A registry call holds its slot while it
    ///   waits on the registry lock or a journal sync. That is why it has
    ///   its own pool: a slow journal cannot starve stateless calls.
    ///
    /// So at most this many stateless assessments and this many registry
    /// calls run at the same time. More wait their turn, in arrival order:
    ///
    /// * Unary and batch calls queue within the server's request timeout.
    ///   If it passes while they wait, they fail with `CANCELLED`.
    /// * Stream items queue with no server-side deadline. Only the end of
    ///   the stream (the client cancels or goes away) stops the wait.
    /// * Work that has started runs to completion and holds its slot until
    ///   then, even if the caller has gone. A started registry write still
    ///   commits.
    ///
    /// Default: the number of CPUs. [`AssessmentService::with_limits`] keeps
    /// it within 1 and [`MAX_SLOTS`].
    pub max_concurrent_assessments: usize,
}

/// The largest `max_concurrent_assessments` and `max_in_flight` the service
/// accepts (the most permits a tokio semaphore can hold).
pub const MAX_SLOTS: usize = Semaphore::MAX_PERMITS;

impl Default for ServiceLimits {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Self {
            max_batch_size: 1_000,
            max_in_flight: cpus.max(2),
            max_concurrent_assessments: cpus.max(1),
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

    /// These limits with `max_in_flight` (at least 1, at most
    /// [`MAX_SLOTS`]).
    #[must_use]
    pub fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight.clamp(1, MAX_SLOTS);
        self
    }

    /// These limits with `max_concurrent_assessments` (at least 1, at most
    /// [`MAX_SLOTS`]).
    #[must_use]
    pub fn with_max_concurrent_assessments(mut self, n: usize) -> Self {
        self.max_concurrent_assessments = n.clamp(1, MAX_SLOTS);
        self
    }

    /// These limits with every value brought into its valid range. The
    /// fields are public, so a caller can set any number.
    fn normalised(mut self) -> Self {
        self.max_batch_size = self.max_batch_size.max(1);
        self.max_in_flight = self.max_in_flight.clamp(1, MAX_SLOTS);
        self.max_concurrent_assessments = self.max_concurrent_assessments.clamp(1, MAX_SLOTS);
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
    /// One permit per stateless assessment that may run at once: `Assess`,
    /// `CompareText`, batch and stream items. Shared by clones made after
    /// the last `with_limits`.
    stateless_slots: Arc<Semaphore>,
    /// One permit per registry call that may run at once: `PutEntity`,
    /// `DeleteEntity`, `RecordActivity`, `CompareVersions`. Separate, so a
    /// wait on the registry lock or the journal never holds a stateless slot.
    registry_slots: Arc<Semaphore>,
}

impl AssessmentService {
    /// A service over `engine` (stateless calls; per-request policy overrides
    /// are merged over its policy) and `registry` (stateful calls).
    pub fn new(engine: Engine, registry: Arc<Registry>) -> Self {
        let limits = ServiceLimits::default().normalised();
        Self {
            engine: Arc::new(engine),
            registry,
            limits,
            stateless_slots: Arc::new(Semaphore::new(limits.max_concurrent_assessments)),
            registry_slots: Arc::new(Semaphore::new(limits.max_concurrent_assessments)),
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
    ///
    /// Values out of range are brought into range first: every limit is at
    /// least 1, and `max_in_flight` and `max_concurrent_assessments` are at
    /// most [`MAX_SLOTS`]. [`limits`](Self::limits) returns the values in
    /// force.
    ///
    /// This starts fresh slot pools. Clones made before this call keep the
    /// old pools, so set the limits before cloning the service.
    #[must_use]
    pub fn with_limits(mut self, limits: ServiceLimits) -> Self {
        let limits = limits.normalised();
        self.limits = limits;
        self.stateless_slots = Arc::new(Semaphore::new(limits.max_concurrent_assessments));
        self.registry_slots = Arc::new(Semaphore::new(limits.max_concurrent_assessments));
        self
    }

    /// Runs one stateless assessment on the blocking pool once a stateless
    /// slot is free.
    async fn assess_blocking<T, F>(&self, f: F) -> Result<T, Status>
    where
        F: FnOnce() -> Result<T, Status> + Send + 'static,
        T: Send + 'static,
    {
        blocking_in_slot(&self.stateless_slots, f).await
    }

    /// Runs one registry call on the blocking pool once a registry slot is
    /// free.
    async fn registry_blocking<T, F>(&self, f: F) -> Result<T, Status>
    where
        F: FnOnce() -> Result<T, Status> + Send + 'static,
        T: Send + 'static,
    {
        blocking_in_slot(&self.registry_slots, f).await
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

/// Waits for a slot of `slots`, then runs `f` on the blocking pool holding
/// it. If the caller goes away while waiting, the place in the queue is
/// given up. Once `f` has started, it runs to completion holding the slot.
async fn blocking_in_slot<T, F>(slots: &Arc<Semaphore>, f: F) -> Result<T, Status>
where
    F: FnOnce() -> Result<T, Status> + Send + 'static,
    T: Send + 'static,
{
    let slot = Arc::clone(slots)
        .acquire_owned()
        .await
        .map_err(|_| Status::unavailable("the service is shutting down"))?;
    blocking(move || {
        let _slot = slot;
        f()
    })
    .await
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

/// One started item of a batch or a stream.
///
/// Its task waits for a stateless slot, then runs the assessment on the
/// blocking pool. Dropping this aborts the task: an item still waiting gives
/// up its place in the queue and its permits; an item already assessing runs
/// to completion and then frees its slot.
struct ItemTask {
    request_id: String,
    handle: tokio::task::JoinHandle<pb::AssessResponse>,
}

impl ItemTask {
    /// Starts one item. The item waits for its slot on its own task, so the
    /// caller never waits for a slot and a call can queue several items at
    /// once. `permit` (the per-call permit, when the item should hold it) is
    /// taken before the slot and released when the item finishes.
    fn spawn(
        engine: &Arc<Engine>,
        slots: &Arc<Semaphore>,
        request: pb::AssessRequest,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Self {
        let request_id = request.request_id.clone();
        let engine = Arc::clone(engine);
        let slots = Arc::clone(slots);
        let handle = tokio::spawn(async move {
            let request_id = request.request_id.clone();
            let Ok(slot) = slots.acquire_owned().await else {
                let failure = Failure::Internal("the service is shutting down".to_owned());
                return item_response(request_id, Err(failure));
            };
            let work = tokio::task::spawn_blocking(move || {
                let _held = (permit, slot);
                assess_item(&engine, request)
            });
            match work.await {
                Ok(response) => response,
                Err(err) => item_response(request_id, Err(join_failure(&err))),
            }
        });
        Self { request_id, handle }
    }

    /// The item's response (an `internal` error if its task failed).
    async fn finish(mut self) -> pb::AssessResponse {
        match (&mut self.handle).await {
            Ok(response) => response,
            Err(err) => {
                let request_id = std::mem::take(&mut self.request_id);
                item_response(request_id, Err(join_failure(&err)))
            }
        }
    }
}

impl Drop for ItemTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<pb::AssessResponse, Status>> + Send>>;

/// What the stream reader hands the writer, in request order.
enum Pending {
    Item {
        task: ItemTask,
        permit: OwnedSemaphorePermit,
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
        let assessment = self
            .assess_blocking(move || {
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
        // Each item takes a per-call permit first, then waits for its
        // stateless slot on its own task. So the call holds up to
        // `max_in_flight` places in the slot queue, and nothing waits while
        // holding a slot. Dropping the call (timeout, cancellation) drops
        // `running`, which aborts the items still waiting.
        let permits = Arc::new(Semaphore::new(self.limits.max_in_flight));
        let mut running = Vec::with_capacity(requests.len());
        for request in requests {
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .map_err(|_| Status::internal("the batch was cancelled"))?;
            running.push(ItemTask::spawn(
                &self.engine,
                &self.stateless_slots,
                request,
                Some(permit),
            ));
        }
        let mut responses = Vec::with_capacity(running.len());
        for task in running {
            responses.push(task.finish().await);
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
        let slots = Arc::clone(&self.stateless_slots);

        // Reader: starts each request as it arrives, at most `in_flight` at
        // once. It takes the per-call permit; the item then waits for its
        // stateless slot on its own task, so the reader never waits for a
        // slot. It stops as soon as the writer has gone.
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    biased;
                    () = pending_tx.closed() => break,
                    next = inbound.message() => next,
                };
                let pending = match next {
                    Ok(Some(request)) => {
                        let permit = tokio::select! {
                            biased;
                            () = pending_tx.closed() => break,
                            permit = Arc::clone(&permits).acquire_owned() => permit,
                        };
                        let Ok(permit) = permit else {
                            break;
                        };
                        let task = ItemTask::spawn(&engine, &slots, request, None);
                        Pending::Item { task, permit }
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
        // once its response is handed to the transport. When the client has
        // gone, the writer stops; dropping the queued items aborts those
        // still waiting for a slot.
        tokio::spawn(async move {
            while let Some(pending) = pending_rx.recv().await {
                let (item, permit) = match pending {
                    Pending::Item { task, permit } => {
                        let response = tokio::select! {
                            biased;
                            () = out_tx.closed() => break,
                            response = task.finish() => response,
                        };
                        (Ok(response), Some(permit))
                    }
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
        let diff = self
            .assess_blocking(move || {
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
        let outcome = self
            .registry_blocking(move || into_core_result(registry.put(entity, options)))
            .await?;
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
        let assessment = self
            .registry_blocking(move || {
                into_core_result(registry.delete(&r.id, dry_run, expected, now))
            })
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
        let assessment = self
            .registry_blocking(move || {
                into_core_result(registry.record_activity(&r.entity_id, events, now))
            })
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
        let assessment = self
            .registry_blocking(move || into_core_result(registry.compare(&r.id, from, to, now)))
            .await?;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assessment_core::model::{
        ActivityEvent, ActivityKind, AssessmentRequest, ChangeSet, Entity,
    };
    use assessment_core::registry::{PutOptions, PutOutcome};
    use assessment_core::semver::SemVer;

    use super::*;

    /// How long a call must stay pending to count as "waiting for a slot".
    const BLOCKED: Duration = Duration::from_millis(100);
    /// The bound on everything that must finish.
    const DONE: Duration = Duration::from_secs(20);

    fn service(slots: usize, in_flight: usize) -> AssessmentService {
        AssessmentService::in_memory().with_limits(
            ServiceLimits::default()
                .with_max_concurrent_assessments(slots)
                .with_max_in_flight(in_flight),
        )
    }

    fn assess_request(id: &str) -> pb::AssessRequest {
        let before = Entity::new(id, "issue")
            .with_title("Export invoices as CSV")
            .with_version(SemVer::new(1, 0, 0));
        let after = before.clone().with_title("Export invoices as PDF");
        let mut request = pb::AssessRequest::from(AssessmentRequest {
            changes: vec![ChangeSet::update(before, after)],
            ..Default::default()
        });
        request.request_id = id.to_owned();
        request
    }

    fn batch_request(n: usize) -> pb::AssessBatchRequest {
        pb::AssessBatchRequest {
            requests: (0..n).map(|i| assess_request(&format!("B-{i}"))).collect(),
        }
    }

    fn compare_text_request() -> pb::CompareTextRequest {
        pb::CompareTextRequest {
            before: "Export invoices as CSV".to_owned(),
            after: "Export invoices as PDF".to_owned(),
            before_embedding: Vec::new(),
            after_embedding: Vec::new(),
        }
    }

    fn put_request(title: &str) -> pb::PutEntityRequest {
        convert::put_request_to_pb(
            Entity::new("ENG-1", "issue").with_title(title),
            PutOptions::default(),
        )
    }

    fn delete_request() -> pb::DeleteEntityRequest {
        pb::DeleteEntityRequest {
            id: "ENG-1".to_owned(),
            dry_run: false,
            expected_version: None,
            now: None,
        }
    }

    fn activity_request() -> pb::RecordActivityRequest {
        let event = ActivityEvent {
            entity_id: "ENG-1".to_owned(),
            kind: ActivityKind::Comment,
            author: Some("dorj".to_owned()),
            body: "Blocked: we are waiting on the identity provider.".to_owned(),
            at: None,
        };
        pb::RecordActivityRequest {
            entity_id: "ENG-1".to_owned(),
            events: vec![event.into()],
            now: None,
        }
    }

    fn compare_versions_request(from: SemVer, to: SemVer) -> pb::CompareVersionsRequest {
        pb::CompareVersionsRequest {
            id: "ENG-1".to_owned(),
            from: Some(from.into()),
            to: Some(to.into()),
            now: None,
        }
    }

    /// Whether `call` is still pending after [`BLOCKED`]. The call is
    /// dropped (cancelled) on return.
    async fn is_blocked<F: Future>(call: F) -> bool {
        tokio::time::timeout(BLOCKED, call).await.is_err()
    }

    /// `call`'s output, or a panic if it takes longer than [`DONE`].
    async fn done<F: Future>(call: F) -> F::Output {
        tokio::time::timeout(DONE, call)
            .await
            .expect("finishes in time")
    }

    /// Waits until every permit of `pool` is free again. Aborted item tasks
    /// and finished blocking work give their permits back a moment later.
    async fn wait_until_full(pool: &Arc<Semaphore>, size: usize) {
        done(async {
            while pool.available_permits() != size {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
    }

    async fn take_all(pool: &Arc<Semaphore>, size: usize) -> OwnedSemaphorePermit {
        let size = u32::try_from(size).expect("a small pool");
        done(Arc::clone(pool).acquire_many_owned(size))
            .await
            .expect("the pool is open")
    }

    fn version(outcome: &PutOutcome) -> SemVer {
        outcome
            .entity
            .version
            .expect("a stored entity has a version")
    }

    #[tokio::test]
    async fn stateless_calls_wait_for_a_stateless_slot_and_registry_calls_do_not() {
        const SLOTS: usize = 2;
        let svc = service(SLOTS, 2);
        let held = take_all(&svc.stateless_slots, SLOTS).await;

        // Every stateless call waits, and gives up its place when dropped.
        assert!(is_blocked(svc.assess(Request::new(assess_request("U-1")))).await);
        assert!(is_blocked(svc.compare_text(Request::new(compare_text_request()))).await);
        assert!(is_blocked(svc.assess_batch(Request::new(batch_request(5)))).await);

        // Registry calls use the other pool and still complete.
        let created = done(svc.put_entity(Request::new(put_request("Export invoices"))))
            .await
            .expect("creates");
        assert!(created.into_inner().created);
        assert_eq!(svc.registry_slots.available_permits(), SLOTS);

        // No cancelled waiter keeps a place: the pool refills completely.
        assert_eq!(svc.stateless_slots.available_permits(), 0);
        drop(held);
        wait_until_full(&svc.stateless_slots, SLOTS).await;

        // Released, the same calls succeed, and free their slots again.
        let response = done(svc.assess(Request::new(assess_request("U-1"))))
            .await
            .expect("assesses")
            .into_inner();
        assert!(response.assessment.is_some());
        done(svc.compare_text(Request::new(compare_text_request())))
            .await
            .expect("compares");
        let batch = done(svc.assess_batch(Request::new(batch_request(5))))
            .await
            .expect("the batch succeeds")
            .into_inner();
        assert_eq!(batch.responses.len(), 5);
        for (i, response) in batch.responses.iter().enumerate() {
            assert_eq!(response.request_id, format!("B-{i}"));
            assert!(response.assessment.is_some() && response.error.is_none());
        }
        wait_until_full(&svc.stateless_slots, SLOTS).await;
    }

    #[tokio::test]
    async fn registry_calls_wait_for_a_registry_slot_and_stateless_calls_do_not() {
        const SLOTS: usize = 2;
        let svc = service(SLOTS, 2);
        let first = SemVer::INITIAL;
        let held = take_all(&svc.registry_slots, SLOTS).await;

        // Every registry call that assesses waits.
        assert!(is_blocked(svc.put_entity(Request::new(put_request("Export invoices")))).await);
        assert!(is_blocked(svc.delete_entity(Request::new(delete_request()))).await);
        assert!(is_blocked(svc.record_activity(Request::new(activity_request()))).await);
        assert!(
            is_blocked(svc.compare_versions(Request::new(compare_versions_request(first, first))))
                .await
        );
        // None of them reached the registry.
        assert_eq!(svc.registry().len(), 0);

        // Stateless calls still complete while the registry pool is used up.
        done(svc.assess(Request::new(assess_request("U-1"))))
            .await
            .expect("assesses");
        done(svc.compare_text(Request::new(compare_text_request())))
            .await
            .expect("compares");
        let batch = done(svc.assess_batch(Request::new(batch_request(4))))
            .await
            .expect("the batch succeeds");
        assert_eq!(batch.into_inner().responses.len(), 4);
        let unslotted = pb::GetHistoryRequest {
            id: "ENG-1".to_owned(),
            limit: 0,
        };
        let status = done(svc.get_history(Request::new(unslotted)))
            .await
            .expect_err("nothing is stored yet");
        assert_eq!(status.code(), tonic::Code::NotFound);

        assert_eq!(svc.registry_slots.available_permits(), 0);
        drop(held);
        wait_until_full(&svc.registry_slots, SLOTS).await;

        // Released, the same calls succeed.
        let created = done(svc.put_entity(Request::new(put_request("Export invoices"))))
            .await
            .expect("creates")
            .into_inner();
        let created = PutOutcome::try_from(created).expect("decodes");
        assert!(created.created && created.stored);
        let updated = done(svc.put_entity(Request::new(put_request(
            "Import bank statements and reconcile them",
        ))))
        .await
        .expect("updates")
        .into_inner();
        let updated = PutOutcome::try_from(updated).expect("decodes");
        assert!(updated.stored && !updated.created);
        done(svc.record_activity(Request::new(activity_request())))
            .await
            .expect("assesses the activity");
        let compare = compare_versions_request(version(&created), version(&updated));
        done(svc.compare_versions(Request::new(compare)))
            .await
            .expect("compares the versions");
        let deleted = done(svc.delete_entity(Request::new(delete_request())))
            .await
            .expect("deletes")
            .into_inner();
        assert!(deleted.deleted);
        wait_until_full(&svc.registry_slots, SLOTS).await;
        assert_eq!(svc.stateless_slots.available_permits(), SLOTS);
    }

    /// A batch queues up to `max_in_flight` items for slots at once, and a
    /// dropped batch takes all of them out of the queue.
    #[tokio::test]
    async fn a_batch_queues_max_in_flight_items_and_a_dropped_batch_leaves_the_queue() {
        let svc = service(3, 3);
        let held = take_all(&svc.stateless_slots, 3).await;

        let batch = svc.assess_batch(Request::new(batch_request(8)));
        tokio::pin!(batch);
        assert!(is_blocked(&mut batch).await);
        // One slot is enough for the whole batch to finish, in order.
        let mut held = held;
        drop(held.split(1).expect("holds three permits"));
        let responses = done(&mut batch)
            .await
            .expect("the batch succeeds")
            .into_inner()
            .responses;
        assert_eq!(responses.len(), 8);
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response.request_id, format!("B-{i}"));
        }
        wait_until_full(&svc.stateless_slots, 1).await;

        // Take the free slot again, then drop a waiting batch. A unary call
        // queued after it gets the next free slot.
        let last = take_all(&svc.stateless_slots, 1).await;
        assert!(is_blocked(svc.assess_batch(Request::new(batch_request(8)))).await);
        drop(last);
        done(svc.assess(Request::new(assess_request("U-1"))))
            .await
            .expect("assesses");
        drop(held);
        wait_until_full(&svc.stateless_slots, 3).await;
    }

    #[test]
    fn limits_are_brought_into_range() {
        // What a caller can build by setting the public fields.
        let mut limits = ServiceLimits {
            max_batch_size: 0,
            max_in_flight: 0,
            max_concurrent_assessments: 0,
        };
        let svc = AssessmentService::in_memory().with_limits(limits);
        assert_eq!(svc.limits().max_batch_size, 1);
        assert_eq!(svc.limits().max_in_flight, 1);
        assert_eq!(svc.limits().max_concurrent_assessments, 1);
        assert_eq!(svc.stateless_slots.available_permits(), 1);
        assert_eq!(svc.registry_slots.available_permits(), 1);

        limits.max_in_flight = usize::MAX;
        limits.max_concurrent_assessments = usize::MAX;
        let svc = AssessmentService::in_memory().with_limits(limits);
        assert_eq!(svc.limits().max_in_flight, MAX_SLOTS);
        assert_eq!(svc.limits().max_concurrent_assessments, MAX_SLOTS);
        assert_eq!(svc.stateless_slots.available_permits(), MAX_SLOTS);

        let limits = ServiceLimits::default()
            .with_max_concurrent_assessments(usize::MAX)
            .with_max_in_flight(usize::MAX);
        assert_eq!(limits.max_concurrent_assessments, MAX_SLOTS);
        assert_eq!(limits.max_in_flight, MAX_SLOTS);
        assert_eq!(
            ServiceLimits::default()
                .with_max_concurrent_assessments(0)
                .max_concurrent_assessments,
            1
        );
    }

    /// With zero-sized limits set through the public fields, calls still
    /// complete (they used to wait forever).
    #[tokio::test]
    async fn zero_limits_do_not_hang() {
        let limits = ServiceLimits {
            max_in_flight: 0,
            max_concurrent_assessments: 0,
            ..ServiceLimits::default()
        };
        let svc = AssessmentService::in_memory().with_limits(limits);
        done(svc.assess(Request::new(assess_request("U-1"))))
            .await
            .expect("assesses");
        let batch = done(svc.assess_batch(Request::new(batch_request(3))))
            .await
            .expect("the batch succeeds");
        assert_eq!(batch.into_inner().responses.len(), 3);
    }
}
