// Copyright © 2026 Jalapeno Labs

//! What a checker is, where it runs, and what a failing one says to the agent.
//!
//! A checker is a repo's own definition of "done": `yarn lint`, `cargo test`,
//! whatever proves the work. It runs after the agents decide they have finished,
//! with the working directory at that repo's checkout, under the same barrier
//! semantics setup commands use. See [`crate::commands`] for the separator rules
//! both ends of a turn share.
//!
//! This is the verification that turns "the agent said it was done" into
//! something checked. Without it a turn reports success on the agent's word.
//!
//! # This module decides nothing
//!
//! It resolves which checkers exist, runs them, and writes the prompt a failure
//! hands back to the agent. Whether to wake the agent at all, how many times, and
//! what the turn's result says about it are the runner's, because those are
//! decisions about a turn rather than facts about a checker.

use crate::commands::{self, NOT_LAUNCHED};
use crate::harness::spawn::AgentVar;
use arsox_sdk::proto::settings::v1::Repo;
use arsox_sdk::proto::turn::v1::CheckerResult;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// How many times a failing checker puts the agent back to work.
///
/// A checker that failed, was fixed, and failed again is telling you the fix did
/// not work, and a third attempt against the same evidence usually reaches the
/// same place at the cost of another agent session.
///
/// The cap is what a flake cannot outlast. A test that fails at random will keep
/// producing new evidence forever, and an uncapped loop would spend a thread's
/// whole budget answering it. So this is a constant rather than a setting: the
/// number that matters is that one exists.
pub(super) const MAX_FIX_ATTEMPTS: u32 = 2;

/// Output per failing command carried into the fix prompt.
///
/// Smaller than the capture cap because several failing commands share one
/// prompt, and a prompt is context an agent pays for. The tail is where the
/// error is, so a shorter tail drops the preamble rather than the finding.
const FIX_PROMPT_OUTPUT_TAIL: usize = 4 * 1024;

/// One repo's checker, resolved to the checkout it runs in.
#[derive(Debug, Clone)]
pub(super) struct Declared {
    pub repo: String,
    pub checkout: PathBuf,
    pub commands: String,
}

/// One checker command's outcome, kept with the repo that declared it.
///
/// The repo travels alongside rather than inside, because `CheckerResult` is the
/// contract's shape and carries only the command. The stream event pairs the two
/// so a consumer watching a multi-repo turn can tell whose lint failed.
#[derive(Debug, Clone)]
pub(super) struct Outcome {
    pub repo: String,
    pub result: CheckerResult,
}

impl Outcome {
    pub(super) fn passed(&self) -> bool {
        self.result.exit_code == 0
    }
}

/// The checkers a thread's repos declare, in the order the repos were given.
///
/// A repo with a blank `checker` has nothing to run and is left out entirely,
/// which is what makes "a thread with no checkers behaves exactly as before" a
/// property of the data rather than a branch somewhere downstream.
///
/// A repo whose name could escape `repos/` is dropped. The API refuses those at
/// thread creation, where the caller is still listening, so this can only fire
/// for settings that predate that check.
pub(super) fn declared(repos: &[Repo], repos_root: &Path) -> Vec<Declared> {
    repos
        .iter()
        .filter(|repo| !repo.checker.trim().is_empty())
        .filter_map(|repo| {
            let name = crate::workspace::directory_name(repo).ok()?;

            Some(Declared {
                checkout: repos_root.join(&name),
                repo: name,
                commands: repo.checker.clone(),
            })
        })
        .collect()
}

/// Runs every declared checker once, in the order the repos were given.
///
/// One repo at a time. A checker is frequently a build, and running four of them
/// at once on a satellite already bounded to four concurrent threads trades a
/// little wall clock for a lot of contention. Within one repo the barrier
/// semantics still run a group concurrently, which is where the parallelism the
/// contract promises actually lives.
///
/// A checkout that is missing is not special-cased: a repo that will not clone
/// parks its thread before any turn runs, so the case is unreachable, and if it
/// ever happens the command runner reports it as a command that could not start
/// rather than as silence.
pub(super) async fn run_all(declared: &[Declared], env: &[AgentVar]) -> Vec<Outcome> {
    let mut outcomes = Vec::new();

    for checker in declared {
        let run = commands::run(&checker.commands, &checker.checkout, env).await;

        outcomes.extend(run.outcomes.iter().map(|outcome| Outcome {
            repo: checker.repo.clone(),
            result: CheckerResult {
                command: outcome.command.clone(),
                exit_code: outcome.exit_code,
                output: outcome.output.clone(),
                // The commander has no way to say so yet. The MCP tool that
                // lets it accept a failure is separate work, and claiming a
                // skip nobody asked for would misreport why a check is red.
                skipped_by_commander: false,
            },
        }));

        outcomes.extend(run.skipped.iter().map(|command| Outcome {
            repo: checker.repo.clone(),
            result: CheckerResult {
                command: command.clone(),
                // Distinct from any code a shell reports, so "never ran" never
                // reads as "ran and failed".
                exit_code: NOT_LAUNCHED,
                output: "never ran: a barrier above it failed".to_owned(),
                skipped_by_commander: false,
            },
        }));
    }

    outcomes
}

