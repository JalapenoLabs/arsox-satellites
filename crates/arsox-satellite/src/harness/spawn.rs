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
//!
//! # The environment is built, and only ever added to
//!
//! Every spawn starts from [`scrubbed_command`], which takes away every
//! `ARSOX_*` variable and every provider credential the satellite holds.
//! Everything an agent is meant to have is then listed back explicitly, so a
//! variable reaches an agent because somebody said so and never by inheritance.
//!
//! A thread's declared variables join that list, and they may only add to it:
//! [`declared_key_refusal`] refuses any key that would put back what the scrub
//! removed, and the proxy's own variables are applied last so a declared key
//! cannot repoint an agent away from the satellite's LLM proxy.

use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::settings::v1::{EnvVar, ExecAccess, Permissions};
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

/// The mask a credential renders as, six stars as the contract's own default.
const REDACTED: &str = "******";

/// One variable set on a child, and whether its value is a credential.
///
/// The pair carries its secrecy rather than being a bare `(String, String)` so
/// that nothing downstream has to remember which of these it is holding. A
/// credential that reaches a log is a credential, whether it came from the
/// caller's [`EnvVar`] list or was minted by the satellite a line earlier.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentVar {
    pub key: String,

    pub value: String,

    /// Whether the value must never be rendered.
    ///
    /// True for everything the satellite mints, and for a declared variable
    /// that did not say otherwise: `EnvVar.is_secret` is absent by default and
    /// absent means secret.
    pub secret: bool,
}

/// Renders a variable without its value when the value is a credential.
///
/// Manual rather than derived, because [`HarnessCommand`] derives `Debug` and a
/// derived one here would put the turn's proxy token and every declared
/// credential into any log line that ever formats a command. Per M-PUBLIC-DEBUG.
impl std::fmt::Debug for AgentVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = if self.secret {
            REDACTED
        } else {
            self.value.as_str()
        };

        f.debug_struct("AgentVar")
            .field("key", &self.key)
            .field("value", &value)
            .finish()
    }
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
    pub env: Vec<AgentVar>,
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
/// `declared` is the thread's [`EnvVar`] list, which reaches the agent on top of
/// the scrub. `permissions` are the thread's, absent when it declared none, and
/// decide the posture the harness runs under. See the module docs for what
/// "posture" buys and what it deliberately does not.
#[must_use]
pub fn command_for(
    harness: Harness,
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    model_access: Option<ModelAccess>,
    declared: &[EnvVar],
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
        Harness::Unspecified | Harness::Claude | Harness::Codex => claude_command(
            prompt,
            session,
            working_dir,
            model_access,
            declared,
            permissions,
        ),
    }
}

