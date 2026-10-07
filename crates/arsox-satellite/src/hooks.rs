// Copyright © 2026 Jalapeno Labs

//! The commands a thread runs after every turn's work, before its artifacts are
//! scanned.
//!
//! A host declares them in `ThreadSettings.turn_end_hooks` for deterministic
//! post-processing an agent should not be trusted to remember, such as
//! converting what it produced into the format the host stores. The runner runs
//! them one at a time, in declaration order, once the work is over however it
//! ended, and runs none for a cancelled turn.
//!
//! # How one runs
//!
//! - **As the agent**, through [`crate::harness::spawn::scrubbed_command`], so it
//!   carries none of the satellite's credentials and none of its privilege.
//! - **From the thread's workspace root**, with the environment a service gets:
//!   the scrubbed base, the thread's declared variables, and where the turn's
//!   services listen. It is host configuration rather than anything an agent
//!   chose, so the exec broker does not shape its `PATH`.
//! - **Its argv as declared.** No shell reads it, so nothing in it is expanded
//!   or split; the program is an absolute path or a name on the satellite's
//!   `PATH`.
//! - **Through [`crate::supervise`]**, as the leader of its own process group,
//!   so a hook that starts Blender and runs past its bound is stopped with
//!   everything it started.
//!
//! # A failure never fails the turn
//!
//! The work a hook runs after is already done. A hook that exits nonzero, runs
//! past its bound, or cannot start is reported as what it was, recorded by the
//! runner as a degraded `TURN_END_HOOK_FAILED` incident, and the hooks after it
//! and the artifact scan run regardless.

use crate::harness::spawn::AgentVar;
use crate::supervise::{Ending, Run};
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::settings::v1::TurnEndHook;
use arsox_sdk::proto::turn::v1::{TurnEndHookOutcome, TurnEndHookResult};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// How long a hook runs when it declares no timeout.
///
/// Long enough to export a heavy scene or transcode a render, short enough that
/// the thread's next turn, which waits behind every hook, is not held for an
/// hour by a hook that hung.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);

/// The longest timeout a hook may declare, from the contract.
pub const MAX_TIMEOUT: Duration = Duration::from_hours(1);

/// Longest hook name, the same bound a service or an MCP server name has.
pub const MAX_NAME: usize = 64;

/// Why a thread's hook declarations may not be used, when they may not.
///
/// # Errors
///
/// Returns the first reason found, naming `settings.turn_end_hooks` and the
/// hook, which is enough for a caller to fix and resubmit.
pub fn refusal(hooks: &[TurnEndHook]) -> Result<(), String> {
    let mut names = BTreeSet::new();

    for hook in hooks {
        let refused = |reason: &str| {
            Err(format!(
                "settings.turn_end_hooks: hook {:?} {reason}",
                hook.name
            ))
        };

        let well_formed = !hook.name.is_empty()
            && hook.name.len() <= MAX_NAME
            && hook
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !well_formed {
            return refused("must be 1 to 64 of A-Z a-z 0-9 _ -");
        }

        if !names.insert(hook.name.to_ascii_lowercase()) {
            return refused("is declared twice, ignoring case");
        }

        match hook.argv.first() {
            None => return refused("has an empty argv, so there is no program to run"),
            Some(program) if program.is_empty() => {
                return refused("names an empty program as argv[0]");
            }
            Some(_program) => {}
        }

        // A NUL cannot cross into a process's argument list, so the hook would
        // fail every turn for a reason its declaration could have been told.
        if hook.argv.iter().any(|argument| argument.contains('\0')) {
            return refused("has a NUL byte in its argv");
        }

        if let Some(timeout) = hook.timeout.as_ref() {
            let nanos = arsox_sdk::helpers::duration_to_nanos(timeout);
            let max = i64::try_from(MAX_TIMEOUT.as_nanos()).unwrap_or(i64::MAX);

            if nanos <= 0 || nanos > max {
                return refused("must declare a timeout that is positive and at most one hour");
            }
        }
    }

    Ok(())
}

/// What one hook needs from the turn it runs after.
#[derive(Debug, Clone, Copy)]
pub struct Surroundings<'a> {
    /// The thread's workspace root, which is the hook's working directory.
    pub working_dir: &'a Path,

    /// The thread's declared variables, then where the turn's services listen.
    pub environment: &'a [AgentVar],
}

/// Runs one hook to its end or its bound and reports what it did.
///
/// The output is unmasked: the runner masks the result with the thread's
/// redactor on every path it leaves by.
pub async fn run(hook: &TurnEndHook, around: Surroundings<'_>) -> TurnEndHookResult {
    let bound = crate::timeouts::bound(hook.timeout.as_ref(), DEFAULT_TIMEOUT);

    // Validated at thread creation, so an empty argv only reaches here from
    // settings stored before that check existed. It is reported as a hook that
    // could not start rather than skipped.
    let Some((program, arguments)) = hook.argv.split_first() else {
        return result_of(
            hook,
            &Run {
                ending: Ending::NotLaunched("its argv is empty".to_owned()),
                output: "the hook declares no program to run".to_owned(),
                elapsed: Duration::ZERO,
            },
        );
    };

    let mut command = crate::harness::spawn::scrubbed_command(program);
    command.args(arguments).current_dir(around.working_dir);

    for variable in around.environment {
        command.env(&variable.key, &variable.value);
    }

    // Nothing stops a hook early but its bound: a turn that was cancelled runs
    // no hooks at all, and one that was not has nothing left to stop for.
    let run =
        crate::supervise::run(command, bound, std::future::pending(), "a turn end hook").await;

    result_of(hook, &run)
}

