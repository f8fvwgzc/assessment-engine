//! End-to-end scenarios on a Linear/nexc-style tree:
//!
//! ```text
//! PRJ-1  project "Customer portal v2"
//! ├── ENG-12 issue "Authentication" (started, 13 pts, due 28th)
//! │   ├── ENG-13 sub-issue "Sign-up form"      (started, 5, assignee dorj)
//! │   ├── ENG-14 sub-issue "Password reset"    (todo, 3)
//! │   ├── ENG-15 sub-issue "Session expiry"    (todo, 2, assignee dorj)
//! │   └── ENG-16 sub-issue "Rate-limit logins" (done, 2)
//! ├── ENG-20 issue "Billing page"   (todo, 5)  ← blocked by ENG-12
//! └── ENG-30 issue "Footer layout"  (todo, 1)
//! ```

use assessment_core::prelude::*;
use chrono::{DateTime, TimeZone, Utc};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 7, 9, 0, 0)
        .single()
        .expect("valid date")
}

fn day(d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, d, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn project() -> Entity {
    Entity::new("PRJ-1", "project")
        .with_title("Customer portal v2")
        .with_status(StatusCategory::Started, "Started")
        .with_version(SemVer::new(1, 4, 0))
}

fn auth() -> Entity {
    Entity::new("ENG-12", "issue")
        .with_parent("PRJ-1")
        .with_title("Authentication for the customer portal")
        .with_description(
            "Customers sign in to the portal.\n\n## Acceptance criteria\n\
             - [ ] Sign up and log in with email and password\n\
             - [ ] Password reset by email link\n\
             - [ ] Sessions expire after 30 days\n\
             - [ ] Rate-limit failed logins",
        )
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(13.0)
        .with_due(day(28))
        .with_assignee("saraa")
        .with_relation(RelationKind::Blocks, "ENG-20")
        .with_version(SemVer::new(1, 2, 0))
}

fn sub(id: &str, title: &str, description: &str, cat: StatusCategory, est: f64) -> Entity {
    Entity::new(id, "sub_issue")
        .with_parent("ENG-12")
        .with_title(title)
        .with_description(description)
        .with_status(
            cat,
            match cat {
                StatusCategory::Started => "In Progress",
                StatusCategory::Completed => "Done",
                StatusCategory::Canceled => "Canceled",
                _ => "Todo",
            },
        )
        .with_estimate(est)
        .with_basis("ENG-12", SemVer::new(1, 2, 0))
        .with_version(SemVer::new(1, 0, 0))
}

fn signup() -> Entity {
    sub(
        "ENG-13",
        "Email and password sign-up form",
        "Form with email, password and confirmation; hash passwords with argon2.",
        StatusCategory::Started,
        5.0,
    )
    .with_assignee("dorj")
    .with_due(day(20))
}

fn reset() -> Entity {
    sub(
        "ENG-14",
        "Password reset email flow",
        "Send a reset link by email; the token expires after one hour.",
        StatusCategory::Unstarted,
        3.0,
    )
}

fn sessions() -> Entity {
    sub(
        "ENG-15",
        "Session expiry and refresh",
        "Sessions expire after 30 days of inactivity; refresh tokens rotate.",
        StatusCategory::Unstarted,
        2.0,
    )
    .with_assignee("dorj")
}

fn ratelimit() -> Entity {
    sub(
        "ENG-16",
        "Rate-limit failed logins",
        "Lock an account for 15 minutes after 5 failed login attempts.",
        StatusCategory::Completed,
        2.0,
    )
}

fn billing() -> Entity {
    Entity::new("ENG-20", "issue")
        .with_parent("PRJ-1")
        .with_title("Billing page")
        .with_description("Show invoices and the current plan to signed-in customers.")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_estimate(5.0)
        .with_due(day(30))
        .with_relation(RelationKind::BlockedBy, "ENG-12")
        .with_version(SemVer::new(1, 0, 0))
}

fn footer() -> Entity {
    Entity::new("ENG-30", "issue")
        .with_parent("PRJ-1")
        .with_title("Fix footer layout on mobile")
        .with_description("The footer overlaps the cookie banner below 380px.")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_estimate(1.0)
        .with_version(SemVer::new(1, 0, 0))
}

fn all() -> Vec<Entity> {
    vec![
        project(),
        auth(),
        signup(),
        reset(),
        sessions(),
        ratelimit(),
        billing(),
        footer(),
    ]
}

/// The tree without the entities being changed.
fn context_without(ids: &[&str]) -> Vec<Entity> {
    all()
        .into_iter()
        .filter(|e| !ids.contains(&e.id.as_str()))
        .collect()
}

fn assess(changes: Vec<ChangeSet>) -> Assessment {
    let ids: Vec<String> = changes.iter().map(|c| c.entity_id.clone()).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    Engine::default()
        .assess(&AssessmentRequest {
            changes,
            context: context_without(&ids),
            now: Some(now()),
            ..Default::default()
        })
        .expect("the request is valid")
}

fn impact<'a>(a: &'a Assessment, id: &str) -> Option<&'a EntityImpact> {
    a.impacts.iter().find(|i| i.entity_id == id)
}

fn score(a: &Assessment, id: &str) -> f64 {
    impact(a, id).map_or(0.0, |i| i.score)
}

