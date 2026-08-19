// Copyright © 2026 Jalapeno Labs

//! Running a repo's declared commands: setup at one end of a turn, checkers at
//! the other.
//!
//! # The separator is the schedule
//!
//! Setup commands and checkers share one syntax, deliberately, because they are
//! the same idea at two ends of a turn: a list of shell commands whose order
//! matters in exactly one way.
//!
//! - Commands separated by **newlines** run in parallel and do not fail fast.
//! - Commands separated by **semicolons** are barriers: nothing below a barrier
//!   starts until everything above it has succeeded.
//!
//! So `yarn install; yarn lint\nyarn typecheck` installs first, then lints and
//! typechecks at the same time, and a failing lint does not stop the typecheck
//! from reporting. That is the whole schedule, and it is enough: the expensive
//! serial step is almost always one install, and everything after it is
//! independent.
//!
//! # One parser, both ends
//!
//! The contract gives setup commands and checkers the same syntax, so they get
//! the same parser. Two copies would be two chances to disagree about what a
//! semicolon means, in two files nobody edits together, and the disagreement
//! would surface as a checker that ran a command a setup script would have held
//! back.
//!
//! # Nothing is dropped
//!
//! A command that never ran because a barrier above it failed is reported as
//! skipped rather than omitted. "Not run" and "found nothing" are different
//! facts, and a report that cannot tell them apart is the silent failure the
//! incident system exists to prevent.

use crate::harness::spawn::AgentVar;
use std::path::Path;

/// Output kept per command.
///
/// A failed install is the reason this is captured at all, and its useful part
/// is the end: the error is on the last lines and the preamble is a package
/// list. Truncating from the front keeps the tail, and keeps one runaway build
/// log out of a database row that outlives the thread.
const MAX_CAPTURED_OUTPUT: usize = 16 * 1024;

/// Exit code recorded when a command could not be started at all.
///
/// Distinct from any code a shell reports so "the shell is missing" never reads
/// as "the command failed".
pub(crate) const NOT_LAUNCHED: i32 = -1;

/// What one command did.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutcome {
    pub command: String,
    pub exit_code: i32,

    /// Standard output and standard error together, oldest lines dropped first
    /// once the capture cap is reached.
    pub output: String,
}

impl CommandOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// Everything a command string amounted to.
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandRun {
    pub outcomes: Vec<CommandOutcome>,

    /// Commands a failed barrier above them kept from ever starting.
    pub skipped: Vec<String>,
}

impl CommandRun {
    /// The commands that exited nonzero, in the order they finished.
    pub(crate) fn failures(&self) -> impl Iterator<Item = &CommandOutcome> {
        self.outcomes.iter().filter(|outcome| !outcome.succeeded())
    }
}

