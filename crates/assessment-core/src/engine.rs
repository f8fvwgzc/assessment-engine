//! The engine: from a request to an [`Assessment`].
//!
//! 1. **Resolve** each change set into its snapshots (validated).
//! 2. **Score fields** of each changed entity (magnitude × severity = risk)
//!    and combine them with noisy-OR into a content score that drives the
//!    **SemVer bump** (each field capped at its policy's `max_bump`).
//! 3. **Modulate** by context — the phase of the work the change disturbs
//!    (in-progress and done work is costlier to disturb), deadline
//!    proximity, churn — with `1 - (1 - s)^γ`.
//! 4. **Propagate** every field change through the graph ([`crate::propagate`]),
//!    each within its own share of the step budget.
//! 5. **Check** plan consistency before and after; a violation the change
//!    introduces sets a floor under the entity that needs action.
//! 6. **Aggregate**: severity (worst impact) raised by breadth (how much of
//!    the rest of the affected effort is touched) into the re-plan score,
//!    level and recommendation; rank the affected entities and choose an
//!    action for each.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::assessment::{
    Action, Assessment, BasisStatus, ChangeAssessment, ChangeType, Channel, Contribution,
    EntityImpact, FieldAssessment, FieldChangeKind, ImpactLevel, Modifier, Recommendation, Signal,
    SignalCategory, Violation, ViolationKind,
};
use crate::constraints;
use crate::error::{Error, Result};
use crate::fields::{self, Channels, Detail, FieldContext, LifecycleKind, Scored, Transition};
use crate::graph::Graph;
use crate::model::{
    ActivityEvent, AssessmentRequest, ChangeSet, Entity, EntityId, FieldValue, HistoryPoint,
    StatusCategory, field,
};
use crate::policy::Policy;
use crate::propagate::{self, Hit, Propagator, Seed};
use crate::semver::{Compatibility, SemVer, VersionBump};
use crate::signals;
use crate::stats::{amplify, clamp01, decayed_sum, escalate, noisy_or, saturate, z_score};
use crate::text::{self, Corpus, SparseVec, TextDelta, TextDiff};

/// Largest number of change sets one request may carry.
pub const MAX_CHANGES: usize = 10_000;
/// Largest number of context entities one request may carry.
pub const MAX_CONTEXT: usize = 200_000;
/// Largest text field, in bytes (also: one change set's activity text, and
/// each text given to [`Engine::compare_text`]).
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// Largest embedding dimension.
pub const MAX_EMBEDDING_DIM: usize = 16_384;
/// Largest number of activity events one change set may carry.
pub const MAX_EVENTS: usize = 1_000;
/// Largest number of field changes one change set may carry.
pub const MAX_FIELD_CHANGES: usize = 1_000;
/// Largest number of history points one request may carry.
pub const MAX_HISTORY: usize = 100_000;
/// Largest number of fields, of relations, of basis pins, and of items in a
/// set field, one entity may carry.
pub const MAX_ITEMS: usize = 10_000;
/// Largest absolute value of a number field (estimates and the like), so
/// sums over a whole request stay finite.
pub const MAX_NUMBER: f64 = 1e12;

/// An introduced violation floors the entity that needs action at this
/// share of its severity (and the overall score likewise).
const VIOLATION_FLOOR: f64 = 0.9;

/// The assessment engine. Cheap to clone (the policy is shared behind an
/// [`Arc`]); immutable and thread-safe.
#[derive(Debug, Clone, Default)]
pub struct Engine {
    policy: Arc<Policy>,
}

/// What a change set's snapshots are, by kind of change. The variants make
/// the snapshot each kind needs a type, not a runtime promise.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "one per change set, built once and never moved in a hot loop"
)]
enum Snapshots {
    Created {
        after: Entity,
    },
    Deleted {
        before: Entity,
    },
    Updated {
        before: Entity,
        after: Entity,
    },
    /// Only activity: the entity as it is (from the context, or a placeholder).
    Activity {
        entity: Entity,
    },
}

/// One change set, resolved.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub id: EntityId,
    snapshots: Snapshots,
    pub events: Vec<ActivityEvent>,
    /// The entity was not in the request at all (activity on an unknown entity).
    pub placeholder: bool,
}

impl Resolved {
    fn change_type(&self) -> ChangeType {
        match self.snapshots {
            Snapshots::Created { .. } => ChangeType::Created,
            Snapshots::Deleted { .. } => ChangeType::Deleted,
            Snapshots::Updated { .. } => ChangeType::Updated,
            Snapshots::Activity { .. } => ChangeType::Activity,
        }
    }

    /// The state the graph holds for this entity.
    fn current(&self) -> &Entity {
        match &self.snapshots {
            Snapshots::Created { after } | Snapshots::Updated { after, .. } => after,
            Snapshots::Deleted { before } => before,
            Snapshots::Activity { entity } => entity,
        }
    }

    fn before(&self) -> Option<&Entity> {
        match &self.snapshots {
            Snapshots::Created { .. } => None,
            Snapshots::Deleted { before } | Snapshots::Updated { before, .. } => Some(before),
            Snapshots::Activity { entity } => Some(entity),
        }
    }
}

/// Per-change scoring results.
struct Scoring {
    fields: Vec<Scored>,
    signals: Vec<Signal>,
    raw: f64,
    local: f64,
    modifiers: Vec<Modifier>,
    bump: VersionBump,
    previous: Option<SemVer>,
    next: Option<SemVer>,
    anomaly_z: Option<f64>,
    confidence: f64,
}

/// A changed entity's pins on the changed entities: the worst status, and
/// the worst status this request made worse.
#[derive(Debug, Clone, Copy)]
struct Basis {
    status: BasisStatus,
    introduced: BasisStatus,
}

