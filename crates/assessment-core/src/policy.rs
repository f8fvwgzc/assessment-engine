//! Every tunable number of the engine, with defaults and validation.
//!
//! A [`Policy`] is plain data (serde), so it can be stored, diffed, shipped
//! in a request as a partial JSON patch ([`Policy::merged`]) and returned by
//! the server for inspection. `docs/SCORING.md` explains what each number
//! does in the math.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::assessment::SignalCategory;
use crate::error::{Error, Result};
use crate::model::field;
use crate::semver::VersionBump;

/// How one field's changes score and travel.
///
/// `severity` caps the risk a change of this field can carry on its own
/// entity (risk = severity × magnitude). The channel strengths say how much
/// of that risk travels along each kind of edge before relevance and decay.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FieldPolicy {
    /// Maximum risk a change of this field carries, `[0, 1]`.
    pub severity: f64,
    /// Parent → children (and deeper descendants).
    pub down: f64,
    /// Child → parent (and further ancestors).
    pub up: f64,
    /// Blocker → blocked entities (downstream).
    pub dependency: f64,
    /// Blocked → its blockers (upstream).
    pub upstream: f64,
    /// `relates_to` / `duplicates` neighbours (one hop).
    pub related: f64,
    /// Other children of the same parent (one hop, via shared capacity).
    pub sibling: f64,
    /// A magnitude at or above this is a breaking (MAJOR) change. Above 1 = never.
    pub breaking_magnitude: f64,
    /// The largest version bump a change of this field can cause on its own
    /// (`major` by default). Logistics fields (assignee, cycle, labels, team)
    /// are capped at `minor`: however they combine, they never make a change
    /// breaking, because they do not change the scope anyone planned against.
    pub max_bump: VersionBump,
}

impl Default for FieldPolicy {
    fn default() -> Self {
        Self::new(0.25, [0.2, 0.1, 0.1, 0.0, 0.05, 0.0], 2.0)
    }
}

impl FieldPolicy {
    /// `channels` = `[down, up, dependency, upstream, related, sibling]`.
    pub const fn new(severity: f64, channels: [f64; 6], breaking_magnitude: f64) -> Self {
        Self {
            severity,
            down: channels[0],
            up: channels[1],
            dependency: channels[2],
            upstream: channels[3],
            related: channels[4],
            sibling: channels[5],
            breaking_magnitude,
            max_bump: VersionBump::Major,
        }
    }

    /// This policy with another [`FieldPolicy::max_bump`].
    pub const fn with_max_bump(mut self, max_bump: VersionBump) -> Self {
        self.max_bump = max_bump;
        self
    }

    fn validate(&self, name: &str) -> Result<()> {
        for (what, v) in [
            ("severity", self.severity),
            ("down", self.down),
            ("up", self.up),
            ("dependency", self.dependency),
            ("upstream", self.upstream),
            ("related", self.related),
            ("sibling", self.sibling),
        ] {
            unit(&format!("fields.{name}.{what}"), v)?;
        }
        finite_non_negative(
            &format!("fields.{name}.breaking_magnitude"),
            self.breaking_magnitude,
        )?;
        if self.max_bump == VersionBump::Initial {
            return Err(Error::invalid_policy(format!(
                "fields.{name}.max_bump must be none, patch, minor or major"
            )));
        }
        Ok(())
    }
}

/// Score thresholds of the impact levels (inclusive lower bounds).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub low: f64,
    pub medium: f64,
    pub high: f64,
    pub critical: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            low: 0.05,
            medium: 0.25,
            high: 0.5,
            critical: 0.75,
        }
    }
}

/// When a change bumps MAJOR or MINOR (by its content score).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SemverPolicy {
    pub major: f64,
    pub minor: f64,
}

impl Default for SemverPolicy {
    fn default() -> Self {
        Self {
            major: 0.55,
            minor: 0.2,
        }
    }
}

/// How impact travels through the graph.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropagationPolicy {
    /// Hops followed from a changed entity.
    pub max_depth: u32,
    /// Factor per hop beyond the first.
    pub hop_decay: f64,
    /// Paths whose strongest possible contribution is below this are not
    /// followed further, and entities whose combined score is below it are
    /// not listed. (Single contributions are kept however small, so many weak
    /// ones still add up at their target.)
    pub min_impact: f64,
    /// Relevance a text change has for a neighbour that shares no terms with it.
    pub relevance_floor: f64,
    /// Overlap of removed text with a child's text above which the child is
    /// flagged as possibly obsolete.
    pub removed_overlap_flag: f64,
    /// Overlap of added text with a child's text above which the child is
    /// flagged as needing a scope update.
    pub added_overlap_flag: f64,
    /// Sibling relevance when the sibling shares the assignee or the cycle.
    pub sibling_shared: f64,
    /// Sibling relevance otherwise.
    pub sibling_default: f64,
    /// A child *owns* a removed or reworded passage of its parent's text when
    /// its overlap with the passage is at least this…
    pub owner_min_overlap: f64,
    /// …and at least this share of the best overlap any of its siblings has
    /// with it. An active owner takes the change at full relevance.
    pub owner_tie_ratio: f64,
    /// The effective magnitude, at least, for the owner of a removed passage
    /// (its planned scope is gone, however small the parent's edit).
    pub removed_owner_magnitude: f64,
    /// The effective magnitude, at least, for the owner of a reworded passage.
    pub reworded_owner_magnitude: f64,
    /// Upper bound on graph steps per assessment (a safety valve), shared
    /// among the seeds in proportion to their risk.
    pub max_steps: u32,
    /// Steps every seed may take whatever its share of `max_steps`.
    pub min_seed_steps: u32,
}