/// Splits a setup or checker string into the barrier-separated groups it means.
///
/// Each group is a set of commands that run at the same time, and the groups
/// themselves run in order. Blank lines and empty segments are dropped, so
/// trailing separators and generous formatting cost nothing.
fn plan(commands: &str) -> Vec<Vec<String>> {
    commands
        .split(';')
        .map(|barrier| {
            barrier
                .lines()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .filter(|group| !group.is_empty())
        .collect()
}

/// Runs a command string with the working directory at `working_dir`.
///
/// `env` is the thread's declared environment, which reaches these commands
/// exactly as it reaches the agent working in the same checkout: an install that
/// needs a registry token needs it here too, and so does the lint that runs
/// afterwards.
///
/// Stops at the first barrier whose group did not fully succeed, and reports
/// everything below it as skipped.
pub(crate) async fn run(commands: &str, working_dir: &Path, env: &[AgentVar]) -> CommandRun {
    let mut run = CommandRun::default();
    let mut groups = plan(commands).into_iter();

    for group in groups.by_ref() {
        // Concurrently rather than one after another: within a group nothing
        // depends on anything else, which is the entire meaning of a newline.
        let finished = futures_util::future::join_all(group.into_iter().map(|command| {
            let working_dir = working_dir.to_path_buf();
            async move { execute(command, &working_dir, env).await }
        }))
        .await;

        let barrier_held = finished.iter().all(CommandOutcome::succeeded);
        run.outcomes.extend(finished);

        if !barrier_held {
            break;
        }
    }

    run.skipped = groups.flatten().collect();

    run
}

/// Runs one command through a shell and captures everything it said.
///
/// Through a shell rather than tokenized, because these are written as shell
/// text: `yarn generate && yarn build` is one entry in the contract and means
/// what a shell says it means. The commands come from the host application
/// rather than from an agent, so this is configuration being executed as
/// configured, not an injection surface.
async fn execute(command: String, working_dir: &Path, env: &[AgentVar]) -> CommandOutcome {
    let (program, flag) = shell();

    let mut process = crate::harness::spawn::scrubbed_command(program);
    process
        .arg(flag)
        .arg(&command)
        .current_dir(working_dir)
        // A command that reads stdin would block forever on a terminal that is
        // not there.
        .stdin(std::process::Stdio::null());

    // Applied on top of the scrub, exactly as they are for a harness. The
    // refusal rule already ran when the list was built, so nothing here can put
    // back what the scrub removed.
    for variable in env {
        process.env(&variable.key, &variable.value);
    }

    let output = process.output().await;

    match output {
        Ok(finished) => {
            let mut said = String::from_utf8_lossy(&finished.stdout).into_owned();
            said.push_str(&String::from_utf8_lossy(&finished.stderr));

            CommandOutcome {
                command,
                // `None` means a signal killed it, which is a failure with no
                // number of its own.
                exit_code: finished.status.code().unwrap_or(NOT_LAUNCHED),
                output: tail(&said, MAX_CAPTURED_OUTPUT),
            }
        }
        Err(error) => CommandOutcome {
            command,
            exit_code: NOT_LAUNCHED,
            output: format!("could not run the command: {error}"),
        },
    }
}

/// The shell a declared command is written for.
///
/// The satellite image is Linux and `sh` is what these commands assume. The
/// Windows arm exists so the same code path runs on a developer machine rather
/// than being skipped there and first exercised in CI.
const fn shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

/// Truncates from the front, keeping the last `keep` bytes where the error is.
///
/// Shared by the capture cap and by anything that has to fit the same output
/// into a smaller space, such as the prompt that hands a failing checker back
/// to an agent. One rule about which end of a log matters, applied everywhere
/// output is shortened.
pub(crate) fn tail(output: &str, keep: usize) -> String {
    if output.len() <= keep {
        return output.to_owned();
    }

    // Nudged forward to a character boundary rather than cut at the byte
    // offset, because output is arbitrary UTF-8 and a build log full of box
    // drawing is normal. Slicing mid-character would panic instead of truncate.
    let cut = output.len() - keep;
    let start = (cut..output.len())
        .find(|index| output.is_char_boundary(*index))
        .unwrap_or(output.len());

    format!("[truncated]\n{}", &output[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory these commands can run in.
    fn scratch() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("arsox-commands-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("should create");
        path
    }

    #[test]
    fn newlines_group_and_semicolons_separate() {
        // The README's own example, which is the contract this parser owes.
        let groups = plan(
            "yarn install;
             yarn lint
             yarn typecheck
             yarn generate && yarn build
             ; yarn deploy --dry-run",
        );

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], vec!["yarn install"]);
        assert_eq!(
            groups[1],
            vec!["yarn lint", "yarn typecheck", "yarn generate && yarn build"]
        );
        assert_eq!(groups[2], vec!["yarn deploy --dry-run"]);
    }

    #[test]
    fn blank_lines_and_trailing_separators_cost_nothing() {
        assert!(plan("").is_empty());
        assert!(plan("  \n \n ;; \n").is_empty());
        assert_eq!(plan("yarn install;\n\n;"), vec![vec!["yarn install"]]);
    }

    #[tokio::test]
    async fn a_group_runs_every_command_even_after_one_fails() {
        // The point of a newline: a failing lint must not hide a typecheck's
        // result. Both are reported, and the report is what the agents get.
        let directory = scratch();

        let run = run("exit 3\nexit 0", &directory, &[]).await;

        assert_eq!(run.outcomes.len(), 2, "both commands in a group run");
        assert!(run.failures().count() == 1);
        assert!(run.skipped.is_empty());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_failed_barrier_stops_everything_below_it() {
        // And says so. A command that never ran is skipped, not missing.
        let directory = scratch();

        let run = run("exit 1; exit 0\nexit 0", &directory, &[]).await;

        assert_eq!(
            run.outcomes.len(),
            1,
            "the barrier failed, so nothing else ran"
        );
        assert_eq!(run.outcomes[0].exit_code, 1);
        assert_eq!(run.skipped.len(), 2, "what did not run is reported");

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_held_barrier_releases_the_group_below_it() {
        let directory = scratch();

        let run = run("exit 0; exit 0\nexit 0", &directory, &[]).await;

        assert_eq!(run.outcomes.len(), 3);
        assert_eq!(run.failures().count(), 0);
        assert!(run.skipped.is_empty());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_command_runs_with_the_working_directory_at_the_repo() {
        // Setup commands and checkers are both written as if somebody had cd'd
        // into the checkout, because that is the only place `yarn install` or
        // `yarn lint` means anything.
        let directory = scratch();

        let run = run("echo hello > proof.txt", &directory, &[]).await;

        assert_eq!(run.outcomes[0].exit_code, 0, "{:?}", run.outcomes[0].output);
        assert!(
            directory.join("proof.txt").exists(),
            "the command ran somewhere else"
        );

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn output_is_captured_so_a_failure_says_why() {
        let directory = scratch();

        let run = run("echo something-went-wrong", &directory, &[]).await;

        assert!(run.outcomes[0].output.contains("something-went-wrong"));

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_declared_variable_reaches_the_command() {
        // A repo's install reaches its registry token this way, and so does the
        // lint that runs against the same checkout at the other end of the turn.
        let directory = scratch();
        let declared = [AgentVar {
            key: "ARSOX_TEST_DECLARED".to_owned(),
            value: "the-declared-value".to_owned(),
            secret: false,
        }];

        let run = run(ECHO_DECLARED, &directory, &declared).await;

        assert!(
            run.outcomes[0].output.contains("the-declared-value"),
            "got {:?}",
            run.outcomes[0].output
        );

        drop(std::fs::remove_dir_all(&directory));
    }

    /// A command that prints the declared variable, whatever shell this is.
    ///
    /// `cmd` spells expansion with percent signs and `sh` with a dollar, and the
    /// two cannot be written as one string.
    const ECHO_DECLARED: &str = if cfg!(windows) {
        "echo %ARSOX_TEST_DECLARED%"
    } else {
        "echo $ARSOX_TEST_DECLARED"
    };

    #[test]
    fn a_runaway_log_is_truncated_from_the_front() {
        // The tail is where the error is, and an untruncated build log would
        // reach a database row that outlives the thread.
        let flood = "x".repeat(MAX_CAPTURED_OUTPUT * 2);

        let kept = tail(&format!("{flood}the actual error"), MAX_CAPTURED_OUTPUT);

        assert!(kept.len() < MAX_CAPTURED_OUTPUT + 64);
        assert!(kept.ends_with("the actual error"));
        assert!(kept.starts_with("[truncated]"));
        // The cap is a cap, not a token: what survives fills it rather than
        // being trimmed to the last line or the last character.
        assert!(kept.len() > MAX_CAPTURED_OUTPUT);
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        let flood = "é".repeat(MAX_CAPTURED_OUTPUT);

        // A byte offset into the middle of a multi-byte character would panic
        // here rather than truncate, and build logs are full of them.
        let kept = tail(&flood, MAX_CAPTURED_OUTPUT);

        assert!(kept.ends_with('é'));
        assert!(
            kept.len() > MAX_CAPTURED_OUTPUT / 2,
            "the cut should land on the nearest boundary, not discard the tail"
        );
    }

    #[test]
    fn a_smaller_budget_keeps_the_same_end_of_the_output() {
        // The fix prompt asks for a shorter tail than the capture cap, and it
        // has to be the same end: an agent handed the preamble of a build log
        // is handed everything except the failure.
        let kept = tail("preamble-preamble-the actual error", 16);

        assert!(kept.ends_with("the actual error"));
        assert!(!kept.contains("preamble-preamble"));
    }
}
