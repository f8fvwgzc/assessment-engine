//! Text judgments: what the engine flags, what a supplied judgment does to
//! the numbers, and what it must never do.

mod support;

use assessment_core::model::field::DESCRIPTION;
use assessment_core::prelude::*;
use assessment_core::registry::VersionRecord;
use assessment_core::text::fingerprint;
use proptest::prelude::*;

const PARAPHRASE: (&str, &str) = (
    "Users can log in with their email address and password.",
    "People are able to sign in using e-mail and a password.",
);
const POSTGRES: (&str, &str) = (
    "Store user sessions in PostgreSQL with a 30 day expiry.",
    "Store user sessions in MongoDB with a 30 day expiry.",
);
const LOGIN: (&str, &str) = ("Build the login button", "Build the logout button");
const FLEXIBLE: (&str, &str) = (
    "The launch date is flexible and can move if needed.",
    "The launch date is fixed and can move if needed.",
);
const PAYPAL: (&str, &str) = (
    "Support all payment methods at checkout.",
    "Support all payment methods at checkout except PayPal.",
);
const HTTP: (&str, &str) = ("Call the API over http", "Call the API over https");

fn judgment(pair: (&str, &str), relation: TextRelation, confidence: f64) -> TextJudgment {
    TextJudgment {
        field: DESCRIPTION.to_owned(),
        fingerprint: fingerprint(pair.0, pair.1),
        relation,
        confidence,
        rationale: String::new(),
        source: "test".to_owned(),
    }
}

fn request(pair: (&str, &str), judgments: Vec<TextJudgment>) -> AssessmentRequest {
    let mut req = support::text_case(pair.0, pair.1);
    req.changes[0].judgments = judgments;
    req
}

fn assess(pair: (&str, &str), judgments: Vec<TextJudgment>) -> Assessment {
    Engine::default()
        .assess(&request(pair, judgments))
        .expect("the request assesses")
}

fn unjudged_engine() -> Engine {
    let mut policy = Policy::default();
    policy.judge.enabled = false;
    Engine::new(policy).expect("the policy is valid")
}

fn description(a: &Assessment) -> &assessment_core::assessment::FieldAssessment {
    a.changes[0]
        .fields
        .iter()
        .find(|f| f.field == DESCRIPTION)
        .expect("the description changed")
}

fn reasons(a: &Assessment) -> Vec<JudgmentReason> {
    description(a)
        .judgment
        .as_ref()
        .map(|j| j.reasons.clone())
        .unwrap_or_default()
}

fn status(a: &Assessment) -> Option<JudgmentStatus> {
    description(a).judgment.as_ref().map(|j| j.status)
}

fn child(a: &Assessment) -> &EntityImpact {
    a.impacts
        .iter()
        .find(|i| i.entity_id == "C")
        .expect("the child is affected")
}

fn json(a: &Assessment) -> String {
    serde_json::to_string(a).expect("the assessment serializes")
}

fn close(x: f64, y: f64) -> bool {
    (x - y).abs() < 0.0015
}

/// The numbers of an assessment, without what only describes the judgment.
fn numbers(mut a: Assessment) -> Assessment {
    for c in &mut a.changes {
        for f in &mut c.fields {
            f.judgment = None;
        }
    }
    a.judgment_requests.clear();
    a.unmatched_judgments.clear();
    a.summary.clear();
    a
}

// ---------------------------------------------------------------------------
// What is flagged.
// ---------------------------------------------------------------------------

/// A change, its expected reasons, and its lexical bump and magnitude.
type Row = (
    (&'static str, &'static str),
    &'static [JudgmentReason],
    VersionBump,
    f64,
);

#[test]
fn the_misreadings_are_flagged() {
    use JudgmentReason::{NearThreshold, ParaphraseSuspected, ScopeQualifier, TermSubstitution};
    let rows: [Row; 6] = [
        (
            PARAPHRASE,
            &[ParaphraseSuspected],
            VersionBump::Major,
            0.802,
        ),
        (POSTGRES, &[TermSubstitution], VersionBump::Minor, 0.242),
        (
            LOGIN,
            &[TermSubstitution, NearThreshold],
            VersionBump::Minor,
            0.441,
        ),
        (FLEXIBLE, &[TermSubstitution], VersionBump::Minor, 0.255),
        (PAYPAL, &[ScopeQualifier], VersionBump::Minor, 0.277),
        (HTTP, &[TermSubstitution], VersionBump::Minor, 0.286),
    ];
    for (pair, expected, bump, magnitude) in rows {
        let a = assess(pair, Vec::new());
        assert_eq!(reasons(&a), expected, "{pair:?}");
        assert_eq!(status(&a), Some(JudgmentStatus::Requested), "{pair:?}");
        // Without a judgment the bump and the magnitude stay lexical.
        assert_eq!(a.changes[0].bump, bump, "{pair:?}");
        assert!(close(description(&a).magnitude, magnitude), "{pair:?}");
        assert_eq!(a.judgment_requests.len(), 1);
        let r = &a.judgment_requests[0];
        assert_eq!(r.entity_id, "P");
        assert_eq!(r.field, DESCRIPTION);
        assert_eq!(r.fingerprint, fingerprint(pair.0, pair.1));
        assert_eq!(r.reasons, expected);
        assert_eq!((r.before.as_deref(), r.after.as_deref()), (None, None));
        assert!(
            description(&a).explanation.contains("meaning not verified"),
            "{}",
            description(&a).explanation
        );
        assert!(a.summary.contains("not verified"), "{}", a.summary);
    }
}

