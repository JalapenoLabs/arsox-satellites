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
//! can reach. What constrains it is [`crate::broker`], which stands underneath
//! these flags rather than through them: a thread that declared `exec: NONE` or
//! `exec: CUSTOM` runs with a root-owned shim directory as its whole `PATH`, and
//! no flag here can switch that off. The same is true of the root-owned
//! `pre-push` hook and of [`crate::egress`], neither of which any flag reaches.
//!
//! This is why `--permission-mode` never reached the contract. It is a Claude
//! spelling for an advisory gate, and putting it in a message whose whole
//! premise is determinism would leak one harness into the wire format and
//! promise an enforcement the satellite does not perform. The flags are derived
//! here from what the contract already states instead, and `docs/harness.md`
//! carries the setting-to-flag table.
//!
//! [`Posture`] is what keeps that derivation single. It holds the decision, and
//! each harness arm renders it in its own vocabulary: Claude reads
//! `--permission-mode` and tool rules, Codex reads a sandbox width and an
//! approval policy. The two are not equally expressive, and where Codex cannot
//! carry something the answer is to say so in the docs rather than to emit a
//! flag that would not hold it.
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
//! removed, and the satellite's own variables are applied last so a declared key
//! cannot repoint an agent away from the LLM proxy or out from behind the egress
//! proxy.

use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::settings::v1::{EnvVar, ExecAccess, Permissions};
use std::path::PathBuf;

/// Overrides the Claude CLI binary.
///
/// Exists so tests and container smoke runs can substitute a stand-in that
/// replays a recorded transcript. The satellite has no way to tell the
/// difference, which is the point: the same code path is exercised either way.
const CLAUDE_BINARY_ENV: &str = "ARSOX_CLAUDE_BIN";

/// Overrides the Codex CLI binary, mirroring [`CLAUDE_BINARY_ENV`].
const CODEX_BINARY_ENV: &str = "ARSOX_CODEX_BIN";

/// Resolves the Claude CLI binary this satellite launches.
///
/// One resolution shared by the spawner and the boot-time version probe, so the
/// binary the capabilities endpoint describes is the one a turn actually runs.
#[must_use]
pub fn claude_binary() -> String {
    std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_ignored| "claude".to_owned())
}

