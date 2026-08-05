// Copyright © 2026 Jalapeno Labs

//! What the client can fail with.

use crate::proto::error::v1::{Error as ContractError, ErrorCode};
use std::backtrace::Backtrace;
use std::fmt::{Debug, Display, Formatter};

/// Anything that can go wrong talking to a satellite.
///
/// One type rather than a family, because a caller handling a failure almost
/// always wants the same three questions answered regardless of where it came
/// from: which contract code was it, is it worth retrying, and what happened.
///
/// The contents are boxed. A contract error plus a backtrace runs to well over a
/// hundred bytes, and an error that size makes every `Result` in the SDK that
/// large whether or not anything went wrong. Failures are the rare path and
/// should not tax the common one.
pub struct Error {
    inner: Box<Inner>,
}

struct Inner {
    kind: Kind,
    backtrace: Backtrace,
}

enum Kind {
    /// The satellite answered with a contract error.
    Contract(ContractError),

    /// The satellite could not be reached, or answered something undecodable.
    Transport(String),

    /// The satellite serves a proto major this SDK does not.
    Incompatible { satellite: u32, sdk: u32 },
}

impl Error {
    fn new(kind: Kind) -> Self {
        Self {
            inner: Box::new(Inner {
                kind,
                // Empty unless RUST_BACKTRACE asks for one, so this costs a few
                // instructions rather than a stack walk on every failure.
                backtrace: Backtrace::capture(),
            }),
        }
    }

    pub(crate) fn contract(error: ContractError) -> Self {
        Self::new(Kind::Contract(error))
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::new(Kind::Transport(message.into()))
    }

    pub(crate) fn incompatible(satellite: u32, sdk: u32) -> Self {
        Self::new(Kind::Incompatible { satellite, sdk })
    }

    /// The contract code, when the satellite named one.
    ///
    /// Absent for a failure that never reached a satellite, such as a refused
    /// connection. Match on this, never on the message: wording may change
    /// within a major version, codes may not.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        match &self.inner.kind {
            Kind::Contract(error) => ErrorCode::try_from(error.code).ok(),
            Kind::Transport(_) | Kind::Incompatible { .. } => None,
        }
    }

    /// Whether retrying the same request could plausibly succeed.
    ///
    /// Answered from the satellite's own `retryable` flag rather than from a
    /// match on the code, which is what makes an older SDK safe against a newer
    /// satellite: a code this build has never heard of still gets a usable
    /// answer.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match &self.inner.kind {
            Kind::Contract(error) => error.retryable,
            // A refused connection is usually a satellite that has not finished
            // starting.
            Kind::Transport(_) => true,
            // A version mismatch does not resolve itself.
            Kind::Incompatible { .. } => false,
        }
    }

    /// Whether the satellite reported that the thing asked for does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self.code(),
            Some(ErrorCode::ThreadNotFound | ErrorCode::TurnNotFound)
        )
    }

    /// Whether this SDK is too old for the satellite it was pointed at.
    #[must_use]
    pub fn is_incompatible(&self) -> bool {
        matches!(self.inner.kind, Kind::Incompatible { .. })
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.inner.kind {
            Kind::Contract(error) => {
                let code = ErrorCode::try_from(error.code).map_or_else(
                    |_unknown| format!("code {}", error.code),
                    |named| format!("{named:?}").to_uppercase(),
                );
                write!(f, "{code}: {}", error.message)
            }
            Kind::Transport(message) => {
                write!(f, "could not reach the satellite: {message}")
            }
            Kind::Incompatible { satellite, sdk } => write!(
                f,
                "this satellite serves proto v{satellite} and this SDK speaks v{sdk}. \
                 Upgrade the SDK, or point at a satellite on the same major."
            ),
        }
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{self}")?;
        write!(f, "{}", self.inner.backtrace)
    }
}

impl std::error::Error for Error {}

/// The client's result type.
pub type Result<T> = std::result::Result<T, Error>;
