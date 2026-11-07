//! Invariants that must hold for any input.

use std::collections::BTreeSet;

use assessment_core::prelude::*;
use assessment_core::stats::{amplify, noisy_or};
use assessment_core::text::compare;
use chrono::{TimeZone, Utc};
use proptest::prelude::*;

const WORDS: &[&str] = &[
    "login",
    "password",
    "reset",
    "email",
    "oauth",
    "google",
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
    "нууц",
    "үг",
    "дууссан",
    "blocked",
    "scope",
    "?",
    "-",
    "[ ]",
    "[x]",
    "#",
    "\n",
    "\n- [ ] ",
    ".",
    "teh",
];

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(WORDS), 0..40).prop_map(|w| w.join(" "))
}

fn status() -> impl Strategy<Value = StatusCategory> {
    prop::sample::select(StatusCategory::ALL.to_vec())
}

fn value() -> impl Strategy<Value = Option<FieldValue>> {
    prop_oneof![
        Just(None),
        text().prop_map(|t| Some(FieldValue::Text(t))),
        (-50.0f64..200.0).prop_map(|n| Some(FieldValue::Number(n))),
        status().prop_map(|c| Some(FieldValue::status(c, "x"))),
        (0u32..5, 1u32..5).prop_map(|(r, s)| Some(FieldValue::ordinal(r.min(s), s))),
        (0i64..90).prop_map(|d| Some(FieldValue::Timestamp(
            Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
                .single()
                .expect("valid")
                + chrono::Duration::days(d)
        ))),
        prop::sample::select(vec!["u1", "u2", "u3"]).prop_map(|r| Some(FieldValue::reference(r))),
        prop::collection::btree_set(prop::sample::select(vec!["a", "b", "security", "ui"]), 0..4)
            .prop_map(|s| Some(FieldValue::Set(s.into_iter().map(str::to_owned).collect()))),
        any::<bool>().prop_map(|b| Some(FieldValue::Bool(b))),
    ]
}

const FIELDS: &[&str] = &[
    "title",
    "description",
    "estimate",
    "status",
    "priority",
    "due_date",
    "assignee",
    "labels",
    "custom",
];

fn entity(id: String, parent: Option<String>) -> impl Strategy<Value = Entity> {
    prop::collection::vec((prop::sample::select(FIELDS), value()), 0..6).prop_map(move |fs| {
        let mut e = Entity::new(id.clone(), "issue");
        e.parent_id = parent.clone();
        for (name, v) in fs {
            if let Some(v) = v {
                e.fields.insert(name.to_owned(), v);
            }
        }
        e
    })
}

fn request() -> impl Strategy<Value = AssessmentRequest> {
    // A random forest of up to 12 entities with random blocking relations;
    // one of them changes randomly.
    (2usize..12)
        .prop_flat_map(|n| {
            let parents = prop::collection::vec(prop::option::of(0usize..n), n);
            let blocks = prop::collection::vec((0usize..n, 0usize..n), 0..6);
            (Just(n), parents, blocks, 0usize..n)
        })
        .prop_flat_map(|(n, parents, blocks, changed)| {
            let ents: Vec<_> = (0..n)
                .map(|i| {
                    let parent = parents[i].filter(|p| *p != i).map(|p| format!("e{p}"));
                    entity(format!("e{i}"), parent)
                })
                .collect();
            let changed_after = entity(
                format!("e{changed}"),
                parents[changed]
                    .filter(|p| *p != changed)
                    .map(|p| format!("e{p}")),
            );
            (ents, Just(blocks), Just(changed), changed_after)
        })
        .prop_map(|(mut ents, blocks, changed, after)| {
            for (a, b) in blocks {
                if a != b {
                    let target = format!("e{b}");
                    ents[a].relations.push(assessment_core::model::Relation {
                        kind: RelationKind::Blocks,
                        target,
                    });
                }
            }
            let mut after = after;
            after.relations = ents[changed].relations.clone();
            let before = ents.remove(changed);
            AssessmentRequest {
                changes: vec![ChangeSet::update(before, after)],
                context: ents,
                now: Some(
                    Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
                        .single()
                        .expect("valid"),
                ),
                ..Default::default()
            }
        })
}