/// Resolves the Codex CLI binary this satellite launches.
#[must_use]
pub fn codex_binary() -> String {
    std::env::var(CODEX_BINARY_ENV).unwrap_or_else(|_ignored| "codex".to_owned())
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
///
/// The two harnesses disagree about who names a session, and the disagreement
/// is absorbed here rather than in the runner. Claude accepts an id and opens a
/// session under it; Codex mints its own and announces it on `thread.started`,
/// so the Codex arm ignores the id in [`Session::Start`] and the mapper records
/// what the CLI chose. Either way the thread's second turn resumes the session
/// its first turn opened, which is the only property above this layer cares
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Open a new session under an id the satellite chooses.
    ///
    /// Choosing it rather than discovering it is what ties one Arsox thread to
    /// exactly one harness session for its whole life, where the harness lets
    /// the caller choose at all. Codex does not, and ignores this id.
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
///
/// `grants` carries what the satellite lends the turn: its admission to the
/// model, and the shim directory that becomes its `PATH` when the exec broker
/// engaged for the thread.
#[must_use]
pub fn command_for(
    harness: Harness,
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    grants: &Grants,
    declared: &[EnvVar],
    permissions: Option<&Permissions>,
) -> HarnessCommand {
    // Flags are rendered inside each arm rather than above the match, because
    // every one of them is a spelling rather than a decision. `--permission-mode`
    // is Claude's and would fail a `codex exec` launch rather than restrict it,
    // and `-c sandbox_mode=` is Codex's and means nothing to Claude. What the
    // thread asked for is decided once, in [`posture_for`], and each arm renders
    // it in its own vocabulary.
    match harness {
        // Unspecified means "the documented default", and the documented default
        // is Claude.
        Harness::Unspecified | Harness::Claude => {
            claude_command(prompt, session, working_dir, grants, declared, permissions)
        }
        Harness::Codex => {
            codex_command(prompt, session, working_dir, grants, declared, permissions)
        }
    }
}

fn claude_command(
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    grants: &Grants,
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

    let proxy = grants.model.clone().map(|access| {
        vec![
            AgentVar {
                key: "ANTHROPIC_BASE_URL".to_owned(),
                value: access.base_url,
                // An address, not a credential, and worth reading in a log when
                // a turn cannot reach its proxy.
                secret: false,
            },
            AgentVar {
                key: "ANTHROPIC_API_KEY".to_owned(),
                value: access.token,
                secret: true,
            },
        ]
    });

    HarnessCommand {
        program: claude_binary(),
        args,
        working_dir,
        env: environment_for(declared, proxy.unwrap_or_default(), grants),
    }
}

/// The Codex config profile the satellite declares for its own proxy.
///
/// Named rather than reusing `openai`, so an operator's own `config.toml` entry
/// for the provider they normally use is left intact and the turn still runs
/// through the satellite: `model_provider` is overridden per invocation, and a
/// `-c` override outranks anything on disk.
const CODEX_PROVIDER: &str = "arsox";

/// Builds the `codex exec` command for one turn.
///
/// # Why the session handling is inverted
///
/// Claude takes `--session-id` and opens a session under it. Codex mints its own
/// and reports it on `thread.started`, so [`Session::Start`] carries an id this
/// arm has nowhere to put and deliberately drops. The mapper records what the
/// CLI announced, and the next turn arrives here as [`Session::Resume`] holding
/// that id, which is what `codex exec resume` takes.
///
/// # Why every flag below is load-bearing
///
/// - `--json`. Without it `codex exec` writes a human report, and the mapper
///   pointed at that parses prose as protocol.
/// - `--skip-git-repo-check`. A thread's workspace is a directory the satellite
///   created, and it is frequently not a git repository. Without this the CLI
///   refuses to start and writes one line of plain English to stdout, which the
///   runner correctly reports as a harness that exited without saying what it
///   did. Measured against 0.147.0.
/// - `--`. The prompt is untrusted text and it is a positional argument here
///   rather than the value of a flag, so a prompt beginning with a dash would
///   otherwise be parsed as one.
///
/// The working directory is the process's rather than `-C`, because
/// `codex exec resume` accepts no `-C` and the two forms must not diverge in
/// where they run.
fn codex_command(
    prompt: &str,
    session: &Session,
    working_dir: PathBuf,
    grants: &Grants,
    declared: &[EnvVar],
    permissions: Option<&Permissions>,
) -> HarnessCommand {
    let mut args = vec!["exec".to_owned()];

    if matches!(session, Session::Resume { .. }) {
        args.push("resume".to_owned());
    }

    args.push("--json".to_owned());
    args.push("--skip-git-repo-check".to_owned());
    args.extend(codex_permission_args(&posture_for(permissions)));

    // Codex 0.147.0 does not read `OPENAI_BASE_URL`, so the address reaches it
    // as a declared provider rather than as an environment variable. The
    // variable is set anyway, and set last: a value the satellite inherited or a
    // thread declared would otherwise be the one a CLI that does read it obeys,
    // which is a route around every ceiling. The key travels in the environment
    // either way, which is what `env_key` above names.
    let proxy = match grants.model.clone() {
        Some(access) => {
            args.extend(codex_provider_args(&access));

            vec![
                AgentVar {
                    key: "OPENAI_BASE_URL".to_owned(),
                    value: proxy_v1(&access.base_url),
                    secret: false,
                },
                AgentVar {
                    key: "OPENAI_API_KEY".to_owned(),
                    value: access.token,
                    secret: true,
                },
            ]
        }
        None => Vec::new(),
    };

    // Positionals last, behind the separator. `resume` takes the session id
    // first and the prompt second.
    args.push("--".to_owned());
    if let Session::Resume { session_id } = session {
        args.push(session_id.clone());
    }
    args.push(prompt.to_owned());

    HarnessCommand {
        program: codex_binary(),
        args,
        working_dir,
        env: environment_for(declared, proxy, grants),
    }
}

/// Points Codex at the satellite's proxy through a declared model provider.
///
/// `wire_api = "responses"` is what makes this an ordinary HTTPS POST to
/// `{base_url}/responses`. Left to its built-in provider the CLI opens a
/// WebSocket to the provider instead, which the proxy does not speak and which
/// would take every model request straight past the ceilings. Measured against
/// 0.147.0 rather than read from a schema.
fn codex_provider_args(access: &ModelAccess) -> Vec<String> {
    // Each value is passed unquoted. The CLI parses it as TOML and falls back to
    // the literal string when that fails, which is what a bare URL or a hyphen
    // separated word takes.
    [
        format!("model_provider={CODEX_PROVIDER}"),
        format!("model_providers.{CODEX_PROVIDER}.name=Arsox"),
        format!(
            "model_providers.{CODEX_PROVIDER}.base_url={}",
            proxy_v1(&access.base_url)
        ),
        format!("model_providers.{CODEX_PROVIDER}.env_key=OPENAI_API_KEY"),
        format!("model_providers.{CODEX_PROVIDER}.wire_api=responses"),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_owned(), setting])
    .collect()
}

/// The versioned prefix a Codex provider base URL carries.
///
/// The grant address is an origin, and Claude appends `/v1` itself when it posts
/// `/v1/messages`. Codex appends only `/responses`, so the `/v1` is added here
/// and both harnesses reach the proxy on the same shape of path. Without it the
/// proxy would forward to the upstream's root and every request would 404.
fn proxy_v1(base_url: &str) -> String {
    format!("{}/v1", base_url.trim_end_matches('/'))
}

