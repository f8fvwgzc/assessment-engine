//! The input model: entities, their fields and relations, and the changes to assess.
//!
//! The engine knows nothing about any particular tracker. An [`Entity`] is
//! any versioned work item — a project, an epic, an issue, a sub-issue —
//! with typed [`FieldValue`]s under free-form names, a parent, and
//! relations to other entities. Well-known field names ([`field`]) get
//! tuned default policies; any other field still works with the default one.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::semver::SemVer;

/// An entity's identifier, opaque to the engine (nexc uses UUIDs).
pub type EntityId = String;

/// Field names the default policy knows. Any other name is accepted and
/// scored with [`crate::policy::Policy::default_field`].
pub mod field {
    /// Short text: what the item is.
    pub const TITLE: &str = "title";
    /// Long text (Markdown): what the item covers, its acceptance criteria.
    pub const DESCRIPTION: &str = "description";
    /// Number: points, hours, days — any additive effort unit.
    pub const ESTIMATE: &str = "estimate";
    /// Status: the workflow state and its category.
    pub const STATUS: &str = "status";
    /// Ordinal: importance, higher rank is more urgent.
    pub const PRIORITY: &str = "priority";
    /// Reference: who does the work.
    pub const ASSIGNEE: &str = "assignee";
    /// Timestamp: when the item is due.
    pub const DUE_DATE: &str = "due_date";
    /// Timestamp: when work is planned to start.
    pub const START_DATE: &str = "start_date";
    /// Set: labels or tags.
    pub const LABELS: &str = "labels";
    /// Reference: the project the item belongs to.
    pub const PROJECT: &str = "project";
    /// Reference: the cycle / sprint / iteration the item is planned in.
    pub const CYCLE: &str = "cycle";
    /// Reference: the owning team.
    pub const TEAM: &str = "team";
    /// Synthetic: the entity moved under another parent.
    pub const PARENT: &str = "parent";
    /// Synthetic: blocking and other relations were added or removed.
    pub const RELATIONS: &str = "relations";
    /// Synthetic: the entity was created or deleted.
    pub const LIFECYCLE: &str = "lifecycle";
    /// Synthetic: comments and other activity on the entity.
    pub const ACTIVITY: &str = "activity";
}

/// What an entity is. Serialized as a plain string; unknown strings become
/// [`EntityKind::Custom`].
///
/// A kind *is* its name: equality, hashing and ordering compare
/// [`EntityKind::as_str`], so `Custom("issue")` equals `Issue` (and
/// serializes, deserializes and compares the same way). Prefer
/// [`EntityKind::from`] or [`EntityKind::normalized`] over building
/// `Custom` with a well-known name, so that pattern matches see the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum EntityKind {
    Initiative,
    Project,
    Milestone,
    Epic,
    Issue,
    SubIssue,
    Task,
    Custom(String),
}

/// The canonical name of the well-known kind `s` names (trimmed,
/// case-insensitive, `-` read as `_`), if it names one.
fn known_kind_name(s: &str) -> Option<&'static str> {
    let t = s.trim();
    let is = |name: &str| {
        t.len() == name.len()
            && t.bytes().zip(name.bytes()).all(|(a, b)| {
                let a = if a == b'-' {
                    b'_'
                } else {
                    a.to_ascii_lowercase()
                };
                a == b
            })
    };
    const NAMES: [&str; 7] = [
        "initiative",
        "project",
        "milestone",
        "epic",
        "issue",
        "sub_issue",
        "task",
    ];
    NAMES
        .into_iter()
        .find(|n| is(n))
        .or_else(|| is("subissue").then_some("sub_issue"))
}

impl EntityKind {
    /// The kind's name: the canonical name for a well-known kind (also when
    /// it is held as `Custom`), the string itself for any other.
    pub fn as_str(&self) -> &str {
        match self {
            EntityKind::Initiative => "initiative",
            EntityKind::Project => "project",
            EntityKind::Milestone => "milestone",
            EntityKind::Epic => "epic",
            EntityKind::Issue => "issue",
            EntityKind::SubIssue => "sub_issue",
            EntityKind::Task => "task",
            EntityKind::Custom(s) => known_kind_name(s).unwrap_or(s),
        }
    }

    /// The same kind with a well-known name held as its variant
    /// (`Custom("issue")` → `Issue`); any other kind unchanged.
    pub fn normalized(self) -> Self {
        match self {
            EntityKind::Custom(s) => EntityKind::from(s),
            known => known,
        }
    }

