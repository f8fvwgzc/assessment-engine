//! End-to-end tests: the real server (health, reflection, auth, limits) on
//! `127.0.0.1:0`, exercised through [`AssessmentClient`], plus core → pb →
//! core round-trip properties.

use std::time::Duration;

use assessment_core::assessment::{
    Action, Assessment, BasisStatus, ChangeAssessment, ChangeType, Channel, Contribution,
    Direction, EntityImpact, FieldAssessment, FieldChangeKind, FieldJudgment, ImpactLevel,
    JudgmentKey, JudgmentReason, JudgmentRequest, JudgmentStatus, Modifier, Recommendation, Signal,
    SignalCategory, Violation, ViolationKind,
};
use assessment_core::model::field::DESCRIPTION;
use assessment_core::model::{
    ActivityEvent, ActivityKind, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity,
    EntityKind, FieldChange, FieldValue, HistoryPoint, Relation, RelationKind, StatusCategory,
    TextJudgment, TextRelation,
};
use assessment_core::policy::Policy;
use assessment_core::registry::{
    DeleteOptions, DeleteOutcome, ImportFailure, ImportItem, ImportSummary, PutOptions, PutOutcome,
    StaleEntry, VersionRecord,
};
use assessment_core::semver::{SemVer, VersionBump};
use assessment_core::text::{GrayZone, StructureDiff, TextDiff, fingerprint};
use assessment_core::{Engine, ErrorKind};
use assessment_grpc::auth::BearerAuth;
use assessment_grpc::client::{AssessmentClient, ClientErrorKind, ItemResponse};
use assessment_grpc::convert::{self, AssessInput};
use assessment_grpc::pb;
use assessment_grpc::server::{ServeError, ServerOptions, serve_with_incoming_shutdown};
use assessment_grpc::service::{
    AssessmentService, MAX_IMPORT_FAILURE_ERROR_BYTES, MAX_IMPORT_FAILURE_ID_BYTES,
    MAX_IMPORT_FAILURES, ServiceLimits,
};
use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::Code;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestServer {
    url: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

impl TestServer {
    async fn start(options: ServerOptions) -> Self {
        Self::start_with(AssessmentService::in_memory(), options).await
    }

    async fn start_with(service: AssessmentService, options: ServerOptions) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a free port");
        let addr = listener.local_addr().expect("has an address");
        let incoming = TcpListenerStream::new(listener);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_with_incoming_shutdown(
            service,
            options,
            incoming,
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            url: format!("http://{addr}"),
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn client(&self) -> AssessmentClient {
        AssessmentClient::connect(self.url.clone())
            .await
            .expect("connects")
    }

    /// A client whose HTTP/2 stream window is the 64 KB minimum, so a
    /// response stream it does not read soon stops the server's writer.
    async fn small_window_client(&self) -> AssessmentClient {
        let endpoint = tonic::transport::Endpoint::from_shared(self.url.clone())
            .expect("a valid url")
            .initial_stream_window_size(65_535);
        AssessmentClient::connect_endpoint(endpoint)
            .await
            .expect("connects")
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("shuts down in time")
                .expect("the server task does not panic");
            result.expect("serves without error");
        }
    }
}

/// The longest an end-to-end test of the slot pools may take. A deadlock
/// fails the test instead of hanging the run.
const E2E_LIMIT: Duration = Duration::from_secs(120);

/// Runs `body`, and fails if it takes longer than `limit`.
async fn within<F: Future>(limit: Duration, body: F) -> F::Output {
    tokio::time::timeout(limit, body)
        .await
        .expect("the test finishes in time (a longer wait means a deadlock)")
}

/// An in-memory service with `slots` assessment slots per pool and
/// `in_flight` items per batch or stream.
fn slot_service(slots: usize, in_flight: usize) -> AssessmentService {
    AssessmentService::in_memory().with_limits(
        assessment_grpc::service::ServiceLimits::default()
            .with_max_concurrent_assessments(slots)
            .with_max_in_flight(in_flight),
    )
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0)
        .single()
        .expect("a valid date")
}

const PARENT_BEFORE: &str = "Customers sign in to the portal.\n\n\
    ## Acceptance criteria\n\
    - [ ] Sign up and log in with email and password\n\
    - [ ] Password reset by email link\n\
    - [ ] Sessions expire after 30 days\n\
    - [ ] Rate-limit failed logins";

const PARENT_AFTER: &str = "Customers sign in to the portal with their company identity.\n\n\
    ## Acceptance criteria\n\
    - [ ] Log in with Google Workspace (OAuth 2.0 / OIDC)\n\
    - [ ] Log in with Microsoft Entra ID (OIDC)\n\
    - [ ] Sessions expire after 7 days\n\
    - [ ] Rate-limit failed logins\n\n\
    Password login is no longer needed.";

fn parent() -> Entity {
    Entity::new("ENG-12", "issue")
        .with_title("Authentication for the customer portal")
        .with_description(PARENT_BEFORE)
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(13.0)
        .with_due(at(28, 0))
}

fn sub_issue(id: &str, title: &str, description: &str, category: StatusCategory) -> Entity {
    Entity::new(id, "sub_issue")
        .with_parent("ENG-12")
        .with_title(title)
        .with_description(description)
        .with_status(category, "State")
        .with_estimate(3.0)
}

fn sub_issues() -> Vec<Entity> {
    vec![
        sub_issue(
            "ENG-13",
            "Email and password sign-up form",
            "Form with email, password and password confirmation; hash passwords with argon2.",
            StatusCategory::Started,
        )
        .with_basis("ENG-12", SemVer::new(1, 2, 0)),
        sub_issue(
            "ENG-14",
            "Password reset email flow",
            "Send a reset link by email; the token expires after one hour.",
            StatusCategory::Unstarted,
        )
        .with_basis("ENG-12", SemVer::new(1, 2, 0)),
    ]
}

/// The parent issue's description is rewritten while a sub-issue is in progress.
fn rewrite_request() -> AssessmentRequest {
    let before = parent().with_version(SemVer::new(1, 2, 0));
    let after = before.clone().with_description(PARENT_AFTER);
    AssessmentRequest {
        changes: vec![ChangeSet::update(before, after)],
        context: sub_issues(),
        now: Some(at(7, 9)),
        ..Default::default()
    }
}

fn simple_request(id: &str, title_after: &str) -> AssessmentRequest {
    let before = Entity::new(id, "issue")
        .with_title("Export invoices as CSV")
        .with_version(SemVer::new(1, 0, 0));
    let after = before.clone().with_title(title_after);
    AssessmentRequest {
        changes: vec![ChangeSet::update(before, after)],
        now: Some(at(7, 9)),
        ..Default::default()
    }
}

fn unspecified_status_request(request_id: &str) -> pb::AssessRequest {
    let mut request = pb::AssessRequest::from(simple_request("ENG-1", "Export invoices"));
    request.request_id = request_id.to_owned();
    let after = request.changes[0]
        .after
        .as_mut()
        .expect("an update has an after");
    after.fields.insert(
        "status".to_owned(),
        pb::FieldValue {
            kind: Some(pb::field_value::Kind::Status(pb::Status {
                category: pb::StatusCategory::Unspecified.into(),
                name: "Mystery".to_owned(),
            })),
        },
    );
    request
}

// ---------------------------------------------------------------------------
// Stateless calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assess_parent_rewrite_is_major_and_affects_sub_issues() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let assessment = client.assess(rewrite_request()).await.expect("assesses");

    let change = &assessment.changes[0];
    assert_eq!(change.entity_id, "ENG-12");
    assert_eq!(change.bump, VersionBump::Major);
    assert_eq!(change.next_version, Some(SemVer::new(2, 0, 0)));
    let child = assessment
        .impacts
        .iter()
        .find(|i| !i.changed && i.entity_id.starts_with("ENG-1") && i.entity_id != "ENG-12")
        .expect("a sub-issue is affected");
    assert!(child.score > 0.0);
    assert_eq!(child.basis_status, BasisStatus::Incompatible);
    assert!(assessment.level >= ImpactLevel::Medium);

    // The wire loses nothing: the same request assessed in-process is identical.
    let direct = Engine::default()
        .assess(&rewrite_request())
        .expect("assesses");
    assert_eq!(assessment, direct);

    server.shutdown().await;
}

#[tokio::test]
async fn assess_batch_reports_item_errors_and_keeps_going() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let responses = client
        .assess_batch(vec![
            simple_request("ENG-1", "Export invoices as CSV and PDF"),
            AssessmentRequest::default(), // no changes: invalid
            rewrite_request(),
        ])
        .await
        .expect("the batch is answered");
    assert_eq!(responses.len(), 3);
    let ids: Vec<&str> = responses.iter().map(|r| r.request_id.as_str()).collect();
    assert_eq!(ids, ["0", "1", "2"]);
    assert!(responses[0].result.is_ok());
    let err = responses[1]
        .result
        .as_ref()
        .expect_err("the empty request fails");
    assert_eq!(err.kind(), "invalid_request");
    assert_eq!(err.core_kind(), Some(ErrorKind::InvalidRequest));
    let third = responses[2].result.as_ref().expect("the rewrite succeeds");
    assert_eq!(third.changes[0].bump, VersionBump::Major);

    // A request the server cannot even decode fails alone, with its path.
    let raw = client
        .raw()
        .assess_batch(pb::AssessBatchRequest {
            requests: vec![
                unspecified_status_request("bad"),
                pb::AssessRequest {
                    request_id: "good".to_owned(),
                    ..pb::AssessRequest::from(simple_request("ENG-2", "Export"))
                },
            ],
        })
        .await
        .expect("the batch is answered")
        .into_inner();
    let bad = raw.responses[0].error.as_ref().expect("an item error");
    assert_eq!(raw.responses[0].request_id, "bad");
    assert_eq!(bad.kind, "invalid_request");
    assert_eq!(
        bad.message,
        "changes[0].after.fields[\"status\"]: status category is unspecified"
    );
    assert!(raw.responses[1].assessment.is_some());
    assert_eq!(raw.responses[1].request_id, "good");

    server.shutdown().await;
}

#[tokio::test]
async fn assess_batch_size_is_limited() {
    let service = AssessmentService::in_memory()
        .with_limits(assessment_grpc::service::ServiceLimits::default().with_max_batch_size(2));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_with_incoming_shutdown(
        service,
        ServerOptions::default(),
        TcpListenerStream::new(listener),
        async {
            let _ = stopped.await;
        },
    ));
    let mut client = AssessmentClient::connect(url).await.expect("connects");
    let err = client
        .assess_batch(vec![simple_request("A", "x"); 3])
        .await
        .expect_err("too many");
    assert_eq!(err.code(), Some(Code::InvalidArgument));
    let _ = stop.send(());
    let _ = task.await;
}

/// With a single assessment slot, concurrent unary calls, a batch and a
/// stream all still complete, with correct results: per-call permits and the
/// service-wide slot never wait on each other in a cycle.
#[tokio::test]
async fn one_assessment_slot_serves_concurrent_calls() {
    within(
        E2E_LIMIT,
        one_assessment_slot_serves_concurrent_calls_body(),
    )
    .await;
}

async fn one_assessment_slot_serves_concurrent_calls_body() {
    let server = TestServer::start_with(slot_service(1, 3), ServerOptions::default()).await;
    let client = server.client().await;

    let unary = (0..6).map(|i| {
        let mut client = client.clone();
        tokio::spawn(async move {
            client
                .assess(simple_request(&format!("U-{i}"), &format!("Export v{i}")))
                .await
        })
    });
    let batch = {
        let mut client = client.clone();
        tokio::spawn(async move {
            let requests = (0..8)
                .map(|i| simple_request(&format!("B-{i}"), &format!("Import v{i}")))
                .collect();
            client.assess_batch(requests).await
        })
    };
    let stream = {
        let mut client = client.clone();
        tokio::spawn(async move {
            let requests: Vec<(String, AssessmentRequest)> = (0..8)
                .map(|i| (format!("s-{i}"), rewrite_request()))
                .collect();
            client
                .assess_stream(tokio_stream::iter(requests))
                .await
                .expect("the stream opens")
                .collect::<Result<Vec<_>, _>>()
                .await
        })
    };

    for (i, handle) in unary.enumerate() {
        let a = handle
            .await
            .expect("the task runs")
            .expect("the call succeeds");
        assert_eq!(a.changes[0].entity_id, format!("U-{i}"));
    }
    let batch = batch
        .await
        .expect("the task runs")
        .expect("the batch succeeds");
    assert_eq!(batch.len(), 8);
    assert!(batch.iter().all(|r| r.result.is_ok()));
    let stream = stream
        .await
        .expect("the task runs")
        .expect("every response arrives");
    assert_eq!(stream.len(), 8);
    for (i, r) in stream.iter().enumerate() {
        assert_eq!(r.request_id, format!("s-{i}"));
        let a = r.result.as_ref().expect("succeeds");
        assert_eq!(a.changes[0].bump, VersionBump::Major);
    }
    server.shutdown().await;
}