/// Assembles the environment a harness child runs with.
///
/// Five layers, in the one order that is safe: what the satellite hands every
/// agent, then what the thread declared, then the LLM proxy's variables, then
/// the egress proxy's, then the broker's `PATH`. Everything the satellite
/// decides goes after everything the thread declared, because the last value set
/// for a key is the one the child sees. A thread that could set
/// `ANTHROPIC_BASE_URL` would route its agent out from under every budget
/// ceiling, a thread that could set `PATH` would step around the exec broker by
/// declaring a variable, and a thread that could set `HTTP_PROXY` would step out
/// from behind the egress allowlist the same way.
fn environment_for(declared: &[EnvVar], proxy: Vec<AgentVar>, grants: &Grants) -> Vec<AgentVar> {
    let mut env = agent_environment();
    env.extend(declared_environment(declared));
    env.extend(proxy);
    env.extend(egress_environment(grants));

    if let Some(shims) = grants.exec_broker.as_deref() {
        env.push(AgentVar {
            key: "PATH".to_owned(),
            value: shims.display().to_string(),
            // A directory, not a credential, and the one variable worth reading
            // in a log when an agent cannot find a command it was allowed.
            secret: false,
        });
    }

    env
}

/// Points an agent's ordinary network traffic at the satellite's egress proxy.
///
/// Empty for a thread that declared no web policy, which is what leaves that
/// thread reaching the network exactly as it does today.
///
/// **Both cases of every name.** The tools in the image disagree about which
/// they read: curl takes the lowercase spelling, Go programs such as `gh` and
/// `jira` take either, and setting one and not the other is how a proxy quietly
/// applies to half an image.
///
/// **The proxy URL is a credential**, because the turn's admission travels in it
/// as userinfo, so it is masked wherever a command is rendered. The address on
/// its own is in the boot log, which is where somebody debugging a turn that
/// cannot reach its proxy would look.
fn egress_environment(grants: &Grants) -> Vec<AgentVar> {
    let Some(access) = grants.egress.as_ref() else {
        return Vec::new();
    };

    let exempt = no_proxy_for(grants.model.as_ref());

    [
        ("HTTP_PROXY", access.proxy_url.clone(), true),
        ("http_proxy", access.proxy_url.clone(), true),
        ("HTTPS_PROXY", access.proxy_url.clone(), true),
        ("https_proxy", access.proxy_url.clone(), true),
        ("NO_PROXY", exempt.clone(), false),
        ("no_proxy", exempt, false),
    ]
    .into_iter()
    .map(|(key, value, secret)| AgentVar {
        key: key.to_owned(),
        value,
        secret,
    })
    .collect()
}

/// The hosts an agent reaches directly rather than through the egress proxy.
///
/// Loopback, always. Every satellite-owned listener an agent legitimately talks
/// to is on it, and the one that matters is the [LLM proxy](crate::proxy): model
/// traffic keeps its own chokepoint, so a completion is not relayed twice and its
/// allowlist decision is not made by a component that holds no provider
/// credential and counts no tokens.
///
/// The model grant's own host is added when it is somehow not loopback, so the
/// exemption follows the proxy rather than an assumption about where it binds.
/// All three spellings of loopback are listed because a client matches this
/// variable as text rather than by resolving it.
fn no_proxy_for(model: Option<&ModelAccess>) -> String {
    let mut exempt = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];

    if let Some(host) = model.and_then(|access| host_of(&access.base_url))
        && !exempt.contains(&host)
    {
        exempt.push(host);
    }

    exempt.join(",")
}

/// The host part of a URL, without its scheme, port, or path.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .split_once("://")
        .map_or(url, |(_scheme, authority)| authority);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);

    // A bracketed IPv6 literal keeps its brackets off: `NO_PROXY` is matched
    // against a hostname, which is what a client has at the point it checks.
    let host = host.strip_prefix('[').map_or_else(
        || {
            host.rsplit_once(':')
                .map_or(host, |(host, _port)| host)
                .to_owned()
        },
        |rest| {
            rest.split_once(']')
                .map_or(rest, |(host, _port)| host)
                .to_owned()
        },
    );

    (!host.is_empty()).then_some(host)
}

/// What a thread asked for, before any CLI has a word for it.
///
/// Held as a decision rather than as a rendering so the question "what did the
/// thread ask for" is answered once and each harness arm answers "what does my
/// CLI call that" separately. Two harnesses re-deriving the posture from
/// `Permissions` would be two automata over one setting, and two of anything is
/// one more thing that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Posture {
    shell: ShellAccess,

    /// The commands the thread named, exactly as the contract carries them.
    ///
    /// Rendered into tool rules by the harness that has them. Codex has no such
    /// concept, and dropping the list there is stated in `docs/harness.md`
    /// rather than papered over with a flag that would not hold it.
    allowed_commands: Vec<String>,
}