    fn known(name: &str) -> Option<Self> {
        Some(match name {
            "initiative" => EntityKind::Initiative,
            "project" => EntityKind::Project,
            "milestone" => EntityKind::Milestone,
            "epic" => EntityKind::Epic,
            "issue" => EntityKind::Issue,
            "sub_issue" => EntityKind::SubIssue,
            "task" => EntityKind::Task,
            _ => return None,
        })
    }
}

impl PartialEq for EntityKind {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for EntityKind {}

impl Hash for EntityKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for EntityKind {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EntityKind {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl From<&str> for EntityKind {
    fn from(s: &str) -> Self {
        known_kind_name(s)
            .and_then(EntityKind::known)
            .unwrap_or_else(|| EntityKind::Custom(s.to_owned()))
    }
}

impl From<String> for EntityKind {
    fn from(s: String) -> Self {
        match known_kind_name(&s).and_then(EntityKind::known) {
            Some(k) => k,
            None => EntityKind::Custom(s),
        }
    }
}

impl From<EntityKind> for String {
    fn from(kind: EntityKind) -> Self {
        kind.as_str().to_owned()
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a workflow state means, whatever a team calls it (Linear's categories).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCategory {
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}

impl StatusCategory {
    pub const ALL: [StatusCategory; 5] = [
        StatusCategory::Backlog,
        StatusCategory::Unstarted,
        StatusCategory::Started,
        StatusCategory::Completed,
        StatusCategory::Canceled,
    ];

    /// Row/column of this category in the transition matrix.
    pub fn index(self) -> usize {
        match self {
            StatusCategory::Backlog => 0,
            StatusCategory::Unstarted => 1,
            StatusCategory::Started => 2,
            StatusCategory::Completed => 3,
            StatusCategory::Canceled => 4,
        }
    }

    /// No more work is expected.
    pub fn is_closed(self) -> bool {
        matches!(self, StatusCategory::Completed | StatusCategory::Canceled)
    }

    /// Work is expected (not completed, not canceled).
    pub fn is_active(self) -> bool {
        !self.is_closed()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StatusCategory::Backlog => "backlog",
            StatusCategory::Unstarted => "unstarted",
            StatusCategory::Started => "started",
            StatusCategory::Completed => "completed",
            StatusCategory::Canceled => "canceled",
        }
    }
}

/// A workflow state: its category and the team's name for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Status {
    pub category: StatusCategory,
    #[serde(default)]
    pub name: String,
}

/// A rank on a bounded scale; **higher is more important**. Map trackers
/// that count the other way (Linear's 1 = urgent … 4 = low, 0 = none) before
/// sending: urgent → 4, high → 3, medium → 2, low → 1, none → 0, scale 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ordinal {
    pub rank: u32,
    pub scale: u32,
}

impl Ordinal {
    /// The rank as a fraction of the scale, in `[0, 1]`.
    pub fn normalized(self) -> f64 {
        if self.scale == 0 {
            0.0
        } else {
            (f64::from(self.rank) / f64::from(self.scale)).clamp(0.0, 1.0)
        }
    }
}

/// A typed field value. Absence (no entry in [`Entity::fields`]) means null.
///
/// JSON: `{"type": "text", "value": "…"}`, `{"type": "status", "value":
/// {"category": "started", "name": "In Progress"}}`, `{"type": "set",
/// "value": ["a", "b"]}`, …
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    Text(String),
    Number(f64),
    Bool(bool),
    /// A point in time. JSON accepts an RFC 3339 timestamp
    /// (`"2026-11-20T17:00:00Z"`) or a plain date (`"2026-11-20"`, as
    /// trackers send due dates), read as **00:00 UTC** of that day. It is
    /// always serialized as RFC 3339.
    Timestamp(#[serde(deserialize_with = "deserialize_timestamp")] DateTime<Utc>),
    Status(Status),
    Ordinal(Ordinal),
    /// An identifier of something else (a user, a project, a cycle).
    Reference(String),
    Set(BTreeSet<String>),
}

impl FieldValue {
    pub fn text(s: impl Into<String>) -> Self {
        FieldValue::Text(s.into())
    }

    pub fn status(category: StatusCategory, name: impl Into<String>) -> Self {
        FieldValue::Status(Status {
            category,
            name: name.into(),
        })
    }

    pub fn ordinal(rank: u32, scale: u32) -> Self {
        FieldValue::Ordinal(Ordinal { rank, scale })
    }

