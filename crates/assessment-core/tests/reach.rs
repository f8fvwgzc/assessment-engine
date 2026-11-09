//! What context an assessment reads: only the text of entities propagation
//! can reach is analysed, and `text.idf_scope` decides whether the rest of
//! the context still weighs vocabulary.

mod support;

use assessment_core::prelude::*;
use serde_json::json;

/// `base` plus an unrelated project of `n` issues that nothing links to.
fn with_unrelated(mut base: AssessmentRequest, n: usize) -> AssessmentRequest {
    let other = support::unrelated(n, 200, 100);
    base.context.extend(
        other
            .context
            .into_iter()
            .filter(|e| e.id == "Q0" || e.id.starts_with('U')),
    );
    base
}

fn engine(scope: &str) -> Engine {
    Engine::default()
        .with_policy_patch(&json!({ "text": { "idf_scope": scope } }))
        .expect("a valid policy patch")
}

#[test]
fn with_reach_scope_unrelated_context_changes_nothing() {
    let engine = engine("reach");
    for base in [
        support::siblings(30, 200, 100),
        support::cousins(10, 3, 200, 100),
        support::blocking_chain(6, 200, 100),
    ] {
        let alone = engine.assess(&base).expect("assesses");
        let crowded = engine
            .assess(&with_unrelated(base.clone(), 300))
            .expect("assesses");
        assert_eq!(alone, crowded, "{}", alone.summary);
    }
}

#[test]
fn with_context_scope_unrelated_context_still_weighs_vocabulary() {
    // The default: the whole request is the IDF corpus, so unrelated text
    // that shares words with the change dilutes them (0.1.0 behaviour).
    let engine = Engine::default();
    let base = support::siblings(30, 200, 100);
    let alone = engine.assess(&base).expect("assesses");
    let crowded = engine.assess(&with_unrelated(base, 300)).expect("assesses");
    assert_eq!(alone.level, crowded.level);
    assert_eq!(alone.recommendation, crowded.recommendation);
    assert_ne!(alone.score, crowded.score, "the corpus should differ");
}

#[test]
fn reach_scope_keeps_every_decision_on_the_golden_graphs() {
    let context = Engine::default();
    let reach = engine("reach");
    for req in [
        support::siblings(200, 120, 25),
        support::fan_out(60, 120, 25),
        support::deep_chain(12, 120, 25),
        support::unrelated(100, 120, 25),
        support::blocking_chain(8, 120, 25),
    ] {
        let a = context.assess(&req).expect("assesses");
        let b = reach.assess(&req).expect("assesses");
        assert_eq!(a.level, b.level);
        assert_eq!(a.recommendation, b.recommendation);
        assert_eq!(a.replan_root, b.replan_root);
        assert_eq!(a.violations, b.violations);
        let bumps = |x: &Assessment| x.changes.iter().map(|c| c.bump).collect::<Vec<_>>();
        assert_eq!(bumps(&a), bumps(&b));
        assert!(
            (a.score - b.score).abs() < 0.01,
            "{} vs {}",
            a.score,
            b.score
        );
    }
}

/// The golden `siblings` graph where `X-1` blocks `D`, and `D` and `E` block
/// each other. No propagation channel leads from `X` to `D` or `E` (a child's
/// `blocks` edge is not followed), so both are linked to the change but
/// outside its text reach.
fn linked_beyond_the_walk(d_estimate: f64, d_text: &str) -> AssessmentRequest {
    let mut req = support::siblings(10, 200, 100);
    for e in &mut req.context {
        if e.id == "X-1" {
            *e = e.clone().with_relation(RelationKind::Blocks, "D");
        }
    }
    req.context.push(
        Entity::new("D", "issue")
            .with_title("Quarterly ledger export")
            .with_description(d_text)
            .with_status(StatusCategory::Started, "In Progress")
            .with_estimate(d_estimate)
            .with_relation(RelationKind::Blocks, "E"),
    );
    req.context.push(
        Entity::new("E", "issue")
            .with_title("Ledger archive")
            .with_description("Archive the exported ledgers.")
            .with_status(StatusCategory::Started, "In Progress")
            .with_estimate(2.0)
            .with_relation(RelationKind::Blocks, "D"),
    );
    req
}