fn claude_command(
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    model_access: Option<ModelAccess>,
    declared: &[EnvVar],
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
    env.extend(declared_environment(declared));

    // Pointed at the satellite's own proxy rather than the provider. The token
    // is worth nothing anywhere else and stops working when the turn ends,
    // which is the whole reason the agent gets one instead of a real key.
    //
    // Applied after the declared variables rather than before them, because the
    // last value set for a key is the one the child sees. A thread cannot
    // repoint its agent away from the proxy by declaring `ANTHROPIC_BASE_URL`,
    // and the budget ceilings stay on the only route to a model.
    if let Some(access) = model_access {
        env.push(AgentVar {
            key: "ANTHROPIC_BASE_URL".to_owned(),
            value: access.base_url,
            // An address, not a credential, and worth reading in a log when a
            // turn cannot reach its proxy.
            secret: false,
        });
        env.push(AgentVar {
            key: "ANTHROPIC_API_KEY".to_owned(),
            value: access.token,
            secret: true,
        });
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

    for variable in &command.env {
        process.env(&variable.key, &variable.value);
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

/// The variables a thread declared, ready to be set on a child.
///
/// Declared variables are how a repo's `yarn install` reaches its registry token
/// and how an agent reaches whatever else the operator runs, so the same list
/// goes to the harness and to setup commands: both run in the thread's workspace
/// against the same scrubbed base environment.
///
/// A value the caller left absent becomes the empty string. The caller named the
/// key deliberately, and "set it to nothing" is a far more likely reading than
/// "do not set it", which is spelled by omitting the entry.
///
/// # Refused keys
///
/// A key that would reintroduce what [`scrubbed_command`] removes is dropped
/// rather than applied. The API refuses these at thread creation, where the
/// caller is still listening and can be told which key was wrong, so this is the
/// second gate rather than the first: it can only fire for a thread whose
/// settings predate that check. It warns with the key and never the value.
#[must_use]
pub fn declared_environment(declared: &[EnvVar]) -> Vec<AgentVar> {
    declared
        .iter()
        .filter_map(|variable| {
            if let Some(refusal) = declared_key_refusal(&variable.key) {
                tracing::warn!(
                    event.name = "harness.env.refused",
                    env.key = variable.key,
                    env.refusal = refusal,
                    "{{env.key}} was withheld from the agent: it {{env.refusal}}",
                );
                return None;
            }

            Some(AgentVar {
                key: variable.key.clone(),
                value: variable
                    .value
                    .as_ref()
                    .and_then(|secret| secret.value.clone())
                    .unwrap_or_default(),
                secret: is_secret(variable),
            })
        })
        .collect()
}

/// Whether a declared variable's value is a credential.
///
/// `is_secret` is `optional bool` on the wire precisely so that absent and
/// `false` stay distinguishable, and **absent means secret**. Defaulting to
/// secret fails safe: the cost of needlessly redacting a public value is a
/// confusing log line, and the cost of the reverse is a leaked credential.
#[must_use]
pub fn is_secret(declared: &EnvVar) -> bool {
    declared.is_secret.unwrap_or(true)
}

/// Why a declared variable may not be set on an agent, when it may not.
///
/// The reason completes the sentence "`KEY` ...", so a caller can name the key
/// alongside it without restating the rule. `None` means the key is fine.
///
/// **A declared variable must never reintroduce what the scrub removes.** It is
/// applied on top of the scrubbed environment, so a thread declaring
/// `ANTHROPIC_API_KEY` would hand an agent the very credential the proxy exists
/// to keep from it, and a thread declaring `ARSOX_SECRET` would hand it command
/// of its own satellite. The declared list is a way to give an agent things, not
/// a way around a boundary.
#[must_use]
pub fn declared_key_refusal(key: &str) -> Option<&'static str> {
    // A name a process cannot carry. The child would refuse the whole
    // environment rather than the one entry, so a turn would fail to launch for
    // a reason nothing in the failure would explain.
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Some("is not a usable environment variable name");
    }

    if key.starts_with("ARSOX_") {
        return Some(
            "is reserved for the satellite: no ARSOX_ variable is ever placed in an agent's \
             environment",
        );
    }

    if is_provider_credential(key) {
        return Some(
            "is shaped like a provider credential, which the satellite's LLM proxy presents on \
             the agent's behalf",
        );
    }

    None
}

/// What an agent is allowed to see, on top of a scrubbed environment.
///
/// Empty in a published image. The stand-in harness needs its transcript path,
/// and that variable is stripped with every other `ARSOX_*` before the child
/// starts, so it has to be handed back deliberately. Compiled out entirely
/// without `test-util`, which is what keeps this from becoming a hole.
fn agent_environment() -> Vec<AgentVar> {
    #[cfg(feature = "test-util")]
    {
        std::env::var("ARSOX_FAKE_TRANSCRIPT")
            .map(|path| {
                vec![AgentVar {
                    key: "ARSOX_FAKE_TRANSCRIPT".to_owned(),
                    value: path,
                    // A fixture path. Masking it would hide the one fact worth
                    // reading when a stand-in harness replays the wrong file.
                    secret: false,
                }]
            })
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
            env: vec![AgentVar {
                key: "ARSOX_FAKE_TRANSCRIPT".to_owned(),
                value: "/fixtures/x.jsonl".to_owned(),
                secret: false,
            }],
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

    /// A variable a thread declared, as the SDK would send one.
    fn declared(key: &str, value: &str, is_secret: Option<bool>) -> EnvVar {
        EnvVar {
            key: key.to_owned(),
            value: Some(arsox_sdk::proto::common::v1::Secret {
                value: Some(value.to_owned()),
                display: None,
            }),
            is_secret,
        }
    }

    #[tokio::test]
    async fn a_declared_variable_reaches_the_child() {
        // The whole point of the setting: an agent's `yarn install` needs the
        // registry token the operator declared, and nothing else will give it
        // one.
        let command = HarnessCommand {
            program: printenv_program(),
            args: printenv_args(),
            working_dir: std::env::temp_dir(),
            env: declared_environment(&[declared("NPM_TOKEN", "npm-declared-value", None)]),
        };

        let output = process_for(&command)
            .output()
            .await
            .expect("should run the probe");
        let seen = String::from_utf8_lossy(&output.stdout);

        assert!(
            seen.contains("npm-declared-value"),
            "a declared variable should survive the scrub"
        );
    }

    #[test]
    fn a_variable_that_did_not_say_is_secret() {
        // `is_secret` is `optional bool` on the wire so that absent and `false`
        // stay distinguishable, and absent means secret. Reading absence as
        // `false` would turn "the caller said nothing" into "the caller said
        // this is public", which is a leaked credential rather than a wrong
        // default.
        let applied = declared_environment(&[
            declared("SAID_NOTHING", "a", None),
            declared("SAID_PUBLIC", "b", Some(false)),
            declared("SAID_SECRET", "c", Some(true)),
        ]);

        assert!(applied[0].secret, "absent means secret");
        assert!(!applied[1].secret);
        assert!(applied[2].secret);
    }

    #[test]
    fn a_variable_declared_without_a_value_is_set_to_an_empty_one() {
        // The caller named the key deliberately. "Do not set it" is spelled by
        // leaving the entry out.
        let applied = declared_environment(&[EnvVar {
            key: "EMPTY".to_owned(),
            value: None,
            is_secret: None,
        }]);

        assert_eq!(applied.len(), 1);
        assert!(applied[0].value.is_empty());
    }

    #[test]
    fn a_secret_value_is_never_rendered_where_a_command_is() {
        // A derived `Debug` would put the turn's proxy token and every declared
        // credential into any log line that ever formats a command.
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            Some(ModelAccess {
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                token: "the-turns-proxy-token".to_owned(),
            }),
            &[
                declared("NPM_TOKEN", "npm-the-real-token", None),
                declared("DEPLOY_ENV", "staging", Some(false)),
            ],
            None,
        );

        let rendered = format!("{command:?}");

        assert!(!rendered.contains("npm-the-real-token"), "{rendered}");
        assert!(!rendered.contains("the-turns-proxy-token"), "{rendered}");
        // The key survives, because a masked value with no name is unreadable
        // and telling two credentials apart is what a log is for.
        assert!(rendered.contains("NPM_TOKEN"));
        // And a value the caller marked public reads plainly, which is the
        // whole difference the flag buys.
        assert!(rendered.contains("staging"));
    }

    #[test]
    fn a_key_that_would_undo_the_scrub_is_refused() {
        // A declared variable is applied on top of the scrub, so these would
        // hand back exactly what the scrub exists to withhold.
        for reintroduced in [
            "ARSOX_SECRET",
            "ARSOX_SOMETHING_ADDED_LATER",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                declared_key_refusal(reintroduced).is_some(),
                "{reintroduced} should be refused"
            );
        }

        // A name a process cannot carry, which would fail the launch rather
        // than the entry.
        assert!(declared_key_refusal("").is_some());
        assert!(declared_key_refusal("HAS=EQUALS").is_some());

        // And the ordinary case is untouched, including a vendor variable that
        // is not a credential.
        for allowed in ["NPM_TOKEN", "AWS_REGION", "DATABASE_URL", "CI"] {
            assert_eq!(declared_key_refusal(allowed), None, "{allowed}");
        }
    }

    #[tokio::test]
    async fn a_refused_key_never_reaches_the_child_even_if_it_was_stored() {
        // The API refuses these at thread creation. This is the second gate,
        // for settings written before that check existed: a thread carrying one
        // must not quietly start handing it to agents.
        let command = HarnessCommand {
            program: printenv_program(),
            args: printenv_args(),
            working_dir: std::env::temp_dir(),
            // Values nothing else in this process sets, since a sibling test
            // putting the same string in the real environment would prove
            // nothing about this one.
            env: declared_environment(&[
                declared("ARSOX_SECRET", "a-declared-arsox-value", None),
                declared("ANTHROPIC_API_KEY", "a-declared-provider-value", None),
            ]),
        };

        assert!(command.env.is_empty(), "{:?}", command.env);

        let output = process_for(&command)
            .output()
            .await
            .expect("should run the probe");
        let seen = String::from_utf8_lossy(&output.stdout);

        assert!(!seen.contains("a-declared-arsox-value"));
        assert!(!seen.contains("a-declared-provider-value"));
    }

    #[test]
    fn the_proxy_keeps_the_last_word_on_where_a_model_request_goes() {
        // Every model request traverses the satellite's proxy, which is what
        // makes the budget ceilings arithmetic rather than a request. A thread
        // that could repoint its agent elsewhere would spend past every one of
        // them.
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            Some(ModelAccess {
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                token: "the-turns-proxy-token".to_owned(),
            }),
            &[declared(
                "ANTHROPIC_BASE_URL",
                "https://elsewhere.invalid",
                None,
            )],
            None,
        );

        // The last value set for a key is the one the child sees.
        let applied = command
            .env
            .iter()
            .rfind(|variable| variable.key == "ANTHROPIC_BASE_URL")
            .expect("the proxy address should be set");

        assert_eq!(applied.value, "http://127.0.0.1:9/v1");
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