#[test]
fn clear_changes_are_not_flagged() {
    let unflagged = [
        "sso_negation",
        "typo",
        "numbers",
        "should_must",
        "reorder",
        "removed_item",
        "flexible_cannot",
    ];
    for (name, before, after) in support::TEXT_CASES {
        if !unflagged.contains(name) {
            continue;
        }
        let a = assess((before, after), Vec::new());
        assert!(description(&a).judgment.is_none(), "{name}");
        assert!(a.judgment_requests.is_empty(), "{name}");
        let off = unjudged_engine()
            .assess(&request((before, after), Vec::new()))
            .expect("assesses");
        assert_eq!(json(&a), json(&off), "{name}");
    }
    // A reversal the rules see as one word that turns into its opposite.
    let pair = (
        "Increase the session timeout for admins.",
        "Decrease the session timeout for admins.",
    );
    let a = assess(pair, Vec::new());
    assert!(description(&a).judgment.is_none());
    // A negation the rules already count: `must` to `must not`.
    let pair = (
        "The API must return the user's email.",
        "The API must not return the user's email.",
    );
    let a = assess(pair, Vec::new());
    assert!(description(&a).judgment.is_none());
    assert_eq!(a.changes[0].bump, VersionBump::Major);
}

#[test]
fn the_unflagged_rows_keep_their_results() {
    let rows = [
        ("sso_negation", VersionBump::Major),
        ("typo", VersionBump::Patch),
        ("numbers", VersionBump::Major),
        ("reorder", VersionBump::Patch),
        ("removed_item", VersionBump::Major),
    ];
    for (name, bump) in rows {
        let (_, before, after) = support::TEXT_CASES
            .iter()
            .find(|(n, _, _)| *n == name)
            .expect("the case exists");
        let a = assess((before, after), Vec::new());
        assert_eq!(a.changes[0].bump, bump, "{name}");
    }
    let (_, b, a) = support::TEXT_CASES[0];
    assert!(close(
        description(&assess((b, a), Vec::new())).magnitude,
        0.482
    ));
}

#[test]
fn compare_text_reports_the_gray_zone_and_applies_nothing() {
    let engine = Engine::default();
    let d = engine
        .compare_text(POSTGRES.0, POSTGRES.1, None)
        .expect("compares");
    let g = d.gray_zone.expect("flagged");
    assert_eq!(g.reasons, [JudgmentReason::TermSubstitution]);
    assert_eq!(g.fingerprint, fingerprint(POSTGRES.0, POSTGRES.1));
    assert!(close(d.magnitude, 0.242));
    // A field the judge does not cover is never flagged.
    let d = engine
        .compare_text_as("notes", POSTGRES.0, POSTGRES.1, None)
        .expect("compares");
    assert!(d.gray_zone.is_none());
    let d = engine
        .compare_text("Implment the login flow", "Implement the login flow", None)
        .expect("compares");
    assert!(d.gray_zone.is_none());
    let d = unjudged_engine()
        .compare_text(POSTGRES.0, POSTGRES.1, None)
        .expect("compares");
    assert!(d.gray_zone.is_none());
}

#[test]
fn the_fingerprint_is_fixed() {
    assert_eq!(
        fingerprint("", ""),
        "78f3ba77faa1ffc62bd2dd7cc8de66fb1c9112793b99729582731a64ee7ae8e4"
    );
    assert_eq!(
        fingerprint(PARAPHRASE.0, PARAPHRASE.1),
        "a5092b8fdb1115a48435979471ef040eb3f920811cb6bfdca108c7393e2d56b5"
    );
    assert_eq!(
        fingerprint(POSTGRES.0, POSTGRES.1),
        "73ca2f8d43d040c50a6443010baf2944716e16c70932fafdeb18dea4d551b8ad"
    );
    assert_eq!(
        fingerprint(PAYPAL.0, PAYPAL.1),
        "b488d7fbeab57e59593c52ae1c56bed8933f3bd0885f409bdcde3e2799e8ce03"
    );
    assert_ne!(fingerprint("a", ""), fingerprint("", "a"));
}

// ---------------------------------------------------------------------------
// What a judgment does.
// ---------------------------------------------------------------------------

#[test]
fn an_equivalent_paraphrase_becomes_a_patch() {
    let a = assess(
        PARAPHRASE,
        vec![judgment(PARAPHRASE, TextRelation::Equivalent, 0.9)],
    );
    let f = description(&a);
    let j = f.judgment.as_ref().expect("judged");
    assert_eq!(j.status, JudgmentStatus::Applied);
    assert_eq!(j.relation, Some(TextRelation::Equivalent));
    assert!(close(j.weight.expect("applied"), 0.75));
    assert!(close(j.lexical_magnitude, 0.802));
    assert!((0.235..=0.241).contains(&f.magnitude), "{}", f.magnitude);
    assert!(!f.breaking);
    assert_eq!(a.changes[0].bump, VersionBump::Patch);
    assert_eq!(a.changes[0].next_version, Some(SemVer::new(1, 0, 1)));
    assert_eq!(child(&a).action, Action::Notify);
    assert_eq!(child(&a).basis_status, BasisStatus::Compatible);
    assert!(a.judgment_requests.is_empty());
    assert!(a.unmatched_judgments.is_empty());
    assert!(
        f.explanation
            .contains("(lexically 0.80; judged equivalent, confidence 0.90)"),
        "{}",
        f.explanation
    );
    assert!(a.summary.contains("judged"), "{}", a.summary);
    assert!(close(f.metrics["lexical_magnitude"], 0.802));
    assert!(close(f.metrics["judgment_weight"], 0.75));

    // Confidence 0.8 is still decisive; 0.75 is not.
    let a = assess(
        PARAPHRASE,
        vec![judgment(PARAPHRASE, TextRelation::Equivalent, 0.8)],
    );
    assert_eq!(a.changes[0].bump, VersionBump::Patch);
    let a = assess(
        PARAPHRASE,
        vec![judgment(PARAPHRASE, TextRelation::Equivalent, 0.75)],
    );
    assert_eq!(status(&a), Some(JudgmentStatus::Applied));
    assert_eq!(a.changes[0].bump, VersionBump::Major);
}

