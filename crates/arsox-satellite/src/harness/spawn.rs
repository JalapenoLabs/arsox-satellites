// Copyright © 2026 Jalapeno Labs

//! Building the command line that launches a harness.
//!
//! Kept apart from the runner so what gets spawned can be asserted in a test
//! without spawning anything, and so pointing the satellite at a stand-in
//! harness is a configuration change rather than a code path.
//!
//! # Permissions are advisory here, and only here
//!
//! A harness launched with `--print` cannot answer a permission prompt, so
//! without permission flags every file edit and every shell command it tries is
//! refused and a non-interactive turn cannot do real work. The flags this module
//! builds are what make a turn useful.
//!
//! They are **advisory**, exactly as `AGENTS.md` is. The harness applies them to
//! itself, so they shape what an agent reaches for and never constrain what it
//! can reach: a shell it is granted can run anything the container can run.
//! `Permissions` in the contract documents itself as deterministic controls
//! enforced by infrastructure the agent cannot touch, and none of that
//! infrastructure exists yet. The exec broker, the egress proxy, and the
//! root-owned `pre-push` hook are separate future work, and until they land
//! **the container is the only real boundary**.
//!
//! This is why `--permission-mode` never reached the contract. It is a Claude
//! spelling for an advisory gate, and putting it in a message whose whole
//! premise is determinism would leak one harness into the wire format and
//! promise an enforcement the satellite does not perform. The flags are derived
//! here from what the contract already states instead, and `docs/harness.md`
//! carries the setting-to-flag table.

use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::settings::v1::{ExecAccess, Permissions};
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
///
/// `permissions` are the thread's, absent when it declared none, and decide the
/// posture the harness runs under. See the module docs for what "posture" buys
/// and what it deliberately does not.
#[must_use]
pub fn command_for(
    harness: Harness,
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    model_access: Option<ModelAccess>,
    permissions: Option<&Permissions>,
) -> HarnessCommand {
    match harness {
        // Claude is the default and the only harness implemented today, so
        // every arm lands in the same place. Codex gets its own the moment its
        // mapper exists, and this match is where it will appear.
        //
        // Permission flags are built inside the Claude arm rather than out here
        // for that reason: `--permission-mode` is a Claude spelling, and handing
        // it to `codex exec` would fail the launch rather than restrict it.
        // Codex expresses approvals through its own flags, and mapping the same
        // posture onto them is part of writing that arm.
        Harness::Unspecified | Harness::Claude | Harness::Codex => {
            claude_command(prompt, session, working_dir, model_access, permissions)
        }
    }
}

fn claude_command(
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    model_access: Option<ModelAccess>,
    permissions: Option<&Permissions>,
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

    args.extend(claude_permission_args(&posture_for(permissions)));

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

/// A Claude CLI permission mode, spelled the way the CLI spells it.
///
/// The CLI offers `acceptEdits`, `auto`, `bypassPermissions`, `manual`,
/// `dontAsk`, and `plan`. Only the two the satellite derives are named here: a
/// variant nothing constructs is a spelling nobody checked against the CLI, and
/// a wrong one fails the launch rather than the permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionMode {
    /// File edits are approved without asking. Every other tool still gates,
    /// which under `--print` means it is refused.
    AcceptEdits,

    /// Every tool call is approved without asking.
    BypassPermissions,
}

impl PermissionMode {
    /// The exact value `--permission-mode` accepts.
    const fn as_flag(self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// The advisory posture one turn's harness runs under.
///
/// Held as a value rather than assembled inline so the decision ("what did the
/// thread ask for") is separable from the spelling ("what does this CLI call
/// it"), which is what lets a second harness map the same posture onto its own
/// flags without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Posture {
    mode: PermissionMode,

    /// Tool rules approved without asking, on top of the mode.
    allowed: Vec<String>,

    /// Tool rules refused outright.
    disallowed: Vec<String>,
}

impl Posture {
    /// The posture a thread that declared no exec policy runs under.
    ///
    /// `bypassPermissions`, and the reasoning is worth stating because the flag
    /// reads alarming out of context.
    ///
    /// The alternative is a gate that grants the shell and withholds everything
    /// else. That buys nothing: an agent holding a shell already reaches every
    /// byte and every socket the container reaches, so refusing it `WebFetch` is
    /// a formality it can route around with `curl`. What the narrower posture
    /// does buy is failure. Under `--print` a gate cannot be answered, so the
    /// first tool nobody thought to list is refused mid-turn, and the agent
    /// spends the rest of the turn working around a restriction that was never
    /// intended and protects nothing.
    ///
    /// So the honest default is the one that matches the boundary that actually
    /// exists. Today that boundary is the container, and a satellite is a
    /// container built to be handed to an agent. When the exec broker, the
    /// egress proxy, and the `pre-push` hook land, they enforce underneath this
    /// flag rather than through it: none of them is something `--permission-mode`
    /// can switch off.
    fn unrestricted() -> Self {
        Self {
            mode: PermissionMode::BypassPermissions,
            allowed: Vec::new(),
            disallowed: Vec::new(),
        }
    }

