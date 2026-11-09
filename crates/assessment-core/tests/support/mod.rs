//! Deterministic requests shared by the golden tests and the `bench` example
//! (`examples/bench.rs` includes this file with `#[path]`).
//!
//! Everything here is a pure function of its arguments: no randomness, no
//! clock, no hash-order dependence, so the same call builds the same request
//! on every platform.

#![allow(dead_code, reason = "each includer uses a different subset")]

use assessment_core::prelude::*;
use chrono::{DateTime, TimeZone, Utc};

/// The reference time of every request built here.
pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 9, 9, 0, 0)
        .single()
        .expect("a valid date")
}

fn day(d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, d, 0, 0, 0)
        .single()
        .expect("a valid date")
}

// ---------------------------------------------------------------------------
// The text changes measured with the 0.1.0 binary on 2026-11-09.
// ---------------------------------------------------------------------------

/// `(name, before, after)` for a parent issue's description.
pub const TEXT_CASES: &[(&str, &str, &str)] = &[
    (
        "sso_negation",
        "The API must support SSO for every customer.",
        "The API must not support SSO for every customer.",
    ),
    (
        "typo",
        "Implment the login flow with email",
        "Implement the login flow with email",
    ),
    (
        "numbers",
        "Support 100 concurrent users",
        "Support 10000 concurrent users",
    ),
    (
        "should_must",
        "The page should load in 2 seconds",
        "The page must load in 2 seconds",
    ),
    (
        "reorder",
        "- [ ] Login\n- [ ] Logout\n- [ ] Reset password",
        "- [ ] Reset password\n- [ ] Login\n- [ ] Logout",
    ),
    (
        "removed_item",
        "- [ ] Login\n- [ ] Logout\n- [ ] Reset password",
        "- [ ] Login\n- [ ] Logout",
    ),
    (
        "paraphrase",
        "Users can log in with their email address and password.",
        "People are able to sign in using e-mail and a password.",
    ),
    (
        "postgres_mongo",
        "Store user sessions in PostgreSQL with a 30 day expiry.",
        "Store user sessions in MongoDB with a 30 day expiry.",
    ),
    (
        "login_logout",
        "Build the login button",
        "Build the logout button",
    ),
    (
        "flexible_fixed",
        "The launch date is flexible and can move if needed.",
        "The launch date is fixed and can move if needed.",
    ),
    (
        "flexible_cannot",
        "The launch date is flexible and can move if needed.",
        "The launch date is fixed and cannot move if needed.",
    ),
    (
        "mongolian_negation",
        "Хэрэглэгч имэйл болон нууц үгээр нэвтэрч чадна.",
        "Хэрэглэгч имэйл болон нууц үгээр нэвтэрч чадахгүй.",
    ),
    (
        "except_paypal",
        "Support all payment methods at checkout.",
        "Support all payment methods at checkout except PayPal.",
    ),
    (
        "http_https",
        "Call the API over http",
        "Call the API over https",
    ),
];

