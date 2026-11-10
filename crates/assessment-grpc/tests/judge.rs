//! End-to-end tests of the judged flow: a real server with a fake judge
//! that records what it is asked. No test reaches a network judge.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assessment_core::Engine;
use assessment_core::assessment::{Assessment, FieldJudgment, JudgmentStatus};
use assessment_core::model::field::{DESCRIPTION, TITLE};
use assessment_core::model::{
    ActivityEvent, ActivityKind, AssessmentRequest, ChangeSet, Entity, FieldValue, StatusCategory,
    TextJudgment, TextRelation,
};
use assessment_core::registry::{ImportItem, PutOptions};
use assessment_core::semver::{SemVer, VersionBump};
use assessment_grpc::flow::JudgeSettings;
use assessment_grpc::server::ServerOptions;
use assessment_grpc::service::{AssessmentService, ServiceLimits};
use assessment_judge::{
    BoxFuture, CachedJudge, Judge, JudgeError, JudgeErrorKind, Question, Verdict,
};
use chrono::{DateTime, TimeZone, Utc};
use common::TestServer;

// ---------------------------------------------------------------------------
// The fake judge
// ---------------------------------------------------------------------------

const FAKE: &str = "fake/judge";

/// The context title that makes the fake judge slow.
const SLOW_TITLE: &str = "Slow parent";

