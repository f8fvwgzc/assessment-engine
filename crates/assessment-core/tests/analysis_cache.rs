//! The registry's analysis cache never changes a result: a registry that
//! keeps the term counts of its entities answers every operation exactly as
//! one that analyses every text again, under both `text.idf_scope` values.

mod support;

use assessment_core::prelude::*;
use assessment_core::registry::{DeleteOptions, ImportItem, ImportOptions};
use serde::Serialize;
use serde_json::json;

/// A 64-bit LCG: the same seed gives the same graph on every platform.
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
];

const COMMENTS: &[&str] = &[
    "This is blocked by the vendor, the deadline will slip by two weeks.",
    "Scope change: we must also add password reset and drop the OAuth login.",
    "Looks good to me.",
];

fn id(i: usize) -> String {
    format!("N{i:02}")
}

/// A node with random text. Its parent is mostly an earlier node, so the
/// graphs are trees with some cycles, self-links and dangling ids.
fn random_entity(rng: &mut Rng, i: usize, n: usize) -> Entity {
    let (category, name) = *rng.pick(STATUSES);
    let mut e = Entity::new(id(i), *rng.pick(KINDS))
        .with_description(support::text(rng.next(), 20 + rng.below(120)))
        .with_status(category, name);
    // Some entities have no title, and some have only a title.
    if rng.chance(5, 6) {
        e = e.with_title(support::text(rng.next(), 20));
    }
    if rng.chance(1, 8) {
        e = e.with_description("");
    }
    if rng.chance(2, 3) {
        e = e.with_estimate((rng.below(8) + 1) as f64);
    }
    if i > 0 && rng.chance(5, 6) {
        e = e.with_parent(if rng.chance(1, 10) {
            id(rng.below(n))
        } else {
            id(rng.below(i))
        });
    }
    for _ in 0..rng.below(3) {
        e = e.with_relation(*rng.pick(RELATIONS), id(rng.below(n + 1)));
    }
    e
}

/// `before` with one or two random edits; most touch the text.
fn random_edit(rng: &mut Rng, before: &Entity, n: usize) -> Entity {
    let mut after = before.clone();
    after.version = None;
    for _ in 0..=rng.below(2) {
        match rng.below(8) {
            0..=2 => after = after.with_description(support::text(rng.next(), 20 + rng.below(120))),
            3 => after = after.with_title(support::text(rng.next(), 20)),
            4 => after = after.with_estimate((rng.below(20) + 1) as f64),
            5 => {
                let (category, name) = *rng.pick(STATUSES);
                after = after.with_status(category, name);
            }
            6 => after.parent_id = Some(id(rng.below(n))),
            _ => after = after.with_relation(*rng.pick(RELATIONS), id(rng.below(n))),
        }
    }
    after
}

/// A result as text: floats print every bit, so equal text is an equal result.
fn shown<T: Serialize>(result: Result<T, Error>) -> String {
    match result {
        Ok(value) => serde_json::to_string(&value).expect("the result serializes"),
        Err(e) => format!("error: {e}"),
    }
}

fn engine(scope: &str) -> Engine {
    Engine::default()
        .with_policy_patch(&json!({ "text": { "idf_scope": scope } }))
        .expect("a valid policy patch")
}

fn put_opts() -> PutOptions {
    PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    }
}