/// A stream whose client sends many requests and reads nothing must not
/// keep the only assessment slot: a unary call made meanwhile completes.
/// The stream then still delivers every response, in order. A stream the
/// client drops unread gives its queue places back too.
#[tokio::test]
async fn an_unread_stream_does_not_starve_unary_calls() {
    within(
        E2E_LIMIT,
        an_unread_stream_does_not_starve_unary_calls_body(),
    )
    .await;
}

async fn an_unread_stream_does_not_starve_unary_calls_body() {
    // About 830 KB of responses per stream: far more than the client's
    // 64 KB stream window, so the server's writer really is held back.
    const ITEMS: usize = 300;
    // No request timeout: nothing but the service itself may free a slot.
    let options = ServerOptions::default().with_request_timeout(None);
    let server = TestServer::start_with(slot_service(1, 2), options).await;
    let mut client = server.small_window_client().await;
    let requests = |prefix: &'static str| -> Vec<(String, AssessmentRequest)> {
        (0..ITEMS)
            .map(|i| (format!("{prefix}-{i}"), rewrite_request()))
            .collect()
    };

    // Opened, never read (until later).
    let mut unread = client
        .assess_stream(tokio_stream::iter(requests("s")))
        .await
        .expect("the stream opens");
    // Opened, never read, then dropped.
    let dropped = client
        .assess_stream(tokio_stream::iter(requests("d")))
        .await
        .expect("the stream opens");

    for i in 0..4 {
        let a = client
            .assess(simple_request(&format!("U-{i}"), "Export invoices as PDF"))
            .await
            .expect("the unary call completes while the streams are unread");
        assert_eq!(a.changes[0].entity_id, format!("U-{i}"));
    }
    drop(dropped);
    let batch = client
        .assess_batch(vec![simple_request("B", "Export invoices as PDF"); 5])
        .await
        .expect("the batch completes after a stream was dropped");
    assert_eq!(batch.len(), 5);

    let mut seen = 0;
    while let Some(response) = unread.next().await {
        let response = response.expect("the response arrives");
        assert_eq!(response.request_id, format!("s-{seen}"));
        let a = response.result.as_ref().expect("succeeds");
        assert_eq!(a.changes[0].bump, VersionBump::Major);
        seen += 1;
    }
    assert_eq!(seen, ITEMS);
    client
        .assess(simple_request("U-last", "Export invoices as PDF"))
        .await
        .expect("the slot is free again");
    server.shutdown().await;
}

#[tokio::test]
async fn assess_stream_preserves_order_and_echoes_request_ids() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let requests: Vec<(String, AssessmentRequest)> = (0..12)
        .map(|i| {
            let request = match i % 4 {
                0 => rewrite_request(),
                1 => AssessmentRequest::default(), // invalid
                _ => simple_request(&format!("ENG-{i}"), &format!("Export invoices v{i}")),
            };
            (format!("req-{i}"), request)
        })
        .collect();
    let responses: Vec<ItemResponse> = client
        .assess_stream(tokio_stream::iter(requests))
        .await
        .expect("the stream opens")
        .collect::<Result<Vec<_>, _>>()
        .await
        .expect("every response arrives");

    assert_eq!(responses.len(), 12);
    for (i, response) in responses.iter().enumerate() {
        assert_eq!(response.request_id, format!("req-{i}"));
        match i % 4 {
            0 => {
                let a = response.result.as_ref().expect("the rewrite succeeds");
                assert_eq!(a.changes[0].bump, VersionBump::Major);
            }
            1 => {
                let e = response
                    .result
                    .as_ref()
                    .expect_err("the empty request fails");
                assert_eq!(e.kind(), "invalid_request");
            }
            _ => {
                let a = response.result.as_ref().expect("succeeds");
                assert_eq!(a.changes[0].entity_id, format!("ENG-{i}"));
            }
        }
    }

    server.shutdown().await;
}

#[tokio::test]
async fn compare_text_matches_the_core() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let diff = client
        .compare_text(PARENT_BEFORE, PARENT_AFTER, None)
        .await
        .expect("compares");
    assert!(diff.magnitude > 0.3);
    assert!(diff.structure.items_added + diff.structure.items_removed > 0);
    assert_eq!(
        diff,
        Engine::default()
            .compare_text(PARENT_BEFORE, PARENT_AFTER, None)
            .expect("valid texts")
    );

    let with_embeddings = client
        .compare_text(
            "same",
            "same",
            Some((vec![1.0, 0.0, 0.5], vec![1.0, 0.0, 0.5])),
        )
        .await
        .expect("compares");
    let cosine = with_embeddings.embedding_cosine.expect("both embeddings");
    assert!((cosine - 1.0).abs() < 1e-6);

    let err = client
        .compare_text("a", "b", Some((vec![f32::NAN], vec![1.0])))
        .await
        .expect_err("non-finite embedding");
    assert_eq!(err.code(), Some(Code::InvalidArgument));

    server.shutdown().await;
}

#[tokio::test]
async fn get_policy_applies_overrides() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let policy = client.get_policy(None).await.expect("reads the policy");
    assert_eq!(policy, Policy::default());

    // `max_depth` is an integer: Struct numbers are doubles on the wire.
    let overrides = serde_json::json!({
        "thresholds": {"high": 0.4},
        "propagation": {"max_depth": 3},
    });
    let patched = client
        .get_policy(Some(&overrides))
        .await
        .expect("reads the patched policy");
    assert!((patched.thresholds.high - 0.4).abs() < f64::EPSILON);
    assert_eq!(patched.propagation.max_depth, 3);
    assert_eq!(
        patched.thresholds.medium,
        Policy::default().thresholds.medium
    );

    // The Struct form agrees with the JSON form.
    let raw = client
        .raw()
        .get_policy(pb::GetPolicyRequest { overrides: None })
        .await
        .expect("reads")
        .into_inner();
    let from_struct: Policy = serde_json::from_value(
        convert::struct_to_json(raw.policy.expect("a struct")).expect("finite"),
    )
    .expect("a policy");
    assert_eq!(from_struct, Policy::default());

    let err = client
        .get_policy(Some(
            &serde_json::json!({"thresholds": {"low": 0.9, "high": 0.1}}),
        ))
        .await
        .expect_err("an invalid policy");
    assert_eq!(err.code(), Some(Code::InvalidArgument));
    let err = client
        .get_policy(Some(&serde_json::json!([1, 2])))
        .await
        .expect_err("not an object");
    assert_eq!(err.kind(), ClientErrorKind::InvalidInput);

    // Per-request overrides reach the assessment.
    let strict = serde_json::json!({"thresholds": {"low": 0.0, "medium": 0.0, "high": 0.0, "critical": 0.0}});
    let assessment = client
        .assess_with_policy(simple_request("ENG-1", "Export invoices as CSV!"), &strict)
        .await
        .expect("assesses");
    assert_eq!(assessment.level, ImpactLevel::Critical);

    server.shutdown().await;
}

#[tokio::test]
async fn invalid_input_is_invalid_argument() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let err = client
        .assess(AssessmentRequest::default())
        .await
        .expect_err("no changes");
    assert_eq!(err.kind(), ClientErrorKind::Status(Code::InvalidArgument));
    assert!(
        err.status()
            .is_some_and(|s| s.message().contains("no changes"))
    );
    assert!(err.to_string().starts_with("the assess call failed"));

    let status = client
        .raw()
        .assess(unspecified_status_request("x"))
        .await
        .expect_err("unspecified status");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "changes[0].after.fields[\"status\"]: status category is unspecified"
    );

    let mut bad_time = pb::AssessRequest::from(simple_request("ENG-1", "x"));
    bad_time.now = Some(prost_types::Timestamp {
        seconds: 0,
        nanos: -5,
    });
    let status = client
        .raw()
        .assess(bad_time)
        .await
        .expect_err("negative nanos");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().starts_with("now: "));

    let mut bad_relation = pb::AssessRequest::from(rewrite_request());
    bad_relation.context[1].relations.push(pb::Relation {
        kind: pb::RelationKind::Unspecified.into(),
        target_id: "ENG-13".to_owned(),
    });
    let status = client
        .raw()
        .assess(bad_relation)
        .await
        .expect_err("unspecified relation");
    assert_eq!(
        status.message(),
        "context[1].relations[0].kind: relation kind is unspecified"
    );

    let mut non_finite = simple_request("ENG-1", "x");
    non_finite.changes[0].after = non_finite.changes[0]
        .after
        .take()
        .map(|e| e.with_estimate(f64::NAN));
    let err = client.assess(non_finite).await.expect_err("nan estimate");
    assert_eq!(err.code(), Some(Code::InvalidArgument));

    let err = client.get_entity("nope", None).await.expect_err("unknown");
    assert_eq!(err.code(), Some(Code::NotFound));

    let status = client
        .raw()
        .compare_versions(pb::CompareVersionsRequest {
            id: "nope".to_owned(),
            from: None,
            to: Some(SemVer::new(1, 0, 0).into()),
            now: None,
            options: None,
            judgments: Vec::new(),
        })
        .await
        .expect_err("no from");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "from: is required");

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_flow_create_update_history_stale_acknowledge() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    let now = |hour| PutOptions {
        now: Some(at(7, hour)),
        ..PutOptions::default()
    };

    let created = client
        .put_entity(parent(), now(9))
        .await
        .expect("creates the parent");
    assert!(created.created && created.stored);
    assert_eq!(created.bump, VersionBump::Initial);
    assert_eq!(created.entity.version, Some(SemVer::INITIAL));

    let child = sub_issue(
        "ENG-13",
        "Email and password sign-up form",
        "Form with email, password and password confirmation; hash passwords with argon2.",
        StatusCategory::Started,
    );
    let child = client
        .put_entity(child, now(10))
        .await
        .expect("creates the child");
    assert_eq!(
        child.entity.basis,
        vec![BasisPin {
            entity_id: "ENG-12".to_owned(),
            version: SemVer::INITIAL,
        }],
        "the parent's version is pinned on creation"
    );

    // A dry run stores nothing.
    let what_if = client
        .put_entity(
            parent().with_description(PARENT_AFTER),
            PutOptions {
                dry_run: true,
                ..now(11)
            },
        )
        .await
        .expect("assesses");
    assert!(!what_if.stored);
    assert_eq!(
        client
            .get_entity("ENG-12", None)
            .await
            .expect("exists")
            .version,
        Some(SemVer::INITIAL)
    );

    let updated = client
        .put_entity(
            parent().with_description(PARENT_AFTER),
            PutOptions {
                expected_version: Some(SemVer::INITIAL),
                ..now(12)
            },
        )
        .await
        .expect("updates the parent");
    assert!(!updated.created && updated.stored);
    assert_eq!(updated.bump, VersionBump::Major);
    assert_eq!(updated.entity.version, Some(SemVer::new(2, 0, 0)));
    assert!(
        updated
            .assessment
            .impacts
            .iter()
            .any(|i| i.entity_id == "ENG-13" && i.basis_status == BasisStatus::Incompatible)
    );

    let history = client.history("ENG-12", 0).await.expect("has history");
    let versions: Vec<(SemVer, VersionBump)> =
        history.iter().map(|r| (r.version, r.bump)).collect();
    assert_eq!(
        versions,
        [
            (SemVer::INITIAL, VersionBump::Initial),
            (SemVer::new(2, 0, 0), VersionBump::Major)
        ]
    );
    assert_eq!(history[1].change_type, ChangeType::Updated);
    assert!(
        history[1]
            .changed_fields
            .contains(&"description".to_owned())
    );
    assert_eq!(client.history("ENG-12", 1).await.expect("newest").len(), 1);

    let old = client
        .get_entity("ENG-12", Some(SemVer::INITIAL))
        .await
        .expect("the first version");
    assert_eq!(old.text("description"), Some(PARENT_BEFORE));

    let compared = client
        .compare_versions(
            "ENG-12",
            SemVer::INITIAL,
            SemVer::new(2, 0, 0),
            Some(at(7, 13)),
        )
        .await
        .expect("compares versions");
    assert_eq!(compared.changes[0].bump, VersionBump::Major);

    let stale = client.stale(None).await.expect("lists stale plans");
    assert_eq!(
        stale,
        vec![StaleEntry {
            entity_id: "ENG-13".to_owned(),
            basis_entity_id: "ENG-12".to_owned(),
            pinned: SemVer::INITIAL,
            current: Some(SemVer::new(2, 0, 0)),
            status: BasisStatus::Incompatible,
        }]
    );
    assert_eq!(client.stale(Some("ENG-12")).await.expect("subtree"), stale);

    let acknowledged = client
        .acknowledge("ENG-13", Some(at(7, 14)))
        .await
        .expect("re-pins");
    assert_eq!(
        acknowledged.pin_for("ENG-12").map(|p| p.version),
        Some(SemVer::new(2, 0, 0))
    );
    assert!(client.stale(None).await.expect("lists").is_empty());

    let activity = client
        .record_activity(
            "ENG-13",
            vec![ActivityEvent {
                entity_id: "ENG-13".to_owned(),
                kind: ActivityKind::Comment,
                author: Some("dorj".to_owned()),
                body: "Blocked: we are waiting on the identity provider's credentials.".to_owned(),
                at: Some(at(7, 15)),
            }],
            Some(at(7, 15)),
        )
        .await
        .expect("assesses activity");
    assert_eq!(activity.changes[0].change_type, ChangeType::Activity);

    let dry = client
        .delete_entity("ENG-12", true, None, Some(at(7, 16)))
        .await
        .expect("assesses the deletion");
    assert_eq!(dry.changes[0].change_type, ChangeType::Deleted);
    client
        .get_entity("ENG-12", None)
        .await
        .expect("still there after a dry run");
    client
        .delete_entity("ENG-12", false, Some(SemVer::new(2, 0, 0)), Some(at(7, 17)))
        .await
        .expect("deletes");
    let err = client
        .get_entity("ENG-12", None)
        .await
        .expect_err("deleted");
    assert_eq!(err.code(), Some(Code::NotFound));
    let stale = client.stale(None).await.expect("lists");
    assert_eq!(stale[0].status, BasisStatus::Removed);
    assert_eq!(stale[0].current, None);

    server.shutdown().await;
}