impl Default for PropagationPolicy {
    fn default() -> Self {
        Self {
            max_depth: 4,
            hop_decay: 0.6,
            min_impact: 0.01,
            relevance_floor: 0.15,
            removed_overlap_flag: 0.3,
            added_overlap_flag: 0.3,
            sibling_shared: 1.0,
            sibling_default: 0.3,
            owner_min_overlap: 0.2,
            owner_tie_ratio: 0.9,
            removed_owner_magnitude: 0.8,
            reworded_owner_magnitude: 0.6,
            max_steps: 500_000,
            min_seed_steps: 64,
        }
    }
}

/// How much a change matters given where the affected entity stands.
/// Applied as `γ` in `1 - (1 - s)^γ`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PhaseFactors {
    pub backlog: f64,
    pub unstarted: f64,
    pub started: f64,
    /// Above 1: changing what is already done means rework.
    pub completed: f64,
    pub canceled: f64,
    /// Entities without a status.
    pub unknown: f64,
}

impl Default for PhaseFactors {
    fn default() -> Self {
        Self {
            backlog: 0.5,
            unstarted: 0.75,
            started: 1.0,
            completed: 1.15,
            canceled: 0.15,
            unknown: 0.8,
        }
    }
}

/// Deadline proximity: `γ = 1 + amplitude · e^{-days_left / horizon_days}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimePressurePolicy {
    pub amplitude: f64,
    pub horizon_days: f64,
    /// `γ` when the entity is already overdue.
    pub overdue: f64,
}

impl Default for TimePressurePolicy {
    fn default() -> Self {
        Self {
            amplitude: 0.5,
            horizon_days: 14.0,
            overdue: 1.5,
        }
    }
}

/// Churn: `γ = 1 + amplitude · tanh(I / saturation)` with `I` the decayed sum
/// of past change magnitudes; and the anomaly z-score.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolatilityPolicy {
    pub amplitude: f64,
    pub half_life_days: f64,
    pub saturation: f64,
    /// A change whose content score is this many standard deviations above
    /// the entity's history is flagged as unusual.
    pub anomaly_z: f64,
    pub min_samples: usize,
}

impl Default for VolatilityPolicy {
    fn default() -> Self {
        Self {
            amplitude: 0.3,
            half_life_days: 7.0,
            saturation: 1.5,
            anomaly_z: 2.0,
            min_samples: 5,
        }
    }
}

/// How the overall score combines the worst impact and the share of effort affected:
/// `1 − (1 − severity)·(1 − breadth)^(breadth_weight / severity_weight)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AggregationPolicy {
    /// Must be positive.
    pub severity_weight: f64,
    pub breadth_weight: f64,
    /// Effort of an entity without an estimate.
    pub default_estimate: f64,
}

impl Default for AggregationPolicy {
    fn default() -> Self {
        Self {
            severity_weight: 1.0,
            breadth_weight: 0.4,
            default_estimate: 1.0,
        }
    }
}

/// Text comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextPolicy {
    /// Weight of the embedding distance when embeddings are available.
    pub embedding_weight: f64,
    /// Cosine at or below which two embeddings count as unrelated.
    pub embedding_floor: f64,
    /// Texts up to this many characters also get a character-level edit distance.
    pub short_text_chars: usize,
    /// Word n-gram size of the shingle similarity.
    pub shingle_size: usize,
    /// Weights of the structural evidence (lists/acceptance criteria, negation, modal verbs, numbers).
    pub structure_weight: f64,
    pub negation_weight: f64,
    pub modal_weight: f64,
    pub number_weight: f64,
    /// Drop common English function words before term statistics.
    pub stopwords: bool,
    pub negation_terms: BTreeSet<String>,
    pub strong_modals: BTreeSet<String>,
    pub weak_modals: BTreeSet<String>,
    /// Which texts make up the IDF corpus that weights passages and overlaps.
    pub idf_scope: IdfScope,
}

/// Which texts of a request make up the IDF corpus (document frequencies)
/// that weights changed passages and their overlap with other entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdfScope {
    /// Every entity in the request (the changed ones and the whole context).
    /// Unreached context still dilutes shared vocabulary, so sending more
    /// unrelated context nudges scores.
    #[default]
    Context,
    /// Only the changed entities and what propagation can reach: everything
    /// within `propagation.max_depth` hops along its channels. Text outside
    /// that reach is not analysed and is not part of the IDF corpus, so
    /// relevance to reached entities is higher where the vocabulary was
    /// shared with unrelated work. Only text is left out. Context linked to
    /// the changes still counts toward breadth and violation checks, even
    /// when it is outside the reach. Context with no link to the changes
    /// leaves the result unchanged.
    Reach,
}

impl Default for TextPolicy {
    fn default() -> Self {
        let set = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect();
        Self {
            embedding_weight: 0.6,
            embedding_floor: 0.3,
            short_text_chars: 512,
            shingle_size: 2,
            structure_weight: 0.8,
            negation_weight: 0.8,
            modal_weight: 0.5,
            number_weight: 0.7,
            stopwords: true,
            negation_terms: set(&[
                "not",
                "no",
                "never",
                "without",
                "none",
                "nor",
                "cannot",
                "neither",
                "биш",
                "үгүй",
                "битгий",
            ]),
            strong_modals: set(&[
                "must",
                "shall",
                "required",
                "require",
                "mandatory",
                "always",
                "need",
                "needs",
                "заавал",
            ]),
            weak_modals: set(&[
                "should",
                "may",
                "might",
                "could",
                "optional",
                "ideally",
                "nice",
                "preferably",
            ]),
            idf_scope: IdfScope::Context,
        }
    }
}

