//! The output model: what an assessment says and why.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{EntityId, EntityKind};
use crate::semver::{SemVer, VersionBump};

/// How much attention a change needs, by score band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactLevel {
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl ImpactLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ImpactLevel::None => "none",
            ImpactLevel::Low => "low",
            ImpactLevel::Medium => "medium",
            ImpactLevel::High => "high",
            ImpactLevel::Critical => "critical",
        }
    }
}

/// What to do about the change as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommendation {
    /// Nothing anyone planned moves.
    NoAction,
    /// Let the owners of the affected items know.
    Notify,
    /// The owners of the listed items should review them against the change.
    Review,
    /// Re-plan the subtree under `replan_root`.
    PartialReplan,
    /// Re-plan the whole effort.
    FullReplan,
}

impl Recommendation {
    pub fn as_str(self) -> &'static str {
        match self {
            Recommendation::NoAction => "no_action",
            Recommendation::Notify => "notify",
            Recommendation::Review => "review",
            Recommendation::PartialReplan => "partial_replan",
            Recommendation::FullReplan => "full_replan",
        }
    }
}

/// What to do about one affected entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    None,
    Notify,
    Review,
    /// Text it depends on gained requirements: update its scope.
    UpdateScope,
    /// Effort under it moved (a child re-estimated, created, deleted or
    /// canceled), or its children now overflow its estimate: re-estimate.
    ReEstimate,
    /// A date it depends on moved into conflict, a blocker slipped or is no
    /// longer done: reschedule.
    Reschedule,
    /// Its parent was reopened: re-verify its completed work.
    Reverify,
    /// Text it was based on was removed: it may no longer be needed.
    ObsoleteCandidate,
    /// Its parent was canceled or deleted: cancel it or move it.
    CancelOrRehome,
    /// Priorities around it changed: re-prioritize.
    Reprioritize,
    /// It changed so much that its own plan needs redoing.
    Replan,
    /// It is part of a plan-consistency violation (a blocking cycle, a
    /// canceled or deleted blocker): resolve it.
    ResolveConflict,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::None => "none",
            Action::Notify => "notify",
            Action::Review => "review",
            Action::UpdateScope => "update_scope",
            Action::ReEstimate => "re_estimate",
            Action::Reschedule => "reschedule",
            Action::Reverify => "reverify",
            Action::ObsoleteCandidate => "obsolete_candidate",
            Action::CancelOrRehome => "cancel_or_rehome",
            Action::Reprioritize => "reprioritize",
            Action::Replan => "replan",
            Action::ResolveConflict => "resolve_conflict",
        }
    }
}

/// How a changed entity changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Created,
    Updated,
    Deleted,
    /// Only activity (comments, …); no field changed.
    Activity,
}

/// The kind of edge an impact travelled along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// The entity itself changed.
    #[serde(rename = "self")]
    SelfChange,
    Down,
    Up,
    Dependency,
    Upstream,
    Related,
    Sibling,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::SelfChange => "self",
            Channel::Down => "down",
            Channel::Up => "up",
            Channel::Dependency => "dependency",
            Channel::Upstream => "upstream",
            Channel::Related => "related",
            Channel::Sibling => "sibling",
        }
    }
}

/// Whether an entity's plan still matches the versions it was planned against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BasisStatus {
    /// The entity pins none of the changed entities.
    NotPinned,
    /// Pinned versions are current.
    Current,
    /// Only patch-level changes since the pin.
    Compatible,
    /// Compatible additions since the pin: worth a look.
    ReviewSuggested,
    /// A breaking change since the pin: the plan is out of date.
    Incompatible,
    /// A pinned entity was deleted.
    Removed,
}

/// Added, removed or modified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldChangeKind {
    Added,
    Removed,
    Modified,
}

/// Which way an ordered value moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    None,
    Increase,
    Decrease,
}

/// A category of signal in activity text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalCategory {
    Blocker,
    Scope,
    Risk,
    Schedule,
    Uncertainty,
    /// Something was resolved: lowers the activity's weight.
    Resolution,
}

impl SignalCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalCategory::Blocker => "blocker",
            SignalCategory::Scope => "scope",
            SignalCategory::Risk => "risk",
            SignalCategory::Schedule => "schedule",
            SignalCategory::Uncertainty => "uncertainty",
            SignalCategory::Resolution => "resolution",
        }
    }
}

/// A plan-consistency problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// Active children under a canceled or deleted parent.
    OrphanedWork,
    /// Entities block each other in a cycle.
    CyclicDependency,
    /// A child is due after its parent.
    ScheduleConflict,
    /// A blocker is due after what it blocks is due (or starts).
    DependencyConflict,
    /// An active entity is blocked by a canceled or deleted one.
    BlockedByClosed,
    /// A parent is completed while children are still active.
    PrematureClose,
    /// Children's estimates add up to more than the parent's.
    EstimateOverflow,
    /// An active entity is past its due date.
    Overdue,
}

