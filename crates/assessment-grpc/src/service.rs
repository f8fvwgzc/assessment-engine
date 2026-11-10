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
//!
//! `Import` reads its stream in chunks ([`ServiceLimits::max_import_chunk`]
//! items, or [`IMPORT_CHUNK_BYTES`] of messages, whichever comes first). Each
//! chunk is one registry call: it waits for a registry slot, runs on the
//! blocking pool and gives the slot back. So the server holds one chunk of
//! a stream in memory, however long the stream is, and other registry calls
//! get their turn between chunks.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use assessment_core::engine::{MAX_EMBEDDING_DIM, MAX_TEXT_BYTES};
use assessment_core::model::field;
use assessment_core::registry::{ImportItem, ImportSummary};
use assessment_core::{Engine, Error as CoreError, ErrorKind, Registry};
use assessment_judge::Judge;
use prost::Message as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::time::Instant;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::convert::{self, AssessInput, ConvertError};
use crate::flow::{self, Flow, JudgeReport, JudgeSettings};
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
    /// to this many places in the slot queue. The second passes of a judged
    /// batch count against the same number. On a stream this also bounds
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
    /// Most items of an `Import` stream the server applies as one registry
    /// call. A chunk also ends once its messages reach
    /// [`IMPORT_CHUNK_BYTES`]. The server reads the next chunk only after
    /// the last one is applied, so this bounds the memory one import holds.
    /// Default 512. [`AssessmentService::with_limits`] keeps it within 1
    /// and [`MAX_IMPORT_CHUNK`].
    pub max_import_chunk: usize,
}

/// The largest [`ServiceLimits::max_import_chunk`] the service accepts.
pub const MAX_IMPORT_CHUNK: usize = 65_536;

/// The encoded size of the messages after which an `Import` chunk ends,
/// whatever its item count: 4 MiB. A chunk can pass it by one message.
pub const IMPORT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Most failed items one `ImportResponse` lists. `failed_total` counts all.
pub const MAX_IMPORT_FAILURES: usize = 1_000;

/// Most bytes of an item's id that an `ImportFailure` carries. A longer id
/// is cut and ends in `...`.
pub const MAX_IMPORT_FAILURE_ID_BYTES: usize = 256;

/// Most bytes of an error text that an `ImportFailure` carries. A longer
/// text is cut and ends in `...`. With [`MAX_IMPORT_FAILURES`] and
/// [`MAX_IMPORT_FAILURE_ID_BYTES`] this keeps the list under 1.5 MiB.
pub const MAX_IMPORT_FAILURE_ERROR_BYTES: usize = 1_024;

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
            max_import_chunk: 512,
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

    /// These limits with `max_import_chunk` (at least 1, at most
    /// [`MAX_IMPORT_CHUNK`]).
    #[must_use]
    pub fn with_max_import_chunk(mut self, items: usize) -> Self {
        self.max_import_chunk = items.clamp(1, MAX_IMPORT_CHUNK);
        self
    }

    /// These limits with every value brought into its valid range. The
    /// fields are public, so a caller can set any number.
    fn normalised(mut self) -> Self {
        self.max_batch_size = self.max_batch_size.max(1);
        self.max_in_flight = self.max_in_flight.clamp(1, MAX_SLOTS);
        self.max_concurrent_assessments = self.max_concurrent_assessments.clamp(1, MAX_SLOTS);
        self.max_import_chunk = self.max_import_chunk.clamp(1, MAX_IMPORT_CHUNK);
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
    /// `DeleteEntity`, `RecordActivity`, `CompareVersions`, and each chunk
    /// of an `Import`. Separate, so a wait on the registry lock or the
    /// journal never holds a stateless slot.
    registry_slots: Arc<Semaphore>,
    /// The optional judge and its settings.
    flow: Flow,
    /// The server's request timeout: a unary call's deadline counts from
    /// the start of the request.
    call_timeout: Option<Duration>,
}

/// When the server began to handle a request. The auth interceptor puts it
/// in the request's extensions; the judge's deadline counts from it.
#[derive(Debug, Clone, Copy)]
pub struct RequestStart(pub Instant);

/// A stream with a judge may have this many more items in flight: an item
/// that waits for the judge uses no CPU.
const JUDGED_STREAM_EXTRA: usize = 8;

