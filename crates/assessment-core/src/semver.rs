//! Semantic versions for work items.
//!
//! Every entity carries a version. A change bumps it the way an API change
//! bumps a library: **MAJOR** when the change breaks what others planned
//! against (scope removed or rewritten, a requirement negated, the item
//! canceled or reopened), **MINOR** when it adds compatibly (new acceptance
//! criteria, a moderate re-estimate), **PATCH** when nothing anyone planned
//! against moves (a typo, a label). Entities that were planned against
//! another one *pin* its version ([`crate::model::BasisPin`]); comparing the
//! pin with the current version says whether that plan still holds, with
//! Cargo's caret rules.
//!
//! Below 1.0.0 the bump follows Cargo's convention too, so that the version a
//! change produces and the compatibility verdict on it always agree: at
//! `0.y.z` (`y > 0`) the minor component is the breaking one (MAJOR →
//! `0.(y+1).0`, MINOR and PATCH → `0.y.(z+1)`), and at `0.0.z` every change is
//! breaking (everything → `0.0.(z+1)`). Entities the registry versions start
//! at [`SemVer::INITIAL`] (1.0.0) and never enter that range; only callers
//! that supply their own `0.x` versions do.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// `MAJOR.MINOR.PATCH`. Serialized as the string `"1.2.3"`.
///
/// [`SemVer::default()`] is [`SemVer::INITIAL`] (1.0.0), the version an
/// entity starts at, not `0.0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    /// The version an entity gets when it is first recorded.
    pub const INITIAL: SemVer = SemVer::new(1, 0, 0);

    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// The largest version; [`SemVer::bump`] saturates here.
    pub const MAX: SemVer = SemVer::new(u64::MAX, u64::MAX, u64::MAX);

    /// The version after a change of this size.
    ///
    /// Above 1.0.0 MAJOR, MINOR and PATCH increment their own component;
    /// below it Cargo's convention applies (see the [module docs](self)), so
    /// that [`SemVer::compatibility`] of the result against `self` is
    /// *breaking* for a MAJOR bump and never breaking for a MINOR or PATCH one
    /// (except at `0.0.z`, where every change is breaking).
    ///
    /// The result is always greater than `self` for a real change, unless
    /// `self` is [`SemVer::MAX`]: when the component to increment is at
    /// `u64::MAX`, the bump carries into the next more significant component
    /// (a larger, more conservative bump), and at `u64::MAX.*.*` a MAJOR bump
    /// saturates at [`SemVer::MAX`]. Use [`SemVer::checked_bump`] to detect
    /// that instead.
    pub fn bump(self, bump: VersionBump) -> SemVer {
        if let Some(v) = self.checked_bump(bump) {
            return v;
        }
        let mut component = self.component(bump);
        loop {
            component = match component {
                Component::Patch => Component::Minor,
                Component::Minor => Component::Major,
                Component::Major => return SemVer::MAX,
            };
            if let Some(v) = self.increment(component) {
                return v;
            }
        }
    }

    /// Like [`SemVer::bump`], but `None` when the component the bump
    /// increments would overflow, instead of carrying or saturating.
    pub fn checked_bump(self, bump: VersionBump) -> Option<SemVer> {
        match bump {
            VersionBump::None => Some(self),
            VersionBump::Initial => Some(SemVer::INITIAL),
            VersionBump::Patch | VersionBump::Minor | VersionBump::Major => {
                self.increment(self.component(bump))
            }
        }
    }

    /// The component a real change of size `bump` increments at this version.
    fn component(self, bump: VersionBump) -> Component {
        match (self.major, self.minor, bump) {
            // 0.0.z: every change is breaking, and the patch is the breaking component.
            (0, 0, _) => Component::Patch,
            // 0.y.z: the minor is the breaking component, the patch the compatible one.
            (0, _, VersionBump::Major) => Component::Minor,
            (0, _, _) => Component::Patch,
            (_, _, VersionBump::Major) => Component::Major,
            (_, _, VersionBump::Minor) => Component::Minor,
            _ => Component::Patch,
        }
    }

    /// `self` with `component` incremented and the lower components reset, or
    /// `None` on overflow.
    fn increment(self, component: Component) -> Option<SemVer> {
        match component {
            Component::Major => self.major.checked_add(1).map(|m| SemVer::new(m, 0, 0)),
            Component::Minor => self
                .minor
                .checked_add(1)
                .map(|m| SemVer::new(self.major, m, 0)),
            Component::Patch => self
                .patch
                .checked_add(1)
                .map(|p| SemVer::new(self.major, self.minor, p)),
        }
    }

    /// Whether a plan made against `self` (the pinned version) still holds
    /// now that the entity is at `current`, with Cargo's caret rules: the
    /// left-most non-zero component must not change.
    pub fn compatibility(self, current: SemVer) -> Compatibility {
        if current == self {
            return Compatibility::Identical;
        }
        let breaking = if self.major != 0 || current.major != 0 {
            current.major != self.major
        } else if self.minor != 0 || current.minor != 0 {
            current.minor != self.minor
        } else {
            current.patch != self.patch
        };
        if breaking || current < self {
            Compatibility::Breaking
        } else if current.minor != self.minor {
            Compatibility::Additive
        } else {
            Compatibility::Patch
        }
    }
}