impl Engine {
    /// An engine with the given policy (validated).
    pub fn new(policy: Policy) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            policy: Arc::new(policy),
        })
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// An engine whose policy is this one's with a partial JSON patch applied.
    pub fn with_policy_patch(&self, patch: &serde_json::Value) -> Result<Self> {
        Ok(Self {
            policy: Arc::new(self.policy.merged(patch)?),
        })
    }

    /// Compares two texts with this engine's text policy, exactly as a text
    /// field's magnitude is measured in [`Engine::assess`]. The texts are
    /// limited to [`MAX_TEXT_BYTES`]; embeddings to [`MAX_EMBEDDING_DIM`]
    /// finite components (embeddings of different dimensions are ignored).
    pub fn compare_text(
        &self,
        before: &str,
        after: &str,
        embeddings: Option<(&[f32], &[f32])>,
    ) -> Result<TextDiff> {
        check_text("the `before` text", before)?;
        check_text("the `after` text", after)?;
        if let Some((a, b)) = embeddings {
            check_embedding("the `before` embedding", a)?;
            check_embedding("the `after` embedding", b)?;
        }
        Ok(text::compare(
            before,
            after,
            &self.policy.text,
            None,
            embeddings,
        ))
    }

    /// Assesses the request's changes together.
    pub fn assess(&self, request: &AssessmentRequest) -> Result<Assessment> {
        let now = request.now.unwrap_or_else(Utc::now);
        self.assess_at(request, now)
    }

    fn assess_at(&self, request: &AssessmentRequest, now: DateTime<Utc>) -> Result<Assessment> {
        let policy: &Policy = &self.policy;
        let pp = &policy.propagation;
        check_request(request)?;

        // ---- 1. resolve -------------------------------------------------
        let mut context: BTreeMap<EntityId, &Entity> = BTreeMap::new();
        for e in &request.context {
            validate_entity(e)?;
            if context.insert(e.id.clone(), e).is_some() {
                return Err(Error::invalid(format!(
                    "entity `{}` appears twice in the context",
                    e.id
                )));
            }
        }
        let mut resolved: Vec<Resolved> = Vec::with_capacity(request.changes.len());
        let mut seen_changes = BTreeSet::new();
        for cs in &request.changes {
            let r = resolve(cs, &context)?;
            if !seen_changes.insert(r.id.clone()) {
                return Err(Error::invalid(format!(
                    "entity `{}` is changed twice in one request; merge its change sets",
                    r.id
                )));
            }
            resolved.push(r);
        }
        let mut history: HashMap<&str, Vec<&HistoryPoint>> = HashMap::new();
        for h in &request.history {
            if !(h.magnitude.is_finite() && (0.0..=1.0).contains(&h.magnitude)) {
                return Err(Error::invalid(format!(
                    "history of `{}` has a magnitude outside [0, 1]",
                    h.entity_id
                )));
            }
            history.entry(h.entity_id.as_str()).or_default().push(h);
        }
        let changed_ids: BTreeMap<&str, usize> = resolved
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.as_str(), i))
            .collect();

        // ---- graphs ------------------------------------------------------
        let (graph, before_graph) = build_graphs(&context, &resolved, &changed_ids);
        let changed_nodes: Vec<usize> = resolved.iter().filter_map(|r| graph.get(&r.id)).collect();

        // ---- corpus & document vectors --------------------------------------
        let mut corpus = Corpus::new();
        let mut analyses = Vec::with_capacity(graph.len());
        for node in &graph.nodes {
            let a = text::analyze(&node.document(), &policy.text);
            corpus.add_terms(a.counts.keys());
            analyses.push(a);
        }
        for r in &resolved {
            if let Snapshots::Updated { before, .. } = &r.snapshots {
                let a = text::analyze(&before.document(), &policy.text);
                corpus.add_terms(a.counts.keys());
            }
        }
        let docs: Vec<SparseVec> = analyses
            .iter()
            .map(|a| text::tfidf(&a.counts, &corpus))
            .collect();
        let titles: Vec<SparseVec> = graph
            .nodes
            .iter()
            .map(|n| match n.title().trim() {
                "" => SparseVec::new(),
                t => text::tfidf(&text::analyze(t, &policy.text).counts, &corpus),
            })
            .collect();

        // ---- 2–3. score each change ---------------------------------------------
        let scorings: Vec<Scoring> = resolved
            .iter()
            .map(|r| {
                let points = history.get(r.id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
                self.score_change(r, &corpus, points, now)
            })
            .collect();

        // ---- 4. propagate ---------------------------------------------------------
        // Every seed walks within its own budget, a share of `max_steps` in
        // proportion to its risk: deterministic, and independent of the
        // order of the changes in the request.
        let propagator = Propagator::new(&graph, policy, &docs, &titles);
        let mut seeds: Vec<(usize, usize, usize, f64)> = Vec::new();
        for (ci, (r, s)) in resolved.iter().zip(&scorings).enumerate() {
            let Some(source) = graph.get(&r.id) else {
                continue;
            };
            for (fi, f) in s.fields.iter().enumerate() {
                let risk = f.assessment.severity * f.weight * f.assessment.magnitude;
                if risk > 0.0 && risk.is_finite() {
                    seeds.push((ci, fi, source, risk));
                }
            }
        }
        let total_risk: f64 = seeds.iter().map(|s| s.3).sum();
        let floor = pp.min_seed_steps.min(pp.max_steps).max(1);
        let mut truncated = vec![false; resolved.len()];
        let mut hits: Vec<Hit> = Vec::new();
        for &(ci, fi, source, risk) in &seeds {
            let f = &scorings[ci].fields[fi];
            let share = if total_risk > 0.0 {
                risk / total_risk
            } else {
                0.0
            };
            let seed = Seed {
                source,
                field: &f.assessment.field,
                severity: f.assessment.severity * f.weight,
                magnitude: f.assessment.magnitude,
                channels: f.channels,
                detail: &f.detail,
                source_estimate: resolved[ci].before().and_then(Entity::estimate),
                budget: ((f64::from(pp.max_steps) * share) as u32).max(floor),
            };
            if propagator.run(&seed, &mut hits) {
                truncated[ci] = true;
            }
        }

        // ---- combine per entity -------------------------------------------------------
        let mut by_target: BTreeMap<usize, Vec<Hit>> = BTreeMap::new();
        for h in hits {
            by_target.entry(h.target).or_default().push(h);
        }
        let mut scores: BTreeMap<usize, f64> = BTreeMap::new();
        for (i, r) in resolved.iter().enumerate() {
            if let Some(n) = graph.get(&r.id) {
                scores.insert(n, scorings[i].local);
            }
        }
        let mut rollup_reasons: BTreeMap<usize, String> = BTreeMap::new();
        for (&target, list) in &by_target {
            let node = &graph.nodes[target];
            let phase = self.phase_factor(node);
            let pressure = self.time_pressure(node, now).0;
            // Strongest contribution per (source, field): one piece of evidence, counted once.
            let mut strongest: BTreeMap<(usize, &str), &Hit> = BTreeMap::new();
            for h in list {
                let e = strongest.entry((h.source, h.field.as_str())).or_insert(h);
                if h.value > e.value {
                    *e = h;
                }
            }
            // The completed-phase boost means rework: it applies only to
            // evidence that changes what was done, not to removed text (the
            // work is moot, not redone) or text that barely concerns it.
            let gamma = phase * pressure;
            let rework_free = phase.min(1.0) * pressure;
            let mut evidence: Vec<(f64, f64)> = Vec::new();
            let mut rollups: BTreeMap<usize, Vec<(&Hit, propagate::Rollup)>> = BTreeMap::new();
            for h in strongest.values() {
                match h.rollup {
                    Some(r) => rollups.entry(r.on).or_default().push((h, r)),
                    None => evidence.push((
                        h.value,
                        if is_rework(h, policy) {
                            gamma
                        } else {
                            rework_free
                        },
                    )),
                }
            }
            for (on, group) in &rollups {
                if group.len() < 2 {
                    evidence.extend(group.iter().map(|(h, _)| (h.value, gamma)));
                    continue;
                }
                // Effort moved by several changes at once is one roll-up: the
                // share of the effort they move together (so fifty small
                // re-estimates weigh what one large one would, and siblings
                // compete for the parent once, not once per sibling).
                let (own, kids) = propagator.effort_parts(*on);
                let moved: f64 = group.iter().map(|(_, r)| r.delta).sum();
                let moved_abs: f64 = group.iter().map(|(_, r)| r.delta.abs()).sum();
                let base = propagate::rollup_base(own, kids, moved);
                let strongest_single = group.iter().map(|(h, _)| h.value).fold(0.0, f64::max);
                let combined = if base > 0.0 && moved_abs > 0.0 {
                    let share = (moved_abs / base).min(1.0);
                    let factor = group
                        .iter()
                        .map(|(_, r)| r.factor * r.delta.abs())
                        .sum::<f64>()
                        / moved_abs;
                    if *on == target {
                        rollup_reasons.insert(
                            target,
                            format!(
                                "{} changes under it moved {:.0}% of its effort together",
                                group.len(),
                                share * 100.0
                            ),
                        );
                    }
                    clamp01(factor * saturate(share, policy.number.rollup_scale))
                } else {
                    0.0
                };
                evidence.push((combined.max(strongest_single), gamma));
            }
            let incoming = noisy_or(evidence.iter().map(|&(v, g)| amplify(v, g)));
            let own = scores.get(&target).copied().unwrap_or(0.0);
            scores.insert(target, noisy_or([own, incoming]));
        }

        // ---- 5. violations ----------------------------------------------------------------
        let scope = graph.neighbourhood(&changed_nodes, pp.max_depth.max(1));
        let before_scope: BTreeSet<usize> = scope
            .iter()
            .filter_map(|&i| before_graph.get(&graph.nodes[i].id))
            .collect();
        let before_found: BTreeMap<_, f64> =
            constraints::check(&before_graph, &before_scope, &policy.violations, now)
                .into_iter()
                .map(|f| (f.key(), f.amount))
                .collect();
        let violations: Vec<Violation> =
            constraints::check(&graph, &scope, &policy.violations, now)
                .into_iter()
                .map(|f| {
                    let before = before_found.get(&f.key()).copied();
                    let mut message = f.message;
                    if before.is_some_and(|b| f.amount > b + 1e-9) {
                        message.push_str("; it was there before and got worse");
                    }
                    Violation {
                        kind: f.kind,
                        entities: f.entities,
                        severity: f.severity,
                        introduced: before.is_none(),
                        message,
                    }
                })
                .collect();
        // Only the entity that needs action carries an introduced
        // violation's floor and action; the others are merely involved.
        let mut primary_of: BTreeMap<usize, Vec<&Violation>> = BTreeMap::new();
        let mut involved_in: BTreeMap<usize, Vec<&Violation>> = BTreeMap::new();
        for v in violations.iter().filter(|v| v.introduced) {
            let primary = primary_entity(v, &changed_ids);
            for id in &v.entities {
                let Some(i) = graph.get(id) else { continue };
                if graph.deleted[i] {
                    continue;
                }
                if Some(id.as_str()) == primary {
                    primary_of.entry(i).or_default().push(v);
                } else {
                    involved_in.entry(i).or_default().push(v);
                }
            }
        }
        let mut violation_floor = 0.0f64;
        for (&i, vs) in &primary_of {
            let floor = vs
                .iter()
                .map(|v| v.severity * VIOLATION_FLOOR)
                .fold(0.0, f64::max);
            violation_floor = violation_floor.max(floor);
            let s = scores.entry(i).or_insert(0.0);
            *s = s.max(floor);
        }

        // ---- basis (pinned versions) ---------------------------------------------------
        let mut basis: BTreeMap<usize, Basis> = BTreeMap::new();
        for (i, node) in graph.nodes.iter().enumerate() {
            let mut b = Basis {
                status: BasisStatus::NotPinned,
                introduced: BasisStatus::NotPinned,
            };
            for pin in &node.basis {
                let Some(&ci) = changed_ids.get(pin.entity_id.as_str()) else {
                    continue;
                };
                if resolved[ci].id == node.id {
                    continue;
                }
                let s = &scorings[ci];
                let compat = |v: SemVer| match pin.version.compatibility(v) {
                    Compatibility::Identical => BasisStatus::Current,
                    Compatibility::Patch => BasisStatus::Compatible,
                    Compatibility::Additive => BasisStatus::ReviewSuggested,
                    Compatibility::Breaking => BasisStatus::Incompatible,
                };
                let after = match (resolved[ci].change_type(), s.next) {
                    (ChangeType::Deleted, _) => BasisStatus::Removed,
                    (_, Some(next)) => compat(next),
                    (_, None) => match s.bump {
                        VersionBump::None => BasisStatus::Current,
                        VersionBump::Patch => BasisStatus::Compatible,
                        VersionBump::Minor => BasisStatus::ReviewSuggested,
                        VersionBump::Major | VersionBump::Initial => BasisStatus::Incompatible,
                    },
                };
                // What the pin was before this request: an already stale pin
                // is reported, but this change did not make it so.
                let before = match resolved[ci].change_type() {
                    ChangeType::Created => BasisStatus::NotPinned,
                    _ => s.previous.map_or(BasisStatus::Current, compat),
                };
                b.status = b.status.max(after);
                if after > before {
                    b.introduced = b.introduced.max(after);
                }
            }
            if b.status != BasisStatus::NotPinned {
                basis.insert(i, b);
            }
        }

        // ---- 6. impacts ---------------------------------------------------------------------
        let th = policy.thresholds;
        let effort_of = |i: usize| {
            graph.nodes[i]
                .estimate()
                .unwrap_or(policy.aggregation.default_estimate)
        };
        let mut candidates: BTreeSet<usize> = scores
            .iter()
            .filter(|(_, s)| **s >= pp.min_impact)
            .map(|(i, _)| *i)
            .collect();
        candidates.extend(changed_nodes.iter().copied());
        candidates.extend(primary_of.keys().copied());
        candidates.extend(
            basis
                .iter()
                .filter(|(_, b)| b.introduced >= BasisStatus::ReviewSuggested)
                .map(|(i, _)| *i),
        );
        let performed: Vec<BTreeSet<Action>> = resolved
            .iter()
            .zip(&scorings)
            .map(|(r, s)| performed_actions(r, s))
            .collect();

        let distance = distances(&graph, &changed_nodes);
        // The change behind each entity's strongest contribution.
        let mut caused_by: BTreeMap<usize, usize> = BTreeMap::new();
        let mut impacts: Vec<EntityImpact> = Vec::new();
        for &i in &candidates {
            let node = &graph.nodes[i];
            let score = clamp01(scores.get(&i).copied().unwrap_or(0.0));
            let changed = changed_ids.get(node.id.as_str()).copied();
            let hits = by_target.get(&i).map(Vec::as_slice).unwrap_or(&[]);
            let primary = primary_of.get(&i).map(Vec::as_slice).unwrap_or(&[]);
            let involved = involved_in.get(&i).map(Vec::as_slice).unwrap_or(&[]);
            let b = basis.get(&i).copied().unwrap_or(Basis {
                status: BasisStatus::NotPinned,
                introduced: BasisStatus::NotPinned,
            });
            let mut sorted_hits: Vec<&Hit> = hits.iter().collect();
            sorted_hits.sort_by(|a, b| {
                b.value
                    .total_cmp(&a.value)
                    .then(a.path.len().cmp(&b.path.len()))
                    .then(a.source.cmp(&b.source))
                    .then(a.field.cmp(&b.field))
            });
            let own_local = changed.map(|c| scorings[c].local).unwrap_or(0.0);
            let dominant_is_self =
                changed.is_some() && sorted_hits.first().is_none_or(|h| own_local >= h.value);
            let channel = if dominant_is_self {
                Channel::SelfChange
            } else {
                sorted_hits
                    .first()
                    .map_or(Channel::SelfChange, |h| h.channel)
            };
            if !dominant_is_self && let Some(h) = sorted_hits.first() {
                caused_by.insert(i, h.source);
            }
            let action = choose_action(
                &ActionInput {
                    score,
                    changed: changed.is_some(),
                    dominant_is_self,
                    hits: &sorted_hits,
                    primary,
                    basis_introduced: b.introduced,
                    active: !graph.deleted[i] && node.is_active(),
                    performed: changed.map(|c| &performed[c]),
                },
                policy,
            );
            let mut reasons = Vec::new();
            if let Some(c) = changed {
                reasons.push(scorings_reason(&scorings[c], resolved[c].change_type()));
            }
            for v in primary.iter().chain(involved) {
                reasons.push(v.message.clone());
            }
            match (b.introduced, b.status) {
                (BasisStatus::Removed, _) => {
                    reasons.push("something it was planned against was deleted".to_owned())
                }
                (BasisStatus::Incompatible, _) => reasons.push(
                    "it was planned against a version that has since changed in a breaking way"
                        .to_owned(),
                ),
                (BasisStatus::ReviewSuggested, _) => reasons.push(
                    "something it was planned against gained compatible additions".to_owned(),
                ),
                (_, BasisStatus::Incompatible | BasisStatus::Removed) => reasons.push(
                    "its plan was already behind a breaking change before this request".to_owned(),
                ),
                _ => {}
            }
            if let Some(r) = rollup_reasons.get(&i) {
                reasons.push(r.clone());
            }
            for h in sorted_hits.iter().take(3) {
                reasons.push(propagate::describe(&graph, h));
            }
            let contributions: Vec<Contribution> = if request.options.explain {
                let mut seen = BTreeSet::new();
                sorted_hits
                    .iter()
                    .filter(|h| seen.insert((h.source, h.field.clone(), h.channel)))
                    .take(request.options.max_contributions)
                    .map(|h| Contribution {
                        source_id: graph.nodes[h.source].id.clone(),
                        field: h.field.clone(),
                        channel: h.channel,
                        path: h.path.iter().map(|&p| graph.nodes[p].id.clone()).collect(),
                        value: h.value,
                        detail: propagate::describe(&graph, h),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let effort = effort_of(i);
            impacts.push(EntityImpact {
                entity_id: node.id.clone(),
                kind: node.kind.clone(),
                rank: 0,
                score,
                level: level_of(score, &th),
                action,
                channel,
                distance: distance.get(&i).copied().unwrap_or(0),
                changed: changed.is_some(),
                basis_status: b.status,
                basis_introduced: b.introduced != BasisStatus::NotPinned
                    && b.introduced >= b.status,
                effort,
                effort_at_risk: score * effort,
                contributions,
                reasons,
            });
        }
        impacts.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.distance.cmp(&b.distance))
                .then(a.entity_id.cmp(&b.entity_id))
        });
        for (rank, imp) in impacts.iter_mut().enumerate() {
            imp.rank = u32::try_from(rank + 1).unwrap_or(u32::MAX);
        }

        // ---- aggregate -------------------------------------------------------------------
        // Breadth looks only at what the change touches: the changed
        // entities, what structurally depends on them, what it reached, and
        // what needs action for a conflict it introduced. Unrelated context
        // does not dilute it.
        let mut affected = graph.dependents(&changed_nodes, pp.max_depth.max(1));
        affected.extend(
            scores
                .iter()
                .filter(|(_, s)| **s >= pp.min_impact)
                .map(|(i, _)| *i),
        );
        affected.extend(primary_of.keys().copied());
        affected.retain(|&i| {
            changed_ids.contains_key(graph.nodes[i].id.as_str())
                || graph.nodes[i].status_category() != Some(StatusCategory::Canceled)
        });
        let at_risk = |i: usize| clamp01(scores.get(&i).copied().unwrap_or(0.0)) * effort_of(i);
        let effort_total: f64 = affected.iter().map(|&i| effort_of(i)).sum();
        let effort_at_risk: f64 = affected.iter().map(|&i| at_risk(i)).sum();
        let severity = impacts.iter().map(|i| i.score).fold(0.0, f64::max);
        let worst = impacts
            .first()
            .and_then(|imp| graph.get(&imp.entity_id))
            .filter(|i| affected.contains(i));
        let rest = (effort_at_risk - worst.map_or(0.0, at_risk)).max(0.0);
        let breadth = if effort_total > 0.0 {
            clamp01(rest / effort_total)
        } else {
            0.0
        };
        let agg = &policy.aggregation;
        let mut score = escalate(severity, breadth, agg.breadth_weight / agg.severity_weight);
        score = score.max(violation_floor);
        let incompatible_active = basis.iter().any(|(&i, b)| {
            b.introduced >= BasisStatus::Incompatible
                && !graph.deleted[i]
                && graph.nodes[i].is_active()
                && !changed_ids.contains_key(graph.nodes[i].id.as_str())
        });
        if incompatible_active {
            score = score.max(th.medium);
        }
        let score = clamp01(score);
        let level = level_of(score, &th);
        let replan_root = if level >= ImpactLevel::High {
            replan_root(
                &graph,
                &impacts,
                &caused_by,
                &resolved,
                &changed_ids,
                th.high,
            )
        } else {
            None
        };
        let recommendation = match level {
            ImpactLevel::None => Recommendation::NoAction,
            ImpactLevel::Low => Recommendation::Notify,
            ImpactLevel::Medium => Recommendation::Review,
            // Nothing left to re-plan (only deleted entities): review.
            ImpactLevel::High | ImpactLevel::Critical if replan_root.is_none() => {
                Recommendation::Review
            }
            ImpactLevel::High => Recommendation::PartialReplan,
            ImpactLevel::Critical => Recommendation::FullReplan,
        };

        // ---- confidence --------------------------------------------------------------------
        let mut confidence = 1.0f64;
        let missing: BTreeSet<&str> = graph
            .missing
            .iter()
            .filter(|(i, _)| changed_nodes.contains(i))
            .map(|(_, id)| id.as_str())
            .collect();
        confidence *= 0.9f64.powi(i32::try_from(missing.len().min(6)).unwrap_or(6));
        if resolved.iter().any(|r| r.placeholder) {
            confidence *= 0.7;
        }
        let field_conf: Vec<f64> = scorings.iter().map(|s| s.confidence).collect();
        if !field_conf.is_empty() {
            confidence *= field_conf.iter().sum::<f64>() / field_conf.len() as f64;
        }
        if truncated.iter().any(|t| *t) {
            confidence *= 0.8;
        }
        let confidence = confidence.clamp(0.3, 1.0);

        // ---- change assessments ---------------------------------------------------------------
        let changes: Vec<ChangeAssessment> = resolved
            .iter()
            .zip(scorings)
            .zip(truncated)
            .map(|((r, s), truncated)| {
                let explanation = scorings_reason(&s, r.change_type());
                ChangeAssessment {
                    entity_id: r.id.clone(),
                    kind: r.current().kind.clone(),
                    change_type: r.change_type(),
                    raw_score: s.raw,
                    local_score: s.local,
                    modifiers: s.modifiers,
                    fields: s.fields.into_iter().map(|f| f.assessment).collect(),
                    signals: s.signals,
                    bump: s.bump,
                    previous_version: s.previous,
                    next_version: s.next,
                    anomaly_z: s.anomaly_z,
                    propagation_truncated: truncated,
                    explanation,
                }
            })
            .collect();

        let impacts_total = u32::try_from(impacts.len()).unwrap_or(u32::MAX);
        let summary = summarize(
            score,
            level,
            recommendation,
            confidence,
            &changes,
            &impacts,
            &violations,
            replan_root.as_deref(),
            &graph,
        );
        if request.options.top_k > 0 {
            impacts.truncate(request.options.top_k);
        }
        let mut assessment = Assessment {
            score,
            level,
            recommendation,
            confidence,
            severity,
            breadth,
            effort_at_risk,
            effort_total,
            replan_root,
            changes,
            impacts,
            impacts_total,
            violations,
            summary,
            engine_version: crate::ENGINE_VERSION.to_owned(),
            assessed_at: now,
        };
        ensure_finite(&mut assessment)?;
        Ok(assessment)
    }

    /// Phase × deadline pressure for an entity, as one `γ`.
    fn phase_factor(&self, entity: &Entity) -> f64 {
        let ph = &self.policy.phase;
        match entity.status_category() {
            Some(StatusCategory::Backlog) => ph.backlog,
            Some(StatusCategory::Unstarted) => ph.unstarted,
            Some(StatusCategory::Started) => ph.started,
            Some(StatusCategory::Completed) => ph.completed,
            Some(StatusCategory::Canceled) => ph.canceled,
            None => ph.unknown,
        }
    }

    /// `(γ, days left)` from the due date of an active entity.
    fn time_pressure(&self, entity: &Entity, now: DateTime<Utc>) -> (f64, Option<f64>) {
        let tp = &self.policy.time_pressure;
        match entity.due() {
            Some(due) if entity.is_active() => {
                let days = (due - now).num_seconds() as f64 / 86_400.0;
                if days < 0.0 {
                    (tp.overdue, Some(days))
                } else {
                    (
                        1.0 + tp.amplitude * (-days / tp.horizon_days).exp(),
                        Some(days),
                    )
                }
            }
            _ => (1.0, None),
        }
    }

    /// The phase `γ` of the work a change disturbs, and why: the larger of
    /// the phase before and after (canceling started work weighs as started
    /// work, reviving canceled work as the work it revives), without the
    /// completed-phase rework boost for completing it; deleting closed work
    /// is cleanup and damped like canceled work.
    fn change_phase(&self, r: &Resolved) -> (f64, String) {
        let ph = &self.policy.phase;
        let name = |e: &Entity| {
            e.status_category()
                .map_or("without a status", StatusCategory::as_str)
        };
        match &r.snapshots {
            Snapshots::Created { after: e } | Snapshots::Activity { entity: e } => {
                (self.phase_factor(e), format!("the entity is {}", name(e)))
            }
            Snapshots::Deleted { before } if !before.is_active() => (
                ph.canceled,
                format!("deleting {} work is cleanup", name(before)),
            ),
            Snapshots::Deleted { before } => (
                self.phase_factor(before),
                format!("the deleted entity was {}", name(before)),
            ),
            Snapshots::Updated { before, after } => {
                let completing = after.status_category() == Some(StatusCategory::Completed)
                    && before.status_category() != Some(StatusCategory::Completed);
                let a = if completing {
                    self.phase_factor(after).min(ph.started)
                } else {
                    self.phase_factor(after)
                };
                let detail = if before.status_category() == after.status_category() {
                    format!("the entity is {}", name(after))
                } else {
                    format!("the entity was {} and is now {}", name(before), name(after))
                };
                (self.phase_factor(before).max(a), detail)
            }
        }
    }

    fn score_change(
        &self,
        r: &Resolved,
        corpus: &Corpus,
        history: &[&HistoryPoint],
        now: DateTime<Utc>,
    ) -> Scoring {
        let policy: &Policy = &self.policy;
        let mut fields: Vec<Scored> = Vec::new();
        match &r.snapshots {
            Snapshots::Created { after } => {
                fields.push(fields::lifecycle(true, after.estimate(), policy));
            }
            Snapshots::Deleted { before } => {
                fields.push(fields::lifecycle(false, before.estimate(), policy));
            }
            Snapshots::Updated { before, after } => {
                let embeddings = match (&before.embedding, &after.embedding) {
                    (Some(a), Some(b)) if a.len() == b.len() && !a.is_empty() => {
                        Some((a.as_slice(), b.as_slice()))
                    }
                    _ => None,
                };
                let ctx = FieldContext {
                    policy,
                    now,
                    corpus,
                    embeddings,
                };
                let names: BTreeSet<&String> =
                    before.fields.keys().chain(after.fields.keys()).collect();
                for name in names {
                    if let Some(s) =
                        fields::score(name, before.fields.get(name), after.fields.get(name), &ctx)
                    {
                        fields.push(s);
                    }
                }
                if let Some(s) = fields::reparent(
                    before.parent_id.as_deref(),
                    after.parent_id.as_deref(),
                    policy,
                ) {
                    fields.push(s);
                }
                let relations = |e: &Entity| {
                    FieldValue::Set(
                        e.relations
                            .iter()
                            .map(|r| format!("{} {}", r.kind.as_str(), r.target))
                            .collect(),
                    )
                };
                let (rb, ra) = (relations(before), relations(after));
                if let Some(s) = fields::score(field::RELATIONS, Some(&rb), Some(&ra), &ctx) {
                    fields.push(s);
                }
                correlate(&mut fields, before, after);
            }
            Snapshots::Activity { .. } => {}
        }

        // Activity: every category counts once across the change set's comments.
        let sigs = signals::detect_all(
            r.events.iter().map(|e| e.body.as_str()),
            &policy.signals,
            &policy.text,
        );
        let activity_magnitude = signals::magnitude(&sigs);
        if activity_magnitude > 0.0 {
            let schedule = sigs.iter().any(|s| {
                matches!(
                    s.category,
                    SignalCategory::Blocker | SignalCategory::Schedule
                )
            });
            let scope = sigs
                .iter()
                .any(|s| s.category == SignalCategory::Scope && s.weight >= 0.6);
            let mut fp = policy.activity;
            if scope {
                fp.severity = fp.severity.max(0.6);
            }
            if schedule {
                let sp = &policy.activity_schedule;
                fp.severity = fp.severity.max(sp.severity);
                fp.down = fp.down.max(sp.down);
                fp.up = fp.up.max(sp.up);
                fp.dependency = fp.dependency.max(sp.dependency);
                fp.upstream = fp.upstream.max(sp.upstream);
                fp.related = fp.related.max(sp.related);
                fp.sibling = fp.sibling.max(sp.sibling);
            }
            let mut metrics = BTreeMap::new();
            for s in &sigs {
                let e = metrics
                    .entry(s.category.as_str().to_owned())
                    .or_insert(0.0f64);
                *e = e.max(s.weight);
            }
            let mut activity_text = String::new();
            for ev in &r.events {
                activity_text.push_str(&ev.body);
                activity_text.push('\n');
            }
            let a = text::analyze(&activity_text, &policy.text);
            let delta = TextDelta::addition(&a.counts, corpus);
            let categories: BTreeSet<&str> = sigs.iter().map(|s| s.category.as_str()).collect();
            fields.push(Scored {
                assessment: FieldAssessment {
                    field: field::ACTIVITY.to_owned(),
                    change: FieldChangeKind::Added,
                    direction: crate::assessment::Direction::None,
                    magnitude: activity_magnitude,
                    severity: fp.severity,
                    risk: clamp01(fp.severity * activity_magnitude),
                    breaking: false,
                    additive: false,
                    before: None,
                    after: None,
                    metrics,
                    explanation: format!(
                        "activity: {} signal(s): {}",
                        sigs.len(),
                        categories.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                },
                channels: Channels::from_policy(&fp),
                detail: Detail::Activity {
                    delta,
                    schedule,
                    scope,
                },
                confidence: 0.85,
                max_bump: VersionBump::None,
                weight: 1.0,
                changed_terms: BTreeSet::new(),
            });
        }

        let is_content = |f: &&Scored| f.assessment.field != field::ACTIVITY;
        let counted = |f: &Scored| f.assessment.risk * f.weight;
        let raw = noisy_or(fields.iter().filter(is_content).map(counted));
        let total = noisy_or(fields.iter().map(counted));

        let mut modifiers = Vec::new();
        let (phase, phase_detail) = self.change_phase(r);
        modifiers.push(Modifier {
            name: "phase".to_owned(),
            factor: phase,
            detail: phase_detail,
        });
        let (pressure, days_left) = match &r.snapshots {
            Snapshots::Updated { before, after } => {
                let (b, a) = (
                    self.time_pressure(before, now),
                    self.time_pressure(after, now),
                );
                if b.0 > a.0 { b } else { a }
            }
            _ => self.time_pressure(r.current(), now),
        };
        if let Some(days) = days_left {
            modifiers.push(Modifier {
                name: "time_pressure".to_owned(),
                factor: pressure,
                detail: if days < 0.0 {
                    format!("overdue by {:.1} days", -days)
                } else {
                    format!("due in {days:.1} days")
                },
            });
        }
        let vp = &policy.volatility;
        let points: Vec<(f64, f64)> = history
            .iter()
            .map(|h| ((now - h.at).num_seconds() as f64 / 86_400.0, h.magnitude))
            .collect();
        let churn = decayed_sum(&points, vp.half_life_days);
        let volatility = 1.0 + vp.amplitude * (churn / vp.saturation).tanh();
        if churn > 0.0 {
            modifiers.push(Modifier {
                name: "volatility".to_owned(),
                factor: volatility,
                detail: format!(
                    "recent churn {churn:.2} (half-life {:.0} days)",
                    vp.half_life_days
                ),
            });
        }
        let local = amplify(total, phase * pressure * volatility);
        let magnitudes: Vec<f64> = history.iter().map(|h| h.magnitude).collect();
        let anomaly_z = z_score(raw, &magnitudes, vp.min_samples);

        let content: Vec<&Scored> = fields.iter().filter(is_content).collect();
        let bump = match r.change_type() {
            ChangeType::Created => VersionBump::Initial,
            ChangeType::Deleted => VersionBump::Major,
            _ if content.is_empty() => VersionBump::None,
            _ => {
                // The content score against a threshold counts only the
                // fields allowed to reach that level: reassigning and moving
                // to the next cycle never add up to a breaking change.
                let up_to = |cap: VersionBump| {
                    noisy_or(
                        content
                            .iter()
                            .filter(|f| f.max_bump >= cap)
                            .map(|f| counted(f)),
                    )
                };
                let ceiling = content
                    .iter()
                    .map(|f| f.max_bump)
                    .max()
                    .unwrap_or(VersionBump::None);
                let bump = if content.iter().any(|f| f.assessment.breaking)
                    || up_to(VersionBump::Major) >= policy.semver.major
                {
                    VersionBump::Major
                } else if content.iter().any(|f| f.assessment.additive)
                    || up_to(VersionBump::Minor) >= policy.semver.minor
                {
                    VersionBump::Minor
                } else {
                    VersionBump::Patch
                };
                bump.min(ceiling)
            }
        };
        let previous = match &r.snapshots {
            Snapshots::Created { .. } => None,
            Snapshots::Deleted { before } => before.version,
            Snapshots::Updated { before, after } => before.version.or(after.version),
            Snapshots::Activity { entity } => entity.version,
        };
        let next = match &r.snapshots {
            Snapshots::Created { after } => Some(after.version.unwrap_or(SemVer::INITIAL)),
            Snapshots::Deleted { .. } => None,
            _ => previous.map(|p| p.bump(bump)),
        };
        let confidence = if content.is_empty() {
            if r.events.is_empty() { 1.0 } else { 0.85 }
        } else {
            content.iter().map(|f| f.confidence).sum::<f64>() / content.len() as f64
        };
        Scoring {
            fields,
            signals: sigs,
            raw,
            local,
            modifiers,
            bump,
            previous,
            next,
            anomaly_z,
            confidence,
        }
    }
}