#[test]
fn parent_description_rewritten_mid_sprint() {
    let after = auth().with_description(
        "Customers sign in to the portal with their company identity.\n\n## Acceptance criteria\n\
         - [ ] Log in with Google Workspace (OAuth 2.0 / OIDC)\n\
         - [ ] Log in with Microsoft Entra ID (OIDC)\n\
         - [ ] Sessions expire after 7 days\n\
         - [ ] Rate-limit failed logins\n\nPassword login is no longer needed.",
    );
    let a = assess(vec![ChangeSet::update(auth(), after)]);

    assert_eq!(a.changes[0].bump, VersionBump::Major);
    assert_eq!(a.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
    assert!(a.level >= ImpactLevel::High, "{}", a.summary);
    assert!(matches!(
        a.recommendation,
        Recommendation::PartialReplan | Recommendation::FullReplan
    ));
    assert_eq!(a.replan_root.as_deref(), Some("ENG-12"));

    // Password work is what the rewrite removed.
    assert_eq!(
        impact(&a, "ENG-14").expect("reset").action,
        Action::ObsoleteCandidate
    );
    assert_eq!(
        impact(&a, "ENG-13").expect("signup").action,
        Action::ObsoleteCandidate
    );
    // Session expiry was reworded (30 → 7 days).
    assert_eq!(
        impact(&a, "ENG-15").expect("sessions").action,
        Action::UpdateScope
    );
    // The untouched, completed criterion matters least among the children.
    let rl = score(&a, "ENG-16");
    for id in ["ENG-13", "ENG-14", "ENG-15"] {
        assert!(score(&a, id) > rl, "{id} should outrank ENG-16");
    }
    assert!(rl < 0.25, "ENG-16 at {rl}");
    // Unrelated work under the same project is not touched by the description.
    assert!(score(&a, "ENG-30") < 0.05);
    // Every child pinned 1.2.0; 2.0.0 is breaking.
    assert_eq!(
        impact(&a, "ENG-14").expect("reset").basis_status,
        BasisStatus::Incompatible
    );
}

#[test]
fn title_typo_is_a_patch_with_no_fallout() {
    let a = assess(vec![ChangeSet::update(
        auth(),
        auth().with_title("Authentcation for the customer portal"),
    )]);
    assert_eq!(a.changes[0].bump, VersionBump::Patch);
    assert!(a.level <= ImpactLevel::Low, "{}", a.summary);
    assert!(
        a.impacts
            .iter()
            .filter(|i| !i.changed)
            .all(|i| i.level <= ImpactLevel::Low)
    );
    // Unaffected children are not listed, or listed as still compatible.
    assert!(impact(&a, "ENG-14").is_none_or(|i| i.basis_status <= BasisStatus::Compatible));
}

#[test]
fn sub_issue_estimate_more_than_doubles() {
    let a = assess(vec![ChangeSet::update(
        signup(),
        signup().with_estimate(13.0),
    )]);
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    let parent = impact(&a, "ENG-12").expect("parent affected");
    assert_eq!(parent.action, Action::ReEstimate);
    // The sibling sharing the assignee competes for capacity; it does not re-estimate.
    let sibling = impact(&a, "ENG-15").expect("the sibling with the same assignee");
    assert!(sibling.score >= 0.05, "{}", sibling.score);
    assert_eq!(sibling.action, Action::Notify);
    // 13 + 3 + 2 + 2 = 20 > 13 × 1.1
    assert!(
        a.violations
            .iter()
            .any(|v| v.kind == ViolationKind::EstimateOverflow && v.introduced)
    );
}

#[test]
fn small_reestimate_is_minor_or_patch() {
    let a = assess(vec![ChangeSet::update(
        signup(),
        signup().with_estimate(6.0),
    )]);
    assert!(a.changes[0].bump <= VersionBump::Minor);
    assert!(a.level <= ImpactLevel::Medium);
}

#[test]
fn completed_sub_issue_reopened() {
    let reopened = ratelimit().with_status(StatusCategory::Started, "In Progress");
    let a = assess(vec![ChangeSet::update(ratelimit(), reopened)]);
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    assert!(score(&a, "ENG-12") > 0.1, "the parent's progress regresses");
}

#[test]
fn parent_canceled_with_work_in_progress() {
    let canceled = auth().with_status(StatusCategory::Canceled, "Canceled");
    let a = assess(vec![ChangeSet::update(auth(), canceled)]);
    assert!(a.level >= ImpactLevel::High, "{}", a.summary);
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    for id in ["ENG-13", "ENG-14", "ENG-15"] {
        assert_eq!(
            impact(&a, id).expect("active child").action,
            Action::CancelOrRehome,
            "{id}"
        );
    }
    // Done work is not orphaned.
    assert!(impact(&a, "ENG-16").is_none_or(|i| i.action != Action::CancelOrRehome));
    assert!(
        a.violations
            .iter()
            .any(|v| v.kind == ViolationKind::OrphanedWork && v.introduced)
    );
    // The blocked billing page lost its blocker.
    assert!(
        a.violations
            .iter()
            .any(|v| v.kind == ViolationKind::BlockedByClosed)
    );
}

#[test]
fn reassignment_mid_work() {
    let a = assess(vec![ChangeSet::update(
        signup(),
        signup().with_assignee("bold"),
    )]);
    assert_eq!(a.changes[0].bump, VersionBump::Minor);
    // The sibling with the same (old) assignee shares the capacity shift.
    assert!(score(&a, "ENG-15") > score(&a, "ENG-14"));
    assert!(a.level <= ImpactLevel::Medium);
}

#[test]
fn blocker_comment_is_versionless_but_reaches_dependents() {
    let cs = ChangeSet::activity(
        "ENG-12",
        vec![ActivityEvent::comment(
            "ENG-12",
            "We're blocked waiting on the identity provider contract. Also the client changed the scope again.",
        )],
    );
    let a = assess(vec![cs]);
    assert_eq!(a.changes[0].bump, VersionBump::None);
    assert_eq!(a.changes[0].change_type, ChangeType::Activity);
    let cats: Vec<_> = a.changes[0]
        .signals
        .iter()
        .map(|s| s.category.as_str())
        .collect();
    assert!(
        cats.contains(&"blocker") && cats.contains(&"scope"),
        "{cats:?}"
    );
    assert!(a.level >= ImpactLevel::Low);
    assert!(
        score(&a, "ENG-20") > 0.0,
        "the blocked billing page hears about it"
    );
}

#[test]
fn neutral_comment_is_noise() {
    let a = assess(vec![ChangeSet::activity(
        "ENG-13",
        vec![ActivityEvent::comment(
            "ENG-13",
            "Pushed a first draft of the form layout",
        )],
    )]);
    assert_eq!(a.level, ImpactLevel::None, "{}", a.summary);
}

#[test]
fn parent_due_date_pulled_in_before_a_child() {
    let a = assess(vec![ChangeSet::update(auth(), auth().with_due(day(15)))]);
    assert!(
        a.violations
            .iter()
            .any(|v| v.kind == ViolationKind::ScheduleConflict
                && v.entities[0] == "ENG-13"
                && v.introduced)
    );
    assert_eq!(
        impact(&a, "ENG-13").expect("signup").action,
        Action::Reschedule
    );
}

#[test]
fn blocker_slips_past_the_blocked_due_date() {
    let a = assess(vec![ChangeSet::update(
        auth(),
        auth().with_due(
            Utc.with_ymd_and_hms(2026, 12, 5, 0, 0, 0)
                .single()
                .expect("valid"),
        ),
    )]);
    assert!(
        a.violations
            .iter()
            .any(|v| v.kind == ViolationKind::DependencyConflict && v.introduced)
    );
    assert_eq!(
        impact(&a, "ENG-20").expect("billing").action,
        Action::Reschedule
    );
}

#[test]
fn urgent_blocked_issue_reprioritizes_its_blocker() {
    let urgent = billing().with_field(field::PRIORITY, FieldValue::ordinal(4, 4));
    let before = billing().with_field(field::PRIORITY, FieldValue::ordinal(2, 4));
    let a = assess(vec![ChangeSet::update(before, urgent)]);
    let blocker = impact(&a, "ENG-12").expect("the blocker is affected upstream");
    assert_eq!(blocker.action, Action::Reprioritize);
    assert_eq!(blocker.channel, Channel::Upstream);
}

#[test]
fn moved_to_another_project_drags_children_along() {
    let with_project = auth().with_field(field::PROJECT, FieldValue::reference("PRJ-1"));
    let moved = auth().with_field(field::PROJECT, FieldValue::reference("PRJ-9"));
    let a = assess(vec![ChangeSet::update(with_project, moved)]);
    assert!(a.changes[0].bump >= VersionBump::Minor);
    assert!(score(&a, "ENG-13") > 0.0);
}

#[test]
fn sub_issue_moved_to_another_parent() {
    let moved = sessions().with_parent("ENG-20");
    let a = assess(vec![ChangeSet::update(sessions(), moved)]);
    assert!(a.changes[0].fields.iter().any(|f| f.field == field::PARENT));
    assert!(score(&a, "ENG-12") > 0.0, "the old parent loses scope");
    assert!(score(&a, "ENG-20") > 0.0, "the new parent gains scope");
}

#[test]
fn significant_label_added() {
    let before = signup().with_field(field::LABELS, FieldValue::set(["frontend"]));
    let after = signup().with_field(field::LABELS, FieldValue::set(["frontend", "security"]));
    let a = assess(vec![ChangeSet::update(before, after)]);
    assert!(a.changes[0].bump >= VersionBump::Minor);
    let plain = assess(vec![ChangeSet::update(
        signup().with_field(field::LABELS, FieldValue::set(["frontend"])),
        signup().with_field(field::LABELS, FieldValue::set(["frontend", "polish"])),
    )]);
    assert!(plain.changes[0].raw_score < a.changes[0].raw_score);
}

#[test]
fn new_sub_issue_adds_scope_to_the_parent() {
    let new = sub(
        "ENG-17",
        "Audit log of sign-ins",
        "Record every sign-in with IP and device.",
        StatusCategory::Unstarted,
        3.0,
    );
    let a = Engine::default()
        .assess(&AssessmentRequest {
            changes: vec![ChangeSet::create(new)],
            context: all(),
            now: Some(now()),
            ..Default::default()
        })
        .expect("valid");
    assert_eq!(a.changes[0].bump, VersionBump::Initial);
    assert_eq!(a.changes[0].next_version, Some(SemVer::INITIAL));
    assert_eq!(
        impact(&a, "ENG-12").expect("parent").action,
        Action::ReEstimate
    );
}

#[test]
fn deleting_a_parent_orphans_its_children() {
    let a = assess(vec![ChangeSet::delete(auth())]);
    assert_eq!(a.changes[0].change_type, ChangeType::Deleted);
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    assert_eq!(
        impact(&a, "ENG-14").expect("child").action,
        Action::CancelOrRehome
    );
    assert_eq!(
        impact(&a, "ENG-14").expect("child").basis_status,
        BasisStatus::Removed
    );
    assert!(a.level >= ImpactLevel::High);
}

#[test]
fn field_deltas_match_snapshots() {
    let after = signup()
        .with_estimate(8.0)
        .with_status(StatusCategory::Unstarted, "Todo");
    let by_snapshot = assess(vec![ChangeSet::update(signup(), after.clone())]);
    let by_delta = assess(vec![ChangeSet::fields(
        after,
        vec![
            FieldChange {
                field: field::ESTIMATE.into(),
                before: Some(FieldValue::Number(5.0)),
                after: Some(FieldValue::Number(8.0)),
            },
            FieldChange {
                field: field::STATUS.into(),
                before: Some(FieldValue::status(StatusCategory::Started, "In Progress")),
                after: Some(FieldValue::status(StatusCategory::Unstarted, "Todo")),
            },
        ],
    )]);
    assert_eq!(by_snapshot.score, by_delta.score);
    assert_eq!(by_snapshot.changes[0].bump, by_delta.changes[0].bump);
    assert_eq!(by_snapshot.impacts, by_delta.impacts);
}

#[test]
fn parent_and_child_changed_together() {
    let a = assess(vec![
        ChangeSet::update(auth(), auth().with_estimate(21.0)),
        ChangeSet::update(signup(), signup().with_estimate(8.0)),
    ]);
    assert_eq!(a.changes.len(), 2);
    let parent = impact(&a, "ENG-12").expect("parent");
    assert!(parent.changed);
    // The child's change also reaches the changed parent.
    assert!(
        parent.contributions.iter().any(|c| c.source_id == "ENG-13")
            || parent.score >= a.changes[0].local_score
    );
}

#[test]
fn embeddings_catch_meaning_changes_words_miss() {
    let mut before = auth();
    let mut after = auth().with_title("Authentication for the customer portal!");
    before.embedding = Some(vec![0.9, 0.1, 0.0, 0.0]);
    after.embedding = Some(vec![0.0, 0.1, 0.9, 0.2]);
    let with = assess(vec![ChangeSet::update(before, after.clone())]);
    let mut no_emb = after;
    no_emb.embedding = None;
    let without = assess(vec![ChangeSet::update(auth(), no_emb)]);
    assert!(with.changes[0].raw_score > without.changes[0].raw_score);
}

#[test]
fn policy_overrides_change_the_verdict() {
    let after = auth().with_estimate(20.0);
    let req = AssessmentRequest {
        changes: vec![ChangeSet::update(auth(), after)],
        context: context_without(&["ENG-12"]),
        now: Some(now()),
        ..Default::default()
    };
    let default = Engine::default().assess(&req).expect("valid");
    let lenient = Engine::default()
        .with_policy_patch(&serde_json::json!({
            "fields": {"estimate": {"severity": 0.1}},
            "thresholds": {"low": 0.3, "medium": 0.6, "high": 0.8, "critical": 0.95}
        }))
        .expect("valid patch")
        .assess(&req)
        .expect("valid");
    assert!(lenient.score < default.score);
    assert!(lenient.level < default.level);
}

#[test]
fn ranks_and_scores_are_well_formed() {
    let a = assess(vec![ChangeSet::update(
        auth(),
        auth().with_status(StatusCategory::Canceled, "Canceled"),
    )]);
    for (n, i) in a.impacts.iter().enumerate() {
        assert_eq!(i.rank as usize, n + 1);
        assert!((0.0..=1.0).contains(&i.score));
        assert!(i.effort_at_risk <= i.effort + 1e-9);
    }
    assert!(a.impacts.windows(2).all(|w| w[0].score >= w[1].score));
    assert!((0.0..=1.0).contains(&a.confidence));
    assert_eq!(a.impacts_total as usize, a.impacts.len());
}

#[test]
fn missing_context_lowers_confidence() {
    let full = assess(vec![ChangeSet::update(
        signup(),
        signup().with_estimate(8.0),
    )]);
    let bare = Engine::default()
        .assess(&AssessmentRequest {
            changes: vec![ChangeSet::update(signup(), signup().with_estimate(8.0))],
            now: Some(now()),
            ..Default::default()
        })
        .expect("valid");
    assert!(bare.confidence < full.confidence);
}

#[test]
fn large_tree_stays_fast() {
    // 1 project → 50 issues → 40 sub-issues each (2,051 entities).
    let mut context = vec![project()];
    for i in 0..50 {
        let id = format!("I-{i}");
        context.push(
            Entity::new(id.clone(), "issue")
                .with_parent("PRJ-1")
                .with_title(format!("Issue {i} about area {}", i % 7))
                .with_description(format!(
                    "Deliver the area {} capability with tests and docs.",
                    i % 7
                ))
                .with_status(StatusCategory::Started, "In Progress")
                .with_estimate(8.0),
        );
        for j in 0..40 {
            context.push(
                Entity::new(format!("S-{i}-{j}"), "sub_issue")
                    .with_parent(id.clone())
                    .with_title(format!("Step {j} of area {}", i % 7))
                    .with_status(StatusCategory::Unstarted, "Todo")
                    .with_estimate(1.0),
            );
        }
    }
    let before = project().with_description("Ship the portal by December.");
    let after =
        project().with_description("Ship the portal by October, without the reporting module.");
    let started = std::time::Instant::now();
    let a = Engine::default()
        .assess(&AssessmentRequest {
            changes: vec![ChangeSet::update(before, after)],
            context: context.into_iter().filter(|e| e.id != "PRJ-1").collect(),
            now: Some(now()),
            ..Default::default()
        })
        .expect("valid");
    let elapsed = started.elapsed();
    assert!(a.impacts_total > 50);
    assert!(a.impacts.len() <= 100, "top_k defaults to 100");
    assert!(elapsed.as_secs_f64() < 10.0, "took {elapsed:?}");
}

// ---------------------------------------------------------------------------
// A second tree, for the regressions of the review findings (the number in
// each test's name is the finding's):
//
// PRJ  project "Web shop"
// ├── PAY  issue "Checkout payments" (started, 13 pts, due 28th) ── blocks ──┐
// │   ├── CARD "Credit card payment form" (started, 5, due 20th)            │
// │   ├── PP   "PayPal checkout button"   (todo, 3)                         │
// │   ├── RCPT "Payment receipt email"    (done, 2)                         │
// │   └── SAVE "Saved cards"              (backlog, 3)                      │
// └── CONF issue "Order confirmation page" (todo, 3, due 30th) ◄────────────┘
// ---------------------------------------------------------------------------

const PAY_DESC: &str = "Customers pay for their order at checkout.\n\n## Acceptance criteria\n\
                        - [ ] Pay by credit card (Visa, Mastercard)\n\
                        - [ ] Pay with PayPal\n\
                        - [ ] Email a receipt after payment\n\
                        - [ ] Save a card for later payments";

fn shop() -> Entity {
    Entity::new("PRJ", "project")
        .with_title("Web shop")
        .with_status(StatusCategory::Started, "Started")
}

fn pay() -> Entity {
    Entity::new("PAY", "issue")
        .with_parent("PRJ")
        .with_title("Checkout payments")
        .with_description(PAY_DESC)
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(13.0)
        .with_due(day(28))
        .with_relation(RelationKind::Blocks, "CONF")
        .with_version(SemVer::new(1, 2, 0))
}

fn pay_child(id: &str, title: &str, description: &str, cat: StatusCategory, est: f64) -> Entity {
    let state = match cat {
        StatusCategory::Started => "In Progress",
        StatusCategory::Completed => "Done",
        StatusCategory::Backlog => "Backlog",
        StatusCategory::Canceled => "Canceled",
        StatusCategory::Unstarted => "Todo",
    };
    Entity::new(id, "sub_issue")
        .with_parent("PAY")
        .with_title(title)
        .with_description(description)
        .with_status(cat, state)
        .with_estimate(est)
        .with_basis("PAY", SemVer::new(1, 2, 0))
        .with_version(SemVer::new(1, 0, 0))
}

fn card() -> Entity {
    pay_child(
        "CARD",
        "Credit card payment form",
        "Card number, expiry and CVC fields with validation; tokenize with Stripe.",
        StatusCategory::Started,
        5.0,
    )
    .with_due(day(20))
}

fn paypal() -> Entity {
    pay_child(
        "PP",
        "PayPal checkout button",
        "Show the PayPal button and handle the redirect back to the order.",
        StatusCategory::Unstarted,
        3.0,
    )
}

fn receipt() -> Entity {
    pay_child(
        "RCPT",
        "Payment receipt email",
        "Send an email receipt with the order total after a successful payment.",
        StatusCategory::Completed,
        2.0,
    )
}

fn saved() -> Entity {
    pay_child(
        "SAVE",
        "Saved cards",
        "Let customers save a card for later payments.",
        StatusCategory::Backlog,
        3.0,
    )
}

fn confirmation() -> Entity {
    Entity::new("CONF", "issue")
        .with_parent("PRJ")
        .with_title("Order confirmation page")
        .with_description("Show the order summary after payment.")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_estimate(3.0)
        .with_due(day(30))
        .with_relation(RelationKind::BlockedBy, "PAY")
}

fn shop_tree() -> Vec<Entity> {
    vec![
        shop(),
        pay(),
        card(),
        paypal(),
        receipt(),
        saved(),
        confirmation(),
    ]
}

/// Assesses `changes` with `tree` as context, minus the entities whose
/// snapshots the changes carry (activity targets stay in the context).
fn assess_in(tree: Vec<Entity>, changes: Vec<ChangeSet>) -> Assessment {
    let ids: Vec<String> = changes
        .iter()
        .filter(|c| c.before.is_some() || c.after.is_some())
        .map(|c| c.entity_id.clone())
        .collect();
    Engine::default()
        .assess(&AssessmentRequest {
            changes,
            context: tree.into_iter().filter(|e| !ids.contains(&e.id)).collect(),
            now: Some(now()),
            ..Default::default()
        })
        .expect("the request is valid")
}

fn shop_assess(changes: Vec<ChangeSet>) -> Assessment {
    assess_in(shop_tree(), changes)
}

fn without_criterion(line: &str) -> Entity {
    pay().with_description(PAY_DESC.replace(&format!("- [ ] {line}\n"), ""))
}

fn action(a: &Assessment, id: &str) -> Option<Action> {
    impact(a, id).map(|i| i.action)
}

#[test]
fn f04_removed_criterion_flags_the_sub_issue_that_implemented_it() {
    // PayPal removed: the PayPal sub-issue's planned scope is gone.
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        without_criterion("Pay with PayPal"),
    )]);
    let pp = impact(&a, "PP").expect("listed");
    assert_eq!(pp.action, Action::ObsoleteCandidate);
    assert!(pp.level >= ImpactLevel::High, "PP at {}", pp.score);
    assert_eq!(a.impacts[0].entity_id, "PP", "{}", a.summary);
    for other in ["CARD", "RCPT", "SAVE"] {
        assert!(score(&a, other) < 0.25, "{other} at {}", score(&a, other));
    }
    // The credit-card criterion: the in-progress card form, though "card"
    // and "pay" appear in most siblings.
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        without_criterion("Pay by credit card (Visa, Mastercard)"),
    )]);
    let c = impact(&a, "CARD").expect("listed");
    assert_eq!(c.action, Action::ObsoleteCandidate);
    assert!(c.level >= ImpactLevel::High, "CARD at {}", c.score);
    assert!(c.score > 5.0 * score(&a, "RCPT"));
    // With a date pull-in in the same edit, obsolescence still wins over rescheduling.
    let a = shop_assess(vec![
        ChangeSet::update(
            pay(),
            without_criterion("Pay with PayPal").with_due(day(19)),
        )
        .with_events(vec![ActivityEvent::comment(
            "PAY",
            "PayPal is no longer needed",
        )]),
    ]);
    assert_eq!(action(&a, "PP"), Some(Action::ObsoleteCandidate));
    assert_eq!(
        action(&a, "CARD"),
        Some(Action::Reschedule),
        "{}",
        a.summary
    );
}

