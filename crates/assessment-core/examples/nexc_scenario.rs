//! The scenario the engine was built for: a Linear-style parent issue's
//! description is rewritten while one of its sub-issues is in progress.
//! Which sub-issues does that touch, how much, and do we need to re-plan?
//!
//! ```text
//! cargo run -p assessment-core --example nexc_scenario
//! ```

use assessment_core::prelude::*;
use chrono::{TimeZone, Utc};

fn main() -> Result<(), assessment_core::Error> {
    let now = Utc
        .with_ymd_and_hms(2026, 11, 7, 9, 0, 0)
        .single()
        .expect("a valid date");
    let day = |d: u32| {
        Utc.with_ymd_and_hms(2026, 11, d, 0, 0, 0)
            .single()
            .expect("a valid date")
    };

    let project = Entity::new("PRJ-1", "project")
        .with_title("Customer portal v2")
        .with_status(StatusCategory::Started, "Started")
        .with_version(SemVer::new(1, 4, 0));

    let parent_before = Entity::new("ENG-12", "issue")
        .with_parent("PRJ-1")
        .with_title("Authentication for the customer portal")
        .with_description(
            "Customers sign in to the portal.\n\n\
             ## Acceptance criteria\n\
             - [ ] Sign up and log in with email and password\n\
             - [ ] Password reset by email link\n\
             - [ ] Sessions expire after 30 days\n\
             - [ ] Rate-limit failed logins",
        )
        .with_status(StatusCategory::Started, "In Progress")
        .with_estimate(13.0)
        .with_due(day(28))
        .with_version(SemVer::new(1, 2, 0));

    // The product owner rewrites the description mid-sprint: passwords are
    // out, Google and Microsoft single sign-on are in.
    let parent_after = parent_before.clone().with_description(
        "Customers sign in to the portal with their company identity.\n\n\
         ## Acceptance criteria\n\
         - [ ] Log in with Google Workspace (OAuth 2.0 / OIDC)\n\
         - [ ] Log in with Microsoft Entra ID (OIDC)\n\
         - [ ] Sessions expire after 7 days\n\
         - [ ] Rate-limit failed logins\n\n\
         Password login is no longer needed.",
    );

    let sub =
        |id: &str, title: &str, description: &str, cat: StatusCategory, state: &str, est: f64| {
            Entity::new(id, "sub_issue")
                .with_parent("ENG-12")
                .with_title(title)
                .with_description(description)
                .with_status(cat, state)
                .with_estimate(est)
                .with_basis("ENG-12", SemVer::new(1, 2, 0))
        };
    let context = vec![
        project,
        sub(
            "ENG-13",
            "Email and password sign-up form",
            "Form with email, password and password confirmation; hash passwords with argon2.",
            StatusCategory::Started,
            "In Progress",
            5.0,
        )
        .with_assignee("dorj"),
        sub(
            "ENG-14",
            "Password reset email flow",
            "Send a reset link by email; the token expires after one hour.",
            StatusCategory::Unstarted,
            "Todo",
            3.0,
        ),
        sub(
            "ENG-15",
            "Session expiry and refresh",
            "Sessions expire after 30 days of inactivity; refresh tokens rotate.",
            StatusCategory::Unstarted,
            "Todo",
            2.0,
        ),
        sub(
            "ENG-16",
            "Rate-limit failed logins",
            "Lock an account for 15 minutes after 5 failed login attempts.",
            StatusCategory::Completed,
            "Done",
            2.0,
        ),
        Entity::new("ENG-20", "issue")
            .with_parent("PRJ-1")
            .with_title("Fix footer layout on mobile")
            .with_description("The footer overlaps the cookie banner below 380px.")
            .with_status(StatusCategory::Unstarted, "Todo")
            .with_estimate(1.0),
    ];

    let assessment = Engine::default().assess(&AssessmentRequest {
        changes: vec![ChangeSet::update(parent_before, parent_after)],
        context,
        now: Some(now),
        ..Default::default()
    })?;

    println!("{}\n", assessment.summary);
    println!(
        "score {:.2}  level {:?}  recommendation {:?}  confidence {:.2}",
        assessment.score, assessment.level, assessment.recommendation, assessment.confidence
    );
    println!(
        "severity {:.2}  breadth {:.2}  effort at risk {:.1} of {:.1}  re-plan root {:?}\n",
        assessment.severity,
        assessment.breadth,
        assessment.effort_at_risk,
        assessment.effort_total,
        assessment.replan_root
    );
    for c in &assessment.changes {
        println!(
            "changed {}: {:?} {} → {}  (content {:.2}, local {:.2}{})",
            c.entity_id,
            c.bump,
            c.previous_version
                .map(|v| v.to_string())
                .unwrap_or_default(),
            c.next_version.map(|v| v.to_string()).unwrap_or_default(),
            c.raw_score,
            c.local_score,
            if c.propagation_truncated {
                ", propagation truncated"
            } else {
                ""
            }
        );
        for f in &c.fields {
            println!(
                "  - {} (magnitude {:.2}, risk {:.2}{})",
                f.explanation,
                f.magnitude,
                f.risk,
                if f.breaking { ", breaking" } else { "" }
            );
        }
    }
    println!();
    for i in &assessment.impacts {
        println!(
            "#{} {:<7} {:.2} {:<8?} {:<18?} basis {:?}{}",
            i.rank,
            i.entity_id,
            i.score,
            i.level,
            i.action,
            i.basis_status,
            if i.basis_introduced {
                " (by this change)"
            } else {
                ""
            }
        );
        for r in i.reasons.iter().take(2) {
            println!("      {r}");
        }
    }
    for v in &assessment.violations {
        println!(
            "violation {:?} (introduced: {}): {}",
            v.kind, v.introduced, v.message
        );
    }
    Ok(())
}