    pub fn reference(id: impl Into<String>) -> Self {
        FieldValue::Reference(id.into())
    }

    pub fn set<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        FieldValue::Set(items.into_iter().map(Into::into).collect())
    }

    /// The value's type name, as used in explanations.
    pub fn type_name(&self) -> &'static str {
        match self {
            FieldValue::Text(_) => "text",
            FieldValue::Number(_) => "number",
            FieldValue::Bool(_) => "bool",
            FieldValue::Timestamp(_) => "timestamp",
            FieldValue::Status(_) => "status",
            FieldValue::Ordinal(_) => "ordinal",
            FieldValue::Reference(_) => "reference",
            FieldValue::Set(_) => "set",
        }
    }

    /// A short rendering for explanations (long text is elided).
    pub fn display(&self) -> String {
        match self {
            FieldValue::Text(s) => {
                let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
                if flat.chars().count() > 80 {
                    let cut: String = flat.chars().take(77).collect();
                    format!("{cut}…")
                } else {
                    flat
                }
            }
            FieldValue::Number(n) => format_number(*n),
            FieldValue::Bool(b) => b.to_string(),
            FieldValue::Timestamp(t) => t.format("%Y-%m-%d").to_string(),
            FieldValue::Status(s) if s.name.is_empty() => s.category.as_str().to_owned(),
            FieldValue::Status(s) => format!("{} ({})", s.name, s.category.as_str()),
            FieldValue::Ordinal(o) => format!("{}/{}", o.rank, o.scale),
            FieldValue::Reference(r) => r.clone(),
            FieldValue::Set(items) => {
                let v: Vec<&str> = items.iter().map(String::as_str).collect();
                format!("[{}]", v.join(", "))
            }
        }
    }

    /// Whether every number inside is finite.
    pub fn is_finite(&self) -> bool {
        match self {
            FieldValue::Number(n) => n.is_finite(),
            _ => true,
        }
    }
}

/// Parses an RFC 3339 timestamp, or a `YYYY-MM-DD` date as 00:00 UTC that day.
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let t = s.trim();
    if let Ok(at) = t.parse::<DateTime<FixedOffset>>() {
        return Some(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|midnight| midnight.and_utc())
}

fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;

    impl serde::de::Visitor<'_> for Visitor {
        type Value = DateTime<Utc>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an RFC 3339 timestamp or a YYYY-MM-DD date")
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
            parse_timestamp(s).ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(s), &self))
        }
    }

    deserializer.deserialize_str(Visitor)
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        let s = format!("{n:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// How two entities relate besides parent/child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// This entity must finish before the target can.
    Blocks,
    /// The target must finish before this entity can (the inverse of `Blocks`).
    BlockedBy,
    /// Loosely related.
    RelatesTo,
    /// The same work as the target.
    Duplicates,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Blocks => "blocks",
            RelationKind::BlockedBy => "blocked_by",
            RelationKind::RelatesTo => "relates_to",
            RelationKind::Duplicates => "duplicates",
        }
    }
}

/// A relation from the entity that carries it to `target`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    pub target: EntityId,
}

/// "This entity was planned against version `version` of `entity_id`" —
/// like a dependency requirement in a lockfile. A breaking (major) bump of
/// that entity makes the pin incompatible.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BasisPin {
    pub entity_id: EntityId,
    pub version: SemVer,
}

