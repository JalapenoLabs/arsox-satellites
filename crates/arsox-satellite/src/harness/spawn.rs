// Copyright © 2026 Jalapeno Labs

//! Building the command line that launches a harness.
//!
//! Kept apart from the runner so what gets spawned can be asserted in a test
//! without spawning anything, and so pointing the satellite at a stand-in
//! harness is a configuration change rather than a code path.

use arsox_sdk::proto::harness::v1::Harness;
use std::path::PathBuf;

/// Overrides the Claude CLI binary.
///
/// Exists so tests and container smoke runs can substitute a stand-in that
/// replays a recorded transcript. The satellite has no way to tell the
/// difference, which is the point: the same code path is exercised either way.
const CLAUDE_BINARY_ENV: &str = "ARSOX_CLAUDE_BIN";

/// What to launch, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
}

/// How a turn attaches to the harness's own session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Open a new session under an id the satellite chooses.
    ///
    /// Choosing it rather than discovering it is what ties one Arsox thread to
    /// exactly one harness session for its whole life.
    Start { session_id: String },

    /// Continue the session a previous turn on this thread opened.
    ///
    /// This is what makes a thread a conversation. Without it every turn would
    /// start from nothing and the second message in a thread would arrive with
    /// no memory of the first.
    Resume { session_id: String },
}

/// Builds the command that runs one turn.
#[must_use]
pub fn command_for(
    harness: Harness,
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
) -> HarnessCommand {
    match harness {
        // Claude is the default and the only harness implemented today, so
        // every arm lands in the same place. Codex gets its own the moment its
        // mapper exists, and this match is where it will appear.
        Harness::Unspecified | Harness::Claude | Harness::Codex => {
            claude_command(prompt, session, working_dir)
        }
    }
}

fn claude_command(prompt: &str, session: &Session, working_dir: PathBuf) -> HarnessCommand {
    let mut args = vec![
        "--print".to_owned(),
        prompt.to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        // stream-json refuses to emit without it.
        "--verbose".to_owned(),
    ];

    match session {
        Session::Start { session_id } => {
            args.push("--session-id".to_owned());
            args.push(session_id.clone());
        }
        Session::Resume { session_id } => {
            args.push("--resume".to_owned());
            args.push(session_id.clone());
        }
    }

    HarnessCommand {
        program: std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_ignored| "claude".to_owned()),
        args,
        working_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_turn_opens_a_session_under_an_id_the_satellite_chose() {
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
        );

        assert!(command.args.contains(&"--session-id".to_owned()));
        assert!(!command.args.contains(&"--resume".to_owned()));
        // Without this the harness emits nothing at all, which looks exactly
        // like a hung process.
        assert!(command.args.contains(&"--verbose".to_owned()));
        assert!(command.args.contains(&"stream-json".to_owned()));
    }

    #[test]
    fn a_later_turn_resumes_rather_than_starting_over() {
        // This is what makes a thread a conversation. Starting fresh each turn
        // would mean the second message arrives with no memory of the first.
        let command = command_for(
            Harness::Claude,
            "and now this",
            &Session::Resume {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
        );

        assert!(command.args.contains(&"--resume".to_owned()));
        assert!(!command.args.contains(&"--session-id".to_owned()));
    }

    #[test]
    fn the_prompt_is_an_argument_rather_than_shell_input() {
        // Never interpolated into a shell string. A prompt is untrusted text and
        // the difference between an argument and a command line is the whole
        // gap a shell injection lives in.
        let command = command_for(
            Harness::Claude,
            "; rm -rf / #",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
        );

        assert!(command.args.contains(&"; rm -rf / #".to_owned()));
    }
}
