use std::borrow::Cow;
use std::fmt;

/// How a board or layer operation ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The operation completed.
    Done,
    /// The operation or caller was invalid or lacked permission.
    Refused,
    /// The operation could not complete now.
    Unavailable,
    /// The operation could not be served as requested.
    NotSupported,
}

/// An unsuccessful operation and its reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    outcome: Outcome,
    reason: Cow<'static, str>,
    limit: bool,
}

impl Error {
    /// Reject an invalid or unauthorized operation.
    pub fn refused(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            outcome: Outcome::Refused,
            reason: reason.into(),
            limit: false,
        }
    }

    /// Report a transient failure, a timeout or a limit.
    pub fn unavailable(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            outcome: Outcome::Unavailable,
            reason: reason.into(),
            limit: false,
        }
    }

    /// Report that the request exceeds a layer or board capability.
    pub fn not_supported(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            outcome: Outcome::NotSupported,
            reason: reason.into(),
            limit: false,
        }
    }

    /// Mark that a configured limit ended the operation, keeping its outcome.
    pub fn at_limit(mut self) -> Self {
        self.limit = true;
        self
    }

    /// Whether a configured limit ended the operation.
    pub const fn is_limit(&self) -> bool {
        self.limit
    }

    /// Return the outcome used by callers to decide whether to retry.
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }
}

impl fmt::Display for Error {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(&self.reason)
    }
}

impl std::error::Error for Error {}
