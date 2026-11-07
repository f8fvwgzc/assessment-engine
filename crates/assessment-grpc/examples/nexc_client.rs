//! Maps Linear / nexc-style issues into core entities and asks a running
//! assessment engine: *the parent issue's description is rewritten while a
//! sub-issue is in progress — what does that touch, and do we re-plan?*
//!
//! ```text
//! cargo run -p assessment-server -- serve               # in one terminal
//! cargo run -p assessment-grpc --example nexc_client -- http://127.0.0.1:50061
//! ```
//!
//! Set `ASSESSMENT_AUTH_TOKEN` when the server requires a bearer token.

use std::error::Error;

use assessment_core::model::{AssessmentRequest, ChangeSet, Entity, FieldValue, StatusCategory};
use assessment_core::semver::SemVer;
use assessment_grpc::client::AssessmentClient;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};

/// A workflow state as Linear reports it: a team-specific name and a type.
#[derive(Debug, Clone)]
struct WorkflowState {
    name: &'static str,
    /// `triage`, `backlog`, `unstarted`, `started`, `completed` or `canceled`.
    kind: &'static str,
}

/// A Linear / nexc issue, as the tracker's API returns it.
#[derive(Debug, Clone)]
struct Issue {
    /// e.g. `ENG-12`.
    identifier: &'static str,
    title: &'static str,
    /// Markdown.
    description: &'static str,
    state: WorkflowState,
    /// 0 = none, 1 = urgent, 2 = high, 3 = medium, 4 = low.
    priority: u8,
    estimate: Option<f64>,
    assignee: Option<&'static str>,
    due_date: Option<NaiveDate>,
    parent: Option<&'static str>,
    labels: &'static [&'static str],
    /// The version the caller tracks for this issue, if any.
    version: Option<SemVer>,
    /// The parent version this issue was planned against, if tracked.
    planned_against: Option<SemVer>,
}

/// Linear's workflow-state type as a status category (triage counts as backlog).
fn status_category(kind: &str) -> StatusCategory {
    match kind {
        "started" => StatusCategory::Started,
        "completed" => StatusCategory::Completed,
        "canceled" | "cancelled" => StatusCategory::Canceled,
        "unstarted" => StatusCategory::Unstarted,
        _ => StatusCategory::Backlog,
    }
}

/// Linear priorities count down (1 = urgent … 4 = low, 0 = none); the
/// engine's ordinal counts up (higher = more important): urgent → 4,
/// high → 3, medium → 2, low → 1, none → 0, on a scale of 4.
fn priority_rank(priority: u8) -> u32 {
    match priority {
        1 => 4,
        2 => 3,
        3 => 2,
        4 => 1,
        _ => 0,
    }
}

/// A due date (a calendar day) as the end of that day, UTC.
fn due_instant(day: NaiveDate) -> Option<DateTime<Utc>> {
    day.and_hms_opt(23, 59, 59)
        .map(|t| Utc.from_utc_datetime(&t))
}

/// An issue as a core entity, with the well-known field names. Issues with
/// a parent are `sub_issue`s, the others `issue`s.
fn to_entity(issue: &Issue) -> Entity {
    let kind = if issue.parent.is_some() {
        "sub_issue"
    } else {
        "issue"
    };
    let mut entity = Entity::new(issue.identifier, kind)
        .with_title(issue.title)
        .with_description(issue.description)
        .with_status(status_category(issue.state.kind), issue.state.name)
        .with_field(
            "priority",
            FieldValue::ordinal(priority_rank(issue.priority), 4),
        );
    if let Some(estimate) = issue.estimate {
        entity = entity.with_estimate(estimate);
    }
    if let Some(assignee) = issue.assignee {
        entity = entity.with_assignee(assignee);
    }
    if let Some(due) = issue.due_date.and_then(due_instant) {
        entity = entity.with_due(due);
    }
    if !issue.labels.is_empty() {
        entity = entity.with_field("labels", FieldValue::set(issue.labels.iter().copied()));
    }
    if let Some(parent) = issue.parent {
        entity = entity.with_parent(parent);
        if let Some(pinned) = issue.planned_against {
            entity = entity.with_basis(parent, pinned);
        }
    }
    if let Some(version) = issue.version {
        entity = entity.with_version(version);
    }
    entity
}

fn day(d: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(2026, 11, d)
}