#[test]
fn f06_violation_floor_and_action_go_to_the_entity_that_needs_action() {
    // One sub-issue's estimate grows past the parent's: only the parent re-estimates.
    let a = shop_assess(vec![ChangeSet::update(card(), card().with_estimate(8.0))]);
    let v = a
        .violations
        .iter()
        .find(|v| v.kind == ViolationKind::EstimateOverflow)
        .expect("overflow");
    assert!(v.introduced);
    assert_eq!(action(&a, "PAY"), Some(Action::ReEstimate));
    assert_ne!(action(&a, "CARD"), Some(Action::ReEstimate), "it just did");
    for sibling in ["PP", "RCPT", "SAVE"] {
        assert!(
            impact(&a, sibling).is_none_or(|i| i.score < 0.25 && i.action != Action::ReEstimate),
            "{sibling}"
        );
    }
    // A canceled parent is not told to cancel itself; what it blocked replaces its blocker.
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        pay().with_status(StatusCategory::Canceled, "Canceled"),
    )]);
    assert_ne!(action(&a, "PAY"), Some(Action::CancelOrRehome));
    assert_eq!(action(&a, "CONF"), Some(Action::ResolveConflict));
    assert_eq!(action(&a, "CARD"), Some(Action::CancelOrRehome));
    // A parent closed too early: the parent reviews, its children are not floored.
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        pay().with_status(StatusCategory::Completed, "Done"),
    )]);
    assert_eq!(action(&a, "PAY"), Some(Action::Review));
    for child in ["CARD", "PP", "SAVE"] {
        assert!(score(&a, child) < 0.25, "{child} at {}", score(&a, child));
    }
}