fn unit(x: f64) -> bool {
    x.is_finite() && (0.0..=1.0).contains(&x)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn text_magnitude_is_bounded_and_zero_on_identity(a in text(), b in text()) {
        let p = Policy::default().text;
        let d = compare(&a, &b, &p, None, None);
        prop_assert!(unit(d.magnitude), "{}", d.magnitude);
        prop_assert!(unit(d.lexical_similarity));
        prop_assert!(unit(d.tfidf_cosine));
        prop_assert!(compare(&a, &a, &p, None, None).magnitude < 1e-9);
    }

    #[test]
    fn text_magnitude_is_roughly_symmetric(a in text(), b in text()) {
        let p = Policy::default().text;
        let ab = compare(&a, &b, &p, None, None).magnitude;
        let ba = compare(&b, &a, &p, None, None).magnitude;
        prop_assert!((ab - ba).abs() < 0.2, "{ab} vs {ba}");
    }

    #[test]
    fn noisy_or_is_monotone(xs in prop::collection::vec(0.0f64..1.0, 0..8), extra in 0.0f64..1.0) {
        let base = noisy_or(xs.iter().copied());
        let more = noisy_or(xs.iter().copied().chain([extra]));
        prop_assert!(more + 1e-12 >= base);
        prop_assert!(unit(base));
    }

    #[test]
    fn amplify_is_monotone_in_both_arguments(s in 0.0f64..1.0, t in 0.0f64..1.0, g in 0.05f64..5.0, h in 0.05f64..5.0) {
        let (lo, hi) = if s <= t { (s, t) } else { (t, s) };
        prop_assert!(amplify(lo, g) <= amplify(hi, g) + 1e-12);
        let (gl, gh) = if g <= h { (g, h) } else { (h, g) };
        prop_assert!(amplify(s, gl) <= amplify(s, gh) + 1e-12);
    }

    #[test]
    fn assessments_are_well_formed(req in request()) {
        let engine = Engine::default();
        let a = engine.assess(&req).expect("generated requests are valid");
        prop_assert!(unit(a.score) && unit(a.confidence) && unit(a.severity) && unit(a.breadth));
        prop_assert!(a.effort_at_risk <= a.effort_total + 1e-9);
        // Breadth only ever raises the worst single impact.
        prop_assert!(a.score + 1e-12 >= a.severity, "{} < {}", a.score, a.severity);
        // A re-plan names what to re-plan, and never an entity just deleted.
        if a.recommendation >= Recommendation::PartialReplan {
            let root = a.replan_root.as_deref();
            prop_assert!(root.is_some(), "{}", a.summary);
            prop_assert!(a.changes.iter().all(|c| c.change_type != ChangeType::Deleted || Some(c.entity_id.as_str()) != root));
        }
        let mut seen = BTreeSet::new();
        for (n, i) in a.impacts.iter().enumerate() {
            prop_assert!(unit(i.score), "{} {}", i.entity_id, i.score);
            prop_assert_eq!(i.rank as usize, n + 1);
            prop_assert!(seen.insert(i.entity_id.clone()), "duplicate impact {}", i.entity_id);
            for c in &i.contributions {
                prop_assert!(unit(c.value));
                prop_assert_eq!(c.path.last(), Some(&i.entity_id));
            }
        }
        prop_assert!(a.impacts.windows(2).all(|w| w[0].score >= w[1].score));
        for c in &a.changes {
            prop_assert!(unit(c.raw_score) && unit(c.local_score));
            for f in &c.fields {
                prop_assert!(unit(f.magnitude) && unit(f.risk) && f.risk <= f.severity + 1e-12);
            }
            let breaking = c.fields.iter().any(|f| f.breaking && f.field != "activity");
            if breaking { prop_assert_eq!(c.bump, VersionBump::Major); }
            if c.fields.is_empty() { prop_assert_eq!(c.bump, VersionBump::None); }
            // Logistics alone never break anyone's plan.
            let logistics = c.fields.iter().all(|f| matches!(f.field.as_str(), "assignee" | "labels" | "activity"));
            if logistics { prop_assert!(c.bump <= VersionBump::Minor, "{:?}", c.fields); }
        }
        // Deterministic.
        prop_assert_eq!(&engine.assess(&req).expect("valid"), &a);
    }

    #[test]
    fn the_order_of_the_changes_does_not_matter(req in request(), title in text()) {
        // A second change, on another entity of the forest.
        let mut req = req;
        let Some(other) = req.context.first().cloned() else { return Ok(()) };
        req.context.remove(0);
        let after = other.clone().with_title(title).with_estimate(other.estimate().unwrap_or(1.0) + 2.0);
        req.changes.push(ChangeSet::update(other, after));
        let a = Engine::default().assess(&req).expect("valid");
        req.changes.reverse();
        let b = Engine::default().assess(&req).expect("valid");
        prop_assert_eq!(&a.impacts, &b.impacts);
        prop_assert_eq!(&a.violations, &b.violations);
        prop_assert_eq!((a.score, a.level, a.recommendation, &a.replan_root), (b.score, b.level, b.recommendation, &b.replan_root));
    }

    #[test]
    fn unchanged_entity_scores_nothing(req in request()) {
        let mut req = req;
        let before = req.changes[0].before.clone().expect("update");
        req.changes[0].after = Some(before);
        let a = Engine::default().assess(&req).expect("valid");
        prop_assert_eq!(a.changes[0].bump, VersionBump::None);
        prop_assert!(a.changes[0].raw_score == 0.0);
        prop_assert!(a.impacts.iter().filter(|i| !i.changed).all(|i| i.score < 1e-9 || a.violations.iter().any(|v| v.entities.contains(&i.entity_id))));
    }
}