impl Default for SemVer {
    /// [`SemVer::INITIAL`]: the version an entity starts at.
    fn default() -> Self {
        SemVer::INITIAL
    }
}

/// One component of a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Component {
    Major,
    Minor,
    Patch,
}

/// How a current version relates to a pinned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compatibility {
    /// The same version.
    Identical,
    /// Only patch-level changes since the pin.
    Patch,
    /// Compatible additions since the pin (a minor bump).
    Additive,
    /// A breaking change since the pin, or the version went backwards.
    Breaking,
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for SemVer {
    type Err = Error;

    /// Parses `1.2.3`, `v1.2.3`, `1.2` (patch 0) and `1` (minor and patch 0).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let body = trimmed
            .strip_prefix('v')
            .or_else(|| trimmed.strip_prefix('V'))
            .unwrap_or(trimmed);
        let mut parts = body.split('.');
        let mut next = |name: &str, required: bool| -> Result<u64, Error> {
            match parts.next() {
                Some(p) => p
                    .parse::<u64>()
                    .map_err(|_| Error::invalid(format!("version `{s}` has a non-numeric {name}"))),
                None if required => Err(Error::invalid(format!("version `{s}` has no {name}"))),
                None => Ok(0),
            }
        };
        let major = next("major", true)?;
        let minor = next("minor", false)?;
        let patch = next("patch", false)?;
        if parts.next().is_some() {
            return Err(Error::invalid(format!(
                "version `{s}` has more than three components"
            )));
        }
        Ok(SemVer::new(major, minor, patch))
    }
}

impl Serialize for SemVer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SemVer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The size of a version change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionBump {
    /// Nothing versioned changed (e.g. only a comment was added).
    None,
    /// Cosmetic: nothing anyone planned against moved.
    Patch,
    /// Compatible additions or moderate changes.
    Minor,
    /// Breaking: plans made against the previous version need revisiting.
    Major,
    /// The entity was created.
    Initial,
}

