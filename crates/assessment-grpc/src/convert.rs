//! Lossless conversions between the protobuf messages ([`crate::pb`]) and the
//! core types of [`assessment_core`].
//!
//! * **Core → protobuf** is infallible: `From<core::T> for pb::T` (or a
//!   `*_to_pb` function where several core values make one message).
//! * **Protobuf → core** is fallible: `TryFrom<pb::T> for core::T` (or a
//!   `*_from_pb` function) returning a [`ConvertError`] whose message names
//!   the offending field precisely, e.g.
//!   `changes[2].after.fields["status"]: status category is unspecified`.
//!   It maps to [`tonic::Status::invalid_argument`].
//!
//! Rules, as documented in the proto:
//!
//! * An absent `FieldValue`, or one with no `kind`, is null: the field is
//!   omitted from [`Entity::fields`] (and is `None` in a [`FieldChange`]).
//! * `*_UNSPECIFIED` values of required enums are errors, except
//!   `ACTIVITY_KIND_UNSPECIFIED`, which means a comment.
//! * An empty `embedding` is no embedding; an empty `author` is no author.
//! * An absent `AssessOptions` (or an unset field of it) takes the core
//!   defaults: `top_k` 100, `explain` true, `max_contributions` 5, `judge`
//!   unset, `judgment_texts` false.
//! * A `TextJudgment` in a request is checked strictly. Its `relation` must
//!   be set, and its `confidence` must be finite and in `[0, 1]`; an unset
//!   confidence is 1. The error names the field, e.g.
//!   `changes[0].judgments[1].relation: text relation is unspecified`.
//! * Judgment data in a response is read leniently, so that an older client
//!   can read a newer server. An unknown `JudgmentStatus` becomes
//!   `unavailable` with a `detail` that names the value. An unknown or
//!   unspecified relation of a `FieldJudgment` is no relation. An unknown
//!   relation of a stored `TextJudgment` is `undetermined`. Unknown reason
//!   strings are dropped.
//! * Timestamps must be normalized: `nanos` in `[0, 999_999_999]` and
//!   seconds within chrono's range.
//! * `google.protobuf.Struct` numbers are doubles; whole numbers (up to
//!   2^53 in magnitude) become JSON integers so that integer policy fields
//!   (such as `propagation.max_depth`) deserialize. Non-finite numbers are
//!   rejected.
//!
//! Non-finite numbers in the input model are passed through: the engine
//! rejects them with its own message.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use assessment_core::assessment::{
    Action, Assessment, BasisStatus, ChangeAssessment, ChangeType, Channel, Contribution,
    Direction, EntityImpact, FieldAssessment, FieldChangeKind, FieldJudgment, ImpactLevel,
    JudgmentKey, JudgmentReason, JudgmentRequest, JudgmentStatus, Modifier, Recommendation, Signal,
    SignalCategory, Violation, ViolationKind,
};
use assessment_core::model::{
    ActivityEvent, ActivityKind, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity,
    EntityKind, FieldChange, FieldValue, HistoryPoint, Ordinal, Relation, RelationKind, Status,
    StatusCategory, TextJudgment, TextRelation,
};
use assessment_core::policy::Policy;
use assessment_core::registry::{
    DeleteOptions, DeleteOutcome, ImportFailure, ImportItem, ImportSummary, PutOptions, PutOutcome,
    StaleEntry, VersionRecord,
};
use assessment_core::semver::{SemVer, VersionBump};
use assessment_core::text::{GrayZone, StructureDiff, TextDiff};
use chrono::{DateTime, Utc};
use prost_types::value::Kind as JsonKind;

use crate::pb;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// A protobuf message that does not describe a valid core value.
///
/// Its [`Display`](fmt::Display) is `path: message`, e.g.
/// `changes[2].after.fields["status"]: status category is unspecified`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    /// Innermost segment first (segments are pushed while unwinding).
    path: Vec<Segment>,
    message: Cow<'static, str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Field(&'static str),
    Index(usize),
    Key(String),
}

impl ConvertError {
    pub(crate) fn new(message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            path: Vec::new(),
            message: message.into(),
        }
    }

    fn required() -> Self {
        Self::new("is required")
    }

    /// The path of the offending field, e.g. `changes[2].after.fields["status"]`
    /// (empty when the whole message is at fault).
    pub fn path(&self) -> String {
        let mut out = String::new();
        for segment in self.path.iter().rev() {
            match segment {
                Segment::Field(name) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(name);
                }
                Segment::Index(i) => {
                    out.push('[');
                    out.push_str(&i.to_string());
                    out.push(']');
                }
                Segment::Key(key) => {
                    out.push('[');
                    out.push_str(&format!("{key:?}"));
                    out.push(']');
                }
            }
        }
        out
    }

    /// What is wrong with the field (lowercase, without the path).
    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn field(mut self, name: &'static str) -> Self {
        self.path.push(Segment::Field(name));
        self
    }

    fn index(mut self, i: usize) -> Self {
        self.path.push(Segment::Index(i));
        self
    }

    fn key(mut self, key: &str) -> Self {
        self.path.push(Segment::Key(key.to_owned()));
        self
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path(), self.message)
        }
    }
}

impl std::error::Error for ConvertError {}

impl From<ConvertError> for tonic::Status {
    fn from(err: ConvertError) -> Self {
        tonic::Status::invalid_argument(err.to_string())
    }
}

type Result<T, E = ConvertError> = std::result::Result<T, E>;

/// Path context for conversion results.
trait At<T> {
    fn at(self, name: &'static str) -> Result<T>;
}

impl<T> At<T> for Result<T> {
    fn at(self, name: &'static str) -> Result<T> {
        self.map_err(|e| e.field(name))
    }
}

/// Converts every item, prefixing errors with the item's index.
fn list<T, U>(items: Vec<T>, mut f: impl FnMut(T) -> Result<U>) -> Result<Vec<U>> {
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| f(item).map_err(|e| e.index(i)))
        .collect()
}

fn required<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(ConvertError::required)
}