    /// The posture for a thread that named which commands it wants.
    ///
    /// Edits stay approved, because the exec policy is about the shell and a
    /// thread that restricted its commands did not ask to stop editing files.
    /// With no command named, the shell is refused by name rather than left to
    /// be refused by silence, so the harness reports a denial an operator can
    /// read instead of an unexplained tool failure.
    fn named(allowed: Vec<String>) -> Self {
        let disallowed = if allowed.is_empty() {
            vec!["Bash".to_owned()]
        } else {
            Vec::new()
        };

        Self {
            mode: PermissionMode::AcceptEdits,
            allowed,
            disallowed,
        }
    }
}

/// Derives the posture from what the thread declared.
///
/// Only the exec policy reaches the harness. The rest of `Permissions` is
/// deterministic by nature and has no honest advisory equivalent: a domain
/// allowlist belongs to the egress proxy, and a push policy belongs to the
/// `pre-push` hook, since a push can be spelled a dozen ways in argv and a
/// protected ref is frequently not in the argv at all. Expressing either as a
/// tool rule would advertise an enforcement that a rename defeats.
fn posture_for(permissions: Option<&Permissions>) -> Posture {
    let Some(permissions) = permissions else {
        return Posture::unrestricted();
    };

    let exec = ExecAccess::try_from(permissions.exec).unwrap_or(ExecAccess::Unspecified);

    // `allowed_commands` is additive on top of whatever base `exec` sets, which
    // is the contract's own rule. Under the preset the base is already
    // everything, so the additions are covered rather than dropped.
    let allowed = permissions
        .allowed_commands
        .iter()
        .map(|command| command.trim())
        .filter(|command| !command.is_empty())
        .flat_map(bash_rules)
        .collect();

    match exec {
        // Unspecified means "the documented default", and the documented default
        // is the preset. Both land here so leaving the field alone and naming
        // the preset cannot behave differently.
        //
        // The preset is a curated command list the exec broker will hold, and no
        // broker exists yet, so there is no list to hand the harness. Granting
        // the shell overshoots what the preset will eventually mean, and it is
        // the overshoot the contract already documents as pending rather than a
        // new one invented here.
        ExecAccess::Unspecified | ExecAccess::Preset => Posture::unrestricted(),
        ExecAccess::None | ExecAccess::Custom => Posture::named(allowed),
    }
}

/// The tool rules that let one command run, with or without arguments.
///
/// Two rules rather than one, because the CLI matches `Bash(yarn install)`
/// exactly and `Bash(yarn install *)` only with something following it. A thread
/// that named `yarn install` means both, and granting one form would refuse the
/// bare invocation of the very command it allowed.
///
/// The wildcard spelling is the current one. The CLI also accepts a `:*` prefix
/// form and its own validator calls that legacy, so this emits what `claude
/// --help` documents today.
fn bash_rules(command: &str) -> [String; 2] {
    [format!("Bash({command})"), format!("Bash({command} *)")]
}

/// Renders a posture as Claude CLI arguments.
///
/// Each list is one argument rather than several. The CLI accepts a comma or
/// space separated list and the option is variadic, so a rule per argument is a
/// parser question nobody should have to answer while reading a bug report.
fn claude_permission_args(posture: &Posture) -> Vec<String> {
    let mut args = vec![
        "--permission-mode".to_owned(),
        posture.mode.as_flag().to_owned(),
    ];

    if !posture.allowed.is_empty() {
        args.push("--allowedTools".to_owned());
        args.push(posture.allowed.join(","));
    }

    if !posture.disallowed.is_empty() {
        args.push("--disallowedTools".to_owned());
        args.push(posture.disallowed.join(","));
    }

    args
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
            None,
        );

