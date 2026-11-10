//! An ergonomic client that speaks core types.
//!
//! [`AssessmentClient`] wraps the generated client: it takes and returns
//! [`assessment_core`] values ([`AssessmentRequest`] → [`Assessment`],
//! [`Entity`] + [`PutOptions`] → [`PutOutcome`], …), adds an optional
//! `authorization: Bearer …` header to every call, and reports failures as
//! one opaque [`ClientError`]. With [`AssessmentClient::with_timeout`] every
//! call also carries a deadline that the server sees.
//!
//! ```no_run
//! # async fn run() -> Result<(), assessment_grpc::client::ClientError> {
//! use assessment_core::prelude::*;
//! use assessment_grpc::client::AssessmentClient;
//!
//! let mut client = AssessmentClient::connect("http://127.0.0.1:50061")
//!     .await?
//!     .with_bearer_token("s3cret")?;
//! let before = Entity::new("ENG-1", "issue").with_title("Login");
//! let after = before.clone().with_title("Login with SSO only");
//! let assessment = client
//!     .assess(AssessmentRequest {
//!         changes: vec![ChangeSet::update(before, after)],
//!         ..Default::default()
//!     })
//!     .await?;
//! println!("{:?}", assessment.recommendation);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use assessment_core::model::{
    ActivityEvent, AssessOptions, AssessmentRequest, Entity, TextJudgment, field,
};
use assessment_core::policy::Policy;
use assessment_core::registry::{
    DeleteOptions, DeleteOutcome, ImportItem, ImportSummary, PutOptions, PutOutcome, StaleEntry,
    VersionRecord,
};
use assessment_core::semver::SemVer;
use assessment_core::text::TextDiff;
use assessment_core::{Assessment, ErrorKind};
use chrono::{DateTime, Utc};
use tokio_stream::{Stream, StreamExt};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

use crate::convert::{self, AssessInput, ConvertError};
use crate::pb;
use crate::pb::assessment_engine_client::AssessmentEngineClient;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong, coarsely. Match on it with a wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientErrorKind {
    /// The client was given something it cannot use: a malformed URL, a
    /// token that is not a valid header value, a policy patch that is not a
    /// JSON object.
    InvalidInput,
    /// The connection could not be made.
    Transport,
    /// The server answered with this status code.
    Status(tonic::Code),
    /// The server's answer does not describe valid core values.
    Decode,
}

