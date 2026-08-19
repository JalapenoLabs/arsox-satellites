// Copyright © 2026 Jalapeno Labs

//! Running a repo's setup commands after it is cloned.
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
//! # Nothing is dropped
//!
//! A command that never ran because a barrier above it failed is reported as
//! skipped rather than omitted. "Not run" and "found nothing" are different
//! facts, and a report that cannot tell them apart is the silent failure the
//! incident system exists to prevent.

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
const NOT_LAUNCHED: i32 = -1;

/// What one command did.
#[derive(Debug, Clone)]
pub(super) struct CommandOutcome {
    pub command: String,
    pub exit_code: i32,

    /// Standard output and standard error together, oldest lines dropped first
    /// once the capture cap is reached.
    pub output: String,
}

impl CommandOutcome {
    fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// Everything a setup string amounted to.
#[derive(Debug, Clone, Default)]
pub(super) struct SetupRun {
    pub outcomes: Vec<CommandOutcome>,

    /// Commands a failed barrier above them kept from ever starting.
    pub skipped: Vec<String>,
}

impl SetupRun {
    /// The commands that exited nonzero, in the order they finished.
    pub fn failures(&self) -> impl Iterator<Item = &CommandOutcome> {
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

/// Runs a setup string with the working directory at `working_dir`.
///
/// Stops at the first barrier whose group did not fully succeed, and reports
/// everything below it as skipped.
pub(super) async fn run(commands: &str, working_dir: &Path) -> SetupRun {
    let mut run = SetupRun::default();
    let mut groups = plan(commands).into_iter();

    for group in groups.by_ref() {
        // Concurrently rather than one after another: within a group nothing
        // depends on anything else, which is the entire meaning of a newline.
        let finished = futures_util::future::join_all(group.into_iter().map(|command| {
            let working_dir = working_dir.to_path_buf();
            async move { execute(command, &working_dir).await }
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
async fn execute(command: String, working_dir: &Path) -> CommandOutcome {
    let (program, flag) = shell();

    let output = crate::harness::spawn::scrubbed_command(program)
        .arg(flag)
        .arg(&command)
        .current_dir(working_dir)
        // A setup command that reads stdin would block forever on a terminal
        // that is not there.
        .stdin(std::process::Stdio::null())
        .output()
        .await;

    match output {
        Ok(finished) => {
            let mut said = String::from_utf8_lossy(&finished.stdout).into_owned();
            said.push_str(&String::from_utf8_lossy(&finished.stderr));

            CommandOutcome {
                command,
                // `None` means a signal killed it, which is a failure with no
                // number of its own.
                exit_code: finished.status.code().unwrap_or(NOT_LAUNCHED),
                output: keep_tail(&said),
            }
        }
        Err(error) => CommandOutcome {
            command,
            exit_code: NOT_LAUNCHED,
            output: format!("could not run the command: {error}"),
        },
    }
}

/// The shell a setup command is written for.
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

/// Truncates from the front, keeping the end where the error is.
fn keep_tail(output: &str) -> String {
    if output.len() <= MAX_CAPTURED_OUTPUT {
        return output.to_owned();
    }

    // Nudged forward to a character boundary rather than cut at the byte
    // offset, because output is arbitrary UTF-8 and a build log full of box
    // drawing is normal. Slicing mid-character would panic instead of truncate.
    let cut = output.len() - MAX_CAPTURED_OUTPUT;
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
        let path = std::env::temp_dir().join(format!("arsox-setup-{}", uuid::Uuid::now_v7()));
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

        let run = run("exit 3\nexit 0", &directory).await;

        assert_eq!(run.outcomes.len(), 2, "both commands in a group run");
        assert!(run.failures().count() == 1);
        assert!(run.skipped.is_empty());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_failed_barrier_stops_everything_below_it() {
        // And says so. A command that never ran is skipped, not missing.
        let directory = scratch();

        let run = run("exit 1; exit 0\nexit 0", &directory).await;

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

        let run = run("exit 0; exit 0\nexit 0", &directory).await;

        assert_eq!(run.outcomes.len(), 3);
        assert_eq!(run.failures().count(), 0);
        assert!(run.skipped.is_empty());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_command_runs_with_the_working_directory_at_the_repo() {
        // Setup commands are written as if somebody had cd'd into the checkout,
        // because that is the only place `yarn install` means anything.
        let directory = scratch();

        let run = run("echo hello > proof.txt", &directory).await;

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

        let run = run("echo something-went-wrong", &directory).await;

        assert!(run.outcomes[0].output.contains("something-went-wrong"));

        drop(std::fs::remove_dir_all(&directory));
    }

    #[test]
    fn a_runaway_log_is_truncated_from_the_front() {
        // The tail is where the error is, and an untruncated build log would
        // reach a database row that outlives the thread.
        let flood = "x".repeat(MAX_CAPTURED_OUTPUT * 2);

        let kept = keep_tail(&format!("{flood}the actual error"));

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
        let kept = keep_tail(&flood);

        assert!(kept.ends_with('é'));
        assert!(
            kept.len() > MAX_CAPTURED_OUTPUT / 2,
            "the cut should land on the nearest boundary, not discard the tail"
        );
    }
}