impl VersionBump {
    pub fn as_str(self) -> &'static str {
        match self {
            VersionBump::None => "none",
            VersionBump::Patch => "patch",
            VersionBump::Minor => "minor",
            VersionBump::Major => "major",
            VersionBump::Initial => "initial",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints() {
        let v: SemVer = "v2.3.4".parse().expect("parses");
        assert_eq!(v, SemVer::new(2, 3, 4));
        assert_eq!(v.to_string(), "2.3.4");
        assert_eq!(
            "1.2".parse::<SemVer>().expect("parses"),
            SemVer::new(1, 2, 0)
        );
        assert!("1.2.3.4".parse::<SemVer>().is_err());
        assert!("x.1".parse::<SemVer>().is_err());
        assert!("".parse::<SemVer>().is_err());
    }

    #[test]
    fn bumps() {
        let v = SemVer::new(1, 4, 2);
        assert_eq!(v.bump(VersionBump::Patch), SemVer::new(1, 4, 3));
        assert_eq!(v.bump(VersionBump::Minor), SemVer::new(1, 5, 0));
        assert_eq!(v.bump(VersionBump::Major), SemVer::new(2, 0, 0));
        assert_eq!(v.bump(VersionBump::None), v);
        assert_eq!(SemVer::MAX.bump(VersionBump::Patch), SemVer::MAX);
        assert_eq!(SemVer::MAX.checked_bump(VersionBump::Patch), None);
    }

    /// Finding 39: below 1.0.0 the bump follows Cargo's convention, so the
    /// version a change produces agrees with the caret-rule verdict on it.
    #[test]
    fn zero_major_bumps_follow_cargo() {
        let v = SemVer::new(0, 3, 0);
        assert_eq!(v.bump(VersionBump::Major), SemVer::new(0, 4, 0));
        assert_eq!(v.bump(VersionBump::Minor), SemVer::new(0, 3, 1));
        assert_eq!(v.bump(VersionBump::Patch), SemVer::new(0, 3, 1));
        let v = SemVer::new(0, 0, 4);
        for b in [VersionBump::Major, VersionBump::Minor, VersionBump::Patch] {
            assert_eq!(v.bump(b), SemVer::new(0, 0, 5));
        }
        // Before the fix, 0.3.0 + MINOR gave 0.4.0, which the caret rules call breaking.
        assert_eq!(
            SemVer::new(0, 3, 0).compatibility(SemVer::new(0, 3, 0).bump(VersionBump::Minor)),
            Compatibility::Patch
        );
    }

    /// The bump → version mapping and `compatibility` agree everywhere: a
    /// MAJOR bump is breaking, MINOR and PATCH are not (except at 0.0.z,
    /// where Cargo calls every change breaking), and versions only go up.
    #[test]
    fn bump_and_compatibility_agree() {
        let big = u64::MAX;
        let versions = [
            SemVer::new(0, 0, 0),
            SemVer::new(0, 0, 7),
            SemVer::new(0, 1, 0),
            SemVer::new(0, 9, 3),
            SemVer::INITIAL,
            SemVer::new(1, 4, 2),
            SemVer::new(7, 0, 9),
            SemVer::new(1, big, 5),
            SemVer::new(1, 2, big),
            SemVer::new(0, 2, big),
            SemVer::new(0, 0, big),
            SemVer::new(0, big, big),
        ];
        for v in versions {
            for b in [VersionBump::Major, VersionBump::Minor, VersionBump::Patch] {
                let next = v.bump(b);
                assert!(next > v, "{v} + {b:?} = {next} must go up");
                let c = v.compatibility(next);
                let zero_zero = v.major == 0 && v.minor == 0;
                match b {
                    VersionBump::Major => assert_eq!(c, Compatibility::Breaking, "{v} + {b:?}"),
                    _ if zero_zero => assert_eq!(c, Compatibility::Breaking, "{v} + {b:?}"),
                    _ if v.checked_bump(b).is_some() => {
                        assert_ne!(c, Compatibility::Breaking, "{v} + {b:?} = {next}");
                    }
                    // Overflow carries into a more significant component: more conservative.
                    _ => {}
                }
            }
        }
    }

    /// Finding 58: overflow never makes a version go backwards or repeat.
    #[test]
    fn overflow_carries_instead_of_going_backwards() {
        let big = u64::MAX;
        // Before the fix: MAX.7.3 + MAJOR = MAX.0.0 (lower), 1.MAX.5 + MINOR =
        // 1.MAX.0 (lower), 1.2.MAX + PATCH = 1.2.MAX (the same version).
        assert_eq!(
            SemVer::new(1, big, 5).bump(VersionBump::Minor),
            SemVer::new(2, 0, 0)
        );
        assert_eq!(
            SemVer::new(1, 2, big).bump(VersionBump::Patch),
            SemVer::new(1, 3, 0)
        );
        assert_eq!(
            SemVer::new(1, big, big).bump(VersionBump::Patch),
            SemVer::new(2, 0, 0)
        );
        let top = SemVer::new(big, 7, 3);
        assert_eq!(top.bump(VersionBump::Major), SemVer::MAX);
        assert!(top.bump(VersionBump::Major) > top);
        assert_eq!(top.checked_bump(VersionBump::Major), None);
        assert_eq!(
            SemVer::new(1, 2, big).checked_bump(VersionBump::Patch),
            None
        );
        assert_eq!(
            SemVer::new(1, 2, 3).checked_bump(VersionBump::Minor),
            Some(SemVer::new(1, 3, 0))
        );
    }

    #[test]
    fn default_is_initial() {
        assert_eq!(SemVer::default(), SemVer::INITIAL);
    }

    #[test]
    fn caret_compatibility() {
        let pin = SemVer::new(1, 2, 0);
        assert_eq!(pin.compatibility(pin), Compatibility::Identical);
        assert_eq!(
            pin.compatibility(SemVer::new(1, 2, 5)),
            Compatibility::Patch
        );
        assert_eq!(
            pin.compatibility(SemVer::new(1, 3, 0)),
            Compatibility::Additive
        );
        assert_eq!(
            pin.compatibility(SemVer::new(2, 0, 0)),
            Compatibility::Breaking
        );
        assert_eq!(
            pin.compatibility(SemVer::new(1, 1, 0)),
            Compatibility::Breaking
        );
        let zero = SemVer::new(0, 3, 1);
        assert_eq!(
            zero.compatibility(SemVer::new(0, 4, 0)),
            Compatibility::Breaking
        );
        assert_eq!(
            zero.compatibility(SemVer::new(0, 3, 2)),
            Compatibility::Patch
        );
    }

    #[test]
    fn serde_as_string() {
        let json = serde_json::to_string(&SemVer::new(3, 0, 1)).expect("serializes");
        assert_eq!(json, "\"3.0.1\"");
        let back: SemVer = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, SemVer::new(3, 0, 1));
    }
}