#[test]
fn f12_propagation_does_not_depend_on_the_order_of_the_changes() {
    // 300 siblings under BIG, 12 of which are re-estimated, and AUTH, whose
    // description is rewritten under three in-progress sub-issues. With a
    // small step budget, the shared counter used to run dry on the siblings
    // and AUTH's sub-issues vanished when AUTH came last.
    let big = Entity::new("BIG", "issue")
        .with_title("Catalogue clean-up")
        .with_status(StatusCategory::Started, "In Progress");
    let siblings: Vec<Entity> = (0..300)
        .map(|k| {
            Entity::new(format!("S{k:03}"), "sub_issue")
                .with_parent("BIG")
                .with_title(format!("Clean up category {k}"))
                .with_status(StatusCategory::Started, "In Progress")
                .with_estimate(3.0)
        })
        .collect();
    let auth = Entity::new("AUTH", "issue")
        .with_title("Sign-in")
        .with_description("- [ ] Email and password login\n- [ ] Password reset by email")
        .with_status(StatusCategory::Started, "In Progress");
    let kids = [
        ("A1", "Email and password login form"),
        ("A2", "Password reset email"),
        ("A3", "Sign-in audit log"),
    ]
    .map(|(id, title)| {
        Entity::new(id, "sub_issue")
            .with_parent("AUTH")
            .with_title(title)
            .with_status(StatusCategory::Started, "In Progress")
    });
    let rewrite = ChangeSet::update(
        auth.clone(),
        auth.clone()
            .with_description("- [ ] Google single sign-on only"),
    );
    let tweaks: Vec<ChangeSet> = siblings
        .iter()
        .take(12)
        .map(|s| ChangeSet::update(s.clone(), s.clone().with_estimate(4.0)))
        .collect();
    let mut context = vec![big];
    context.extend(siblings.iter().skip(12).cloned());
    context.extend(kids);
    let engine = Engine::default()
        .with_policy_patch(&serde_json::json!({"propagation": {"max_steps": 2000}}))
        .expect("valid patch");
    let run = |changes: Vec<ChangeSet>| {
        engine
            .assess(&AssessmentRequest {
                changes,
                context: context.clone(),
                now: Some(now()),
                options: AssessOptions {
                    top_k: 0,
                    ..AssessOptions::default()
                },
                ..Default::default()
            })
            .expect("valid")
    };
    let mut first = vec![rewrite.clone()];
    first.extend(tweaks.iter().cloned());
    let mut last = tweaks.clone();
    last.push(rewrite);
    let (a, b) = (run(first), run(last));
    assert_eq!(a.impacts, b.impacts);
    assert_eq!((a.score, a.level), (b.score, b.level));
    for kid in ["A1", "A2"] {
        assert!(
            score(&b, kid) > 0.25,
            "{kid} at {} with AUTH last",
            score(&b, kid)
        );
    }
}