#[test]
fn a_raising_judgment_makes_the_change_breaking() {
    let rows = [
        (POSTGRES, TextRelation::Replaced, 0.408),
        (LOGIN, TextRelation::Replaced, 0.564),
        (FLEXIBLE, TextRelation::Reversed, 0.495),
        (PAYPAL, TextRelation::Narrowed, 0.459),
        (HTTP, TextRelation::Replaced, 0.443),
    ];
    for (pair, relation, magnitude) in rows {
        let a = assess(pair, vec![judgment(pair, relation, 0.9)]);
        let f = description(&a);
        let j = f.judgment.as_ref().expect("judged");
        assert_eq!(j.status, JudgmentStatus::Applied, "{pair:?}");
        assert!(close(j.weight.expect("applied"), 0.8), "{pair:?}");
        assert!(close(f.magnitude, magnitude), "{pair:?}: {}", f.magnitude);
        assert!(f.breaking, "{pair:?}");
        assert_eq!(a.changes[0].bump, VersionBump::Major, "{pair:?}");
        assert_eq!(a.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
        assert_eq!(child(&a).action, Action::UpdateScope, "{pair:?}");
        assert_eq!(child(&a).basis_status, BasisStatus::Incompatible);
        assert!(a.judgment_requests.is_empty());
    }
}

#[test]
fn a_stale_judgment_changes_nothing() {
    let plain = assess(POSTGRES, Vec::new());
    let mut j = judgment(POSTGRES, TextRelation::Replaced, 0.9);
    j.fingerprint = "0".repeat(64);
    let a = assess(POSTGRES, vec![j.clone()]);
    assert_eq!(status(&a), Some(JudgmentStatus::Stale));
    let fj = description(&a).judgment.as_ref().expect("reported");
    assert_eq!(fj.relation, Some(TextRelation::Replaced));
    assert_eq!(fj.weight, None);
    assert_eq!(a.changes[0].bump, VersionBump::Minor);
    // It is asked for again, with the right fingerprint.
    assert_eq!(a.judgment_requests, plain.judgment_requests);
    assert_eq!(json(&numbers(a)), json(&numbers(plain.clone())));

    // A judgment of other texts of the same field is stale too.
    let a = assess(POSTGRES, vec![judgment(LOGIN, TextRelation::Replaced, 0.9)]);
    assert_eq!(status(&a), Some(JudgmentStatus::Stale));
    assert_eq!(json(&numbers(a)), json(&numbers(plain)));
}

#[test]
fn a_weak_or_undetermined_judgment_is_inconclusive() {
    let plain = assess(PARAPHRASE, Vec::new());
    for j in [
        judgment(PARAPHRASE, TextRelation::Equivalent, 0.6),
        judgment(PARAPHRASE, TextRelation::Equivalent, 0.0),
        judgment(PARAPHRASE, TextRelation::Undetermined, 1.0),
        judgment(PARAPHRASE, TextRelation::Replaced, 0.5),
    ] {
        let a = assess(PARAPHRASE, vec![j.clone()]);
        assert_eq!(status(&a), Some(JudgmentStatus::Inconclusive), "{j:?}");
        assert!(close(a.confidence, 0.765));
        assert!(a.judgment_requests.is_empty());
        assert_eq!(json(&numbers(a)), json(&numbers(plain.clone())), "{j:?}");
    }
}

#[test]
fn a_judgment_of_an_unflagged_field_is_not_applied() {
    let typo = (
        "Implment the login flow with email",
        "Implement the login flow with email",
    );
    let plain = assess(typo, Vec::new());
    let a = assess(typo, vec![judgment(typo, TextRelation::Reversed, 1.0)]);
    assert_eq!(status(&a), Some(JudgmentStatus::NotFlagged));
    assert_eq!(a.changes[0].bump, VersionBump::Patch);
    assert_eq!(json(&numbers(a)), json(&numbers(plain)));
}

#[test]
fn a_lowering_judgment_never_lifts_a_blocked_bump() {
    let mut policy = Policy::default();
    policy.judge.apply_unflagged = true;
    let engine = Engine::new(policy).expect("valid");
    for name in ["reorder", "removed_item", "sso_negation"] {
        let (_, before, after) = support::TEXT_CASES
            .iter()
            .find(|(n, _, _)| *n == name)
            .expect("the case exists");
        let pair = (*before, *after);
        let plain = engine.assess(&request(pair, Vec::new())).expect("assesses");
        let judged = engine
            .assess(&request(
                pair,
                vec![judgment(pair, TextRelation::Equivalent, 1.0)],
            ))
            .expect("assesses");
        assert_eq!(judged.changes[0].bump, plain.changes[0].bump, "{name}");
        if name != "reorder" {
            assert_eq!(child(&judged).action, child(&plain).action, "{name}");
        }
    }
    // A changed number blocks the caps of a lowering judgment too.
    let pair = (
        "Sessions expire after 30 days of inactivity.",
        "Sessions expire after a month of inactivity.",
    );
    let plain = engine.assess(&request(pair, Vec::new())).expect("assesses");
    let judged = engine
        .assess(&request(
            pair,
            vec![judgment(pair, TextRelation::Equivalent, 1.0)],
        ))
        .expect("assesses");
    // Nothing is lowered: not the magnitude, not the bump.
    assert_eq!(plain.changes[0].bump, VersionBump::Major);
    assert_eq!(judged.changes[0].bump, VersionBump::Major);
    assert!(close(
        description(&judged).magnitude,
        description(&plain).magnitude
    ));
}

#[test]
fn a_requirement_word_or_a_number_blocks_every_lowering() {
    let pairs = [
        (
            "Users should confirm their email before checkout. Users can log in with their email address and password.",
            "Users must confirm their email before checkout. People are able to sign in using e-mail and a password.",
        ),
        (
            "Sessions expire after 30 days of inactivity. Users can log in with their email address and password.",
            "Sessions expire after 7 days of inactivity. People are able to sign in using e-mail and a password.",
        ),
    ];
    for pair in pairs {
        let plain = assess(pair, Vec::new());
        assert_eq!(plain.changes[0].bump, VersionBump::Major, "{}", pair.1);
        for relation in [TextRelation::Equivalent, TextRelation::Extended] {
            for confidence in [0.75, 0.9, 1.0] {
                let judged = assess(pair, vec![judgment(pair, relation, confidence)]);
                assert_eq!(status(&judged), Some(JudgmentStatus::Applied));
                assert_eq!(judged.changes[0].bump, VersionBump::Major, "{}", pair.1);
                assert_eq!(description(&judged).breaking, description(&plain).breaking);
                assert!(close(
                    description(&judged).magnitude,
                    description(&plain).magnitude
                ));
                assert_eq!(child(&judged).action, child(&plain).action);
            }
        }
        // A raising judgment is still applied.
        let raised = assess(pair, vec![judgment(pair, TextRelation::Reversed, 1.0)]);
        assert!(description(&raised).magnitude > description(&plain).magnitude);
    }
}

#[test]
fn an_equivalent_judgment_keeps_an_added_sentence_minor() {
    let pairs = [
        (
            "Support all payment methods at checkout.",
            "Support all payment methods at checkout. PayPal payment is not supported.",
        ),
        (
            "Admins approve every refund request.",
            "Admins approve every refund request. Only managers see the audit log.",
        ),
    ];
    for pair in pairs {
        let plain = assess(pair, Vec::new());
        assert_eq!(plain.changes[0].bump, VersionBump::Minor, "{}", pair.1);
        assert!(description(&plain).additive);
        for confidence in [0.9, 1.0] {
            let judged = assess(
                pair,
                vec![judgment(pair, TextRelation::Equivalent, confidence)],
            );
            assert_eq!(status(&judged), Some(JudgmentStatus::Applied));
            assert_eq!(judged.changes[0].bump, VersionBump::Minor, "{}", pair.1);
            assert!(description(&judged).additive, "{}", pair.1);
        }
    }
}

#[test]
fn with_the_judge_off_judgments_are_ignored() {
    let engine = unjudged_engine();
    let plain = engine
        .assess(&request(POSTGRES, Vec::new()))
        .expect("assesses");
    let mut bad = judgment(POSTGRES, TextRelation::Replaced, 7.0);
    bad.fingerprint = "nonsense".to_owned();
    let a = engine
        .assess(&request(POSTGRES, vec![bad.clone(), bad]))
        .expect("malformed judgments are not even read");
    assert_eq!(json(&a), json(&plain));
    assert!(description(&a).judgment.is_none());
}

// ---------------------------------------------------------------------------
// Requests: validation, texts, budget.
// ---------------------------------------------------------------------------

#[test]
fn malformed_judgments_are_rejected() {
    let good = || judgment(POSTGRES, TextRelation::Replaced, 0.9);
    let fails = |js: Vec<TextJudgment>, what: &str| {
        let err = Engine::default()
            .assess(&request(POSTGRES, js))
            .expect_err("rejected");
        assert_eq!(err.kind(), ErrorKind::InvalidRequest);
        assert!(err.to_string().contains(what), "{err}");
    };
    for c in [1.5, -0.1, f64::NAN, f64::INFINITY] {
        let mut j = good();
        j.confidence = c;
        fails(vec![j], "confidence outside [0, 1]");
    }
    let mut j = good();
    j.fingerprint = "ABC".to_owned();
    fails(vec![j], "64 lowercase hex digits");
    let mut j = good();
    j.fingerprint = j.fingerprint.to_uppercase();
    fails(vec![j], "64 lowercase hex digits");
    let mut j = good();
    j.field = " ".to_owned();
    fails(vec![j], "names no field");
    fails(vec![good(), good()], "given twice");
    let mut j = good();
    j.rationale = "x".repeat(2049);
    fails(vec![j], "rationale longer");
    let mut j = good();
    j.source = "x".repeat(257);
    fails(vec![j], "source longer");
    fails(vec![good(); 65], "more than the 64 allowed");
}

#[test]
fn a_judgment_of_an_unchanged_field_is_unmatched() {
    let mut other = judgment(POSTGRES, TextRelation::Replaced, 0.9);
    other.field = "title".to_owned();
    let a = assess(
        POSTGRES,
        vec![judgment(POSTGRES, TextRelation::Replaced, 0.9), other],
    );
    assert_eq!(status(&a), Some(JudgmentStatus::Applied));
    assert_eq!(a.unmatched_judgments.len(), 1);
    assert_eq!(a.unmatched_judgments[0].entity_id, "P");
    assert_eq!(a.unmatched_judgments[0].field, "title");
}

#[test]
fn requests_carry_the_texts_only_when_asked() {
    let mut req = request(POSTGRES, Vec::new());
    req.options.judgment_texts = true;
    let a = Engine::default().assess(&req).expect("assesses");
    let r = &a.judgment_requests[0];
    assert_eq!(r.before.as_deref(), Some(POSTGRES.0));
    assert_eq!(r.after.as_deref(), Some(POSTGRES.1));
    // The option shapes the requests and nothing else.
    let plain = assess(POSTGRES, Vec::new());
    assert_eq!(json(&numbers(a)), json(&numbers(plain)));
}

fn three_changes() -> AssessmentRequest {
    let item = |id: &str, text: &str| {
        Entity::new(id, "issue")
            .with_title(format!("Item {id}"))
            .with_description(text)
            .with_status(StatusCategory::Started, "In Progress")
            .with_version(SemVer::new(1, 0, 0))
    };
    let change =
        |id: &str, pair: (&str, &str)| ChangeSet::update(item(id, pair.0), item(id, pair.1));
    AssessmentRequest {
        changes: vec![
            change("A", PARAPHRASE),
            change("B", POSTGRES),
            change("D", PAYPAL),
        ],
        now: Some(support::now()),
        ..Default::default()
    }
}

#[test]
fn the_budget_asks_for_the_strongest_reasons_first() {
    let mut policy = Policy::default();
    policy.judge.max_requests = 2;
    let engine = Engine::new(policy).expect("valid");
    let req = three_changes();
    let a = engine.assess(&req).expect("assesses");
    let asked: Vec<&str> = a
        .judgment_requests
        .iter()
        .map(|r| r.entity_id.as_str())
        .collect();
    // scope_qualifier, then term_substitution; the paraphrase waits.
    assert_eq!(asked, ["D", "B"]);
    let st = |a: &Assessment, id: &str| {
        a.changes
            .iter()
            .find(|c| c.entity_id == id)
            .and_then(|c| c.fields.iter().find(|f| f.field == DESCRIPTION))
            .and_then(|f| f.judgment.as_ref())
            .map(|j| j.status)
    };
    assert_eq!(st(&a, "A"), Some(JudgmentStatus::OverBudget));
    assert_eq!(st(&a, "B"), Some(JudgmentStatus::Requested));
    assert_eq!(st(&a, "D"), Some(JudgmentStatus::Requested));

    // The order of the changes plays no part.
    let mut reversed = req.clone();
    reversed.changes.reverse();
    let b = engine.assess(&reversed).expect("assesses");
    let key = |a: &Assessment| -> Vec<(String, String, String)> {
        a.judgment_requests
            .iter()
            .map(|r| (r.entity_id.clone(), r.field.clone(), r.fingerprint.clone()))
            .collect()
    };
    assert_eq!(key(&a), key(&b));
    assert_eq!(st(&b, "A"), Some(JudgmentStatus::OverBudget));

    // A supplied judgment does not free a place for another field.
    let mut judged = req.clone();
    judged.changes[2].judgments = vec![judgment(PAYPAL, TextRelation::Narrowed, 0.9)];
    let c = engine.assess(&judged).expect("assesses");
    assert_eq!(st(&c, "D"), Some(JudgmentStatus::Applied));
    assert_eq!(st(&c, "A"), Some(JudgmentStatus::OverBudget));
    assert_eq!(key(&c), key(&a)[1..].to_vec());

    // Without a budget nothing is asked for and every flag is over budget.
    let mut policy = Policy::default();
    policy.judge.max_requests = 0;
    let none = Engine::new(policy)
        .expect("valid")
        .assess(&req)
        .expect("assesses");
    assert!(none.judgment_requests.is_empty());
    assert_eq!(st(&none, "B"), Some(JudgmentStatus::OverBudget));
}

// ---------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------

fn parent(text: &str) -> Entity {
    Entity::new("P", "issue")
        .with_title("Parent issue")
        .with_description(text)
        .with_status(StatusCategory::Started, "In Progress")
}

fn opts() -> PutOptions {
    PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    }
}