fn sub_issue(
    identifier: &'static str,
    title: &'static str,
    description: &'static str,
    state: WorkflowState,
    estimate: f64,
    assignee: Option<&'static str>,
) -> Issue {
    Issue {
        identifier,
        title,
        description,
        state,
        priority: 3,
        estimate: Some(estimate),
        assignee,
        due_date: day(21),
        parent: Some("ENG-12"),
        labels: &["auth"],
        version: None,
        planned_against: Some(SemVer::new(1, 2, 0)),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:50061".to_owned());
    let mut client = AssessmentClient::connect(url).await?;
    if let Ok(token) = std::env::var("ASSESSMENT_AUTH_TOKEN") {
        client = client.with_bearer_token(&token)?;
    }

    let parent = Issue {
        identifier: "ENG-12",
        title: "Authentication for the customer portal",
        description: "Customers sign in to the portal.\n\n\
             ## Acceptance criteria\n\
             - [ ] Sign up and log in with email and password\n\
             - [ ] Password reset by email link\n\
             - [ ] Sessions expire after 30 days\n\
             - [ ] Rate-limit failed logins",
        state: WorkflowState {
            name: "In Progress",
            kind: "started",
        },
        priority: 2,
        estimate: Some(13.0),
        assignee: Some("saraa"),
        due_date: day(28),
        parent: None,
        labels: &["auth", "portal"],
        version: Some(SemVer::new(1, 2, 0)),
        planned_against: None,
    };
    // Mid-sprint, the product owner rewrites the description: passwords are
    // out, company single sign-on is in.
    let rewritten = Issue {
        description: "Customers sign in to the portal with their company identity.\n\n\
             ## Acceptance criteria\n\
             - [ ] Log in with Google Workspace (OAuth 2.0 / OIDC)\n\
             - [ ] Log in with Microsoft Entra ID (OIDC)\n\
             - [ ] Sessions expire after 7 days\n\
             - [ ] Rate-limit failed logins\n\n\
             Password login is no longer needed.",
        ..parent.clone()
    };
    let sub_issues = [
        sub_issue(
            "ENG-13",
            "Email and password sign-up form",
            "Form with email, password and password confirmation; hash passwords with argon2.",
            WorkflowState {
                name: "In Progress",
                kind: "started",
            },
            5.0,
            Some("dorj"),
        ),
        sub_issue(
            "ENG-14",
            "Password reset email flow",
            "Send a reset link by email; the token expires after one hour.",
            WorkflowState {
                name: "Todo",
                kind: "unstarted",
            },
            3.0,
            None,
        ),
        sub_issue(
            "ENG-15",
            "Session expiry and refresh",
            "Sessions expire after 30 days of inactivity; refresh tokens rotate.",
            WorkflowState {
                name: "Todo",
                kind: "unstarted",
            },
            2.0,
            None,
        ),
        sub_issue(
            "ENG-16",
            "Rate-limit failed logins",
            "Lock an account for 15 minutes after 5 failed login attempts.",
            WorkflowState {
                name: "Done",
                kind: "completed",
            },
            2.0,
            Some("bat"),
        ),
    ];

    let request = AssessmentRequest {
        changes: vec![ChangeSet::update(to_entity(&parent), to_entity(&rewritten))],
        context: sub_issues.iter().map(to_entity).collect(),
        now: Utc.with_ymd_and_hms(2026, 11, 7, 9, 0, 0).single(),
        ..Default::default()
    };
    let assessment = client.assess(request).await?;

    println!("{}\n", assessment.summary);
    println!(
        "recommendation {:?} (score {:.2}, level {:?}, confidence {:.2}, re-plan root {})",
        assessment.recommendation,
        assessment.score,
        assessment.level,
        assessment.confidence,
        assessment.replan_root.as_deref().unwrap_or("-"),
    );
    for change in &assessment.changes {
        println!(
            "{}: {:?} bump, {} → {}",
            change.entity_id,
            change.bump,
            change
                .previous_version
                .map_or_else(|| "-".to_owned(), |v| v.to_string()),
            change
                .next_version
                .map_or_else(|| "-".to_owned(), |v| v.to_string()),
        );
    }
    println!(
        "\nimpacts ({} of {}):",
        assessment.impacts.len(),
        assessment.impacts_total
    );
    for impact in &assessment.impacts {
        println!(
            "  #{:<2} {:<7} {:<9} score {:.2}  {:<8} {:<18} basis {:?}",
            impact.rank,
            impact.entity_id,
            impact.kind.as_str(),
            impact.score,
            impact.level.as_str(),
            impact.action.as_str(),
            impact.basis_status,
        );
        if let Some(reason) = impact.reasons.first() {
            println!("       {reason}");
        }
    }
    for violation in &assessment.violations {
        println!("violation {:?}: {}", violation.kind, violation.message);
    }
    Ok(())
}
