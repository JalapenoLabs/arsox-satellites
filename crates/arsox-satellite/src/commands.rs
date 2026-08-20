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
//!
//! # Every command is bounded
//!
//! A command has no natural end. A test suite waiting on a prompt, an install
//! against a remote that stopped answering, a build that deadlocked: each one
//! holds its thread for the life of the process while looking exactly like work
//! in progress. So every command runs under [`Execution::bound`], and one past
//! it is killed and reported as a failure that says it timed out.
//!
//! **Killing the command kills the shell, and only the shell.** These run
//! through `sh -c`, so `yarn install` is a grandchild of the satellite, and
//! without a process group on Unix or a job object on Windows a grandchild
//! outlives the parent that is killed above it. An orphan keeps running until
//! the container stops. That is a real limit of what is built here rather than
//! something to imply away: the bound reliably ends the satellite's *wait*, and
//! reliably ends the shell, and process-group teardown is the separate piece of
//! work that would end everything below it.

use crate::harness::spawn::AgentVar;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _};

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

/// Exit code recorded when a command was killed for exceeding its bound.
///
/// Its own code for the same reason [`NOT_LAUNCHED`] has one: "it never
/// finished" and "it finished badly" lead a reader to look in different places,
/// and a shared code would hide the first behind the second.
pub(crate) const TIMED_OUT: i32 = -2;

/// How a thread's declared commands run.
///
/// The environment, the bound, and the mask travel together because every call
/// site needs all three, and a signature that took them separately is a
/// signature that can be called with one. Setup commands and checkers are the
/// same commands at two ends of a turn, so they run under the same three.
#[derive(Debug, Clone)]
pub struct Execution {
    /// The thread's declared variables, applied on top of the scrubbed
    /// environment exactly as they are for the agent that works in the same
    /// checkout afterwards.
    pub env: Vec<AgentVar>,

    /// How long any one command may run before it is killed.
    pub bound: Duration,

    /// The thread's secrets, masked out of captured output as it is captured.
    ///
    /// A command runs with the thread's credentials in its environment, so a
    /// failing install that echoes its own registry token is an ordinary
    /// Tuesday. Masking here rather than at each of the places captured output
    /// travels to means the incident, the checker result, the turn report, and
    /// the prompt that hands a failure back to the agent all carry the same
    /// masked text, from one scan.
    pub redactor: crate::redaction::Redactor,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            bound: crate::timeouts::DEFAULT_EXEC_COMMAND,
            redactor: crate::redaction::Redactor::none(),
        }
    }
}