#[test]
fn f14_an_already_stale_pin_does_not_raise_later_changes() {
    let parent = Entity::new("P", "issue")
        .with_title("Checkout flow")
        .with_description("- [ ] Card\n- [ ] PayPal")
        .with_status(StatusCategory::Started, "In Progress")
        .with_version(SemVer::new(2, 0, 0));
    let child = Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("PayPal button")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_basis("P", SemVer::new(1, 0, 0));
    let typo = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::update(
            parent.clone(),
            parent.clone().with_title("Checkout flwo"),
        )],
    );
    assert_eq!(typo.changes[0].bump, VersionBump::Patch);
    assert!(typo.level <= ImpactLevel::Low, "{}", typo.summary);
    assert!(impact(&typo, "C").is_none_or(|i| !i.basis_introduced && i.action != Action::Review));
    let comment = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::activity(
            "P",
            vec![ActivityEvent::comment("P", "thanks!")],
        )],
    );
    assert!(comment.level <= ImpactLevel::Low, "{}", comment.summary);
    // A breaking change still flags the pins it breaks.
    let current = Entity {
        basis: Vec::new(),
        ..child.clone()
    }
    .with_basis("P", SemVer::new(2, 0, 0));
    let cut = assess_in(
        vec![parent.clone(), current],
        vec![ChangeSet::update(
            parent.clone(),
            parent.clone().with_description("- [ ] Card"),
        )],
    );
    let c = impact(&cut, "C").expect("listed");
    assert_eq!(c.basis_status, BasisStatus::Incompatible);
    assert!(c.basis_introduced);
    assert!(cut.level >= ImpactLevel::Medium);
}

#[test]
fn f16_f27_an_estimate_overflow_that_existed_is_not_introduced() {
    // The parent is at 10 while its children already sum to 13.
    let over = |est: f64| -> Vec<Entity> {
        shop_tree()
            .into_iter()
            .map(|e| {
                if e.id == "PAY" {
                    e.with_estimate(est)
                } else {
                    e
                }
            })
            .collect()
    };
    let gift = pay_child(
        "GIFT",
        "Gift cards",
        "Accept gift cards.",
        StatusCategory::Unstarted,
        1.0,
    );
    let a = assess_in(over(10.0), vec![ChangeSet::create(gift)]);
    let v = a
        .violations
        .iter()
        .find(|v| v.kind == ViolationKind::EstimateOverflow)
        .expect("still over");
    assert!(!v.introduced);
    assert!(v.message.contains("got worse"), "{}", v.message);
    assert!(a.level <= ImpactLevel::Low, "{}", a.summary);
    // Canceling a child shrinks the overflow: certainly not introduced.
    let a = assess_in(
        over(8.0),
        vec![ChangeSet::update(
            saved(),
            saved().with_status(StatusCategory::Canceled, "Canceled"),
        )],
    );
    assert!(
        a.violations
            .iter()
            .all(|v| v.kind != ViolationKind::EstimateOverflow || !v.introduced)
    );
    for child in ["CARD", "PP", "RCPT"] {
        assert_ne!(action(&a, child), Some(Action::ReEstimate), "{child}");
    }
}

#[test]
fn f17_actions_follow_the_channel_and_the_target_state() {
    // A done sub-issue reopened: its in-progress parent is not "re-verified".
    let a = shop_assess(vec![ChangeSet::update(
        receipt(),
        receipt().with_status(StatusCategory::Started, "In Progress"),
    )]);
    assert_ne!(action(&a, "PAY"), Some(Action::Reverify));
    assert_ne!(action(&a, "PRJ"), Some(Action::Reverify));
    // A reopened parent: done children re-verify, the blocked item reschedules.
    let done = pay().with_status(StatusCategory::Completed, "Done");
    let a = assess_in(
        shop_tree()
            .into_iter()
            .map(|e| if e.id == "PAY" { done.clone() } else { e })
            .collect(),
        vec![ChangeSet::update(
            done.clone(),
            done.clone()
                .with_status(StatusCategory::Started, "In Progress"),
        )],
    );
    assert_eq!(action(&a, "RCPT"), Some(Action::Reverify));
    assert_eq!(action(&a, "CONF"), Some(Action::Reschedule));
    // The parent's estimate grows: done work is not re-estimated, the blocked item reschedules.
    let a = shop_assess(vec![ChangeSet::update(pay(), pay().with_estimate(21.0))]);
    assert_ne!(action(&a, "RCPT"), Some(Action::ReEstimate));
    assert_eq!(action(&a, "CONF"), Some(Action::Reschedule));
    // A total rewrite: done work is notified, never "obsolete"; the
    // in-progress work outranks it.
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        pay().with_description(
            "Customers pay by bank transfer only. Show IBAN and a payment reference. \
             Send a payment receipt email after the transfer arrives.",
        ),
    )]);
    assert!(impact(&a, "RCPT").is_none_or(|i| i.action == Action::Notify));
    assert!(score(&a, "CARD") > score(&a, "RCPT"));
    // A sub-issue canceled in the same request is not "obsolete".
    let a = shop_assess(vec![
        ChangeSet::update(pay(), without_criterion("Pay with PayPal")),
        ChangeSet::update(
            paypal(),
            paypal().with_status(StatusCategory::Canceled, "Canceled"),
        ),
    ]);
    assert_ne!(action(&a, "PP"), Some(Action::ObsoleteCandidate));
}