/// Runs one random history on a registry and returns what every operation
/// answered, and what the registry holds at the end.
fn history(seed: u64, scope: &str, cache: bool) -> Vec<String> {
    let mut rng = Rng::new(seed);
    let registry = Registry::new(engine(scope)).with_analysis_cache(cache);
    assert_eq!(registry.analysis_cache(), cache);
    let n = 4 + rng.below(20);
    let nodes: Vec<Entity> = (0..n).map(|i| random_entity(&mut rng, i, n)).collect();
    let mut log = Vec::new();

    // Half the graph arrives as an import (its analysis is made on first
    // use), the rest put by put (analysed as it is stored).
    let half = n / 2;
    let import = ImportOptions {
        now: Some(support::now()),
        ..ImportOptions::default()
    };
    log.push(shown(registry.import_with(
        nodes[..half].iter().cloned().map(ImportItem::upsert),
        &import,
    )));
    for e in &nodes[half..] {
        log.push(shown(registry.put(e.clone(), put_opts())));
    }

    for _ in 0..30 {
        let target = id(rng.below(n));
        let live = registry.get(&target, None).ok();
        let line = match (rng.below(12), live) {
            (0..=5, Some(current)) => {
                let after = random_edit(&mut rng, &current, n);
                let opts = PutOptions {
                    dry_run: rng.chance(1, 6),
                    ..put_opts()
                };
                shown(registry.put(after, opts))
            }
            (0..=5, None) => {
                // Deleted before: it comes back with new text.
                let like = rng.below(n);
                let e = random_entity(&mut rng, like, n);
                shown(registry.put(Entity { id: target, ..e }, put_opts()))
            }
            (6, Some(current)) => {
                // The same content again: nothing to assess.
                shown(registry.put(current, put_opts()))
            }
            (7, _) => {
                let opts = DeleteOptions {
                    dry_run: rng.chance(1, 2),
                    now: Some(support::now()),
                    ..DeleteOptions::default()
                };
                shown(registry.delete_with(&target, opts))
            }
            (8 | 9, _) => {
                let comment = ActivityEvent::comment(target.clone(), *rng.pick(COMMENTS));
                shown(registry.record_activity(&target, vec![comment], Some(support::now())))
            }
            (10, _) => {
                let versions = registry.history(&target, 0).unwrap_or_default();
                match (versions.first(), versions.last()) {
                    (Some(a), Some(b)) => {
                        shown(registry.compare(&target, a.version, b.version, Some(support::now())))
                    }
                    _ => "no history".to_owned(),
                }
            }
            (_, Some(current)) => {
                // A changed entity through an import: assessed like a put.
                let after = random_edit(&mut rng, &current, n);
                shown(registry.import_with([ImportItem::upsert(after)], &import))
            }
            (_, None) => "not stored".to_owned(),
        };
        log.push(line);
    }

    for i in 0..n {
        log.push(shown(registry.get(&id(i), None)));
        log.push(shown(registry.history(&id(i), 0)));
    }
    log.push(shown(registry.stale(None)));
    log
}

#[test]
fn a_registry_answers_the_same_with_and_without_the_analysis_cache() {
    for scope in ["context", "reach"] {
        for seed in 0..60 {
            let with = history(seed, scope, true);
            let without = history(seed, scope, false);
            assert_eq!(with.len(), without.len(), "seed {seed}, {scope}");
            for (step, (a, b)) in with.iter().zip(&without).enumerate() {
                assert_eq!(a, b, "seed {seed}, idf_scope {scope}, step {step}");
            }
        }
    }
}

/// The histories above must exercise the engine, not only fail early.
#[test]
fn the_random_histories_assess_real_changes() {
    let mut assessed = 0usize;
    let mut impacts = 0usize;
    for seed in 0..120 {
        for line in history(seed, "context", true) {
            if line.contains("\"assessment\"") && line.contains("\"stored\":true") {
                assessed += 1;
            }
            if line.contains("\"impacts\":[{") {
                impacts += 1;
            }
        }
    }
    assert!(assessed > 1_000, "{assessed} stored puts were assessed");
    assert!(impacts > 500, "{impacts} answers carried impacts");
}

/// The cache on the golden graphs: a put that rewrites the changed issue on
/// a stored graph, again under both scopes, and after a reload by import.
#[test]
fn the_golden_graphs_assess_the_same_through_a_cached_registry() {
    for scope in ["context", "reach"] {
        for req in [
            support::siblings(40, 200, 100),
            support::cousins(12, 3, 200, 100),
            support::fan_out(60, 200, 100),
            support::deep_chain(30, 200, 100),
            support::blocking_chain(12, 200, 100),
        ] {
            let order = support::load_order(&req);
            let after = req.changes[0].after.clone().expect("an after");
            let before = req.changes[0].before.clone().expect("a before");
            let answers = |cache: bool, imported: bool| {
                let registry = Registry::new(engine(scope)).with_analysis_cache(cache);
                if imported {
                    registry
                        .import(order.iter().cloned().map(ImportItem::upsert))
                        .expect("the graph imports");
                } else {
                    for e in &order {
                        registry.put(e.clone(), put_opts()).expect("it stores");
                    }
                }
                // Twice each way: the second round reads only kept analysis.
                [&after, &before, &after, &before]
                    .map(|e| shown(registry.put(e.clone(), put_opts())))
            };
            for imported in [false, true] {
                assert_eq!(
                    answers(true, imported),
                    answers(false, imported),
                    "idf_scope {scope}, imported {imported}"
                );
            }
        }
    }
}