/// A versioned work item as it is at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub kind: EntityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntityId>,
    /// The entity's current version, if the caller tracks one.
    ///
    /// The [`crate::registry::Registry`] assigns versions itself: it honours
    /// this one only when it first records the entity (to import items that
    /// are already versioned; a re-created id takes the larger of this and the
    /// next major after its tombstone), and ignores it on later puts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<SemVer>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    /// The versions of other entities this one was planned against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basis: Vec<BasisPin>,
    /// A semantic embedding of the entity's text (title + description) from
    /// the caller's model. When both sides of a comparison carry one of the
    /// same dimension, text scores blend it with the lexical measures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Entity {
    pub fn new(id: impl Into<EntityId>, kind: impl Into<EntityKind>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into().normalized(),
            parent_id: None,
            version: None,
            fields: BTreeMap::new(),
            relations: Vec::new(),
            basis: Vec::new(),
            embedding: None,
            updated_at: None,
        }
    }

    pub fn with_parent(mut self, parent: impl Into<EntityId>) -> Self {
        self.parent_id = Some(parent.into());
        self
    }

    pub fn with_version(mut self, version: SemVer) -> Self {
        self.version = Some(version);
        self
    }

    pub fn with_field(mut self, name: impl Into<String>, value: FieldValue) -> Self {
        self.fields.insert(name.into(), value);
        self
    }

    pub fn with_title(self, title: impl Into<String>) -> Self {
        self.with_field(field::TITLE, FieldValue::Text(title.into()))
    }

    pub fn with_description(self, description: impl Into<String>) -> Self {
        self.with_field(field::DESCRIPTION, FieldValue::Text(description.into()))
    }

    pub fn with_status(self, category: StatusCategory, name: impl Into<String>) -> Self {
        self.with_field(field::STATUS, FieldValue::status(category, name))
    }

    pub fn with_estimate(self, estimate: f64) -> Self {
        self.with_field(field::ESTIMATE, FieldValue::Number(estimate))
    }

    pub fn with_assignee(self, assignee: impl Into<String>) -> Self {
        self.with_field(field::ASSIGNEE, FieldValue::Reference(assignee.into()))
    }

    pub fn with_due(self, due: DateTime<Utc>) -> Self {
        self.with_field(field::DUE_DATE, FieldValue::Timestamp(due))
    }

    pub fn with_relation(mut self, kind: RelationKind, target: impl Into<EntityId>) -> Self {
        self.relations.push(Relation {
            kind,
            target: target.into(),
        });
        self
    }

    pub fn with_basis(mut self, entity_id: impl Into<EntityId>, version: SemVer) -> Self {
        self.basis.push(BasisPin {
            entity_id: entity_id.into(),
            version,
        });
        self
    }

    pub fn field(&self, name: &str) -> Option<&FieldValue> {
        self.fields.get(name)
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        match self.fields.get(name) {
            Some(FieldValue::Text(s)) => Some(s),
            _ => None,
        }
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        match self.fields.get(name) {
            Some(FieldValue::Number(n)) if n.is_finite() => Some(*n),
            _ => None,
        }
    }

    pub fn timestamp(&self, name: &str) -> Option<DateTime<Utc>> {
        match self.fields.get(name) {
            Some(FieldValue::Timestamp(t)) => Some(*t),
            _ => None,
        }
    }

    pub fn reference(&self, name: &str) -> Option<&str> {
        match self.fields.get(name) {
            Some(FieldValue::Reference(r)) => Some(r),
            _ => None,
        }
    }

    /// The workflow status, if the entity has a `status` field of type status.
    pub fn status(&self) -> Option<&Status> {
        match self.fields.get(field::STATUS) {
            Some(FieldValue::Status(s)) => Some(s),
            _ => None,
        }
    }

    pub fn status_category(&self) -> Option<StatusCategory> {
        self.status().map(|s| s.category)
    }

    /// Active unless the status says completed or canceled (no status = active).
    pub fn is_active(&self) -> bool {
        self.status_category().is_none_or(StatusCategory::is_active)
    }

    pub fn estimate(&self) -> Option<f64> {
        self.number(field::ESTIMATE).filter(|e| *e >= 0.0)
    }

    pub fn due(&self) -> Option<DateTime<Utc>> {
        self.timestamp(field::DUE_DATE)
    }

    pub fn start(&self) -> Option<DateTime<Utc>> {
        self.timestamp(field::START_DATE)
    }

    pub fn assignee(&self) -> Option<&str> {
        self.reference(field::ASSIGNEE)
    }

    pub fn title(&self) -> &str {
        self.text(field::TITLE).unwrap_or("")
    }

    /// The text the entity is about: title and description.
    pub fn document(&self) -> String {
        let title = self.text(field::TITLE).unwrap_or("");
        let description = self.text(field::DESCRIPTION).unwrap_or("");
        match (title.is_empty(), description.is_empty()) {
            (true, true) => String::new(),
            (false, true) => title.to_owned(),
            (true, false) => description.to_owned(),
            (false, false) => format!("{title}\n{description}"),
        }
    }

    /// A label for explanations: the title when there is one, else the id.
    pub fn label(&self) -> String {
        let title = self.title().trim();
        if title.is_empty() {
            self.id.clone()
        } else {
            let short: String = title.chars().take(60).collect();
            if short.len() < title.len() {
                format!("\"{short}…\"")
            } else {
                format!("\"{short}\"")
            }
        }
    }

    /// The pin this entity holds on `other`, if any.
    pub fn pin_for(&self, other: &str) -> Option<&BasisPin> {
        self.basis.iter().find(|p| p.entity_id == other)
    }
}