/// The error every [`AssessmentClient`] call returns.
///
/// Its [`Display`](fmt::Display) says what failed, without the cause; walk
/// [`std::error::Error::source`] for it (a [`tonic::Status`] for
/// [`ClientErrorKind::Status`], see also [`ClientError::status`]).
#[derive(Debug)]
pub struct ClientError {
    kind: ClientErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl ClientError {
    /// What kind of failure this is.
    pub fn kind(&self) -> ClientErrorKind {
        self.kind
    }

    /// The status code, when the server answered with an error status.
    pub fn code(&self) -> Option<tonic::Code> {
        match self.kind {
            ClientErrorKind::Status(code) => Some(code),
            _ => None,
        }
    }

    /// The server's status (code, message, metadata), when it answered with one.
    pub fn status(&self) -> Option<&tonic::Status> {
        self.source.as_deref()?.downcast_ref::<tonic::Status>()
    }

    fn new(
        kind: ClientErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    fn from_status(call: &'static str, status: tonic::Status) -> Self {
        let code = status.code();
        Self::new(
            ClientErrorKind::Status(code),
            format!("the {call} call failed with status {code:?}"),
            status,
        )
    }

    fn decode(call: &'static str, err: ConvertError) -> Self {
        Self::new(
            ClientErrorKind::Decode,
            format!("the {call} response could not be decoded"),
            err,
        )
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// A per-request failure inside a batch or a stream (`AssessError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemError {
    kind: String,
    message: String,
}

impl ItemError {
    /// The machine-readable kind: `invalid_request`, `not_found`, `conflict`,
    /// `invalid_policy`, `storage` or `internal`.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The core error kind, when the kind names one.
    pub fn core_kind(&self) -> Option<ErrorKind> {
        match self.kind.as_str() {
            "invalid_request" => Some(ErrorKind::InvalidRequest),
            "not_found" => Some(ErrorKind::NotFound),
            "conflict" => Some(ErrorKind::Conflict),
            "invalid_policy" => Some(ErrorKind::InvalidPolicy),
            "storage" => Some(ErrorKind::Storage),
            _ => None,
        }
    }

    /// The server's message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ItemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ItemError {}

/// One response of a batch or a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemResponse {
    /// The request's id, echoed.
    pub request_id: String,
    /// The assessment, or why that request failed.
    pub result: Result<Assessment, ItemError>,
}

fn item_from_pb(call: &'static str, r: pb::AssessResponse) -> Result<ItemResponse, ClientError> {
    let result = match (r.assessment, r.error) {
        (_, Some(e)) => Err(ItemError {
            kind: e.kind,
            message: e.message,
        }),
        (Some(a), None) => Ok(Assessment::try_from(a)
            .map_err(|e| ClientError::decode(call, e.field("assessment")))?),
        (None, None) => {
            return Err(ClientError::decode(
                call,
                ConvertError::new("neither an assessment nor an error is set"),
            ));
        }
    };
    Ok(ItemResponse {
        request_id: r.request_id,
        result,
    })
}

/// What [`AssessmentClient::import`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// The counts, and the failed items the server listed (it lists at most
    /// the first 1 000).
    pub summary: ImportSummary,
    /// How many items failed in all; larger than `summary.failed.len()`
    /// when the server cut the list short.
    pub failed_total: u64,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Adds `authorization: Bearer <token>` to every call, when it has a token.
#[derive(Clone, Default)]
pub struct BearerToken {
    header: Option<MetadataValue<Ascii>>,
}

impl BearerToken {
    /// A token to send; fails when it is not a valid header value.
    pub fn new(token: &str) -> Result<Self, ClientError> {
        let header = format!("Bearer {token}").parse().map_err(|e| {
            ClientError::new(
                ClientErrorKind::InvalidInput,
                "the bearer token is not a valid header value",
                e,
            )
        })?;
        Ok(Self {
            header: Some(header),
        })
    }

    /// No token.
    pub fn none() -> Self {
        Self::default()
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the token.
        f.debug_struct("BearerToken")
            .field("set", &self.header.is_some())
            .finish()
    }
}

impl Interceptor for BearerToken {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(header) = &self.header {
            request
                .metadata_mut()
                .insert("authorization", header.clone());
        }
        Ok(request)
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// The generated client, as [`AssessmentClient`] drives it.
pub type RawClient = AssessmentEngineClient<InterceptedService<Channel, BearerToken>>;

/// A client of the assessment engine that speaks core types. Cheap to clone
/// (clones share the connection).
#[derive(Debug, Clone)]
pub struct AssessmentClient {
    inner: RawClient,
    channel: Channel,
    max_message_bytes: Option<usize>,
    timeout: Option<Duration>,
}

impl AssessmentClient {
    /// Connects to `url` (e.g. `http://127.0.0.1:50061`).
    pub async fn connect(url: impl Into<String>) -> Result<Self, ClientError> {
        let url = url.into();
        let endpoint = Endpoint::from_shared(url.clone()).map_err(|e| {
            ClientError::new(
                ClientErrorKind::InvalidInput,
                format!("`{url}` is not a valid endpoint url"),
                e,
            )
        })?;
        Self::connect_endpoint(endpoint).await
    }

    /// Connects to a configured endpoint (timeouts, TLS, keepalive …).
    pub async fn connect_endpoint(endpoint: Endpoint) -> Result<Self, ClientError> {
        let uri = endpoint.uri().to_string();
        let channel = endpoint.connect().await.map_err(|e| {
            ClientError::new(
                ClientErrorKind::Transport,
                format!("could not connect to `{uri}`"),
                e,
            )
        })?;
        Ok(Self::from_channel(channel))
    }

    /// A client over an existing channel.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            inner: AssessmentEngineClient::with_interceptor(channel.clone(), BearerToken::none()),
            channel,
            max_message_bytes: None,
            timeout: None,
        }
    }

    /// This client, sending `authorization: Bearer <token>` with every call.
    pub fn with_bearer_token(self, token: &str) -> Result<Self, ClientError> {
        let token = BearerToken::new(token)?;
        Ok(self.rebuild(token))
    }

    /// This client with a message-size limit for both directions (the
    /// server's default is 16 MiB; tonic's decoding default is 4 MiB).
    #[must_use]
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = Some(bytes);
        self.inner = self
            .inner
            .max_decoding_message_size(bytes)
            .max_encoding_message_size(bytes);
        self
    }

    fn rebuild(self, token: BearerToken) -> Self {
        let mut inner = AssessmentEngineClient::with_interceptor(self.channel.clone(), token);
        if let Some(bytes) = self.max_message_bytes {
            inner = inner
                .max_decoding_message_size(bytes)
                .max_encoding_message_size(bytes);
        }
        Self {
            inner,
            channel: self.channel,
            max_message_bytes: self.max_message_bytes,
            timeout: self.timeout,
        }
    }

    /// This client with a deadline for every call. It is sent to the server
    /// as `grpc-timeout`, so the server stops working on a call that the
    /// client has given up. A timeout set on the [`Endpoint`] is not sent.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// A request with this client's timeout, if it has one.
    fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Some(timeout) = self.timeout {
            request.set_timeout(timeout);
        }
        request
    }