#[tokio::test]
async fn version_conflict_is_aborted() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    client
        .put_entity(parent(), PutOptions::default())
        .await
        .expect("creates");
    let err = client
        .put_entity(
            parent().with_title("Authentication (SSO)"),
            PutOptions {
                expected_version: Some(SemVer::new(9, 9, 9)),
                ..PutOptions::default()
            },
        )
        .await
        .expect_err("a stale expected version");
    assert_eq!(err.kind(), ClientErrorKind::Status(Code::Aborted));
    let err = client
        .delete_entity("ENG-12", false, Some(SemVer::new(3, 0, 0)), None)
        .await
        .expect_err("a stale expected version");
    assert_eq!(err.code(), Some(Code::Aborted));

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Import and source sequences
// ---------------------------------------------------------------------------

fn issue(id: &str, title: &str) -> Entity {
    Entity::new(id, "issue").with_title(title)
}

/// An in-memory service that applies an import `chunk` items at a time,
/// with `slots` registry slots.
fn import_service(chunk: usize, slots: usize) -> AssessmentService {
    AssessmentService::in_memory().with_limits(
        ServiceLimits::default()
            .with_max_import_chunk(chunk)
            .with_max_concurrent_assessments(slots),
    )
}

/// Waits until the server holds `id`.
async fn wait_for_entity(client: &mut AssessmentClient, id: &str) -> Entity {
    loop {
        match client.get_entity(id, None).await {
            Ok(entity) => return entity,
            Err(err) => assert_eq!(err.code(), Some(Code::NotFound), "{err}"),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn import_counts_created_unchanged_assessed_stale_deleted_and_failed() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;

        let first = client
            .import_iter(vec![
                ImportItem::upsert(issue("ENG-1", "Rate limits for the public API"))
                    .with_source_seq(5),
                ImportItem::upsert(issue("ENG-2", "Usage dashboard")),
                ImportItem::upsert(issue("ENG-3", "Billing export")),
            ])
            .await
            .expect("the first import runs");
        assert_eq!(
            first.summary,
            ImportSummary {
                created: 3,
                ..ImportSummary::default()
            }
        );
        assert_eq!(first.failed_total, 0);

        let second = client
            .import_iter(vec![
                // Older than what the server holds: skipped.
                ImportItem::upsert(issue("ENG-1", "Rate limits")).with_source_seq(3),
                // The same content: nothing to do.
                ImportItem::upsert(issue("ENG-2", "Usage dashboard")),
                // New content: assessed and stored.
                ImportItem::upsert(issue("ENG-3", "Billing export to CSV and PDF")),
                ImportItem::upsert(issue("ENG-4", "Audit log")),
                // Not a valid entity: reported, and the rest still runs.
                ImportItem::upsert(
                    issue("ENG-5", "Estimate").with_field("points", FieldValue::Number(f64::NAN)),
                ),
                ImportItem::delete("ENG-2"),
            ])
            .await
            .expect("the second import runs");
        assert_eq!(second.summary.created, 1, "{second:?}");
        assert_eq!(second.summary.unchanged, 1, "{second:?}");
        assert_eq!(second.summary.assessed, 1, "{second:?}");
        assert_eq!(second.summary.skipped_stale, 1, "{second:?}");
        assert_eq!(second.summary.deleted, 1, "{second:?}");
        assert_eq!(second.failed_total, 1, "{second:?}");
        assert_eq!(second.summary.failed.len(), 1, "{second:?}");
        assert_eq!(second.summary.failed[0].id, "ENG-5");
        assert!(!second.summary.failed[0].error.is_empty());

        let kept = client.get_entity("ENG-1", None).await.expect("is stored");
        assert_eq!(kept.title(), "Rate limits for the public API");
        let changed = client.get_entity("ENG-3", None).await.expect("is stored");
        assert_eq!(changed.title(), "Billing export to CSV and PDF");
        assert_eq!(client.history("ENG-3", 0).await.expect("history").len(), 2);
        client.get_entity("ENG-4", None).await.expect("is stored");
        for gone in ["ENG-2", "ENG-5"] {
            let err = client.get_entity(gone, None).await.expect_err("not stored");
            assert_eq!(err.code(), Some(Code::NotFound), "{gone}");
        }

        // Sending the same items again changes nothing.
        let again = client
            .import_iter(vec![
                ImportItem::upsert(issue("ENG-3", "Billing export to CSV and PDF")),
                ImportItem::upsert(issue("ENG-4", "Audit log")),
            ])
            .await
            .expect("the repeat runs");
        assert_eq!(
            again.summary,
            ImportSummary {
                unchanged: 2,
                ..ImportSummary::default()
            }
        );

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn import_keeps_the_order_of_repeated_ids_across_chunks() {
    within(E2E_LIMIT, async {
        // Two items per chunk, so the five versions of ENG-1 are spread
        // over several registry calls.
        let server = TestServer::start_with(import_service(2, 2), ServerOptions::default()).await;
        let mut client = server.client().await;

        let titles = [
            "Rate limits",
            "Rate limits for the API",
            "Rate limits for the public API",
            "Rate limits for the public and the partner API",
            "Rate limits and quotas for the public and the partner API",
        ];
        let mut items = Vec::new();
        for (n, title) in titles.iter().enumerate() {
            items.push(ImportItem::upsert(issue("ENG-1", title)));
            items.push(ImportItem::upsert(issue(&format!("OTHER-{n}"), "Filler")));
        }
        // Created, deleted and created again: it is there at the end.
        items.push(ImportItem::upsert(issue("ENG-2", "Usage dashboard")));
        items.push(ImportItem::delete("ENG-2"));
        items.push(ImportItem::upsert(issue("ENG-2", "Usage dashboard, again")));
        // Created and deleted: it is gone at the end.
        items.push(ImportItem::upsert(issue("ENG-3", "Billing export")));
        items.push(ImportItem::delete("ENG-3"));

        let report = client.import_iter(items).await.expect("the import runs");
        assert_eq!(report.failed_total, 0, "{report:?}");
        assert_eq!(report.summary.deleted, 2, "{report:?}");
        assert_eq!(report.summary.assessed, 4, "{report:?}");

        let last = client.get_entity("ENG-1", None).await.expect("is stored");
        assert_eq!(last.title(), titles[4]);
        let history = client.history("ENG-1", 0).await.expect("history");
        let seen: Vec<String> = history
            .iter()
            .map(|record| {
                record
                    .snapshot
                    .as_ref()
                    .expect("a snapshot")
                    .title()
                    .to_owned()
            })
            .collect();
        assert_eq!(seen, titles, "every version, in the order it was sent");

        let again = client.get_entity("ENG-2", None).await.expect("is stored");
        assert_eq!(again.title(), "Usage dashboard, again");
        let err = client.get_entity("ENG-3", None).await.expect_err("deleted");
        assert_eq!(err.code(), Some(Code::NotFound));

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn other_calls_are_answered_while_an_import_is_open() {
    within(E2E_LIMIT, async {
        // One registry slot: an import that kept it between chunks would
        // block the put below.
        let server = TestServer::start_with(import_service(2, 1), ServerOptions::default()).await;
        let mut client = server.client().await;
        let mut importer = server.client().await;

        let (items, stream) = tokio::sync::mpsc::channel::<ImportItem>(8);
        let import =
            tokio::spawn(async move { importer.import(ReceiverStream::new(stream)).await });
        for id in ["ENG-1", "ENG-2"] {
            items
                .send(ImportItem::upsert(issue(id, "First chunk")))
                .await
                .expect("the import reads");
        }

        // The first chunk is applied and visible while the stream is open.
        wait_for_entity(&mut client, "ENG-2").await;
        assert!(!import.is_finished(), "the import waits for more items");
        client.get_entity("ENG-1", None).await.expect("answers");
        let put = client
            .put_entity(
                issue("ENG-9", "Put during the import"),
                PutOptions::default(),
            )
            .await
            .expect("a put gets the registry slot between chunks");
        assert!(put.created);
        assert_eq!(client.history("ENG-1", 0).await.expect("answers").len(), 1);

        items
            .send(ImportItem::upsert(issue("ENG-3", "Second chunk")))
            .await
            .expect("the import reads");
        drop(items);
        let report = import
            .await
            .expect("the import task finishes")
            .expect("the import runs");
        assert_eq!(report.summary.created, 3, "{report:?}");

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn get_entity_answers_during_a_large_import() {
    within(E2E_LIMIT, async {
        const ITEMS: usize = 3_000;
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;
        let mut importer = server.client().await;
        client
            .put_entity(parent(), PutOptions::default())
            .await
            .expect("creates");

        let items: Vec<ImportItem> = (0..ITEMS)
            .map(|n| {
                ImportItem::upsert(issue(
                    &format!("BULK-{n}"),
                    &format!("Imported work item number {n}"),
                ))
            })
            .collect();
        let import = tokio::spawn(async move { importer.import_iter(items).await });

        let mut reads = 0u32;
        while !import.is_finished() {
            let entity =
                tokio::time::timeout(Duration::from_secs(5), client.get_entity("ENG-12", None))
                    .await
                    .expect("a read does not wait for the import")
                    .expect("answers");
            assert_eq!(entity.id, "ENG-12");
            reads += 1;
        }
        assert!(reads > 0);

        let report = import
            .await
            .expect("the import task finishes")
            .expect("the import runs");
        assert_eq!(report.summary.created, ITEMS as u64, "{report:?}");
        assert_eq!(report.failed_total, 0);
        // The first and the last item, from different chunks.
        client.get_entity("BULK-0", None).await.expect("is stored");
        client
            .get_entity(format!("BULK-{}", ITEMS - 1), None)
            .await
            .expect("is stored");

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_dropped_import_keeps_its_applied_chunks_and_applies_no_more() {
    within(E2E_LIMIT, async {
        let server = TestServer::start_with(import_service(2, 2), ServerOptions::default()).await;
        let mut client = server.client().await;
        let mut importer = server.client().await;

        let (items, stream) = tokio::sync::mpsc::channel::<ImportItem>(8);
        let import =
            tokio::spawn(async move { importer.import(ReceiverStream::new(stream)).await });
        for id in ["ENG-1", "ENG-2", "ENG-3"] {
            items
                .send(ImportItem::upsert(issue(id, "Sent before the client left")))
                .await
                .expect("the import reads");
        }
        wait_for_entity(&mut client, "ENG-2").await;

        // The client gives up with ENG-3 sent but its chunk not full, and
        // without ending the stream.
        import.abort();
        let _ = import.await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        client
            .get_entity("ENG-1", None)
            .await
            .expect("stays stored");
        client
            .get_entity("ENG-2", None)
            .await
            .expect("stays stored");
        let err = client
            .get_entity("ENG-3", None)
            .await
            .expect_err("the unfinished chunk is not applied");
        assert_eq!(err.code(), Some(Code::NotFound));
        drop(items);

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn an_import_that_passes_the_deadline_keeps_its_applied_chunks() {
    within(E2E_LIMIT, async {
        let options =
            ServerOptions::default().with_request_timeout(Some(Duration::from_millis(1_500)));
        let server = TestServer::start_with(import_service(2, 2), options).await;
        let mut client = server.client().await;
        let mut importer = server.client().await;

        let (items, stream) = tokio::sync::mpsc::channel::<ImportItem>(8);
        let import =
            tokio::spawn(async move { importer.import(ReceiverStream::new(stream)).await });
        for id in ["ENG-1", "ENG-2", "ENG-3"] {
            items
                .send(ImportItem::upsert(issue(id, "Sent before the deadline")))
                .await
                .expect("the import reads");
        }
        // The stream stays open, so the call runs into the deadline.
        let err = import
            .await
            .expect("the import task finishes")
            .expect_err("the deadline passes");
        assert_eq!(err.code(), Some(Code::Cancelled), "{err}");

        client
            .get_entity("ENG-1", None)
            .await
            .expect("stays stored");
        client
            .get_entity("ENG-2", None)
            .await
            .expect("stays stored");
        let err = client
            .get_entity("ENG-3", None)
            .await
            .expect_err("the unfinished chunk is not applied");
        assert_eq!(err.code(), Some(Code::NotFound));
        drop(items);

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn import_lists_an_item_without_an_entity_as_failed_and_goes_on() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;

        let mut unspecified_status = pb::Entity::from(issue("ENG-2", "Status without a category"));
        unspecified_status.fields.insert(
            "status".to_owned(),
            pb::FieldValue {
                kind: Some(pb::field_value::Kind::Status(pb::Status {
                    category: pb::StatusCategory::Unspecified.into(),
                    name: "Somewhere".to_owned(),
                })),
            },
        );
        let requests = vec![
            pb::ImportRequest::from(ImportItem::upsert(issue("ENG-1", "Before"))),
            pb::ImportRequest::default(),
            pb::ImportRequest {
                entity: Some(unspecified_status),
                ..Default::default()
            },
            pb::ImportRequest::from(ImportItem::upsert(issue("", "No id"))),
            pb::ImportRequest::from(ImportItem::upsert(issue("ENG-3", "After"))),
        ];
        let response = client
            .raw()
            .import(tokio_stream::iter(requests))
            .await
            .expect("the stream as a whole is accepted")
            .into_inner();
        assert_eq!(response.created, 2, "{response:?}");
        assert_eq!(response.failed_total, 3, "{response:?}");
        let failed: Vec<(&str, &str)> = response
            .failed
            .iter()
            .map(|f| (f.id.as_str(), f.error.as_str()))
            .collect();
        assert_eq!(failed.len(), 3, "{failed:?}");
        assert_eq!(failed[0], ("", "entity: is required"));
        assert_eq!(failed[1].0, "ENG-2");
        assert!(
            failed[1].1.starts_with("entity.fields[\"status\"]"),
            "{failed:?}"
        );
        assert_eq!(failed[2].0, "");
        client.get_entity("ENG-1", None).await.expect("is stored");
        client.get_entity("ENG-3", None).await.expect("is stored");

        // The same entities fail a put for the same reason.
        let err = client
            .raw()
            .put_entity(pb::PutEntityRequest {
                entity: None,
                ..Default::default()
            })
            .await
            .expect_err("a put without an entity");
        assert_eq!(err.code(), Code::InvalidArgument);

        server.shutdown().await;
    })
    .await;
}

/// Ids have no length limit, and each failure repeats the id in its error.
/// The answer must still fit in one message.
#[tokio::test]
async fn import_answers_when_a_thousand_failed_items_have_very_long_ids() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;

        let long = "é".repeat(5_000);
        let mut items: Vec<ImportItem> = (0..MAX_IMPORT_FAILURES)
            .map(|n| {
                ImportItem::upsert(
                    issue(&format!("BAD-{n}-{long}"), "Estimate")
                        .with_field("points", FieldValue::Number(f64::INFINITY)),
                )
            })
            .collect();
        items.push(ImportItem::upsert(issue("ENG-1", "Valid")));

        let report = client
            .import_iter(items)
            .await
            .expect("the answer fits in one message");
        assert_eq!(report.summary.created, 1);
        assert_eq!(report.failed_total, MAX_IMPORT_FAILURES as u64);
        assert_eq!(report.summary.failed.len(), MAX_IMPORT_FAILURES);
        for f in &report.summary.failed {
            assert!(f.id.len() <= MAX_IMPORT_FAILURE_ID_BYTES, "{}", f.id.len());
            assert!(
                f.error.len() <= MAX_IMPORT_FAILURE_ERROR_BYTES,
                "{}",
                f.error.len()
            );
            assert!(f.id.ends_with("..."), "a cut id says so");
        }
        assert!(report.summary.failed[0].id.starts_with("BAD-0-é"));
        server.shutdown().await;
    })
    .await;
}

/// A deletion can arrive before the item it deletes. It fails, and its
/// sequence still guards the id against the older put.
#[tokio::test]
async fn delete_entity_of_an_item_that_is_not_stored_keeps_its_sequence() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;

        let err = client
            .delete_entity_with(
                "ENG-1",
                DeleteOptions {
                    source_seq: Some(5),
                    ..DeleteOptions::default()
                },
            )
            .await
            .expect_err("nothing is stored to delete");
        assert_eq!(err.code(), Some(Code::NotFound));

        let late = client
            .put_entity(
                issue("ENG-1", "Late"),
                PutOptions {
                    source_seq: Some(3),
                    ..PutOptions::default()
                },
            )
            .await
            .expect("the put answers");
        assert!(late.skipped_stale && !late.stored && !late.created);
        // The item as sent: the server does not hold it.
        assert_eq!(late.entity.title(), "Late");
        assert_eq!(late.entity.version, None);
        let err = client
            .get_entity("ENG-1", None)
            .await
            .expect_err("the late put did not bring the item back");
        assert_eq!(err.code(), Some(Code::NotFound));
        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn import_lists_at_most_a_thousand_failures_and_counts_them_all() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;

        let extra = 7;
        let mut items: Vec<ImportItem> = (0..MAX_IMPORT_FAILURES + extra)
            .map(|n| {
                ImportItem::upsert(
                    issue(&format!("BAD-{n}"), "Estimate")
                        .with_field("points", FieldValue::Number(f64::INFINITY)),
                )
            })
            .collect();
        items.push(ImportItem::upsert(issue("ENG-1", "Valid")));

        let report = client.import_iter(items).await.expect("the import runs");
        assert_eq!(report.summary.created, 1, "{:?}", report.summary.created);
        assert_eq!(report.failed_total, (MAX_IMPORT_FAILURES + extra) as u64);
        assert_eq!(report.summary.failed.len(), MAX_IMPORT_FAILURES);
        assert_eq!(report.summary.failed[0].id, "BAD-0");
        assert_eq!(
            report.summary.failed[MAX_IMPORT_FAILURES - 1].id,
            format!("BAD-{}", MAX_IMPORT_FAILURES - 1),
            "the first failures, in input order"
        );

        server.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_put_or_delete_with_an_older_source_seq_is_skipped() {
    within(E2E_LIMIT, async {
        let server = TestServer::start(ServerOptions::default()).await;
        let mut client = server.client().await;
        let seq = |n| PutOptions {
            source_seq: Some(n),
            ..PutOptions::default()
        };

        let created = client
            .put_entity(issue("ENG-1", "Rate limits"), seq(10))
            .await
            .expect("creates");
        assert!(created.created && created.stored && !created.skipped_stale);

        let stale = client
            .put_entity(issue("ENG-1", "An older title that arrives late"), seq(4))
            .await
            .expect("answers");
        assert!(stale.skipped_stale, "{stale:?}");
        assert!(!stale.stored);
        assert_eq!(stale.entity.title(), "Rate limits", "the stored entity");

        let stale = client
            .delete_entity_with(
                "ENG-1",
                DeleteOptions {
                    source_seq: Some(9),
                    ..DeleteOptions::default()
                },
            )
            .await
            .expect("answers");
        assert!(stale.skipped_stale && !stale.deleted, "{stale:?}");
        let entity = client.get_entity("ENG-1", None).await.expect("still there");
        assert_eq!(entity.title(), "Rate limits");
        assert_eq!(client.history("ENG-1", 0).await.expect("history").len(), 1);

        let newer = client
            .put_entity(issue("ENG-1", "Rate limits for the public API"), seq(11))
            .await
            .expect("updates");
        assert!(newer.stored && !newer.skipped_stale && !newer.unchanged);
        let same = client
            .put_entity(
                issue("ENG-1", "Rate limits for the public API"),
                PutOptions::default(),
            )
            .await
            .expect("answers");
        assert!(same.unchanged, "{same:?}");

        let deleted = client
            .delete_entity_with(
                "ENG-1",
                DeleteOptions {
                    source_seq: Some(12),
                    ..DeleteOptions::default()
                },
            )
            .await
            .expect("deletes");
        assert!(deleted.deleted && !deleted.skipped_stale);
        let err = client.get_entity("ENG-1", None).await.expect_err("deleted");
        assert_eq!(err.code(), Some(Code::NotFound));

        server.shutdown().await;
    })
    .await;
}

// ---------------------------------------------------------------------------
// Judgments
// ---------------------------------------------------------------------------

const PARAPHRASE: (&str, &str) = (
    "Users can log in with their email address and password.",
    "People are able to sign in using e-mail and a password.",
);
const POSTGRES: (&str, &str) = (
    "Store user sessions in PostgreSQL with a 30 day expiry.",
    "Store user sessions in MongoDB with a 30 day expiry.",
);

fn judged_parent(description: &str) -> Entity {
    Entity::new("P", "issue")
        .with_title("Parent issue")
        .with_description(description)
        .with_status(StatusCategory::Started, "In Progress")
}

fn judged_child(description: &str) -> Entity {
    Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("Child task")
        .with_description(description)
        .with_status(StatusCategory::Started, "In Progress")
}

fn judgment(pair: (&str, &str), relation: TextRelation) -> TextJudgment {
    TextJudgment {
        field: DESCRIPTION.to_owned(),
        fingerprint: fingerprint(pair.0, pair.1),
        relation,
        confidence: 0.9,
        rationale: "read by a person".to_owned(),
        source: "test".to_owned(),
    }
}

/// The parent's description goes from `pair.0` to `pair.1`; one sub-issue
/// was planned against the old text.
fn judged_request(pair: (&str, &str), judgments: Vec<TextJudgment>) -> AssessmentRequest {
    let v1 = SemVer::new(1, 0, 0);
    let mut change = ChangeSet::update(
        judged_parent(pair.0).with_version(v1),
        judged_parent(pair.1).with_version(v1),
    );
    change.judgments = judgments;
    AssessmentRequest {
        changes: vec![change],
        context: vec![judged_child(pair.0).with_basis("P", v1)],
        now: Some(at(9, 9)),
        ..Default::default()
    }
}

fn description_judgment(assessment: &Assessment) -> &FieldJudgment {
    assessment.changes[0]
        .fields
        .iter()
        .find(|f| f.field == DESCRIPTION)
        .expect("the description changed")
        .judgment
        .as_ref()
        .expect("the description has a judgment")
}

/// A registry with the parent at `text` (1.0.0) and one sub-issue.
async fn judged_registry(client: &mut AssessmentClient, text: &str) {
    let options = || PutOptions {
        now: Some(at(9, 9)),
        ..PutOptions::default()
    };
    client
        .put_entity(judged_parent(text), options())
        .await
        .expect("stores the parent");
    client
        .put_entity(judged_child(text), options())
        .await
        .expect("stores the child");
}

#[tokio::test]
async fn a_flagged_change_returns_a_judgment_request() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let request = judged_request(PARAPHRASE, Vec::new());
    let assessment = client.assess(request.clone()).await.expect("assesses");
    assert_eq!(
        assessment,
        Engine::default().assess(&request).expect("assesses")
    );
    assert_eq!(assessment.changes[0].bump, VersionBump::Major);
    let flagged = description_judgment(&assessment);
    assert_eq!(flagged.status, JudgmentStatus::Requested);
    assert_eq!(flagged.reasons, vec![JudgmentReason::ParaphraseSuspected]);
    assert_eq!(flagged.relation, None);
    assert_eq!(flagged.fingerprint, fingerprint(PARAPHRASE.0, PARAPHRASE.1));

    // The request names the change and the field, without the texts.
    assert_eq!(assessment.judgment_requests.len(), 1);
    let asked = &assessment.judgment_requests[0];
    assert_eq!(asked.change_index, 0);
    assert_eq!(asked.entity_id, "P");
    assert_eq!(asked.field, DESCRIPTION);
    assert_eq!(asked.fingerprint, flagged.fingerprint);
    assert_eq!(asked.reasons, flagged.reasons);
    assert_eq!(
        (asked.before.as_deref(), asked.after.as_deref()),
        (None, None)
    );

    // The texts come only when they are asked for.
    let mut with_texts = request.clone();
    with_texts.options.judgment_texts = true;
    let assessment = client.assess(with_texts).await.expect("assesses");
    let asked = &assessment.judgment_requests[0];
    assert_eq!(
        (asked.before.as_deref(), asked.after.as_deref()),
        (Some(PARAPHRASE.0), Some(PARAPHRASE.1))
    );

    // No judge is wired: asking for one changes no number. The field says
    // that no judge answered.
    let mut judged = request.clone();
    judged.options.judge = Some(true);
    let with_judge = client.assess(judged).await.expect("assesses");
    let unanswered = description_judgment(&with_judge);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "this server has no judge");
    let mut same = with_judge.changes.clone();
    same[0]
        .fields
        .iter_mut()
        .find(|f| f.field == DESCRIPTION)
        .expect("the description changed")
        .judgment = Some(flagged.clone());
    assert_eq!(same, assessment.changes);
    assert_eq!(with_judge.score, assessment.score);
    assert_eq!(client.judge_info().await.expect("answers"), None);

    server.shutdown().await;
}

#[tokio::test]
async fn a_supplied_judgment_is_applied_end_to_end() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let lexical = client
        .assess(judged_request(PARAPHRASE, Vec::new()))
        .await
        .expect("assesses");

    let supplied = judgment(PARAPHRASE, TextRelation::Equivalent);
    let request = judged_request(PARAPHRASE, vec![supplied.clone()]);
    let assessment = client.assess(request.clone()).await.expect("assesses");
    assert_eq!(
        assessment,
        Engine::default().assess(&request).expect("assesses")
    );
    assert_eq!(assessment.changes[0].bump, VersionBump::Patch);
    assert_eq!(
        assessment.changes[0].next_version,
        Some(SemVer::new(1, 0, 1))
    );
    assert!(assessment.score < lexical.score);
    assert!(assessment.judgment_requests.is_empty());
    assert!(assessment.unmatched_judgments.is_empty());
    let applied = description_judgment(&assessment);
    assert_eq!(applied.status, JudgmentStatus::Applied);
    assert_eq!(applied.relation, Some(TextRelation::Equivalent));
    assert_eq!(applied.confidence, Some(0.9));
    assert_eq!(applied.rationale, supplied.rationale);
    assert_eq!(applied.source, supplied.source);
    assert_eq!(
        applied.lexical_magnitude,
        description_judgment(&lexical).lexical_magnitude
    );

    // A judgment the other way makes the change breaking.
    let replaced = client
        .assess(judged_request(
            POSTGRES,
            vec![judgment(POSTGRES, TextRelation::Replaced)],
        ))
        .await
        .expect("assesses");
    assert_eq!(replaced.changes[0].bump, VersionBump::Major);

    // In a batch and on a stream too.
    let batch = client
        .assess_batch(vec![request.clone()])
        .await
        .expect("answers");
    assert_eq!(batch[0].result.as_ref().expect("assesses"), &assessment);

    server.shutdown().await;
}

#[tokio::test]
async fn a_judgment_of_another_text_is_stale_and_changes_nothing() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;

    let lexical = client
        .assess(judged_request(PARAPHRASE, Vec::new()))
        .await
        .expect("assesses");
    let request = judged_request(
        PARAPHRASE,
        vec![
            // Made for another pair of texts.
            judgment(POSTGRES, TextRelation::Equivalent),
            // For a field that did not change.
            TextJudgment {
                field: "notes".to_owned(),
                ..judgment(PARAPHRASE, TextRelation::Equivalent)
            },
        ],
    );
    let assessment = client.assess(request.clone()).await.expect("assesses");
    assert_eq!(
        assessment,
        Engine::default().assess(&request).expect("assesses")
    );
    assert_eq!(
        description_judgment(&assessment).status,
        JudgmentStatus::Stale
    );
    assert_eq!(assessment.score, lexical.score);
    assert_eq!(assessment.level, lexical.level);
    assert_eq!(assessment.changes[0].bump, lexical.changes[0].bump);
    assert_eq!(assessment.impacts, lexical.impacts);
    assert_eq!(
        assessment.unmatched_judgments,
        vec![JudgmentKey {
            entity_id: "P".to_owned(),
            field: "notes".to_owned(),
        }]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_invalid_judgment_is_rejected_with_its_path() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    let good = pb::TextJudgment::from(judgment(PARAPHRASE, TextRelation::Equivalent));
    let request = |bad: pb::TextJudgment| {
        let mut request = pb::AssessRequest::from(judged_request(PARAPHRASE, Vec::new()));
        request.changes[0].judgments = vec![bad];
        request
    };

    let status = client
        .raw()
        .assess(request(pb::TextJudgment {
            relation: pb::TextRelation::Unspecified.into(),
            ..good.clone()
        }))
        .await
        .expect_err("an unspecified relation");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status
            .message()
            .contains("changes[0].judgments[0].relation: text relation is unspecified"),
        "{}",
        status.message()
    );

    for confidence in [1.5, -0.1, f64::NAN] {
        let status = client
            .raw()
            .assess(request(pb::TextJudgment {
                confidence: Some(confidence),
                ..good.clone()
            }))
            .await
            .expect_err("a confidence outside [0, 1]");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            status
                .message()
                .contains("changes[0].judgments[0].confidence"),
            "{}",
            status.message()
        );
    }

    // In a batch the item fails and the rest goes on.
    let batch = client
        .raw()
        .assess_batch(pb::AssessBatchRequest {
            requests: vec![
                request(pb::TextJudgment {
                    relation: 0,
                    ..good.clone()
                }),
                request(good.clone()),
            ],
        })
        .await
        .expect("answers")
        .into_inner();
    let error = batch.responses[0].error.as_ref().expect("the item fails");
    assert!(
        error.message.contains("changes[0].judgments[0].relation"),
        "{}",
        error.message
    );
    assert!(batch.responses[1].assessment.is_some());

    // The registry calls check their judgments the same way.
    let mut put = convert::put_request_to_pb(judged_parent(PARAPHRASE.0), PutOptions::default());
    put.judgments = vec![pb::TextJudgment {
        relation: 0,
        ..good.clone()
    }];
    let status = client.raw().put_entity(put).await.expect_err("is rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("judgments[0].relation"),
        "{}",
        status.message()
    );
    let status = client
        .raw()
        .compare_versions(pb::CompareVersionsRequest {
            id: "P".to_owned(),
            from: Some(SemVer::new(1, 0, 0).into()),
            to: Some(SemVer::new(1, 0, 1).into()),
            now: None,
            options: None,
            judgments: vec![pb::TextJudgment {
                relation: 0,
                ..good
            }],
        })
        .await
        .expect_err("is rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status
            .message()
            .contains("judgments[0].relation: text relation is unspecified"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn put_entity_holds_a_flagged_change_until_it_is_judged() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    judged_registry(&mut client, POSTGRES.0).await;
    let hold = || PutOptions {
        hold_if_flagged: true,
        now: Some(at(9, 10)),
        assess: AssessOptions {
            judgment_texts: true,
            ..AssessOptions::default()
        },
        ..PutOptions::default()
    };

    let held = client
        .put_entity(judged_parent(POSTGRES.1), hold())
        .await
        .expect("answers");
    assert!(!held.stored && !held.unchanged && !held.created);
    assert_eq!(held.assessment.judgment_requests.len(), 1);
    let asked = &held.assessment.judgment_requests[0];
    assert_eq!(asked.reasons, vec![JudgmentReason::TermSubstitution]);
    assert_eq!(
        (asked.before.as_deref(), asked.after.as_deref()),
        (Some(POSTGRES.0), Some(POSTGRES.1))
    );
    assert_eq!(client.history("P", 0).await.expect("history").len(), 1);
    let stored = client.get_entity("P", None).await.expect("is stored");
    assert_eq!(stored.text(DESCRIPTION), Some(POSTGRES.0));

    // The second put brings the judgment and is stored.
    let supplied = TextJudgment {
        fingerprint: asked.fingerprint.clone(),
        ..judgment(POSTGRES, TextRelation::Replaced)
    };
    let out = client
        .put_entity(
            judged_parent(POSTGRES.1),
            PutOptions {
                judgments: vec![supplied.clone()],
                ..hold()
            },
        )
        .await
        .expect("stores");
    assert!(out.stored);
    assert_eq!(out.bump, VersionBump::Major);
    assert_eq!(out.entity.version, Some(SemVer::new(2, 0, 0)));
    assert_eq!(
        description_judgment(&out.assessment).status,
        JudgmentStatus::Applied
    );
    let history = client.history("P", 0).await.expect("history");
    assert_eq!(history.len(), 2);
    assert!(history[0].judgments.is_empty());
    assert_eq!(
        history.last().expect("a record").judgments,
        vec![supplied.clone()]
    );

    // A judgment of a text that moved on is stale. The field is asked for
    // again, so a held put stores nothing.
    let later = "Store user sessions in Redis with a 30 day expiry.";
    let out = client
        .put_entity(
            judged_parent(later),
            PutOptions {
                judgments: vec![supplied.clone()],
                ..hold()
            },
        )
        .await
        .expect("answers");
    assert!(!out.stored);
    assert_eq!(out.assessment.judgment_requests.len(), 1);
    assert_eq!(
        description_judgment(&out.assessment).status,
        JudgmentStatus::Stale
    );
    // Without the hold the lexical bump is stored and the judgment is not
    // kept.
    let out = client
        .put_entity(
            judged_parent(later),
            PutOptions {
                judgments: vec![supplied],
                hold_if_flagged: false,
                ..hold()
            },
        )
        .await
        .expect("stores");
    assert!(out.stored);
    assert_eq!(
        description_judgment(&out.assessment).status,
        JudgmentStatus::Stale
    );
    let history = client.history("P", 0).await.expect("history");
    assert_eq!(history.len(), 3);
    assert!(history.last().expect("a record").judgments.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn compare_versions_takes_options_and_judgments() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    judged_registry(&mut client, PARAPHRASE.0).await;
    let supplied = judgment(PARAPHRASE, TextRelation::Equivalent);
    let out = client
        .put_entity(
            judged_parent(PARAPHRASE.1),
            PutOptions {
                judgments: vec![supplied],
                now: Some(at(9, 10)),
                ..PutOptions::default()
            },
        )
        .await
        .expect("stores");
    assert_eq!(out.bump, VersionBump::Patch);
    let (from, to) = (SemVer::new(1, 0, 0), SemVer::new(1, 0, 1));
    assert_eq!(out.entity.version, Some(to));
    let now = Some(at(9, 11));

    // The stored judgment is used again.
    let stored = client
        .compare_versions("P", from, to, now)
        .await
        .expect("compares");
    assert_eq!(stored.changes[0].bump, VersionBump::Patch);
    assert_eq!(
        description_judgment(&stored).status,
        JudgmentStatus::Applied
    );
    assert_eq!(
        client
            .compare_versions_with("P", from, to, now, AssessOptions::default(), Vec::new())
            .await
            .expect("compares"),
        stored
    );

    // A given judgment of the field goes before the stored one.
    let replaced = client
        .compare_versions_with(
            "P",
            from,
            to,
            now,
            AssessOptions::default(),
            vec![judgment(PARAPHRASE, TextRelation::Replaced)],
        )
        .await
        .expect("compares");
    assert_eq!(replaced.changes[0].bump, VersionBump::Major);
    assert_eq!(
        description_judgment(&replaced).relation,
        Some(TextRelation::Replaced)
    );

    // The options reach the engine.
    let short = client
        .compare_versions_with(
            "P",
            from,
            to,
            now,
            AssessOptions {
                top_k: 1,
                explain: false,
                ..AssessOptions::default()
            },
            Vec::new(),
        )
        .await
        .expect("compares");
    assert!(short.impacts.len() <= 1);
    assert_eq!(short.changes[0].bump, VersionBump::Patch);

    server.shutdown().await;
}

#[tokio::test]
async fn compare_text_reports_the_gray_zone() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    let engine = Engine::default();

    let diff = client
        .compare_text(POSTGRES.0, POSTGRES.1, None)
        .await
        .expect("compares");
    assert_eq!(
        diff,
        engine
            .compare_text(POSTGRES.0, POSTGRES.1, None)
            .expect("valid texts")
    );
    let zone = diff.gray_zone.expect("the change is flagged");
    assert_eq!(zone.reasons, vec![JudgmentReason::TermSubstitution]);
    assert_eq!(zone.fingerprint, fingerprint(POSTGRES.0, POSTGRES.1));

    // The field decides how the text is read.
    for field in [DESCRIPTION, "title", "notes"] {
        let diff = client
            .compare_text_as(field, PARAPHRASE.0, PARAPHRASE.1, None)
            .await
            .expect("compares");
        assert_eq!(
            diff,
            engine
                .compare_text_as(field, PARAPHRASE.0, PARAPHRASE.1, None)
                .expect("valid texts"),
            "{field}"
        );
    }

    // An unflagged change has no gray zone.
    let same = client
        .compare_text("Add a footer.", "Add a footer.", None)
        .await
        .expect("compares");
    assert_eq!(same.gray_zone, None);

    let err = client
        .compare_text_as("f".repeat(257), "a", "b", None)
        .await
        .expect_err("the field name is too long");
    assert_eq!(err.code(), Some(Code::InvalidArgument));

    server.shutdown().await;
}

#[tokio::test]
async fn with_timeout_sends_grpc_timeout() {
    use assessment_grpc::pb::assessment_engine_server::AssessmentEngineServer;
    use std::sync::{Arc, Mutex};

    // A plain server that records the `grpc-timeout` of every call.
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = Arc::clone(&seen);
    let service = AssessmentEngineServer::with_interceptor(
        AssessmentService::in_memory(),
        move |request: tonic::Request<()>| {
            let timeout = request
                .metadata()
                .get("grpc-timeout")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            record.lock().expect("not poisoned").push(timeout);
            Ok(request)
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds a free port");
    let url = format!("http://{}", listener.local_addr().expect("has an address"));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            }),
    );

    let mut plain = AssessmentClient::connect(url.clone())
        .await
        .expect("connects");
    plain.compare_text("a", "b", None).await.expect("compares");
    let mut timed = AssessmentClient::connect(url)
        .await
        .expect("connects")
        .with_timeout(Duration::from_secs(7))
        .with_max_message_bytes(1 << 20);
    timed.compare_text("a", "b", None).await.expect("compares");
    timed.judge_info().await.expect("answers");
    timed
        .assess(simple_request("A", "A new title"))
        .await
        .expect("assesses");

    let seen = seen.lock().expect("not poisoned").clone();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0], None);
    for timeout in &seen[1..] {
        // tonic writes the deadline in the largest unit that fits.
        let timeout = timeout.as_deref().expect("a grpc-timeout header");
        let (digits, unit) = timeout.split_at(timeout.len() - 1);
        let value: u64 = digits.parse().expect("a number");
        let seconds = match unit {
            "H" => value * 3600,
            "M" => value * 60,
            "S" => value,
            "m" => value / 1_000,
            "u" => value / 1_000_000,
            "n" => value / 1_000_000_000,
            other => panic!("an unknown unit {other}"),
        };
        assert_eq!(seconds, 7, "{timeout}");
    }

    let _ = stop.send(());
    task.await
        .expect("the server task does not panic")
        .expect("serves without error");
}

#[tokio::test]
async fn a_call_past_its_timeout_fails() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await.with_timeout(Duration::from_nanos(1));
    let err = client
        .compare_text("a", "b", None)
        .await
        .expect_err("the deadline has passed");
    assert!(
        matches!(err.code(), Some(Code::DeadlineExceeded | Code::Cancelled)),
        "{err}"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Auth, health, reflection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bearer_token_is_required_but_health_and_reflection_stay_open() {
    let server =
        TestServer::start(ServerOptions::default().with_auth(BearerAuth::new("s3cret"))).await;

    let mut anonymous = server.client().await;
    let err = anonymous.get_policy(None).await.expect_err("no token");
    assert_eq!(err.code(), Some(Code::Unauthenticated));

    let mut wrong = server
        .client()
        .await
        .with_bearer_token("guess")
        .expect("a valid header");
    let err = wrong.get_policy(None).await.expect_err("wrong token");
    assert_eq!(err.code(), Some(Code::Unauthenticated));

    let mut authorized = server
        .client()
        .await
        .with_bearer_token("s3cret")
        .expect("a valid header");
    authorized.get_policy(None).await.expect("the right token");
    authorized
        .assess(simple_request("ENG-1", "Export invoices as XLSX"))
        .await
        .expect("the right token");

    let channel = tonic::transport::Endpoint::from_shared(server.url.clone())
        .expect("a url")
        .connect()
        .await
        .expect("connects");
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel.clone());
    let status = health
        .check(tonic_health::pb::HealthCheckRequest {
            service: "assessment.v1.AssessmentEngine".to_owned(),
        })
        .await
        .expect("health is open")
        .into_inner()
        .status;
    assert_eq!(
        status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::Serving)
    );

    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
    let mut reflection = ServerReflectionClient::new(channel);
    let request = tonic_reflection::pb::v1::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut responses = reflection
        .server_reflection_info(tokio_stream::iter([request]))
        .await
        .expect("reflection is open")
        .into_inner();
    let response = responses
        .message()
        .await
        .expect("a response")
        .expect("not empty");
    let Some(MessageResponse::ListServicesResponse(list)) = response.message_response else {
        panic!("expected a service list");
    };
    let names: Vec<String> = list.service.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"assessment.v1.AssessmentEngine".to_owned()));
    assert!(names.contains(&"grpc.health.v1.Health".to_owned()));

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

fn round_trip_request(request: &AssessmentRequest) -> AssessmentRequest {
    AssessmentRequest::try_from(pb::AssessRequest::from(request.clone())).expect("decodes")
}

fn round_trip_assessment(assessment: &Assessment) -> Assessment {
    Assessment::try_from(pb::Assessment::from(assessment.clone())).expect("decodes")
}

fn precise_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_793_000_123, 987_654_321).expect("in range")
}

/// Every input field type and shape, with every enum variant.
fn rich_request() -> AssessmentRequest {
    let t = precise_time();
    let full = Entity::new("ENG-1", "epic")
        .with_parent("PRJ-1")
        .with_version(SemVer::new(3, 1, 4))
        .with_title("Title")
        .with_description("- [ ] one\n- [x] two")
        .with_field("estimate", FieldValue::Number(8.5))
        .with_field("flag", FieldValue::Bool(true))
        .with_field("due_date", FieldValue::Timestamp(t))
        .with_field("priority", FieldValue::ordinal(4, 4))
        .with_field("assignee", FieldValue::reference("user-1"))
        .with_field("labels", FieldValue::set(["backend", "auth"]))
        .with_field("empty_set", FieldValue::set(Vec::<String>::new()))
        .with_relation(RelationKind::Blocks, "ENG-2")
        .with_relation(RelationKind::BlockedBy, "ENG-3")
        .with_relation(RelationKind::RelatesTo, "ENG-4")
        .with_relation(RelationKind::Duplicates, "ENG-5")
        .with_basis("PRJ-1", SemVer::new(1, 0, 0));
    let mut full = full;
    full.embedding = Some(vec![0.25, -1.5, 3.0e-8]);
    full.updated_at = Some(t);
    let statuses = StatusCategory::ALL
        .iter()
        .enumerate()
        .map(|(i, c)| {
            Entity::new(format!("S-{i}"), "custom kind")
                .with_status(*c, format!("State {i}"))
                .with_parent("ENG-1")
        })
        .collect::<Vec<_>>();
    let kinds = [
        ActivityKind::Comment,
        ActivityKind::Mention,
        ActivityKind::Attachment,
        ActivityKind::Link,
        ActivityKind::Other,
    ];
    let events = kinds
        .iter()
        .map(|k| ActivityEvent {
            entity_id: "ENG-1".to_owned(),
            kind: *k,
            author: Some("dorj".to_owned()),
            body: "We are blocked on the API, scope may grow.".to_owned(),
            at: Some(t),
        })
        .chain([ActivityEvent::comment("", "no author, no time")])
        .collect();
    AssessmentRequest {
        changes: vec![
            ChangeSet::update(full.clone(), full.clone().with_title("Title 2")).with_events(events),
            ChangeSet::create(Entity::new("NEW-1", "task")),
            ChangeSet::delete(Entity::new("OLD-1", "milestone")),
            ChangeSet::fields(
                Entity::new("ENG-9", "issue").with_estimate(5.0),
                vec![
                    FieldChange {
                        field: "estimate".to_owned(),
                        before: Some(FieldValue::Number(3.0)),
                        after: Some(FieldValue::Number(5.0)),
                    },
                    FieldChange {
                        field: "assignee".to_owned(),
                        before: None,
                        after: Some(FieldValue::reference("u")),
                    },
                    FieldChange {
                        field: "labels".to_owned(),
                        before: Some(FieldValue::set(["x"])),
                        after: None,
                    },
                ],
            ),
            ChangeSet::activity("S-0", vec![ActivityEvent::comment("S-0", "done!")]),
        ],
        context: statuses,
        history: vec![
            HistoryPoint {
                entity_id: "ENG-1".to_owned(),
                at: t,
                magnitude: 0.125,
            },
            HistoryPoint {
                entity_id: "ENG-1".to_owned(),
                at: precise_time() - chrono::Duration::days(3),
                magnitude: 0.5,
            },
        ],
        now: Some(t),
        options: AssessOptions {
            judge: None,
            judgment_texts: false,
            top_k: 0,
            explain: false,
            max_contributions: 17,
        },
    }
}

/// One judgment per relation, with every field set.
fn rich_judgments() -> Vec<TextJudgment> {
    RELATIONS
        .iter()
        .enumerate()
        .map(|(i, relation)| TextJudgment {
            field: format!("field_{i}"),
            fingerprint: fingerprint("before", &format!("after {i}")),
            relation: *relation,
            confidence: 0.125 * i as f64,
            rationale: format!("because {i}"),
            source: "reviewer".to_owned(),
        })
        .collect()
}

const RELATIONS: [TextRelation; 6] = [
    TextRelation::Equivalent,
    TextRelation::Extended,
    TextRelation::Narrowed,
    TextRelation::Replaced,
    TextRelation::Reversed,
    TextRelation::Undetermined,
];

const STATUSES: [JudgmentStatus; 7] = [
    JudgmentStatus::Requested,
    JudgmentStatus::OverBudget,
    JudgmentStatus::Applied,
    JudgmentStatus::Stale,
    JudgmentStatus::Inconclusive,
    JudgmentStatus::NotFlagged,
    JudgmentStatus::Unavailable,
];

const REASONS: [JudgmentReason; 5] = [
    JudgmentReason::NegationChanged,
    JudgmentReason::ScopeQualifier,
    JudgmentReason::TermSubstitution,
    JudgmentReason::ParaphraseSuspected,
    JudgmentReason::NearThreshold,
];

/// The judgment of the `i`-th field: none for the first, then every status
/// and relation in turn, with and without the optional parts.
fn rich_field_judgment(i: usize) -> Option<FieldJudgment> {
    if i == 0 {
        return None;
    }
    let judged = i.is_multiple_of(2);
    Some(FieldJudgment {
        status: STATUSES[i % STATUSES.len()],
        reasons: REASONS[..i % (REASONS.len() + 1)].to_vec(),
        fingerprint: fingerprint("a", &format!("b {i}")),
        lexical_magnitude: 0.05 * i as f64,
        relation: judged.then(|| RELATIONS[i % RELATIONS.len()]),
        confidence: judged.then_some(0.9),
        weight: judged.then_some(0.75),
        rationale: if judged { "same meaning" } else { "" }.to_owned(),
        source: if judged { "caller" } else { "" }.to_owned(),
        detail: if i.is_multiple_of(3) { "a detail" } else { "" }.to_owned(),
    })
}

fn rich_judgment_requests() -> Vec<JudgmentRequest> {
    vec![
        JudgmentRequest {
            change_index: 0,
            entity_id: "ENG-1".to_owned(),
            field: DESCRIPTION.to_owned(),
            fingerprint: fingerprint("a", "b"),
            reasons: REASONS.to_vec(),
            lexical_magnitude: 0.8,
            before: Some("a".to_owned()),
            after: Some(String::new()),
        },
        JudgmentRequest {
            change_index: 3,
            entity_id: "ENG-9".to_owned(),
            field: "notes".to_owned(),
            fingerprint: fingerprint("c", "d"),
            reasons: vec![JudgmentReason::NearThreshold],
            lexical_magnitude: 0.25,
            before: None,
            after: None,
        },
    ]
}

/// Every output field and every enum variant.
fn rich_assessments() -> Vec<Assessment> {
    let actions = [
        Action::None,
        Action::Notify,
        Action::Review,
        Action::UpdateScope,
        Action::ReEstimate,
        Action::Reschedule,
        Action::Reverify,
        Action::ObsoleteCandidate,
        Action::CancelOrRehome,
        Action::Reprioritize,
        Action::Replan,
        Action::ResolveConflict,
    ];
    let channels = [
        Channel::SelfChange,
        Channel::Down,
        Channel::Up,
        Channel::Dependency,
        Channel::Upstream,
        Channel::Related,
        Channel::Sibling,
    ];
    let basis = [
        BasisStatus::NotPinned,
        BasisStatus::Current,
        BasisStatus::Compatible,
        BasisStatus::ReviewSuggested,
        BasisStatus::Incompatible,
        BasisStatus::Removed,
    ];
    let levels = [
        ImpactLevel::None,
        ImpactLevel::Low,
        ImpactLevel::Medium,
        ImpactLevel::High,
        ImpactLevel::Critical,
    ];
    let change_types = [
        ChangeType::Created,
        ChangeType::Updated,
        ChangeType::Deleted,
        ChangeType::Activity,
    ];
    let bumps = [
        VersionBump::None,
        VersionBump::Patch,
        VersionBump::Minor,
        VersionBump::Major,
        VersionBump::Initial,
    ];
    let field_kinds = [
        FieldChangeKind::Added,
        FieldChangeKind::Removed,
        FieldChangeKind::Modified,
    ];
    let directions = [Direction::None, Direction::Increase, Direction::Decrease];
    let signal_categories = [
        SignalCategory::Blocker,
        SignalCategory::Scope,
        SignalCategory::Risk,
        SignalCategory::Schedule,
        SignalCategory::Uncertainty,
        SignalCategory::Resolution,
    ];
    let violations = [
        ViolationKind::OrphanedWork,
        ViolationKind::CyclicDependency,
        ViolationKind::ScheduleConflict,
        ViolationKind::DependencyConflict,
        ViolationKind::BlockedByClosed,
        ViolationKind::PrematureClose,
        ViolationKind::EstimateOverflow,
        ViolationKind::Overdue,
    ];
    let recommendations = [
        Recommendation::NoAction,
        Recommendation::Notify,
        Recommendation::Review,
        Recommendation::PartialReplan,
        Recommendation::FullReplan,
    ];

    let fields: Vec<FieldAssessment> = field_kinds
        .iter()
        .flat_map(|k| directions.iter().map(move |d| (*k, *d)))
        .enumerate()
        .map(|(i, (change, direction))| FieldAssessment {
            judgment: rich_field_judgment(i),
            field: format!("field_{i}"),
            change,
            direction,
            magnitude: 0.1 * i as f64,
            severity: 0.9,
            risk: 0.09 * i as f64,
            breaking: i % 2 == 0,
            additive: i % 3 == 0,
            before: (i % 2 == 0).then(|| format!("before {i}")),
            after: (i % 3 != 0).then(|| format!("after {i}")),
            metrics: [
                ("tfidf_cosine".to_owned(), 0.5),
                ("ratio".to_owned(), -1.25),
            ]
            .into_iter()
            .collect(),
            explanation: "changed".to_owned(),
        })
        .collect();
    let signals: Vec<Signal> = signal_categories
        .iter()
        .map(|c| Signal {
            category: *c,
            weight: 0.4,
            terms: vec!["blocked".to_owned(), "waiting".to_owned()],
            excerpt: "we are blocked".to_owned(),
        })
        .collect();
    let changes: Vec<ChangeAssessment> = change_types
        .iter()
        .zip(bumps.iter().cycle())
        .enumerate()
        .map(|(i, (change_type, bump))| ChangeAssessment {
            entity_id: format!("ENG-{i}"),
            kind: EntityKind::from(["issue", "sub_issue", "epic", "Whatever"][i]),
            change_type: *change_type,
            raw_score: 0.2,
            local_score: 0.3,
            modifiers: vec![Modifier {
                name: "phase".to_owned(),
                factor: 1.4,
                detail: "in progress".to_owned(),
            }],
            fields: fields.clone(),
            signals: signals.clone(),
            bump: *bump,
            previous_version: (i % 2 == 0).then(|| SemVer::new(1, 2, 3)),
            next_version: (i != 1).then(|| SemVer::new(2, 0, 0)),
            anomaly_z: (i % 2 == 1).then_some(2.75),
            explanation: "rewritten".to_owned(),
            propagation_truncated: i % 2 == 0,
        })
        .chain([ChangeAssessment {
            entity_id: "ENG-last".to_owned(),
            kind: EntityKind::Initiative,
            change_type: ChangeType::Updated,
            raw_score: 0.0,
            local_score: 0.0,
            modifiers: Vec::new(),
            fields: Vec::new(),
            signals: Vec::new(),
            bump: VersionBump::Initial,
            previous_version: None,
            next_version: None,
            anomaly_z: None,
            explanation: String::new(),
            propagation_truncated: false,
        }])
        .collect();
    let impacts: Vec<EntityImpact> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| EntityImpact {
            entity_id: format!("E-{i}"),
            kind: EntityKind::from(["project", "milestone", "task"][i % 3]),
            rank: u32::try_from(i + 1).expect("small"),
            score: 0.05 * i as f64,
            level: levels[i % levels.len()],
            action: *action,
            channel: channels[i % channels.len()],
            distance: u32::try_from(i).expect("small"),
            changed: i == 0,
            basis_status: basis[i % basis.len()],
            basis_introduced: i % 3 == 0,
            effort: 2.5,
            effort_at_risk: 0.125 * i as f64,
            contributions: vec![Contribution {
                source_id: "ENG-0".to_owned(),
                field: "description".to_owned(),
                channel: channels[(i + 3) % channels.len()],
                path: vec!["ENG-0".to_owned(), format!("E-{i}")],
                value: 0.33,
                detail: "shares text".to_owned(),
            }],
            reasons: vec![format!("reason {i}")],
        })
        .collect();
    let violations: Vec<Violation> = violations
        .iter()
        .enumerate()
        .map(|(i, kind)| Violation {
            kind: *kind,
            entities: vec![format!("E-{i}"), "ENG-0".to_owned()],
            severity: 0.6,
            introduced: i % 2 == 0,
            message: "conflict".to_owned(),
        })
        .collect();
    recommendations
        .iter()
        .enumerate()
        .map(|(i, recommendation)| Assessment {
            judgment_requests: rich_judgment_requests(),
            unmatched_judgments: vec![
                JudgmentKey {
                    entity_id: "ENG-1".to_owned(),
                    field: DESCRIPTION.to_owned(),
                },
                JudgmentKey {
                    entity_id: String::new(),
                    field: "notes".to_owned(),
                },
            ],
            score: 0.71,
            level: levels[i],
            recommendation: *recommendation,
            confidence: 0.8,
            severity: 0.9,
            breadth: 0.4,
            effort_at_risk: 6.5,
            effort_total: 21.0,
            replan_root: (i % 2 == 0).then(|| "ENG-12".to_owned()),
            changes: changes.clone(),
            impacts: impacts.clone(),
            impacts_total: 99,
            violations: violations.clone(),
            summary: "summary".to_owned(),
            engine_version: assessment_core::ENGINE_VERSION.to_owned(),
            assessed_at: precise_time(),
        })
        .collect()
}

#[test]
fn rich_request_round_trips() {
    let mut request = rich_request();
    request.changes[0].judgments = rich_judgments();
    request.options.judgment_texts = true;
    for judge in [None, Some(false), Some(true)] {
        request.options.judge = judge;
        assert_eq!(round_trip_request(&request), request);
    }

    let input = AssessInput {
        request_id: "id-7".to_owned(),
        request: request.clone(),
        policy_overrides:
            serde_json::json!({"thresholds": {"high": 0.4}, "propagation": {"max_depth": 3}})
                .as_object()
                .cloned(),
    };
    let back = AssessInput::try_from(pb::AssessRequest::from(input.clone())).expect("decodes");
    assert_eq!(back, input);
}

#[test]
fn rich_assessments_round_trip() {
    for assessment in rich_assessments() {
        assert_eq!(round_trip_assessment(&assessment), assessment);
    }
    // And a real one.
    let real = Engine::default()
        .assess(&rewrite_request())
        .expect("assesses");
    assert!(!real.impacts.is_empty());
    assert_eq!(round_trip_assessment(&real), real);
}

#[test]
fn registry_and_text_types_round_trip() {
    let assessment = rich_assessments().remove(0);
    let outcome = PutOutcome {
        entity: rich_request().changes[0].after.clone().expect("an after"),
        created: true,
        stored: false,
        bump: VersionBump::Minor,
        assessment,
        unchanged: false,
        skipped_stale: false,
    };
    let back = PutOutcome::try_from(pb::PutEntityResponse::from(outcome.clone())).expect("decodes");
    assert_eq!(back, outcome);
    for (unchanged, skipped_stale) in [(true, false), (false, true)] {
        let outcome = PutOutcome {
            unchanged,
            skipped_stale,
            ..outcome.clone()
        };
        let back =
            PutOutcome::try_from(pb::PutEntityResponse::from(outcome.clone())).expect("decodes");
        assert_eq!(back, outcome);
    }

    for (snapshot, change_type) in [
        (Some(Entity::new("A", "issue")), ChangeType::Created),
        (None, ChangeType::Deleted),
    ] {
        let record = VersionRecord {
            judgments: rich_judgments(),
            version: SemVer::new(2, 1, 0),
            bump: VersionBump::Minor,
            at: precise_time(),
            change_type,
            raw_score: 0.42,
            level: ImpactLevel::Low,
            changed_fields: vec!["title".to_owned(), "labels".to_owned()],
            snapshot,
        };
        let back =
            VersionRecord::try_from(pb::VersionRecord::from(record.clone())).expect("decodes");
        assert_eq!(back, record);
    }

    for current in [Some(SemVer::new(2, 0, 0)), None] {
        let entry = StaleEntry {
            entity_id: "A".to_owned(),
            basis_entity_id: "B".to_owned(),
            pinned: SemVer::new(1, 0, 0),
            current,
            status: BasisStatus::Incompatible,
        };
        let back = StaleEntry::try_from(pb::StaleEntry::from(entry.clone())).expect("decodes");
        assert_eq!(back, entry);
    }

    let options = PutOptions {
        judgments: rich_judgments(),
        hold_if_flagged: true,
        dry_run: true,
        expected_version: Some(SemVer::new(1, 2, 3)),
        events: vec![ActivityEvent::comment("ENG-1", "hi")],
        now: Some(precise_time()),
        auto_pin: false,
        assess: AssessOptions {
            judge: Some(true),
            judgment_texts: true,
            top_k: 3,
            explain: false,
            max_contributions: 1,
        },
        source_seq: None,
    };
    let entity = Entity::new("ENG-1", "issue");
    let (e, o) =
        convert::put_request_from_pb(convert::put_request_to_pb(entity.clone(), options.clone()))
            .expect("decodes");
    assert_eq!((e, o), (entity, options));

    let diff = TextDiff {
        gray_zone: Some(GrayZone {
            reasons: REASONS.to_vec(),
            fingerprint: fingerprint("a", "b"),
        }),
        magnitude: 0.5,
        lexical_similarity: 0.4,
        char_similarity: Some(0.3),
        token_jaccard: 0.2,
        shingle_similarity: 0.1,
        tfidf_cosine: 0.6,
        embedding_cosine: None,
        structure: StructureDiff {
            items_before: 1,
            items_after: 2,
            items_added: 3,
            items_removed: 4,
            items_modified: 5,
            items_checked: 6,
            items_unchecked: 7,
            headings_added: 8,
            headings_removed: 9,
        },
        negation_flips: 1,
        modal_shifts: 2,
        numbers_changed: 3,
        added_terms: vec!["oauth".to_owned()],
        removed_terms: vec!["password".to_owned()],
        chars_before: 120,
        chars_after: 140,
        low_information: true,
        passages_added: 2,
        passages_removed: 1,
        passages_modified: 3,
        reordered: false,
    };
    let back = TextDiff::try_from(pb::CompareTextResponse::from(diff.clone())).expect("decodes");
    assert_eq!(back, diff);
    let plain = TextDiff {
        gray_zone: None,
        ..diff
    };
    let back = TextDiff::try_from(pb::CompareTextResponse::from(plain.clone())).expect("decodes");
    assert_eq!(back, plain);
}

#[test]
fn judgment_responses_decode_leniently() {
    let judged = rich_field_judgment(2).expect("a judgment");

    // A status this client does not know reads as unavailable, with a note.
    for (status, detail, expected) in [
        (42, "", "unknown judgment status 42"),
        (42, "busy", "unknown judgment status 42: busy"),
        (0, "", "unknown judgment status 0"),
    ] {
        let message = pb::FieldJudgment {
            status,
            detail: detail.to_owned(),
            ..pb::FieldJudgment::from(judged.clone())
        };
        let decoded = FieldJudgment::from(message);
        assert_eq!(decoded.status, JudgmentStatus::Unavailable);
        assert_eq!(decoded.detail, expected);
        assert_eq!(decoded.fingerprint, judged.fingerprint);
    }

    // An unknown or unset relation is no relation.
    for relation in [0, 42] {
        let message = pb::FieldJudgment {
            relation,
            ..pb::FieldJudgment::from(judged.clone())
        };
        assert_eq!(FieldJudgment::from(message).relation, None);
    }

    // Reasons this client does not know are dropped.
    let message = pb::FieldJudgment {
        reasons: vec![
            "term_substitution".to_owned(),
            "a_new_reason".to_owned(),
            "near_threshold".to_owned(),
        ],
        ..pb::FieldJudgment::from(judged)
    };
    assert_eq!(
        FieldJudgment::from(message).reasons,
        vec![
            JudgmentReason::TermSubstitution,
            JudgmentReason::NearThreshold
        ]
    );
    let zone = pb::GrayZone {
        reasons: vec!["a_new_reason".to_owned(), "scope_qualifier".to_owned()],
        fingerprint: "f".to_owned(),
    };
    assert_eq!(
        GrayZone::from(zone).reasons,
        vec![JudgmentReason::ScopeQualifier]
    );

    // A stored judgment with an unknown relation is undetermined.
    let mut record = pb::VersionRecord::from(VersionRecord {
        judgments: rich_judgments(),
        version: SemVer::new(1, 0, 1),
        bump: VersionBump::Patch,
        at: precise_time(),
        change_type: ChangeType::Updated,
        raw_score: 0.1,
        level: ImpactLevel::Low,
        changed_fields: vec![DESCRIPTION.to_owned()],
        snapshot: None,
    });
    record.judgments[0].relation = 42;
    let decoded = VersionRecord::try_from(record).expect("decodes");
    assert_eq!(decoded.judgments[0].relation, TextRelation::Undetermined);
    assert_eq!(decoded.judgments[1].relation, TextRelation::Extended);
}

#[test]
fn request_judgments_decode_strictly() {
    let good = pb::TextJudgment::from(rich_judgments().remove(0));
    let decode = |judgment: pb::TextJudgment| {
        let mut request = pb::AssessRequest::from(simple_request("A", "t"));
        request.changes[0].judgments = vec![good.clone(), judgment];
        AssessmentRequest::try_from(request)
            .expect_err("is rejected")
            .to_string()
    };
    assert_eq!(
        decode(pb::TextJudgment {
            relation: 0,
            ..good.clone()
        }),
        "changes[0].judgments[1].relation: text relation is unspecified"
    );
    assert!(
        decode(pb::TextJudgment {
            relation: 42,
            ..good.clone()
        })
        .starts_with("changes[0].judgments[1].relation: ")
    );
    for confidence in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
        let message = decode(pb::TextJudgment {
            confidence: Some(confidence),
            ..good.clone()
        });
        assert!(
            message.starts_with("changes[0].judgments[1].confidence: "),
            "{message}"
        );
    }
}