/// The after and before graphs. Nodes are sorted by id, so indices (and
/// every tie-break on them) do not depend on the order of the request.
fn build_graphs(
    context: &BTreeMap<EntityId, &Entity>,
    resolved: &[Resolved],
    changed_ids: &BTreeMap<&str, usize>,
) -> (Graph, Graph) {
    let mut after: Vec<(Entity, bool)> = Vec::new();
    let mut before: Vec<(Entity, bool)> = Vec::new();
    for (id, e) in context {
        if !changed_ids.contains_key(id.as_str()) {
            after.push(((*e).clone(), false));
            before.push(((*e).clone(), false));
        }
    }
    for r in resolved {
        match &r.snapshots {
            Snapshots::Deleted { before: b } => {
                after.push((b.clone(), true));
                before.push((b.clone(), false));
            }
            Snapshots::Created { after: a } => after.push((a.clone(), false)),
            Snapshots::Updated {
                before: b,
                after: a,
            } => {
                after.push((a.clone(), false));
                before.push((b.clone(), false));
            }
            Snapshots::Activity { entity } => {
                after.push((entity.clone(), false));
                before.push((entity.clone(), false));
            }
        }
    }
    after.sort_by(|x, y| x.0.id.cmp(&y.0.id));
    before.sort_by(|x, y| x.0.id.cmp(&y.0.id));
    (Graph::build(after), Graph::build(before))
}