    /// The generated client, for calls in protobuf terms.
    pub fn raw(&mut self) -> &mut RawClient {
        &mut self.inner
    }

    /// Assesses one set of changes.
    pub async fn assess(&mut self, request: AssessmentRequest) -> Result<Assessment, ClientError> {
        self.assess_input(AssessInput {
            request_id: String::new(),
            request,
            policy_overrides: None,
        })
        .await
    }

    /// Assesses one set of changes with a partial policy (a JSON object)
    /// merged over the server's for this request only.
    pub async fn assess_with_policy(
        &mut self,
        request: AssessmentRequest,
        policy_overrides: &serde_json::Value,
    ) -> Result<Assessment, ClientError> {
        let overrides = policy_object(policy_overrides)?;
        self.assess_input(AssessInput {
            request_id: String::new(),
            request,
            policy_overrides: Some(overrides),
        })
        .await
    }

    async fn assess_input(&mut self, input: AssessInput) -> Result<Assessment, ClientError> {
        let request = self.request(pb::AssessRequest::from(input));
        let response = self
            .inner
            .assess(request)
            .await
            .map_err(|s| ClientError::from_status("assess", s))?
            .into_inner();
        let item = item_from_pb("assess", response)?;
        item.result.map_err(|e| {
            ClientError::new(
                ClientErrorKind::Decode,
                "the assess response carries an item error",
                e,
            )
        })
    }

    /// Assesses independent requests; each one succeeds or fails on its own.
    /// Responses are in request order, with request ids `"0"`, `"1"`, ….
    pub async fn assess_batch(
        &mut self,
        requests: Vec<AssessmentRequest>,
    ) -> Result<Vec<ItemResponse>, ClientError> {
        let requests = requests
            .into_iter()
            .enumerate()
            .map(|(i, request)| {
                let mut pb = pb::AssessRequest::from(request);
                pb.request_id = i.to_string();
                pb
            })
            .collect();
        let request = self.request(pb::AssessBatchRequest { requests });
        let response = self
            .inner
            .assess_batch(request)
            .await
            .map_err(|s| ClientError::from_status("assess_batch", s))?
            .into_inner();
        response
            .responses
            .into_iter()
            .map(|r| item_from_pb("assess_batch", r))
            .collect()
    }