#[test]
fn proto_defaults_apply() {
    // An absent AssessOptions takes the core defaults.
    let request = pb::AssessRequest {
        changes: vec![pb::ChangeSet::from(ChangeSet::create(Entity::new(
            "A", "issue",
        )))],
        ..Default::default()
    };
    let decoded = AssessmentRequest::try_from(request).expect("decodes");
    assert_eq!(decoded.options, AssessOptions::default());
    assert_eq!(decoded.options.judge, None);
    assert!(decoded.changes[0].judgments.is_empty());

    // A judgment without a confidence is fully trusted.
    let judgment = pb::TextJudgment {
        field: DESCRIPTION.to_owned(),
        fingerprint: fingerprint("a", "b"),
        relation: pb::TextRelation::Equivalent.into(),
        ..Default::default()
    };
    let decoded = TextJudgment::try_from(judgment).expect("decodes");
    assert_eq!(decoded.confidence, 1.0);
    assert_eq!(decoded.relation, TextRelation::Equivalent);

    // A FieldValue without a kind is null: the field is omitted.
    let mut entity = pb::Entity::from(Entity::new("A", "issue").with_title("t"));
    entity
        .fields
        .insert("cleared".to_owned(), pb::FieldValue { kind: None });
    let decoded = Entity::try_from(entity).expect("decodes");
    assert_eq!(decoded.fields.len(), 1);

    // ACTIVITY_KIND_UNSPECIFIED is a comment.
    let event = pb::ActivityEvent {
        entity_id: "A".to_owned(),
        kind: pb::ActivityKind::Unspecified.into(),
        ..Default::default()
    };
    let decoded = ActivityEvent::try_from(event).expect("decodes");
    assert_eq!(decoded.kind, ActivityKind::Comment);
    assert_eq!(decoded.author, None);

    // Unknown enum values are errors, with their path.
    let relation = pb::Relation {
        kind: 42,
        target_id: "B".to_owned(),
    };
    let err = Relation::try_from(relation).expect_err("unknown");
    assert_eq!(
        err.to_string(),
        "kind: relation kind has the unknown value 42"
    );
}