        assert!(command.args.contains(&"; rm -rf / #".to_owned()));
    }

    /// The command a permission test builds, with only the posture varying.
    fn command_with(permissions: Option<&Permissions>) -> HarnessCommand {
        command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            None,
            permissions,
        )
    }

    /// The value the CLI was given for `flag`, when it was given one.
    fn value_of(command: &HarnessCommand, flag: &str) -> Option<String> {
        command
            .args
            .iter()
            .position(|argument| argument == flag)
            .and_then(|index| command.args.get(index + 1))
            .cloned()
    }

    #[test]
    fn a_thread_that_declares_nothing_can_still_edit_files_and_run_commands() {
        // Without this the harness is launched with `--print` and no posture, a
        // permission prompt nothing can answer refuses every edit and every
        // command, and the satellite cannot do the work it exists to do.
        let command = command_with(None);

        assert_eq!(
            value_of(&command, "--permission-mode"),
            Some("bypassPermissions".to_owned()),
            "the default posture has to permit real work in the container"
        );
        assert!(
            !command.args.contains(&"--disallowedTools".to_owned()),
            "a thread that declared nothing had nothing refused on its behalf"
        );
    }

    #[test]
    fn declaring_the_preset_reads_the_same_as_declaring_nothing() {
        // Unspecified means "the documented default" and the documented default
        // is the preset, so the two cannot diverge without the contract's own
        // default rule quietly breaking.
        let preset = Permissions {
            exec: ExecAccess::Preset.into(),
            ..Permissions::default()
        };

        assert_eq!(command_with(Some(&preset)).args, command_with(None).args);
    }

    #[test]
    fn an_explicit_exec_policy_overrides_the_default_posture() {
        let custom = Permissions {
            exec: ExecAccess::Custom.into(),
            allowed_commands: vec!["yarn install".to_owned()],
            ..Permissions::default()
        };

        let command = command_with(Some(&custom));

        assert_eq!(
            value_of(&command, "--permission-mode"),
            Some("acceptEdits".to_owned()),
            "a thread that named its commands did not ask for a blanket bypass"
        );
        assert!(
            !command.args.contains(&"bypassPermissions".to_owned()),
            "the default mode survived a thread that declared its own"
        );
    }

    #[test]
    fn an_allowed_command_is_granted_both_bare_and_with_arguments() {
        let custom = Permissions {
            exec: ExecAccess::Custom.into(),
            // Trimmed and skipped respectively, so a list a human typed does not
            // become `Bash()`, which the CLI rejects outright.
            allowed_commands: vec![" yarn install ".to_owned(), String::new(), "gh".to_owned()],
            ..Permissions::default()
        };

        let command = command_with(Some(&custom));

        assert_eq!(
            value_of(&command, "--allowedTools"),
            Some("Bash(yarn install),Bash(yarn install *),Bash(gh),Bash(gh *)".to_owned()),
            "granting only the wildcard form would refuse the bare command a thread allowed"
        );
        assert!(
            !command.args.contains(&"--disallowedTools".to_owned()),
            "commands outside the list are refused by absence, not by a blanket denial"
        );
    }

    #[test]
    fn a_thread_with_no_shell_keeps_its_ability_to_edit_files() {
        // The exec policy is about the shell. A thread that turned commands off
        // did not ask to stop editing files, and a posture that took both would
        // make the setting far broader than it reads.
        let none = Permissions {
            exec: ExecAccess::None.into(),
            ..Permissions::default()
        };

        let command = command_with(Some(&none));

        assert_eq!(
            value_of(&command, "--permission-mode"),
            Some("acceptEdits".to_owned())
        );
        assert_eq!(
            value_of(&command, "--disallowedTools"),
            Some("Bash".to_owned()),
            "the shell should be refused by name, so the denial is readable"
        );
        assert!(!command.args.contains(&"--allowedTools".to_owned()));
    }

    #[test]
    fn commands_are_additive_on_top_of_whatever_the_exec_policy_set() {
        // The contract says `allowed_commands` adds to the base `exec` chose, so
        // naming commands beside a base of nothing is a grant rather than a
        // contradiction to resolve.
        let none_but_one = Permissions {
            exec: ExecAccess::None.into(),
            allowed_commands: vec!["git status".to_owned()],
            ..Permissions::default()
        };

        let command = command_with(Some(&none_but_one));

        assert_eq!(
            value_of(&command, "--allowedTools"),
            Some("Bash(git status),Bash(git status *)".to_owned())
        );
    }

    #[test]
    fn the_permission_flags_are_a_claude_spelling_rather_than_a_shared_one() {
        // `--permission-mode` is Claude's. Handing it to `codex exec` would fail
        // the launch rather than restrict it, so the flags are appended inside
        // the Claude arm and nowhere above the match. Codex lands in that arm
        // today only because it has no spawn path of its own; writing one means
        // mapping this same posture onto Codex's approval flags.
        let command = command_with(None);
        let flags = claude_permission_args(&posture_for(None));

        assert!(
            command.args.ends_with(&flags),
            "the Claude command should carry exactly the Claude permission flags"
        );
    }

    #[test]
    fn a_deterministic_control_is_never_dressed_up_as_a_tool_rule() {
        // Egress and push policy are enforced by the proxy and the pre-push
        // hook. Rendering either as a tool rule would advertise an enforcement
        // that renaming a command defeats.
        use arsox_sdk::proto::settings::v1::WebAccess;

        let deterministic = Permissions {
            web: WebAccess::None.into(),
            additional_domains: vec!["example.com".to_owned()],
            allow_git_push: Some(false),
            protected_branches: vec!["main".to_owned()],
            ..Permissions::default()
        };

        let command = command_with(Some(&deterministic));

        assert_eq!(command.args, command_with(None).args);
    }
}