impl ViolationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationKind::OrphanedWork => "orphaned_work",
            ViolationKind::CyclicDependency => "cyclic_dependency",
            ViolationKind::ScheduleConflict => "schedule_conflict",
            ViolationKind::DependencyConflict => "dependency_conflict",
            ViolationKind::BlockedByClosed => "blocked_by_closed",
            ViolationKind::PrematureClose => "premature_close",
            ViolationKind::EstimateOverflow => "estimate_overflow",
            ViolationKind::Overdue => "overdue",
        }
    }
}

/// A context factor applied to a score (`γ` in `1 - (1 - s)^γ`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modifier {
    pub name: String,
    pub factor: f64,
    pub detail: String,
}

/// A signal found in activity text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub category: SignalCategory,
    pub weight: f64,
    pub terms: Vec<String>,
    pub excerpt: String,
}

/// How one field changed and how much it matters on its own entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldAssessment {
    pub field: String,
    pub change: FieldChangeKind,
    pub direction: Direction,
    /// How different the values are, `[0, 1]`.
    pub magnitude: f64,
    /// The field's severity cap.
    pub severity: f64,
    /// `severity × magnitude`: the risk this change carries locally.
    pub risk: f64,
    /// Breaks what others planned against (forces a MAJOR bump).
    pub breaking: bool,
    /// Adds compatibly (e.g. new acceptance criteria): at least a MINOR bump.
    pub additive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The measures behind the magnitude (similarities, ratios, shifts).
    pub metrics: BTreeMap<String, f64>,
    pub explanation: String,
}

/// One changed entity: what changed, how much, and its new version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeAssessment {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    pub change_type: ChangeType,
    /// The content score: noisy-OR of the field risks, before context. Drives the version bump.
    pub raw_score: f64,
    /// `raw_score` with activity and context modifiers (phase, deadline, churn).
    pub local_score: f64,
    pub modifiers: Vec<Modifier>,
    pub fields: Vec<FieldAssessment>,
    pub signals: Vec<Signal>,
    pub bump: VersionBump,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<SemVer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_version: Option<SemVer>,
    /// How unusual this change is for the entity, in standard deviations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly_z: Option<f64>,
    /// Whether the propagation of this change ran out of its share of the
    /// step budget (its impacts on far or very many entities may be missing).
    #[serde(default)]
    pub propagation_truncated: bool,
    pub explanation: String,
}

/// One piece of evidence for an impact: which change, which field, which path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contribution {
    pub source_id: EntityId,
    pub field: String,
    pub channel: Channel,
    /// From the changed entity to the affected one, both included.
    pub path: Vec<EntityId>,
    pub value: f64,
    pub detail: String,
}

/// One entity the change affects (changed entities included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityImpact {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    /// 1 = most affected.
    pub rank: u32,
    pub score: f64,
    pub level: ImpactLevel,
    pub action: Action,
    /// The channel of the strongest contribution.
    pub channel: Channel,
    /// Hops from the nearest changed entity (0 = changed itself).
    pub distance: u32,
    pub changed: bool,
    pub basis_status: BasisStatus,
    /// Whether the assessed changes produced `basis_status` (made a pin
    /// worse). `false` for a pin that was already out of date before them:
    /// it is reported, but neither raises the score nor asks for a review.
    #[serde(default)]
    pub basis_introduced: bool,
    /// The entity's effort (its estimate, or the default).
    pub effort: f64,
    /// `score × effort`.
    pub effort_at_risk: f64,
    pub contributions: Vec<Contribution>,
    pub reasons: Vec<String>,
}

/// A plan-consistency problem among the affected entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub kind: ViolationKind,
    pub entities: Vec<EntityId>,
    pub severity: f64,
    /// Whether the assessed changes introduced it (it was not there before).
    pub introduced: bool,
    pub message: String,
}

/// The engine's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    /// The re-plan score, `[0, 1]`.
    pub score: f64,
    pub level: ImpactLevel,
    pub recommendation: Recommendation,
    /// How much the engine trusts its own answer, `[0, 1]`: lower when the
    /// context is missing referenced entities or texts are too short to compare.
    pub confidence: f64,
    /// The strongest single impact.
    pub severity: f64,
    /// How widely the rest of the affected effort is touched: the effort at
    /// risk outside the most affected entity, as a share of `effort_total`.
    pub breadth: f64,
    /// `Σ score × effort` over the affected entities.
    pub effort_at_risk: f64,
    /// The effort of the affected entities: the changed ones, what
    /// structurally depends on them (descendants, blocked items), and
    /// whatever the change reached. Unrelated context does not dilute it.
    pub effort_total: f64,
    /// The smallest subtree containing every highly affected entity; always
    /// set when the recommendation is a re-plan, and never a deleted entity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replan_root: Option<EntityId>,
    pub changes: Vec<ChangeAssessment>,
    /// Ranked, most affected first; at most `options.top_k`.
    pub impacts: Vec<EntityImpact>,
    /// How many impacts there were before `top_k`.
    pub impacts_total: u32,
    pub violations: Vec<Violation>,
    pub summary: String,
    pub engine_version: String,
    pub assessed_at: DateTime<Utc>,
}