/// How much of the shell a thread asked its agent to be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellAccess {
    /// The thread declared no exec policy, or named the preset.
    ///
    /// The agent runs with the container as its boundary, and the reasoning is
    /// worth stating because the flags this renders into read alarming out of
    /// context.
    ///
    /// The alternative is a gate that grants the shell and withholds everything
    /// else. That buys nothing: an agent holding a shell already reaches every
    /// byte and every socket the container reaches, so refusing it `WebFetch` is
    /// a formality it can route around with `curl`. What the narrower posture
    /// does buy is failure. A non-interactive turn cannot answer a gate, so the
    /// first tool nobody thought to list is refused mid-turn, and the agent
    /// spends the rest of the turn working around a restriction that was never
    /// intended and protects nothing.
    ///
    /// So the honest default is the one that matches the boundary that actually
    /// exists. For a thread that declared nothing that boundary is the
    /// container, and a satellite is a container built to be handed to an agent.
    /// A thread that declares an exec policy gets the broker underneath these
    /// flags rather than through them, and no CLI flag switches it off.
    Unrestricted,

    /// The thread named which commands it wants, or none at all.
    ///
    /// Edits stay approved either way, because the exec policy is about the
    /// shell and a thread that restricted its commands did not ask to stop
    /// editing files.
    Named,
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
        return Posture {
            shell: ShellAccess::Unrestricted,
            allowed_commands: Vec::new(),
        };
    };

    let exec = ExecAccess::try_from(permissions.exec).unwrap_or(ExecAccess::Unspecified);

    // `allowed_commands` is additive on top of whatever base `exec` sets, which
    // is the contract's own rule. Under the preset the base is already
    // everything, so the additions are covered rather than dropped.
    let allowed_commands = permissions
        .allowed_commands
        .iter()
        .map(|command| command.trim())
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
        .collect();

    let shell = match exec {
        // Unspecified means "the documented default", and the documented default
        // is the preset. Both land here so leaving the field alone and naming
        // the preset cannot behave differently.
        //
        // The preset is a curated command list, `broker::PRESET_COMMANDS`, and
        // it is deliberately not brokered: the contract makes unspecified mean
        // the preset, so brokering it would change the default for every thread
        // that never asked for a policy. Granting the shell overshoots what the
        // preset names, and it is the overshoot the contract already documents
        // as pending rather than a new one invented here.
        ExecAccess::Unspecified | ExecAccess::Preset => ShellAccess::Unrestricted,
        ExecAccess::None | ExecAccess::Custom => ShellAccess::Named,
    };

    Posture {
        shell,
        allowed_commands,
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
/// The CLI offers `acceptEdits`, `auto`, `bypassPermissions`, `manual`,
/// `dontAsk`, and `plan`. Only the two the satellite derives are ever emitted: a
/// spelling nothing constructs is one nobody checked against the CLI, and a
/// wrong one fails the launch rather than the permission.
///
/// Each rule list is one argument rather than several. The CLI accepts a comma
/// or space separated list and the option is variadic, so a rule per argument is
/// a parser question nobody should have to answer while reading a bug report.
///
/// With no command named, the shell is refused by name rather than left to be
/// refused by silence, so the harness reports a denial an operator can read
/// instead of an unexplained tool failure.
fn claude_permission_args(posture: &Posture) -> Vec<String> {
    let mode = match posture.shell {
        ShellAccess::Unrestricted => "bypassPermissions",
        ShellAccess::Named => "acceptEdits",
    };

    let mut args = vec!["--permission-mode".to_owned(), mode.to_owned()];

    let allowed: Vec<String> = posture
        .allowed_commands
        .iter()
        .flat_map(|command| bash_rules(command))
        .collect();

    if !allowed.is_empty() {
        args.push("--allowedTools".to_owned());
        args.push(allowed.join(","));
    }

    if posture.shell == ShellAccess::Named && allowed.is_empty() {
        args.push("--disallowedTools".to_owned());
        args.push("Bash".to_owned());
    }

    args
}

/// Renders the same posture as Codex CLI arguments.
///
/// Codex expresses permission through a coarse sandbox and an approval policy
/// rather than through per-tool rules, so this is a narrower map than the Claude
/// one and says so.
///
/// **Approvals are always `never`.** `codex exec` emits a one-way event stream,
/// so an approval request reaches nobody and a turn that raised one would hang
/// until the idle bound tore it down. That is the same reasoning that makes
/// Claude's `--print` posture what it is: a gate nothing can answer is a stall,
/// not a control.
///
/// **`allowed_commands` is not expressible.** Codex has no per-command rule on
/// `exec`; its only lever is how wide the sandbox is. A thread that named its
/// commands gets `workspace-write`, which is the narrowest sandbox that still
/// lets an agent edit the files it was asked to edit, and the command list
/// reaches the Claude arm only. Rendering it as something Codex would ignore
/// would advertise an enforcement that does not exist. Per-command approval is
/// what the app-server protocol on the roadmap carries.
///
/// Both settings are written as `-c` overrides rather than as `-s`, because
/// `codex exec resume` accepts no `-s` and the first turn and every turn after
/// it must run under the same posture.
fn codex_permission_args(posture: &Posture) -> Vec<String> {
    let sandbox = match posture.shell {
        ShellAccess::Unrestricted => "danger-full-access",
        ShellAccess::Named => "workspace-write",
    };

    vec![
        "-c".to_owned(),
        format!("sandbox_mode={sandbox}"),
        "-c".to_owned(),
        "approval_policy=never".to_owned(),
    ]
}

/// What the satellite lends one turn, and takes back when it ends.
///
/// The three travel together because they are the same kind of thing: something
/// the satellite hands an agent for the length of a turn, and the environment it
/// arrives in. Taking them as separate parameters is how a signature grows until
/// nobody can read a call site, and each is `Option` for the same reason: a turn
/// may run without a model grant, and most threads run without a broker or a
/// declared web policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// The turn's admission to the model.
    pub model: Option<ModelAccess>,

    /// The turn's admission to the network.
    ///
    /// Absent for every thread that declared no web policy, which is the default
    /// and leaves that thread's agents reaching the network exactly as they do
    /// today. See [`crate::egress`].
    pub egress: Option<EgressAccess>,

    /// The thread's shim directory, which becomes the agent's whole `PATH`.
    ///
    /// Absent for every thread that keeps the satellite's own, which is the
    /// default and every thread that declared no exec policy. See
    /// [`crate::broker`].
    pub exec_broker: Option<PathBuf>,
}