#[test]
fn f18_comments_widgets_are_noise_slips_reach_what_they_block() {
    let comment = |id: &str, body: &str| {
        shop_assess(vec![ChangeSet::activity(
            id,
            vec![ActivityEvent::comment(id, body)],
        )])
    };
    for body in [
        "Implemented the card brand drop-down and the expiry picker.",
        "I will rewrite the unit test names to match the new convention.",
    ] {
        let a = comment("CARD", body);
        assert!(a.level <= ImpactLevel::Low, "{body}: {}", a.summary);
    }
    let a = comment(
        "PAY",
        "This will slip by a week, Stripe review is taking longer than expected.",
    );
    let conf = impact(&a, "CONF").expect("the blocked page hears about it");
    assert_eq!(conf.action, Action::Reschedule);
    assert!(conf.level >= ImpactLevel::Low, "{}", conf.score);
    let a = comment(
        "CARD",
        "Blocked: waiting on Stripe to approve the account, cannot proceed",
    );
    assert_eq!(action(&a, "PAY"), Some(Action::Reschedule));
    let a = comment(
        "PAY",
        "Client says PayPal is no longer needed, please drop it",
    );
    let pp = impact(&a, "PP").expect("the named sub-issue hears about it");
    assert!(pp.level >= ImpactLevel::Low, "{}", pp.score);
    assert!(pp.score > 2.0 * score(&a, "CARD"));
}

#[test]
fn f19_premature_close_is_a_review_and_a_replan_always_has_a_root() {
    let a = shop_assess(vec![ChangeSet::update(
        pay(),
        pay().with_status(StatusCategory::Completed, "Done"),
    )]);
    assert_eq!(a.level, ImpactLevel::Medium, "{}", a.summary);
    assert_eq!(a.recommendation, Recommendation::Review);
    // Canceling a started parent whose children are done but which blocks another issue.
    let done: Vec<Entity> = shop_tree()
        .into_iter()
        .filter(|e| e.id != "PAY")
        .map(|e| {
            if e.parent_id.as_deref() == Some("PAY") {
                e.with_status(StatusCategory::Completed, "Done")
            } else {
                e
            }
        })
        .collect();
    let a = assess_in(
        done,
        vec![ChangeSet::update(
            pay(),
            pay().with_status(StatusCategory::Canceled, "Canceled"),
        )],
    );
    assert!(
        a.recommendation < Recommendation::PartialReplan || a.replan_root.is_some(),
        "{}",
        a.summary
    );
}

#[test]
fn f20_f23_canceling_started_work_weighs_like_deleting_it() {
    let canceled = shop_assess(vec![ChangeSet::update(
        card(),
        card().with_status(StatusCategory::Canceled, "Canceled"),
    )]);
    let deleted = shop_assess(vec![ChangeSet::delete(card())]);
    assert!(canceled.level >= ImpactLevel::High, "{}", canceled.summary);
    assert_eq!(canceled.level, deleted.level);
    let parent = impact(&canceled, "PAY").expect("parent");
    assert_eq!(parent.action, Action::ReEstimate);
    assert!(
        parent.reasons.iter().any(|r| r.contains("% of its effort")),
        "{:?}",
        parent.reasons
    );
    // The local score is monotone in how disruptive the move is.
    let local = |to: StatusCategory| {
        shop_assess(vec![ChangeSet::update(card(), card().with_status(to, "x"))]).changes[0]
            .local_score
    };
    assert!(local(StatusCategory::Canceled) > local(StatusCategory::Backlog));
    assert!(local(StatusCategory::Backlog) > local(StatusCategory::Unstarted));
    // Finishing work is not "rework".
    let done = shop_assess(vec![ChangeSet::update(
        card(),
        card().with_status(StatusCategory::Completed, "Done"),
    )]);
    let phase = done.changes[0]
        .modifiers
        .iter()
        .find(|m| m.name == "phase")
        .expect("phase");
    assert!(phase.factor <= 1.0, "{phase:?}");
    // Deleting finished work is cleanup.
    let cleanup = shop_assess(vec![ChangeSet::delete(receipt())]);
    assert!(cleanup.level <= ImpactLevel::Low, "{}", cleanup.summary);
}

#[test]
fn f21_f24_unrelated_context_does_not_change_the_verdict() {
    let change = pay().with_description(format!("{PAY_DESC}\n- [ ] Pay with Apple Pay"));
    let with_noise = |n: usize| {
        let mut tree = shop_tree();
        for k in 0..n {
            tree.push(
                Entity::new(format!("U{k}"), "issue")
                    .with_parent("PRJ")
                    .with_title(format!("Unrelated work item {k}"))
                    .with_status(StatusCategory::Unstarted, "Todo")
                    .with_estimate(3.0),
            );
        }
        assess_in(tree, vec![ChangeSet::update(pay(), change.clone())])
    };
    let base = with_noise(0);
    for n in [10, 40] {
        let a = with_noise(n);
        assert_eq!(a.level, base.level, "n = {n}");
        assert!((a.score - base.score).abs() < 1e-9, "n = {n}");
        assert!((a.breadth - base.breadth).abs() < 1e-9, "n = {n}");
    }
    // The overall score never falls below the worst single impact.
    for a in [
        &base,
        &shop_assess(vec![ChangeSet::update(card(), card().with_estimate(8.0))]),
    ] {
        assert!(
            a.score + 1e-12 >= a.severity,
            "{} < {}",
            a.score,
            a.severity
        );
    }
}

#[test]
fn f22_a_project_move_mirrored_in_the_parent_counts_once() {
    let shipping = Entity::new("SHIP", "project")
        .with_title("Shipping")
        .with_status(StatusCategory::Started, "Started");
    let before = pay().with_field(field::PROJECT, FieldValue::reference("PRJ"));
    let after = pay()
        .with_parent("SHIP")
        .with_field(field::PROJECT, FieldValue::reference("SHIP"));
    let mut tree = shop_tree();
    tree.push(shipping);
    let a = assess_in(tree, vec![ChangeSet::update(before, after)]);
    let parent_risk = a.changes[0]
        .fields
        .iter()
        .find(|f| f.field == field::PARENT)
        .expect("parent")
        .risk;
    assert!(
        (a.changes[0].raw_score - parent_risk).abs() < 1e-9,
        "{} vs {parent_risk}",
        a.changes[0].raw_score
    );
    for child in ["CARD", "PP", "RCPT", "SAVE"] {
        let i = impact(&a, child).expect("child listed");
        assert_eq!(i.action, Action::Notify, "{child}");
        assert!(i.level <= ImpactLevel::Low, "{child} at {}", i.score);
    }
}

