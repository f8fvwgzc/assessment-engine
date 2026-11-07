//! The crate's public error type.
//!
//! One opaque [`Error`] with a `#[non_exhaustive]` [`ErrorKind`], in the style
//! of `std::io::Error`: callers branch on the kind, read the message, and walk
//! [`std::error::Error::source`] for the cause. New kinds can be added without
//! breaking them.

use std::fmt;

/// What went wrong, coarsely. Match on it with a wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The request is malformed: an empty change list, a non-finite number,
    /// a change set whose snapshots disagree on the entity, a duplicate id.
    InvalidRequest,
    /// The entity, or the version of it, does not exist.
    NotFound,
    /// An optimistic-concurrency check failed: the stored version is not the
    /// one the caller expected.
    Conflict,
    /// The policy, or a patch applied to it, does not describe a valid policy.
    InvalidPolicy,
    /// Reading or writing the registry journal failed.
    Storage,
}

impl ErrorKind {
    /// A short, stable, machine-readable name.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::InvalidPolicy => "invalid_policy",
            ErrorKind::Storage => "storage",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error every fallible operation of this crate returns.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// What kind of failure this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable message (lowercase, no trailing punctuation, without the cause).
    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub(crate) fn invalid_policy(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidPolicy, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