    /// Streams `(request_id, request)` pairs to the server; the responses
    /// come back in the same order, each echoing its request id.
    pub async fn assess_stream<S>(&mut self, requests: S) -> Result<AssessResponses, ClientError>
    where
        S: Stream<Item = (String, AssessmentRequest)> + Send + 'static,
    {
        let outbound = requests.map(|(request_id, request)| {
            let mut pb = pb::AssessRequest::from(request);
            pb.request_id = request_id;
            pb
        });
        let request = self.request(outbound);
        let inbound = self
            .inner
            .assess_stream(request)
            .await
            .map_err(|s| ClientError::from_status("assess_stream", s))?
            .into_inner();
        Ok(AssessResponses { inner: inbound })
    }

    /// Compares two texts; `embeddings` are both sides' vectors, if any.
    /// The texts are read as a description.
    pub async fn compare_text(
        &mut self,
        before: impl Into<String>,
        after: impl Into<String>,
        embeddings: Option<(Vec<f32>, Vec<f32>)>,
    ) -> Result<TextDiff, ClientError> {
        self.compare_text_as(field::DESCRIPTION, before, after, embeddings)
            .await
    }

    /// [`compare_text`](Self::compare_text) for the text of `field`. The
    /// result's `gray_zone` says why an assessment would flag the change
    /// for a judgment in that field, when it would. No judge is asked.
    pub async fn compare_text_as(
        &mut self,
        field: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
        embeddings: Option<(Vec<f32>, Vec<f32>)>,
    ) -> Result<TextDiff, ClientError> {
        let (before_embedding, after_embedding) = embeddings.unwrap_or_default();
        let request = self.request(pb::CompareTextRequest {
            before: before.into(),
            after: after.into(),
            before_embedding,
            after_embedding,
            field: field.into(),
        });
        let response = self
            .inner
            .compare_text(request)
            .await
            .map_err(|s| ClientError::from_status("compare_text", s))?
            .into_inner();
        TextDiff::try_from(response).map_err(|e| ClientError::decode("compare_text", e))
    }

    /// The server's effective policy, with `overrides` (a JSON object)
    /// merged over it when given.
    pub async fn get_policy(
        &mut self,
        overrides: Option<&serde_json::Value>,
    ) -> Result<Policy, ClientError> {
        let overrides = overrides
            .map(policy_object)
            .transpose()?
            .map(convert::map_to_struct);
        let request = self.request(pb::GetPolicyRequest { overrides });
        let response = self
            .inner
            .get_policy(request)
            .await
            .map_err(|s| ClientError::from_status("get_policy", s))?
            .into_inner();
        serde_json::from_str(&response.policy_json).map_err(|e| {
            ClientError::new(
                ClientErrorKind::Decode,
                "the get_policy response could not be decoded",
                e,
            )
        })
    }

    /// The id of the server's judge, such as a model name. `None` when the
    /// server has no judge: it then only reports flagged fields.
    pub async fn judge_info(&mut self) -> Result<Option<String>, ClientError> {
        let request = self.request(pb::GetPolicyRequest { overrides: None });
        let response = self
            .inner
            .get_policy(request)
            .await
            .map_err(|s| ClientError::from_status("get_policy", s))?
            .into_inner();
        Ok(Some(response.judge).filter(|judge| !judge.is_empty()))
    }

    /// Creates or updates an entity in the server's registry.
    ///
    /// With [`PutOptions::source_seq`], a put that is not newer than what
    /// the server holds is skipped; see [`PutOutcome::skipped_stale`].
    pub async fn put_entity(
        &mut self,
        entity: Entity,
        options: PutOptions,
    ) -> Result<PutOutcome, ClientError> {
        let request = self.request(convert::put_request_to_pb(entity, options));
        let response = self
            .inner
            .put_entity(request)
            .await
            .map_err(|s| ClientError::from_status("put_entity", s))?
            .into_inner();
        PutOutcome::try_from(response).map_err(|e| ClientError::decode("put_entity", e))
    }