/// Numeric fields (estimates).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NumberPolicy {
    /// Log-ratio at which the magnitude reaches `1 - 1/e` (ln 2 = doubling).
    pub log_scale: f64,
    /// Additive smoothing so `0 → n` has a finite ratio.
    pub smoothing: f64,
    /// Decreases count this much of an equal increase.
    pub decrease_factor: f64,
    /// Share of the parent's effort at which a roll-up reaches `1 - 1/e`.
    pub rollup_scale: f64,
    /// Magnitude of setting a number that was unset.
    pub set_magnitude: f64,
    /// Magnitude of clearing a number.
    pub cleared_magnitude: f64,
}

impl Default for NumberPolicy {
    fn default() -> Self {
        Self {
            log_scale: std::f64::consts::LN_2,
            smoothing: 1.0,
            decrease_factor: 0.7,
            rollup_scale: 0.3,
            set_magnitude: 0.3,
            cleared_magnitude: 0.5,
        }
    }
}

/// Timestamps (due and start dates).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatePolicy {
    /// Rate for pulling a date in (schedule compression): `1 - e^{-rate·ρ}`.
    pub pull_in_rate: f64,
    /// Rate for pushing a date out (slip).
    pub push_out_rate: f64,
    /// Push-outs count this much (slack is good news locally, bad for dependents).
    pub push_out_factor: f64,
    pub set_magnitude: f64,
    pub cleared_magnitude: f64,
    /// The smallest remaining window, in days, a shift is measured against
    /// (a horizon, so an imminent or overdue date does not turn every
    /// one-day correction into a breaking change).
    pub min_window_days: f64,
    /// A move between two dates that are both already past counts this
    /// much of a push-out of the same size (bookkeeping on overdue work).
    pub overdue_factor: f64,
}

impl Default for DatePolicy {
    fn default() -> Self {
        Self {
            pull_in_rate: 2.0,
            push_out_rate: 1.0,
            push_out_factor: 0.8,
            set_magnitude: 0.15,
            cleared_magnitude: 0.3,
            min_window_days: 14.0,
            overdue_factor: 0.5,
        }
    }
}

/// References (assignee, project, cycle, team).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReferencePolicy {
    pub reassign: f64,
    pub assign: f64,
    pub unassign: f64,
}

impl Default for ReferencePolicy {
    fn default() -> Self {
        Self {
            reassign: 1.0,
            assign: 0.25,
            unassign: 0.6,
        }
    }
}

/// Ordinals (priority).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrdinalPolicy {
    pub escalation_factor: f64,
    pub deescalation_factor: f64,
}

impl Default for OrdinalPolicy {
    fn default() -> Self {
        Self {
            escalation_factor: 1.0,
            deescalation_factor: 0.6,
        }
    }
}

/// Sets (labels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetPolicy {
    /// Labels whose addition or removal is significant on its own (case-insensitive).
    pub significant: BTreeSet<String>,
    /// Labels whose addition is significant but whose removal is a
    /// resolution (`blocked`, `incident`, `regression`): removing one is
    /// scored like any other label (Jaccard distance).
    pub significant_on_add: BTreeSet<String>,
    /// The severity a change touching a significant label carries at least.
    pub significant_severity: f64,
}

impl Default for SetPolicy {
    fn default() -> Self {
        Self {
            significant: [
                "security",
                "breaking-change",
                "breaking",
                "urgent",
                "compliance",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            significant_on_add: ["blocked", "regression", "incident"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            significant_severity: 0.5,
        }
    }
}

/// A comment lexicon entry: phrases that signal a category.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalRule {
    pub category: SignalCategory,
    pub weight: f64,
    pub terms: Vec<String>,
}

/// Severities of plan-consistency violations (also the score floor they impose).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViolationPolicy {
    pub orphaned_work: f64,
    pub cyclic_dependency: f64,
    pub schedule_conflict: f64,
    pub dependency_conflict: f64,
    pub blocked_by_closed: f64,
    pub premature_close: f64,
    pub estimate_overflow: f64,
    pub overdue: f64,
    /// Children may sum to this fraction above the parent's estimate before it is an overflow.
    pub estimate_tolerance: f64,
}

impl Default for ViolationPolicy {
    fn default() -> Self {
        Self {
            orphaned_work: 0.8,
            cyclic_dependency: 0.7,
            schedule_conflict: 0.6,
            dependency_conflict: 0.6,
            blocked_by_closed: 0.55,
            premature_close: 0.5,
            estimate_overflow: 0.45,
            overdue: 0.4,
            estimate_tolerance: 0.1,
        }
    }
}

/// Creation and deletion, scored as a synthetic `lifecycle` field.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecyclePolicy {
    pub create: FieldPolicy,
    pub delete: FieldPolicy,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self {
            create: FieldPolicy::new(0.3, [0.0, 0.6, 0.3, 0.0, 0.05, 0.15], 2.0),
            delete: FieldPolicy::new(0.7, [1.0, 0.6, 0.7, 0.1, 0.1, 0.1], 0.0),
        }
    }
}