/// The prompt that wakes the agent back up for a failing checker.
///
/// Evidence rather than a conclusion: the repo, the command, its exit code, and
/// the tail of what it printed. The agent decides what that means, which is the
/// whole reason it is woken rather than the satellite retrying something.
///
/// Both acceptable answers are named. A checker can be failing for a reason the
/// agents deliberately accept, and an agent told only to fix it will keep trying
/// to fix something that is not broken until its budget runs out.
pub(super) fn fix_prompt(failures: &[&Outcome]) -> String {
    let mut prompt = String::from(
        "The checkers for this turn did not pass. Each one below ran with the \
         working directory at its repo's checkout.\n\n",
    );

    for failure in failures {
        // Written into the buffer rather than formatted and pushed, which would
        // allocate a second string per failure only to copy it in. Writing into
        // a `String` is infallible, so the trait's `Result` has nothing to say.
        let written = write!(
            prompt,
            "Repo `{}`, command `{}`, exited with {}:\n\n```\n{}\n```\n\n",
            failure.repo,
            failure.result.command,
            failure.result.exit_code,
            commands::tail(failure.result.output.trim_end(), FIX_PROMPT_OUTPUT_TAIL),
        );
        debug_assert!(written.is_ok(), "writing into a String cannot fail");
    }

    prompt.push_str(
        "Do one of two things, and say plainly which. Change the code so the \
         command passes, or explain why this failure should be accepted and \
         left alone. Do not change the checker command itself, and do not \
         disable the check to make it pass.",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, checker: &str) -> Repo {
        Repo {
            name: name.to_owned(),
            url: format!("https://example.com/{name}.git"),
            checker: checker.to_owned(),
            ..Repo::default()
        }
    }

    fn failed(repo: &str, command: &str, exit_code: i32, output: &str) -> Outcome {
        Outcome {
            repo: repo.to_owned(),
            result: CheckerResult {
                command: command.to_owned(),
                exit_code,
                output: output.to_owned(),
                skipped_by_commander: false,
            },
        }
    }

    #[test]
    fn a_repo_with_no_checker_declares_nothing() {
        // The whole "a thread with no checkers costs exactly what it did before"
        // guarantee rests here: an empty string is absence, not a command.
        let resolved = declared(
            &[repo("api", ""), repo("web", "   \n  ")],
            Path::new("/workspace/thread/repos"),
        );

        assert!(resolved.is_empty());
    }

    #[test]
    fn a_checker_runs_in_its_own_repos_checkout() {
        // `yarn lint` means nothing anywhere else, and two repos must not share
        // a working directory.
        let resolved = declared(
            &[repo("api", "yarn lint"), repo("web", "yarn test")],
            Path::new("/workspace/thread/repos"),
        );

        assert_eq!(resolved.len(), 2);
        assert!(
            resolved[0].checkout.ends_with("repos/api") || resolved[0].checkout.ends_with("api")
        );
        assert_eq!(resolved[0].repo, "api");
        assert_eq!(resolved[1].repo, "web");
        assert_eq!(resolved[1].commands, "yarn test");
    }

    #[test]
    fn a_repo_name_that_could_escape_the_workspace_declares_nothing() {
        let resolved = declared(
            &[repo("../../etc", "yarn lint")],
            Path::new("/workspace/thread/repos"),
        );

        assert!(resolved.is_empty(), "a traversal must not reach a shell");
    }

    #[test]
    fn the_fix_prompt_carries_the_evidence_and_both_acceptable_answers() {
        let prompt = fix_prompt(&[
            &failed("api", "yarn lint", 1, "src/x.ts:3 unused variable"),
            &failed("web", "yarn test", 2, "2 tests failed"),
        ]);

        // Every failing command, with the repo it belongs to. A prompt naming
        // one of two failures sends the agent back for a second round it could
        // have avoided.
        assert!(prompt.contains("Repo `api`, command `yarn lint`, exited with 1"));
        assert!(prompt.contains("Repo `web`, command `yarn test`, exited with 2"));
        assert!(prompt.contains("unused variable"));
        assert!(prompt.contains("2 tests failed"));

        // Accepting a failure has to be an available answer, or an agent will
        // keep fixing something that is not broken.
        assert!(prompt.contains("accepted"));
        assert!(prompt.contains("Do not change the checker command itself"));
    }

    #[test]
    fn a_runaway_checker_log_reaches_the_prompt_as_its_tail() {
        // The error is at the end of a build log and the front is a package
        // list. A prompt that carried the front would spend context proving it.
        let flood = "noise\n".repeat(FIX_PROMPT_OUTPUT_TAIL);
        let outcome = failed("api", "yarn build", 1, &format!("{flood}the actual error"));

        let prompt = fix_prompt(&[&outcome]);

        assert!(prompt.contains("the actual error"));
        assert!(
            prompt.len() < FIX_PROMPT_OUTPUT_TAIL * 2,
            "the whole log reached the prompt, {} bytes",
            prompt.len()
        );
    }
}