#[test]
fn with_reach_scope_linked_context_beyond_the_walk_still_counts() {
    const LEDGER: &str = "Export the quarterly ledger for the billing team.";
    const LOGIN: &str = "Password login session email token reset for the portal.";
    for scope in ["reach", "context"] {
        let engine = engine(scope);
        let small = engine
            .assess(&linked_beyond_the_walk(1.0, LEDGER))
            .expect("assesses");
        let large = engine
            .assess(&linked_beyond_the_walk(100.0, LEDGER))
            .expect("assesses");
        // The walk never visits D: it is not an impact.
        assert!(small.impacts.iter().all(|i| i.entity_id != "D"), "{scope}");
        assert_eq!(small.impacts_total, large.impacts_total, "{scope}");
        // D still depends on the change structurally, so its effort counts
        // toward breadth and the score.
        assert!(
            (large.effort_total - small.effort_total - 99.0).abs() < 1e-9,
            "{scope}: {} vs {}",
            small.effort_total,
            large.effort_total
        );
        assert!(large.breadth < small.breadth, "{scope}");
        assert_ne!(small.score, large.score, "{scope}");
        // The neighbourhood of the change is still checked for violations.
        assert!(
            small
                .violations
                .iter()
                .any(|v| v.kind == ViolationKind::CyclicDependency
                    && v.entities.iter().any(|e| e == "D")),
            "{scope}: {:?}",
            small.violations
        );
    }
    // Only D's text is out of the picture under `reach`: it is neither
    // analysed nor part of the IDF corpus.
    let reach = engine("reach");
    assert_eq!(
        reach
            .assess(&linked_beyond_the_walk(1.0, LEDGER))
            .expect("assesses"),
        reach
            .assess(&linked_beyond_the_walk(1.0, LOGIN))
            .expect("assesses"),
    );
}

#[test]
fn a_parent_reference_field_is_inside_the_reach() {
    // A user field named `parent` of type reference seeds a move, so the
    // walk steps up to the entity it names. Debug builds assert that the
    // walk stays inside the analysed reach.
    let before = Entity::new("S", "issue").with_title("Login page");
    let after = before
        .clone()
        .with_field("parent", FieldValue::reference("X"));
    let x = Entity::new("X", "epic").with_title("Customer portal");
    for scope in ["context", "reach"] {
        let a = engine(scope)
            .assess(&AssessmentRequest {
                changes: vec![ChangeSet::update(before.clone(), after.clone())],
                context: vec![x.clone()],
                now: Some(support::now()),
                ..Default::default()
            })
            .expect("assesses");
        let impact = a
            .impacts
            .iter()
            .find(|i| i.entity_id == "X")
            .unwrap_or_else(|| panic!("{scope}: X is affected"));
        assert_eq!(impact.channel, Channel::Up, "{scope}");
        assert!(
            impact
                .contributions
                .iter()
                .any(|c| c.field == "parent" && c.channel == Channel::Up),
            "{scope}"
        );
    }
}

#[test]
fn an_unchanged_parent_reference_field_does_not_widen_the_reach() {
    // No seed comes from a field that did not change, so the walk never
    // steps to X. Under `reach`, X's children must not weigh vocabulary.
    let before = Entity::new("S", "issue")
        .with_title("Login page")
        .with_description("Users sign in with a password and an email token.")
        .with_field("parent", FieldValue::reference("X"));
    let after = before
        .clone()
        .with_description("Users sign in with a hardware key. Sessions expire daily.");
    let child = Entity::new("S-1", "issue")
        .with_title("Password reset email")
        .with_description("Send the password reset token by email.")
        .with_parent("S");
    let x = Entity::new("X", "epic").with_title("Customer portal");
    let request = |extra: usize| {
        let mut context = vec![x.clone(), child.clone()];
        context.extend((0..extra).map(|i| {
            Entity::new(format!("K{i}"), "issue")
                .with_title("Password email token")
                .with_description("Users sign in with a password and an email token.")
                .with_parent("X")
        }));
        AssessmentRequest {
            changes: vec![ChangeSet::update(before.clone(), after.clone())],
            context,
            now: Some(support::now()),
            ..Default::default()
        }
    };
    let engine = engine("reach");
    let alone = engine.assess(&request(0)).expect("assesses");
    let crowded = engine.assess(&request(40)).expect("assesses");
    assert_eq!(alone, crowded, "{}", alone.summary);
}