/// Fields of one change that carry the same evidence count once:
///
/// * a move to another project that is also the new parent (issues without a
///   parent issue use their project as parent) is one fact — the weaker of
///   `parent` and `project` is not counted, and does not travel;
/// * a rename mirrored in title and description: the weaker of the two
///   counts only for the share of its changed terms the other does not
///   already change.
fn correlate(fields: &mut [Scored], before: &Entity, after: &Entity) {
    let find =
        |fields: &[Scored], name: &str| fields.iter().position(|f| f.assessment.field == name);
    let weaker = |fields: &[Scored], x: usize, y: usize| {
        if fields[x].assessment.risk > fields[y].assessment.risk {
            (x, y)
        } else {
            (y, x)
        }
    };
    if let (Some(p), Some(j)) = (find(fields, field::PARENT), find(fields, field::PROJECT)) {
        let project_is_parent = |e: &Entity| matches!((e.parent_id.as_deref(), e.reference(field::PROJECT)), (Some(a), Some(b)) if a == b);
        if project_is_parent(before) || project_is_parent(after) {
            let (strong, weak) = weaker(fields, p, j);
            discount(fields, weak, strong, 1.0);
        }
    }
    if let (Some(t), Some(d)) = (find(fields, field::TITLE), find(fields, field::DESCRIPTION)) {
        let (strong, weak) = weaker(fields, t, d);
        let (w, s) = (&fields[weak].changed_terms, &fields[strong].changed_terms);
        if !w.is_empty() {
            let shared = w.intersection(s).count() as f64 / w.len() as f64;
            if shared > 0.0 {
                discount(fields, weak, strong, shared);
            }
        }
    }
}

