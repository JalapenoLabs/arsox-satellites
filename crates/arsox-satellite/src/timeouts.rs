// Copyright © 2026 Jalapeno Labs

//! The bounds on every operation that can otherwise hang forever.
//!
//! Three operations in a turn have no natural end: a shell command, a model
//! request, and the harness itself. Each one, left unbounded, holds its thread
//! for the life of the process while reporting itself perfectly healthy. So each
//! carries a bound, and this module is where a thread's declared bounds and the
//! documented defaults meet.
//!
//! # Why these live together
//!
//! The three are enforced in three different places: the command runner kills a
//! command, the proxy aborts a request, the runner restarts a harness. What they
//! share is where they come from. Resolving them once, here, is what keeps
//! "absent means 30 minutes" from being written down in three files that drift.
//!
//! The turn wall clock is deliberately absent. Exceeding it is a budget outcome
//! rather than a hung operation, so it lives in [`Budget`] and is resolved by
//! [`crate::proxy::budget::Ceilings`] alongside the other two ceilings.
//!
//! [`Budget`]: arsox_sdk::proto::settings::v1::Budget

use arsox_sdk::proto::settings::v1::{ThreadSettings, Timeouts};
use std::time::Duration;

/// How long one exec command may run before it is killed.
///
/// Long enough for a cold `yarn install` on a slow network or a full release
/// build, which are the commands that legitimately take the longest. A bound
/// tight enough to interrupt one of those would fail turns for doing their work.
pub const DEFAULT_EXEC_COMMAND: Duration = Duration::from_mins(30);

/// How long one model request may take before it is abandoned.
///
/// Comfortably past the slowest completion a large context produces, and well
/// short of a turn. A request still open after this is a socket nothing is
/// coming back on, not a model thinking hard.
pub const DEFAULT_LLM_REQUEST: Duration = Duration::from_mins(10);

/// How long a harness may produce nothing at all before it is considered hung.
///
/// Measured against total silence rather than against progress, which is what
/// makes it safe to set this low relative to a turn: an agent running a
/// forty minute build still writes tool output around it, and a harness that
/// says nothing for a quarter of an hour has stopped rather than slowed.
pub const DEFAULT_HARNESS_IDLE: Duration = Duration::from_mins(15);

/// The bounds one thread's operations run under.
///
/// Resolved once from the thread's settings, then carried to the three places
/// that enforce them. Every field is a real bound rather than an option: a
/// thread that declared nothing gets the documented default, and "no bound at
/// all" is deliberately not expressible. The whole point of the table is that a
/// hung operation always ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// One exec command, setup and checker alike. On expiry the command is
    /// killed and its outcome returns to the agent as a failure.
    pub exec_command: Duration,

    /// One model request. On expiry it counts as an endpoint failure.
    pub llm_request: Duration,

    /// No output at all from the harness. On expiry it is restarted once.
    pub harness_idle: Duration,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            exec_command: DEFAULT_EXEC_COMMAND,
            llm_request: DEFAULT_LLM_REQUEST,
            harness_idle: DEFAULT_HARNESS_IDLE,
        }
    }
}

impl Bounds {
    /// Reads the bounds a thread declared, defaulting whatever it left out.
    #[must_use]
    pub fn from_timeouts(declared: Option<&Timeouts>) -> Self {
        let defaults = Self::default();

        let Some(declared) = declared else {
            return defaults;
        };

        Self {
            exec_command: bound(declared.exec_command.as_ref(), defaults.exec_command),
            llm_request: bound(declared.llm_request.as_ref(), defaults.llm_request),
            harness_idle: bound(declared.harness_idle.as_ref(), defaults.harness_idle),
        }
    }

    /// Reads the bounds a thread's settings declared.
    #[must_use]
    pub fn for_thread(settings: &ThreadSettings) -> Self {
        Self::from_timeouts(settings.timeouts.as_ref())
    }
}

/// One declared span, or `fallback` when it is absent or unusable.
///
/// Zero and negative spans fall back rather than being honored. A bound of zero
/// would kill every command the instant it started and restart every harness
/// before it drew breath, which nobody types on purpose, and a negative one is
/// not a span at all. Refusing them here means the enforcement sites never have
/// to ask whether their bound is real.
fn bound(
    declared: Option<&arsox_sdk::proto::common::v1::Duration>,
    fallback: Duration,
) -> Duration {
    declared
        .map(arsox_sdk::helpers::duration_to_nanos)
        .filter(|nanos| *nanos > 0)
        .and_then(|nanos| u64::try_from(nanos).ok())
        .map_or(fallback, Duration::from_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Duration as ProtoDuration;

    fn millis(millis: i32) -> ProtoDuration {
        ProtoDuration {
            seconds: 0,
            nanos: millis.saturating_mul(1_000_000),
        }
    }

    #[test]
    fn a_thread_that_declared_nothing_gets_the_documented_defaults() {
        // The README publishes these three numbers, so they are a contract
        // rather than a tuning knob nobody reads.
        let bounds = Bounds::from_timeouts(None);

        assert_eq!(bounds.exec_command, Duration::from_mins(30));
        assert_eq!(bounds.llm_request, Duration::from_mins(10));
        assert_eq!(bounds.harness_idle, Duration::from_mins(15));
    }

    #[test]
    fn a_declared_bound_wins_and_the_rest_still_default() {
        // Every field is independently optional on the wire, so declaring one
        // must not silently zero the other two.
        let bounds = Bounds::from_timeouts(Some(&Timeouts {
            harness_idle: Some(millis(250)),
            ..Timeouts::default()
        }));

        assert_eq!(bounds.harness_idle, Duration::from_millis(250));
        assert_eq!(bounds.exec_command, DEFAULT_EXEC_COMMAND);
        assert_eq!(bounds.llm_request, DEFAULT_LLM_REQUEST);
    }

    #[test]
    fn a_zero_or_negative_bound_falls_back_rather_than_ending_everything_instantly() {
        // A bound of zero would kill every command as it started. Reading it as
        // written would turn a typo into a satellite that runs nothing.
        let bounds = Bounds::from_timeouts(Some(&Timeouts {
            exec_command: Some(millis(0)),
            llm_request: Some(ProtoDuration {
                seconds: -5,
                nanos: 0,
            }),
            harness_idle: None,
        }));

        assert_eq!(bounds, Bounds::default());
    }

    #[test]
    fn settings_resolve_to_the_same_bounds_their_timeouts_do() {
        let declared = Timeouts {
            exec_command: Some(millis(10)),
            ..Timeouts::default()
        };

        assert_eq!(
            Bounds::for_thread(&ThreadSettings {
                timeouts: Some(declared),
                ..ThreadSettings::default()
            }),
            Bounds::from_timeouts(Some(&declared)),
        );
    }
}
