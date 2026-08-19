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

/// Resolves the Claude CLI binary this satellite launches.
///
/// One resolution shared by the spawner and the boot-time version probe, so the
/// binary the capabilities endpoint describes is the one a turn actually runs.
#[must_use]
pub fn claude_binary() -> String {
    std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_ignored| "claude".to_owned())
}

/// What to launch, where, and with what environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,

    /// Variables to set on the child, on top of a scrubbed environment.
    ///
    /// Everything an agent is allowed to see is listed here explicitly. The
    /// runner removes every `ARSOX_*` variable the satellite holds before
    /// applying these, so nothing reaches an agent by inheritance.
    pub env: Vec<(String, String)>,
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
    model_access: Option<ModelAccess>,
) -> HarnessCommand {
    match harness {
        // Claude is the default and the only harness implemented today, so
        // every arm lands in the same place. Codex gets its own the moment its
        // mapper exists, and this match is where it will appear.
        Harness::Unspecified | Harness::Claude | Harness::Codex => {
            claude_command(prompt, session, working_dir, model_access)
        }
    }
}

fn claude_command(
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    model_access: Option<ModelAccess>,
) -> HarnessCommand {
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

    let mut env = agent_environment();

    // Pointed at the satellite's own proxy rather than the provider. The token
    // is worth nothing anywhere else and stops working when the turn ends,
    // which is the whole reason the agent gets one instead of a real key.
    if let Some(access) = model_access {
        env.push(("ANTHROPIC_BASE_URL".to_owned(), access.base_url));
        env.push(("ANTHROPIC_API_KEY".to_owned(), access.token));
    }

    HarnessCommand {
        program: claude_binary(),
        args,
        working_dir,
        env,
    }
}

/// One turn's admission to the model, by way of the satellite's proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAccess {
    pub base_url: String,

    /// Identifies the turn. Not a credential: it authorizes nothing beyond
    /// spending this turn's budget through this satellite.
    pub token: String,
}

/// Builds the process to spawn, with an environment an agent may safely hold.
///
/// **No `ARSOX_*` variable reaches an agent, ever.** Written as a rule over the
/// whole prefix rather than a list of names, because a denylist is one
/// forgotten entry away from leaking the next setting somebody adds.
///
/// `ARSOX_SECRET` is the one that matters. An agent holding it could command
/// its own satellite: destroy threads, read another thread's artifacts, or
/// rewrite its own permissions. Inheritance is the default for a spawned
/// process, so withholding it has to be a deliberate act on every spawn, which
/// is why this lives in one function that the runner cannot spawn without.
#[must_use]
pub fn process_for(command: &HarnessCommand) -> tokio::process::Command {
    let mut process = scrubbed_command(&command.program);
    process
        .args(&command.args)
        .current_dir(&command.working_dir);

    for (key, value) in &command.env {
        process.env(key, value);
    }

    process
}

/// A process that inherits none of the satellite's own credentials.
///
/// Shared with workspace provisioning, because `git` and a repo's setup
/// commands are spawned by the satellite exactly as a harness is and inherit
/// exactly the same environment unless something takes it away. One scrub, used
/// by every spawn, is what keeps the rule from holding in one place and lapsing
/// in the next.
#[must_use]
pub fn scrubbed_command(program: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(program);

    for (key, _value) in std::env::vars() {
        if key.starts_with("ARSOX_") || is_provider_credential(&key) {
            process.env_remove(&key);
        }
    }

    process
}

/// Whether a variable holds a credential the proxy should be presenting instead.
///
/// An agent that can read the provider key can spend it outside every ceiling
/// the satellite enforces, print it into a log, or commit it. The proxy holds
/// it and attaches it on the way out, so the agent has no reason to carry one.
///
/// Matched on shape rather than by name, so a variable the next provider
/// introduces is withheld before anybody thinks to list it.
fn is_provider_credential(key: &str) -> bool {
    const VENDORS: [&str; 6] = [
        "ANTHROPIC_",
        "OPENAI_",
        "AWS_",
        "AZURE_",
        "GOOGLE_",
        "DEEPSEEK_",
    ];
    const SECRETS: [&str; 5] = [
        "API_KEY",
        "AUTH_TOKEN",
        "ACCESS_KEY",
        "SECRET",
        "CREDENTIALS",
    ];

    VENDORS.iter().any(|vendor| key.starts_with(vendor))
        && SECRETS.iter().any(|secret| key.contains(secret))
}