fn discount(fields: &mut [Scored], weak: usize, strong: usize, share: f64) {
    let other = fields[strong].assessment.field.clone();
    let f = &mut fields[weak];
    let share = clamp01(share);
    f.weight *= 1.0 - share;
    f.assessment.metrics.insert("counted".to_owned(), f.weight);
    f.assessment.explanation.push_str(&format!(
        " (repeats {:.0}% of the {other} change: counted {:.0}%)",
        share * 100.0,
        f.weight * 100.0
    ));
}

/// The entity of a violation that needs action: the first one, except in a
/// blocking cycle, where it is the changed member (the one that closed it).
fn primary_entity<'a>(v: &'a Violation, changed: &BTreeMap<&str, usize>) -> Option<&'a str> {
    if v.kind == ViolationKind::CyclicDependency
        && let Some(c) = v
            .entities
            .iter()
            .find(|id| changed.contains_key(id.as_str()))
    {
        return Some(c);
    }
    v.entities.first().map(String::as_str)
}

/// The actions a change itself performed, which its own entity is not asked to do again.
fn performed_actions(r: &Resolved, s: &Scoring) -> BTreeSet<Action> {
    let mut out = BTreeSet::new();
    if r.change_type() == ChangeType::Deleted {
        out.insert(Action::CancelOrRehome);
    }
    for f in &s.fields {
        match (f.assessment.field.as_str(), &f.detail) {
            (field::ESTIMATE, _) => {
                out.insert(Action::ReEstimate);
            }
            (field::DUE_DATE | field::START_DATE, _) => {
                out.insert(Action::Reschedule);
            }
            (field::PARENT, _) | (_, Detail::Status(Transition::Cancel)) => {
                out.insert(Action::CancelOrRehome);
            }
            _ => {}
        }
    }
    out
}

/// Root candidates for one highly affected entity: never a deleted entity
/// (its parent instead), and a changed leaf stands for its parent (or, when
/// it moved, both the parent it left and the one it joined).
fn root_candidates(
    g: &Graph,
    i: usize,
    resolved: &[Resolved],
    changed: &BTreeMap<&str, usize>,
) -> Vec<usize> {
    if g.deleted[i] {
        return g.parent[i].into_iter().filter(|&p| !g.deleted[p]).collect();
    }
    let Some(&c) = changed.get(g.nodes[i].id.as_str()) else {
        return vec![i];
    };
    if !g.children[i].is_empty() {
        return vec![i];
    }
    let mut out: Vec<usize> = Vec::new();
    if let Snapshots::Updated { before, after } = &resolved[c].snapshots
        && before.parent_id != after.parent_id
        && let Some(old) = before.parent_id.as_deref().and_then(|id| g.get(id))
        && !g.deleted[old]
    {
        out.push(old);
    }
    if let Some(p) = g.parent[i].filter(|&p| !g.deleted[p]) {
        out.push(p);
    }
    if out.is_empty() {
        out.push(i);
    }
    out
}

/// The smallest subtree to re-plan: the lowest common ancestor of the root
/// candidates of every entity scoring at least `high` (of the most affected
/// one when none does) and of the changes behind them, lifted above deleted
/// entities; else the most affected candidate. `None` only when nothing but
/// deleted entities is left.
fn replan_root(
    g: &Graph,
    impacts: &[EntityImpact],
    caused_by: &BTreeMap<usize, usize>,
    resolved: &[Resolved],
    changed: &BTreeMap<&str, usize>,
    high: f64,
) -> Option<EntityId> {
    let highly: Vec<&EntityImpact> = impacts.iter().filter(|imp| imp.score >= high).collect();
    let source: Vec<&EntityImpact> = if highly.is_empty() {
        impacts.iter().take(1).collect()
    } else {
        highly
    };
    let mut nodes: Vec<usize> = Vec::new();
    let add = |imp: &EntityImpact, nodes: &mut Vec<usize>| {
        let Some(i) = g.get(&imp.entity_id) else {
            return;
        };
        // An entity hit by a change is re-planned together with that
        // change's subtree (a child whose scope the parent removed: the
        // parent's plan), not on its own.
        for j in std::iter::once(i).chain(caused_by.get(&i).copied()) {
            for n in root_candidates(g, j, resolved, changed) {
                if !nodes.contains(&n) {
                    nodes.push(n);
                }
            }
        }
    };
    for imp in source {
        add(imp, &mut nodes);
    }
    // Only deleted entities scored high: fall back to the next affected one.
    for imp in impacts {
        if !nodes.is_empty() {
            break;
        }
        add(imp, &mut nodes);
    }
    let mut root = g
        .lowest_common_ancestor(&nodes)
        .or_else(|| nodes.first().copied())?;
    let mut hops = 0;
    while g.deleted[root] {
        root = g.parent[root]?;
        hops += 1;
        if hops > crate::graph::MAX_ANCESTRY {
            return None;
        }
    }
    Some(g.nodes[root].id.clone())
}

/// Whether a hit would mean redoing the target's work if it were done:
/// anything but text, and text that rewords or adds to what it is about.
fn is_rework(h: &Hit, policy: &Policy) -> bool {
    !h.text
        || (!removed_text(h, policy)
            && h.added_overlap.max(h.modified_overlap) >= policy.propagation.added_overlap_flag)
}

/// Whether a hit is about text that was removed (it may make the target obsolete).
fn removed_text(h: &Hit, policy: &Policy) -> bool {
    h.text
        && h.field != field::ACTIVITY
        && h.removed_overlap >= policy.propagation.removed_overlap_flag
        && h.removed_overlap > h.added_overlap.max(h.modified_overlap)
}

fn violation_action(kind: ViolationKind) -> Action {
    match kind {
        ViolationKind::OrphanedWork => Action::CancelOrRehome,
        // The blocked entity replaces or drops its blocker.
        ViolationKind::BlockedByClosed | ViolationKind::CyclicDependency => Action::ResolveConflict,
        ViolationKind::ScheduleConflict
        | ViolationKind::DependencyConflict
        | ViolationKind::Overdue => Action::Reschedule,
        ViolationKind::EstimateOverflow => Action::ReEstimate,
        ViolationKind::PrematureClose => Action::Review,
    }
}

/// What [`choose_action`] looks at for one entity.
struct ActionInput<'a> {
    score: f64,
    changed: bool,
    dominant_is_self: bool,
    /// Strongest first.
    hits: &'a [&'a Hit],
    /// Introduced violations this entity is the one to act on.
    primary: &'a [&'a Violation],
    basis_introduced: BasisStatus,
    /// Not closed and not deleted.
    active: bool,
    /// For a changed entity: what its change already did.
    performed: Option<&'a BTreeSet<Action>>,
}

fn choose_action(input: &ActionInput<'_>, policy: &Policy) -> Action {
    let action = action_for(input, policy);
    // Never ask a changed entity to do what its change just did.
    if input.performed.is_some_and(|p| p.contains(&action)) {
        Action::Review
    } else {
        action
    }
}