/// What kind of activity an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    #[default]
    Comment,
    Mention,
    Attachment,
    Link,
    Other,
}

/// Something that happened on an entity without changing its fields: a
/// comment, a mention, a link. Its text is scanned for signals (blockers,
/// scope changes, risks, schedule slips, questions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// The entity the activity is on. May be empty (or, in JSON, absent)
    /// inside a [`ChangeSet`]: it is then the change set's entity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entity_id: EntityId,
    #[serde(default)]
    pub kind: ActivityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl ActivityEvent {
    pub fn comment(entity_id: impl Into<EntityId>, body: impl Into<String>) -> Self {
        Self {
            entity_id: entity_id.into(),
            kind: ActivityKind::Comment,
            author: None,
            body: body.into(),
            at: None,
        }
    }
}

/// One field's old and new value (absent = null), for callers that log
/// changes as deltas (like an issue timeline) rather than snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    #[serde(default)]
    pub before: Option<FieldValue>,
    #[serde(default)]
    pub after: Option<FieldValue>,
}

/// One entity's change.
///
/// * `before` and `after` → an update; only `after` → a creation; only
///   `before` → a deletion.
/// * `field_changes` (with `before` absent) → an update described as deltas:
///   `after` (or, when absent, the entity in the request's context) is the
///   new state, and `before` is derived by reverting each change.
/// * Neither, with `events` → activity on an entity found in the context.
///
/// `events` may accompany any of the above.
///
/// `entity_id` may be empty (or, in JSON, absent): it is then taken from
/// `after`, else `before`, else the first event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChangeSet {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entity_id: EntityId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Entity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Entity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_changes: Vec<FieldChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ActivityEvent>,
}

impl ChangeSet {
    pub fn update(before: Entity, after: Entity) -> Self {
        Self {
            entity_id: after.id.clone(),
            before: Some(before),
            after: Some(after),
            ..Self::default()
        }
    }

    pub fn create(after: Entity) -> Self {
        Self {
            entity_id: after.id.clone(),
            after: Some(after),
            ..Self::default()
        }
    }

    pub fn delete(before: Entity) -> Self {
        Self {
            entity_id: before.id.clone(),
            before: Some(before),
            ..Self::default()
        }
    }

    pub fn activity(entity_id: impl Into<EntityId>, events: Vec<ActivityEvent>) -> Self {
        Self {
            entity_id: entity_id.into(),
            events,
            ..Self::default()
        }
    }

    pub fn fields(after: Entity, changes: Vec<FieldChange>) -> Self {
        Self {
            entity_id: after.id.clone(),
            after: Some(after),
            field_changes: changes,
            ..Self::default()
        }
    }

    pub fn with_events(mut self, events: Vec<ActivityEvent>) -> Self {
        self.events.extend(events);
        self
    }
}

/// A past change of an entity, for volatility and anomaly statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    pub entity_id: EntityId,
    pub at: DateTime<Utc>,
    /// The raw (content) score of that change, in `[0, 1]`.
    pub magnitude: f64,
}

/// Knobs for one assessment that are not policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssessOptions {
    /// At most this many entries in [`crate::assessment::Assessment::impacts`]
    /// (`0` = all). The total is always reported.
    pub top_k: usize,
    /// Include each impact's contributions (the paths the impact came along).
    pub explain: bool,
    /// At most this many contributions per impact.
    pub max_contributions: usize,
}

impl Default for AssessOptions {
    fn default() -> Self {
        Self {
            top_k: 100,
            explain: true,
            max_contributions: 5,
        }
    }
}