fn registry(cache: bool, first: &str) -> Registry {
    let registry = Registry::new(Engine::default()).with_analysis_cache(cache);
    registry.put(parent(first), opts()).expect("stores");
    registry
        .put(
            Entity::new("C", "sub_issue")
                .with_parent("P")
                .with_title("Child task")
                .with_description(first)
                .with_status(StatusCategory::Started, "In Progress"),
            opts(),
        )
        .expect("stores");
    registry
}

#[test]
fn a_put_with_a_judgment_stores_the_judged_version() {
    for cache in [true, false] {
        let registry = registry(cache, PARAPHRASE.0);
        let j = judgment(PARAPHRASE, TextRelation::Equivalent, 0.9);
        let out = registry
            .put(
                parent(PARAPHRASE.1),
                PutOptions {
                    judgments: vec![j.clone()],
                    ..opts()
                },
            )
            .expect("stores");
        assert!(out.stored);
        assert_eq!(out.bump, VersionBump::Patch);
        assert_eq!(out.entity.version, Some(SemVer::new(1, 0, 1)));
        let history = registry.history("P", 0).expect("history");
        let last = history.last().expect("a record");
        assert_eq!(last.version, SemVer::new(1, 0, 1));
        assert_eq!(last.judgments, vec![j]);
        assert!(history[0].judgments.is_empty());

        // Comparing the two versions agrees with what was stored.
        let cmp = registry
            .compare(
                "P",
                SemVer::new(1, 0, 0),
                SemVer::new(1, 0, 1),
                Some(support::now()),
            )
            .expect("compares");
        assert_eq!(cmp.changes[0].bump, VersionBump::Patch);
        assert_eq!(status(&cmp), Some(JudgmentStatus::Applied));

        // A caller's judgment of the field goes before the stored one.
        let cmp = registry
            .compare_with(
                "P",
                SemVer::new(1, 0, 0),
                SemVer::new(1, 0, 1),
                Some(support::now()),
                &AssessOptions::default(),
                &[judgment(PARAPHRASE, TextRelation::Replaced, 0.9)],
            )
            .expect("compares");
        assert_eq!(cmp.changes[0].bump, VersionBump::Major);
    }
}