#[test]
fn f25_a_bulk_reestimate_reaches_the_parent_whatever_its_size() {
    let parent_score = |n: usize| {
        let parent = Entity::new("EP", "issue")
            .with_title("Epic")
            .with_status(StatusCategory::Started, "In Progress");
        let kids: Vec<Entity> = (0..n)
            .map(|k| {
                Entity::new(format!("K{k:03}"), "sub_issue")
                    .with_parent("EP")
                    .with_title(format!("Step {k}"))
                    .with_status(StatusCategory::Started, "In Progress")
                    .with_estimate(3.0)
            })
            .collect();
        let changes = kids
            .iter()
            .map(|k| ChangeSet::update(k.clone(), k.clone().with_estimate(4.0)))
            .collect();
        let a = Engine::default()
            .assess(&AssessmentRequest {
                changes,
                context: vec![parent],
                now: Some(now()),
                options: AssessOptions {
                    top_k: 0,
                    ..AssessOptions::default()
                },
                ..Default::default()
            })
            .expect("valid");
        let ep = impact(&a, "EP").expect("the parent is affected");
        assert_eq!(ep.action, Action::ReEstimate);
        // Siblings compete for the parent once, not once per sibling, and
        // are not asked to re-estimate for it.
        assert!(
            a.impacts
                .iter()
                .filter(|i| i.entity_id != "EP")
                .all(|i| i.score < 0.25 && i.action != Action::ReEstimate),
            "n = {n}"
        );
        ep.score
    };
    let ten = parent_score(10);
    assert!(ten >= 0.25, "{ten}");
    for n in [50, 200] {
        assert!((parent_score(n) - ten).abs() < 1e-6, "n = {n}");
    }
}

#[test]
fn f26_moving_a_date_away_from_a_conflict_is_not_a_new_conflict() {
    let parent = Entity::new("P", "issue")
        .with_title("Parent")
        .with_status(StatusCategory::Started, "In Progress")
        .with_due(day(20));
    let child = Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("Child")
        .with_status(StatusCategory::Started, "In Progress")
        .with_due(day(25));
    let relief = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::update(
            parent.clone(),
            parent.clone().with_due(day(22)),
        )],
    );
    assert!(impact(&relief, "C").is_none_or(|i| i.action != Action::Reschedule && i.score < 0.05));
    let worse = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::update(
            parent.clone(),
            parent.clone().with_due(day(18)),
        )],
    );
    assert_eq!(action(&worse, "C"), Some(Action::Reschedule));
    // The child pulls in, still late but less so.
    let improving = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::update(
            child.clone(),
            child.clone().with_due(day(23)),
        )],
    );
    assert_ne!(action(&improving, "P"), Some(Action::Reschedule));
}

#[test]
fn f28_a_field_magnitude_does_not_depend_on_the_context() {
    let before = "Customer password reset by email for customer accounts with a signed link";
    let after = "Customer password reset by SMS for customer accounts with a signed link";
    let parent = Entity::new("P", "issue")
        .with_title("Accounts")
        .with_description(before)
        .with_version(SemVer::new(1, 0, 0));
    let run = |n: usize| {
        let kids: Vec<Entity> = (0..n)
            .map(|k| {
                Entity::new(format!("K{k}"), "sub_issue")
                    .with_parent("P")
                    .with_title(format!("customer accounts password reset signed link {k}"))
            })
            .collect();
        let mut tree = kids;
        tree.push(parent.clone());
        assess_in(
            tree,
            vec![ChangeSet::update(
                parent.clone(),
                parent.clone().with_description(after),
            )],
        )
    };
    let (bare, crowded) = (run(0), run(60));
    let m = |a: &Assessment| a.changes[0].fields[0].magnitude;
    assert_eq!(m(&bare), m(&crowded));
    assert_eq!(bare.changes[0].bump, crowded.changes[0].bump);
    let direct = Engine::default()
        .compare_text(before, after, None)
        .expect("valid texts");
    assert_eq!(m(&bare), direct.magnitude);
}

#[test]
fn f30_correcting_an_overdue_or_imminent_date_is_not_breaking() {
    let hours = |h: i64| now() + chrono::Duration::hours(h);
    let moved = |from: i64, to: i64| {
        let x = Entity::new("X", "issue")
            .with_title("Task")
            .with_status(StatusCategory::Started, "In Progress")
            .with_due(hours(from));
        assess_in(
            vec![x.clone()],
            vec![ChangeSet::update(x.clone(), x.with_due(hours(to)))],
        )
    };
    for (from, to) in [(-720, -744), (-720, -648), (24, 96)] {
        let a = moved(from, to);
        assert!(a.changes[0].bump <= VersionBump::Minor, "{from} → {to}");
        assert!(!a.changes[0].fields[0].breaking, "{from} → {to}");
        assert!(a.level <= ImpactLevel::Low, "{from} → {to}: {}", a.summary);
    }
    // Pulling a date in hard is still breaking.
    assert_eq!(moved(30 * 24, 2 * 24).changes[0].bump, VersionBump::Major);
}

#[test]
fn f31_activity_counts_once_per_change_set() {
    let comments = |bodies: &[&str]| {
        shop_assess(vec![ChangeSet::activity(
            "CARD",
            bodies
                .iter()
                .map(|b| ActivityEvent::comment("CARD", *b))
                .collect(),
        )])
    };
    let text = "Blocked on the vendor API. Still blocked. Blocked again today.";
    let one = comments(&[text]);
    let three = comments(&[text, text, text]);
    assert_eq!(one.changes[0].local_score, three.changes[0].local_score);
    let chatty = comments(&[
        "Can you take a look?",
        "Is staging up?",
        "Lunch at noon?",
        "Pairing tomorrow?",
        "Want coffee?",
    ]);
    assert!(chatty.level <= ImpactLevel::Low, "{}", chatty.summary);
    let after_negative = comments(&["Not great. Blocked on the vendor API."]);
    assert!(
        after_negative.changes[0]
            .signals
            .iter()
            .any(|s| s.category == assessment_core::assessment::SignalCategory::Blocker)
    );
}

#[test]
fn f32_reassigning_and_moving_cycle_is_never_breaking() {
    let x = Entity::new("X", "issue")
        .with_title("Search")
        .with_status(StatusCategory::Started, "In Progress")
        .with_assignee("u1")
        .with_field(field::CYCLE, FieldValue::reference("c1"))
        .with_version(SemVer::new(1, 4, 0));
    let d = Entity::new("D", "issue")
        .with_title("Dependent")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_basis("X", SemVer::new(1, 4, 0));
    let a = assess_in(
        vec![x.clone(), d],
        vec![ChangeSet::update(
            x.clone(),
            x.with_assignee("u2")
                .with_field(field::CYCLE, FieldValue::reference("c2")),
        )],
    );
    assert!(a.changes[0].raw_score >= 0.55, "noisy-OR would say major");
    assert_eq!(a.changes[0].bump, VersionBump::Minor);
    assert_eq!(a.changes[0].next_version, Some(SemVer::new(1, 5, 0)));
    assert!(impact(&a, "D").is_none_or(|i| i.basis_status < BasisStatus::Incompatible));
}

#[test]
fn f33_pure_additions_to_text_are_minor() {
    for (before, after) in [
        (
            "",
            "Allow users to export invoices as PDF from the billing page.",
        ),
        (
            "Add a login page.",
            "Add a login page. Also show a remember-me checkbox and a password strength meter under the form.",
        ),
    ] {
        let x = Entity::new("X", "issue")
            .with_title("Invoices")
            .with_description(before)
            .with_version(SemVer::new(1, 0, 0));
        let a = assess_in(
            vec![x.clone()],
            vec![ChangeSet::update(x.clone(), x.with_description(after))],
        );
        let f = &a.changes[0].fields[0];
        assert!(!f.breaking && f.additive, "{before:?} → {after:?}: {f:?}");
        assert_eq!(a.changes[0].bump, VersionBump::Minor);
    }
    // A first description (None → text) is an addition too.
    let x = Entity::new("X", "issue")
        .with_title("Invoices")
        .with_version(SemVer::new(1, 0, 0));
    let a = assess_in(
        vec![x.clone()],
        vec![ChangeSet::update(
            x.clone(),
            x.with_description("Export invoices as PDF."),
        )],
    );
    assert_eq!(a.changes[0].bump, VersionBump::Minor);
}