/// Everything one assessment needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AssessmentRequest {
    /// The changes, assessed together (their impacts combine).
    pub changes: Vec<ChangeSet>,
    /// The current state of related entities: the parent chain, children,
    /// siblings, blockers and dependents. The more of the neighbourhood is
    /// here, the further the engine can see (and the higher its confidence).
    #[serde(default)]
    pub context: Vec<Entity>,
    /// Past changes, for volatility and anomaly statistics.
    #[serde(default)]
    pub history: Vec<HistoryPoint>,
    /// The reference time for schedule pressure; defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<DateTime<Utc>>,
    #[serde(default)]
    pub options: AssessOptions,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::HashSet;

    /// Finding 42: a kind is its name, so a `Custom` holding a well-known
    /// name round-trips through serde to an equal value.
    #[test]
    fn entity_kind_equality_follows_its_name() {
        let custom = EntityKind::Custom("issue".to_owned());
        let json = serde_json::to_string(&custom).expect("serializes");
        let back: EntityKind = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, EntityKind::Issue);
        assert_eq!(back, custom);
        assert_eq!(
            EntityKind::Custom(" Sub-Issue ".to_owned()),
            EntityKind::SubIssue
        );
        assert_eq!(
            EntityKind::Custom(" Sub-Issue ".to_owned()).as_str(),
            "sub_issue"
        );
        assert_ne!(
            EntityKind::Custom("bug".to_owned()),
            EntityKind::Custom("Bug".to_owned())
        );
        let set: HashSet<EntityKind> = [custom.clone(), EntityKind::Issue].into_iter().collect();
        assert_eq!(set.len(), 1);
        assert_eq!(custom.cmp(&EntityKind::Issue), Ordering::Equal);
        assert!(matches!(custom.normalized(), EntityKind::Issue));
        assert!(matches!(
            Entity::new("X", EntityKind::Custom("task".to_owned())).kind,
            EntityKind::Task
        ));
        assert_eq!(EntityKind::from("Whatever").as_str(), "Whatever");
        assert!(matches!(EntityKind::from("subissue"), EntityKind::SubIssue));

        let e = Entity::new("ENG-9", "issue");
        let mut live = e.clone();
        live.kind = EntityKind::Custom("issue".to_owned());
        let replayed: Entity =
            serde_json::from_str(&serde_json::to_string(&live).expect("serializes"))
                .expect("deserializes");
        assert_eq!(replayed, live);
    }

    /// Finding 44: JSON may leave out the entity id where the engine infers it.
    #[test]
    fn entity_ids_may_be_left_out_in_json() {
        let cs: ChangeSet = serde_json::from_str(
            r#"{"after":{"id":"ENG-1","kind":"issue"},"events":[{"body":"blocked"}]}"#,
        )
        .expect("a change set without entity ids parses");
        assert!(cs.entity_id.is_empty());
        assert!(cs.events[0].entity_id.is_empty());
        let json = serde_json::to_string(&cs).expect("serializes");
        assert!(!json.contains("entity_id"));
        let back: ChangeSet = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, cs);
        let named = ChangeSet::activity("ENG-1", vec![ActivityEvent::comment("ENG-1", "hi")]);
        let back: ChangeSet =
            serde_json::from_str(&serde_json::to_string(&named).expect("serializes"))
                .expect("round-trips");
        assert_eq!(back, named);
    }

    /// Finding 45: a date-only due date is read as 00:00 UTC.
    #[test]
    fn timestamps_accept_plain_dates() {
        let midnight = Utc
            .with_ymd_and_hms(2026, 11, 20, 0, 0, 0)
            .single()
            .expect("valid");
        let date: FieldValue = serde_json::from_str(r#"{"type":"timestamp","value":"2026-11-20"}"#)
            .expect("a plain date parses");
        assert_eq!(date, FieldValue::Timestamp(midnight));
        let full: FieldValue =
            serde_json::from_str(r#"{"type":"timestamp","value":"2026-11-20T00:00:00Z"}"#)
                .expect("RFC 3339 parses");
        assert_eq!(full, date);
        let offset: FieldValue =
            serde_json::from_str(r#"{"type":"timestamp","value":"2026-11-20T08:00:00+08:00"}"#)
                .expect("an offset parses");
        assert_eq!(offset, date);
        assert_eq!(
            serde_json::to_string(&date).expect("serializes"),
            r#"{"type":"timestamp","value":"2026-11-20T00:00:00Z"}"#
        );
        for bad in ["2026-13-01", "20 Nov 2026", "", "2026-11-20T25:00:00Z"] {
            let json = format!(r#"{{"type":"timestamp","value":"{bad}"}}"#);
            assert!(
                serde_json::from_str::<FieldValue>(&json).is_err(),
                "`{bad}` must be rejected"
            );
        }
        let e: Entity = serde_json::from_str(
            r#"{"id":"ENG-1","kind":"issue","fields":{"due_date":{"type":"timestamp","value":"2026-11-20"}}}"#,
        )
        .expect("an entity with a date-only due date parses");
        assert_eq!(e.due(), Some(midnight));
    }
}