#[test]
fn hold_if_flagged_holds_a_flagged_put_and_stores_an_unflagged_one() {
    let registry = registry(true, POSTGRES.0);
    let hold = || PutOptions {
        hold_if_flagged: true,
        assess: AssessOptions {
            judgment_texts: true,
            ..AssessOptions::default()
        },
        ..opts()
    };
    let held = registry.put(parent(POSTGRES.1), hold()).expect("answers");
    assert!(!held.stored);
    assert!(!held.unchanged);
    assert_eq!(held.assessment.judgment_requests.len(), 1);
    assert_eq!(
        held.assessment.judgment_requests[0].after.as_deref(),
        Some(POSTGRES.1)
    );
    assert_eq!(registry.history("P", 0).expect("history").len(), 1);
    assert_eq!(
        registry.get("P", None).expect("stored").text(DESCRIPTION),
        Some(POSTGRES.0)
    );

    // The second put brings the judgment and is stored.
    let out = registry
        .put(
            parent(POSTGRES.1),
            PutOptions {
                judgments: vec![judgment(POSTGRES, TextRelation::Replaced, 0.9)],
                ..hold()
            },
        )
        .expect("stores");
    assert!(out.stored);
    assert_eq!(out.entity.version, Some(SemVer::new(2, 0, 0)));

    // An unflagged change is stored at once.
    let typo = format!("{} Thanks.", POSTGRES.1);
    let out = registry.put(parent(&typo), hold()).expect("stores");
    assert!(out.assessment.judgment_requests.is_empty());
    assert!(out.stored);
}