/// The contract's account of one run.
fn result_of(hook: &TurnEndHook, run: &Run) -> TurnEndHookResult {
    let (outcome, exit_code) = match &run.ending {
        Ending::Exited(0) => (TurnEndHookOutcome::Succeeded, Some(0)),
        Ending::Exited(code) => (TurnEndHookOutcome::Failed, Some(*code)),
        // Ended by a signal nobody here sent. Stopped cannot happen, since
        // nothing asks a hook to stop, and would mean the same if it did.
        Ending::Signalled | Ending::Stopped => (TurnEndHookOutcome::Failed, None),
        Ending::TimedOut(_bound) => (TurnEndHookOutcome::TimedOut, None),
        Ending::NotLaunched(_reason) => (TurnEndHookOutcome::NotLaunched, None),
    };

    let output_tail = match &run.ending {
        // The tail is what a host reads, and a log that simply stops mid-export
        // reads like a crash.
        Ending::TimedOut(bound) => format!(
            "the hook was stopped after {bound:?} without finishing\n{}",
            run.output
        ),
        Ending::NotLaunched(reason) => format!("the hook could not be started: {reason}"),
        Ending::Exited(_) | Ending::Signalled | Ending::Stopped => run.output.clone(),
    };

    TurnEndHookResult {
        name: hook.name.clone(),
        outcome: outcome.into(),
        exit_code,
        output_tail,
        elapsed: Some(ProtoDuration {
            seconds: i64::try_from(run.elapsed.as_secs()).unwrap_or(i64::MAX),
            nanos: i32::try_from(run.elapsed.subsec_nanos()).unwrap_or_default(),
        }),
    }
}

/// Whether a hook did what it was declared to do.
#[must_use]
pub fn succeeded(result: &TurnEndHookResult) -> bool {
    result.outcome == i32::from(TurnEndHookOutcome::Succeeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, argv: &[&str]) -> TurnEndHook {
        TurnEndHook {
            name: name.to_owned(),
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            timeout: None,
        }
    }

    fn timed(seconds: i64) -> TurnEndHook {
        TurnEndHook {
            timeout: Some(ProtoDuration { seconds, nanos: 0 }),
            ..hook("export", &["/opt/export"])
        }
    }

    #[test]
    fn a_well_formed_list_is_accepted() {
        assert_eq!(
            refusal(&[
                hook("export-glb", &["/opt/elysium/bin/export-glb"]),
                timed(3600)
            ]),
            Ok(())
        );
    }

    #[test]
    fn each_rule_is_refused_naming_the_hook() {
        let cases = [
            vec![hook("", &["/bin/true"])],
            vec![hook("has space", &["/bin/true"])],
            vec![hook(&"x".repeat(MAX_NAME + 1), &["/bin/true"])],
            vec![
                hook("Export", &["/bin/true"]),
                hook("export", &["/bin/true"]),
            ],
            vec![hook("empty", &[])],
            vec![hook("blank", &[""])],
            vec![hook("nul", &["/bin/echo", "a\0b"])],
            vec![timed(0)],
            vec![timed(-5)],
            vec![timed(3601)],
        ];

        for hooks in cases {
            let reason = refusal(&hooks).expect_err("the declaration breaks a rule");
            assert!(
                reason.starts_with("settings.turn_end_hooks: hook "),
                "{reason}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_ending_is_reported_as_the_contract_names_it() {
        let directory = std::env::temp_dir();
        let around = Surroundings {
            working_dir: &directory,
            environment: &[AgentVar {
                key: "GREETING".to_owned(),
                value: "hello".to_owned(),
                secret: false,
            }],
        };

        let ok = run(&hook("ok", &["/bin/sh", "-c", "echo $GREETING"]), around).await;
        assert!(succeeded(&ok));
        assert_eq!(ok.exit_code, Some(0));
        assert_eq!(
            ok.output_tail.trim(),
            "hello",
            "it sees the declared variables"
        );

        let failed = run(
            &hook("failed", &["/bin/sh", "-c", "echo nope; exit 3"]),
            around,
        )
        .await;
        assert_eq!(failed.outcome, i32::from(TurnEndHookOutcome::Failed));
        assert_eq!(failed.exit_code, Some(3));
        assert!(failed.output_tail.contains("nope"));

        let missing = run(&hook("missing", &["/no/such/program"]), around).await;
        assert_eq!(missing.outcome, i32::from(TurnEndHookOutcome::NotLaunched));
        assert_eq!(missing.exit_code, None);

        let slow = TurnEndHook {
            timeout: Some(ProtoDuration {
                seconds: 0,
                nanos: 200_000_000,
            }),
            ..hook("slow", &["/bin/sh", "-c", "echo started; sleep 30"])
        };
        let stopped = run(&slow, around).await;
        assert_eq!(stopped.outcome, i32::from(TurnEndHookOutcome::TimedOut));
        assert_eq!(stopped.exit_code, None);
        assert!(stopped.output_tail.contains("stopped after"));
        assert!(stopped.output_tail.contains("started"));
    }
}
