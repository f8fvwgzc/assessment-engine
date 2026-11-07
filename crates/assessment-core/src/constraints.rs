//! Plan-consistency checks: facts about the graph that, when a change makes
//! them true, demand action whatever the scores say — work left under a
//! canceled parent, a child now due after its parent, a blocker now due
//! after what it blocks, children whose estimates overflow the parent's.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::assessment::ViolationKind;
use crate::graph::Graph;
use crate::model::{EntityId, StatusCategory};
use crate::policy::ViolationPolicy;

/// A violation found in one graph.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Found {
    pub kind: ViolationKind,
    /// The entities involved; the first one is the one that needs action
    /// (for a blocking cycle, the members in id order).
    pub entities: Vec<EntityId>,
    pub severity: f64,
    pub message: String,
    /// How bad it is, in the violation's own unit (days late, points over;
    /// 0 for yes/no violations), to tell a worsened violation from a relieved one.
    pub amount: f64,
}

impl Found {
    /// Identity for "was this already there before the change". An estimate
    /// overflow is the parent's, whichever children make it up: adding,
    /// canceling or deleting a child does not make it a new one.
    pub(crate) fn key(&self) -> (ViolationKind, Vec<EntityId>) {
        let ids = match self.kind {
            ViolationKind::EstimateOverflow => self.entities.iter().take(1).cloned().collect(),
            _ => {
                let mut ids = self.entities.clone();
                ids.sort();
                ids
            }
        };
        (self.kind, ids)
    }
}

fn days(d: chrono::TimeDelta) -> f64 {
    d.num_seconds() as f64 / 86_400.0
}

fn closed_or_deleted(g: &Graph, i: usize) -> bool {
    g.deleted[i] || !g.nodes[i].is_active()
}

fn active(g: &Graph, i: usize) -> bool {
    !g.deleted[i] && g.nodes[i].is_active()
}