impl AssessmentService {
    /// A service over `engine` (stateless calls; per-request policy overrides
    /// are merged over its policy) and `registry` (stateful calls).
    pub fn new(engine: Engine, registry: Arc<Registry>) -> Self {
        let limits = ServiceLimits::default().normalised();
        let stateless_slots = Arc::new(Semaphore::new(limits.max_concurrent_assessments));
        Self {
            engine: Arc::new(engine),
            registry,
            limits,
            flow: Flow::new().with_slots(Arc::clone(&stateless_slots)),
            stateless_slots,
            registry_slots: Arc::new(Semaphore::new(limits.max_concurrent_assessments)),
            call_timeout: None,
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
    /// least 1, `max_in_flight` and `max_concurrent_assessments` are at
    /// most [`MAX_SLOTS`], and `max_import_chunk` is at most
    /// [`MAX_IMPORT_CHUNK`]. [`limits`](Self::limits) returns the values in
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
        self.flow = self.flow.with_slots(Arc::clone(&self.stateless_slots));
        self
    }

    /// This service with a judge for the flagged `title` and `description`
    /// changes of `Assess`, `AssessBatch`, `AssessStream`, `PutEntity` and
    /// `CompareVersions`. A judge that fails or is slow never fails a call.
    #[must_use]
    pub fn with_judge(mut self, judge: Arc<dyn Judge>, settings: JudgeSettings) -> Self {
        self.flow = self.flow.with_judge(judge, settings);
        self
    }

    /// This service with the server's request timeout. A unary call gives
    /// its judge only the time that leaves room to answer within it.
    /// [`engine_server`](crate::server::engine_server) sets it.
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// This service with a shutdown signal. Once it is `true`, open calls
    /// stop waiting for the judge and answer without it.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: watch::Receiver<bool>) -> Self {
        self.flow = self.flow.with_shutdown(shutdown);
        self
    }

    /// The name of the judge, if the service has one.
    pub fn judge_id(&self) -> Option<&str> {
        self.flow.judge_id()
    }

    /// When the caller of `request` stops waiting: the start of the request
    /// plus the shorter of the server's timeout (unary calls only) and the
    /// client's `grpc-timeout`.
    fn call_deadline<T>(&self, request: &Request<T>, unary: bool) -> Option<Instant> {
        let start = request
            .extensions()
            .get::<RequestStart>()
            .map_or_else(Instant::now, |start| start.0);
        let client = request
            .metadata()
            .get("grpc-timeout")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_grpc_timeout);
        let server = if unary { self.call_timeout } else { None };
        let limit = match (server, client) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        limit.and_then(|limit| start.checked_add(limit))
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

    /// One registry put, in a registry slot.
    async fn put(
        &self,
        entity: assessment_core::model::Entity,
        options: assessment_core::registry::PutOptions,
    ) -> Result<assessment_core::registry::PutOutcome, Status> {
        let registry = Arc::clone(&self.registry);
        self.registry_blocking(move || {
            let outcome = registry.put(entity, options);
            log_compaction_error(&registry);
            into_core_result(outcome)
        })
        .await
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

/// The value of a `grpc-timeout` header: up to 8 digits and a unit.
fn parse_grpc_timeout(value: &str) -> Option<Duration> {
    let unit = value.chars().last()?;
    let digits = value.strip_suffix(unit)?;
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        'H' => Duration::from_secs(n * 3_600),
        'M' => Duration::from_secs(n * 60),
        'S' => Duration::from_secs(n),
        'm' => Duration::from_millis(n),
        'u' => Duration::from_micros(n),
        'n' => Duration::from_nanos(n),
        _ => return None,
    })
}

/// The first pass of one request.
enum First {
    /// Nothing for the judge: the answer is ready.
    Done(Box<pb::Assessment>),
    /// The judge may be asked about a flagged field.
    Open(Box<flow::Pass1>),
}

/// Assesses one request (blocking). Without flagged fields for the judge,
/// the answer is encoded here too.
fn first_pass(engine: &Arc<Engine>, input: AssessInput, flow: &Flow) -> Result<First, Failure> {
    let engine = engine_for(engine, input.policy_overrides).map_err(Failure::Core)?;
    let first = flow::pass_1(engine, input.request, flow).map_err(Failure::Core)?;
    Ok(if first.flagged() {
        First::Open(Box::new(first))
    } else {
        First::Done(Box::new(first.into_assessment().into()))
    })
}