fn action_for(input: &ActionInput<'_>, policy: &Policy) -> Action {
    let th = &policy.thresholds;
    let pp = &policy.propagation;
    let obsolete = input.active
        && input.hits.iter().any(|h| {
            h.value >= th.low
                && matches!(h.channel, Channel::Down | Channel::Related)
                && removed_text(h, policy)
        });
    if let Some(v) = input
        .primary
        .iter()
        .max_by(|a, b| a.severity.total_cmp(&b.severity))
    {
        let action = violation_action(v.kind);
        // Rescheduling or re-estimating work whose scope was removed is moot.
        if obsolete
            && matches!(
                action,
                Action::Reschedule | Action::ReEstimate | Action::Review
            )
        {
            return Action::ObsoleteCandidate;
        }
        return action;
    }
    if input.score < th.low {
        return if input.basis_introduced >= BasisStatus::ReviewSuggested && input.active {
            Action::Review
        } else {
            Action::None
        };
    }
    if input.changed && input.dominant_is_self {
        return if input.score >= th.high {
            Action::Replan
        } else if input.score >= th.medium {
            Action::Review
        } else {
            Action::Notify
        };
    }
    let fallback =
        if input.score >= th.medium || input.basis_introduced >= BasisStatus::Incompatible {
            Action::Review
        } else {
            Action::Notify
        };
    let Some(h) = input.hits.first() else {
        return fallback;
    };
    if !input.active {
        // Closed work is not re-estimated, rescheduled or rescoped: its
        // owners hear about it, and completed work under a reopened parent
        // is re-verified.
        return if h.channel == Channel::Down && h.transition == Some(Transition::Reopen) {
            Action::Reverify
        } else {
            Action::Notify
        };
    }
    match (h.lifecycle, h.transition, h.channel) {
        (Some(LifecycleKind::Deleted), _, Channel::Down)
        | (_, Some(Transition::Cancel), Channel::Down) => {
            return Action::CancelOrRehome;
        }
        (Some(LifecycleKind::Deleted), _, Channel::Dependency)
        | (_, Some(Transition::Cancel), Channel::Dependency) => return Action::ResolveConflict,
        (Some(_), _, Channel::Up) | (_, Some(Transition::Cancel), Channel::Up) => {
            return Action::ReEstimate;
        }
        // A new blocker, or a blocker that is no longer done or moved back.
        (Some(LifecycleKind::Created), _, Channel::Dependency)
        | (
            _,
            Some(Transition::Reopen | Transition::Regress | Transition::Revive),
            Channel::Dependency,
        ) => {
            return Action::Reschedule;
        }
        (Some(_), _, _) => return Action::Notify,
        _ => {}
    }
    if h.rollup.is_some() {
        // Effort moved under it: re-estimate; a sibling only competes.
        return if h.channel == Channel::Up {
            Action::ReEstimate
        } else {
            Action::Notify
        };
    }
    if h.moved_along {
        return Action::Notify;
    }
    if h.schedule_signal {
        return Action::Reschedule;
    }
    if h.text {
        // Text a child or a related item is about: obsolete or rescope it.
        // A parent, blocker or sibling only reviews.
        if !matches!(h.channel, Channel::Down | Channel::Related) {
            return fallback;
        }
        let changed = h.added_overlap.max(h.modified_overlap);
        if removed_text(h, policy) {
            return Action::ObsoleteCandidate;
        }
        if changed >= pp.added_overlap_flag && (h.field != field::ACTIVITY || h.scope_signal) {
            return Action::UpdateScope;
        }
        return fallback;
    }
    match h.field.as_str() {
        field::ESTIMATE => match h.channel {
            Channel::Up => Action::ReEstimate,
            Channel::Dependency => Action::Reschedule,
            _ => Action::Notify,
        },
        field::DUE_DATE | field::START_DATE => {
            if h.date_conflict || input.score >= th.medium {
                Action::Reschedule
            } else {
                Action::Notify
            }
        }
        field::PRIORITY => Action::Reprioritize,
        _ => fallback,
    }
}

fn check_request(request: &AssessmentRequest) -> Result<()> {
    if request.changes.is_empty() {
        return Err(Error::invalid("the request has no changes to assess"));
    }
    if request.changes.len() > MAX_CHANGES {
        return Err(Error::invalid(format!(
            "the request has {} changes, more than the {MAX_CHANGES} allowed",
            request.changes.len()
        )));
    }
    if request.context.len() > MAX_CONTEXT {
        return Err(Error::invalid(format!(
            "the request has {} context entities, more than the {MAX_CONTEXT} allowed",
            request.context.len()
        )));
    }
    if request.history.len() > MAX_HISTORY {
        return Err(Error::invalid(format!(
            "the request has {} history points, more than the {MAX_HISTORY} allowed",
            request.history.len()
        )));
    }
    Ok(())
}

fn check_text(what: &str, text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(Error::invalid(format!(
            "{what} is longer than {MAX_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_embedding(what: &str, embedding: &[f32]) -> Result<()> {
    if embedding.len() > MAX_EMBEDDING_DIM {
        return Err(Error::invalid(format!(
            "{what} has more than {MAX_EMBEDDING_DIM} dimensions"
        )));
    }
    if embedding.iter().any(|x| !x.is_finite()) {
        return Err(Error::invalid(format!("{what} is not finite")));
    }
    Ok(())
}

fn check_value(name: &str, value: &FieldValue, id: &str) -> Result<()> {
    let too_long = |len: usize| {
        (len > MAX_TEXT_BYTES).then(|| {
            Error::invalid(format!(
                "field `{name}` of `{id}` is longer than {MAX_TEXT_BYTES} bytes"
            ))
        })
    };
    let err = match value {
        FieldValue::Number(n) if !n.is_finite() => Some(Error::invalid(format!(
            "field `{name}` of `{id}` is not a finite number"
        ))),
        FieldValue::Number(n) if n.abs() > MAX_NUMBER => Some(Error::invalid(format!(
            "field `{name}` of `{id}` is larger than {MAX_NUMBER:e} in magnitude"
        ))),
        FieldValue::Ordinal(o) if o.scale == 0 || o.rank > o.scale => {
            Some(Error::invalid(format!(
                "field `{name}` of `{id}` has rank {} on a scale of {}; the scale must be positive and at least the rank",
                o.rank, o.scale
            )))
        }
        FieldValue::Text(t) | FieldValue::Reference(t) => too_long(t.len()),
        FieldValue::Status(s) => too_long(s.name.len()),
        FieldValue::Set(items) if items.len() > MAX_ITEMS => Some(Error::invalid(format!(
            "field `{name}` of `{id}` has more than {MAX_ITEMS} items"
        ))),
        FieldValue::Set(items) => too_long(items.iter().map(String::len).sum()),
        _ => None,
    };
    err.map_or(Ok(()), Err)
}

fn validate_entity(e: &Entity) -> Result<()> {
    if e.id.trim().is_empty() {
        return Err(Error::invalid("an entity has an empty id"));
    }
    if e.fields.len() > MAX_ITEMS || e.relations.len() > MAX_ITEMS || e.basis.len() > MAX_ITEMS {
        return Err(Error::invalid(format!(
            "`{}` has more than {MAX_ITEMS} fields, relations or basis pins",
            e.id
        )));
    }
    for (name, value) in &e.fields {
        check_value(name, value, &e.id)?;
    }
    if let Some(emb) = &e.embedding {
        check_embedding(&format!("the embedding of `{}`", e.id), emb)?;
    }
    Ok(())
}

fn resolve(cs: &ChangeSet, context: &BTreeMap<EntityId, &Entity>) -> Result<Resolved> {
    let id = if cs.entity_id.trim().is_empty() {
        cs.after
            .as_ref()
            .or(cs.before.as_ref())
            .map(|e| e.id.clone())
            .or_else(|| cs.events.first().map(|e| e.entity_id.clone()))
            .unwrap_or_default()
    } else {
        cs.entity_id.clone()
    };
    if id.trim().is_empty() {
        return Err(Error::invalid("a change set names no entity"));
    }
    for snap in [&cs.before, &cs.after].into_iter().flatten() {
        validate_entity(snap)?;
        if snap.id != id {
            return Err(Error::invalid(format!(
                "change set for `{id}` carries a snapshot of `{}`",
                snap.id
            )));
        }
    }
    if cs.events.len() > MAX_EVENTS {
        return Err(Error::invalid(format!(
            "change set for `{id}` has {} events, more than the {MAX_EVENTS} allowed",
            cs.events.len()
        )));
    }
    let activity_bytes: usize = cs.events.iter().map(|e| e.body.len()).sum();
    if activity_bytes > MAX_TEXT_BYTES {
        return Err(Error::invalid(format!(
            "the activity of `{id}` is longer than {MAX_TEXT_BYTES} bytes in total"
        )));
    }
    let mut events = cs.events.clone();
    for ev in &mut events {
        if ev.entity_id.is_empty() {
            ev.entity_id = id.clone();
        } else if ev.entity_id != id {
            return Err(Error::invalid(format!(
                "change set for `{id}` carries an event of `{}`",
                ev.entity_id
            )));
        }
    }

    if !cs.field_changes.is_empty() {
        if cs.before.is_some() {
            return Err(Error::invalid(format!(
                "change set for `{id}` has both `before` and `field_changes`; send one"
            )));
        }
        if cs.field_changes.len() > MAX_FIELD_CHANGES {
            return Err(Error::invalid(format!(
                "change set for `{id}` has {} field changes, more than the {MAX_FIELD_CHANGES} allowed",
                cs.field_changes.len()
            )));
        }
        let base = match (&cs.after, context.get(&id)) {
            (Some(a), _) => a.clone(),
            (None, Some(c)) => (*c).clone(),
            (None, None) => {
                return Err(Error::invalid(format!(
                    "field changes for `{id}` need the entity in `after` or in the context"
                )));
            }
        };
        let mut before = base.clone();
        let mut after = base;
        let mut names = BTreeSet::new();
        for fc in &cs.field_changes {
            if fc.field.trim().is_empty() {
                return Err(Error::invalid(format!(
                    "a field change of `{id}` names no field"
                )));
            }
            if !names.insert(fc.field.as_str()) {
                return Err(Error::invalid(format!(
                    "field `{}` of `{id}` is changed twice",
                    fc.field
                )));
            }
            if fc.field == field::PARENT {
                let as_id = |v: &Option<FieldValue>| match v {
                    Some(FieldValue::Reference(r)) | Some(FieldValue::Text(r)) => Some(r.clone()),
                    _ => None,
                };
                before.parent_id = as_id(&fc.before);
                after.parent_id = as_id(&fc.after);
                continue;
            }
            match &fc.before {
                Some(v) => before.fields.insert(fc.field.clone(), v.clone()),
                None => before.fields.remove(&fc.field),
            };
            match &fc.after {
                Some(v) => after.fields.insert(fc.field.clone(), v.clone()),
                None => after.fields.remove(&fc.field),
            };
        }
        before.version = after.version.or(before.version);
        // The derived snapshots obey the same limits as sent ones.
        validate_entity(&before)?;
        validate_entity(&after)?;
        return Ok(Resolved {
            id,
            snapshots: Snapshots::Updated { before, after },
            events,
            placeholder: false,
        });
    }

    let (snapshots, placeholder) = match (&cs.before, &cs.after) {
        (Some(b), Some(a)) => (
            Snapshots::Updated {
                before: b.clone(),
                after: a.clone(),
            },
            false,
        ),
        (None, Some(a)) => (Snapshots::Created { after: a.clone() }, false),
        (Some(b), None) => (Snapshots::Deleted { before: b.clone() }, false),
        (None, None) => {
            if events.is_empty() {
                return Err(Error::invalid(format!(
                    "change set for `{id}` has nothing to assess"
                )));
            }
            match context.get(&id) {
                Some(e) => (
                    Snapshots::Activity {
                        entity: (*e).clone(),
                    },
                    false,
                ),
                None => (
                    Snapshots::Activity {
                        entity: Entity::new(id.clone(), "unknown"),
                    },
                    true,
                ),
            }
        }
    };
    Ok(Resolved {
        id,
        snapshots,
        events,
        placeholder,
    })
}

/// Rejects an assessment with a non-finite number (which JSON cannot carry).
/// Validation keeps inputs bounded, so this is a last line of defence.
fn ensure_finite(a: &mut Assessment) -> Result<()> {
    for c in &mut a.changes {
        if c.anomaly_z.is_some_and(|z| !z.is_finite()) {
            c.anomaly_z = None;
        }
    }
    let finite = |x: &f64| x.is_finite();
    let ok = [
        a.score,
        a.confidence,
        a.severity,
        a.breadth,
        a.effort_at_risk,
        a.effort_total,
    ]
    .iter()
    .all(finite)
        && a.changes.iter().all(|c| {
            [c.raw_score, c.local_score].iter().all(finite)
                && c.modifiers.iter().all(|m| m.factor.is_finite())
                && c.signals.iter().all(|s| s.weight.is_finite())
                && c.fields.iter().all(|f| {
                    [f.magnitude, f.severity, f.risk].iter().all(finite)
                        && f.metrics.values().all(finite)
                })
        })
        && a.impacts.iter().all(|i| {
            [i.score, i.effort, i.effort_at_risk].iter().all(finite)
                && i.contributions.iter().all(|c| c.value.is_finite())
        })
        && a.violations.iter().all(|v| v.severity.is_finite());
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "the request produces a number that is not finite; check its estimates and other numbers",
        ))
    }
}