// ---------------------------------------------------------------------------
// Random small graphs. In a debug build the propagation walk asserts that
// every node it visits is inside the analysed text reach, so a reach that
// misses a node fails these tests with a panic.
// ---------------------------------------------------------------------------

/// A 64-bit LCG: the same seed gives the same requests on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        let mut rng = Rng(seed ^ 0x9E37_79B9_7F4A_7C15);
        rng.next();
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True `num` times out of `den`.
    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const KINDS: &[&str] = &["project", "epic", "issue", "sub_issue"];

const RELATIONS: &[RelationKind] = &[
    RelationKind::Blocks,
    RelationKind::BlockedBy,
    RelationKind::RelatesTo,
    RelationKind::Duplicates,
];

const STATUSES: &[(StatusCategory, &str)] = &[
    (StatusCategory::Unstarted, "Todo"),
    (StatusCategory::Started, "In Progress"),
    (StatusCategory::Completed, "Done"),
    (StatusCategory::Canceled, "Canceled"),
];

const COMMENTS: &[&str] = &[
    "This is blocked by the vendor, the deadline will slip by two weeks.",
    "Scope change: we must also add password reset and drop the OAuth login.",
    "Out of scope now, the session token work is no longer needed.",
    "Risk: the billing export may not be ready, we are delayed.",
    "Looks good to me.",
];

/// An id of the graph, or (one time in six) an id nothing has.
fn some_id(rng: &mut Rng, n: usize) -> String {
    if rng.chance(1, 6) {
        "GHOST".to_owned()
    } else {
        format!("N{}", rng.below(n))
    }
}

/// A node with random text, status and estimate. Its parent and relations
/// point anywhere: at itself, into a cycle, or at an id nothing has.
fn random_entity(rng: &mut Rng, id: &str, n: usize) -> Entity {
    let (category, name) = *rng.pick(STATUSES);
    let mut e = Entity::new(id, *rng.pick(KINDS))
        .with_title(support::text(rng.next(), 20))
        .with_description(support::text(rng.next(), 60))
        .with_status(category, name);
    if rng.chance(2, 3) {
        e = e.with_estimate((rng.below(8) + 1) as f64);
    }
    if rng.chance(3, 4) {
        e = e.with_parent(some_id(rng, n));
    }
    for _ in 0..rng.below(3) {
        e = e.with_relation(*rng.pick(RELATIONS), some_id(rng, n));
    }
    if rng.chance(1, 8) {
        e = e.with_field("parent", FieldValue::reference(some_id(rng, n)));
    }
    e
}

/// `before` with one to three random edits.
fn random_edit(rng: &mut Rng, before: &Entity, n: usize) -> Entity {
    let mut after = before.clone();
    for _ in 0..=rng.below(3) {
        match rng.below(10) {
            0 => after = after.with_description(support::text(rng.next(), 60)),
            1 => after = after.with_title(support::text(rng.next(), 20)),
            2 => after = after.with_estimate((rng.below(20) + 1) as f64),
            3 => {
                let (category, name) = *rng.pick(STATUSES);
                after = after.with_status(category, name);
            }
            // Re-parenting: to another node, to an unknown id, or to no parent.
            4 if rng.chance(1, 4) => after.parent_id = None,
            4 => after.parent_id = Some(some_id(rng, n)),
            // A user field named `parent`: set, changed, cleared, or of
            // another type.
            5 | 6 => match rng.below(5) {
                0 => {
                    after.fields.remove("parent");
                }
                1 => after = after.with_field("parent", FieldValue::text("see the roadmap")),
                _ => after = after.with_field("parent", FieldValue::reference(some_id(rng, n))),
            },
            7 => after = after.with_relation(*rng.pick(RELATIONS), some_id(rng, n)),
            8 => after.relations.clear(),
            _ => {
                let name = *rng.pick(&["project", "cycle", "team", "custom"]);
                after = after.with_field(name, FieldValue::reference(some_id(rng, n)));
            }
        }
    }
    after
}