#[test]
fn a_stale_judgment_holds_a_held_put_and_is_ignored_by_a_plain_one() {
    let registry = registry(true, POSTGRES.0);
    let hold = || PutOptions {
        hold_if_flagged: true,
        ..opts()
    };
    let held = registry.put(parent(POSTGRES.1), hold()).expect("answers");
    assert!(!held.stored);
    // The text changes again before the judgment arrives.
    let later = "Store user sessions in Redis with a 30 day expiry.";
    let out = registry
        .put(
            parent(later),
            PutOptions {
                judgments: vec![judgment(POSTGRES, TextRelation::Replaced, 0.9)],
                ..hold()
            },
        )
        .expect("answers");
    // The stale field is asked for again, so a held put stores nothing.
    assert_eq!(status(&out.assessment), Some(JudgmentStatus::Stale));
    assert_eq!(out.assessment.judgment_requests.len(), 1);
    assert!(!out.stored);
    assert_eq!(registry.history("P", 0).expect("history").len(), 1);
    // Without the hold the lexical bump is stored.
    let out = registry
        .put(
            parent(later),
            PutOptions {
                judgments: vec![judgment(POSTGRES, TextRelation::Replaced, 0.9)],
                ..opts()
            },
        )
        .expect("stores");
    assert_eq!(status(&out.assessment), Some(JudgmentStatus::Stale));
    assert!(out.stored);
    assert_eq!(out.bump, VersionBump::Minor);
    let history = registry.history("P", 0).expect("history");
    assert!(history.last().expect("a record").judgments.is_empty());
}

