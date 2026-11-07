//! End-to-end tests: the real server (health, reflection, auth, limits) on
//! `127.0.0.1:0`, exercised through [`AssessmentClient`], plus core → pb →
//! core round-trip properties.

use std::time::Duration;

use assessment_core::assessment::{
    Action, Assessment, BasisStatus, ChangeAssessment, ChangeType, Channel, Contribution,
    Direction, EntityImpact, FieldAssessment, FieldChangeKind, ImpactLevel, Modifier,
    Recommendation, Signal, SignalCategory, Violation, ViolationKind,
};
use assessment_core::model::{
    ActivityEvent, ActivityKind, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity,
    EntityKind, FieldChange, FieldValue, HistoryPoint, Relation, RelationKind, StatusCategory,
};
use assessment_core::policy::Policy;
use assessment_core::registry::{PutOptions, PutOutcome, StaleEntry, VersionRecord};
use assessment_core::semver::{SemVer, VersionBump};
use assessment_core::text::{StructureDiff, TextDiff};
use assessment_core::{Engine, ErrorKind};
use assessment_grpc::auth::BearerAuth;
use assessment_grpc::client::{AssessmentClient, ClientErrorKind, ItemResponse};
use assessment_grpc::convert::{self, AssessInput};
use assessment_grpc::pb;
use assessment_grpc::server::{ServeError, ServerOptions, serve_with_incoming_shutdown};
use assessment_grpc::service::AssessmentService;
use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a free port");
        let addr = listener.local_addr().expect("has an address");
        let incoming = TcpListenerStream::new(listener);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_with_incoming_shutdown(
            AssessmentService::in_memory(),
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
            top_k: 0,
            explain: false,
            max_contributions: 17,
        },
    }
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
    let request = rich_request();
    assert_eq!(round_trip_request(&request), request);

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
    };
    let back = PutOutcome::try_from(pb::PutEntityResponse::from(outcome.clone())).expect("decodes");
    assert_eq!(back, outcome);

    for (snapshot, change_type) in [
        (Some(Entity::new("A", "issue")), ChangeType::Created),
        (None, ChangeType::Deleted),
    ] {
        let record = VersionRecord {
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
        dry_run: true,
        expected_version: Some(SemVer::new(1, 2, 3)),
        events: vec![ActivityEvent::comment("ENG-1", "hi")],
        now: Some(precise_time()),
        auto_pin: false,
        assess: AssessOptions {
            top_k: 3,
            explain: false,
            max_contributions: 1,
        },
    };
    let entity = Entity::new("ENG-1", "issue");
    let (e, o) =
        convert::put_request_from_pb(convert::put_request_to_pb(entity.clone(), options.clone()))
            .expect("decodes");
    assert_eq!((e, o), (entity, options));

    let diff = TextDiff {
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
    )
        .prop_map(
            |(entity_id, before, after, field_changes, events)| ChangeSet {
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
        (any::<u32>(), any::<bool>(), any::<u32>()),
    )
        .prop_map(
            |(changes, context, history, now, (top_k, explain, max_contributions))| {
                AssessmentRequest {
                    changes,
                    context,
                    history,
                    now,
                    options: AssessOptions {
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
    fn timestamps_round_trip(time in arb_time()) {
        let back = convert::timestamp_from_pb(convert::timestamp_to_pb(time)).expect("valid");
        prop_assert_eq!(back, time);
    }
}