#[derive(Debug, Clone)]
enum Reply {
    Relation(TextRelation),
    Error(JudgeErrorKind, &'static str, Option<u16>),
}

/// Records every question. Answers after `delay`, or after `slow_delay`
/// when the question is about the entity titled [`SLOW_TITLE`].
#[derive(Debug)]
struct FakeJudge {
    reply: Reply,
    delay: Duration,
    slow_delay: Duration,
    questions: Mutex<Vec<Question>>,
    /// Questions dropped before they were answered.
    cancelled: Arc<AtomicUsize>,
}

impl FakeJudge {
    fn new(reply: Reply) -> Self {
        Self {
            reply,
            delay: Duration::ZERO,
            slow_delay: Duration::ZERO,
            questions: Mutex::new(Vec::new()),
            cancelled: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn equivalent() -> Self {
        Self::new(Reply::Relation(TextRelation::Equivalent))
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self.slow_delay = delay;
        self
    }

    fn with_slow_delay(mut self, delay: Duration) -> Self {
        self.slow_delay = delay;
        self
    }

    fn questions(&self) -> Vec<Question> {
        self.questions.lock().expect("not poisoned").clone()
    }

    fn calls(&self) -> usize {
        self.questions.lock().expect("not poisoned").len()
    }
}

/// Counts a question that is dropped before its answer.
struct Open {
    cancelled: Arc<AtomicUsize>,
    answered: bool,
}

impl Drop for Open {
    fn drop(&mut self) {
        if !self.answered {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Judge for FakeJudge {
    fn id(&self) -> &str {
        FAKE
    }

    fn judge<'a>(
        &'a self,
        question: &'a Question,
        _deadline: tokio::time::Instant,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(async move {
            self.questions
                .lock()
                .expect("not poisoned")
                .push(question.clone());
            let mut open = Open {
                cancelled: Arc::clone(&self.cancelled),
                answered: false,
            };
            let slow = question.context_title.as_deref() == Some(SLOW_TITLE);
            let delay = if slow { self.slow_delay } else { self.delay };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            open.answered = true;
            match &self.reply {
                Reply::Relation(relation) => {
                    Ok(Verdict::new(*relation, 0.9, "the same requirement", FAKE))
                }
                Reply::Error(kind, message, status) => {
                    let error = JudgeError::new(*kind, *message);
                    Err(match status {
                        Some(status) => error.with_http_status(*status),
                        None => error,
                    })
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PARAPHRASE: (&str, &str) = (
    "Users can log in with their email address and password.",
    "People are able to sign in using e-mail and a password.",
);

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0)
        .single()
        .expect("a valid time")
}

fn issue(id: &str, title: &str, description: &str) -> Entity {
    Entity::new(id, "issue")
        .with_title(title)
        .with_description(description)
        .with_status(StatusCategory::Started, "In Progress")
}

fn child(description: &str) -> Entity {
    Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("Child task")
        .with_description(description)
        .with_status(StatusCategory::Started, "In Progress")
}

/// The description of issue `id` goes from `pair.0` to `pair.1`.
fn change(id: &str, title: &str, pair: (&str, &str)) -> ChangeSet {
    let v1 = SemVer::new(1, 0, 0);
    ChangeSet::update(
        issue(id, title, pair.0).with_version(v1),
        issue(id, title, pair.1).with_version(v1),
    )
}

/// The parent's description changes; one sub-issue was planned against the
/// old text.
fn request_for(title: &str, pair: (&str, &str)) -> AssessmentRequest {
    AssessmentRequest {
        changes: vec![change("P", title, pair)],
        context: vec![child(pair.0).with_basis("P", SemVer::new(1, 0, 0))],
        now: Some(at(9, 9)),
        ..Default::default()
    }
}

fn paraphrase() -> AssessmentRequest {
    request_for("Parent issue", PARAPHRASE)
}

fn judgment_of<'a>(assessment: &'a Assessment, entity: &str, field: &str) -> &'a FieldJudgment {
    assessment
        .changes
        .iter()
        .find(|c| c.entity_id == entity)
        .expect("the entity changed")
        .fields
        .iter()
        .find(|f| f.field == field)
        .expect("the field changed")
        .judgment
        .as_ref()
        .expect("the field has a judgment")
}

fn description(assessment: &Assessment) -> &FieldJudgment {
    judgment_of(assessment, "P", DESCRIPTION)
}

/// `assessment` with the judgment of its first description put back to
/// `judgment`: what is left must be the same as without a judge.
fn with_description_judgment(assessment: &Assessment, judgment: &FieldJudgment) -> Assessment {
    let mut out = assessment.clone();
    out.changes[0]
        .fields
        .iter_mut()
        .find(|f| f.field == DESCRIPTION)
        .expect("the description changed")
        .judgment = Some(judgment.clone());
    out
}

fn service(judge: Arc<dyn Judge>, settings: JudgeSettings) -> AssessmentService {
    AssessmentService::in_memory().with_judge(judge, settings)
}

async fn server(judge: &Arc<FakeJudge>) -> TestServer {
    server_with(judge, JudgeSettings::default(), ServerOptions::default()).await
}

async fn server_with(
    judge: &Arc<FakeJudge>,
    settings: JudgeSettings,
    options: ServerOptions,
) -> TestServer {
    let judge: Arc<dyn Judge> = Arc::clone(judge) as Arc<dyn Judge>;
    TestServer::start_with(service(judge, settings), options).await
}

// ---------------------------------------------------------------------------
// Assess
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_paraphrase_judged_equivalent_is_a_patch() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;
    assert_eq!(
        client.judge_info().await.expect("answers"),
        Some(FAKE.to_owned())
    );

    let lexical = Engine::default().assess(&paraphrase()).expect("assesses");
    assert_eq!(lexical.changes[0].bump, VersionBump::Major);

    let judged = client.assess(paraphrase()).await.expect("assesses");
    assert_eq!(judged.changes[0].bump, VersionBump::Patch);
    let applied = description(&judged);
    assert_eq!(applied.status, JudgmentStatus::Applied);
    assert_eq!(applied.relation, Some(TextRelation::Equivalent));
    assert_eq!(applied.source, FAKE);
    assert_eq!(fake.calls(), 1);
    // The caller did not ask for the texts.
    assert!(
        judged
            .judgment_requests
            .iter()
            .all(|r| r.before.is_none() && r.after.is_none())
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_judge_error_leaves_the_lexical_result() {
    let fake = Arc::new(FakeJudge::new(Reply::Error(
        JudgeErrorKind::Api,
        "overloaded",
        Some(529),
    )));
    let server = server(&fake).await;
    let mut client = server.client().await;

    let lexical = Engine::default().assess(&paraphrase()).expect("assesses");
    let judged = client.assess(paraphrase()).await.expect("never fails");
    assert_eq!(fake.calls(), 1);
    let unanswered = description(&judged);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "HTTP 529 overloaded");
    assert_eq!(
        with_description_judgment(&judged, description(&lexical)),
        lexical
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_refusal_and_a_spend_cap_are_named() {
    for (kind, message, detail) in [
        (JudgeErrorKind::Refused, "cyber", "refused (cyber)"),
        (JudgeErrorKind::SpendCap, "over budget", "spend cap reached"),
        (
            JudgeErrorKind::ShuttingDown,
            "stopping",
            "server shutting down",
        ),
    ] {
        let fake = Arc::new(FakeJudge::new(Reply::Error(kind, message, None)));
        let server = server(&fake).await;
        let mut client = server.client().await;
        let judged = client.assess(paraphrase()).await.expect("never fails");
        assert_eq!(description(&judged).status, JudgmentStatus::Unavailable);
        assert_eq!(description(&judged).detail, detail);
        server.shutdown().await;
    }
}

#[tokio::test]
async fn judge_false_is_the_engine_result() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;

    let mut request = paraphrase();
    request.options.judge = Some(false);
    let assessment = client.assess(request.clone()).await.expect("assesses");
    assert_eq!(
        assessment,
        Engine::default().assess(&request).expect("assesses")
    );
    assert_eq!(description(&assessment).status, JudgmentStatus::Requested);
    assert_eq!(fake.calls(), 0);

    // A server whose default is off asks only when the call says so.
    let off = server_with(
        &fake,
        JudgeSettings::default().with_default_on(false),
        ServerOptions::default(),
    )
    .await;
    let mut client = off.client().await;
    let assessment = client.assess(paraphrase()).await.expect("assesses");
    assert_eq!(description(&assessment).status, JudgmentStatus::Requested);
    assert_eq!(fake.calls(), 0);
    request.options.judge = Some(true);
    let assessment = client.assess(request).await.expect("assesses");
    assert_eq!(description(&assessment).status, JudgmentStatus::Applied);
    assert_eq!(fake.calls(), 1);

    off.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn a_server_without_a_judge_says_so() {
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    let mut request = paraphrase();
    request.options.judge = Some(true);
    let assessment = client.assess(request).await.expect("assesses");
    let unanswered = description(&assessment);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "this server has no judge");
    server.shutdown().await;
}

#[tokio::test]
async fn clear_changes_are_not_sent() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;

    let clear = [
        // A typo.
        (
            "Users can log in with their email address and password.",
            "Users can log in with their emial address and password.",
        ),
        // A negation.
        (
            "The API must support SSO for every customer.",
            "The API must not support SSO for every customer.",
        ),
        // The same list in another order.
        (
            "- Export the report as CSV\n- Send the report by email\n- Archive the report after 30 days",
            "- Archive the report after 30 days\n- Export the report as CSV\n- Send the report by email",
        ),
    ];
    for pair in clear {
        let request = request_for("Parent issue", pair);
        let assessment = client.assess(request.clone()).await.expect("assesses");
        assert_eq!(
            assessment,
            Engine::default().assess(&request).expect("assesses"),
            "{pair:?}"
        );
        assert_eq!(fake.calls(), 0, "{pair:?}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn only_the_flagged_texts_reach_the_judge() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;

    let v1 = SemVer::new(1, 0, 0);
    let before = issue("P", "Parent issue", PARAPHRASE.0)
        .with_version(v1)
        .with_field("customer", FieldValue::Text("SECRET-CUSTOMER".to_owned()))
        .with_field("notes", FieldValue::Text("SECRET-NOTES-OLD".to_owned()));
    let after = issue("P", "Parent issue", PARAPHRASE.1)
        .with_version(v1)
        .with_field("customer", FieldValue::Text("SECRET-CUSTOMER".to_owned()))
        .with_field("notes", FieldValue::Text("SECRET-NOTES-NEW".to_owned()));
    let request = AssessmentRequest {
        changes: vec![ChangeSet::update(before, after)],
        context: vec![
            Entity::new("C", "sub_issue")
                .with_parent("P")
                .with_title("SECRET-CHILD-TITLE")
                .with_description("SECRET-CHILD-TEXT")
                .with_basis("P", v1),
        ],
        now: Some(at(9, 9)),
        ..Default::default()
    };
    client.assess(request).await.expect("assesses");

    let questions = fake.questions();
    assert_eq!(questions.len(), 1);
    let question = &questions[0];
    assert_eq!(question.field, DESCRIPTION);
    assert_eq!(question.entity_kind, "issue");
    assert_eq!(question.context_title.as_deref(), Some("Parent issue"));
    assert_eq!(
        (question.before.as_str(), question.after.as_str()),
        PARAPHRASE
    );
    let everything = format!("{question:?}");
    assert!(!everything.contains("SECRET"), "{everything}");
    assert!(!everything.contains("\"P\""), "{everything}");

    server.shutdown().await;
}

/// `n` issues, each with its own paraphrased description.
fn many_flagged(n: usize) -> AssessmentRequest {
    AssessmentRequest {
        changes: (0..n)
            .map(|i| change(&format!("P{i}"), &format!("Issue {i}"), PARAPHRASE))
            .collect(),
        now: Some(at(9, 9)),
        ..Default::default()
    }
}

#[tokio::test]
async fn one_call_asks_about_at_most_four_fields() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;

    let assessment = client.assess(many_flagged(6)).await.expect("assesses");
    assert_eq!(fake.calls(), 4);
    let statuses: Vec<_> = (0..6)
        .map(|i| judgment_of(&assessment, &format!("P{i}"), DESCRIPTION))
        .collect();
    let applied = statuses
        .iter()
        .filter(|j| j.status == JudgmentStatus::Applied)
        .count();
    let over: Vec<_> = statuses
        .iter()
        .filter(|j| j.status == JudgmentStatus::Unavailable)
        .collect();
    assert_eq!((applied, over.len()), (4, 2));
    for j in over {
        assert_eq!(j.detail, "over the server's cap of 4 fields");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_long_text_is_not_sent() {
    let fake = Arc::new(FakeJudge::equivalent());
    let settings = JudgeSettings::default().with_max_text_chars(40);
    let server = server_with(&fake, settings, ServerOptions::default()).await;
    let mut client = server.client().await;
    let assessment = client.assess(paraphrase()).await.expect("assesses");
    assert_eq!(fake.calls(), 0);
    assert_eq!(description(&assessment).status, JudgmentStatus::Unavailable);
    assert_eq!(
        description(&assessment).detail,
        "text longer than 40 characters"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_batch_asks_about_at_most_sixteen_fields() {
    for _ in 0..2 {
        let fake = Arc::new(FakeJudge::equivalent());
        let server = server(&fake).await;
        let mut client = server.client().await;

        let requests: Vec<_> = (0..20).map(|_| paraphrase()).collect();
        let responses = client.assess_batch(requests).await.expect("answers");
        assert_eq!(responses.len(), 20);
        assert_eq!(fake.calls(), 16);
        // The fields are equally doubtful, so the first items are asked.
        for (i, response) in responses.iter().enumerate() {
            let assessment = response.result.as_ref().expect("assesses");
            let j = description(assessment);
            if i < 16 {
                assert_eq!(j.status, JudgmentStatus::Applied, "item {i}");
                assert_eq!(assessment.changes[0].bump, VersionBump::Patch);
            } else {
                assert_eq!(j.status, JudgmentStatus::Unavailable, "item {i}");
                assert_eq!(j.detail, "over the server's cap of 16 fields");
                assert_eq!(assessment.changes[0].bump, VersionBump::Major);
            }
        }
        server.shutdown().await;
    }
}

#[tokio::test]
async fn a_cached_answer_is_not_asked_again() {
    let fake = Arc::new(FakeJudge::equivalent());
    let inner: Arc<dyn Judge> = Arc::clone(&fake) as Arc<dyn Judge>;
    let cached: Arc<dyn Judge> = Arc::new(CachedJudge::new(inner));
    let server = TestServer::start_with(
        service(cached, JudgeSettings::default()),
        ServerOptions::default(),
    )
    .await;
    let mut client = server.client().await;

    let first = client.assess(paraphrase()).await.expect("assesses");
    let second = client.assess(paraphrase()).await.expect("assesses");
    assert_eq!(fake.calls(), 1);
    assert_eq!(description(&first).status, JudgmentStatus::Applied);
    assert_eq!(first, second);

    // Cached answers do not count against the cap: the second call has
    // four of them and asks about the other two fields.
    let applied = |assessment: &Assessment| {
        (0..6)
            .filter(|i| {
                judgment_of(assessment, &format!("P{i}"), DESCRIPTION).status
                    == JudgmentStatus::Applied
            })
            .count()
    };
    let assessment = client.assess(many_flagged(6)).await.expect("assesses");
    assert_eq!(applied(&assessment), 4);
    let assessment = client.assess(many_flagged(6)).await.expect("assesses");
    assert_eq!(applied(&assessment), 6);
    assert_eq!(fake.calls(), 7);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_slow_judge_never_fails_a_call() {
    let fake = Arc::new(FakeJudge::equivalent().with_delay(Duration::from_secs(60)));
    let lexical = Engine::default().assess(&paraphrase()).expect("assesses");

    // The server's request timeout bounds the wait.
    let options = ServerOptions::default().with_request_timeout(Some(Duration::from_secs(3)));
    let server = server_with(&fake, JudgeSettings::default(), options).await;
    let mut client = server.client().await;
    let started = Instant::now();
    let assessment = client.assess(paraphrase()).await.expect("answers in time");
    assert!(started.elapsed() < Duration::from_millis(2_900));
    assert_eq!(description(&assessment).status, JudgmentStatus::Unavailable);
    assert_eq!(description(&assessment).detail, "not enough time left");
    assert_eq!(
        with_description_judgment(&assessment, description(&lexical)),
        lexical
    );

    // So does the client's own deadline.
    let mut hurried = server.client().await.with_timeout(Duration::from_secs(2));
    let started = Instant::now();
    let assessment = hurried.assess(paraphrase()).await.expect("answers in time");
    assert!(started.elapsed() < Duration::from_millis(1_900));
    assert_eq!(description(&assessment).detail, "not enough time left");
    server.shutdown().await;

    // And the judge's own timeout.
    let settings = JudgeSettings::default().with_timeout(Duration::from_secs(1));
    let server = server_with(&fake, settings, ServerOptions::default()).await;
    let mut client = server.client().await;
    let assessment = client.assess(paraphrase()).await.expect("answers");
    assert_eq!(description(&assessment).status, JudgmentStatus::Unavailable);
    assert_eq!(description(&assessment).detail, "judge timed out after 1 s");
    server.shutdown().await;
}

#[tokio::test]
async fn other_calls_answer_while_judged_calls_wait() {
    let fake = Arc::new(FakeJudge::equivalent().with_delay(Duration::from_secs(3)));
    let judge: Arc<dyn Judge> = Arc::clone(&fake) as Arc<dyn Judge>;
    // Two slots: a call that held one while it waits would block the rest.
    let service = service(judge, JudgeSettings::default()).with_limits(
        ServiceLimits::default()
            .with_max_concurrent_assessments(2)
            .with_max_in_flight(2),
    );
    let server = TestServer::start_with(service, ServerOptions::default()).await;

    let mut waiting = Vec::new();
    for _ in 0..8 {
        let mut client = server.client().await;
        waiting.push(tokio::spawn(
            async move { client.assess(paraphrase()).await },
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.calls() < 8 {
        assert!(Instant::now() < deadline, "the judge was asked 8 times");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut client = server.client().await;
    let started = Instant::now();
    let mut plain = paraphrase();
    plain.options.judge = Some(false);
    client.assess(plain).await.expect("assesses");
    assert_eq!(
        client.judge_info().await.expect("answers"),
        Some(FAKE.to_owned())
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    for call in waiting {
        let assessment = call.await.expect("joins").expect("assesses");
        assert_eq!(description(&assessment).status, JudgmentStatus::Applied);
    }
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// AssessStream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_keeps_its_order_with_a_slow_item() {
    let fake = Arc::new(FakeJudge::equivalent().with_slow_delay(Duration::from_millis(700)));
    let server = server(&fake).await;
    let mut client = server.client().await;

    let items = vec![
        ("slow".to_owned(), request_for(SLOW_TITLE, PARAPHRASE)),
        ("b".to_owned(), paraphrase()),
        ("c".to_owned(), paraphrase()),
    ];
    let mut responses = client
        .assess_stream(tokio_stream::iter(items))
        .await
        .expect("opens");
    let mut ids = Vec::new();
    while let Some(item) = responses.message().await.expect("reads") {
        let assessment = item.result.expect("assesses");
        assert_eq!(description(&assessment).status, JudgmentStatus::Applied);
        ids.push(item.request_id);
    }
    assert_eq!(ids, ["slow", "b", "c"]);
    assert_eq!(fake.calls(), 3);
    let slow = fake
        .questions()
        .iter()
        .filter(|q| q.context_title.as_deref() == Some(SLOW_TITLE))
        .count();
    assert_eq!(slow, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn a_dropped_stream_stops_asking() {
    let fake = Arc::new(FakeJudge::equivalent().with_delay(Duration::from_secs(60)));
    let settings = JudgeSettings::default().with_timeout(Duration::from_secs(60));
    let server = server_with(&fake, settings, ServerOptions::default()).await;
    let mut client = server.client().await;

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(("a".to_owned(), paraphrase()))
        .await
        .expect("queues");
    let responses = client
        .assess_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect("opens");
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.calls() < 1 {
        assert!(Instant::now() < deadline, "the judge was asked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);

    drop(responses);
    drop(tx);
    drop(client);
    while fake.cancelled.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline, "the open question was dropped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

fn put_options() -> PutOptions {
    PutOptions {
        now: Some(at(9, 9)),
        ..PutOptions::default()
    }
}

/// A registry with the parent at `PARAPHRASE.0` (1.0.0) and one sub-issue.
async fn stored(client: &mut assessment_grpc::client::AssessmentClient) {
    client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.0), put_options())
        .await
        .expect("stores the parent");
    client
        .put_entity(child(PARAPHRASE.0), put_options())
        .await
        .expect("stores the child");
}

#[tokio::test]
async fn a_flagged_put_is_judged_and_stored_with_its_judgment() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;
    assert_eq!(fake.calls(), 0);

    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), put_options())
        .await
        .expect("stores");
    assert!(outcome.stored);
    assert_eq!(outcome.bump, VersionBump::Patch);
    assert_eq!(outcome.entity.version, Some(SemVer::new(1, 0, 1)));
    assert_eq!(
        description(&outcome.assessment).status,
        JudgmentStatus::Applied
    );
    assert!(
        outcome
            .assessment
            .judgment_requests
            .iter()
            .all(|r| r.before.is_none() && r.after.is_none())
    );
    assert_eq!(fake.calls(), 1);

    // The record keeps the answer: history and a later comparison agree,
    // and nobody is asked again.
    let history = client.history("P", 10).await.expect("reads");
    let record = history
        .iter()
        .find(|r| r.version == SemVer::new(1, 0, 1))
        .expect("the new version");
    assert_eq!(record.bump, VersionBump::Patch);
    assert_eq!(record.judgments.len(), 1);
    assert_eq!(record.judgments[0].source, FAKE);
    let compared = client
        .compare_versions("P", SemVer::new(1, 0, 0), SemVer::new(1, 0, 1), None)
        .await
        .expect("compares");
    assert_eq!(compared.changes[0].bump, VersionBump::Patch);
    assert_eq!(description(&compared).status, JudgmentStatus::Applied);
    assert_eq!(fake.calls(), 1);

    server.shutdown().await;
}

/// A caller's judgment of an older pair of texts.
fn stale_judgment() -> TextJudgment {
    TextJudgment {
        field: DESCRIPTION.to_owned(),
        fingerprint: assessment_core::text::fingerprint(PARAPHRASE.0, "Another text."),
        relation: TextRelation::Replaced,
        confidence: 0.9,
        rationale: String::new(),
        source: "caller".to_owned(),
    }
}

#[tokio::test]
async fn a_put_with_a_stale_judgment_is_judged_before_it_is_stored() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;

    let options = PutOptions {
        judgments: vec![stale_judgment()],
        ..put_options()
    };
    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), options)
        .await
        .expect("stores");
    assert_eq!(fake.calls(), 1);
    assert!(outcome.stored);
    assert_eq!(outcome.bump, VersionBump::Patch);
    assert_eq!(
        description(&outcome.assessment).status,
        JudgmentStatus::Applied
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_put_is_stored_when_the_judge_fails() {
    let fake = Arc::new(FakeJudge::new(Reply::Error(
        JudgeErrorKind::Transport,
        "connection refused",
        None,
    )));
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;

    // A dry run is not judged unless the call asks for it.
    let dry = PutOptions {
        dry_run: true,
        ..put_options()
    };
    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), dry)
        .await
        .expect("answers");
    assert!(!outcome.stored);
    assert_eq!(
        description(&outcome.assessment).status,
        JudgmentStatus::Requested
    );
    assert_eq!(fake.calls(), 0);

    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), put_options())
        .await
        .expect("never fails");
    assert!(outcome.stored);
    assert_eq!(outcome.bump, VersionBump::Major);
    assert_eq!(outcome.entity.version, Some(SemVer::new(2, 0, 0)));
    let unanswered = description(&outcome.assessment);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "connection refused");
    assert_eq!(fake.calls(), 1);

    server.shutdown().await;
}

/// A field whose supplied judgment is stale is asked about. When the judge
/// gives no answer, the field says so, like a field with no judgment.
#[tokio::test]
async fn a_stale_judgment_with_a_failing_judge_is_unavailable_with_the_reason() {
    let fake = Arc::new(FakeJudge::new(Reply::Error(
        JudgeErrorKind::Transport,
        "connection refused",
        None,
    )));
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;

    // Assess.
    let mut request = paraphrase();
    request.changes[0].judgments = vec![stale_judgment()];
    let mut unjudged = request.clone();
    unjudged.options.judge = Some(false);
    let lexical = client.assess(unjudged).await.expect("assesses");
    assert_eq!(description(&lexical).status, JudgmentStatus::Stale);
    let assessment = client.assess(request).await.expect("never fails");
    assert_eq!(fake.calls(), 1);
    let unanswered = description(&assessment);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "connection refused");
    assert_eq!(
        with_description_judgment(&assessment, description(&lexical)),
        lexical
    );

    // PutEntity: stored with the engine's own reading.
    let options = PutOptions {
        judgments: vec![stale_judgment()],
        ..put_options()
    };
    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), options)
        .await
        .expect("never fails");
    assert_eq!(fake.calls(), 2);
    assert!(outcome.stored);
    assert_eq!(outcome.bump, VersionBump::Major);
    let unanswered = description(&outcome.assessment);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "connection refused");

    server.shutdown().await;

    // A server without a judge, asked for one, says so for this field too.
    let server = TestServer::start(ServerOptions::default()).await;
    let mut client = server.client().await;
    let mut request = paraphrase();
    request.changes[0].judgments = vec![stale_judgment()];
    request.options.judge = Some(true);
    let assessment = client.assess(request).await.expect("assesses");
    let unanswered = description(&assessment);
    assert_eq!(unanswered.status, JudgmentStatus::Unavailable);
    assert_eq!(unanswered.detail, "this server has no judge");
    server.shutdown().await;
}

/// A put that only carries activity changes nothing and asks for no
/// judgment. It is assessed once: a second run would find its own
/// `source_seq` already remembered and answer `skipped_stale`.
#[tokio::test]
async fn an_activity_only_put_with_a_sequence_is_assessed_once() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;

    let comment = ActivityEvent {
        entity_id: "P".to_owned(),
        kind: ActivityKind::Comment,
        author: Some("dorj".to_owned()),
        body: "Blocked: we are waiting on the identity provider.".to_owned(),
        at: Some(at(9, 8)),
    };
    let with_comment = |source_seq| PutOptions {
        events: vec![comment.clone()],
        source_seq: Some(source_seq),
        ..put_options()
    };
    // What a put with the judge off answers (on a dry run, so that nothing
    // is remembered).
    let mut plain = with_comment(7);
    plain.dry_run = true;
    plain.assess.judge = Some(false);
    let expected = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.0), plain)
        .await
        .expect("answers");
    assert_eq!(expected.assessment.changes.len(), 1);

    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.0), with_comment(7))
        .await
        .expect("answers");
    assert!(!outcome.skipped_stale, "{}", outcome.assessment.summary);
    assert!(!outcome.stored && !outcome.unchanged);
    assert_eq!(outcome.assessment, expected.assessment);
    assert_eq!(fake.calls(), 0);

    // The sequence was remembered once: the same change again is stale,
    // and a newer one is assessed.
    let again = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.0), with_comment(7))
        .await
        .expect("answers");
    assert!(again.skipped_stale);
    let newer = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.0), with_comment(8))
        .await
        .expect("answers");
    assert!(!newer.skipped_stale);
    assert_eq!(newer.assessment.changes.len(), 1);

    server.shutdown().await;
}

