//! Field-level scoring: how different two values of one field are
//! (`magnitude`, `[0, 1]`), whether that breaks plans, and how strongly the
//! change travels along each kind of edge.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::assessment::{
    Direction, FieldAssessment, FieldChangeKind, FieldJudgment, JudgmentReason, JudgmentStatus,
};
use crate::model::{
    FieldValue, Ordinal, Status, StatusCategory, TextJudgment, TextRelation, field,
};
use crate::policy::{FieldPolicy, Policy};
use crate::semver::VersionBump;
use crate::stats::{clamp01, saturate};
use crate::text::{self, Analysis, Corpus, TextDelta};

/// How strongly a change travels along each kind of edge (before relevance and decay).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Channels {
    pub down: f64,
    pub up: f64,
    pub dependency: f64,
    pub upstream: f64,
    pub related: f64,
    pub sibling: f64,
}

impl Channels {
    pub(crate) fn from_policy(p: &FieldPolicy) -> Self {
        Self {
            down: p.down,
            up: p.up,
            dependency: p.dependency,
            upstream: p.upstream,
            related: p.related,
            sibling: p.sibling,
        }
    }

    fn scaled(self, down: f64, up: f64, dependency: f64, upstream: f64) -> Self {
        Self {
            down: self.down * down,
            up: self.up * up,
            dependency: self.dependency * dependency,
            upstream: self.upstream * upstream,
            ..self
        }
    }
}

/// What kind of move a status change is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Into canceled.
    Cancel,
    /// Out of canceled.
    Revive,
    /// Out of completed (not into canceled).
    Reopen,
    /// Into completed.
    Complete,
    /// Forward among backlog → unstarted → started.
    Progress,
    /// Backward among them.
    Regress,
    /// Same category, another state name.
    Lateral,
}

impl Transition {
    pub(crate) fn of(from: StatusCategory, to: StatusCategory) -> Self {
        use StatusCategory::*;
        match (from, to) {
            (f, t) if f == t => Transition::Lateral,
            (_, Canceled) => Transition::Cancel,
            (Canceled, _) => Transition::Revive,
            (Completed, _) => Transition::Reopen,
            (_, Completed) => Transition::Complete,
            (f, t) if t.index() > f.index() => Transition::Progress,
            _ => Transition::Regress,
        }
    }

    /// Channel multipliers `(down, up, dependency, upstream)`.
    fn channel_factors(self) -> (f64, f64, f64, f64) {
        match self {
            // Up is high because a cancellation also takes the entity's effort
            // out of its parent (scored as a roll-up, like a deletion).
            Transition::Cancel => (1.0, 0.8, 0.8, 0.3),
            Transition::Reopen => (0.6, 0.7, 0.8, 0.2),
            Transition::Regress => (0.7, 0.4, 0.6, 0.2),
            Transition::Revive => (0.5, 0.4, 0.4, 0.2),
            Transition::Complete => (0.2, 0.15, 0.1, 0.0),
            Transition::Progress => (0.1, 0.05, 0.05, 0.0),
            Transition::Lateral => (0.05, 0.02, 0.02, 0.0),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Transition::Cancel => "canceled",
            Transition::Revive => "revived",
            Transition::Reopen => "reopened",
            Transition::Complete => "completed",
            Transition::Progress => "progressed",
            Transition::Regress => "moved back",
            Transition::Lateral => "moved within its category",
        }
    }
}

/// Whether an entity was created or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleKind {
    Created,
    Deleted,
}

impl LifecycleKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LifecycleKind::Created => "was created",
            LifecycleKind::Deleted => "was deleted",
        }
    }
}

/// Type-specific facts propagation needs about a scored field.
#[derive(Debug, Clone, Default)]
pub(crate) enum Detail {
    #[default]
    None,
    Text(TextDelta),
    /// Comments: what they say, and whether they report a blocker or a slip
    /// (`schedule`) or a scope change (`scope`).
    Activity {
        delta: TextDelta,
        schedule: bool,
        scope: bool,
    },
    Estimate {
        delta: f64,
    },
    /// The entity was created or deleted, with its estimate.
    Lifecycle {
        kind: LifecycleKind,
        estimate: Option<f64>,
    },
    Status(Transition),
    Date {
        before: Option<DateTime<Utc>>,
        after: Option<DateTime<Utc>>,
    },
    Reference {
        before: Option<String>,
        after: Option<String>,
    },
}

/// A scored field change.
#[derive(Debug, Clone)]
pub(crate) struct Scored {
    pub assessment: FieldAssessment,
    pub channels: Channels,
    pub detail: Detail,
    /// How much the engine trusts this field's magnitude.
    pub confidence: f64,
    /// The largest bump this change can cause (the field's cap, lowered to
    /// `minor` for a text change that only adds).
    pub max_bump: VersionBump,
    /// How much of this field's risk counts towards the content score and
    /// travels (`1`; less when another field of the same change already
    /// carries the same evidence, e.g. a rename mirrored in title and
    /// description).
    pub weight: f64,
    /// For text: the terms whose count changed (after typo alignment).
    pub changed_terms: BTreeSet<String>,
}

/// What field scoring may look at besides the two values.
pub(crate) struct FieldContext<'a> {
    pub policy: &'a Policy,
    pub now: DateTime<Utc>,
    pub corpus: &'a Corpus,
    /// The policy's word lexicon, built once per assessment.
    pub lexicon: &'a text::Lexicon<'a>,
    /// The entity's embeddings before and after, when both exist.
    pub embeddings: Option<(&'a [f32], &'a [f32])>,
    /// The supplied judgments of this change, by field.
    pub judgments: &'a BTreeMap<&'a str, &'a TextJudgment>,
}