/// What an agent is allowed to see, on top of a scrubbed environment.
///
/// Empty in a published image. The stand-in harness needs its transcript path,
/// and that variable is stripped with every other `ARSOX_*` before the child
/// starts, so it has to be handed back deliberately. Compiled out entirely
/// without `test-util`, which is what keeps this from becoming a hole.
fn agent_environment() -> Vec<(String, String)> {
    #[cfg(feature = "test-util")]
    {
        std::env::var("ARSOX_FAKE_TRANSCRIPT")
            .map(|path| vec![("ARSOX_FAKE_TRANSCRIPT".to_owned(), path)])
            .unwrap_or_default()
    }

    #[cfg(not(feature = "test-util"))]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_arsox_variable_survives_into_the_child() {
        // Asserted from inside the spawned process, because that is the only
        // vantage point that can answer what an agent actually sees. Checking
        // the Command's own bookkeeping would assert on intent instead.
        //
        // SAFETY: this test owns these names and no other test reads them.
        unsafe {
            std::env::set_var("ARSOX_SECRET", "the-satellites-own-secret");
            std::env::set_var("ARSOX_SOMETHING_ADDED_LATER", "also-withheld");
        };

        let command = HarnessCommand {
            program: printenv_program(),
            args: printenv_args(),
            working_dir: std::env::temp_dir(),
            env: Vec::new(),
        };

        let output = process_for(&command)
            .output()
            .await
            .expect("should run the probe");
        let seen = String::from_utf8_lossy(&output.stdout);

        assert!(
            !seen.contains("the-satellites-own-secret"),
            "ARSOX_SECRET reached the child: an agent holding it could command its own satellite"
        );
        assert!(
            !seen.contains("also-withheld"),
            "a variable added later leaked, so the rule is a denylist rather than a prefix"
        );

        // The scrub is targeted, not a wholesale clear: a harness still needs a
        // working environment to run in.
        assert!(
            !seen.trim().is_empty(),
            "the child was left with no environment at all"
        );
    }

    #[tokio::test]
    async fn a_provider_credential_never_reaches_the_child() {
        // The proxy presents the key on the way out, so an agent has no reason
        // to carry one. An agent that could read it could spend it outside
        // every ceiling the satellite enforces.
        //
        // SAFETY: this test owns these names and no other test reads them.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-the-real-key");
            std::env::set_var("OPENAI_API_KEY", "sk-openai-real");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "aws-real");
        };

        let command = HarnessCommand {
            program: printenv_program(),
            args: printenv_args(),
            working_dir: std::env::temp_dir(),
            env: Vec::new(),
        };

        let output = process_for(&command)
            .output()
            .await
            .expect("should run the probe");
        let seen = String::from_utf8_lossy(&output.stdout);

        for leaked in ["sk-ant-the-real-key", "sk-openai-real", "aws-real"] {
            assert!(
                !seen.contains(leaked),
                "a provider credential reached the child: {leaked}"
            );
        }
    }

    #[test]
    fn the_credential_rule_matches_shape_rather_than_a_list_of_names() {
        assert!(is_provider_credential("ANTHROPIC_API_KEY"));
        assert!(is_provider_credential("ANTHROPIC_AUTH_TOKEN"));
        assert!(is_provider_credential("AWS_SECRET_ACCESS_KEY"));
        // A vendor variable that is not a credential stays: an agent may well
        // need to know which region or model it is pointed at.
        assert!(!is_provider_credential("AWS_REGION"));
        assert!(!is_provider_credential("ANTHROPIC_MODEL"));
        // And an unrelated variable is untouched.
        assert!(!is_provider_credential("PATH"));
        assert!(!is_provider_credential("HOME"));
    }

    #[tokio::test]
    async fn declared_variables_are_handed_to_the_child_deliberately() {
        let command = HarnessCommand {
            program: printenv_program(),
            args: printenv_args(),
            working_dir: std::env::temp_dir(),
            env: vec![(
                "ARSOX_FAKE_TRANSCRIPT".to_owned(),
                "/fixtures/x.jsonl".to_owned(),
            )],
        };

        let output = process_for(&command)
            .output()
            .await
            .expect("should run the probe");
        let seen = String::from_utf8_lossy(&output.stdout);

        assert!(
            seen.contains("/fixtures/x.jsonl"),
            "an explicitly declared variable should survive the scrub"
        );
    }

    /// A program that prints its environment, whatever platform this is.
    fn printenv_program() -> String {
        if cfg!(windows) {
            "cmd".to_owned()
        } else {
            "env".to_owned()
        }
    }

    fn printenv_args() -> Vec<String> {
        if cfg!(windows) {
            vec!["/C".to_owned(), "set".to_owned()]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn a_first_turn_opens_a_session_under_an_id_the_satellite_chose() {
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            None,
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
            None,
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
            None,
        );

        assert!(command.args.contains(&"; rm -rf / #".to_owned()));
    }
}