#[tokio::test]
async fn compare_versions_asks_about_an_unjudged_version() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;
    stored(&mut client).await;

    // Stored without the judge: the record has no judgment.
    let mut unjudged = put_options();
    unjudged.assess.judge = Some(false);
    let outcome = client
        .put_entity(issue("P", "Parent issue", PARAPHRASE.1), unjudged)
        .await
        .expect("stores");
    assert_eq!(outcome.entity.version, Some(SemVer::new(2, 0, 0)));
    assert_eq!(fake.calls(), 0);

    let compared = client
        .compare_versions("P", SemVer::new(1, 0, 0), SemVer::new(2, 0, 0), None)
        .await
        .expect("compares");
    assert_eq!(description(&compared).status, JudgmentStatus::Applied);
    assert_eq!(compared.changes[0].bump, VersionBump::Patch);
    assert_eq!(fake.calls(), 1);
    let question = &fake.questions()[0];
    assert_eq!(question.context_title.as_deref(), Some("Parent issue"));

    server.shutdown().await;
}

#[tokio::test]
async fn import_never_asks_the_judge() {
    let fake = Arc::new(FakeJudge::equivalent());
    let server = server(&fake).await;
    let mut client = server.client().await;

    let items = vec![
        ImportItem::upsert(issue("P", "Parent issue", PARAPHRASE.0)),
        ImportItem::upsert(child(PARAPHRASE.0)),
        ImportItem::upsert(issue("P", "Parent issue", PARAPHRASE.1)),
    ];
    client.import_iter(items).await.expect("imports");
    assert_eq!(fake.calls(), 0);
    // A title is judged like a description; nothing else is.
    assert_eq!(assessment_grpc::flow::JUDGED_FIELDS, [TITLE, DESCRIPTION]);

    server.shutdown().await;
}