#[test]
fn import_and_delete_messages_round_trip() {
    let entity = rich_request().changes[0].after.clone().expect("an after");

    let put = PutOptions {
        source_seq: Some(u64::MAX),
        expected_version: Some(SemVer::new(1, 2, 3)),
        now: Some(precise_time()),
        ..PutOptions::default()
    };
    let (back_entity, back_put) =
        convert::put_request_from_pb(convert::put_request_to_pb(entity.clone(), put.clone()))
            .expect("decodes");
    assert_eq!(back_entity, entity);
    assert_eq!(back_put, put);

    for options in [
        DeleteOptions::default(),
        DeleteOptions {
            dry_run: true,
            expected_version: Some(SemVer::new(4, 0, 1)),
            now: Some(precise_time()),
            source_seq: Some(77),
        },
    ] {
        let (id, back) = convert::delete_request_from_pb(convert::delete_request_to_pb(
            "ENG-1",
            options.clone(),
        ))
        .expect("decodes");
        assert_eq!(id, "ENG-1");
        assert_eq!(back, options);
    }
    // A request from a client that does not know `source_seq`.
    let (_, old) = convert::delete_request_from_pb(pb::DeleteEntityRequest {
        id: "ENG-1".to_owned(),
        ..Default::default()
    })
    .expect("decodes");
    assert_eq!(old, DeleteOptions::default());

    for (deleted, skipped_stale) in [(true, false), (false, true), (false, false)] {
        let outcome = DeleteOutcome {
            deleted,
            assessment: rich_assessments().remove(0),
            skipped_stale,
        };
        let back = DeleteOutcome::try_from(pb::DeleteEntityResponse::from(outcome.clone()))
            .expect("decodes");
        assert_eq!(back, outcome);
    }
    let err = DeleteOutcome::try_from(pb::DeleteEntityResponse::default())
        .expect_err("an assessment is required");
    assert!(err.to_string().contains("assessment"), "{err}");

    for item in [
        ImportItem::upsert(entity.clone()),
        ImportItem::upsert(entity.clone()).with_source_seq(0),
        ImportItem::delete("ENG-1"),
        ImportItem::delete("ENG-1").with_source_seq(42),
    ] {
        let back = ImportItem::try_from(pb::ImportRequest::from(item.clone())).expect("decodes");
        assert_eq!(back, item);
    }
    // Only the id of a deleted entity is read.
    let deletion = pb::ImportRequest {
        entity: Some(entity.clone().into()),
        source_seq: Some(3),
        deleted: true,
    };
    assert_eq!(
        ImportItem::try_from(deletion).expect("decodes"),
        ImportItem::delete(entity.id.clone()).with_source_seq(3)
    );
    let err = ImportItem::try_from(pb::ImportRequest::default()).expect_err("no entity");
    assert_eq!(err.to_string(), "entity: is required");

    let summary = ImportSummary {
        created: 1,
        unchanged: 2,
        assessed: 3,
        skipped_stale: 4,
        deleted: 5,
        failed: vec![
            ImportFailure {
                id: "ENG-7".to_owned(),
                error: "entity: is required".to_owned(),
            },
            ImportFailure {
                id: String::new(),
                error: "the entity id is empty".to_owned(),
            },
        ],
    };
    let message = pb::ImportResponse::from(summary.clone());
    assert_eq!(message.failed_total, 2);
    assert_eq!(ImportSummary::from(message), summary);
    assert_eq!(
        ImportSummary::from(pb::ImportResponse::default()),
        ImportSummary::default()
    );
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn arb_time() -> impl Strategy<Value = DateTime<Utc>> {
    // 0001-01-01 … 9999-12-31, the protobuf Timestamp range.
    (
        -62_135_596_800_i64..253_402_300_799_i64,
        0_u32..1_000_000_000,
    )
        .prop_map(|(s, n)| DateTime::from_timestamp(s, n).expect("in range"))
}

fn arb_status() -> impl Strategy<Value = StatusCategory> {
    prop::sample::select(StatusCategory::ALL.to_vec())
}

fn arb_field_value() -> impl Strategy<Value = FieldValue> {
    prop_oneof![
        any::<String>().prop_map(FieldValue::Text),
        // NaN is not equal to itself (and the engine rejects it anyway).
        any::<f64>()
            .prop_filter("finite", |n| n.is_finite())
            .prop_map(FieldValue::Number),
        any::<bool>().prop_map(FieldValue::Bool),
        arb_time().prop_map(FieldValue::Timestamp),
        (arb_status(), any::<String>()).prop_map(|(c, n)| FieldValue::status(c, n)),
        (any::<u32>(), any::<u32>()).prop_map(|(r, s)| FieldValue::ordinal(r, s)),
        any::<String>().prop_map(FieldValue::Reference),
        prop::collection::btree_set(any::<String>(), 0..4).prop_map(FieldValue::Set),
    ]
}

fn arb_version() -> impl Strategy<Value = SemVer> {
    (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(a, b, c)| SemVer::new(a, b, c))
}

fn arb_kind() -> impl Strategy<Value = EntityKind> {
    prop_oneof![
        Just("issue".to_owned()),
        Just("sub_issue".to_owned()),
        Just("project".to_owned()),
        "[a-zA-Z_ -]{0,10}",
    ]
    .prop_map(EntityKind::from)
}

fn arb_relation() -> impl Strategy<Value = Relation> {
    (
        prop::sample::select(vec![
            RelationKind::Blocks,
            RelationKind::BlockedBy,
            RelationKind::RelatesTo,
            RelationKind::Duplicates,
        ]),
        "[A-Z]{3}-[0-9]{1,3}",
    )
        .prop_map(|(kind, target)| Relation { kind, target })
}

fn arb_entity() -> impl Strategy<Value = Entity> {
    (
        "[A-Z]{3}-[0-9]{1,3}",
        arb_kind(),
        prop::option::of("[A-Z]{3}-[0-9]{1,3}"),
        prop::option::of(arb_version()),
        prop::collection::btree_map("[a-z_]{1,10}", arb_field_value(), 0..6),
        prop::collection::vec(arb_relation(), 0..3),
        prop::collection::vec(
            ("[A-Z]{3}-[0-9]{1,3}", arb_version())
                .prop_map(|(entity_id, version)| BasisPin { entity_id, version }),
            0..3,
        ),
        prop::option::of(prop::collection::vec(-1.0e6_f32..1.0e6, 1..6)),
        prop::option::of(arb_time()),
    )
        .prop_map(
            |(id, kind, parent_id, version, fields, relations, basis, embedding, updated_at)| {
                Entity {
                    id,
                    kind,
                    parent_id,
                    version,
                    fields,
                    relations,
                    basis,
                    embedding,
                    updated_at,
                }
            },
        )
}

fn arb_event() -> impl Strategy<Value = ActivityEvent> {
    (
        "[A-Z]{0,3}",
        prop::sample::select(vec![
            ActivityKind::Comment,
            ActivityKind::Mention,
            ActivityKind::Attachment,
            ActivityKind::Link,
            ActivityKind::Other,
        ]),
        prop::option::of("[a-z]{1,6}"),
        any::<String>(),
        prop::option::of(arb_time()),
    )
        .prop_map(|(entity_id, kind, author, body, at)| ActivityEvent {
            entity_id,
            kind,
            author,
            body,
            at,
        })
}

fn arb_judgment() -> impl Strategy<Value = TextJudgment> {
    (
        "[a-z_]{1,8}",
        "[0-9a-f]{0,16}",
        prop::sample::select(RELATIONS.to_vec()),
        0.0_f64..=1.0,
        ".{0,12}",
        "[a-z]{0,6}",
    )
        .prop_map(
            |(field, fingerprint, relation, confidence, rationale, source)| TextJudgment {
                field,
                fingerprint,
                relation,
                confidence,
                rationale,
                source,
            },
        )
}

fn arb_change() -> impl Strategy<Value = ChangeSet> {
    (
        "[A-Z]{0,3}",
        prop::option::of(arb_entity()),
        prop::option::of(arb_entity()),
        prop::collection::vec(
            (
                "[a-z_]{1,8}",
                prop::option::of(arb_field_value()),
                prop::option::of(arb_field_value()),
            )
                .prop_map(|(field, before, after)| FieldChange {
                    field,
                    before,
                    after,
                }),
            0..3,
        ),
        prop::collection::vec(arb_event(), 0..3),
        prop::collection::vec(arb_judgment(), 0..3),
    )
        .prop_map(
            |(entity_id, before, after, field_changes, events, judgments)| ChangeSet {
                judgments,
                entity_id,
                before,
                after,
                field_changes,
                events,
            },
        )
}

fn arb_request() -> impl Strategy<Value = AssessmentRequest> {
    (
        prop::collection::vec(arb_change(), 0..3),
        prop::collection::vec(arb_entity(), 0..3),
        prop::collection::vec(
            ("[A-Z]{3}", arb_time(), 0.0_f64..1.0).prop_map(|(entity_id, at, magnitude)| {
                HistoryPoint {
                    entity_id,
                    at,
                    magnitude,
                }
            }),
            0..3,
        ),
        prop::option::of(arb_time()),
        (
            any::<u32>(),
            any::<bool>(),
            any::<u32>(),
            prop::option::of(any::<bool>()),
            any::<bool>(),
        ),
    )
        .prop_map(
            |(
                changes,
                context,
                history,
                now,
                (top_k, explain, max_contributions, judge, judgment_texts),
            )| {
                AssessmentRequest {
                    changes,
                    context,
                    history,
                    now,
                    options: AssessOptions {
                        judge,
                        judgment_texts,
                        top_k: top_k as usize,
                        explain,
                        max_contributions: max_contributions as usize,
                    },
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn requests_round_trip(request in arb_request()) {
        prop_assert_eq!(round_trip_request(&request), request);
    }

    #[test]
    fn import_items_round_trip(
        entity in arb_entity(),
        source_seq in proptest::option::of(any::<u64>()),
    ) {
        let item = ImportItem { entity, source_seq, deleted: false };
        let back = ImportItem::try_from(pb::ImportRequest::from(item.clone())).expect("valid");
        prop_assert_eq!(back, item);
    }

    #[test]
    fn timestamps_round_trip(time in arb_time()) {
        let back = convert::timestamp_from_pb(convert::timestamp_to_pb(time)).expect("valid");
        prop_assert_eq!(back, time);
    }
}