#[test]
fn the_analysis_cache_does_not_change_judged_results() {
    let run = |cache: bool| {
        let registry = registry(cache, PAYPAL.0);
        let mut out = Vec::new();
        let held = registry
            .put(
                parent(PAYPAL.1),
                PutOptions {
                    hold_if_flagged: true,
                    ..opts()
                },
            )
            .expect("answers");
        out.push(serde_json::to_string(&held).expect("serializes"));
        for j in [
            judgment(PAYPAL, TextRelation::Narrowed, 0.9),
            judgment(PAYPAL, TextRelation::Equivalent, 0.9),
            judgment(LOGIN, TextRelation::Equivalent, 0.9),
        ] {
            let dry = registry
                .put(
                    parent(PAYPAL.1),
                    PutOptions {
                        dry_run: true,
                        judgments: vec![j],
                        ..opts()
                    },
                )
                .expect("answers");
            out.push(serde_json::to_string(&dry).expect("serializes"));
        }
        let stored = registry
            .put(
                parent(PAYPAL.1),
                PutOptions {
                    judgments: vec![judgment(PAYPAL, TextRelation::Narrowed, 0.9)],
                    ..opts()
                },
            )
            .expect("stores");
        out.push(serde_json::to_string(&stored).expect("serializes"));
        let cmp = registry
            .compare(
                "P",
                SemVer::new(1, 0, 0),
                SemVer::new(2, 0, 0),
                Some(support::now()),
            )
            .expect("compares");
        out.push(json(&cmp));
        out.push(serde_json::to_string(&registry.history("P", 0).expect("history")).expect("ok"));
        out
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn records_and_options_from_before_judgments_still_load() {
    let registry = registry(true, POSTGRES.0);
    let record = registry.history("P", 0).expect("history").remove(0);
    let value = serde_json::to_value(&record).expect("serializes");
    assert!(value.get("judgments").is_none());
    let back: VersionRecord = serde_json::from_value(value).expect("an old record loads");
    assert_eq!(back, record);
    let old: PutOptions = serde_json::from_str(r#"{"dry_run":true}"#).expect("loads");
    assert!(old.judgments.is_empty() && !old.hold_if_flagged);
    let value = serde_json::to_value(PutOptions::default()).expect("serializes");
    assert!(value.get("judgments").is_none() && value.get("hold_if_flagged").is_none());
    let old: ChangeSet =
        serde_json::from_str(r#"{"entity_id":"P","before":null,"after":null}"#).expect("loads");
    assert!(old.judgments.is_empty());
}

// ---------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "login",
    "logout",
    "password",
    "reset",
    "email",
    "oauth",
    "session",
    "expire",
    "days",
    "not",
    "must",
    "should",
    "support",
    "users",
    "api",
    "rate",
    "limit",
    "30",
    "7",
    "billing",
    "the",
    "and",
    "with",
    "except",
    "only",
    "all",
    "http",
    "https",
    "store",
    "postgresql",
    "mongodb",
    "flexible",
    "fixed",
    ".",
    ".",
    "\n- ",
    "нууц",
    "үг",
];

fn words(range: std::ops::Range<usize>) -> impl Strategy<Value = Vec<&'static str>> {
    prop::collection::vec(prop::sample::select(WORDS), range)
}

/// Two texts: unrelated, or the second a small edit of the first.
fn pair() -> impl Strategy<Value = (String, String)> {
    let edited = (
        words(3..24),
        any::<prop::sample::Index>(),
        prop::sample::select(WORDS),
        0u8..4,
    )
        .prop_map(|(ws, at, w, how)| {
            let before = ws.join(" ");
            let mut after = ws.clone();
            let i = at.index(after.len());
            match how {
                0 => after[i] = w,
                1 => after.insert(i, w),
                2 => {
                    after.remove(i);
                }
                _ => after.push(w),
            }
            (before, after.join(" "))
        });
    let unrelated = (words(0..24), words(0..24)).prop_map(|(a, b)| (a.join(" "), b.join(" ")));
    prop_oneof![3 => edited, 1 => unrelated]
}

fn relation() -> impl Strategy<Value = TextRelation> {
    prop::sample::select(vec![
        TextRelation::Equivalent,
        TextRelation::Extended,
        TextRelation::Narrowed,
        TextRelation::Replaced,
        TextRelation::Reversed,
        TextRelation::Undetermined,
    ])
}

fn magnitude(pair: (&str, &str), relation: TextRelation, c: f64) -> f64 {
    description(&assess(pair, vec![judgment(pair, relation, c)])).magnitude
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn nothing_flagged_is_byte_identical((b, a) in pair()) {
        let on = assess((&b, &a), Vec::new());
        let flagged = on.changes.iter().flat_map(|c| &c.fields).any(|f| f.judgment.is_some());
        prop_assert_eq!(flagged, !on.judgment_requests.is_empty());
        let off = unjudged_engine().assess(&request((&b, &a), Vec::new())).expect("assesses");
        if flagged {
            // Only the confidence and the wording move.
            prop_assert_eq!(on.score.to_bits(), off.score.to_bits());
            prop_assert_eq!(on.level, off.level);
            prop_assert_eq!(on.changes[0].bump, off.changes[0].bump);
            prop_assert!(on.confidence <= off.confidence);
        } else {
            prop_assert_eq!(json(&on), json(&off));
        }
    }

    #[test]
    fn judged_assessments_are_well_formed((b, a) in pair(), r in relation(), c in 0.0f64..=1.0) {
        let out = assess((&b, &a), vec![judgment((&b, &a), r, c)]);
        prop_assert!((0.0..=1.0).contains(&out.score));
        prop_assert!((0.0..=1.0).contains(&out.confidence));
        for f in out.changes.iter().flat_map(|c| &c.fields) {
            prop_assert!((0.0..=1.0).contains(&f.magnitude));
            if let Some(j) = &f.judgment {
                prop_assert_eq!(&j.fingerprint, &fingerprint(&b, &a));
                prop_assert_eq!(j.weight.is_some(), j.status == JudgmentStatus::Applied);
                if let Some(w) = j.weight {
                    prop_assert!((0.0..=1.0).contains(&w));
                }
                prop_assert_ne!(j.status, JudgmentStatus::Requested);
            }
        }
        // The answer is a pure function of the request.
        let again = assess((&b, &a), vec![judgment((&b, &a), r, c)]);
        prop_assert_eq!(json(&out), json(&again));
    }

    #[test]
    fn zero_weight_judgments_change_no_number((b, a) in pair(), r in relation()) {
        let plain = numbers(assess((&b, &a), Vec::new()));
        // Confidence at or below the threshold of the relation: no weight.
        let c = if r.is_raising() { 0.5 } else { 0.6 };
        let weak = numbers(assess((&b, &a), vec![judgment((&b, &a), r, c)]));
        prop_assert_eq!(json(&weak), json(&plain.clone()));
        let none = numbers(assess((&b, &a), vec![judgment((&b, &a), r, 0.0)]));
        prop_assert_eq!(json(&none), json(&plain));
    }

    #[test]
    fn judged_magnitude_is_monotone((b, a) in pair(), lo in 0.0f64..=1.0, hi in 0.0f64..=1.0) {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let p = (b.as_str(), a.as_str());
        let lexical = description_magnitude(p);
        let Some(lexical) = lexical else { return Ok(()); };
        let eps = 1e-12;
        for r in [TextRelation::Narrowed, TextRelation::Replaced, TextRelation::Reversed] {
            // More confidence in a raising relation never lowers the magnitude.
            prop_assert!(magnitude(p, r, lo) <= magnitude(p, r, hi) + eps);
            prop_assert!(magnitude(p, r, lo) >= lexical - eps);
        }
        // More confidence in `equivalent` never raises it.
        let r = TextRelation::Equivalent;
        prop_assert!(magnitude(p, r, lo) + eps >= magnitude(p, r, hi));
        prop_assert!(magnitude(p, r, hi) <= lexical + eps);
        // `extended` pulls towards its target, from either side.
        let r = TextRelation::Extended;
        let (m_lo, m_hi, m_full) = (magnitude(p, r, lo), magnitude(p, r, hi), magnitude(p, r, 1.0));
        if m_full >= lexical {
            prop_assert!(lexical <= m_lo + eps && m_lo <= m_hi + eps && m_hi <= m_full + eps);
        } else {
            prop_assert!(lexical + eps >= m_lo && m_lo + eps >= m_hi && m_hi + eps >= m_full);
        }
        // At one confidence: equivalent ≤ extended ≤ every raising relation.
        let eq = magnitude(p, TextRelation::Equivalent, hi);
        let ex = magnitude(p, TextRelation::Extended, hi);
        prop_assert!(eq <= ex + eps);
        for r in [TextRelation::Narrowed, TextRelation::Replaced, TextRelation::Reversed] {
            prop_assert!(ex <= magnitude(p, r, hi) + eps);
        }
    }

    #[test]
    fn flags_do_not_depend_on_judgments_or_order(
        (b, a) in pair(),
        (b2, a2) in pair(),
        r in relation(),
        c in 0.0f64..=1.0,
        stale in any::<bool>(),
    ) {
        let item = |id: &str, text: &str| Entity::new(id, "issue")
            .with_title("Item")
            .with_description(text)
            .with_version(SemVer::new(1, 0, 0));
        let change = |id: &str, b: &str, a: &str| ChangeSet::update(item(id, b), item(id, a));
        let req = AssessmentRequest {
            changes: vec![change("A", &b, &a), change("B", &b2, &a2)],
            now: Some(support::now()),
            ..Default::default()
        };
        let flags = |a: &Assessment| -> Vec<(String, String, Vec<JudgmentReason>)> {
            let mut v: Vec<_> = a.changes.iter().flat_map(|c| c.fields.iter().filter_map(|f| {
                let j = f.judgment.as_ref()?;
                (!j.reasons.is_empty()).then(|| (c.entity_id.clone(), f.field.clone(), j.reasons.clone()))
            })).collect();
            v.sort();
            v
        };
        let engine = Engine::default();
        let plain = engine.assess(&req).expect("assesses");

        let mut judged = req.clone();
        let mut j = judgment((&b, &a), r, c);
        if stale {
            j.fingerprint = fingerprint(&a, &b);
        }
        judged.changes[0].judgments = vec![j];
        let with = engine.assess(&judged).expect("assesses");
        prop_assert_eq!(flags(&plain), flags(&with));

        let mut reversed = req.clone();
        reversed.changes.reverse();
        let rev = engine.assess(&reversed).expect("assesses");
        prop_assert_eq!(flags(&plain), flags(&rev));
        let asked = |a: &Assessment| -> Vec<(String, String, String)> {
            a.judgment_requests.iter()
                .map(|r| (r.entity_id.clone(), r.field.clone(), r.fingerprint.clone()))
                .collect()
        };
        prop_assert_eq!(asked(&plain), asked(&rev));
    }

    #[test]
    fn a_wrong_fingerprint_is_stale_and_changes_nothing((b, a) in pair(), r in relation(), c in 0.0f64..=1.0) {
        prop_assume!(b != a);
        let plain = assess((&b, &a), Vec::new());
        let mut j = judgment((&b, &a), r, c);
        j.fingerprint = fingerprint(&a, &b);
        prop_assume!(j.fingerprint != fingerprint(&b, &a));
        let out = assess((&b, &a), vec![j]);
        for f in out.changes.iter().flat_map(|c| &c.fields) {
            if let Some(j) = &f.judgment {
                prop_assert!(matches!(j.status, JudgmentStatus::Stale | JudgmentStatus::NotFlagged));
            }
        }
        prop_assert_eq!(&out.judgment_requests, &plain.judgment_requests);
        prop_assert_eq!(json(&numbers(out)), json(&numbers(plain)));
    }
}

/// The lexical magnitude of the description change, if it changed.
fn description_magnitude(pair: (&str, &str)) -> Option<f64> {
    let a = unjudged_engine()
        .assess(&request(pair, Vec::new()))
        .expect("assesses");
    a.changes[0]
        .fields
        .iter()
        .find(|f| f.field == DESCRIPTION)
        .map(|f| f.magnitude)
}

// ---------------------------------------------------------------------------
// More gray-zone rows.
// ---------------------------------------------------------------------------

fn flags(before: &str, after: &str) -> Vec<JudgmentReason> {
    Engine::default()
        .compare_text(before, after, None)
        .expect("compares")
        .gray_zone
        .map(|g| g.reasons)
        .unwrap_or_default()
}

#[test]
fn the_gray_zone_rules_flag_what_they_should() {
    use JudgmentReason::{NegationChanged, ParaphraseSuspected, ScopeQualifier, TermSubstitution};
    let long = "Users sign up with an email address. \
        The system sends a confirmation link. \
        Sessions last for thirty minutes of inactivity. \
        Administrators can revoke any session. \
        Passwords are hashed with a slow function. \
        Failed logins are rate limited per account. \
        Audit events are written for every login. \
        Exports are available as CSV files.";
    let reworded = long.replace(
        "Administrators can revoke any session.",
        "Staff members are able to end whichever sign-in they pick.",
    );
    assert_eq!(flags(long, &reworded), [ParaphraseSuspected]);
    assert_eq!(
        flags(
            "Support card payments at checkout.",
            "Support card payments at checkout. PayPal is not supported at checkout.",
        ),
        [NegationChanged]
    );
    assert_eq!(
        flags(
            "Only admins can export the audit report.",
            "Admins can export the audit report.",
        ),
        [ScopeQualifier]
    );
    assert_eq!(
        flags(
            "Do log passwords for every user of the service.",
            "Do not store passwords for every user of the service.",
        ),
        [NegationChanged, TermSubstitution]
    );
}

#[test]
fn the_gray_zone_rules_leave_the_rest_alone() {
    let none = |before: &str, after: &str| {
        assert_eq!(flags(before, after), [], "{before:?} -> {after:?}");
    };
    // A pure addition.
    none(
        "Users can reset their password.",
        "Users can reset their password. Admins can export reports as CSV files.",
    );
    // A swap that comes with a changed number is already counted.
    none(
        "Store user sessions in PostgreSQL with a 30 day expiry.",
        "Store user sessions in PostgreSQL with a 7 day expiry.",
    );
    // An empty side, and the same text.
    none("", "Build the login button");
    none("Build the login button", "");
    none("Build the login button", "Build the login button");
    // A word that turns into its near twin of opposite meaning.
    none(
        "Open the reporting dashboard to internal users of the finance team.",
        "Open the reporting dashboard to external users of the finance team.",
    );
    // Longer than `judge.max_text_chars`.
    let mut policy = Policy::default();
    policy.judge.max_text_chars = 20;
    let d = Engine::new(policy)
        .expect("valid")
        .compare_text(POSTGRES.0, POSTGRES.1, None)
        .expect("compares");
    assert!(d.gray_zone.is_none());
}

#[test]
fn a_title_near_the_threshold_is_flagged() {
    let d = Engine::default()
        .compare_text_as("title", "Login", "Login page", None)
        .expect("compares");
    assert_eq!(
        d.gray_zone.map(|g| g.reasons),
        Some(vec![JudgmentReason::NearThreshold])
    );
}

#[test]
fn the_fingerprint_separates_the_two_texts() {
    assert_eq!(
        fingerprint("", "a"),
        "d476ca72caa7bbdc5b23666b11d6762fa47034529ba497ac557d123407fa8aae"
    );
    assert_eq!(
        fingerprint("a", ""),
        "9388b92a75a2bd819468478a7f33321b47c40cf2837424349f3bea772a70c1ea"
    );
    assert_eq!(
        fingerprint(LOGIN.0, LOGIN.1),
        "2086096a222bb499be60b77fa3ca64d94bf16c2135817a0c5d141cc73e81c86f"
    );
    // Lengths are in bytes, so a moved boundary changes the fingerprint.
    assert_ne!(fingerprint("ab", "c"), fingerprint("a", "bc"));
    assert_eq!(
        fingerprint("Хэрэглэгч нэвтэрч чадна.", "Хэрэглэгч нэвтэрч чадахгүй."),
        fingerprint("Хэрэглэгч нэвтэрч чадна.", "Хэрэглэгч нэвтэрч чадахгүй.")
    );
}