fn level_of(score: f64, th: &crate::policy::Thresholds) -> ImpactLevel {
    if score >= th.critical {
        ImpactLevel::Critical
    } else if score >= th.high {
        ImpactLevel::High
    } else if score >= th.medium {
        ImpactLevel::Medium
    } else if score >= th.low {
        ImpactLevel::Low
    } else {
        ImpactLevel::None
    }
}

/// Hops from the nearest changed entity over any edge.
fn distances(g: &Graph, changed: &[usize]) -> BTreeMap<usize, u32> {
    let mut out: BTreeMap<usize, u32> = changed.iter().map(|&c| (c, 0)).collect();
    let mut queue: std::collections::VecDeque<usize> = changed.iter().copied().collect();
    while let Some(i) = queue.pop_front() {
        let d = out.get(&i).copied().unwrap_or(0);
        if d >= 32 {
            continue;
        }
        let neighbours = g.parent[i]
            .into_iter()
            .chain(g.children[i].iter().copied())
            .chain(g.blocks[i].iter().copied())
            .chain(g.blocked_by[i].iter().copied())
            .chain(g.related[i].iter().copied());
        for j in neighbours {
            if let std::collections::btree_map::Entry::Vacant(e) = out.entry(j) {
                e.insert(d + 1);
                queue.push_back(j);
            }
        }
    }
    out
}