/// A parent issue `P` (in progress, `1.0.0`) whose description changes from
/// `before` to `after`, and an in-progress child `C` that was planned against
/// `P@1.0.0` and whose description is the parent's old text (so it owns it).
pub fn text_case(before: &str, after: &str) -> AssessmentRequest {
    let parent = |description: &str| {
        Entity::new("P", "issue")
            .with_title("Parent issue")
            .with_description(description)
            .with_status(StatusCategory::Started, "In Progress")
            .with_version(SemVer::new(1, 0, 0))
    };
    let child = Entity::new("C", "sub_issue")
        .with_parent("P")
        .with_title("Child task")
        .with_description(before)
        .with_status(StatusCategory::Started, "In Progress")
        .with_basis("P", SemVer::new(1, 0, 0));
    AssessmentRequest {
        changes: vec![ChangeSet::update(parent(before), parent(after))],
        context: vec![child],
        now: Some(now()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Synthetic graphs.
// ---------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "login", "session", "password", "email", "reset", "token", "oauth", "google", "account",
    "portal", "billing", "invoice", "footer", "layout", "cache", "search", "export", "import",
    "report", "filter", "upload", "avatar", "profile", "settings", "webhook", "retry", "queue",
    "audit", "metrics", "alert", "checkout", "payment", "refund", "mobile", "desktop", "theme",
];

/// A deterministic text of at least `len` bytes: sentences of eight words
/// drawn by a 64-bit LCG seeded with `seed`.
pub fn text(seed: u64, len: usize) -> String {
    let mut x = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut out = String::with_capacity(len + 16);
    let mut words = 0usize;
    while out.len() < len {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let word = WORDS[((x >> 33) % WORDS.len() as u64) as usize];
        if words > 0 {
            out.push(if words.is_multiple_of(8) { '.' } else { ' ' });
            if words.is_multiple_of(8) {
                out.push(' ');
            }
        }
        out.push_str(word);
        words += 1;
    }
    out.push('.');
    out
}

const STATUSES: [(StatusCategory, &str); 4] = [
    (StatusCategory::Unstarted, "Todo"),
    (StatusCategory::Started, "In Progress"),
    (StatusCategory::Backlog, "Backlog"),
    (StatusCategory::Completed, "Done"),
];

/// A work item with generated text, a status and an estimate.
pub fn item(id: &str, kind: &str, parent: Option<&str>, seed: u64, len: usize) -> Entity {
    let (category, name) = STATUSES[(seed % STATUSES.len() as u64) as usize];
    let mut e = Entity::new(id, kind)
        .with_title(text(seed.wrapping_add(7_919), 40))
        .with_description(text(seed, len))
        .with_status(category, name)
        .with_estimate(((seed % 5) + 1) as f64)
        .with_version(SemVer::new(1, 0, 0));
    if let Some(p) = parent {
        e = e.with_parent(p);
    }
    e
}

const X_BEFORE: &str = "Customers sign in to the portal.\n\n## Acceptance criteria\n\
- [ ] Sign up and log in with email and password\n\
- [ ] Password reset by email link\n\
- [ ] Sessions expire after 30 days\n\
- [ ] Rate-limit failed logins";

const X_AFTER: &str = "Customers sign in with their company identity.\n\n## Acceptance criteria\n\
- [ ] Log in with Google Workspace (OAuth 2.0 / OIDC)\n\
- [ ] Sessions expire after 7 days\n\
- [ ] Rate-limit failed logins\n\n\
Password login is no longer needed.";

/// The changed issue `X` (in project `P0`), before and after its rewrite.
fn changed(project: &str) -> (Entity, Entity) {
    let x = |description: &str| {
        Entity::new("X", "issue")
            .with_parent(project)
            .with_title("Authentication for the customer portal")
            .with_description(description)
            .with_status(StatusCategory::Started, "In Progress")
            .with_estimate(13.0)
            .with_due(day(28))
            .with_version(SemVer::new(1, 2, 0))
    };
    (x(X_BEFORE), x(X_AFTER))
}

/// `X`'s own sub-issues, each about one acceptance criterion, pinned to `X@1.2.0`.
fn x_children() -> Vec<Entity> {
    let subs = [
        (
            "X-1",
            "Email and password sign-up form",
            StatusCategory::Started,
        ),
        (
            "X-2",
            "Password reset email flow",
            StatusCategory::Unstarted,
        ),
        (
            "X-3",
            "Session expiry and refresh",
            StatusCategory::Unstarted,
        ),
        (
            "X-4",
            "Rate limiting for failed logins",
            StatusCategory::Completed,
        ),
    ];
    subs.iter()
        .enumerate()
        .map(|(i, (id, title, category))| {
            Entity::new(*id, "sub_issue")
                .with_parent("X")
                .with_title(*title)
                .with_description(text(900 + i as u64, 120))
                .with_status(*category, "Status")
                .with_estimate(3.0)
                .with_basis("X", SemVer::new(1, 2, 0))
        })
        .collect()
}

fn project(id: &str) -> Entity {
    Entity::new(id, "project")
        .with_title(format!("Project {id}"))
        .with_status(StatusCategory::Started, "Started")
        .with_version(SemVer::new(1, 0, 0))
}

fn request(context: Vec<Entity>, top_k: usize) -> AssessmentRequest {
    let (before, after) = changed("P0");
    request_for(before, after, context, top_k)
}

fn request_for(
    before: Entity,
    after: Entity,
    context: Vec<Entity>,
    top_k: usize,
) -> AssessmentRequest {
    AssessmentRequest {
        changes: vec![ChangeSet::update(before, after)],
        context,
        now: Some(now()),
        options: AssessOptions {
            top_k,
            ..AssessOptions::default()
        },
        ..Default::default()
    }
}

fn base() -> Vec<Entity> {
    let mut context = vec![project("P0")];
    context.extend(x_children());
    context
}

/// `n` sibling issues of `X` in the same project, without sub-issues.
pub fn siblings(n: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let mut context = base();
    context.extend((0..n).map(|i| item(&format!("S{i}"), "issue", Some("P0"), i as u64, len)));
    request(context, top_k)
}

/// `issues` sibling issues with `subs` sub-issues each: the cousins are
/// inside the registry's gather but out of propagation's reach.
pub fn cousins(issues: usize, subs: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let mut context = base();
    for i in 0..issues {
        let id = format!("S{i}");
        context.push(item(&id, "issue", Some("P0"), i as u64, len));
        for s in 0..subs {
            let seed = (i * 31 + s) as u64 + 10_000;
            context.push(item(
                &format!("S{i}-{s}"),
                "sub_issue",
                Some(&id),
                seed,
                len,
            ));
        }
    }
    request(context, top_k)
}

/// `n` descendants of `X` in a tree with fan-out 4: everything is reached.
pub fn fan_out(n: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let mut context = base();
    let mut ids: Vec<String> = vec!["X".to_owned()];
    for i in 0..n {
        let parent = ids[i / 4].clone();
        let id = format!("F{i}");
        context.push(item(
            &id,
            "sub_issue",
            Some(&parent),
            i as u64 + 20_000,
            len,
        ));
        ids.push(id);
    }
    request(context, top_k)
}

/// A chain of `depth` descendants under `X`'s first sub-issue: only the first
/// few links are within propagation's reach.
pub fn deep_chain(depth: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let mut context = base();
    let mut parent = "X-1".to_owned();
    for i in 0..depth {
        let id = format!("D{i}");
        context.push(item(
            &id,
            "sub_issue",
            Some(&parent),
            i as u64 + 30_000,
            len,
        ));
        parent = id;
    }
    request(context, top_k)
}

/// `n` issues in another project that nothing links to `X`.
pub fn unrelated(n: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let mut context = base();
    context.push(project("Q0"));
    context.extend((0..n).map(|i| {
        item(
            &format!("U{i}"),
            "issue",
            Some("Q0"),
            i as u64 + 40_000,
            len,
        )
    }));
    request(context, top_k)
}

/// `X` blocks `B0`, which blocks `B1`, and so on: a transitive dependency chain.
pub fn blocking_chain(n: usize, len: usize, top_k: usize) -> AssessmentRequest {
    let (before, after) = changed("P0");
    let mut context = base();
    for i in 0..n {
        let mut e = item(
            &format!("B{i}"),
            "issue",
            Some("P0"),
            i as u64 + 50_000,
            len,
        );
        if i + 1 < n {
            e = e.with_relation(RelationKind::Blocks, format!("B{}", i + 1));
        }
        context.push(e);
    }
    request_for(
        before.with_relation(RelationKind::Blocks, "B0"),
        after.with_relation(RelationKind::Blocks, "B0"),
        context,
        top_k,
    )
}

/// Every entity of a request's context plus the changed entities' `before`
/// snapshots, parents before children (then by id): the order a registry
/// can be loaded in.
pub fn load_order(req: &AssessmentRequest) -> Vec<Entity> {
    let all: Vec<Entity> = req
        .changes
        .iter()
        .filter_map(|c| c.before.clone())
        .chain(req.context.iter().cloned())
        .collect();
    let parent_of: std::collections::HashMap<&str, Option<&str>> = all
        .iter()
        .map(|e| (e.id.as_str(), e.parent_id.as_deref()))
        .collect();
    let depth = |e: &Entity| {
        let mut d = 0usize;
        let mut p = e.parent_id.as_deref();
        while let Some(pid) = p {
            d += 1;
            if d > 64 {
                break;
            }
            p = parent_of.get(pid).copied().flatten();
        }
        d
    };
    let mut keyed: Vec<(usize, &Entity)> = all.iter().map(|e| (depth(e), e)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    keyed.into_iter().map(|(_, e)| e.clone()).collect()
}
