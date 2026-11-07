//! # assessment-core
//!
//! Change-impact assessment for versioned, hierarchical work items —
//! projects, epics, issues, sub-issues, tasks.
//!
//! When something changes on an item (its title, description, estimate,
//! status, priority, assignee, dates, labels, parent, or a comment lands on
//! it), the engine answers:
//!
//! * **How big is this change?** Per field, with measures that fit the type:
//!   TF-IDF / shingle / edit-distance / embedding similarity plus structural,
//!   negation, modal and number diffs for text; log-ratios for estimates; a
//!   transition-risk matrix for workflow status; schedule compression for
//!   dates; Jaccard distance for labels.
//! * **What version is it now?** Every item is semantically versioned: a
//!   breaking change bumps MAJOR, a compatible addition MINOR, a cosmetic
//!   edit PATCH. Items planned against another pin its version, and a MAJOR
//!   bump makes their plan *incompatible*, exactly like a dependency.
//! * **Who else is affected, and how much?** Impact propagates to children,
//!   parents, siblings, blocked and blocking items, weighted by how much of
//!   the changed text each one shares, what share of the parent's effort an
//!   estimate change is, whether a moved date now conflicts, and where each
//!   item stands (in-progress and done work is costlier to disturb).
//! * **Do we need to re-plan?** A score, a level, a recommendation (no
//!   action → notify → review → partial re-plan → full re-plan), the subtree
//!   to re-plan, a ranked list of affected items with an action each, the
//!   plan conflicts the change introduced, and the reasons for all of it.
//!
//! ```
//! use assessment_core::prelude::*;
//!
//! let parent = Entity::new("ENG-1", "issue")
//!     .with_title("Authentication")
//!     .with_description("- [ ] Email and password login\n- [ ] Password reset by email")
//!     .with_status(StatusCategory::Started, "In Progress")
//!     .with_version(SemVer::new(1, 0, 0));
//! let rewritten = parent.clone()
//!     .with_description("- [ ] Google OAuth login only");
//! let reset = Entity::new("ENG-3", "sub_issue")
//!     .with_parent("ENG-1")
//!     .with_title("Password reset email flow")
//!     .with_status(StatusCategory::Unstarted, "Todo");
//!
//! let assessment = Engine::default().assess(&AssessmentRequest {
//!     changes: vec![ChangeSet::update(parent, rewritten)],
//!     context: vec![reset],
//!     ..Default::default()
//! })?;
//!
//! assert_eq!(assessment.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
//! assert!(assessment.level >= ImpactLevel::Medium);
//! # Ok::<(), assessment_core::Error>(())
//! ```
//!
//! The [`registry`] keeps versions and history for you: put entities in, and
//! every update comes back assessed and versioned, with child pins managed.

pub mod assessment;
pub mod engine;
pub mod error;
pub mod model;
pub mod policy;
pub mod registry;
pub mod semver;
pub mod stats;
pub mod text;

mod constraints;
mod fields;
mod graph;
mod propagate;
mod signals;

pub use assessment::Assessment;
pub use engine::Engine;
pub use error::{Error, ErrorKind, Result};
pub use policy::Policy;
pub use registry::Registry;

/// The version of this engine, reported in every assessment.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything most callers need.
pub mod prelude {
    pub use crate::assessment::{
        Action, Assessment, BasisStatus, ChangeAssessment, ChangeType, Channel, EntityImpact,
        ImpactLevel, Recommendation, Violation, ViolationKind,
    };
    pub use crate::engine::Engine;
    pub use crate::error::{Error, ErrorKind};
    pub use crate::model::{
        ActivityEvent, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity, EntityKind,
        FieldChange, FieldValue, HistoryPoint, RelationKind, StatusCategory, field,
    };
    pub use crate::policy::Policy;
    pub use crate::registry::{PutOptions, Registry};
    pub use crate::semver::{SemVer, VersionBump};
}