/// Every tunable number of the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// Per-field policies by field name.
    pub fields: BTreeMap<String, FieldPolicy>,
    /// For fields not in `fields`.
    pub default_field: FieldPolicy,
    /// For comments and other activity.
    pub activity: FieldPolicy,
    /// For comments that report a blocker or a schedule slip: the severity
    /// and channel strengths are at least these (a slip of a blocker reaches
    /// what it blocks whatever words they share).
    pub activity_schedule: FieldPolicy,
    pub lifecycle: LifecyclePolicy,
    pub thresholds: Thresholds,
    pub semver: SemverPolicy,
    pub propagation: PropagationPolicy,
    pub phase: PhaseFactors,
    pub time_pressure: TimePressurePolicy,
    pub volatility: VolatilityPolicy,
    pub aggregation: AggregationPolicy,
    pub text: TextPolicy,
    pub number: NumberPolicy,
    pub date: DatePolicy,
    pub reference: ReferencePolicy,
    pub ordinal: OrdinalPolicy,
    pub set: SetPolicy,
    pub violations: ViolationPolicy,
    /// Risk of a status transition, `[from][to]`, in the order backlog,
    /// unstarted, started, completed, canceled.
    pub status_transitions: [[f64; 5]; 5],
    /// The comment lexicon.
    pub signals: Vec<SignalRule>,
    /// Which text changes are flagged for a judgment of their meaning, and
    /// how a supplied judgment moves the numbers.
    pub judge: JudgePolicy,
}

/// The gray zone of text changes and the effect of a judgment.
///
/// The engine flags a text change when its lexical reading may be wrong
/// about the meaning. A caller may then supply a
/// [`TextJudgment`](crate::model::TextJudgment). The engine never calls a
/// judge itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgePolicy {
    /// Off: nothing is flagged and supplied judgments are ignored.
    pub enabled: bool,
    /// The text fields that can be flagged and judged.
    pub fields: BTreeSet<String>,
    /// At most this many flagged fields per assessment are asked for. It
    /// shapes the output only.
    pub max_requests: usize,
    /// A text longer than this (in characters) is never flagged.
    pub max_text_chars: usize,
    /// Confidence at which `narrowed`, `replaced` and `reversed` start to count.
    pub min_confidence: f64,
    /// Confidence at which `equivalent` and `extended` start to count.
    pub min_confidence_lowering: f64,
    /// Factor on the confidence of a flagged field that was not judged.
    pub unjudged_confidence: f64,
    /// Use a judgment of a field the engine did not flag.
    pub apply_unflagged: bool,
    /// A change with no structural evidence is flagged when its magnitude
    /// is within `[breaking − near_below, breaking + near_above)`.
    pub near_below: f64,
    pub near_above: f64,
    /// A reworded sentence that swaps at most this many terms each way is
    /// flagged as a substitution.
    pub max_substituted_terms: usize,
    /// The lexical distance an `equivalent` judgment pulls towards.
    pub equivalent_target: f64,
    /// The lexical distance an `extended` judgment pulls towards.
    pub extended_target: f64,
    /// The evidence a full-weight `narrowed` judgment adds (noisy-OR).
    pub narrowed_evidence: f64,
    pub replaced_evidence: f64,
    pub reversed_evidence: f64,
    /// The least share of the owner floors a lowering judgment leaves, so
    /// owners of a changed passage are still notified.
    pub min_owner_share: f64,
    /// Words and phrases that narrow or widen a scope (lowercase).
    pub scope_qualifiers: Vec<String>,
}