fn opt<T, U>(value: Option<T>, f: impl FnOnce(T) -> Result<U>) -> Result<Option<U>> {
    value.map(f).transpose()
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Generates `fn $from(i32) -> Result<Core>` (unspecified and unknown values
/// are errors) and `fn $to(Core) -> i32`, both exhaustive.
macro_rules! enum_conversions {
    ($(
        $from:ident, $to:ident: $core:ident <=> pb::$pb:ident, $what:literal {
            $($c:ident <=> $p:ident),+ $(,)?
        }
    )+) => {$(
        fn $from(value: i32) -> Result<$core> {
            match pb::$pb::try_from(value) {
                $(Ok(pb::$pb::$p) => Ok($core::$c),)+
                Ok(pb::$pb::Unspecified) => Err(ConvertError::new(concat!($what, " is unspecified"))),
                Err(_) => Err(ConvertError::new(format!(
                    concat!($what, " has the unknown value {}"),
                    value
                ))),
            }
        }

        fn $to(value: $core) -> i32 {
            match value {
                $($core::$c => pb::$pb::$p.into(),)+
            }
        }
    )+};
}

enum_conversions! {
    status_category_from_pb, status_category_to_pb: StatusCategory <=> pb::StatusCategory, "status category" {
        Backlog <=> Backlog,
        Unstarted <=> Unstarted,
        Started <=> Started,
        Completed <=> Completed,
        Canceled <=> Canceled,
    }
    relation_kind_from_pb, relation_kind_to_pb: RelationKind <=> pb::RelationKind, "relation kind" {
        Blocks <=> Blocks,
        BlockedBy <=> BlockedBy,
        RelatesTo <=> RelatesTo,
        Duplicates <=> Duplicates,
    }
    impact_level_from_pb, impact_level_to_pb: ImpactLevel <=> pb::ImpactLevel, "impact level" {
        None <=> None,
        Low <=> Low,
        Medium <=> Medium,
        High <=> High,
        Critical <=> Critical,
    }
    recommendation_from_pb, recommendation_to_pb: Recommendation <=> pb::Recommendation, "recommendation" {
        NoAction <=> NoAction,
        Notify <=> Notify,
        Review <=> Review,
        PartialReplan <=> PartialReplan,
        FullReplan <=> FullReplan,
    }
    action_from_pb, action_to_pb: Action <=> pb::Action, "action" {
        None <=> None,
        Notify <=> Notify,
        Review <=> Review,
        UpdateScope <=> UpdateScope,
        ReEstimate <=> ReEstimate,
        Reschedule <=> Reschedule,
        Reverify <=> Reverify,
        ObsoleteCandidate <=> ObsoleteCandidate,
        CancelOrRehome <=> CancelOrRehome,
        Reprioritize <=> Reprioritize,
        Replan <=> Replan,
        ResolveConflict <=> ResolveConflict,
    }
    change_type_from_pb, change_type_to_pb: ChangeType <=> pb::ChangeType, "change type" {
        Created <=> Created,
        Updated <=> Updated,
        Deleted <=> Deleted,
        Activity <=> Activity,
    }
    channel_from_pb, channel_to_pb: Channel <=> pb::Channel, "channel" {
        SelfChange <=> Self_,
        Down <=> Down,
        Up <=> Up,
        Dependency <=> Dependency,
        Upstream <=> Upstream,
        Related <=> Related,
        Sibling <=> Sibling,
    }
    basis_status_from_pb, basis_status_to_pb: BasisStatus <=> pb::BasisStatus, "basis status" {
        NotPinned <=> NotPinned,
        Current <=> Current,
        Compatible <=> Compatible,
        ReviewSuggested <=> ReviewSuggested,
        Incompatible <=> Incompatible,
        Removed <=> Removed,
    }
    version_bump_from_pb, version_bump_to_pb: VersionBump <=> pb::VersionBump, "version bump" {
        None <=> None,
        Patch <=> Patch,
        Minor <=> Minor,
        Major <=> Major,
        Initial <=> Initial,
    }
    field_change_kind_from_pb, field_change_kind_to_pb: FieldChangeKind <=> pb::FieldChangeKind, "field change kind" {
        Added <=> Added,
        Removed <=> Removed,
        Modified <=> Modified,
    }
    direction_from_pb, direction_to_pb: Direction <=> pb::Direction, "direction" {
        None <=> None,
        Increase <=> Increase,
        Decrease <=> Decrease,
    }
    signal_category_from_pb, signal_category_to_pb: SignalCategory <=> pb::SignalCategory, "signal category" {
        Blocker <=> Blocker,
        Scope <=> Scope,
        Risk <=> Risk,
        Schedule <=> Schedule,
        Uncertainty <=> Uncertainty,
        Resolution <=> Resolution,
    }
    violation_kind_from_pb, violation_kind_to_pb: ViolationKind <=> pb::ViolationKind, "violation kind" {
        OrphanedWork <=> OrphanedWork,
        CyclicDependency <=> CyclicDependency,
        ScheduleConflict <=> ScheduleConflict,
        DependencyConflict <=> DependencyConflict,
        BlockedByClosed <=> BlockedByClosed,
        PrematureClose <=> PrematureClose,
        EstimateOverflow <=> EstimateOverflow,
        Overdue <=> Overdue,
    }
    text_relation_from_pb, text_relation_to_pb: TextRelation <=> pb::TextRelation, "text relation" {
        Equivalent <=> Equivalent,
        Extended <=> Extended,
        Narrowed <=> Narrowed,
        Replaced <=> Replaced,
        Reversed <=> Reversed,
        Undetermined <=> Undetermined,
    }
}

fn judgment_status_to_pb(status: JudgmentStatus) -> i32 {
    match status {
        JudgmentStatus::Requested => pb::JudgmentStatus::Requested,
        JudgmentStatus::OverBudget => pb::JudgmentStatus::OverBudget,
        JudgmentStatus::Applied => pb::JudgmentStatus::Applied,
        JudgmentStatus::Stale => pb::JudgmentStatus::Stale,
        JudgmentStatus::Inconclusive => pb::JudgmentStatus::Inconclusive,
        JudgmentStatus::NotFlagged => pb::JudgmentStatus::NotFlagged,
        JudgmentStatus::Unavailable => pb::JudgmentStatus::Unavailable,
    }
    .into()
}

/// A status this build does not know (or an unspecified one) is `None`.
fn judgment_status_from_pb(value: i32) -> Option<JudgmentStatus> {
    match pb::JudgmentStatus::try_from(value).ok()? {
        pb::JudgmentStatus::Unspecified => None,
        pb::JudgmentStatus::Requested => Some(JudgmentStatus::Requested),
        pb::JudgmentStatus::OverBudget => Some(JudgmentStatus::OverBudget),
        pb::JudgmentStatus::Applied => Some(JudgmentStatus::Applied),
        pb::JudgmentStatus::Stale => Some(JudgmentStatus::Stale),
        pb::JudgmentStatus::Inconclusive => Some(JudgmentStatus::Inconclusive),
        pb::JudgmentStatus::NotFlagged => Some(JudgmentStatus::NotFlagged),
        pb::JudgmentStatus::Unavailable => Some(JudgmentStatus::Unavailable),
    }
}

/// The wire name of every reason. Reasons are strings on the wire so that
/// new ones can be added.
const JUDGMENT_REASONS: [JudgmentReason; 5] = [
    JudgmentReason::NegationChanged,
    JudgmentReason::ScopeQualifier,
    JudgmentReason::TermSubstitution,
    JudgmentReason::ParaphraseSuspected,
    JudgmentReason::NearThreshold,
];

fn reasons_to_pb(reasons: Vec<JudgmentReason>) -> Vec<String> {
    reasons.into_iter().map(|r| r.as_str().to_owned()).collect()
}

/// Unknown reasons are dropped.
fn reasons_from_pb(reasons: Vec<String>) -> Vec<JudgmentReason> {
    reasons
        .iter()
        .filter_map(|name| JUDGMENT_REASONS.into_iter().find(|r| r.as_str() == name))
        .collect()
}

/// `ACTIVITY_KIND_UNSPECIFIED` means a comment, as the core default does.
fn activity_kind_from_pb(value: i32) -> Result<ActivityKind> {
    match pb::ActivityKind::try_from(value) {
        Ok(pb::ActivityKind::Unspecified | pb::ActivityKind::Comment) => Ok(ActivityKind::Comment),
        Ok(pb::ActivityKind::Mention) => Ok(ActivityKind::Mention),
        Ok(pb::ActivityKind::Attachment) => Ok(ActivityKind::Attachment),
        Ok(pb::ActivityKind::Link) => Ok(ActivityKind::Link),
        Ok(pb::ActivityKind::Other) => Ok(ActivityKind::Other),
        Err(_) => Err(ConvertError::new(format!(
            "activity kind has the unknown value {value}"
        ))),
    }
}

fn activity_kind_to_pb(kind: ActivityKind) -> i32 {
    match kind {
        ActivityKind::Comment => pb::ActivityKind::Comment,
        ActivityKind::Mention => pb::ActivityKind::Mention,
        ActivityKind::Attachment => pb::ActivityKind::Attachment,
        ActivityKind::Link => pb::ActivityKind::Link,
        ActivityKind::Other => pb::ActivityKind::Other,
    }
    .into()
}

// ---------------------------------------------------------------------------
// Well-known types
// ---------------------------------------------------------------------------

/// A protobuf timestamp as a UTC time. `nanos` must be in `[0, 999_999_999]`.
pub fn timestamp_from_pb(ts: prost_types::Timestamp) -> Result<DateTime<Utc>, ConvertError> {
    let nanos = u32::try_from(ts.nanos)
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .ok_or_else(|| {
            ConvertError::new(format!(
                "timestamp nanos {} are outside [0, 999999999]",
                ts.nanos
            ))
        })?;
    DateTime::from_timestamp(ts.seconds, nanos).ok_or_else(|| {
        ConvertError::new(format!("timestamp seconds {} are out of range", ts.seconds))
    })
}

/// A UTC time as a protobuf timestamp. A leap second (chrono's nanos ≥ 1e9)
/// is clamped to the last nanosecond of the second before it.
pub fn timestamp_to_pb(time: DateTime<Utc>) -> prost_types::Timestamp {
    let nanos = time.timestamp_subsec_nanos().min(999_999_999);
    prost_types::Timestamp {
        seconds: time.timestamp(),
        nanos: i32::try_from(nanos).unwrap_or(999_999_999),
    }
}

fn ts_opt(ts: Option<prost_types::Timestamp>) -> Result<Option<DateTime<Utc>>> {
    opt(ts, timestamp_from_pb)
}

/// The largest magnitude below which every whole double is exact.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

/// A `google.protobuf.Struct` as a JSON object.
pub fn struct_to_json(value: prost_types::Struct) -> Result<serde_json::Value, ConvertError> {
    struct_to_map(value).map(serde_json::Value::Object)
}

/// A `google.protobuf.Struct` as a JSON map.
pub fn struct_to_map(
    value: prost_types::Struct,
) -> Result<serde_json::Map<String, serde_json::Value>, ConvertError> {
    let mut map = serde_json::Map::new();
    for (key, v) in value.fields {
        let json = value_to_json(v).map_err(|e| e.key(&key))?;
        map.insert(key, json);
    }
    Ok(map)
}

fn value_to_json(value: prost_types::Value) -> Result<serde_json::Value> {
    Ok(match value.kind {
        None | Some(JsonKind::NullValue(_)) => serde_json::Value::Null,
        Some(JsonKind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(JsonKind::StringValue(s)) => serde_json::Value::String(s),
        Some(JsonKind::NumberValue(n)) => number_to_json(n)?,
        Some(JsonKind::StructValue(s)) => struct_to_json(s)?,
        Some(JsonKind::ListValue(l)) => serde_json::Value::Array(list(l.values, value_to_json)?),
    })
}

fn number_to_json(n: f64) -> Result<serde_json::Value> {
    if !n.is_finite() {
        return Err(ConvertError::new(format!("number {n} is not finite")));
    }
    if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER {
        // Exact: |n| ≤ 2^53 and whole.
        let i = n as i64;
        return Ok(serde_json::Value::from(i));
    }
    serde_json::Number::from_f64(n)
        .map(serde_json::Value::Number)
        .ok_or_else(|| ConvertError::new(format!("number {n} is not finite")))
}

/// A JSON object as a `google.protobuf.Struct`. Anything but an object is an error.
pub fn json_to_struct(value: serde_json::Value) -> Result<prost_types::Struct, ConvertError> {
    match value {
        serde_json::Value::Object(map) => Ok(map_to_struct(map)),
        other => Err(ConvertError::new(format!(
            "a struct must be a json object, not {}",
            json_type(&other)
        ))),
    }
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// A JSON map as a `google.protobuf.Struct`.
pub fn map_to_struct(map: serde_json::Map<String, serde_json::Value>) -> prost_types::Struct {
    prost_types::Struct {
        fields: map
            .into_iter()
            .map(|(k, v)| (k, json_to_value(v)))
            .collect(),
    }
}

fn json_to_value(value: serde_json::Value) -> prost_types::Value {
    let kind = match value {
        serde_json::Value::Null => JsonKind::NullValue(prost_types::NullValue::NullValue.into()),
        serde_json::Value::Bool(b) => JsonKind::BoolValue(b),
        serde_json::Value::Number(n) => JsonKind::NumberValue(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => JsonKind::StringValue(s),
        serde_json::Value::Array(items) => JsonKind::ListValue(prost_types::ListValue {
            values: items.into_iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(map) => JsonKind::StructValue(map_to_struct(map)),
    };
    prost_types::Value { kind: Some(kind) }
}

/// A policy as a `google.protobuf.Struct`.
pub fn policy_to_struct(policy: &Policy) -> Result<prost_types::Struct, ConvertError> {
    let json = serde_json::to_value(policy)
        .map_err(|e| ConvertError::new(format!("the policy could not be serialized: {e}")))?;
    json_to_struct(json)
}

// ---------------------------------------------------------------------------
// Input model
// ---------------------------------------------------------------------------

impl From<SemVer> for pb::SemVer {
    fn from(v: SemVer) -> Self {
        Self {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
        }
    }
}

impl From<pb::SemVer> for SemVer {
    fn from(v: pb::SemVer) -> Self {
        SemVer::new(v.major, v.minor, v.patch)
    }
}

impl From<FieldValue> for pb::FieldValue {
    fn from(value: FieldValue) -> Self {
        use pb::field_value::Kind;
        let kind = match value {
            FieldValue::Text(s) => Kind::Text(s),
            FieldValue::Number(n) => Kind::Number(n),
            FieldValue::Bool(b) => Kind::Flag(b),
            FieldValue::Timestamp(t) => Kind::Timestamp(timestamp_to_pb(t)),
            FieldValue::Status(s) => Kind::Status(pb::Status {
                category: status_category_to_pb(s.category),
                name: s.name,
            }),
            FieldValue::Ordinal(o) => Kind::Ordinal(pb::Ordinal {
                rank: o.rank,
                scale: o.scale,
            }),
            FieldValue::Reference(r) => Kind::Reference(r),
            FieldValue::Set(items) => Kind::Set(pb::StringSet {
                values: items.into_iter().collect(),
            }),
        };
        Self { kind: Some(kind) }
    }
}

/// A protobuf field value as a core one; `None` when it is null (no `kind`).
pub fn field_value_from_pb(value: pb::FieldValue) -> Result<Option<FieldValue>, ConvertError> {
    use pb::field_value::Kind;
    let Some(kind) = value.kind else {
        return Ok(None);
    };
    Ok(Some(match kind {
        Kind::Text(s) => FieldValue::Text(s),
        Kind::Number(n) => FieldValue::Number(n),
        Kind::Flag(b) => FieldValue::Bool(b),
        Kind::Timestamp(t) => FieldValue::Timestamp(timestamp_from_pb(t)?),
        Kind::Status(s) => FieldValue::Status(Status {
            category: status_category_from_pb(s.category)?,
            name: s.name,
        }),
        Kind::Ordinal(o) => FieldValue::Ordinal(Ordinal {
            rank: o.rank,
            scale: o.scale,
        }),
        Kind::Reference(r) => FieldValue::Reference(r),
        Kind::Set(s) => FieldValue::Set(s.values.into_iter().collect::<BTreeSet<_>>()),
    }))
}

fn field_value_opt(value: Option<pb::FieldValue>) -> Result<Option<FieldValue>> {
    Ok(opt(value, field_value_from_pb)?.flatten())
}

impl From<Relation> for pb::Relation {
    fn from(r: Relation) -> Self {
        Self {
            kind: relation_kind_to_pb(r.kind),
            target_id: r.target,
        }
    }
}

impl TryFrom<pb::Relation> for Relation {
    type Error = ConvertError;

    fn try_from(r: pb::Relation) -> Result<Self> {
        Ok(Relation {
            kind: relation_kind_from_pb(r.kind).at("kind")?,
            target: r.target_id,
        })
    }
}

impl From<BasisPin> for pb::BasisPin {
    fn from(p: BasisPin) -> Self {
        Self {
            entity_id: p.entity_id,
            version: Some(p.version.into()),
        }
    }
}

impl TryFrom<pb::BasisPin> for BasisPin {
    type Error = ConvertError;

    fn try_from(p: pb::BasisPin) -> Result<Self> {
        Ok(BasisPin {
            entity_id: p.entity_id,
            version: required(p.version).at("version")?.into(),
        })
    }
}

impl From<Entity> for pb::Entity {
    fn from(e: Entity) -> Self {
        Self {
            id: e.id,
            kind: String::from(e.kind),
            parent_id: e.parent_id,
            version: e.version.map(Into::into),
            fields: e
                .fields
                .into_iter()
                .map(|(name, value)| (name, value.into()))
                .collect(),
            relations: e.relations.into_iter().map(Into::into).collect(),
            basis: e.basis.into_iter().map(Into::into).collect(),
            embedding: e.embedding.unwrap_or_default(),
            updated_at: e.updated_at.map(timestamp_to_pb),
        }
    }
}

impl TryFrom<pb::Entity> for Entity {
    type Error = ConvertError;

    fn try_from(e: pb::Entity) -> Result<Self> {
        let mut fields = BTreeMap::new();
        for (name, value) in e.fields {
            let value = field_value_from_pb(value)
                .map_err(|err| err.key(&name))
                .at("fields")?;
            if let Some(value) = value {
                fields.insert(name, value);
            }
        }
        Ok(Entity {
            id: e.id,
            kind: EntityKind::from(e.kind),
            parent_id: e.parent_id,
            version: e.version.map(Into::into),
            fields,
            relations: list(e.relations, Relation::try_from).at("relations")?,
            basis: list(e.basis, BasisPin::try_from).at("basis")?,
            embedding: (!e.embedding.is_empty()).then_some(e.embedding),
            updated_at: ts_opt(e.updated_at).at("updated_at")?,
        })
    }
}

fn entity_opt(e: Option<pb::Entity>) -> Result<Option<Entity>> {
    opt(e, Entity::try_from)
}

impl From<ActivityEvent> for pb::ActivityEvent {
    fn from(e: ActivityEvent) -> Self {
        Self {
            entity_id: e.entity_id,
            kind: activity_kind_to_pb(e.kind),
            author: e.author.unwrap_or_default(),
            body: e.body,
            at: e.at.map(timestamp_to_pb),
        }
    }
}

impl TryFrom<pb::ActivityEvent> for ActivityEvent {
    type Error = ConvertError;

    fn try_from(e: pb::ActivityEvent) -> Result<Self> {
        Ok(ActivityEvent {
            entity_id: e.entity_id,
            kind: activity_kind_from_pb(e.kind).at("kind")?,
            author: non_empty(e.author),
            body: e.body,
            at: ts_opt(e.at).at("at")?,
        })
    }
}

impl From<FieldChange> for pb::FieldChange {
    fn from(c: FieldChange) -> Self {
        Self {
            field: c.field,
            before: c.before.map(Into::into),
            after: c.after.map(Into::into),
        }
    }
}

impl TryFrom<pb::FieldChange> for FieldChange {
    type Error = ConvertError;

    fn try_from(c: pb::FieldChange) -> Result<Self> {
        Ok(FieldChange {
            field: c.field,
            before: field_value_opt(c.before).at("before")?,
            after: field_value_opt(c.after).at("after")?,
        })
    }
}

impl From<ChangeSet> for pb::ChangeSet {
    fn from(c: ChangeSet) -> Self {
        Self {
            entity_id: c.entity_id,
            before: c.before.map(Into::into),
            after: c.after.map(Into::into),
            field_changes: c.field_changes.into_iter().map(Into::into).collect(),
            events: c.events.into_iter().map(Into::into).collect(),
            judgments: c.judgments.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<TextJudgment> for pb::TextJudgment {
    fn from(j: TextJudgment) -> Self {
        Self {
            field: j.field,
            fingerprint: j.fingerprint,
            relation: text_relation_to_pb(j.relation),
            confidence: Some(j.confidence),
            rationale: j.rationale,
            source: j.source,
        }
    }
}

/// The strict reading, for requests: the relation must be set, and the
/// confidence must be finite and in `[0, 1]` (unset = 1).
impl TryFrom<pb::TextJudgment> for TextJudgment {
    type Error = ConvertError;

    fn try_from(j: pb::TextJudgment) -> Result<Self> {
        let confidence = j.confidence.unwrap_or(1.0);
        if !confidence.is_finite() {
            return Err(ConvertError::new("confidence is not finite").field("confidence"));
        }
        if !(0.0..=1.0).contains(&confidence) {
            return Err(
                ConvertError::new(format!("confidence {confidence} is outside [0, 1]"))
                    .field("confidence"),
            );
        }
        Ok(TextJudgment {
            field: j.field,
            fingerprint: j.fingerprint,
            relation: text_relation_from_pb(j.relation).at("relation")?,
            confidence,
            rationale: j.rationale,
            source: j.source,
        })
    }
}

/// The lenient reading, for responses: a relation this build does not know
/// is `undetermined`, and the confidence is taken as it is.
fn stored_judgment_from_pb(j: pb::TextJudgment) -> TextJudgment {
    TextJudgment {
        field: j.field,
        fingerprint: j.fingerprint,
        relation: text_relation_from_pb(j.relation).unwrap_or(TextRelation::Undetermined),
        confidence: j.confidence.unwrap_or(1.0),
        rationale: j.rationale,
        source: j.source,
    }
}

/// The judgments of a request message, checked strictly. Errors are
/// prefixed with `name[i]`.
pub(crate) fn judgments_from_pb(
    judgments: Vec<pb::TextJudgment>,
    name: &'static str,
) -> Result<Vec<TextJudgment>> {
    list(judgments, TextJudgment::try_from).at(name)
}

impl TryFrom<pb::ChangeSet> for ChangeSet {
    type Error = ConvertError;

    fn try_from(c: pb::ChangeSet) -> Result<Self> {
        Ok(ChangeSet {
            judgments: judgments_from_pb(c.judgments, "judgments")?,
            entity_id: c.entity_id,
            before: entity_opt(c.before).at("before")?,
            after: entity_opt(c.after).at("after")?,
            field_changes: list(c.field_changes, FieldChange::try_from).at("field_changes")?,
            events: list(c.events, ActivityEvent::try_from).at("events")?,
        })
    }
}

impl From<HistoryPoint> for pb::HistoryPoint {
    fn from(h: HistoryPoint) -> Self {
        Self {
            entity_id: h.entity_id,
            at: Some(timestamp_to_pb(h.at)),
            magnitude: h.magnitude,
        }
    }
}

impl TryFrom<pb::HistoryPoint> for HistoryPoint {
    type Error = ConvertError;

    fn try_from(h: pb::HistoryPoint) -> Result<Self> {
        Ok(HistoryPoint {
            entity_id: h.entity_id,
            at: timestamp_from_pb(required(h.at).at("at")?).at("at")?,
            magnitude: h.magnitude,
        })
    }
}

impl From<AssessOptions> for pb::AssessOptions {
    fn from(o: AssessOptions) -> Self {
        Self {
            top_k: Some(to_u32(o.top_k)),
            explain: Some(o.explain),
            max_contributions: Some(to_u32(o.max_contributions)),
            judge: o.judge,
            judgment_texts: o.judgment_texts,
        }
    }
}

impl From<pb::AssessOptions> for AssessOptions {
    fn from(o: pb::AssessOptions) -> Self {
        let defaults = AssessOptions::default();
        AssessOptions {
            judge: o.judge,
            judgment_texts: o.judgment_texts,
            top_k: o.top_k.map_or(defaults.top_k, to_usize),
            explain: o.explain.unwrap_or(defaults.explain),
            max_contributions: o
                .max_contributions
                .map_or(defaults.max_contributions, to_usize),
        }
    }
}

#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) fn options_from_pb(o: Option<pb::AssessOptions>) -> AssessOptions {
    o.map(Into::into).unwrap_or_default()
}

/// The request id and the policy overrides are not part of the core request:
/// they are empty here (see [`AssessInput`] for the other direction).
impl From<AssessmentRequest> for pb::AssessRequest {
    fn from(r: AssessmentRequest) -> Self {
        Self {
            request_id: String::new(),
            changes: r.changes.into_iter().map(Into::into).collect(),
            context: r.context.into_iter().map(Into::into).collect(),
            history: r.history.into_iter().map(Into::into).collect(),
            now: r.now.map(timestamp_to_pb),
            policy_overrides: None,
            options: Some(r.options.into()),
        }
    }
}

/// Ignores `request_id` and `policy_overrides`; use [`AssessInput`] to keep them.
impl TryFrom<pb::AssessRequest> for AssessmentRequest {
    type Error = ConvertError;

    fn try_from(r: pb::AssessRequest) -> Result<Self> {
        AssessInput::try_from(r).map(|input| input.request)
    }
}

/// A decoded `AssessRequest`: the core request and what travels beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct AssessInput {
    /// Echoed in the response.
    pub request_id: String,
    /// The core request.
    pub request: AssessmentRequest,
    /// A partial policy to merge over the server's, if any.
    pub policy_overrides: Option<serde_json::Map<String, serde_json::Value>>,
}

impl TryFrom<pb::AssessRequest> for AssessInput {
    type Error = ConvertError;

    fn try_from(r: pb::AssessRequest) -> Result<Self> {
        let request = AssessmentRequest {
            changes: list(r.changes, ChangeSet::try_from).at("changes")?,
            context: list(r.context, Entity::try_from).at("context")?,
            history: list(r.history, HistoryPoint::try_from).at("history")?,
            now: ts_opt(r.now).at("now")?,
            options: options_from_pb(r.options),
        };
        let policy_overrides = opt(r.policy_overrides, struct_to_map).at("policy_overrides")?;
        Ok(AssessInput {
            request_id: r.request_id,
            request,
            policy_overrides,
        })
    }
}

impl From<AssessInput> for pb::AssessRequest {
    fn from(input: AssessInput) -> Self {
        let mut pb = pb::AssessRequest::from(input.request);
        pb.request_id = input.request_id;
        pb.policy_overrides = input.policy_overrides.map(map_to_struct);
        pb
    }
}

// ---------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------

impl From<Modifier> for pb::Modifier {
    fn from(m: Modifier) -> Self {
        Self {
            name: m.name,
            factor: m.factor,
            detail: m.detail,
        }
    }
}

impl From<pb::Modifier> for Modifier {
    fn from(m: pb::Modifier) -> Self {
        Modifier {
            name: m.name,
            factor: m.factor,
            detail: m.detail,
        }
    }
}

impl From<Signal> for pb::Signal {
    fn from(s: Signal) -> Self {
        Self {
            category: signal_category_to_pb(s.category),
            weight: s.weight,
            terms: s.terms,
            excerpt: s.excerpt,
        }
    }
}

impl TryFrom<pb::Signal> for Signal {
    type Error = ConvertError;

    fn try_from(s: pb::Signal) -> Result<Self> {
        Ok(Signal {
            category: signal_category_from_pb(s.category).at("category")?,
            weight: s.weight,
            terms: s.terms,
            excerpt: s.excerpt,
        })
    }
}

impl From<FieldAssessment> for pb::FieldAssessment {
    fn from(f: FieldAssessment) -> Self {
        Self {
            field: f.field,
            change: field_change_kind_to_pb(f.change),
            direction: direction_to_pb(f.direction),
            magnitude: f.magnitude,
            severity: f.severity,
            risk: f.risk,
            breaking: f.breaking,
            additive: f.additive,
            before: f.before,
            after: f.after,
            metrics: f.metrics,
            explanation: f.explanation,
            judgment: f.judgment.map(Into::into),
        }
    }
}

impl From<FieldJudgment> for pb::FieldJudgment {
    fn from(j: FieldJudgment) -> Self {
        Self {
            status: judgment_status_to_pb(j.status),
            reasons: reasons_to_pb(j.reasons),
            fingerprint: j.fingerprint,
            lexical_magnitude: j.lexical_magnitude,
            relation: j
                .relation
                .map_or(pb::TextRelation::Unspecified.into(), text_relation_to_pb),
            confidence: j.confidence,
            weight: j.weight,
            rationale: j.rationale,
            source: j.source,
            detail: j.detail,
        }
    }
}

/// Lenient: see the module rules.
impl From<pb::FieldJudgment> for FieldJudgment {
    fn from(j: pb::FieldJudgment) -> Self {
        let (status, detail) = match judgment_status_from_pb(j.status) {
            Some(status) => (status, j.detail),
            None => {
                let note = format!("unknown judgment status {}", j.status);
                let detail = if j.detail.is_empty() {
                    note
                } else {
                    format!("{note}: {}", j.detail)
                };
                (JudgmentStatus::Unavailable, detail)
            }
        };
        FieldJudgment {
            status,
            reasons: reasons_from_pb(j.reasons),
            fingerprint: j.fingerprint,
            lexical_magnitude: j.lexical_magnitude,
            relation: text_relation_from_pb(j.relation).ok(),
            confidence: j.confidence,
            weight: j.weight,
            rationale: j.rationale,
            source: j.source,
            detail,
        }
    }
}

impl From<JudgmentRequest> for pb::JudgmentRequest {
    fn from(r: JudgmentRequest) -> Self {
        Self {
            change_index: to_u32(r.change_index),
            entity_id: r.entity_id,
            field: r.field,
            fingerprint: r.fingerprint,
            reasons: reasons_to_pb(r.reasons),
            lexical_magnitude: r.lexical_magnitude,
            before: r.before,
            after: r.after,
        }
    }
}

impl From<pb::JudgmentRequest> for JudgmentRequest {
    fn from(r: pb::JudgmentRequest) -> Self {
        JudgmentRequest {
            change_index: to_usize(r.change_index),
            entity_id: r.entity_id,
            field: r.field,
            fingerprint: r.fingerprint,
            reasons: reasons_from_pb(r.reasons),
            lexical_magnitude: r.lexical_magnitude,
            before: r.before,
            after: r.after,
        }
    }
}

impl From<JudgmentKey> for pb::JudgmentKey {
    fn from(k: JudgmentKey) -> Self {
        Self {
            entity_id: k.entity_id,
            field: k.field,
        }
    }
}

impl From<pb::JudgmentKey> for JudgmentKey {
    fn from(k: pb::JudgmentKey) -> Self {
        JudgmentKey {
            entity_id: k.entity_id,
            field: k.field,
        }
    }
}

impl From<GrayZone> for pb::GrayZone {
    fn from(g: GrayZone) -> Self {
        Self {
            reasons: reasons_to_pb(g.reasons),
            fingerprint: g.fingerprint,
        }
    }
}

impl From<pb::GrayZone> for GrayZone {
    fn from(g: pb::GrayZone) -> Self {
        GrayZone {
            reasons: reasons_from_pb(g.reasons),
            fingerprint: g.fingerprint,
        }
    }
}

impl TryFrom<pb::FieldAssessment> for FieldAssessment {
    type Error = ConvertError;

    fn try_from(f: pb::FieldAssessment) -> Result<Self> {
        Ok(FieldAssessment {
            judgment: f.judgment.map(Into::into),
            field: f.field,
            change: field_change_kind_from_pb(f.change).at("change")?,
            direction: direction_from_pb(f.direction).at("direction")?,
            magnitude: f.magnitude,
            severity: f.severity,
            risk: f.risk,
            breaking: f.breaking,
            additive: f.additive,
            before: f.before,
            after: f.after,
            metrics: f.metrics,
            explanation: f.explanation,
        })
    }
}

impl From<ChangeAssessment> for pb::ChangeAssessment {
    fn from(c: ChangeAssessment) -> Self {
        Self {
            entity_id: c.entity_id,
            kind: String::from(c.kind),
            change_type: change_type_to_pb(c.change_type),
            raw_score: c.raw_score,
            local_score: c.local_score,
            modifiers: c.modifiers.into_iter().map(Into::into).collect(),
            fields: c.fields.into_iter().map(Into::into).collect(),
            signals: c.signals.into_iter().map(Into::into).collect(),
            bump: version_bump_to_pb(c.bump),
            previous_version: c.previous_version.map(Into::into),
            next_version: c.next_version.map(Into::into),
            anomaly_z: c.anomaly_z,
            explanation: c.explanation,
            propagation_truncated: c.propagation_truncated,
        }
    }
}

impl TryFrom<pb::ChangeAssessment> for ChangeAssessment {
    type Error = ConvertError;

    fn try_from(c: pb::ChangeAssessment) -> Result<Self> {
        Ok(ChangeAssessment {
            entity_id: c.entity_id,
            kind: EntityKind::from(c.kind),
            change_type: change_type_from_pb(c.change_type).at("change_type")?,
            raw_score: c.raw_score,
            local_score: c.local_score,
            modifiers: c.modifiers.into_iter().map(Into::into).collect(),
            fields: list(c.fields, FieldAssessment::try_from).at("fields")?,
            signals: list(c.signals, Signal::try_from).at("signals")?,
            bump: version_bump_from_pb(c.bump).at("bump")?,
            previous_version: c.previous_version.map(Into::into),
            next_version: c.next_version.map(Into::into),
            anomaly_z: c.anomaly_z,
            explanation: c.explanation,
            propagation_truncated: c.propagation_truncated,
        })
    }
}

impl From<Contribution> for pb::Contribution {
    fn from(c: Contribution) -> Self {
        Self {
            source_id: c.source_id,
            field: c.field,
            channel: channel_to_pb(c.channel),
            path: c.path,
            value: c.value,
            detail: c.detail,
        }
    }
}

impl TryFrom<pb::Contribution> for Contribution {
    type Error = ConvertError;

    fn try_from(c: pb::Contribution) -> Result<Self> {
        Ok(Contribution {
            source_id: c.source_id,
            field: c.field,
            channel: channel_from_pb(c.channel).at("channel")?,
            path: c.path,
            value: c.value,
            detail: c.detail,
        })
    }
}

impl From<EntityImpact> for pb::EntityImpact {
    fn from(i: EntityImpact) -> Self {
        Self {
            entity_id: i.entity_id,
            kind: String::from(i.kind),
            rank: i.rank,
            score: i.score,
            level: impact_level_to_pb(i.level),
            action: action_to_pb(i.action),
            channel: channel_to_pb(i.channel),
            distance: i.distance,
            changed: i.changed,
            basis_status: basis_status_to_pb(i.basis_status),
            basis_introduced: i.basis_introduced,
            effort: i.effort,
            effort_at_risk: i.effort_at_risk,
            contributions: i.contributions.into_iter().map(Into::into).collect(),
            reasons: i.reasons,
        }
    }
}

impl TryFrom<pb::EntityImpact> for EntityImpact {
    type Error = ConvertError;

    fn try_from(i: pb::EntityImpact) -> Result<Self> {
        Ok(EntityImpact {
            entity_id: i.entity_id,
            kind: EntityKind::from(i.kind),
            rank: i.rank,
            score: i.score,
            level: impact_level_from_pb(i.level).at("level")?,
            action: action_from_pb(i.action).at("action")?,
            channel: channel_from_pb(i.channel).at("channel")?,
            distance: i.distance,
            changed: i.changed,
            basis_status: basis_status_from_pb(i.basis_status).at("basis_status")?,
            basis_introduced: i.basis_introduced,
            effort: i.effort,
            effort_at_risk: i.effort_at_risk,
            contributions: list(i.contributions, Contribution::try_from).at("contributions")?,
            reasons: i.reasons,
        })
    }
}

impl From<Violation> for pb::Violation {
    fn from(v: Violation) -> Self {
        Self {
            kind: violation_kind_to_pb(v.kind),
            entities: v.entities,
            severity: v.severity,
            introduced: v.introduced,
            message: v.message,
        }
    }
}

impl TryFrom<pb::Violation> for Violation {
    type Error = ConvertError;

    fn try_from(v: pb::Violation) -> Result<Self> {
        Ok(Violation {
            kind: violation_kind_from_pb(v.kind).at("kind")?,
            entities: v.entities,
            severity: v.severity,
            introduced: v.introduced,
            message: v.message,
        })
    }
}

impl From<Assessment> for pb::Assessment {
    fn from(a: Assessment) -> Self {
        Self {
            score: a.score,
            level: impact_level_to_pb(a.level),
            recommendation: recommendation_to_pb(a.recommendation),
            confidence: a.confidence,
            severity: a.severity,
            breadth: a.breadth,
            effort_at_risk: a.effort_at_risk,
            effort_total: a.effort_total,
            replan_root: a.replan_root,
            changes: a.changes.into_iter().map(Into::into).collect(),
            impacts: a.impacts.into_iter().map(Into::into).collect(),
            impacts_total: a.impacts_total,
            violations: a.violations.into_iter().map(Into::into).collect(),
            summary: a.summary,
            engine_version: a.engine_version,
            assessed_at: Some(timestamp_to_pb(a.assessed_at)),
            judgment_requests: a.judgment_requests.into_iter().map(Into::into).collect(),
            unmatched_judgments: a.unmatched_judgments.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::Assessment> for Assessment {
    type Error = ConvertError;

    fn try_from(a: pb::Assessment) -> Result<Self> {
        Ok(Assessment {
            judgment_requests: a.judgment_requests.into_iter().map(Into::into).collect(),
            unmatched_judgments: a.unmatched_judgments.into_iter().map(Into::into).collect(),
            score: a.score,
            level: impact_level_from_pb(a.level).at("level")?,
            recommendation: recommendation_from_pb(a.recommendation).at("recommendation")?,
            confidence: a.confidence,
            severity: a.severity,
            breadth: a.breadth,
            effort_at_risk: a.effort_at_risk,
            effort_total: a.effort_total,
            replan_root: a.replan_root,
            changes: list(a.changes, ChangeAssessment::try_from).at("changes")?,
            impacts: list(a.impacts, EntityImpact::try_from).at("impacts")?,
            impacts_total: a.impacts_total,
            violations: list(a.violations, Violation::try_from).at("violations")?,
            summary: a.summary,
            engine_version: a.engine_version,
            assessed_at: timestamp_from_pb(required(a.assessed_at).at("assessed_at")?)
                .at("assessed_at")?,
        })
    }
}

/// A required `Assessment` field of a response message.
pub(crate) fn assessment_field(a: Option<pb::Assessment>) -> Result<Assessment> {
    Assessment::try_from(required(a).at("assessment")?).at("assessment")
}

/// A required `Entity` field of a response message.
pub(crate) fn entity_field(e: Option<pb::Entity>) -> Result<Entity> {
    Entity::try_from(required(e).at("entity")?).at("entity")
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

impl From<StructureDiff> for pb::StructureDiff {
    fn from(s: StructureDiff) -> Self {
        Self {
            items_before: s.items_before,
            items_after: s.items_after,
            items_added: s.items_added,
            items_removed: s.items_removed,
            items_modified: s.items_modified,
            items_checked: s.items_checked,
            items_unchecked: s.items_unchecked,
            headings_added: s.headings_added,
            headings_removed: s.headings_removed,
        }
    }
}

impl From<pb::StructureDiff> for StructureDiff {
    fn from(s: pb::StructureDiff) -> Self {
        StructureDiff {
            items_before: s.items_before,
            items_after: s.items_after,
            items_added: s.items_added,
            items_removed: s.items_removed,
            items_modified: s.items_modified,
            items_checked: s.items_checked,
            items_unchecked: s.items_unchecked,
            headings_added: s.headings_added,
            headings_removed: s.headings_removed,
        }
    }
}

impl From<TextDiff> for pb::CompareTextResponse {
    fn from(d: TextDiff) -> Self {
        Self {
            magnitude: d.magnitude,
            lexical_similarity: d.lexical_similarity,
            char_similarity: d.char_similarity,
            token_jaccard: d.token_jaccard,
            shingle_similarity: d.shingle_similarity,
            tfidf_cosine: d.tfidf_cosine,
            embedding_cosine: d.embedding_cosine,
            structure: Some(d.structure.into()),
            negation_flips: d.negation_flips,
            modal_shifts: d.modal_shifts,
            numbers_changed: d.numbers_changed,
            added_terms: d.added_terms,
            removed_terms: d.removed_terms,
            chars_before: u64::try_from(d.chars_before).unwrap_or(u64::MAX),
            chars_after: u64::try_from(d.chars_after).unwrap_or(u64::MAX),
            low_information: d.low_information,
            passages_added: d.passages_added,
            passages_removed: d.passages_removed,
            passages_modified: d.passages_modified,
            reordered: d.reordered,
            gray_zone: d.gray_zone.map(Into::into),
        }
    }
}

/// An absent `structure` is all zeros, as proto3 has it.
impl TryFrom<pb::CompareTextResponse> for TextDiff {
    type Error = ConvertError;

    fn try_from(d: pb::CompareTextResponse) -> Result<Self> {
        let chars = |n: u64| {
            usize::try_from(n).map_err(|_| ConvertError::new(format!("{n} does not fit in usize")))
        };
        Ok(TextDiff {
            gray_zone: d.gray_zone.map(Into::into),
            magnitude: d.magnitude,
            lexical_similarity: d.lexical_similarity,
            char_similarity: d.char_similarity,
            token_jaccard: d.token_jaccard,
            shingle_similarity: d.shingle_similarity,
            tfidf_cosine: d.tfidf_cosine,
            embedding_cosine: d.embedding_cosine,
            structure: d.structure.map(Into::into).unwrap_or_default(),
            negation_flips: d.negation_flips,
            modal_shifts: d.modal_shifts,
            numbers_changed: d.numbers_changed,
            added_terms: d.added_terms,
            removed_terms: d.removed_terms,
            chars_before: chars(d.chars_before).at("chars_before")?,
            chars_after: chars(d.chars_after).at("chars_after")?,
            low_information: d.low_information,
            passages_added: d.passages_added,
            passages_removed: d.passages_removed,
            passages_modified: d.passages_modified,
            reordered: d.reordered,
        })
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

impl From<VersionRecord> for pb::VersionRecord {
    fn from(r: VersionRecord) -> Self {
        Self {
            version: Some(r.version.into()),
            bump: version_bump_to_pb(r.bump),
            at: Some(timestamp_to_pb(r.at)),
            change_type: change_type_to_pb(r.change_type),
            raw_score: r.raw_score,
            level: impact_level_to_pb(r.level),
            changed_fields: r.changed_fields,
            snapshot: r.snapshot.map(Into::into),
            judgments: r.judgments.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::VersionRecord> for VersionRecord {
    type Error = ConvertError;

    fn try_from(r: pb::VersionRecord) -> Result<Self> {
        Ok(VersionRecord {
            judgments: r
                .judgments
                .into_iter()
                .map(stored_judgment_from_pb)
                .collect(),
            version: required(r.version).at("version")?.into(),
            bump: version_bump_from_pb(r.bump).at("bump")?,
            at: timestamp_from_pb(required(r.at).at("at")?).at("at")?,
            change_type: change_type_from_pb(r.change_type).at("change_type")?,
            raw_score: r.raw_score,
            level: impact_level_from_pb(r.level).at("level")?,
            changed_fields: r.changed_fields,
            snapshot: entity_opt(r.snapshot).at("snapshot")?,
        })
    }
}

impl From<StaleEntry> for pb::StaleEntry {
    fn from(s: StaleEntry) -> Self {
        Self {
            entity_id: s.entity_id,
            basis_entity_id: s.basis_entity_id,
            pinned: Some(s.pinned.into()),
            current: s.current.map(Into::into),
            status: basis_status_to_pb(s.status),
        }
    }
}

impl TryFrom<pb::StaleEntry> for StaleEntry {
    type Error = ConvertError;

    fn try_from(s: pb::StaleEntry) -> Result<Self> {
        Ok(StaleEntry {
            entity_id: s.entity_id,
            basis_entity_id: s.basis_entity_id,
            pinned: required(s.pinned).at("pinned")?.into(),
            current: s.current.map(Into::into),
            status: basis_status_from_pb(s.status).at("status")?,
        })
    }
}

impl From<PutOutcome> for pb::PutEntityResponse {
    fn from(o: PutOutcome) -> Self {
        Self {
            entity: Some(o.entity.into()),
            created: o.created,
            stored: o.stored,
            bump: version_bump_to_pb(o.bump),
            assessment: Some(o.assessment.into()),
            skipped_stale: o.skipped_stale,
            unchanged: o.unchanged,
        }
    }
}

impl TryFrom<pb::PutEntityResponse> for PutOutcome {
    type Error = ConvertError;

    fn try_from(o: pb::PutEntityResponse) -> Result<Self> {
        Ok(PutOutcome {
            entity: entity_field(o.entity)?,
            created: o.created,
            stored: o.stored,
            bump: version_bump_from_pb(o.bump).at("bump")?,
            assessment: assessment_field(o.assessment)?,
            unchanged: o.unchanged,
            skipped_stale: o.skipped_stale,
        })
    }
}

/// A `PutEntityRequest` for `entity` with `options`.
pub fn put_request_to_pb(entity: Entity, options: PutOptions) -> pb::PutEntityRequest {
    pb::PutEntityRequest {
        entity: Some(entity.into()),
        dry_run: options.dry_run,
        expected_version: options.expected_version.map(Into::into),
        events: options.events.into_iter().map(Into::into).collect(),
        now: options.now.map(timestamp_to_pb),
        disable_auto_pin: !options.auto_pin,
        options: Some(options.assess.into()),
        source_seq: options.source_seq,
        judgments: options.judgments.into_iter().map(Into::into).collect(),
        hold_if_flagged: options.hold_if_flagged,
    }
}

/// The entity and the options of a `PutEntityRequest`.
pub fn put_request_from_pb(
    request: pb::PutEntityRequest,
) -> Result<(Entity, PutOptions), ConvertError> {
    let entity = entity_field(request.entity)?;
    let options = PutOptions {
        judgments: judgments_from_pb(request.judgments, "judgments")?,
        hold_if_flagged: request.hold_if_flagged,
        dry_run: request.dry_run,
        expected_version: request.expected_version.map(Into::into),
        events: list(request.events, ActivityEvent::try_from).at("events")?,
        now: ts_opt(request.now).at("now")?,
        auto_pin: !request.disable_auto_pin,
        assess: options_from_pb(request.options),
        source_seq: request.source_seq,
    };
    Ok((entity, options))
}

/// A `DeleteEntityRequest` for `id` with `options`.
pub fn delete_request_to_pb(
    id: impl Into<String>,
    options: DeleteOptions,
) -> pb::DeleteEntityRequest {
    pb::DeleteEntityRequest {
        id: id.into(),
        dry_run: options.dry_run,
        expected_version: options.expected_version.map(Into::into),
        now: options.now.map(timestamp_to_pb),
        source_seq: options.source_seq,
    }
}

/// The id and the options of a `DeleteEntityRequest`.
pub fn delete_request_from_pb(
    request: pb::DeleteEntityRequest,
) -> Result<(String, DeleteOptions), ConvertError> {
    let options = DeleteOptions {
        dry_run: request.dry_run,
        expected_version: request.expected_version.map(Into::into),
        now: ts_opt(request.now).at("now")?,
        source_seq: request.source_seq,
    };
    Ok((request.id, options))
}

impl From<DeleteOutcome> for pb::DeleteEntityResponse {
    fn from(o: DeleteOutcome) -> Self {
        Self {
            assessment: Some(o.assessment.into()),
            deleted: o.deleted,
            skipped_stale: o.skipped_stale,
        }
    }
}

impl TryFrom<pb::DeleteEntityResponse> for DeleteOutcome {
    type Error = ConvertError;

    fn try_from(o: pb::DeleteEntityResponse) -> Result<Self> {
        Ok(DeleteOutcome {
            deleted: o.deleted,
            assessment: assessment_field(o.assessment)?,
            skipped_stale: o.skipped_stale,
        })
    }
}

impl From<ImportItem> for pb::ImportRequest {
    fn from(item: ImportItem) -> Self {
        Self {
            entity: Some(item.entity.into()),
            source_seq: item.source_seq,
            deleted: item.deleted,
        }
    }
}

impl TryFrom<pb::ImportRequest> for ImportItem {
    type Error = ConvertError;

    /// For a deletion only the entity's id is read, so the rest of it is
    /// not checked.
    fn try_from(r: pb::ImportRequest) -> Result<Self> {
        let entity = required(r.entity).at("entity")?;
        let item = if r.deleted {
            ImportItem::delete(entity.id)
        } else {
            ImportItem::upsert(Entity::try_from(entity).at("entity")?)
        };
        Ok(ImportItem {
            source_seq: r.source_seq,
            ..item
        })
    }
}

impl From<ImportFailure> for pb::ImportFailure {
    fn from(f: ImportFailure) -> Self {
        Self {
            id: f.id,
            error: f.error,
        }
    }
}

impl From<pb::ImportFailure> for ImportFailure {
    fn from(f: pb::ImportFailure) -> Self {
        Self {
            id: f.id,
            error: f.error,
        }
    }
}

/// `failed_total` is the length of `failed`: a summary lists every failure.
impl From<ImportSummary> for pb::ImportResponse {
    fn from(s: ImportSummary) -> Self {
        Self {
            created: s.created,
            unchanged: s.unchanged,
            assessed: s.assessed,
            skipped_stale: s.skipped_stale,
            deleted: s.deleted,
            failed_total: s.failed.len() as u64,
            failed: s.failed.into_iter().map(Into::into).collect(),
        }
    }
}

/// `failed_total` is not part of a summary. When the server cut the list
/// short, it is larger than `failed.len()`; read it from the message.
impl From<pb::ImportResponse> for ImportSummary {
    fn from(r: pb::ImportResponse) -> Self {
        Self {
            created: r.created,
            unchanged: r.unchanged,
            assessed: r.assessed,
            skipped_stale: r.skipped_stale,
            deleted: r.deleted,
            failed: r.failed.into_iter().map(Into::into).collect(),
        }
    }
}

/// Decodes a list of protobuf messages, prefixing errors with `name[i]`.
#[cfg_attr(not(any(feature = "server", feature = "client")), allow(dead_code))]
pub(crate) fn decode_list<T, U>(
    items: Vec<T>,
    name: &'static str,
    f: impl FnMut(T) -> Result<U>,
) -> Result<Vec<U>> {
    list(items, f).at(name)
}

/// An optional protobuf timestamp field, prefixing errors with `name`.
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) fn decode_time(
    ts: Option<prost_types::Timestamp>,
    name: &'static str,
) -> Result<Option<DateTime<Utc>>> {
    ts_opt(ts).at(name)
}

/// A required `SemVer` field.
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) fn decode_version(v: Option<pb::SemVer>, name: &'static str) -> Result<SemVer> {
    required(v).at(name).map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_paths_render_like_the_proto_docs() {
        let err = ConvertError::new("status category is unspecified")
            .key("status")
            .field("fields")
            .field("after")
            .index(2)
            .field("changes");
        assert_eq!(
            err.to_string(),
            "changes[2].after.fields[\"status\"]: status category is unspecified"
        );
        assert_eq!(err.path(), "changes[2].after.fields[\"status\"]");
    }

    #[test]
    fn timestamps_reject_unnormalized_nanos() {
        let bad = prost_types::Timestamp {
            seconds: 0,
            nanos: -1,
        };
        assert!(timestamp_from_pb(bad).is_err());
        let bad = prost_types::Timestamp {
            seconds: 0,
            nanos: 1_000_000_000,
        };
        assert!(timestamp_from_pb(bad).is_err());
        let bad = prost_types::Timestamp {
            seconds: i64::MAX,
            nanos: 0,
        };
        assert!(timestamp_from_pb(bad).is_err());
    }

    #[test]
    fn whole_struct_numbers_become_integers() {
        let s = json_to_struct(serde_json::json!({"a": 3, "b": 0.5, "c": [1, null, "x"]}))
            .expect("an object");
        let back = struct_to_json(s).expect("finite");
        assert_eq!(
            back,
            serde_json::json!({"a": 3, "b": 0.5, "c": [1, null, "x"]})
        );
        assert!(back["a"].is_i64());
    }

    #[test]
    fn non_finite_struct_numbers_are_rejected_with_a_path() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "x".to_owned(),
            prost_types::Value {
                kind: Some(JsonKind::NumberValue(f64::NAN)),
            },
        );
        let err = struct_to_json(prost_types::Struct { fields }).expect_err("nan");
        assert_eq!(err.path(), "[\"x\"]");
    }
}