impl Grants {
    /// A turn admitted to the model, brokered by nothing and gated by nothing.
    #[must_use]
    pub fn model(access: ModelAccess) -> Self {
        Self {
            model: Some(access),
            ..Self::default()
        }
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

/// One turn's admission to the network, by way of the satellite's egress proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressAccess {
    /// The forward proxy an agent is pointed at, carrying the turn's admission
    /// as the URL's own credentials.
    ///
    /// One field rather than an address and a token, because every client that
    /// reads `HTTP_PROXY` takes both from one URL and splitting them here would
    /// only mean rejoining them at every call site.
    pub proxy_url: String,
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

/// A process that inherits none of the satellite's own credentials, and none of
/// its privilege.
///
/// Shared with workspace provisioning, because `git` and a repo's setup
/// commands are spawned by the satellite exactly as a harness is and inherit
/// exactly the same environment unless something takes it away. One scrub, used
/// by every spawn, is what keeps the rule from holding in one place and lapsing
/// in the next.
///
/// The same reasoning puts the privilege drop here. Inside the image the
/// satellite is root and every child of it runs as the unprivileged `arsox`
/// account, which is what makes the enforcement points something an agent cannot
/// rewrite. A drop applied per spawn site would be a step somebody could forget
/// on the next one; applied here, a new spawn site inherits it by construction.
/// See [the enforcement doc](../../../../docs/enforcement.md).
#[must_use]
pub fn scrubbed_command(program: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(program);

    for (key, _value) in std::env::vars() {
        if key.starts_with("ARSOX_") || is_provider_credential(&key) {
            process.env_remove(&key);
        }
    }

    // A no-op on a satellite that is not root, which is every bare-metal run and
    // every test. The posture is stated once at boot rather than per spawn.
    crate::privilege::hand_down(&mut process);

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
///
/// One variable per harness, because the stand-in replays a native transcript
/// and the two vocabularies are not interchangeable. Both are set once for a
/// test process and never change, so neither is a knob one test can turn under
/// another.
fn agent_environment() -> Vec<AgentVar> {
    #[cfg(feature = "test-util")]
    {
        ["ARSOX_FAKE_TRANSCRIPT", "ARSOX_FAKE_CODEX_TRANSCRIPT"]
            .into_iter()
            .filter_map(|key| {
                std::env::var(key).ok().map(|path| AgentVar {
                    key: key.to_owned(),
                    value: path,
                    // A fixture path. Masking it would hide the one fact worth
                    // reading when a stand-in harness replays the wrong file.
                    secret: false,
                })
            })
            .collect()
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
            &Grants::model(ModelAccess {
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
            &Grants::model(ModelAccess {
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
            &Grants::default(),
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
            &Grants::default(),
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
            &Grants::default(),
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
            &Grants::default(),
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
        // the Claude arm and nowhere above the match.
        let command = command_with(None);
        let flags = claude_permission_args(&posture_for(None));

        assert!(
            command.args.ends_with(&flags),
            "the Claude command should carry exactly the Claude permission flags"
        );
        assert!(
            !command.args.iter().any(|argument| argument == "-c"),
            "a Codex config override has no meaning to Claude"
        );
    }

    /// The Codex command a test builds, with only the varying parts named.
    fn codex_command_with(
        session: &Session,
        grants: &Grants,
        permissions: Option<&Permissions>,
    ) -> HarnessCommand {
        command_for(
            Harness::Codex,
            "do the thing",
            session,
            PathBuf::from("/workspace/thread"),
            grants,
            &[],
            permissions,
        )
    }

    /// A first turn's Codex command, with nothing else declared.
    fn first_codex_turn() -> HarnessCommand {
        codex_command_with(
            &Session::Start {
                session_id: "0199c0de-1111-7000-8000-000000000001".to_owned(),
            },
            &Grants::default(),
            None,
        )
    }

    #[test]
    fn a_codex_turn_runs_exec_with_the_json_stream() {
        // Without `--json` the CLI writes a human report, and the mapper pointed
        // at that parses prose as protocol.
        let command = first_codex_turn();

        assert_eq!(command.program, codex_binary());
        assert_eq!(command.args[0], "exec");
        assert!(command.args.contains(&"--json".to_owned()));
        assert!(!command.args.contains(&"resume".to_owned()));
    }

    #[test]
    fn a_codex_turn_is_allowed_to_run_outside_a_git_repository() {
        // Measured against 0.147.0: without this the CLI refuses to start and
        // writes one line of plain English, which the runner reports as a
        // harness that exited without saying what it did. A thread's workspace
        // is a directory the satellite created and frequently has no repository
        // in it at all.
        assert!(
            first_codex_turn()
                .args
                .contains(&"--skip-git-repo-check".to_owned())
        );
    }

    #[test]
    fn a_first_codex_turn_lets_the_cli_name_its_own_session() {
        // Codex mints the session id and announces it on `thread.started`, so
        // the id the satellite chose has nowhere to go. Passing one anyway would
        // mean inventing a flag the CLI does not have.
        let command = first_codex_turn();

        assert!(
            !command
                .args
                .iter()
                .any(|argument| argument.contains("0199c0de")),
            "{:?}",
            command.args
        );
    }

    #[test]
    fn a_later_codex_turn_resumes_the_session_the_cli_minted() {
        // This is what makes a thread a conversation. The id is the one the
        // mapper recorded from `thread.started`, which is exactly what
        // `codex exec resume` takes.
        let command = codex_command_with(
            &Session::Resume {
                session_id: "01a01cd2-200b-77f0-b4b8-7421557ff5ed".to_owned(),
            },
            &Grants::default(),
            None,
        );

        assert_eq!(command.args[0], "exec");
        assert_eq!(command.args[1], "resume");

        let positionals = command
            .args
            .iter()
            .position(|argument| argument == "--")
            .map(|index| &command.args[index + 1..])
            .expect("positionals travel behind the separator");

        assert_eq!(
            positionals,
            ["01a01cd2-200b-77f0-b4b8-7421557ff5ed", "do the thing"],
            "resume takes the session id first and the prompt second"
        );
    }

    #[test]
    fn a_codex_prompt_travels_behind_the_separator() {
        // A prompt is untrusted text and it is a positional argument here rather
        // than the value of a flag, so one beginning with a dash would otherwise
        // be parsed as one.
        let command = command_for(
            Harness::Codex,
            "--dangerously-bypass-approvals-and-sandbox",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants::default(),
            &[],
            None,
        );

        let separator = command
            .args
            .iter()
            .position(|argument| argument == "--")
            .expect("the separator should be emitted");

        assert_eq!(
            command.args[separator + 1],
            "--dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(command.args.len(), separator + 2);
    }

    #[test]
    fn codex_reaches_the_model_through_the_proxy_rather_than_the_provider() {
        // 0.147.0 does not read `OPENAI_BASE_URL`, so the address has to arrive
        // as a declared provider. `wire_api=responses` is what keeps this an
        // ordinary POST: left to its built-in provider the CLI opens a WebSocket
        // the proxy does not speak, and every model request would go straight
        // past the ceilings.
        let command = codex_command_with(
            &Session::Start {
                session_id: "x".to_owned(),
            },
            &Grants::model(ModelAccess {
                base_url: "http://127.0.0.1:9/t/the-token".to_owned(),
                token: "the-turns-proxy-token".to_owned(),
            }),
            None,
        );

        let overrides: Vec<&String> = command
            .args
            .iter()
            .enumerate()
            .filter(|(index, _setting)| {
                index
                    .checked_sub(1)
                    .and_then(|before| command.args.get(before))
                    == Some(&"-c".to_owned())
            })
            .map(|(_index, setting)| setting)
            .collect();

        assert!(overrides.contains(&&"model_provider=arsox".to_owned()));
        assert!(overrides.contains(
            &&"model_providers.arsox.base_url=http://127.0.0.1:9/t/the-token/v1".to_owned()
        ));
        assert!(overrides.contains(&&"model_providers.arsox.env_key=OPENAI_API_KEY".to_owned()));
        assert!(overrides.contains(&&"model_providers.arsox.wire_api=responses".to_owned()));

        // The key travels in the environment, which is what `env_key` names, and
        // it is the turn's token rather than a provider credential.
        let key = command
            .env
            .iter()
            .rfind(|variable| variable.key == "OPENAI_API_KEY")
            .expect("the turn's token should be set");
        assert_eq!(key.value, "the-turns-proxy-token");
        assert!(key.secret, "a turn token is never rendered in a log");
    }

    #[test]
    fn a_codex_thread_cannot_declare_its_way_around_the_proxy() {
        // The variable this version ignores is still set, and set last. A CLI
        // that grew a reading of it must not find an inherited or declared value
        // there, because that value is a route around every ceiling.
        let command = command_for(
            Harness::Codex,
            "do the thing",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants::model(ModelAccess {
                base_url: "http://127.0.0.1:9/t/the-token".to_owned(),
                token: "the-turns-proxy-token".to_owned(),
            }),
            &[declared(
                "OPENAI_BASE_URL",
                "https://elsewhere.invalid",
                None,
            )],
            None,
        );

        let applied = command
            .env
            .iter()
            .rfind(|variable| variable.key == "OPENAI_BASE_URL")
            .expect("the proxy address should be set");

        assert_eq!(applied.value, "http://127.0.0.1:9/t/the-token/v1");
    }

    #[test]
    fn a_codex_posture_is_a_sandbox_width_rather_than_a_tool_rule() {
        let unrestricted = first_codex_turn();

        assert!(
            unrestricted
                .args
                .contains(&"sandbox_mode=danger-full-access".to_owned()),
            "the default posture matches the boundary that actually exists"
        );
        // A one-way event stream has nobody to answer an approval, so a turn
        // that raised one would hang until the idle bound tore it down.
        assert!(
            unrestricted
                .args
                .contains(&"approval_policy=never".to_owned())
        );

        let named = Permissions {
            exec: ExecAccess::Custom.into(),
            allowed_commands: vec!["yarn install".to_owned()],
            ..Permissions::default()
        };
        let restricted = codex_command_with(
            &Session::Start {
                session_id: "x".to_owned(),
            },
            &Grants::default(),
            Some(&named),
        );

        assert!(
            restricted
                .args
                .contains(&"sandbox_mode=workspace-write".to_owned()),
            "a thread that named its commands did not ask for a blanket bypass"
        );
        // Codex has no per-command rule on `exec`. Emitting the list anyway
        // would advertise an enforcement the CLI would ignore. See
        // docs/harness.md.
        assert!(
            !restricted
                .args
                .iter()
                .any(|argument| argument.contains("yarn install")),
            "{:?}",
            restricted.args
        );
    }

    #[test]
    fn a_brokered_thread_runs_with_the_shim_directory_as_its_whole_path() {
        // The broker is a PATH and nothing else. An agent that kept the
        // satellite's PATH alongside it would resolve every real binary and
        // never meet a shim.
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants {
                exec_broker: Some(PathBuf::from("/opt/arsox/threads/x/bin")),
                ..Grants::default()
            },
            &[],
            None,
        );

        let path = command
            .env
            .iter()
            .rfind(|variable| variable.key == "PATH")
            .expect("a brokered thread is given a PATH");

        assert_eq!(path.value, "/opt/arsox/threads/x/bin");
        assert!(!path.secret, "a directory is not a credential");
    }

    #[test]
    fn a_thread_cannot_declare_its_way_around_the_exec_broker() {
        // The last value set for a key is the one the child sees, so the
        // broker's PATH is applied after everything the thread declared. A
        // declared PATH that won would be a gate a settings field opens.
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants {
                exec_broker: Some(PathBuf::from("/opt/arsox/threads/x/bin")),
                ..Grants::default()
            },
            &[declared("PATH", "/usr/bin:/bin", Some(false))],
            None,
        );

        let path = command
            .env
            .iter()
            .rfind(|variable| variable.key == "PATH")
            .expect("a brokered thread is given a PATH");

        assert_eq!(path.value, "/opt/arsox/threads/x/bin");
    }

    #[test]
    fn an_unbrokered_thread_keeps_the_satellites_own_path() {
        // The opt-in, asserted where it would break: a thread that declared no
        // exec policy must run exactly as it did before the broker existed,
        // which means nothing here sets PATH at all.
        let command = command_with(None);

        assert!(
            !command.env.iter().any(|variable| variable.key == "PATH"),
            "{:?}",
            command.env
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

    /// A turn's admission to the network, as the egress proxy issues one.
    fn egress_access() -> EgressAccess {
        EgressAccess {
            proxy_url: "http://arsox:the-turns-egress-token@127.0.0.1:41234".to_owned(),
        }
    }

    /// The command a gated turn builds, model grant and all.
    fn gated_command() -> HarnessCommand {
        command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants {
                model: Some(ModelAccess {
                    base_url: "http://127.0.0.1:9/t/the-token".to_owned(),
                    token: "the-turns-proxy-token".to_owned(),
                }),
                egress: Some(egress_access()),
                exec_broker: None,
            },
            &[],
            None,
        )
    }

    /// The value the child would see for `key`, which is the last one set.
    fn applied(command: &HarnessCommand, key: &str) -> Option<String> {
        command
            .env
            .iter()
            .rfind(|variable| variable.key == key)
            .map(|variable| variable.value.clone())
    }

    #[test]
    fn an_ungated_thread_gets_no_proxy_variables_at_all() {
        // The opt-in, asserted where it would break: a thread that declared no
        // web policy reaches the network exactly as it did before the egress
        // proxy existed, which means nothing here points it anywhere.
        let command = command_with(None);

        for variable in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "NO_PROXY"] {
            assert_eq!(applied(&command, variable), None, "{variable}");
        }
    }

    #[test]
    fn a_gated_thread_is_pointed_at_the_proxy_in_both_spellings() {
        // The image's tools disagree about which case they read: curl takes the
        // lowercase names and Go programs such as `gh` take either. Setting one
        // and not the other is how a proxy quietly applies to half an image.
        let command = gated_command();

        for variable in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            assert_eq!(
                applied(&command, variable).as_deref(),
                Some(egress_access().proxy_url.as_str()),
                "{variable}"
            );
        }
    }

    #[test]
    fn model_traffic_keeps_its_own_chokepoint() {
        // The two proxies stay apart. A completion relayed through the egress
        // proxy would pay a second hop and have its allowlist decision made by
        // the component that holds no provider credential and counts no tokens.
        let command = gated_command();
        let exempt = applied(&command, "NO_PROXY").expect("a gated thread is given one");

        assert!(exempt.contains("127.0.0.1"), "{exempt}");
        assert!(exempt.contains("localhost"), "{exempt}");
        assert!(exempt.contains("::1"), "{exempt}");

        // Both spellings, for the same reason the proxy variables have both.
        assert_eq!(applied(&command, "no_proxy"), Some(exempt));
    }

    #[test]
    fn the_exemption_follows_the_llm_proxy_rather_than_assuming_where_it_binds() {
        // Loopback today. Written as a rule about the model grant's own address
        // so that a proxy which ever moved would take its exemption with it.
        assert!(no_proxy_for(None).contains("127.0.0.1"));

        let elsewhere = ModelAccess {
            base_url: "http://model-proxy.internal:9000/t/the-token".to_owned(),
            token: "the-turns-proxy-token".to_owned(),
        };

        assert!(no_proxy_for(Some(&elsewhere)).contains("model-proxy.internal"));
    }

    #[test]
    fn a_url_yields_the_host_a_client_matches_no_proxy_against() {
        assert_eq!(
            host_of("http://127.0.0.1:9/t/x"),
            Some("127.0.0.1".to_owned())
        );
        assert_eq!(
            host_of("https://example.com/a/b"),
            Some("example.com".to_owned())
        );
        assert_eq!(
            host_of("http://user:pass@example.com:80/"),
            Some("example.com".to_owned())
        );
        assert_eq!(host_of("http://[::1]:9000/t/x"), Some("::1".to_owned()));
        assert_eq!(host_of("example.com"), Some("example.com".to_owned()));
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn a_thread_cannot_declare_its_way_around_the_egress_proxy() {
        // The last value set for a key is the one the child sees, so the
        // satellite's proxy variables are applied after everything the thread
        // declared. A declared HTTP_PROXY that won would be an allowlist a
        // settings field opens, and a declared NO_PROXY of `*` would be the same
        // hole wearing a different name.
        let command = command_for(
            Harness::Claude,
            "do the thing",
            &Session::Start {
                session_id: "x".to_owned(),
            },
            PathBuf::from("/workspace/thread"),
            &Grants {
                egress: Some(egress_access()),
                ..Grants::default()
            },
            &[
                declared("HTTP_PROXY", "http://elsewhere.invalid:3128", Some(false)),
                declared("https_proxy", "http://elsewhere.invalid:3128", Some(false)),
                declared("NO_PROXY", "*", Some(false)),
            ],
            None,
        );

        assert_eq!(
            applied(&command, "HTTP_PROXY").as_deref(),
            Some(egress_access().proxy_url.as_str())
        );
        assert_eq!(
            applied(&command, "https_proxy").as_deref(),
            Some(egress_access().proxy_url.as_str())
        );
        assert_ne!(applied(&command, "NO_PROXY").as_deref(), Some("*"));
    }

    #[test]
    fn the_admission_is_never_rendered_where_a_command_is() {
        // The turn's admission travels in the proxy URL as userinfo, so the URL
        // is a credential and a derived `Debug` would put a live one into any
        // log line that ever formatted a command.
        let rendered = format!("{:?}", gated_command());

        assert!(!rendered.contains("the-turns-egress-token"), "{rendered}");
        // The exemption is an address list and reads plainly, which is what
        // makes a turn that cannot reach its proxy debuggable at all.
        assert!(rendered.contains("127.0.0.1"), "{rendered}");
    }
}