#[test]
fn f47_a_new_blocker_is_not_a_reason_to_cancel() {
    let api = Entity::new("API", "issue")
        .with_parent("PRJ")
        .with_title("Payments webhook endpoint")
        .with_status(StatusCategory::Unstarted, "Todo")
        .with_relation(RelationKind::Blocks, "CONF");
    let a = shop_assess(vec![ChangeSet::create(api)]);
    let conf = impact(&a, "CONF").expect("the blocked page is affected");
    assert_eq!(conf.action, Action::Reschedule);
    assert!(conf.reasons.iter().any(|r| r.contains("was created")));
    assert!(conf.reasons.iter().all(|r| !r.contains("deleted")));
    // A deleted sub-issue without an estimate reads as deleted.
    let a = shop_assess(vec![ChangeSet::delete(Entity {
        fields: paypal()
            .fields
            .into_iter()
            .filter(|(k, _)| k != field::ESTIMATE)
            .collect(),
        ..paypal()
    })]);
    let parent = impact(&a, "PAY").expect("parent");
    assert!(
        parent.reasons.iter().any(|r| r.contains("was deleted")),
        "{:?}",
        parent.reasons
    );
}

#[test]
fn f48_the_replan_root_is_never_a_deleted_or_moved_leaf() {
    // Deleting the in-progress card form: re-plan its parent, not the
    // entity that no longer exists.
    let a = shop_assess(vec![ChangeSet::delete(card())]);
    assert!(
        a.recommendation >= Recommendation::PartialReplan,
        "{}",
        a.summary
    );
    assert_eq!(a.replan_root.as_deref(), Some("PAY"), "{}", a.summary);
    let options = Entity::new("OPT", "issue")
        .with_parent("PRJ")
        .with_title("Shipping options")
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(5.0);
    let mut tree = shop_tree();
    tree.push(options);
    let a = assess_in(
        tree,
        vec![ChangeSet::update(paypal(), paypal().with_parent("OPT"))],
    );
    assert!(
        a.recommendation >= Recommendation::PartialReplan,
        "{}",
        a.summary
    );
    assert_eq!(a.replan_root.as_deref(), Some("PRJ"), "{}", a.summary);
}

#[test]
fn f51_an_estimate_roll_up_weighs_increases_and_decreases_alike() {
    let parent = Entity::new("P", "issue")
        .with_title("Parent")
        .with_status(StatusCategory::Started, "In Progress");
    let kid = |est: f64| {
        Entity::new("K", "sub_issue")
            .with_parent("P")
            .with_title("Changed")
            .with_status(StatusCategory::Started, "In Progress")
            .with_estimate(est)
    };
    let other = Entity::new("O", "sub_issue")
        .with_parent("P")
        .with_title("Other")
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(2.0);
    let up = assess_in(
        vec![parent.clone(), other.clone(), kid(2.0)],
        vec![ChangeSet::update(kid(2.0), kid(8.0))],
    );
    let down = assess_in(
        vec![parent, other, kid(8.0)],
        vec![ChangeSet::update(kid(8.0), kid(2.0))],
    );
    // The same six points of a ten-point effort move either way; the
    // decrease is not more disruptive upstream than the increase.
    assert!(score(&down, "P") <= score(&up, "P") + 1e-9);
}

#[test]
fn f52_removing_a_blocked_label_is_good_news() {
    let x = |labels: &[&str]| {
        Entity::new("X", "issue")
            .with_title("Task")
            .with_status(StatusCategory::Started, "In Progress")
            .with_field(field::LABELS, FieldValue::set(labels.iter().copied()))
    };
    let blocked = assess_in(
        vec![x(&["api"])],
        vec![ChangeSet::update(x(&["api"]), x(&["api", "blocked"]))],
    );
    let unblocked = assess_in(
        vec![x(&["api", "blocked"])],
        vec![ChangeSet::update(x(&["api", "blocked"]), x(&["api"]))],
    );
    assert!(blocked.level >= ImpactLevel::Medium);
    assert!(unblocked.level <= ImpactLevel::Low, "{}", unblocked.summary);
}

#[test]
fn f53_embeddings_never_lower_an_exact_lexical_match() {
    let parent = Entity::new("P", "issue")
        .with_title("Checkout")
        .with_description("Support card payments.\nSupport PayPal express checkout.")
        .with_status(StatusCategory::Started, "In Progress");
    let child = Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("PayPal express checkout")
        .with_status(StatusCategory::Started, "In Progress");
    let after = parent.clone().with_description("Support card payments.");
    let lexical = assess_in(
        vec![parent.clone(), child.clone()],
        vec![ChangeSet::update(parent.clone(), after.clone())],
    );
    let (mut pb, mut pa, mut c) = (parent, after, child);
    pb.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    pa.embedding = Some(vec![0.9, 0.0, 0.0, 0.43]);
    c.embedding = Some(vec![0.0, 1.0, 0.0, 0.05]);
    let semantic = assess_in(vec![pb.clone(), c], vec![ChangeSet::update(pb, pa)]);
    assert!(score(&semantic, "C") + 1e-9 >= score(&lexical, "C") * 0.95);
    assert_eq!(action(&semantic, "C"), Some(Action::ObsoleteCandidate));
}

#[test]
fn f60_a_rename_mirrored_in_title_and_description_counts_once() {
    let parent = Entity::new("P", "issue")
        .with_title("PayPal checkout")
        .with_description("Add PayPal checkout to the cart page.")
        .with_status(StatusCategory::Started, "In Progress");
    let child = Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("PayPal button on cart")
        .with_status(StatusCategory::Started, "In Progress");
    let run = |after: Entity| {
        assess_in(
            vec![parent.clone(), child.clone()],
            vec![ChangeSet::update(parent.clone(), after)],
        )
    };
    let title = run(parent.clone().with_title("Stripe checkout"));
    let both = run(parent
        .clone()
        .with_title("Stripe checkout")
        .with_description("Add Stripe checkout to the cart page."));
    assert!(
        (both.changes[0].raw_score - title.changes[0].raw_score).abs() < 1e-9,
        "{} vs {}",
        both.changes[0].raw_score,
        title.changes[0].raw_score
    );
}

#[test]
fn every_replan_names_a_live_root() {
    let cases = vec![
        shop_assess(vec![ChangeSet::delete(paypal())]),
        shop_assess(vec![ChangeSet::delete(card())]),
        shop_assess(vec![ChangeSet::update(
            pay(),
            pay().with_status(StatusCategory::Canceled, "Canceled"),
        )]),
        shop_assess(vec![ChangeSet::update(
            pay(),
            without_criterion("Pay with PayPal"),
        )]),
        assess(vec![ChangeSet::delete(auth())]),
    ];
    for a in cases {
        if a.recommendation >= Recommendation::PartialReplan {
            let root = a.replan_root.as_deref().expect("a root");
            assert!(
                a.changes
                    .iter()
                    .all(|c| c.change_type != ChangeType::Deleted || c.entity_id != root),
                "{}",
                a.summary
            );
        }
    }
}