fn date(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Every violation among the entities in `scope`.
pub(crate) fn check(
    g: &Graph,
    scope: &BTreeSet<usize>,
    policy: &ViolationPolicy,
    now: DateTime<Utc>,
) -> Vec<Found> {
    let mut out = Vec::new();
    for &i in scope {
        let node = &g.nodes[i];
        if g.deleted[i] {
            continue;
        }
        let label = node.label();

        if let Some(p) = g.parent[i] {
            let parent = &g.nodes[p];
            if active(g, i) {
                if g.deleted[p] || parent.status_category() == Some(StatusCategory::Canceled) {
                    out.push(Found {
                        kind: ViolationKind::OrphanedWork,
                        entities: vec![node.id.clone(), parent.id.clone()],
                        severity: policy.orphaned_work,
                        message: format!(
                            "{label} is still active but its parent {} was {}",
                            parent.label(),
                            if g.deleted[p] { "deleted" } else { "canceled" }
                        ),
                        amount: 0.0,
                    });
                } else if parent.status_category() == Some(StatusCategory::Completed) {
                    out.push(Found {
                        kind: ViolationKind::PrematureClose,
                        entities: vec![parent.id.clone(), node.id.clone()],
                        severity: policy.premature_close,
                        message: format!(
                            "parent {} is completed while {label} is still active",
                            parent.label()
                        ),
                        amount: 0.0,
                    });
                }
                if !g.deleted[p]
                    && let (Some(cd), Some(pd)) = (node.due(), parent.due())
                    && cd > pd
                {
                    out.push(Found {
                        kind: ViolationKind::ScheduleConflict,
                        entities: vec![node.id.clone(), parent.id.clone()],
                        severity: policy.schedule_conflict,
                        message: format!(
                            "{label} is due {} but its parent {} is due {}",
                            date(cd),
                            parent.label(),
                            date(pd)
                        ),
                        amount: days(cd - pd),
                    });
                }
            }
        }

        if active(g, i) {
            for &b in &g.blocked_by[i] {
                let blocker = &g.nodes[b];
                if closed_or_deleted(g, b) {
                    if g.deleted[b] || blocker.status_category() == Some(StatusCategory::Canceled) {
                        out.push(Found {
                            kind: ViolationKind::BlockedByClosed,
                            entities: vec![node.id.clone(), blocker.id.clone()],
                            severity: policy.blocked_by_closed,
                            message: format!(
                                "{label} is blocked by {}, which was {}",
                                blocker.label(),
                                if g.deleted[b] { "deleted" } else { "canceled" }
                            ),
                            amount: 0.0,
                        });
                    }
                    continue;
                }
                if let Some(bd) = blocker.due() {
                    let late_for_due = node.due().is_some_and(|d| bd > d);
                    let late_for_start = node.start().is_some_and(|s| bd > s);
                    let lateness = [node.due(), node.start()]
                        .into_iter()
                        .flatten()
                        .map(|d| days(bd - d))
                        .fold(0.0, f64::max);
                    if late_for_due || late_for_start {
                        out.push(Found {
                            kind: ViolationKind::DependencyConflict,
                            entities: vec![blocker.id.clone(), node.id.clone()],
                            severity: policy.dependency_conflict,
                            message: format!(
                                "blocker {} is due {}, after {label} {} {}",
                                blocker.label(),
                                date(bd),
                                if late_for_due { "is due" } else { "starts" },
                                date(if late_for_due {
                                    node.due().unwrap_or(bd)
                                } else {
                                    node.start().unwrap_or(bd)
                                })
                            ),
                            amount: lateness,
                        });
                    }
                }
            }
            if let Some(d) = node.due()
                && d < now
            {
                out.push(Found {
                    kind: ViolationKind::Overdue,
                    entities: vec![node.id.clone()],
                    severity: policy.overdue,
                    message: format!("{label} was due {} and is still active", date(d)),
                    amount: days(now - d),
                });
            }
        }

        if let Some(own) = node.estimate() {
            let kids: Vec<usize> = g.children[i]
                .iter()
                .copied()
                .filter(|&c| {
                    !g.deleted[c] && g.nodes[c].status_category() != Some(StatusCategory::Canceled)
                })
                .collect();
            let estimates: Vec<f64> = kids.iter().filter_map(|&c| g.nodes[c].estimate()).collect();
            let sum: f64 = estimates.iter().sum();
            let allowed = own * (1.0 + policy.estimate_tolerance);
            if !estimates.is_empty() && sum > allowed + 1e-9 {
                let mut entities = vec![node.id.clone()];
                entities.extend(kids.iter().map(|&c| g.nodes[c].id.clone()));
                out.push(Found {
                    kind: ViolationKind::EstimateOverflow,
                    entities,
                    severity: policy.estimate_overflow,
                    message: format!(
                        "children of {label} are estimated at {sum} in total, above its own {own}"
                    ),
                    amount: sum - allowed,
                });
            }
        }
    }

    for cycle in g.blocking_cycles(scope) {
        let ids: Vec<EntityId> = cycle.iter().map(|&i| g.nodes[i].id.clone()).collect();
        let labels: Vec<String> = cycle.iter().map(|&i| g.nodes[i].label()).collect();
        out.push(Found {
            kind: ViolationKind::CyclicDependency,
            entities: ids,
            severity: policy.cyclic_dependency,
            message: format!("these block each other in a cycle: {}", labels.join(" → ")),
            amount: 0.0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entity, RelationKind};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
            .single()
            .expect("valid")
    }

    fn day(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, 0, 0, 0)
            .single()
            .expect("valid")
    }

    fn kinds(found: &[Found]) -> Vec<ViolationKind> {
        found.iter().map(|f| f.kind).collect()
    }

    #[test]
    fn orphaned_and_schedule() {
        let g = Graph::build(vec![
            (
                Entity::new("p", "issue")
                    .with_status(StatusCategory::Canceled, "Canceled")
                    .with_due(day(10)),
                false,
            ),
            (
                Entity::new("c", "sub_issue")
                    .with_parent("p")
                    .with_status(StatusCategory::Started, "In Progress")
                    .with_due(day(20)),
                false,
            ),
        ]);
        let scope = (0..g.len()).collect();
        let found = check(&g, &scope, &ViolationPolicy::default(), now());
        assert!(kinds(&found).contains(&ViolationKind::OrphanedWork));
        assert!(kinds(&found).contains(&ViolationKind::ScheduleConflict));
        assert_eq!(found[0].entities[0], "c", "the child needs the action");
    }

    #[test]
    fn dependency_overflow_overdue_cycle() {
        let g = Graph::build(vec![
            (
                Entity::new("blocker", "issue")
                    .with_due(day(15))
                    .with_relation(RelationKind::Blocks, "blocked"),
                false,
            ),
            (
                Entity::new("blocked", "issue")
                    .with_due(day(12))
                    .with_relation(RelationKind::Blocks, "blocker"),
                false,
            ),
            (Entity::new("parent", "issue").with_estimate(5.0), false),
            (
                Entity::new("k1", "issue")
                    .with_parent("parent")
                    .with_estimate(3.0),
                false,
            ),
            (
                Entity::new("k2", "issue")
                    .with_parent("parent")
                    .with_estimate(4.0),
                false,
            ),
            (
                Entity::new("late", "issue").with_due(
                    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
                        .single()
                        .expect("valid"),
                ),
                false,
            ),
        ]);
        let scope = (0..g.len()).collect();
        let found = check(&g, &scope, &ViolationPolicy::default(), now());
        let k = kinds(&found);
        assert!(k.contains(&ViolationKind::DependencyConflict));
        assert!(k.contains(&ViolationKind::EstimateOverflow));
        assert!(k.contains(&ViolationKind::Overdue));
        assert!(k.contains(&ViolationKind::CyclicDependency));
    }

    #[test]
    fn estimate_overflow_is_the_parents_whatever_its_children() {
        let build = |kids: &[(&str, f64, StatusCategory)]| {
            let mut entities = vec![(Entity::new("p", "issue").with_estimate(10.0), false)];
            for (id, est, cat) in kids {
                entities.push((
                    Entity::new(*id, "issue")
                        .with_parent("p")
                        .with_estimate(*est)
                        .with_status(*cat, "x"),
                    false,
                ));
            }
            Graph::build(entities)
        };
        let overflow = |g: &Graph| {
            let scope = (0..g.len()).collect();
            check(g, &scope, &ViolationPolicy::default(), now())
                .into_iter()
                .find(|f| f.kind == ViolationKind::EstimateOverflow)
        };
        let started = StatusCategory::Started;
        let before = overflow(&build(&[
            ("a", 6.0, started),
            ("b", 6.0, started),
            ("c", 3.0, started),
        ]))
        .expect("15 > 11");
        // Canceling `c` shrinks the overflow (15 → 12) and keeps its identity.
        let after = overflow(&build(&[
            ("a", 6.0, started),
            ("b", 6.0, started),
            ("c", 3.0, StatusCategory::Canceled),
        ]))
        .expect("12 > 11");
        assert_eq!(before.key(), after.key());
        assert!(after.amount < before.amount);
        assert_eq!(before.entities[0], "p", "the parent needs the action");
    }

    #[test]
    fn deleted_parent_orphans_and_closed_blocker() {
        let g = Graph::build(vec![
            (Entity::new("p", "issue"), true),
            (Entity::new("c", "issue").with_parent("p"), false),
            (
                Entity::new("b", "issue")
                    .with_status(StatusCategory::Canceled, "Canceled")
                    .with_relation(RelationKind::Blocks, "c"),
                false,
            ),
        ]);
        let scope = (0..g.len()).collect();
        let k = kinds(&check(&g, &scope, &ViolationPolicy::default(), now()));
        assert!(k.contains(&ViolationKind::OrphanedWork));
        assert!(k.contains(&ViolationKind::BlockedByClosed));
    }
}