impl Execution {
    /// Resolves how one thread's commands run, from its settings.
    #[must_use]
    pub fn for_thread(settings: &arsox_sdk::proto::settings::v1::ThreadSettings) -> Self {
        Self {
            env: crate::harness::spawn::declared_environment(&settings.env),
            bound: crate::timeouts::Bounds::for_thread(settings).exec_command,
            redactor: crate::redaction::Redactor::for_thread(settings),
        }
    }
}

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

    /// The outcome as anything outside the satellite may see it.
    ///
    /// Applied at the moment of capture, so every place this travels to, the
    /// incident, the checker result, the turn report, and the prompt that hands
    /// a failure back to the agent, is masked from one scan rather than from
    /// four that could each be forgotten.
    fn masked(mut self, redactor: &crate::redaction::Redactor) -> Self {
        redactor.redact_in_place(&mut self.command);
        redactor.redact_in_place(&mut self.output);

        self
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
/// `execution` carries the thread's declared environment, which reaches these
/// commands exactly as it reaches the agent working in the same checkout, and
/// the bound each of them runs under.
///
/// Stops at the first barrier whose group did not fully succeed, and reports
/// everything below it as skipped.
pub(crate) async fn run(commands: &str, working_dir: &Path, execution: &Execution) -> CommandRun {
    let mut run = CommandRun::default();
    let mut groups = plan(commands).into_iter();

    for group in groups.by_ref() {
        // Concurrently rather than one after another: within a group nothing
        // depends on anything else, which is the entire meaning of a newline.
        //
        // The bound is per command rather than per group, so one command that
        // hangs is killed on its own schedule while its siblings finish.
        let finished = futures_util::future::join_all(group.into_iter().map(|command| {
            let working_dir = working_dir.to_path_buf();
            async move { execute(command, &working_dir, execution).await }
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
///
/// A command that outlives `execution.bound` is killed and reported as a failure
/// that says so, carrying whatever it had printed by then. The tail of a hung
/// build is the only evidence of what it was doing when it stopped, so it is
/// kept rather than discarded along with the process.
///
/// Every outcome leaves through [`Execution::redactor`], the command text
/// included: a checker written as `deploy --token ghp_...` puts a credential in
/// the command rather than in its output, and both end up in the same report.
async fn execute(command: String, working_dir: &Path, execution: &Execution) -> CommandOutcome {
    let (program, flag) = shell();

    let mut process = crate::harness::spawn::scrubbed_command(program);
    process
        .arg(flag)
        .arg(&command)
        .current_dir(working_dir)
        // A command that reads stdin would block forever on a terminal that is
        // not there.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The backstop for every path that drops the child without killing it
        // first, including a caller cancelled out from under this future.
        .kill_on_drop(true);

    // Applied on top of the scrub, exactly as they are for a harness. The
    // refusal rule already ran when the list was built, so nothing here can put
    // back what the scrub removed.
    for variable in &execution.env {
        process.env(&variable.key, &variable.value);
    }

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                command,
                exit_code: NOT_LAUNCHED,
                output: format!("could not run the command: {error}"),
            }
            .masked(&execution.redactor);
        }
    };

    // Both pipes are drained while the command runs rather than after it exits.
    // A pipe nobody reads fills its buffer and blocks the writer, which would
    // turn a chatty command into a hang the bound below then blames on the
    // command.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut said = Vec::new();
    let mut complained = Vec::new();

    let finished = {
        let running = async {
            let ((), ()) = tokio::join!(
                drain(&mut stdout, &mut said),
                drain(&mut stderr, &mut complained),
            );
            child.wait().await
        };

        tokio::time::timeout(execution.bound, running).await
    };

    let mut output = String::from_utf8_lossy(&said).into_owned();
    output.push_str(&String::from_utf8_lossy(&complained));

    let outcome = match finished {
        Ok(Ok(status)) => CommandOutcome {
            command,
            // `None` means a signal killed it, which is a failure with no
            // number of its own.
            exit_code: status.code().unwrap_or(NOT_LAUNCHED),
            output: tail(&output, MAX_CAPTURED_OUTPUT),
        },

        Ok(Err(error)) => CommandOutcome {
            command,
            exit_code: NOT_LAUNCHED,
            output: format!("could not wait on the command: {error}"),
        },

        Err(_elapsed) => {
            tracing::warn!(
                event.name = "command.timed_out",
                // A checker or setup command can carry a credential in its own
                // text, and a satellite's logs leave the satellite too.
                command.text = %execution.redactor.redact(&command),
                command.bound_seconds = execution.bound.as_secs(),
                "killing a command that ran past its {{command.bound_seconds}} second bound: \
                 {{command.text}}",
            );

            // Reaped here rather than left to `kill_on_drop`, so the process is
            // gone by the time the outcome is reported instead of shortly
            // afterwards. `wait` cannot hang: a kill is not something a process
            // can decline.
            drop(child.start_kill());
            drop(child.wait().await);

            CommandOutcome {
                command,
                exit_code: TIMED_OUT,
                output: format!(
                    "the command was killed after {:?} without finishing\n{}",
                    execution.bound,
                    tail(&output, MAX_CAPTURED_OUTPUT),
                ),
            }
        }
    };

    outcome.masked(&execution.redactor)
}

/// Reads one of a child's pipes to the end, into `into`.
///
/// A pipe that errors mid-read is a child that went away, which the exit status
/// already describes. Whatever arrived before it is kept.
async fn drain<R: AsyncRead + Unpin>(pipe: &mut Option<R>, into: &mut Vec<u8>) {
    if let Some(pipe) = pipe.as_mut() {
        drop(pipe.read_to_end(into).await);
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

        let run = run("exit 3\nexit 0", &directory, &Execution::default()).await;

        assert_eq!(run.outcomes.len(), 2, "both commands in a group run");
        assert!(run.failures().count() == 1);
        assert!(run.skipped.is_empty());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_failed_barrier_stops_everything_below_it() {
        // And says so. A command that never ran is skipped, not missing.
        let directory = scratch();

        let run = run("exit 1; exit 0\nexit 0", &directory, &Execution::default()).await;

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

        let run = run("exit 0; exit 0\nexit 0", &directory, &Execution::default()).await;

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

        let run = run("echo hello > proof.txt", &directory, &Execution::default()).await;

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

        let run = run(
            "echo something-went-wrong",
            &directory,
            &Execution::default(),
        )
        .await;

        assert!(run.outcomes[0].output.contains("something-went-wrong"));

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_declared_variable_reaches_the_command() {
        // A repo's install reaches its registry token this way, and so does the
        // lint that runs against the same checkout at the other end of the turn.
        let directory = scratch();
        let declared = Execution {
            env: vec![AgentVar {
                key: "ARSOX_TEST_DECLARED".to_owned(),
                value: "the-declared-value".to_owned(),
                secret: false,
            }],
            ..Execution::default()
        };

        let run = run(ECHO_DECLARED, &directory, &declared).await;

        assert!(
            run.outcomes[0].output.contains("the-declared-value"),
            "got {:?}",
            run.outcomes[0].output
        );

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_command_that_echoes_a_secret_is_masked_before_anything_reads_it() {
        // The whole reason masking happens at capture: a command runs with the
        // thread's credentials in its environment, so an install that prints
        // its own registry token is an ordinary Tuesday. Every place this
        // outcome travels to reads the masked text.
        let directory = scratch();
        let declared = Execution {
            env: vec![AgentVar {
                key: "ARSOX_TEST_DECLARED".to_owned(),
                value: "the-declared-secret".to_owned(),
                secret: true,
            }],
            redactor: crate::redaction::Redactor::for_values(
                vec!["the-declared-secret".to_owned()],
                None,
            ),
            ..Execution::default()
        };

        let run = run(ECHO_DECLARED, &directory, &declared).await;

        assert!(
            !run.outcomes[0].output.contains("the-declared-secret"),
            "got {:?}",
            run.outcomes[0].output
        );
        assert!(run.outcomes[0].output.contains("******"));

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_secret_written_into_the_command_itself_is_masked_too() {
        // A checker spelled `deploy --token ghp_...` puts the credential in the
        // command rather than in its output, and both reach the same report.
        let directory = scratch();
        let declared = Execution {
            redactor: crate::redaction::Redactor::for_values(
                vec!["the-inlined-secret".to_owned()],
                None,
            ),
            ..Execution::default()
        };

        let run = run("exit 0 the-inlined-secret", &directory, &declared).await;

        assert!(
            !run.outcomes[0].command.contains("the-inlined-secret"),
            "got {:?}",
            run.outcomes[0].command
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

    /// A command that does nothing for far longer than any bound a test sets.
    ///
    /// `cmd` has no `sleep`, and its `timeout` builtin refuses to run without a
    /// console, so a ping to loopback is the portable way to spend time.
    const HANGS: &str = if cfg!(windows) {
        "ping -n 10 127.0.0.1 > nul"
    } else {
        "sleep 10"
    };

    /// The same, after printing one line. `&&` chains in both shells, and a
    /// semicolon would be read by this module's own parser as a barrier.
    const PRINTS_THEN_HANGS: &str = if cfg!(windows) {
        "echo before-the-hang && ping -n 10 127.0.0.1 > nul"
    } else {
        "echo before-the-hang && sleep 10"
    };

    /// Runs with a bound short enough that a test does not wait out a hang.
    fn bounded(millis: u64) -> Execution {
        Execution {
            bound: Duration::from_millis(millis),
            ..Execution::default()
        }
    }

    #[tokio::test]
    async fn a_command_past_its_bound_is_killed_and_says_it_timed_out() {
        // The whole point of the bound: an unbounded command holds its thread
        // for the life of the process while looking exactly like work.
        let directory = scratch();
        let started = std::time::Instant::now();

        let run = run(HANGS, &directory, &bounded(200)).await;

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the bound should have ended the wait, not the command finishing"
        );
        assert_eq!(
            run.outcomes[0].exit_code, TIMED_OUT,
            "a timed out command needs its own code: \"it never finished\" and \
             \"it finished badly\" send a reader to different places"
        );
        assert!(
            run.outcomes[0].output.contains("killed"),
            "the outcome should say what happened, got {:?}",
            run.outcomes[0].output
        );
        assert!(!run.outcomes[0].succeeded());

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_timed_out_command_holds_the_barrier_below_it() {
        // A bound that reported a failure nothing acted on would let a hung
        // install be followed by the build that needed it.
        let directory = scratch();

        let run = run(&format!("{HANGS}; exit 0"), &directory, &bounded(200)).await;

        assert_eq!(run.outcomes.len(), 1);
        assert_eq!(run.outcomes[0].exit_code, TIMED_OUT);
        assert_eq!(run.skipped, vec!["exit 0"]);

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_command_killed_at_its_bound_keeps_what_it_printed_first() {
        // The tail of a hung build is the only evidence of what it was doing
        // when it stopped. Discarding it along with the process would leave the
        // agent a timeout and nothing to act on.
        let directory = scratch();

        let run = run(PRINTS_THEN_HANGS, &directory, &bounded(500)).await;

        assert_eq!(run.outcomes[0].exit_code, TIMED_OUT);
        assert!(
            run.outcomes[0].output.contains("before-the-hang"),
            "got {:?}",
            run.outcomes[0].output
        );

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn a_command_that_finishes_inside_its_bound_is_untouched() {
        let directory = scratch();

        let run = run("exit 0", &directory, &bounded(30_000)).await;

        assert_eq!(run.outcomes[0].exit_code, 0);

        drop(std::fs::remove_dir_all(&directory));
    }

    #[test]
    fn the_default_bound_is_the_documented_one() {
        // The README publishes thirty minutes, so a thread that declares
        // nothing has to get thirty minutes.
        assert_eq!(
            Execution::default().bound,
            crate::timeouts::DEFAULT_EXEC_COMMAND
        );
    }

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