/// No judgments, for a change that carries none.
#[cfg(test)]
pub(crate) static NO_JUDGMENTS: BTreeMap<&str, &TextJudgment> = BTreeMap::new();

fn numbers_equal(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Whether two values are the same for assessment purposes.
pub(crate) fn same_value(a: &FieldValue, b: &FieldValue) -> bool {
    match (a, b) {
        (FieldValue::Number(x), FieldValue::Number(y)) => numbers_equal(*x, *y),
        _ => a == b,
    }
}

/// Scores one field. `None` when the values are the same (or both absent).
pub(crate) fn score(
    name: &str,
    before: Option<&FieldValue>,
    after: Option<&FieldValue>,
    ctx: &FieldContext<'_>,
) -> Option<Scored> {
    let policy = ctx.policy.field(name);
    let change = match (before, after) {
        (None, None) => return None,
        (Some(a), Some(b)) if same_value(a, b) => return None,
        (None, Some(_)) => FieldChangeKind::Added,
        (Some(_), None) => FieldChangeKind::Removed,
        (Some(_), Some(_)) => FieldChangeKind::Modified,
    };
    let mut out = Partial::new(change);
    match (before, after) {
        (Some(FieldValue::Text(a)), Some(FieldValue::Text(b))) => out.text(name, a, b, ctx),
        (None, Some(FieldValue::Text(b))) => out.text(name, "", b, ctx),
        (Some(FieldValue::Text(a)), None) => out.text(name, a, "", ctx),
        (Some(FieldValue::Number(a)), Some(FieldValue::Number(b))) => {
            out.number(*a, *b, ctx.policy)
        }
        (None, Some(FieldValue::Number(b))) => out.number_set(*b, true, ctx.policy),
        (Some(FieldValue::Number(a)), None) => out.number_set(*a, false, ctx.policy),
        (Some(FieldValue::Status(a)), Some(FieldValue::Status(b))) => out.status(a, b, ctx.policy),
        (None, Some(FieldValue::Status(b))) => out.status_set(b, true),
        (Some(FieldValue::Status(a)), None) => out.status_set(a, false),
        (Some(FieldValue::Ordinal(a)), Some(FieldValue::Ordinal(b))) => {
            out.ordinal(*a, *b, ctx.policy)
        }
        (None, Some(FieldValue::Ordinal(b))) => out.ordinal(
            Ordinal {
                rank: 0,
                scale: b.scale,
            },
            *b,
            ctx.policy,
        ),
        (Some(FieldValue::Ordinal(a)), None) => out.ordinal(
            *a,
            Ordinal {
                rank: 0,
                scale: a.scale,
            },
            ctx.policy,
        ),
        (Some(FieldValue::Timestamp(a)), Some(FieldValue::Timestamp(b))) => {
            out.date(Some(*a), Some(*b), ctx)
        }
        (None, Some(FieldValue::Timestamp(b))) => out.date(None, Some(*b), ctx),
        (Some(FieldValue::Timestamp(a)), None) => out.date(Some(*a), None, ctx),
        (
            a @ (None | Some(FieldValue::Reference(_))),
            b @ (None | Some(FieldValue::Reference(_))),
        ) => {
            let r = |v: Option<&FieldValue>| match v {
                Some(FieldValue::Reference(s)) => Some(s.clone()),
                _ => None,
            };
            out.reference(r(a), r(b), ctx.policy);
        }
        (a @ (None | Some(FieldValue::Set(_))), b @ (None | Some(FieldValue::Set(_)))) => {
            let s = |v: Option<&FieldValue>| match v {
                Some(FieldValue::Set(s)) => s.clone(),
                _ => BTreeSet::new(),
            };
            out.set(&s(a), &s(b), ctx.policy);
        }
        (a @ (None | Some(FieldValue::Bool(_))), b @ (None | Some(FieldValue::Bool(_)))) => {
            out.magnitude = 1.0;
            out.explanation = format!(
                "flag changed from {} to {}",
                a.map_or("unset".to_owned(), FieldValue::display),
                b.map_or("unset".to_owned(), FieldValue::display)
            );
        }
        (a, b) => {
            out.magnitude = 1.0;
            out.explanation = format!(
                "value type changed from {} to {}",
                a.map_or("unset", FieldValue::type_name),
                b.map_or("unset", FieldValue::type_name)
            );
        }
    }

    let mut severity = policy.severity;
    if let Some(min) = out.severity_floor {
        severity = severity.max(min);
    }
    let magnitude = clamp01(out.magnitude);
    let risk = clamp01(severity * magnitude);
    let max_bump = policy.max_bump.min(out.max_bump);
    let breaking =
        max_bump >= VersionBump::Major && (out.breaking || magnitude >= policy.breaking_magnitude);
    let mut channels = Channels::from_policy(policy);
    if let Detail::Status(t) = &out.detail {
        let (d, u, dep, up) = t.channel_factors();
        channels = channels.scaled(d, u, dep, up);
    }
    let explanation = if out.explanation.is_empty() {
        format!("{name} changed")
    } else {
        format!("{name}: {}", out.explanation)
    };
    Some(Scored {
        assessment: FieldAssessment {
            field: name.to_owned(),
            change,
            direction: out.direction,
            magnitude,
            severity,
            risk,
            breaking,
            additive: out.additive && !breaking && max_bump >= VersionBump::Minor,
            before: before.map(FieldValue::display),
            after: after.map(FieldValue::display),
            metrics: out.metrics,
            explanation,
            judgment: out.judgment,
        },
        channels,
        detail: out.detail,
        confidence: out.confidence,
        max_bump,
        weight: 1.0,
        changed_terms: out.changed_terms,
    })
}

/// A field assessment under construction.
struct Partial {
    change: FieldChangeKind,
    magnitude: f64,
    direction: Direction,
    breaking: bool,
    additive: bool,
    severity_floor: Option<f64>,
    metrics: BTreeMap<String, f64>,
    explanation: String,
    detail: Detail,
    confidence: f64,
    max_bump: VersionBump,
    changed_terms: BTreeSet<String>,
    judgment: Option<FieldJudgment>,
}

impl Partial {
    fn new(change: FieldChangeKind) -> Self {
        Self {
            change,
            magnitude: 0.0,
            direction: Direction::None,
            breaking: false,
            additive: false,
            severity_floor: None,
            metrics: BTreeMap::new(),
            explanation: String::new(),
            detail: Detail::None,
            confidence: 1.0,
            max_bump: VersionBump::Major,
            changed_terms: BTreeSet::new(),
            judgment: None,
        }
    }

    fn metric(&mut self, name: &str, value: f64) {
        if value.is_finite() {
            self.metrics.insert(name.to_owned(), value);
        }
    }

    fn text(&mut self, name: &str, a: &str, b: &str, ctx: &FieldContext<'_>) {
        let pa: Analysis = text::analyze_with(a, ctx.lexicon);
        let mut pb: Analysis = text::analyze_with(b, ctx.lexicon);
        text::align_typos(&pa, &mut pb, ctx.lexicon);
        // The field's own magnitude (and so the version bump) is measured
        // against the two texts alone, exactly as `Engine::compare_text`
        // does: it must not depend on how much context the caller sent. The
        // neighbourhood corpus only weighs the passages that propagate.
        let mut own = Corpus::new();
        own.add_terms(pa.counts.keys());
        own.add_terms(pb.counts.keys());
        let (diff, mut tparts) =
            text::compare_parts(&pa, &pb, &ctx.policy.text, &own, ctx.embeddings);
        for (t, c) in &pa.counts {
            if pb.counts.get(t) != Some(c) {
                self.changed_terms.insert(t.clone());
            }
        }
        for t in pb.counts.keys() {
            if !pa.counts.contains_key(t) {
                self.changed_terms.insert(t.clone());
            }
        }
        self.magnitude = diff.magnitude;
        self.metric("lexical_similarity", diff.lexical_similarity);
        self.metric("tfidf_cosine", diff.tfidf_cosine);
        self.metric("shingle_similarity", diff.shingle_similarity);
        self.metric("token_jaccard", diff.token_jaccard);
        if let Some(cs) = diff.char_similarity {
            self.metric("char_similarity", cs);
        }
        if let Some(ec) = diff.embedding_cosine {
            self.metric("embedding_cosine", ec);
        }
        let s = diff.structure;
        self.metric("items_added", f64::from(s.items_added));
        self.metric("items_removed", f64::from(s.items_removed));
        self.metric("items_modified", f64::from(s.items_modified));
        self.metric("items_checked", f64::from(s.items_checked));
        self.metric("negation_flips", f64::from(diff.negation_flips));
        self.metric("modal_shifts", f64::from(diff.modal_shifts));
        self.metric("numbers_changed", f64::from(diff.numbers_changed));
        self.breaking = s.items_removed > 0 || diff.negation_flips > 0;
        // Nothing planned against the old text went away (no passage or list
        // item removed or reworded, no negation or requirement-strength
        // change): however large, the change can only add compatibly.
        let nothing_lost = diff.nothing_lost();
        if nothing_lost {
            self.max_bump = VersionBump::Minor;
        }
        self.additive =
            s.items_added > 0 || s.headings_added > 0 || (nothing_lost && diff.passages_added > 0);
        self.metric("passages_added", f64::from(diff.passages_added));
        self.metric("passages_removed", f64::from(diff.passages_removed));
        self.metric("passages_modified", f64::from(diff.passages_modified));
        self.direction = match diff.chars_after.cmp(&diff.chars_before) {
            std::cmp::Ordering::Greater => Direction::Increase,
            std::cmp::Ordering::Less => Direction::Decrease,
            std::cmp::Ordering::Equal => Direction::None,
        };
        self.confidence = if diff.embedding_cosine.is_some() {
            1.0
        } else if diff.low_information {
            0.7
        } else {
            0.9
        };
        let mut parts = Vec::new();
        match self.change {
            FieldChangeKind::Added => parts.push("set".to_owned()),
            FieldChangeKind::Removed => parts.push("cleared".to_owned()),
            FieldChangeKind::Modified => {
                parts.push(format!("change magnitude {:.2}", diff.magnitude));
            }
        }
        // A judgment of the meaning, when the field is flagged or one came.
        let mut delta = text::text_delta(&pa, &pb, ctx.corpus, ctx.embeddings);
        let judge = &ctx.policy.judge;
        let eligible = judge.enabled
            && self.change == FieldChangeKind::Modified
            && judge.fields.contains(name);
        let reasons = if eligible {
            let bm = ctx.policy.field(name).breaking_magnitude;
            text::gray_zone(&pa, &pb, &diff, &mut tparts, judge, bm)
        } else {
            Vec::new()
        };
        let supplied = if eligible {
            ctx.judgments.get(name).copied()
        } else {
            None
        };
        let mut unverified: Option<String> = None;
        if !reasons.is_empty() || supplied.is_some() {
            let flagged = !reasons.is_empty();
            let lexical = diff.magnitude;
            let fingerprint = text::fingerprint(a, b);
            let mut status = JudgmentStatus::Requested;
            let mut weight = None;
            if let Some(j) = supplied {
                let c0 = judge.threshold(j.relation);
                let w = if j.relation == TextRelation::Undetermined {
                    0.0
                } else {
                    clamp01((j.confidence - c0) / (1.0 - c0))
                };
                status = if j.fingerprint != fingerprint {
                    JudgmentStatus::Stale
                } else if !flagged && !judge.apply_unflagged {
                    JudgmentStatus::NotFlagged
                } else if w > 0.0 {
                    JudgmentStatus::Applied
                } else {
                    JudgmentStatus::Inconclusive
                };
                if status == JudgmentStatus::Applied {
                    weight = Some(w);
                    let shares = self.apply_judgment(j.relation, w, &diff, &tparts, ctx);
                    delta.set_shares(shares);
                    if j.relation.is_raising() && w >= 0.5 {
                        delta.add_modified(text::scope_passages(&pa, &pb, &tparts, ctx.corpus));
                    }
                    self.metric("lexical_magnitude", lexical);
                    self.metric("judgment_weight", w);
                    parts[0] = format!(
                        "change magnitude {:.2} (lexically {lexical:.2}; judged {}, confidence {:.2})",
                        self.magnitude,
                        j.relation.as_str(),
                        j.confidence
                    );
                    let floor = if flagged {
                        judge.unjudged_confidence * self.confidence
                    } else {
                        self.confidence
                    };
                    self.confidence = floor + w * (j.confidence - floor).max(0.0);
                }
            }
            if flagged && status != JudgmentStatus::Applied {
                self.confidence *= judge.unjudged_confidence;
                unverified = Some(format!(
                    "meaning not verified: {}",
                    reasons
                        .iter()
                        .map(|r| JudgmentReason::label(*r))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            self.judgment = Some(FieldJudgment {
                status,
                reasons,
                fingerprint,
                lexical_magnitude: lexical,
                relation: supplied.map(|j| j.relation),
                confidence: supplied.map(|j| j.confidence),
                weight,
                rationale: supplied.map(|j| j.rationale.clone()).unwrap_or_default(),
                source: supplied.map(|j| j.source.clone()).unwrap_or_default(),
                detail: String::new(),
            });
        }
        if s.items_added + s.items_removed + s.items_modified > 0 {
            parts.push(format!(
                "list items +{} −{} ~{}",
                s.items_added, s.items_removed, s.items_modified
            ));
        }
        if diff.negation_flips > 0 {
            parts.push(format!("{} negation flip(s)", diff.negation_flips));
        }
        if diff.modal_shifts > 0 {
            parts.push(format!(
                "{} requirement-strength change(s)",
                diff.modal_shifts
            ));
        }
        if diff.numbers_changed > 0 {
            parts.push(format!("{} number(s) changed", diff.numbers_changed));
        }
        if !diff.removed_terms.is_empty() && self.change == FieldChangeKind::Modified {
            parts.push(format!(
                "dropped: {}",
                diff.removed_terms
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !diff.added_terms.is_empty() && self.change == FieldChangeKind::Modified {
            parts.push(format!(
                "added: {}",
                diff.added_terms
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.extend(unverified);
        self.explanation = parts.join("; ");
        self.detail = Detail::Text(delta);
    }

    /// Applies a judgment of weight `w > 0` to a scored text change: the
    /// magnitude, the bump cap and the breaking and additive marks. Returns
    /// the `(removed, changed)` shares it leaves of the owner floors.
    fn apply_judgment(
        &mut self,
        relation: TextRelation,
        w: f64,
        diff: &text::TextDiff,
        tparts: &text::Parts,
        ctx: &FieldContext<'_>,
    ) -> (f64, f64) {
        let judge = &ctx.policy.judge;
        let s = diff.structure;
        let decisive = w >= 0.5;
        // Evidence a reader cannot argue away.
        let block = s.items_removed > 0
            || diff.negation_flips > 0
            || diff.modal_shifts > 0
            || diff.numbers_changed > 0;
        let soft = s.items_added > 0
            || s.items_modified > 0
            || s.headings_added > 0
            || s.headings_removed > 0;
        let base = tparts.base();
        let left = (1.0 - (2.0 * w).min(1.0)).max(judge.min_owner_share);
        // Such evidence blocks every lowering: the magnitude, the bump cap
        // and the shares all stay as the engine scored them.
        if block && !relation.is_raising() {
            return (1.0, 1.0);
        }
        match relation {
            TextRelation::Undetermined => (1.0, 1.0),
            TextRelation::Equivalent => {
                let lowered = base - w * (base - judge.equivalent_target).max(0.0);
                self.magnitude = text::rejudge(tparts, lowered, &ctx.policy.text);
                if decisive {
                    // What the engine saw added stays at least Minor.
                    self.max_bump = self.max_bump.min(if soft || self.additive {
                        VersionBump::Minor
                    } else {
                        VersionBump::Patch
                    });
                }
                (left, left)
            }
            TextRelation::Extended => {
                let moved = base + w * (judge.extended_target - base);
                self.magnitude = text::rejudge(tparts, moved, &ctx.policy.text);
                if decisive {
                    self.max_bump = self.max_bump.min(VersionBump::Minor);
                    self.additive = true;
                }
                (left, 1.0)
            }
            TextRelation::Narrowed | TextRelation::Replaced | TextRelation::Reversed => {
                let e = judge.evidence(relation);
                self.magnitude = 1.0 - (1.0 - self.magnitude) * (1.0 - w * e);
                if decisive {
                    self.breaking = true;
                    self.max_bump = VersionBump::Major;
                }
                (1.0, 1.0)
            }
        }
    }

    fn number(&mut self, a: f64, b: f64, policy: &Policy) {
        let np = &policy.number;
        let magnitude = if a < 0.0 || b < 0.0 {
            let rel = (b - a).abs() / a.abs().max(b.abs()).max(1.0);
            self.metric("relative_change", rel);
            clamp01(rel)
        } else {
            let ratio = (b + np.smoothing) / (a + np.smoothing);
            let log_ratio = ratio.ln().abs();
            self.metric("ratio", ratio);
            self.metric("log_ratio", log_ratio);
            saturate(log_ratio, np.log_scale)
        };
        self.direction = if b > a {
            Direction::Increase
        } else {
            Direction::Decrease
        };
        self.magnitude = if b < a {
            magnitude * np.decrease_factor
        } else {
            magnitude
        };
        self.metric("delta", b - a);
        self.explanation = format!(
            "{} from {} to {}",
            if b > a { "increased" } else { "decreased" },
            FieldValue::Number(a).display(),
            FieldValue::Number(b).display()
        );
        self.detail = Detail::Estimate { delta: b - a };
    }

    fn number_set(&mut self, v: f64, set: bool, policy: &Policy) {
        self.magnitude = if set {
            policy.number.set_magnitude
        } else {
            policy.number.cleared_magnitude
        };
        self.direction = if set {
            Direction::Increase
        } else {
            Direction::Decrease
        };
        self.explanation = if set {
            format!("set to {}", FieldValue::Number(v).display())
        } else {
            format!("cleared (was {})", FieldValue::Number(v).display())
        };
        let delta = if set { v } else { -v };
        self.metric("delta", delta);
        self.detail = Detail::Estimate { delta };
    }

    fn status(&mut self, a: &Status, b: &Status, policy: &Policy) {
        let t = Transition::of(a.category, b.category);
        self.magnitude = if t == Transition::Lateral {
            0.05
        } else {
            policy.status_transitions[a.category.index()][b.category.index()]
        };
        self.direction = match b.category.index().cmp(&a.category.index()) {
            std::cmp::Ordering::Greater => Direction::Increase,
            std::cmp::Ordering::Less => Direction::Decrease,
            std::cmp::Ordering::Equal => Direction::None,
        };
        self.explanation = format!(
            "{} ({} → {})",
            t.as_str(),
            FieldValue::Status(a.clone()).display(),
            FieldValue::Status(b.clone()).display()
        );
        self.metric("transition_risk", self.magnitude);
        self.detail = Detail::Status(t);
    }

    fn status_set(&mut self, s: &Status, set: bool) {
        self.magnitude = if set { 0.1 } else { 0.5 };
        self.explanation = if set {
            format!("set to {}", FieldValue::Status(s.clone()).display())
        } else {
            "cleared".to_owned()
        };
    }

    fn ordinal(&mut self, a: Ordinal, b: Ordinal, policy: &Policy) {
        let (x, y) = (a.normalized(), b.normalized());
        let delta = y - x;
        let factor = if delta > 0.0 {
            policy.ordinal.escalation_factor
        } else {
            policy.ordinal.deescalation_factor
        };
        self.magnitude = clamp01(delta.abs() * factor);
        self.direction = if delta > 0.0 {
            Direction::Increase
        } else {
            Direction::Decrease
        };
        self.metric("delta", delta);
        self.explanation = format!(
            "{} ({}/{} → {}/{})",
            if delta > 0.0 {
                "escalated"
            } else {
                "de-escalated"
            },
            a.rank,
            a.scale,
            b.rank,
            b.scale
        );
    }

    fn date(&mut self, a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>, ctx: &FieldContext<'_>) {
        let dp = &ctx.policy.date;
        self.detail = Detail::Date {
            before: a,
            after: b,
        };
        match (a, b) {
            (Some(a), Some(b)) => {
                let days_from_now =
                    |d: DateTime<Utc>| (d - ctx.now).num_seconds() as f64 / 86_400.0;
                let shift_days = (b - a).num_seconds() as f64 / 86_400.0;
                // Measured against what was left of the old window, but never
                // less than the horizon: a one-day correction of an imminent
                // or overdue date is not a breaking compression.
                let window_days = days_from_now(a).max(dp.min_window_days);
                let push_out =
                    |rho: f64| dp.push_out_factor * saturate(rho, 1.0 / dp.push_out_rate);
                self.metric("shift_days", shift_days);
                self.metric("window_days", window_days);
                self.direction = if shift_days < 0.0 {
                    Direction::Decrease
                } else {
                    Direction::Increase
                };
                if a < ctx.now && b < ctx.now {
                    // Both dates are already missed: bookkeeping on overdue work.
                    let rho = shift_days.abs() / window_days;
                    self.metric("compression", rho);
                    self.magnitude = dp.overdue_factor * push_out(rho);
                    self.explanation = format!(
                        "moved by {:.1} days; both dates are already past",
                        shift_days
                    );
                } else {
                    // A date already missed counts from today: only the part of
                    // the move beyond now changes anyone's schedule.
                    let effective = days_from_now(b).max(0.0) - days_from_now(a).max(0.0);
                    let rho = effective.abs() / window_days;
                    self.metric("effective_shift_days", effective);
                    self.metric("compression", rho);
                    if effective < 0.0 {
                        self.magnitude = saturate(rho, 1.0 / dp.pull_in_rate);
                        self.explanation = format!(
                            "pulled in by {:.1} days ({:.0}% of a {:.1}-day window)",
                            -effective,
                            rho * 100.0,
                            window_days
                        );
                    } else {
                        self.magnitude = push_out(rho);
                        self.explanation = format!("pushed out by {effective:.1} days");
                    }
                }
                if b < ctx.now {
                    self.metric("now_overdue", 1.0);
                }
            }
            (None, Some(b)) => {
                self.magnitude = dp.set_magnitude;
                self.explanation = format!("set to {}", b.format("%Y-%m-%d"));
            }
            (Some(a), None) => {
                self.magnitude = dp.cleared_magnitude;
                self.explanation = format!("cleared (was {})", a.format("%Y-%m-%d"));
            }
            (None, None) => {}
        }
    }

    fn reference(&mut self, a: Option<String>, b: Option<String>, policy: &Policy) {
        let rp = &policy.reference;
        self.magnitude = match (&a, &b) {
            (Some(_), Some(_)) => rp.reassign,
            (None, Some(_)) => rp.assign,
            (Some(_), None) => rp.unassign,
            (None, None) => 0.0,
        };
        self.explanation = match (&a, &b) {
            (Some(x), Some(y)) => format!("moved from {x} to {y}"),
            (None, Some(y)) => format!("set to {y}"),
            (Some(x), None) => format!("cleared (was {x})"),
            (None, None) => String::new(),
        };
        self.detail = Detail::Reference {
            before: a,
            after: b,
        };
    }

    fn set(&mut self, a: &BTreeSet<String>, b: &BTreeSet<String>, policy: &Policy) {
        let added: Vec<&String> = b.difference(a).collect();
        let removed: Vec<&String> = a.difference(b).collect();
        let union = a.union(b).count() as f64;
        let inter = a.intersection(b).count() as f64;
        let distance = if union == 0.0 {
            0.0
        } else {
            1.0 - inter / union
        };
        self.metric("jaccard_distance", distance);
        self.metric("added", added.len() as f64);
        self.metric("removed", removed.len() as f64);
        let listed = |list: &BTreeSet<String>, label: &str| {
            list.iter().any(|s| s.eq_ignore_ascii_case(label))
        };
        // Removing a resolution-type label (`blocked`, `incident`) is good
        // news, so only its addition is significant.
        let significant: Vec<&String> = added
            .iter()
            .copied()
            .filter(|l| {
                listed(&policy.set.significant, l) || listed(&policy.set.significant_on_add, l)
            })
            .chain(
                removed
                    .iter()
                    .copied()
                    .filter(|l| listed(&policy.set.significant, l)),
            )
            .collect();
        self.magnitude = distance;
        if !significant.is_empty() {
            self.magnitude = 1.0;
            self.severity_floor = Some(policy.set.significant_severity);
            self.metric("significant", significant.len() as f64);
        }
        self.direction = match added.len().cmp(&removed.len()) {
            std::cmp::Ordering::Greater => Direction::Increase,
            std::cmp::Ordering::Less => Direction::Decrease,
            std::cmp::Ordering::Equal => Direction::None,
        };
        let mut parts = Vec::new();
        if !added.is_empty() {
            parts.push(format!(
                "+{}",
                added
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", +")
            ));
        }
        if !removed.is_empty() {
            parts.push(format!(
                "−{}",
                removed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", −")
            ));
        }
        if !significant.is_empty() {
            parts.push("touches a significant label".to_owned());
        }
        self.explanation = parts.join("; ");
    }
}

/// The synthetic `parent` field for a re-parenting.
pub(crate) fn reparent(
    before: Option<&str>,
    after: Option<&str>,
    policy: &Policy,
) -> Option<Scored> {
    if before == after {
        return None;
    }
    let p = policy.field(field::PARENT);
    let (change, magnitude, explanation) = match (before, after) {
        (Some(x), Some(y)) => (
            FieldChangeKind::Modified,
            1.0,
            format!("parent: moved from {x} to {y}"),
        ),
        (None, Some(y)) => (
            FieldChangeKind::Added,
            0.5,
            format!("parent: attached under {y}"),
        ),
        (Some(x), None) => (
            FieldChangeKind::Removed,
            0.6,
            format!("parent: detached from {x}"),
        ),
        (None, None) => return None,
    };
    let breaking = p.max_bump >= VersionBump::Major && magnitude >= p.breaking_magnitude;
    Some(Scored {
        assessment: FieldAssessment {
            field: field::PARENT.to_owned(),
            change,
            direction: Direction::None,
            magnitude,
            severity: p.severity,
            risk: clamp01(p.severity * magnitude),
            breaking,
            additive: false,
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
            metrics: BTreeMap::new(),
            explanation,
            judgment: None,
        },
        channels: Channels::from_policy(p),
        detail: Detail::Reference {
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
        },
        confidence: 1.0,
        max_bump: p.max_bump,
        weight: 1.0,
        changed_terms: BTreeSet::new(),
    })
}

/// The synthetic `lifecycle` field for a creation or a deletion.
pub(crate) fn lifecycle(created: bool, estimate: Option<f64>, policy: &Policy) -> Scored {
    let p = if created {
        &policy.lifecycle.create
    } else {
        &policy.lifecycle.delete
    };
    let mut metrics = BTreeMap::new();
    if let Some(e) = estimate {
        metrics.insert("estimate".to_owned(), e);
    }
    Scored {
        assessment: FieldAssessment {
            field: field::LIFECYCLE.to_owned(),
            change: if created {
                FieldChangeKind::Added
            } else {
                FieldChangeKind::Removed
            },
            direction: if created {
                Direction::Increase
            } else {
                Direction::Decrease
            },
            magnitude: 1.0,
            severity: p.severity,
            risk: clamp01(p.severity),
            breaking: !created && p.breaking_magnitude <= 1.0 && p.max_bump >= VersionBump::Major,
            additive: created,
            before: None,
            after: None,
            metrics,
            explanation: if created {
                "lifecycle: created".to_owned()
            } else {
                "lifecycle: deleted".to_owned()
            },
            judgment: None,
        },
        channels: Channels::from_policy(p),
        detail: Detail::Lifecycle {
            kind: if created {
                LifecycleKind::Created
            } else {
                LifecycleKind::Deleted
            },
            estimate,
        },
        confidence: 1.0,
        max_bump: p.max_bump,
        weight: 1.0,
        changed_terms: BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx<'a>(
        policy: &'a Policy,
        corpus: &'a Corpus,
        lexicon: &'a text::Lexicon<'a>,
    ) -> FieldContext<'a> {
        FieldContext {
            policy,
            now: Utc
                .with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
                .single()
                .expect("valid date"),
            corpus,
            lexicon,
            embeddings: None,
            judgments: &NO_JUDGMENTS,
        }
    }

    fn sc(name: &str, a: Option<FieldValue>, b: Option<FieldValue>) -> Option<Scored> {
        let policy = Policy::default();
        let corpus = Corpus::new();
        let lexicon = text::Lexicon::new(&policy.text);
        score(
            name,
            a.as_ref(),
            b.as_ref(),
            &ctx(&policy, &corpus, &lexicon),
        )
    }

    #[test]
    fn unchanged_is_none() {
        assert!(
            sc(
                "estimate",
                Some(FieldValue::Number(3.0)),
                Some(FieldValue::Number(3.0 + 1e-12))
            )
            .is_none()
        );
        assert!(sc("title", None, None).is_none());
    }

    #[test]
    fn estimate_doubling_is_breaking_and_monotone() {
        let double = sc(
            "estimate",
            Some(FieldValue::Number(3.0)),
            Some(FieldValue::Number(7.0)),
        )
        .expect("changed");
        assert!(double.assessment.breaking);
        let bit = sc(
            "estimate",
            Some(FieldValue::Number(3.0)),
            Some(FieldValue::Number(4.0)),
        )
        .expect("changed");
        assert!(!bit.assessment.breaking);
        assert!(bit.assessment.magnitude < double.assessment.magnitude);
        let down = sc(
            "estimate",
            Some(FieldValue::Number(4.0)),
            Some(FieldValue::Number(3.0)),
        )
        .expect("changed");
        assert!(
            down.assessment.magnitude < bit.assessment.magnitude,
            "decreases weigh less"
        );
    }

    #[test]
    fn status_transitions() {
        let cancel = sc(
            "status",
            Some(FieldValue::status(StatusCategory::Started, "In Progress")),
            Some(FieldValue::status(StatusCategory::Canceled, "Canceled")),
        )
        .expect("changed");
        assert!(cancel.assessment.breaking);
        assert!(cancel.channels.down > 0.9);
        let progress = sc(
            "status",
            Some(FieldValue::status(StatusCategory::Unstarted, "Todo")),
            Some(FieldValue::status(StatusCategory::Started, "In Progress")),
        )
        .expect("changed");
        assert!(!progress.assessment.breaking);
        assert!(progress.assessment.risk < 0.1);
        let lateral = sc(
            "status",
            Some(FieldValue::status(StatusCategory::Started, "In Progress")),
            Some(FieldValue::status(StatusCategory::Started, "In Review")),
        )
        .expect("changed");
        assert!(lateral.assessment.magnitude <= 0.05);
        let reopen = sc(
            "status",
            Some(FieldValue::status(StatusCategory::Completed, "Done")),
            Some(FieldValue::status(StatusCategory::Started, "In Progress")),
        )
        .expect("changed");
        assert!(reopen.assessment.breaking);
    }

    #[test]
    fn dates_pull_in_hurts_more_than_push_out() {
        let d = |y, m, d| {
            FieldValue::Timestamp(
                Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
                    .single()
                    .expect("valid"),
            )
        };
        let pull = sc("due_date", Some(d(2026, 11, 21)), Some(d(2026, 11, 11))).expect("changed");
        let push = sc("due_date", Some(d(2026, 11, 21)), Some(d(2026, 12, 1))).expect("changed");
        assert!(pull.assessment.magnitude > push.assessment.magnitude);
        assert_eq!(pull.assessment.direction, Direction::Decrease);
    }

    #[test]
    fn references_and_sets() {
        let re = sc(
            "assignee",
            Some(FieldValue::reference("u1")),
            Some(FieldValue::reference("u2")),
        )
        .expect("changed");
        assert_eq!(re.assessment.magnitude, 1.0);
        let un = sc("assignee", Some(FieldValue::reference("u1")), None).expect("changed");
        assert!(
            un.assessment.magnitude
                > sc("assignee", None, Some(FieldValue::reference("u1")))
                    .expect("changed")
                    .assessment
                    .magnitude
        );
        let labels = sc(
            "labels",
            Some(FieldValue::set(["ui"])),
            Some(FieldValue::set(["ui", "polish"])),
        )
        .expect("changed");
        assert!(labels.assessment.risk < 0.15);
        let security = sc(
            "labels",
            Some(FieldValue::set(["ui"])),
            Some(FieldValue::set(["ui", "Security"])),
        )
        .expect("changed");
        assert!(security.assessment.risk >= 0.5);
    }

    #[test]
    fn priority_escalation() {
        let up = sc(
            "priority",
            Some(FieldValue::ordinal(1, 4)),
            Some(FieldValue::ordinal(4, 4)),
        )
        .expect("changed");
        let down = sc(
            "priority",
            Some(FieldValue::ordinal(4, 4)),
            Some(FieldValue::ordinal(1, 4)),
        )
        .expect("changed");
        assert!(up.assessment.magnitude > down.assessment.magnitude);
        assert!(up.assessment.breaking);
    }

    #[test]
    fn type_change_is_full_magnitude() {
        let s = sc(
            "custom",
            Some(FieldValue::text("x")),
            Some(FieldValue::Number(1.0)),
        )
        .expect("changed");
        assert_eq!(s.assessment.magnitude, 1.0);
    }

    #[test]
    fn text_magnitude_ignores_the_neighbourhood_corpus() {
        let policy = Policy::default();
        let empty = Corpus::new();
        let mut crowded = Corpus::new();
        for k in 0..60 {
            crowded.add_document(
                &format!("customer accounts password reset signed link {k}"),
                &policy.text,
            );
        }
        let (a, b) = (
            FieldValue::text(
                "Customer password reset by email for customer accounts with a signed link",
            ),
            FieldValue::text(
                "Customer password reset by SMS for customer accounts with a signed link",
            ),
        );
        let lexicon = text::Lexicon::new(&policy.text);
        let m = |corpus: &Corpus| {
            score(
                "description",
                Some(&a),
                Some(&b),
                &ctx(&policy, corpus, &lexicon),
            )
            .expect("changed")
            .assessment
            .magnitude
        };
        assert_eq!(m(&empty), m(&crowded));
    }

    #[test]
    fn pure_additions_are_capped_at_minor() {
        for (before, after) in [
            (
                None,
                Some("Allow users to export invoices as PDF from the billing page."),
            ),
            (Some(""), Some("Allow users to export invoices as PDF.")),
            (
                Some("Add a login page."),
                Some("Add a login page. Also show a remember-me checkbox under the form."),
            ),
        ] {
            let s = sc(
                "description",
                before.map(FieldValue::text),
                after.map(FieldValue::text),
            )
            .expect("changed");
            assert!(!s.assessment.breaking, "{before:?}: {:?}", s.assessment);
            assert!(s.assessment.additive);
            assert_eq!(s.max_bump, VersionBump::Minor);
        }
        // Clearing a description, or rewording one, may still break.
        let cleared = sc(
            "description",
            Some(FieldValue::text(
                "Export invoices as PDF from the billing page.",
            )),
            None,
        )
        .expect("changed");
        assert!(cleared.assessment.breaking);
        assert_eq!(cleared.max_bump, VersionBump::Major);
    }

    #[test]
    fn logistics_fields_are_never_breaking() {
        for name in ["assignee", "cycle", "team"] {
            let s = sc(
                name,
                Some(FieldValue::reference("a")),
                Some(FieldValue::reference("b")),
            )
            .expect("changed");
            assert!(!s.assessment.breaking, "{name}");
            assert_eq!(s.max_bump, VersionBump::Minor, "{name}");
        }
        let s = sc(
            "labels",
            Some(FieldValue::set(["ui"])),
            Some(FieldValue::set(["ui", "breaking"])),
        )
        .expect("changed");
        assert!(!s.assessment.breaking);
        assert!(s.assessment.risk >= 0.5, "still significant");
    }

    #[test]
    fn resolution_labels_matter_when_added_not_when_removed() {
        let added = sc(
            "labels",
            Some(FieldValue::set(["api"])),
            Some(FieldValue::set(["api", "blocked"])),
        )
        .expect("changed");
        let removed = sc(
            "labels",
            Some(FieldValue::set(["api", "Blocked"])),
            Some(FieldValue::set(["api"])),
        )
        .expect("changed");
        assert!(added.assessment.risk >= 0.5);
        assert!(removed.assessment.risk < 0.15, "{:?}", removed.assessment);
        // Labels significant both ways stay so.
        let security_removed = sc(
            "labels",
            Some(FieldValue::set(["api", "security"])),
            Some(FieldValue::set(["api"])),
        )
        .expect("changed");
        assert!(security_removed.assessment.risk >= 0.5);
    }

    #[test]
    fn date_shifts_are_measured_against_a_horizon() {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
            .single()
            .expect("valid date");
        let at = |h: i64| FieldValue::Timestamp(now + chrono::Duration::hours(h));
        let m = |from: i64, to: i64| {
            sc("due_date", Some(at(from)), Some(at(to)))
                .expect("changed")
                .assessment
        };
        // 30 days overdue, pulled one day earlier / pushed three (still overdue).
        assert!(m(-720, -744).magnitude < 0.1);
        assert!(m(-720, -648).magnitude < 0.1);
        // Due tomorrow, pushed three days: not a breaking compression.
        assert!(!m(24, 96).breaking);
        assert!(m(24, 96).magnitude < 0.3);
        // An overdue date rescheduled two weeks out counts from today.
        let resched = m(-720, 13 * 24);
        assert!(resched.magnitude < 0.7 && !resched.breaking, "{resched:?}");
        // Pulling a date into the past compresses the whole window.
        assert!(m(20 * 24, -24).breaking);
    }

    #[test]
    fn lifecycle_carries_its_kind() {
        let policy = Policy::default();
        let created = lifecycle(true, Some(3.0), &policy);
        assert!(matches!(
            created.detail,
            Detail::Lifecycle {
                kind: LifecycleKind::Created,
                estimate: Some(e)
            } if e == 3.0
        ));
        let deleted = lifecycle(false, None, &policy);
        assert!(matches!(
            deleted.detail,
            Detail::Lifecycle {
                kind: LifecycleKind::Deleted,
                estimate: None
            }
        ));
        assert!(deleted.assessment.breaking);
    }

    #[test]
    fn text_removed_criterion_is_breaking_added_is_additive() {
        let removed = sc(
            "description",
            Some(FieldValue::text("- [ ] A thing\n- [ ] Another thing")),
            Some(FieldValue::text("- [ ] A thing")),
        )
        .expect("changed");
        assert!(removed.assessment.breaking);
        let added = sc(
            "description",
            Some(FieldValue::text("Some context for the work here.\n- [ ] A thing to deliver\n- [ ] Another thing to deliver")),
            Some(FieldValue::text("Some context for the work here.\n- [ ] A thing to deliver\n- [ ] Another thing to deliver\n- [ ] A third")),
        )
        .expect("changed");
        assert!(added.assessment.additive, "{:?}", added.assessment);
        assert!(!added.assessment.breaking);
    }
}