/// Decodes and assesses one request of a batch or a stream (blocking).
fn first_item(
    engine: &Arc<Engine>,
    request: pb::AssessRequest,
    flow: &Flow,
) -> Result<First, Failure> {
    let input = AssessInput::try_from(request).map_err(Failure::Convert)?;
    first_pass(engine, input, flow)
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

/// A task that is aborted when this is dropped.
struct Task<T>(tokio::task::JoinHandle<T>);

impl<T> Task<T> {
    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Waits for a stateless slot, then runs the first pass of one item on the
/// blocking pool. `permit` (the per-call permit, when the item should hold
/// it) is released with the slot.
async fn run_first(
    engine: Arc<Engine>,
    slots: Arc<Semaphore>,
    request: pb::AssessRequest,
    permit: Option<OwnedSemaphorePermit>,
    flow: Flow,
) -> Result<First, Failure> {
    let Ok(slot) = slots.acquire_owned().await else {
        return Err(Failure::Internal("the service is shutting down".to_owned()));
    };
    let work = tokio::task::spawn_blocking(move || {
        let _held = (permit, slot);
        first_item(&engine, request, &flow)
    });
    match work.await {
        Ok(result) => result,
        Err(err) => Err(join_failure(&err)),
    }
}

/// One started item of a batch or a stream.
///
/// Its task waits for a stateless slot, then runs the assessment on the
/// blocking pool. Dropping this aborts the task: an item still waiting gives
/// up its place in the queue and its permits; an item already assessing runs
/// to completion and then frees its slot; an item waiting for the judge
/// stops asking.
struct ItemTask<T> {
    request_id: String,
    task: Task<T>,
}

impl ItemTask<Result<First, Failure>> {
    /// Starts the first pass of one batch item. The item waits for its slot
    /// on its own task, so the caller never waits for a slot and a call can
    /// queue several items at once.
    fn first(
        engine: &Arc<Engine>,
        slots: &Arc<Semaphore>,
        request: pb::AssessRequest,
        permit: OwnedSemaphorePermit,
        flow: &Flow,
    ) -> Self {
        let request_id = request.request_id.clone();
        let work = run_first(
            Arc::clone(engine),
            Arc::clone(slots),
            request,
            Some(permit),
            flow.clone(),
        );
        Self {
            request_id,
            task: Task(tokio::spawn(work)),
        }
    }

    /// The item's id and its first pass.
    async fn finish(mut self) -> (String, Result<First, Failure>) {
        let request_id = std::mem::take(&mut self.request_id);
        match self.task.join().await {
            Ok(first) => (request_id, first),
            Err(err) => (request_id, Err(join_failure(&err))),
        }
    }
}

impl ItemTask<pb::AssessResponse> {
    /// Starts one stream item: both passes and the judge between them.
    /// `deadline` is when the client stops waiting, if it said so.
    fn spawn(
        engine: &Arc<Engine>,
        slots: &Arc<Semaphore>,
        request: pb::AssessRequest,
        flow: &Flow,
        deadline: Option<Instant>,
    ) -> Self {
        let request_id = request.request_id.clone();
        let engine = Arc::clone(engine);
        let slots = Arc::clone(slots);
        let flow = flow.clone();
        let handle = tokio::spawn(async move {
            let request_id = request.request_id.clone();
            let first = run_first(engine, slots, request, None, flow.clone()).await;
            let result = match first {
                Ok(First::Done(assessment)) => Ok(*assessment),
                Ok(First::Open(first)) => {
                    let judged = flow::complete(*first, &flow, deadline).await;
                    judged.report.log("AssessStream");
                    Ok(judged.assessment.into())
                }
                Err(failure) => Err(failure),
            };
            item_response(request_id, result)
        });
        Self {
            request_id,
            task: Task(handle),
        }
    }

    /// The item's response (an `internal` error if its task failed).
    async fn finish(mut self) -> pb::AssessResponse {
        let request_id = std::mem::take(&mut self.request_id);
        match self.task.join().await {
            Ok(response) => response,
            Err(err) => item_response(request_id, Err(join_failure(&err))),
        }
    }
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<pb::AssessResponse, Status>> + Send>>;

/// What the stream reader hands the writer, in request order.
enum Pending {
    Item {
        task: ItemTask<pb::AssessResponse>,
        permit: OwnedSemaphorePermit,
    },
    Failed(Status),
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// What the chunks of one `Import` call did so far.
#[derive(Debug, Default)]
struct ImportTotals {
    summary: ImportSummary,
    /// Every failed item, including those `summary.failed` no longer lists.
    failed_total: u64,
}

impl ImportTotals {
    fn add(&mut self, chunk: ImportSummary) {
        self.summary.created += chunk.created;
        self.summary.unchanged += chunk.unchanged;
        self.summary.assessed += chunk.assessed;
        self.summary.skipped_stale += chunk.skipped_stale;
        self.summary.deleted += chunk.deleted;
        self.failed_total += chunk.failed.len() as u64;
        let room = MAX_IMPORT_FAILURES.saturating_sub(self.summary.failed.len());
        // Ids have no length limit and the error repeats the id, so both
        // are cut: the list then has a fixed largest size.
        self.summary
            .failed
            .extend(chunk.failed.into_iter().take(room).map(|mut f| {
                cut(&mut f.id, MAX_IMPORT_FAILURE_ID_BYTES);
                cut(&mut f.error, MAX_IMPORT_FAILURE_ERROR_BYTES);
                f
            }));
    }

    fn into_pb(self) -> pb::ImportResponse {
        let failed_total = self.failed_total;
        pb::ImportResponse {
            failed_total,
            ..self.summary.into()
        }
    }
}

/// Cuts `text` to at most `max` bytes, on a character boundary. A text
/// that was cut ends in `...` (inside the limit).
fn cut(text: &mut String, max: usize) {
    const MARK: &str = "...";
    if text.len() <= max {
        return;
    }
    let mut end = max.saturating_sub(MARK.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(MARK);
}

/// Applies one chunk of an import, in order (blocking).
///
/// An item that does not decode is listed as failed at its place, like an
/// item the registry rejects; the items around it are still applied. Only a
/// journal that cannot be written fails the chunk.
fn import_chunk(
    registry: &Registry,
    chunk: Vec<pb::ImportRequest>,
) -> Result<ImportSummary, CoreError> {
    let mut summary = ImportSummary::default();
    let mut add = |part: ImportSummary| {
        summary.created += part.created;
        summary.unchanged += part.unchanged;
        summary.assessed += part.assessed;
        summary.skipped_stale += part.skipped_stale;
        summary.deleted += part.deleted;
        summary.failed.extend(part.failed);
    };
    // Valid items go to the registry in runs, so that a failure to decode
    // keeps its place among the failures the registry reports.
    let mut run: Vec<ImportItem> = Vec::with_capacity(chunk.len());
    for request in chunk {
        let id = request.entity.as_ref().map(|e| e.id.clone());
        match ImportItem::try_from(request) {
            Ok(item) => run.push(item),
            Err(e) => {
                if !run.is_empty() {
                    add(registry.import(std::mem::take(&mut run))?);
                }
                add(ImportSummary {
                    failed: vec![assessment_core::registry::ImportFailure {
                        id: id.unwrap_or_default(),
                        error: e.to_string(),
                    }],
                    ..ImportSummary::default()
                });
            }
        }
    }
    if !run.is_empty() {
        add(registry.import(run)?);
    }
    Ok(summary)
}

/// Logs why the registry's last automatic compaction failed, if one did.
/// The write that set it off was stored all the same.
fn log_compaction_error(registry: &Registry) {
    if let Some(err) = registry.take_compaction_error() {
        log_internal(&err);
    }
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

/// The longest field name CompareText accepts.
const MAX_FIELD_NAME_BYTES: usize = 256;

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
        let deadline = self.call_deadline(&request, true);
        let request = request.into_inner();
        let request_id = request.request_id.clone();
        let engine = Arc::clone(&self.engine);
        let flow = self.flow.clone();
        let first = self
            .assess_blocking(move || {
                AssessInput::try_from(request)
                    .map_err(Failure::Convert)
                    .and_then(|input| first_pass(&engine, input, &flow))
                    .map_err(Failure::into_status)
            })
            .await?;
        let assessment = match first {
            First::Done(assessment) => *assessment,
            First::Open(first) => {
                let judged = flow::complete(*first, &self.flow, deadline).await;
                judged.report.log("Assess");
                judged.assessment.into()
            }
        };
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
        let deadline = self.call_deadline(&request, true);
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
        // The loop below waits for permits, so in a batch larger than
        // `max_in_flight` most of the first pass runs inside it. The time of
        // the first pass counts from here.
        let started = Instant::now();
        for request in requests {
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .map_err(|_| Status::internal("the batch was cancelled"))?;
            running.push(ItemTask::first(
                &self.engine,
                &self.stateless_slots,
                request,
                permit,
                &self.flow,
            ));
        }
        let mut items = Vec::with_capacity(running.len());
        for task in running {
            items.push(task.finish().await);
        }

        // One selection over the whole batch: the questions are spent on
        // the most doubtful fields first, whichever item they are in.
        let mut candidates: Vec<flow::Candidate> = items
            .iter()
            .enumerate()
            .filter_map(|(i, (_, first))| match first {
                Ok(First::Open(first)) => Some(first.candidates(i)),
                _ => None,
            })
            .flatten()
            .collect();
        candidates.sort_by(flow::Candidate::batch_order);
        let answers = if candidates.is_empty() {
            Vec::new()
        } else {
            let cap = self.flow.settings().max_fields_per_batch;
            flow::ask_all(&self.flow, &candidates, cap, deadline, started.elapsed()).await
        };
        let mut asked: Vec<(Vec<flow::Candidate>, Vec<flow::Answer>)> =
            items.iter().map(|_| (Vec::new(), Vec::new())).collect();
        for (candidate, answer) in candidates.into_iter().zip(answers) {
            let slot = &mut asked[candidate.item];
            slot.0.push(candidate);
            slot.1.push(answer);
        }

        // The second passes run side by side, each in its own slot. Like the
        // first passes, each takes a per-call permit before it queues for a
        // slot, so the call still holds at most `max_in_flight` places in
        // the slot queue. An item with no answer has no second pass and
        // takes no permit.
        enum Second {
            Ready(Box<pb::AssessResponse>),
            Running(String, Task<flow::Judged>),
        }
        let mut seconds = Vec::with_capacity(items.len());
        for ((request_id, first), (candidates, answers)) in items.into_iter().zip(asked) {
            seconds.push(match first {
                Ok(First::Done(assessment)) => {
                    Second::Ready(Box::new(item_response(request_id, Ok(*assessment))))
                }
                Ok(First::Open(first)) => {
                    let flow = self.flow.clone();
                    let permit = if flow::has_verdict(&answers) {
                        let permit = Arc::clone(&permits)
                            .acquire_owned()
                            .await
                            .map_err(|_| Status::internal("the batch was cancelled"))?;
                        Some(permit)
                    } else {
                        None
                    };
                    let work = async move {
                        let _permit = permit;
                        flow::finish(*first, candidates, answers, &flow, deadline).await
                    };
                    Second::Running(request_id, Task(tokio::spawn(work)))
                }
                Err(failure) => Second::Ready(Box::new(item_response(request_id, Err(failure)))),
            });
        }
        let mut report = JudgeReport::default();
        let mut responses = Vec::with_capacity(seconds.len());
        for second in seconds {
            responses.push(match second {
                Second::Ready(response) => *response,
                Second::Running(request_id, task) => match task.join().await {
                    Ok(judged) => {
                        report.add(&judged.report);
                        item_response(request_id, Ok(judged.assessment.into()))
                    }
                    Err(err) => item_response(request_id, Err(join_failure(&err))),
                },
            });
        }
        report.log("AssessBatch");
        Ok(Response::new(pb::AssessBatchResponse { responses }))
    }

    type AssessStreamStream = ResponseStream;

    async fn assess_stream(
        &self,
        request: Request<Streaming<pb::AssessRequest>>,
    ) -> Result<Response<Self::AssessStreamStream>, Status> {
        // A stream has no server-side deadline: only the client's counts.
        let deadline = self.call_deadline(&request, false);
        let mut inbound = request.into_inner();
        let flow = self.flow.clone();
        let extra = if flow.has_judge() {
            JUDGED_STREAM_EXTRA
        } else {
            0
        };
        let in_flight = self
            .limits
            .max_in_flight
            .saturating_add(extra)
            .min(MAX_SLOTS);
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
                        let task = ItemTask::spawn(&engine, &slots, request, &flow, deadline);
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
        if r.field.len() > MAX_FIELD_NAME_BYTES {
            return Err(Status::invalid_argument(format!(
                "field: is longer than {MAX_FIELD_NAME_BYTES} bytes"
            )));
        }
        check_embedding("before_embedding", &r.before_embedding)?;
        check_embedding("after_embedding", &r.after_embedding)?;
        let engine = Arc::clone(&self.engine);
        let diff = self
            .assess_blocking(move || {
                let embeddings = (!r.before_embedding.is_empty() && !r.after_embedding.is_empty())
                    .then_some((r.before_embedding.as_slice(), r.after_embedding.as_slice()));
                // An empty field is the description.
                let field = if r.field.is_empty() {
                    field::DESCRIPTION
                } else {
                    r.field.as_str()
                };
                engine
                    .compare_text_as(field, &r.before, &r.after, embeddings)
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
        // Empty when this service has no judge.
        let judge = self.flow.judge_id().unwrap_or_default().to_owned();
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
                judge,
            })
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn put_entity(
        &self,
        request: Request<pb::PutEntityRequest>,
    ) -> Result<Response<pb::PutEntityResponse>, Status> {
        let deadline = self.call_deadline(&request, true);
        let (entity, options) = convert::put_request_from_pb(request.into_inner())?;
        let asked = options.assess.judge;
        // A dry run is judged only when the caller asks for it.
        let judged = self.flow.wants(asked) && (!options.dry_run || asked == Some(true));
        if !judged {
            let mut outcome = self.put(entity, options).await?;
            if asked == Some(true) && !self.flow.has_judge() {
                flow::mark_no_judge(&mut outcome.assessment);
            }
            return Ok(Response::new(outcome.into()));
        }

        // Put 1 stores the change unless a field is flagged.
        let keep_texts = options.assess.judgment_texts;
        let mut first_options = options.clone();
        first_options.hold_if_flagged = true;
        first_options.assess.judgment_texts = true;
        let started = Instant::now();
        let mut outcome = self.put(entity.clone(), first_options).await?;
        let pass_1_time = started.elapsed();
        // Put 1 was held only if it asks for a judgment. Any other put is
        // settled: stored, unchanged, stale, or assessed with nothing to
        // store (activity on the same content). Running it again would be
        // wrong: its `source_seq` is remembered now, so put 2 would be stale.
        if outcome.assessment.judgment_requests.is_empty() {
            if !keep_texts {
                flow::strip_texts(&mut outcome.assessment);
            }
            return Ok(Response::new(outcome.into()));
        }

        let title = entity.title().to_owned();
        let candidates = flow::candidates(&outcome.assessment, 0, |_| Some(title.clone()));
        let mut answers = if candidates.is_empty() {
            Vec::new()
        } else {
            let cap = self.flow.settings().max_fields_per_request;
            flow::ask_all(&self.flow, &candidates, cap, deadline, pass_1_time).await
        };
        let verdicts = flow::judgments(&candidates, &answers);
        // With no answer, a dry run and a put the caller wanted held are
        // already what put 1 returned.
        if !(verdicts.is_empty() && (options.dry_run || options.hold_if_flagged)) {
            // Put 2 is the caller's own put, with the answers added. Its
            // version check and its time are the caller's.
            let mut second_options = options.clone();
            for (_, judgment) in verdicts.iter().cloned() {
                flow::merge_judgment(&mut second_options.judgments, judgment);
            }
            outcome = match self.put(entity.clone(), second_options).await {
                Ok(outcome) => outcome,
                Err(status)
                    if !verdicts.is_empty() && status.code() == tonic::Code::InvalidArgument =>
                {
                    // Put 1 took the same entity, so an answer is at fault.
                    tracing::warn!(error = %status.message(), "a judged put was rejected; storing without the judge");
                    flow::void_verdicts(&mut answers, flow::PASS_2_FAILED);
                    self.put(entity, options).await?
                }
                Err(status) => return Err(status),
            };
        }
        let report = flow::settle(
            &mut outcome.assessment,
            &candidates,
            &answers,
            keep_texts,
            &self.flow,
        );
        report.log("PutEntity");
        Ok(Response::new(outcome.into()))
    }

    async fn delete_entity(
        &self,
        request: Request<pb::DeleteEntityRequest>,
    ) -> Result<Response<pb::DeleteEntityResponse>, Status> {
        let (id, options) = convert::delete_request_from_pb(request.into_inner())?;
        let registry = Arc::clone(&self.registry);
        let outcome = self
            .registry_blocking(move || {
                let outcome = registry.delete_with(&id, options);
                log_compaction_error(&registry);
                into_core_result(outcome)
            })
            .await?;
        Ok(Response::new(outcome.into()))
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
        let deadline = self.call_deadline(&request, true);
        let r = request.into_inner();
        let from = convert::decode_version(r.from, "from")?;
        let to = convert::decode_version(r.to, "to")?;
        let now = convert::decode_time(r.now, "now")?;
        let mut options = convert::options_from_pb(r.options);
        let judgments = convert::judgments_from_pb(r.judgments, "judgments")?;
        let asked = options.judge;
        if !self.flow.wants(asked) {
            let registry = Arc::clone(&self.registry);
            let mut assessment = self
                .registry_blocking(move || {
                    into_core_result(
                        registry.compare_with(&r.id, from, to, now, &options, &judgments),
                    )
                })
                .await?;
            if asked == Some(true) && !self.flow.has_judge() {
                flow::mark_no_judge(&mut assessment);
            }
            return Ok(Response::new(pb::CompareVersionsResponse {
                assessment: Some(assessment.into()),
            }));
        }

        // Pass 1, with the texts of the flagged fields. Both passes see the
        // same time.
        let keep_texts = options.judgment_texts;
        options.judgment_texts = true;
        let now = Some(now.unwrap_or_else(chrono::Utc::now));
        let id = r.id;
        let started = Instant::now();
        let (mut assessment, title) = {
            let registry = Arc::clone(&self.registry);
            let (id, options, judgments) = (id.clone(), options.clone(), judgments.clone());
            self.registry_blocking(move || {
                let assessment = into_core_result(
                    registry.compare_with(&id, from, to, now, &options, &judgments),
                )?;
                let title = registry
                    .get(&id, Some(to))
                    .ok()
                    .map(|e| e.title().to_owned());
                Ok((assessment, title))
            })
            .await?
        };
        let pass_1_time = started.elapsed();
        let candidates = flow::candidates(&assessment, 0, |_| title.clone());
        let mut answers = if candidates.is_empty() {
            Vec::new()
        } else {
            let cap = self.flow.settings().max_fields_per_request;
            flow::ask_all(&self.flow, &candidates, cap, deadline, pass_1_time).await
        };
        let verdicts = flow::judgments(&candidates, &answers);
        if !verdicts.is_empty() {
            // Pass 2: the caller's judgments and the answers.
            let mut judgments = judgments;
            for (_, judgment) in verdicts {
                flow::merge_judgment(&mut judgments, judgment);
            }
            let registry = Arc::clone(&self.registry);
            let second = self.registry_blocking(move || {
                into_core_result(registry.compare_with(&id, from, to, now, &options, &judgments))
            });
            let second = match flow::watchdog(deadline) {
                Some(at) => tokio::time::timeout_at(at, second)
                    .await
                    .map_err(|_| flow::LATE),
                None => Ok(second.await),
            };
            match second {
                Ok(Ok(second)) => assessment = second,
                Ok(Err(status)) => {
                    tracing::warn!(error = %status.message(), "the second pass was rejected; using the first");
                    flow::void_verdicts(&mut answers, flow::PASS_2_FAILED);
                }
                Err(detail) => flow::void_verdicts(&mut answers, detail),
            }
        }
        let report = flow::settle(
            &mut assessment,
            &candidates,
            &answers,
            keep_texts,
            &self.flow,
        );
        report.log("CompareVersions");
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
        let entity = blocking(move || {
            let entity = registry.acknowledge(&r.id, now);
            log_compaction_error(&registry);
            into_core_result(entity)
        })
        .await?;
        Ok(Response::new(pb::AcknowledgeResponse {
            entity: Some(entity.into()),
        }))
    }

    async fn import(
        &self,
        request: Request<Streaming<pb::ImportRequest>>,
    ) -> Result<Response<pb::ImportResponse>, Status> {
        let mut inbound = request.into_inner();
        let max_items = self.limits.max_import_chunk;
        let mut totals = ImportTotals::default();
        let mut chunk: Vec<pb::ImportRequest> = Vec::new();
        let mut bytes = 0usize;
        // One chunk at a time: the next message is read only after the
        // chunk before it is applied, so the transport's flow control holds
        // the client back and memory stays at one chunk. If this call is
        // dropped (the client went away, the deadline passed), no further
        // chunk starts; a chunk that has started commits.
        loop {
            // A stream that fails (the client reset it, or sent a message
            // that does not decode) ends the call. The items read since the
            // last chunk are not applied.
            let next = inbound.message().await?;
            let ended = next.is_none();
            if let Some(item) = next {
                bytes = bytes.saturating_add(item.encoded_len());
                chunk.push(item);
            }
            if ended && !chunk.is_empty() {
                // A client that cancels the call looks like the end of the
                // stream here. Giving the transport one turn lets it drop
                // this call for a cancelled stream, before the items that
                // were read last are applied.
                tokio::task::yield_now().await;
            }
            let full = chunk.len() >= max_items || bytes >= IMPORT_CHUNK_BYTES;
            if !chunk.is_empty() && (ended || full) {
                let items = std::mem::take(&mut chunk);
                bytes = 0;
                let registry = Arc::clone(&self.registry);
                let summary = self
                    .registry_blocking(move || {
                        let summary = import_chunk(&registry, items);
                        log_compaction_error(&registry);
                        into_core_result(summary)
                    })
                    .await?;
                totals.add(summary);
            }
            if ended {
                break;
            }
        }
        tracing::debug!(
            created = totals.summary.created,
            unchanged = totals.summary.unchanged,
            assessed = totals.summary.assessed,
            skipped_stale = totals.summary.skipped_stale,
            deleted = totals.summary.deleted,
            failed = totals.failed_total,
            "import applied"
        );
        Ok(Response::new(totals.into_pb()))
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

    #[test]
    fn a_long_failure_text_is_cut_on_a_character_boundary() {
        let mut short = "ENG-1".to_owned();
        cut(&mut short, 8);
        assert_eq!(short, "ENG-1");

        let mut exact = "12345678".to_owned();
        cut(&mut exact, 8);
        assert_eq!(exact, "12345678");

        let mut long = "123456789".to_owned();
        cut(&mut long, 8);
        assert_eq!(long, "12345...");

        // "é" is two bytes: the cut does not split it.
        let mut accents = "éééééé".to_owned();
        cut(&mut accents, 8);
        assert_eq!(accents, "éé...");
        assert!(accents.len() <= 8);
    }

    #[test]
    fn import_totals_cut_the_id_and_the_error_of_a_failure() {
        let mut totals = ImportTotals::default();
        totals.add(ImportSummary {
            failed: vec![assessment_core::registry::ImportFailure {
                id: "x".repeat(10_000),
                error: "y".repeat(10_000),
            }],
            ..ImportSummary::default()
        });
        assert_eq!(totals.failed_total, 1);
        let f = &totals.summary.failed[0];
        assert_eq!(f.id.len(), MAX_IMPORT_FAILURE_ID_BYTES);
        assert_eq!(f.error.len(), MAX_IMPORT_FAILURE_ERROR_BYTES);
    }

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
            field: String::new(),
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
            ..Default::default()
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
            options: None,
            judgments: Vec::new(),
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
            max_import_chunk: 0,
        };
        let svc = AssessmentService::in_memory().with_limits(limits);
        assert_eq!(svc.limits().max_batch_size, 1);
        assert_eq!(svc.limits().max_in_flight, 1);
        assert_eq!(svc.limits().max_concurrent_assessments, 1);
        assert_eq!(svc.limits().max_import_chunk, 1);
        assert_eq!(svc.stateless_slots.available_permits(), 1);
        assert_eq!(svc.registry_slots.available_permits(), 1);

        limits.max_in_flight = usize::MAX;
        limits.max_concurrent_assessments = usize::MAX;
        limits.max_import_chunk = usize::MAX;
        let svc = AssessmentService::in_memory().with_limits(limits);
        assert_eq!(svc.limits().max_import_chunk, MAX_IMPORT_CHUNK);
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

    // -----------------------------------------------------------------
    // A batch with a judge
    // -----------------------------------------------------------------

    use std::sync::Mutex;

    use assessment_core::model::TextRelation;
    use assessment_judge::{BoxFuture, JudgeError, Question, Verdict};

    /// A judge for the batch tests. It records the deadline of each
    /// question. With `cached`, every answer is already known (`peek`), and
    /// the first `peek` takes every permit of `pool`: that is after the
    /// first passes and before the second ones.
    #[derive(Debug, Default)]
    struct BatchJudge {
        cached: bool,
        pool: Mutex<Option<(Arc<Semaphore>, u32)>>,
        taken: Mutex<Option<OwnedSemaphorePermit>>,
        deadlines: Mutex<Vec<Instant>>,
    }

    impl BatchJudge {
        fn verdict() -> Verdict {
            Verdict::new(
                TextRelation::Equivalent,
                0.9,
                "the same requirement",
                "fake",
            )
        }
    }

    impl Judge for BatchJudge {
        fn id(&self) -> &str {
            "fake/batch"
        }

        fn judge<'a>(
            &'a self,
            _question: &'a Question,
            deadline: Instant,
        ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
            Box::pin(async move {
                self.deadlines.lock().expect("not poisoned").push(deadline);
                Ok(Self::verdict())
            })
        }

        fn peek(&self, _question: &Question) -> Option<Verdict> {
            if !self.cached {
                return None;
            }
            if let Some((pool, size)) = self.pool.lock().expect("not poisoned").take() {
                let all = pool
                    .try_acquire_many_owned(size)
                    .expect("the first passes gave their slots back");
                *self.taken.lock().expect("not poisoned") = Some(all);
            }
            Some(Self::verdict())
        }
    }

    /// A request whose description is a paraphrase: the engine flags it.
    fn flagged_request(id: &str) -> pb::AssessRequest {
        let before = Entity::new(id, "issue")
            .with_title("Sign-in")
            .with_description("Users can log in with their email address and password.")
            .with_version(SemVer::new(1, 0, 0));
        let after = before
            .clone()
            .with_description("People are able to sign in using e-mail and a password.");
        let mut request = pb::AssessRequest::from(AssessmentRequest {
            changes: vec![ChangeSet::update(before, after)],
            ..Default::default()
        });
        request.request_id = id.to_owned();
        request
    }

    fn flagged_batch(n: usize) -> pb::AssessBatchRequest {
        pb::AssessBatchRequest {
            requests: (0..n).map(|i| flagged_request(&format!("F-{i}"))).collect(),
        }
    }

    fn judged_service(judge: &Arc<BatchJudge>) -> AssessmentService {
        let judge: Arc<dyn Judge> = Arc::clone(judge) as Arc<dyn Judge>;
        service(1, 1).with_judge(judge, JudgeSettings::default())
    }

    /// The first passes of a batch larger than `max_in_flight` run mostly
    /// while the items are still being started. That time counts: the judge
    /// must leave twice the first pass for the second one.
    #[tokio::test]
    async fn a_batch_reserves_its_whole_first_pass_for_the_second() {
        const HELD: Duration = Duration::from_secs(1);
        const CALL: Duration = Duration::from_secs(10);
        let judge = Arc::new(BatchJudge::default());
        let svc = judged_service(&judge);

        // The only slot is busy for a second, so the first pass of the
        // first item takes that long. The second item is started after it.
        let held = take_all(&svc.stateless_slots, 1).await;
        tokio::spawn(async move {
            tokio::time::sleep(HELD).await;
            drop(held);
        });
        let mut request = Request::new(flagged_batch(2));
        request
            .metadata_mut()
            .insert("grpc-timeout", "10S".parse().expect("a valid value"));
        let start = Instant::now();
        let batch = done(svc.assess_batch(request))
            .await
            .expect("the batch succeeds")
            .into_inner();
        assert_eq!(batch.responses.len(), 2);

        let deadlines = judge.deadlines.lock().expect("not poisoned").clone();
        assert_eq!(deadlines.len(), 2);
        // The reserve is at least twice the held second. Measured after
        // the items were started, it was 1.25 s.
        let latest = start + CALL - HELD * 2;
        for deadline in deadlines {
            assert!(
                deadline < latest,
                "the judge got {:?} of a 10 s call",
                deadline - start
            );
        }
    }

    /// The second passes of a batch queue for slots `max_in_flight` at a
    /// time, like the first ones, so another caller waits for one item of
    /// the batch and not for all of them.
    #[tokio::test]
    async fn the_second_passes_of_a_batch_queue_max_in_flight_at_a_time() {
        let judge = Arc::new(BatchJudge {
            cached: true,
            ..BatchJudge::default()
        });
        let svc = judged_service(&judge);
        let slots = Arc::clone(&svc.stateless_slots);
        *judge.pool.lock().expect("not poisoned") = Some((Arc::clone(&slots), 1));

        let call = svc.clone();
        let mut batch =
            tokio::spawn(async move { call.assess_batch(Request::new(flagged_batch(3))).await });
        // The judge takes the only slot once the first passes are done.
        let taken = done(async {
            loop {
                if let Some(taken) = judge.taken.lock().expect("not poisoned").take() {
                    break taken;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        // The second passes queue for the slot; then another caller does.
        tokio::time::sleep(BLOCKED).await;
        let other = tokio::spawn(Arc::clone(&slots).acquire_owned());
        tokio::time::sleep(BLOCKED).await;
        drop(taken);

        // The other caller gets the slot after at most one second pass of
        // the batch (`max_in_flight` is 1), so the batch is not done yet.
        let other = done(other).await.expect("joins").expect("the pool is open");
        assert!(is_blocked(&mut batch).await);
        drop(other);
        let responses = done(batch)
            .await
            .expect("joins")
            .expect("the batch succeeds")
            .into_inner()
            .responses;
        assert_eq!(responses.len(), 3);
        for response in &responses {
            assert!(response.assessment.is_some() && response.error.is_none());
        }
        wait_until_full(&slots, 1).await;
    }
}