impl Default for JudgePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            fields: [field::DESCRIPTION, field::TITLE]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            max_requests: 16,
            max_text_chars: 20_000,
            min_confidence: 0.5,
            min_confidence_lowering: 0.6,
            unjudged_confidence: 0.85,
            apply_unflagged: false,
            near_below: 0.2,
            near_above: 0.1,
            max_substituted_terms: 3,
            equivalent_target: 0.05,
            extended_target: 0.25,
            narrowed_evidence: 0.315,
            replaced_evidence: 0.275,
            reversed_evidence: 0.403,
            min_owner_share: 0.25,
            scope_qualifiers: [
                "apart from",
                "at most",
                "but not",
                "except",
                "excepting",
                "exclude",
                "excluded",
                "excludes",
                "excluding",
                "exclusively",
                "instead of",
                "limited to",
                "no longer",
                "only",
                "other than",
                "rather than",
                "restricted to",
                "solely",
                "unless",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

impl JudgePolicy {
    /// The evidence a full-weight judgment of a raising relation adds.
    pub(crate) fn evidence(&self, relation: crate::model::TextRelation) -> f64 {
        use crate::model::TextRelation as R;
        match relation {
            R::Narrowed => self.narrowed_evidence,
            R::Replaced => self.replaced_evidence,
            R::Reversed => self.reversed_evidence,
            R::Equivalent | R::Extended | R::Undetermined => 0.0,
        }
    }

    /// The confidence at which a judgment of `relation` starts to count.
    pub(crate) fn threshold(&self, relation: crate::model::TextRelation) -> f64 {
        if relation.is_raising() {
            self.min_confidence
        } else {
            self.min_confidence_lowering
        }
    }

    fn validate(&self) -> Result<()> {
        for (what, v) in [
            ("min_confidence", self.min_confidence),
            ("min_confidence_lowering", self.min_confidence_lowering),
            ("unjudged_confidence", self.unjudged_confidence),
            ("near_below", self.near_below),
            ("near_above", self.near_above),
            ("equivalent_target", self.equivalent_target),
            ("extended_target", self.extended_target),
            ("narrowed_evidence", self.narrowed_evidence),
            ("replaced_evidence", self.replaced_evidence),
            ("reversed_evidence", self.reversed_evidence),
            ("min_owner_share", self.min_owner_share),
        ] {
            unit(&format!("judge.{what}"), v)?;
        }
        let bad = |m: String| Err(Error::invalid_policy(m));
        if self.min_confidence >= 1.0 || self.min_confidence_lowering >= 1.0 {
            return bad(
                "judge.min_confidence and judge.min_confidence_lowering must be below 1".to_owned(),
            );
        }
        let least_evidence = self
            .narrowed_evidence
            .min(self.replaced_evidence)
            .min(self.reversed_evidence);
        if !(self.equivalent_target <= self.extended_target
            && self.extended_target <= least_evidence)
        {
            return bad(
                "judge: equivalent_target ≤ extended_target ≤ every *_evidence must hold"
                    .to_owned(),
            );
        }
        if !(1..=16).contains(&self.max_substituted_terms) {
            return bad(format!(
                "judge.max_substituted_terms must be in [1, 16], got {}",
                self.max_substituted_terms
            ));
        }
        if self.max_requests > 1_000 {
            return bad(format!(
                "judge.max_requests must be at most 1000, got {}",
                self.max_requests
            ));
        }
        if self.max_text_chars > 1_048_576 {
            return bad(format!(
                "judge.max_text_chars must be at most 1048576, got {}",
                self.max_text_chars
            ));
        }
        if self.fields.iter().any(|f| f.trim().is_empty()) {
            return bad("judge.fields has a blank field name".to_owned());
        }
        for q in &self.scope_qualifiers {
            if crate::text::token_count(q) == 0 {
                return bad(format!("judge.scope_qualifiers: `{q}` has no word"));
            }
        }
        Ok(())
    }
}

impl Default for Policy {
    fn default() -> Self {
        let mut fields = BTreeMap::new();
        // [down, up, dependency, upstream, related, sibling]
        let entries: [(&str, FieldPolicy); 14] = [
            (
                field::TITLE,
                FieldPolicy::new(0.55, [0.6, 0.2, 0.2, 0.05, 0.15, 0.1], 0.6),
            ),
            (
                field::DESCRIPTION,
                FieldPolicy::new(0.85, [0.9, 0.3, 0.3, 0.15, 0.2, 0.15], 0.5),
            ),
            (
                field::ESTIMATE,
                FieldPolicy::new(0.6, [0.2, 0.8, 0.4, 0.1, 0.0, 0.15], 0.63),
            ),
            (
                field::STATUS,
                FieldPolicy::new(0.9, [1.0, 0.6, 0.8, 0.2, 0.1, 0.0], 0.6),
            ),
            (
                field::PRIORITY,
                FieldPolicy::new(0.35, [0.4, 0.15, 0.3, 0.5, 0.05, 0.25], 0.75),
            ),
            (
                field::ASSIGNEE,
                FieldPolicy::new(0.4, [0.1, 0.05, 0.15, 0.0, 0.0, 0.2], 2.0)
                    .with_max_bump(VersionBump::Minor),
            ),
            (
                field::DUE_DATE,
                FieldPolicy::new(0.6, [0.7, 0.5, 0.9, 0.6, 0.05, 0.1], 0.7),
            ),
            (
                field::START_DATE,
                FieldPolicy::new(0.4, [0.5, 0.3, 0.6, 0.3, 0.0, 0.05], 0.8),
            ),
            (
                field::LABELS,
                FieldPolicy::new(0.2, [0.2, 0.1, 0.1, 0.0, 0.05, 0.0], 2.0)
                    .with_max_bump(VersionBump::Minor),
            ),
            (
                field::PROJECT,
                FieldPolicy::new(0.6, [0.8, 0.4, 0.2, 0.0, 0.0, 0.0], 0.9),
            ),
            (
                field::CYCLE,
                FieldPolicy::new(0.35, [0.3, 0.1, 0.3, 0.1, 0.0, 0.1], 2.0)
                    .with_max_bump(VersionBump::Minor),
            ),
            (
                field::TEAM,
                FieldPolicy::new(0.5, [0.4, 0.2, 0.2, 0.0, 0.0, 0.0], 2.0)
                    .with_max_bump(VersionBump::Minor),
            ),
            (
                field::PARENT,
                FieldPolicy::new(0.7, [0.3, 0.7, 0.2, 0.0, 0.0, 0.2], 0.9),
            ),
            (
                field::RELATIONS,
                FieldPolicy::new(0.4, [0.1, 0.2, 0.5, 0.3, 0.1, 0.0], 2.0),
            ),
        ];
        for (name, p) in entries {
            fields.insert(name.to_owned(), p);
        }
        Self {
            fields,
            default_field: FieldPolicy::default(),
            activity: FieldPolicy::new(0.4, [0.3, 0.25, 0.35, 0.2, 0.1, 0.05], 2.0),
            activity_schedule: FieldPolicy::new(0.4, [0.3, 0.5, 0.8, 0.2, 0.1, 0.05], 2.0),
            lifecycle: LifecyclePolicy::default(),
            thresholds: Thresholds::default(),
            semver: SemverPolicy::default(),
            propagation: PropagationPolicy::default(),
            phase: PhaseFactors::default(),
            time_pressure: TimePressurePolicy::default(),
            volatility: VolatilityPolicy::default(),
            aggregation: AggregationPolicy::default(),
            text: TextPolicy::default(),
            number: NumberPolicy::default(),
            date: DatePolicy::default(),
            reference: ReferencePolicy::default(),
            ordinal: OrdinalPolicy::default(),
            set: SetPolicy::default(),
            violations: ViolationPolicy::default(),
            //            to: backlog unstarted started completed canceled
            status_transitions: [
                [0.0, 0.05, 0.10, 0.25, 0.20], // from backlog
                [0.15, 0.0, 0.05, 0.20, 0.30], // from unstarted
                [0.50, 0.35, 0.0, 0.10, 0.80], // from started
                [0.80, 0.70, 0.60, 0.0, 0.50], // from completed
                [0.40, 0.45, 0.50, 0.40, 0.0], // from canceled
            ],
            signals: default_signals(),
            judge: JudgePolicy::default(),
        }
    }
}

fn default_signals() -> Vec<SignalRule> {
    let rule = |category, weight, terms: &[&str]| SignalRule {
        category,
        weight,
        terms: terms.iter().map(|s| (*s).to_owned()).collect(),
    };
    vec![
        rule(
            SignalCategory::Blocker,
            0.6,
            &[
                "blocked",
                "blocker",
                "blocking",
                "stuck",
                "cannot proceed",
                "can not proceed",
                "waiting on",
                "waiting for",
                "depends on",
                "external dependency",
                "missing dependency",
                "hard stop",
                "саатсан",
                "гацсан",
            ],
        ),
        rule(
            SignalCategory::Scope,
            0.7,
            &[
                "scope",
                "requirement changed",
                "requirements changed",
                "new requirement",
                "out of scope",
                "also need",
                "also needs",
                "instead of",
                "change of plan",
                "pivot",
                "rewrite it",
                "rewrite the feature",
                "full rewrite",
                "complete rewrite",
                "rewrite from scratch",
                "redesign",
                "rethink",
                "no longer needed",
                "drop it",
                "drop this",
                "drop that",
                "drop the feature",
                "drop the requirement",
                "drop support",
                "descope",
                "descoped",
                "шаардлага өөрчлөгдсөн",
            ],
        ),
        rule(
            SignalCategory::Risk,
            0.5,
            &[
                "risk",
                "concern",
                "regression",
                "breaking",
                "broken",
                "bug",
                "fails",
                "failing",
                "failure",
                "security",
                "vulnerability",
                "incident",
                "outage",
                "data loss",
                "эрсдэл",
            ],
        ),
        rule(
            SignalCategory::Schedule,
            0.55,
            &[
                "delay",
                "delayed",
                "slip",
                "behind schedule",
                "more time",
                "longer than",
                "underestimated",
                "overrun",
                "miss the deadline",
                "push back",
                "хойшлох",
                "хоцорсон",
            ],
        ),
        rule(
            SignalCategory::Uncertainty,
            0.3,
            &[
                "unclear",
                "not sure",
                "unsure",
                "clarify",
                "confused",
                "confusing",
                "ambiguous",
                "question",
                "тодорхойгүй",
            ],
        ),
        rule(
            SignalCategory::Resolution,
            0.5,
            &[
                "done",
                "fixed",
                "resolved",
                "lgtm",
                "approved",
                "merged",
                "shipped",
                "unblocked",
                "дууссан",
                "шийдэгдсэн",
            ],
        ),
    ]
}

impl Policy {
    /// The policy of a field by name (the default for unknown names).
    pub fn field(&self, name: &str) -> &FieldPolicy {
        match name {
            field::ACTIVITY => &self.activity,
            _ => self.fields.get(name).unwrap_or(&self.default_field),
        }
    }

    /// This policy with a partial JSON patch applied: objects merge
    /// recursively, anything else replaces. The result is validated.
    ///
    /// ```
    /// # use assessment_core::policy::Policy;
    /// let p = Policy::default()
    ///     .merged(&serde_json::json!({"thresholds": {"high": 0.4}}))
    ///     .unwrap();
    /// assert_eq!(p.thresholds.high, 0.4);
    /// assert_eq!(p.thresholds.medium, 0.25);
    /// ```
    pub fn merged(&self, patch: &serde_json::Value) -> Result<Policy> {
        if patch.is_null() {
            return Ok(self.clone());
        }
        if !patch.is_object() {
            return Err(Error::invalid_policy(
                "a policy patch must be a json object",
            ));
        }
        let mut base = serde_json::to_value(self).map_err(|e| {
            Error::with_source(
                crate::error::ErrorKind::InvalidPolicy,
                "the policy could not be serialized",
                e,
            )
        })?;
        merge_json(&mut base, patch);
        let merged: Policy = serde_json::from_value(base).map_err(|e| {
            Error::with_source(
                crate::error::ErrorKind::InvalidPolicy,
                format!("the policy patch does not describe a valid policy: {e}"),
                e,
            )
        })?;
        merged.validate()?;
        Ok(merged)
    }

    /// Checks every number is finite and within its range.
    pub fn validate(&self) -> Result<()> {
        for (name, p) in &self.fields {
            p.validate(name)?;
        }
        self.default_field.validate("default_field")?;
        self.activity.validate("activity")?;
        self.activity_schedule.validate("activity_schedule")?;
        self.lifecycle.create.validate("lifecycle.create")?;
        self.lifecycle.delete.validate("lifecycle.delete")?;

        let t = &self.thresholds;
        for (what, v) in [
            ("low", t.low),
            ("medium", t.medium),
            ("high", t.high),
            ("critical", t.critical),
        ] {
            unit(&format!("thresholds.{what}"), v)?;
        }
        if !(t.low <= t.medium && t.medium <= t.high && t.high <= t.critical) {
            return Err(Error::invalid_policy(
                "thresholds must be ordered low <= medium <= high <= critical",
            ));
        }
        unit("semver.major", self.semver.major)?;
        unit("semver.minor", self.semver.minor)?;
        if self.semver.minor > self.semver.major {
            return Err(Error::invalid_policy(
                "semver.minor must not exceed semver.major",
            ));
        }

        let pr = &self.propagation;
        if pr.max_depth > 16 {
            return Err(Error::invalid_policy(
                "propagation.max_depth must be at most 16",
            ));
        }
        if !(pr.hop_decay > 0.0 && pr.hop_decay <= 1.0) {
            return Err(Error::invalid_policy(
                "propagation.hop_decay must be in (0, 1]",
            ));
        }
        for (what, v) in [
            ("min_impact", pr.min_impact),
            ("relevance_floor", pr.relevance_floor),
            ("removed_overlap_flag", pr.removed_overlap_flag),
            ("added_overlap_flag", pr.added_overlap_flag),
            ("sibling_shared", pr.sibling_shared),
            ("sibling_default", pr.sibling_default),
            ("owner_min_overlap", pr.owner_min_overlap),
            ("owner_tie_ratio", pr.owner_tie_ratio),
            ("removed_owner_magnitude", pr.removed_owner_magnitude),
            ("reworded_owner_magnitude", pr.reworded_owner_magnitude),
        ] {
            unit(&format!("propagation.{what}"), v)?;
        }
        if pr.max_steps == 0 {
            return Err(Error::invalid_policy(
                "propagation.max_steps must be positive",
            ));
        }

        let ph = &self.phase;
        for (what, v) in [
            ("backlog", ph.backlog),
            ("unstarted", ph.unstarted),
            ("started", ph.started),
            ("completed", ph.completed),
            ("canceled", ph.canceled),
            ("unknown", ph.unknown),
        ] {
            positive(&format!("phase.{what}"), v)?;
        }
        finite_non_negative("time_pressure.amplitude", self.time_pressure.amplitude)?;
        positive(
            "time_pressure.horizon_days",
            self.time_pressure.horizon_days,
        )?;
        positive("time_pressure.overdue", self.time_pressure.overdue)?;
        finite_non_negative("volatility.amplitude", self.volatility.amplitude)?;
        positive("volatility.half_life_days", self.volatility.half_life_days)?;
        positive("volatility.saturation", self.volatility.saturation)?;
        finite_non_negative("volatility.anomaly_z", self.volatility.anomaly_z)?;
        positive(
            "aggregation.severity_weight",
            self.aggregation.severity_weight,
        )?;
        finite_non_negative(
            "aggregation.breadth_weight",
            self.aggregation.breadth_weight,
        )?;
        positive(
            "aggregation.default_estimate",
            self.aggregation.default_estimate,
        )?;

        let tx = &self.text;
        for (what, v) in [
            ("embedding_weight", tx.embedding_weight),
            ("structure_weight", tx.structure_weight),
            ("negation_weight", tx.negation_weight),
            ("modal_weight", tx.modal_weight),
            ("number_weight", tx.number_weight),
        ] {
            unit(&format!("text.{what}"), v)?;
        }
        if !(tx.embedding_floor.is_finite() && (-1.0..1.0).contains(&tx.embedding_floor)) {
            return Err(Error::invalid_policy(
                "text.embedding_floor must be in [-1, 1)",
            ));
        }
        if tx.shingle_size == 0 || tx.shingle_size > 8 {
            return Err(Error::invalid_policy("text.shingle_size must be in 1..=8"));
        }
        if tx.short_text_chars > 4096 {
            return Err(Error::invalid_policy(
                "text.short_text_chars must be at most 4096",
            ));
        }

        positive("number.log_scale", self.number.log_scale)?;
        positive("number.smoothing", self.number.smoothing)?;
        unit("number.decrease_factor", self.number.decrease_factor)?;
        positive("number.rollup_scale", self.number.rollup_scale)?;
        unit("number.set_magnitude", self.number.set_magnitude)?;
        unit("number.cleared_magnitude", self.number.cleared_magnitude)?;
        positive("date.pull_in_rate", self.date.pull_in_rate)?;
        positive("date.push_out_rate", self.date.push_out_rate)?;
        unit("date.push_out_factor", self.date.push_out_factor)?;
        unit("date.set_magnitude", self.date.set_magnitude)?;
        unit("date.cleared_magnitude", self.date.cleared_magnitude)?;
        positive("date.min_window_days", self.date.min_window_days)?;
        unit("date.overdue_factor", self.date.overdue_factor)?;
        unit("reference.reassign", self.reference.reassign)?;
        unit("reference.assign", self.reference.assign)?;
        unit("reference.unassign", self.reference.unassign)?;
        unit("ordinal.escalation_factor", self.ordinal.escalation_factor)?;
        unit(
            "ordinal.deescalation_factor",
            self.ordinal.deescalation_factor,
        )?;
        unit("set.significant_severity", self.set.significant_severity)?;

        let v = &self.violations;
        for (what, x) in [
            ("orphaned_work", v.orphaned_work),
            ("cyclic_dependency", v.cyclic_dependency),
            ("schedule_conflict", v.schedule_conflict),
            ("dependency_conflict", v.dependency_conflict),
            ("blocked_by_closed", v.blocked_by_closed),
            ("premature_close", v.premature_close),
            ("estimate_overflow", v.estimate_overflow),
            ("overdue", v.overdue),
        ] {
            unit(&format!("violations.{what}"), x)?;
        }
        finite_non_negative("violations.estimate_tolerance", v.estimate_tolerance)?;

        for (i, row) in self.status_transitions.iter().enumerate() {
            for (j, x) in row.iter().enumerate() {
                unit(&format!("status_transitions[{i}][{j}]"), *x)?;
            }
        }
        for (i, rule) in self.signals.iter().enumerate() {
            unit(&format!("signals[{i}].weight"), rule.weight)?;
        }
        self.judge.validate()?;
        Ok(())
    }
}

fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        merge_json(existing, v)
                    }
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn unit(name: &str, v: f64) -> Result<()> {
    if v.is_finite() && (0.0..=1.0).contains(&v) {
        Ok(())
    } else {
        Err(Error::invalid_policy(format!(
            "{name} must be in [0, 1], got {v}"
        )))
    }
}