fn random_comments(rng: &mut Rng, id: &str) -> Vec<ActivityEvent> {
    (0..=rng.below(2))
        .map(|_| ActivityEvent::comment(id, *rng.pick(COMMENTS)))
        .collect()
}

/// A graph of two to nine nodes and one to three changes of distinct nodes:
/// updates, creations, deletions and activity.
fn random_request(seed: u64) -> AssessmentRequest {
    let mut rng = Rng::new(seed);
    let n = 2 + rng.below(8);
    let nodes: Vec<Entity> = (0..n)
        .map(|i| random_entity(&mut rng, &format!("N{i}"), n))
        .collect();
    let mut order: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        order.swap(i, rng.below(i + 1));
    }
    let changed = &order[..(1 + rng.below(3)).min(n)];
    let mut context: Vec<Entity> = nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| !changed.contains(i))
        .map(|(_, e)| e.clone())
        .collect();
    let mut changes = Vec::new();
    for &i in changed {
        let node = &nodes[i];
        let change = match rng.below(8) {
            0 => ChangeSet::create(node.clone()),
            1 => ChangeSet::delete(node.clone()),
            2 => {
                // Activity on an entity the context has, or on one nobody sent.
                if rng.chance(3, 4) {
                    context.push(node.clone());
                }
                ChangeSet::activity(node.id.clone(), random_comments(&mut rng, &node.id))
            }
            3 => ChangeSet::update(node.clone(), random_edit(&mut rng, node, n))
                .with_events(random_comments(&mut rng, &node.id)),
            _ => ChangeSet::update(node.clone(), random_edit(&mut rng, node, n)),
        };
        changes.push(change);
    }
    AssessmentRequest {
        changes,
        context,
        now: Some(support::now()),
        ..Default::default()
    }
}

fn engine_with(scope: &str, max_depth: u32) -> Engine {
    Engine::default()
        .with_policy_patch(&json!({
            "text": { "idf_scope": scope },
            "propagation": { "max_depth": max_depth },
        }))
        .expect("a valid policy patch")
}

#[test]
fn the_walk_stays_inside_the_reach_on_random_graphs() {
    const DEPTHS: &[u32] = &[0, 1, 2, 3, 4, 7, 16];
    let engines: Vec<(u32, Engine, Engine)> = DEPTHS
        .iter()
        .map(|&d| (d, engine_with("context", d), engine_with("reach", d)))
        .collect();
    let (mut moved, mut field_moved, mut reached) = (0usize, 0usize, 0usize);
    for seed in 0..400u64 {
        let req = random_request(seed);
        for (depth, context, reach) in &engines {
            // A panic here is the walk leaving the analysed reach.
            let a = context
                .assess(&req)
                .unwrap_or_else(|e| panic!("seed {seed}, max_depth {depth}: {e}"));
            let b = reach
                .assess(&req)
                .unwrap_or_else(|e| panic!("seed {seed}, max_depth {depth}: {e}"));
            // The scope only changes how text is weighed, never who changed.
            assert_eq!(a.changes.len(), b.changes.len(), "seed {seed}");
            if *depth == 0 {
                // No hops: only the changed entities themselves are affected.
                for x in [&a, &b] {
                    assert!(
                        x.impacts
                            .iter()
                            .flat_map(|i| &i.contributions)
                            .all(|c| c.channel == Channel::SelfChange),
                        "seed {seed}"
                    );
                }
            } else {
                reached += a.impacts.iter().filter(|i| !i.changed).count();
            }
        }
        for cs in &req.changes {
            if let (Some(b), Some(a)) = (&cs.before, &cs.after) {
                moved += usize::from(b.parent_id != a.parent_id);
                field_moved += usize::from(b.reference("parent") != a.reference("parent"));
            }
        }
    }
    // The generator must keep producing the cases this test is for.
    assert!(moved >= 20, "re-parentings: {moved}");
    assert!(field_moved >= 20, "`parent` field moves: {field_moved}");
    assert!(reached >= 1000, "impacts beyond the changes: {reached}");
}