    /// Deletes an entity (unless `dry_run`) and returns what the deletion
    /// leaves behind.
    pub async fn delete_entity(
        &mut self,
        id: impl Into<String>,
        dry_run: bool,
        expected_version: Option<SemVer>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment, ClientError> {
        let request = self.request(pb::DeleteEntityRequest {
            id: id.into(),
            dry_run,
            expected_version: expected_version.map(Into::into),
            now: now.map(convert::timestamp_to_pb),
            source_seq: None,
        });
        let response = self
            .inner
            .delete_entity(request)
            .await
            .map_err(|s| ClientError::from_status("delete_entity", s))?
            .into_inner();
        convert::assessment_field(response.assessment)
            .map_err(|e| ClientError::decode("delete_entity", e))
    }

    /// Like [`delete_entity`](Self::delete_entity), with every option
    /// (among them [`DeleteOptions::source_seq`]) and the full outcome.
    pub async fn delete_entity_with(
        &mut self,
        id: impl Into<String>,
        options: DeleteOptions,
    ) -> Result<DeleteOutcome, ClientError> {
        let request = self.request(convert::delete_request_to_pb(id, options));
        let response = self
            .inner
            .delete_entity(request)
            .await
            .map_err(|s| ClientError::from_status("delete_entity", s))?
            .into_inner();
        DeleteOutcome::try_from(response).map_err(|e| ClientError::decode("delete_entity", e))
    }

    /// Streams `items` to the server's registry, which applies them in
    /// order: new entities are stored without an assessment, unchanged ones
    /// are skipped, changed ones are assessed like a put, and deletions are
    /// applied.
    ///
    /// An item that fails is reported in the summary and does not stop the
    /// rest. If the call itself fails, the items the server applied before
    /// that stay applied; sending the same items again is safe.
    pub async fn import<S>(&mut self, items: S) -> Result<ImportReport, ClientError>
    where
        S: Stream<Item = ImportItem> + Send + 'static,
    {
        let request = self.request(items.map(pb::ImportRequest::from));
        let response = self
            .inner
            .import(request)
            .await
            .map_err(|s| ClientError::from_status("import", s))?
            .into_inner();
        Ok(ImportReport {
            failed_total: response.failed_total,
            summary: response.into(),
        })
    }

    /// [`import`](Self::import) for items that are already in memory.
    pub async fn import_iter<I>(&mut self, items: I) -> Result<ImportReport, ClientError>
    where
        I: IntoIterator<Item = ImportItem>,
        I::IntoIter: Send + 'static,
    {
        self.import(tokio_stream::iter(items)).await
    }

    /// Assesses activity (comments, …) on a stored entity.
    pub async fn record_activity(
        &mut self,
        id: impl Into<String>,
        events: Vec<ActivityEvent>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment, ClientError> {
        let request = self.request(pb::RecordActivityRequest {
            entity_id: id.into(),
            events: events.into_iter().map(Into::into).collect(),
            now: now.map(convert::timestamp_to_pb),
        });
        let response = self
            .inner
            .record_activity(request)
            .await
            .map_err(|s| ClientError::from_status("record_activity", s))?
            .into_inner();
        convert::assessment_field(response.assessment)
            .map_err(|e| ClientError::decode("record_activity", e))
    }

    /// The current entity, or the snapshot at `version`.
    pub async fn get_entity(
        &mut self,
        id: impl Into<String>,
        version: Option<SemVer>,
    ) -> Result<Entity, ClientError> {
        let request = self.request(pb::GetEntityRequest {
            id: id.into(),
            version: version.map(Into::into),
        });
        let response = self
            .inner
            .get_entity(request)
            .await
            .map_err(|s| ClientError::from_status("get_entity", s))?
            .into_inner();
        convert::entity_field(response.entity).map_err(|e| ClientError::decode("get_entity", e))
    }

    /// The version history, oldest first; with `limit > 0`, only the newest
    /// `limit` records.
    pub async fn history(
        &mut self,
        id: impl Into<String>,
        limit: u32,
    ) -> Result<Vec<VersionRecord>, ClientError> {
        let request = self.request(pb::GetHistoryRequest {
            id: id.into(),
            limit,
        });
        let response = self
            .inner
            .get_history(request)
            .await
            .map_err(|s| ClientError::from_status("get_history", s))?
            .into_inner();
        convert::decode_list(response.records, "records", VersionRecord::try_from)
            .map_err(|e| ClientError::decode("get_history", e))
    }

    /// Assesses the difference between two stored versions of an entity.
    /// The judgments that were applied when `to` was stored are used again.
    pub async fn compare_versions(
        &mut self,
        id: impl Into<String>,
        from: SemVer,
        to: SemVer,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment, ClientError> {
        self.compare_versions_with(id, from, to, now, AssessOptions::default(), Vec::new())
            .await
    }

    /// [`compare_versions`](Self::compare_versions) with assessment options
    /// and judgments of the text changes between the two versions. A given
    /// judgment of a field goes before the stored one.
    pub async fn compare_versions_with(
        &mut self,
        id: impl Into<String>,
        from: SemVer,
        to: SemVer,
        now: Option<DateTime<Utc>>,
        options: AssessOptions,
        judgments: Vec<TextJudgment>,
    ) -> Result<Assessment, ClientError> {
        let request = self.request(pb::CompareVersionsRequest {
            id: id.into(),
            from: Some(from.into()),
            to: Some(to.into()),
            now: now.map(convert::timestamp_to_pb),
            options: Some(options.into()),
            judgments: judgments.into_iter().map(Into::into).collect(),
        });
        let response = self
            .inner
            .compare_versions(request)
            .await
            .map_err(|s| ClientError::from_status("compare_versions", s))?
            .into_inner();
        convert::assessment_field(response.assessment)
            .map_err(|e| ClientError::decode("compare_versions", e))
    }

    /// Plans that are behind what they were planned against: under `root`
    /// (and `root` itself), or everywhere.
    pub async fn stale(&mut self, root: Option<&str>) -> Result<Vec<StaleEntry>, ClientError> {
        let request = self.request(pb::ListStaleRequest {
            root_id: root.unwrap_or_default().to_owned(),
        });
        let response = self
            .inner
            .list_stale(request)
            .await
            .map_err(|s| ClientError::from_status("list_stale", s))?
            .into_inner();
        convert::decode_list(response.entries, "entries", StaleEntry::try_from)
            .map_err(|e| ClientError::decode("list_stale", e))
    }

    /// Re-pins an entity to the current versions of what it was planned against.
    pub async fn acknowledge(
        &mut self,
        id: impl Into<String>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Entity, ClientError> {
        let request = self.request(pb::AcknowledgeRequest {
            id: id.into(),
            now: now.map(convert::timestamp_to_pb),
        });
        let response = self
            .inner
            .acknowledge(request)
            .await
            .map_err(|s| ClientError::from_status("acknowledge", s))?
            .into_inner();
        convert::entity_field(response.entity).map_err(|e| ClientError::decode("acknowledge", e))
    }
}

fn policy_object(
    value: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, ClientError> {
    match value {
        serde_json::Value::Object(map) => Ok(map.clone()),
        _ => Err(ClientError {
            kind: ClientErrorKind::InvalidInput,
            message: "a policy patch must be a json object".to_owned(),
            source: None,
        }),
    }
}

/// The responses of [`AssessmentClient::assess_stream`], in request order.
/// A [`Stream`]; or call [`AssessResponses::message`].
#[derive(Debug)]
pub struct AssessResponses {
    inner: tonic::Streaming<pb::AssessResponse>,
}

impl AssessResponses {
    /// The next response, or `None` when the server ended the stream.
    pub async fn message(&mut self) -> Result<Option<ItemResponse>, ClientError> {
        match self.inner.message().await {
            Ok(Some(r)) => item_from_pb("assess_stream", r).map(Some),
            Ok(None) => Ok(None),
            Err(status) => Err(ClientError::from_status("assess_stream", status)),
        }
    }
}

impl Stream for AssessResponses {
    type Item = Result<ItemResponse, ClientError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx).map(|next| {
            next.map(|result| match result {
                Ok(r) => item_from_pb("assess_stream", r),
                Err(status) => Err(ClientError::from_status("assess_stream", status)),
            })
        })
    }
}