fn positive(name: &str, v: f64) -> Result<()> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(Error::invalid_policy(format!(
            "{name} must be positive, got {v}"
        )))
    }
}

fn finite_non_negative(name: &str, v: f64) -> Result<()> {
    if v.is_finite() && v >= 0.0 {
        Ok(())
    } else {
        Err(Error::invalid_policy(format!(
            "{name} must be finite and non-negative, got {v}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        Policy::default()
            .validate()
            .expect("the default policy is valid");
    }

    #[test]
    fn merge_overrides_only_what_is_given() {
        let p = Policy::default()
            .merged(&serde_json::json!({
                "fields": {"description": {"severity": 0.95}, "story_points": {"severity": 0.5}},
                "propagation": {"max_depth": 2}
            }))
            .expect("valid patch");
        assert_eq!(p.field("description").severity, 0.95);
        assert_eq!(
            p.field("description").down,
            0.9,
            "other description knobs keep their defaults"
        );
        assert_eq!(p.field("story_points").severity, 0.5);
        assert_eq!(p.propagation.max_depth, 2);
        assert_eq!(p.propagation.hop_decay, 0.6);
        assert!(p.fields.contains_key("title"));
    }

    #[test]
    fn merge_rejects_invalid() {
        let bad = Policy::default().merged(&serde_json::json!({"thresholds": {"low": 0.9}}));
        assert_eq!(
            bad.expect_err("unordered").kind(),
            crate::error::ErrorKind::InvalidPolicy
        );
        let bad = Policy::default().merged(&serde_json::json!({"propagation": {"hop_decay": 0}}));
        assert!(bad.is_err());
        let bad = Policy::default().merged(&serde_json::json!([1, 2]));
        assert!(bad.is_err());
        let bad = Policy::default().merged(&serde_json::json!({"thresholds": {"low": "x"}}));
        assert!(bad.is_err());
    }

    #[test]
    fn new_knobs_are_validated() {
        let bad = Policy::default()
            .merged(&serde_json::json!({"fields": {"assignee": {"max_bump": "initial"}}}));
        assert!(bad.is_err());
        let bad =
            Policy::default().merged(&serde_json::json!({"aggregation": {"severity_weight": 0.0}}));
        assert!(bad.is_err());
        let bad = Policy::default()
            .merged(&serde_json::json!({"propagation": {"owner_min_overlap": 1.5}}));
        assert!(bad.is_err());
        let bad = Policy::default().merged(&serde_json::json!({"date": {"overdue_factor": -1}}));
        assert!(bad.is_err());
        let p = Policy::default()
            .merged(&serde_json::json!({"fields": {"cycle": {"max_bump": "major"}}}))
            .expect("valid");
        assert_eq!(p.field("cycle").max_bump, VersionBump::Major);
        assert_eq!(p.field("assignee").max_bump, VersionBump::Minor);
        assert_eq!(p.field("description").max_bump, VersionBump::Major);
    }

    #[test]
    fn judge_defaults_are_valid_and_merge_keeps_the_rest() {
        let d = JudgePolicy::default();
        d.validate().expect("valid");
        assert!(d.enabled && d.fields.contains("description") && d.fields.contains("title"));
        let p = Policy::default()
            .merged(&serde_json::json!({"judge": {"enabled": false, "max_requests": 2}}))
            .expect("valid patch");
        assert!(!p.judge.enabled);
        assert_eq!(p.judge.max_requests, 2);
        assert_eq!(
            JudgePolicy {
                enabled: true,
                max_requests: 16,
                ..p.judge.clone()
            },
            d
        );
        assert_eq!(p.text, Policy::default().text);
        // A policy stored before the judge existed still loads.
        let mut old = serde_json::to_value(Policy::default()).expect("serializes");
        old.as_object_mut().expect("an object").remove("judge");
        let back: Policy = serde_json::from_value(old).expect("deserializes");
        assert_eq!(back, Policy::default());
    }

    #[test]
    fn invalid_judge_settings_are_rejected() {
        use serde_json::json;
        for patch in [
            json!({"min_confidence": 1.0}),
            json!({"min_confidence_lowering": 1.0}),
            json!({"min_confidence": -0.1}),
            json!({"unjudged_confidence": 1.5}),
            json!({"near_below": 2}),
            json!({"near_above": -1}),
            json!({"equivalent_target": 0.3}),
            json!({"extended_target": 0.3}),
            json!({"replaced_evidence": 0.2}),
            json!({"reversed_evidence": 1.1}),
            json!({"min_owner_share": 1.2}),
            json!({"max_substituted_terms": 0}),
            json!({"max_substituted_terms": 17}),
            json!({"max_requests": 1001}),
            json!({"max_text_chars": 1_048_577}),
            json!({"fields": ["description", " "]}),
            json!({"scope_qualifiers": ["except", "…"]}),
        ] {
            let bad = Policy::default().merged(&json!({ "judge": patch }));
            assert_eq!(
                bad.expect_err(&patch.to_string()).kind(),
                crate::error::ErrorKind::InvalidPolicy,
                "{patch}"
            );
        }
        Policy::default()
            .merged(&json!({"judge": {"max_requests": 0, "scope_qualifiers": []}}))
            .expect("zero requests and no qualifiers are fine");
    }

    #[test]
    fn round_trips_through_json() {
        let p = Policy::default();
        let json = serde_json::to_value(&p).expect("serializes");
        let back: Policy = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, p);
    }
}