fn scorings_reason(s: &Scoring, change_type: ChangeType) -> String {
    let what = match change_type {
        ChangeType::Created => "created".to_owned(),
        ChangeType::Deleted => "deleted".to_owned(),
        ChangeType::Activity => "new activity".to_owned(),
        ChangeType::Updated => {
            let names: Vec<&str> = s
                .fields
                .iter()
                .filter(|f| f.assessment.field != field::ACTIVITY)
                .map(|f| f.assessment.field.as_str())
                .collect();
            if names.is_empty() {
                "no field changed".to_owned()
            } else {
                format!("changed {}", names.join(", "))
            }
        }
    };
    let version = match (s.previous, s.next) {
        (Some(p), Some(n)) if p != n => format!(" ({} {p} → {n})", s.bump.as_str()),
        (None, Some(n)) => format!(" (v{n})"),
        _ if s.bump != VersionBump::None => format!(" ({} bump)", s.bump.as_str()),
        _ => String::new(),
    };
    let mut out = format!(
        "{what}{version}; content score {:.2}, local score {:.2}",
        s.raw, s.local
    );
    if let Some(top) = s
        .fields
        .iter()
        .max_by(|a, b| (a.assessment.risk * a.weight).total_cmp(&(b.assessment.risk * b.weight)))
    {
        out.push_str(&format!("; strongest: {}", top.assessment.explanation));
    }
    if let Some(z) = s.anomaly_z
        && z >= 2.0
    {
        out.push_str(&format!("; unusually large for this entity (z = {z:.1})"));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    score: f64,
    level: ImpactLevel,
    recommendation: Recommendation,
    confidence: f64,
    changes: &[ChangeAssessment],
    impacts: &[EntityImpact],
    violations: &[Violation],
    replan_root: Option<&str>,
    graph: &Graph,
) -> String {
    let label = |id: &str| {
        graph
            .get(id)
            .map(|i| graph.nodes[i].label())
            .unwrap_or_else(|| id.to_owned())
    };
    let mut parts = vec![format!(
        "{} impact (score {score:.2}, confidence {confidence:.2})",
        level.as_str()
    )];
    let majors: Vec<String> = changes
        .iter()
        .filter(|c| c.bump == VersionBump::Major)
        .map(|c| label(&c.entity_id))
        .collect();
    if !majors.is_empty() {
        parts.push(format!("breaking change to {}", majors.join(", ")));
    }
    let affected: Vec<&EntityImpact> = impacts
        .iter()
        .filter(|i| !i.changed && i.level >= ImpactLevel::Low)
        .collect();
    if !affected.is_empty() {
        let top: Vec<String> = affected
            .iter()
            .take(3)
            .map(|i| {
                format!(
                    "{} ({}, {})",
                    label(&i.entity_id),
                    i.level.as_str(),
                    i.action.as_str()
                )
            })
            .collect();
        parts.push(format!(
            "{} other item(s) affected, most: {}",
            affected.len(),
            top.join("; ")
        ));
    }
    let introduced = violations.iter().filter(|v| v.introduced).count();
    if introduced > 0 {
        parts.push(format!("{introduced} plan conflict(s) introduced"));
    }
    let rec = match (recommendation, replan_root) {
        (Recommendation::PartialReplan, Some(root)) => {
            format!("re-plan the work under {}", label(root))
        }
        (Recommendation::FullReplan, Some(root)) => {
            format!("re-plan everything under {}", label(root))
        }
        (r, _) => r.as_str().replace('_', " "),
    };
    parts.push(format!("recommendation: {rec}"));
    parts.join("; ")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ActivityEvent, FieldChange, RelationKind};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 0, 0, 0)
            .single()
            .expect("valid")
    }

    fn request(changes: Vec<ChangeSet>, context: Vec<Entity>) -> AssessmentRequest {
        AssessmentRequest {
            changes,
            context,
            now: Some(now()),
            ..AssessmentRequest::default()
        }
    }

    fn engine() -> Engine {
        Engine::default()
    }

    #[test]
    fn empty_request_is_rejected() {
        let err = engine()
            .assess(&request(vec![], vec![]))
            .expect_err("no changes");
        assert_eq!(err.kind(), crate::error::ErrorKind::InvalidRequest);
    }

    #[test]
    fn no_op_update_has_no_impact() {
        let e = Entity::new("a", "issue")
            .with_title("Same")
            .with_version(SemVer::new(1, 2, 3));
        let a = engine()
            .assess(&request(vec![ChangeSet::update(e.clone(), e)], vec![]))
            .expect("assesses");
        assert_eq!(a.level, ImpactLevel::None);
        assert_eq!(a.changes[0].bump, VersionBump::None);
        assert_eq!(a.changes[0].next_version, Some(SemVer::new(1, 2, 3)));
    }

    #[test]
    fn canceled_parent_orphans_active_children() {
        let parent = Entity::new("p", "issue")
            .with_title("Payments revamp")
            .with_status(StatusCategory::Started, "In Progress")
            .with_version(SemVer::new(1, 0, 0));
        let canceled = parent
            .clone()
            .with_status(StatusCategory::Canceled, "Canceled");
        let kid = Entity::new("k", "sub_issue")
            .with_parent("p")
            .with_title("Card form")
            .with_status(StatusCategory::Started, "In Progress");
        let a = engine()
            .assess(&request(
                vec![ChangeSet::update(parent, canceled)],
                vec![kid],
            ))
            .expect("assesses");
        assert!(a.level >= ImpactLevel::High, "{}", a.summary);
        assert_eq!(a.changes[0].bump, VersionBump::Major);
        let k = a
            .impacts
            .iter()
            .find(|i| i.entity_id == "k")
            .expect("child affected");
        assert_eq!(k.action, Action::CancelOrRehome);
        assert!(
            a.violations
                .iter()
                .any(|v| v.kind == ViolationKind::OrphanedWork && v.introduced)
        );
    }

    #[test]
    fn field_changes_and_errors() {
        let current = Entity::new("a", "issue")
            .with_estimate(8.0)
            .with_title("Do it");
        let cs = ChangeSet::fields(
            current.clone(),
            vec![FieldChange {
                field: "estimate".into(),
                before: Some(FieldValue::Number(3.0)),
                after: Some(FieldValue::Number(8.0)),
            }],
        );
        let a = engine()
            .assess(&request(vec![cs], vec![]))
            .expect("assesses");
        assert_eq!(
            a.changes[0].bump,
            VersionBump::Major,
            "3 → 8 more than doubles"
        );

        let dup = engine().assess(&request(
            vec![
                ChangeSet::update(current.clone(), current.clone()),
                ChangeSet::update(current.clone(), current.clone()),
            ],
            vec![],
        ));
        assert!(dup.is_err());
        let nan = engine().assess(&request(
            vec![ChangeSet::update(
                current.clone(),
                current.clone().with_estimate(f64::NAN),
            )],
            vec![],
        ));
        assert!(nan.is_err());
        let mismatch = engine().assess(&request(
            vec![ChangeSet::update(
                current.clone(),
                Entity::new("b", "issue"),
            )],
            vec![],
        ));
        assert!(mismatch.is_err());
    }

    #[test]
    fn comment_blocker_reaches_dependents() {
        let blocker = Entity::new("b", "issue")
            .with_title("Auth API")
            .with_status(StatusCategory::Started, "In Progress")
            .with_relation(RelationKind::Blocks, "d");
        let dependent = Entity::new("d", "issue")
            .with_title("Login page")
            .with_status(StatusCategory::Unstarted, "Todo");
        let cs = ChangeSet::activity(
            "b",
            vec![ActivityEvent::comment(
                "b",
                "We are blocked waiting on the vendor; this will delay the API",
            )],
        );
        let a = engine()
            .assess(&request(vec![cs], vec![blocker, dependent]))
            .expect("assesses");
        assert_eq!(a.changes[0].bump, VersionBump::None);
        assert!(!a.changes[0].signals.is_empty());
        assert!(
            a.impacts
                .iter()
                .any(|i| i.entity_id == "d" && i.score > 0.0)
        );
    }

    #[test]
    fn basis_pins_turn_incompatible_on_major() {
        let parent = Entity::new("p", "issue")
            .with_title("Checkout")
            .with_description("- [ ] Card payments\n- [ ] PayPal")
            .with_version(SemVer::new(1, 3, 0))
            .with_status(StatusCategory::Started, "In Progress");
        let changed = parent.clone().with_description("- [ ] Card payments");
        let kid = Entity::new("k", "sub_issue")
            .with_parent("p")
            .with_title("PayPal button")
            .with_basis("p", SemVer::new(1, 3, 0))
            .with_status(StatusCategory::Unstarted, "Todo");
        let a = engine()
            .assess(&request(
                vec![ChangeSet::update(parent, changed)],
                vec![kid],
            ))
            .expect("assesses");
        assert_eq!(a.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
        let k = a
            .impacts
            .iter()
            .find(|i| i.entity_id == "k")
            .expect("child listed");
        assert_eq!(k.basis_status, BasisStatus::Incompatible);
        assert!(a.level >= ImpactLevel::Medium);
    }

    fn invalid(r: Result<Assessment>) -> bool {
        r.is_err_and(|e| e.kind() == crate::error::ErrorKind::InvalidRequest)
    }

    #[test]
    fn engines_share_their_policy() {
        let a = engine();
        let b = a.clone();
        assert!(
            Arc::ptr_eq(&a.policy, &b.policy),
            "cloning copies a pointer"
        );
        let patched = a
            .with_policy_patch(&serde_json::json!({"thresholds": {"high": 0.4}}))
            .expect("valid patch");
        assert_eq!(patched.policy().thresholds.high, 0.4);
        assert_eq!(a.policy().thresholds.high, 0.5);
    }

    #[test]
    fn compare_text_validates_like_assess() {
        let e = engine();
        let d = e
            .compare_text("must support SSO", "must not support SSO", None)
            .expect("valid");
        assert!(d.negation_flips > 0);
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(e.compare_text(&big, "y", None).is_err());
        assert!(e.compare_text("y", &big, None).is_err());
        assert!(
            e.compare_text("a", "b", Some((&[f32::NAN], &[1.0])))
                .is_err()
        );
        let wide = vec![0.5f32; MAX_EMBEDDING_DIM + 1];
        assert!(e.compare_text("a", "b", Some((&wide, &wide))).is_err());
        assert!(
            e.compare_text("a", "b", Some((&[1.0, 0.0], &[0.0])))
                .is_ok(),
            "different dimensions are ignored, as in assess"
        );
    }

    #[test]
    fn field_changes_and_activity_obey_the_size_limits() {
        let current = Entity::new("a", "issue").with_title("Do it");
        let big = "word ".repeat(MAX_TEXT_BYTES / 5 + 1);
        let delta = ChangeSet::fields(
            current.clone(),
            vec![FieldChange {
                field: "description".into(),
                before: Some(FieldValue::text("short")),
                after: Some(FieldValue::Text(big.clone())),
            }],
        );
        assert!(invalid(engine().assess(&request(vec![delta], vec![]))));
        let reverted = ChangeSet::fields(
            current.clone(),
            vec![FieldChange {
                field: "description".into(),
                before: Some(FieldValue::Text(big.clone())),
                after: None,
            }],
        );
        assert!(invalid(engine().assess(&request(vec![reverted], vec![]))));
        let comment = ChangeSet::activity("a", vec![ActivityEvent::comment("a", big)]);
        assert!(invalid(
            engine().assess(&request(vec![comment], vec![current.clone()]))
        ));
        let chatty = ChangeSet::activity(
            "a",
            (0..=MAX_EVENTS)
                .map(|_| ActivityEvent::comment("a", "hi"))
                .collect(),
        );
        assert!(invalid(
            engine().assess(&request(vec![chatty], vec![current.clone()]))
        ));
        let many = ChangeSet::fields(
            current,
            (0..=MAX_FIELD_CHANGES)
                .map(|i| FieldChange {
                    field: format!("f{i}"),
                    before: None,
                    after: Some(FieldValue::Number(1.0)),
                })
                .collect(),
        );
        assert!(invalid(engine().assess(&request(vec![many], vec![]))));
    }

    #[test]
    fn ordinals_need_a_scale() {
        let e = Entity::new("a", "issue").with_field("priority", FieldValue::ordinal(1, 0));
        let after = e.clone().with_field("priority", FieldValue::ordinal(4, 0));
        assert!(invalid(
            engine().assess(&request(vec![ChangeSet::update(e, after)], vec![]))
        ));
        let e = Entity::new("a", "issue").with_field("priority", FieldValue::ordinal(5, 4));
        assert!(invalid(engine().assess(&request(
            vec![ChangeSet::update(e.clone(), e)],
            vec![]
        ))));
        let ok = Entity::new("a", "issue").with_field("priority", FieldValue::ordinal(1, 4));
        let escalated = ok.clone().with_field("priority", FieldValue::ordinal(4, 4));
        let a = engine()
            .assess(&request(vec![ChangeSet::update(ok, escalated)], vec![]))
            .expect("valid");
        assert!(a.changes[0].fields[0].magnitude > 0.5);
    }

    #[test]
    fn huge_numbers_and_magnitudes_are_rejected_and_output_stays_finite() {
        let p = Entity::new("p", "issue").with_estimate(1e308);
        let changed = p.clone().with_title("Renamed");
        assert!(invalid(
            engine().assess(&request(vec![ChangeSet::update(p, changed)], vec![]))
        ));
        let x = Entity::new("x", "issue")
            .with_title("A")
            .with_estimate(MAX_NUMBER);
        let kid = Entity::new("k", "issue")
            .with_parent("x")
            .with_estimate(MAX_NUMBER);
        let a = engine()
            .assess(&request(
                vec![ChangeSet::update(x.clone(), x.with_title("B"))],
                vec![kid],
            ))
            .expect("bounded numbers are fine");
        let json = serde_json::to_string(&a).expect("serializes");
        assert!(!json.contains("null"), "{json}");
        let back: Assessment = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, a);

        let e = Entity::new("h", "issue").with_title("A");
        let mut req = request(
            vec![ChangeSet::update(e.clone(), e.with_title("B"))],
            vec![],
        );
        req.history = (0..6)
            .map(|d| HistoryPoint {
                entity_id: "h".into(),
                at: now() - chrono::Duration::days(d),
                magnitude: 1e308,
            })
            .collect();
        assert!(invalid(engine().assess(&req)));
        for h in &mut req.history {
            h.magnitude = 0.5;
        }
        let a = engine().assess(&req).expect("valid history");
        assert!(a.changes[0].anomaly_z.is_none_or(f64::is_finite));
    }

    #[test]
    fn resolved_snapshots_follow_the_change_type() {
        let e = Entity::new("a", "issue").with_title("T");
        let ctx = BTreeMap::new();
        let created = resolve(&ChangeSet::create(e.clone()), &ctx).expect("valid");
        assert_eq!(created.change_type(), ChangeType::Created);
        assert!(created.before().is_none());
        let deleted = resolve(&ChangeSet::delete(e.clone()), &ctx).expect("valid");
        assert_eq!(deleted.change_type(), ChangeType::Deleted);
        assert_eq!(deleted.current().id, "a");
        let placeholder = resolve(
            &ChangeSet::activity("b", vec![ActivityEvent::comment("b", "hi")]),
            &ctx,
        )
        .expect("valid");
        assert!(placeholder.placeholder);
        assert_eq!(placeholder.change_type(), ChangeType::Activity);
        assert!(resolve(&ChangeSet::activity("b", vec![]), &ctx).is_err());
    }

    #[test]
    fn scores_are_bounded_and_deterministic() {
        let parent = Entity::new("p", "project").with_title("Q4");
        let issue = Entity::new("i", "issue")
            .with_parent("p")
            .with_title("Search")
            .with_estimate(5.0);
        let bigger = issue
            .clone()
            .with_estimate(13.0)
            .with_title("Search with filters");
        let req = request(vec![ChangeSet::update(issue, bigger)], vec![parent]);
        let a = engine().assess(&req).expect("assesses");
        let b = engine().assess(&req).expect("assesses");
        assert_eq!(a, b);
        for imp in &a.impacts {
            assert!((0.0..=1.0).contains(&imp.score));
        }
        assert!((0.0..=1.0).contains(&a.score));
        assert!(
            a.impacts
                .iter()
                .any(|i| i.entity_id == "p" && i.action == Action::ReEstimate)
        );
    }
}
