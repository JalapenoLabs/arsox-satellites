// Copyright © 2026 Jalapeno Labs

//! The exec broker: what an agent's shell can reach, decided by the satellite.
//!
//! A thread that declares `exec: NONE` or `exec: CUSTOM` gets a **shim
//! directory**, and that directory becomes the entire `PATH` of the harness
//! process. Every name in it is a small root-owned file whose only content is a
//! shebang pointing back at the satellite binary, so a command an agent types is
//! resolved into the satellite, checked against the thread's allowlist, and
//! either `exec`ed or refused.
//!
//! A refusal writes one record into the thread's deny spool, which the runner
//! drains after each session into a `PERMISSION_COMMAND_DENIED` incident with a
//! `blocked` disposition. Nothing about being denied is silent.
//!
//! # It is a gate, not a jail
//!
//! What this deterministically shapes is **name resolution**, which is how a
//! harness invokes a command and how nearly everything inside a script is
//! written. It does not stop an agent that types `/usr/bin/git`, because the real
//! binaries stay readable and executable, and it cannot stop anything at all once
//! a general-purpose interpreter is on the allowlist. Both limits are stated in
//! full in [the enforcement doc](../../../../docs/enforcement.md), because a
//! control whose limits are unwritten is one somebody will over-trust.
//!
//! # It engages only where it can hold
//!
//! Installing a shim directory the agent could delete would be theatre, so the
//! broker engages only on a satellite that separates privilege: root, with the
//! agent account present. See [`crate::privilege`]. Off that, and for a thread
//! that declared `PRESET` or declared nothing, the agent keeps the satellite's
//! own `PATH` exactly as it does today.

mod install;
pub mod shim;
pub mod spool;

use arsox_sdk::proto::settings::v1::{ExecAccess, Permissions};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the per-thread shim directories live when `ARSOX_BROKER_ROOT` is unset.
///
/// Under `/opt` rather than under the workspace volume, deliberately: the
/// workspace is the agent's own and the shims must not be. The Dockerfile
/// creates this root-owned and `0711`, so a thread can traverse into its own
/// directory by name and cannot list its neighbours'.
pub const DEFAULT_BROKER_ROOT: &str = "/opt/arsox/threads";

/// Where a thread's policy sits, relative to its broker directory.
///
/// Beside the shim directory rather than inside it, so the policy is not itself
/// a name on the agent's `PATH`.
const POLICY_FILE: &str = "policy.json";

/// Names that resolve for any brokered thread, because a harness that cannot
/// start is not a restricted agent.
///
/// The Claude CLI is a Node program launched through a `#!/usr/bin/env node`
/// shebang, so both names have to resolve or no turn runs at all under any
/// policy. This is a floor rather than a grant: it is written into the policy
/// file, so what a thread actually runs under is readable rather than inferred.
const HARNESS_RUNTIME: [&str; 2] = ["node", "env"];

/// The harness CLIs themselves, resolved as this satellite would launch them.
///
/// The satellite spawns the harness by name, so a policy that refused it would
/// refuse the turn rather than restrict it. Taken from the same resolution the
/// spawner uses, so a satellite pointed at a stand-in through `ARSOX_CLAUDE_BIN`
/// puts that stand-in's name on the floor: an override naming an absolute path
/// bypasses `PATH` entirely, and one naming a bare command needs a shim like any
/// other name.
fn harness_names() -> Vec<String> {
    [
        crate::harness::spawn::claude_binary(),
        crate::harness::spawn::codex_binary(),
    ]
    .iter()
    .filter_map(|program| {
        Path::new(program)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
    })
    .collect()
}

/// Shells added to the floor for a thread that named its commands.
///
/// A harness runs an allowed command *through* a shell, so a policy that took
/// the shell away would refuse every command it had just permitted. Under
/// `exec: NONE` they are deliberately absent: a shell reached by its absolute
/// path still starts, and every name it then looks up is a shim that records a
/// denial, which is a better outcome than the silence of a missing shell.
const SHELLS: [&str; 2] = ["sh", "bash"];

/// The curated command set `ExecAccess::PRESET` names.
///
/// **The curation principle**: a command earns a place when an ordinary
/// development task fails without it, and its effect is confined to the
/// workspace when run by an unprivileged user. So this is the image's shipped
/// tooling plus the utilities development actually reaches for, and nothing that
/// changes the machine or changes who the agent is.
///
/// Deliberately absent, each for the second half of that principle rather than
/// the first: `sudo` and `su`, `apt` and `dpkg`, `docker`, `systemctl`, `mount`,
/// `useradd`, `chown`.
///
/// **Not yet the base of a brokered policy.** `PRESET` and an undeclared `exec`
/// keep the satellite's own `PATH`, exactly as they do today; the contract says
/// unspecified means the preset, so putting this under the broker would change
/// the default for every thread that never asked for a policy. That is a
/// decision with its own blast radius and it gets its own change. The list is
/// defined now so that when it lands there is one definition rather than one
/// invented at the time.
pub const PRESET_COMMANDS: &[&str] = &[
    // The image's shipped tooling, per the README's preinstalled list.
    "git",
    "git-lfs",
    "gh",
    "jira",
    "node",
    "npm",
    "npx",
    "corepack",
    "yarn",
    "pnpm",
    "python3",
    "pip",
    "pip3",
    "curl",
    "wget",
    "jq",
    "rg",
    "fd",
    "zip",
    "unzip",
    "tar",
    "gzip",
    "gunzip",
    "less",
    // Building, which `build-essential` is in the image for.
    "make",
    "cc",
    "gcc",
    "g++",
    "pkg-config",
    // Shells and the interpreter shebang, without which nothing above runs.
    "sh",
    "bash",
    "env",
    // Files and directories.
    "ls",
    "cat",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "rmdir",
    "touch",
    "ln",
    "chmod",
    "pwd",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "stat",
    "du",
    "df",
    "find",
    "xargs",
    "which",
    // Text, which is most of what an agent does at a shell.
    "echo",
    "printf",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "tr",
    "sed",
    "awk",
    "grep",
    "diff",
    "patch",
    // Odds and ends a script assumes exist.
    "true",
    "false",
    "sleep",
    "date",
    // Git over SSH invokes these itself.
    "ssh",
    "ssh-keygen",
];

/// One thread's brokered allowlist, as the shim reads it.
///
/// Written to disk beside the shim directory and read on every invocation. It
/// carries the resolved facts rather than the settings they came from, so the
/// shim needs no view of the satellite's state and a policy file can be read by
/// a human debugging what a thread was actually allowed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub thread_id: String,

    /// The entries the thread declared, trimmed, exactly as it wrote them.
    pub allowed: Vec<String>,

    /// Names that resolve whatever `allowed` says. See [`HARNESS_RUNTIME`].
    pub runtime: Vec<String>,

    /// Where the shim looks for the real binary, in order.
    ///
    /// Recorded rather than inherited, because the shim runs with the agent's
    /// `PATH`, which is the shim directory and nothing else. A shim that
    /// searched its own `PATH` would find itself.
    pub search_path: Vec<PathBuf>,

    /// Where a refusal is written down.
    pub spool: PathBuf,
}

impl Policy {
    /// Resolves a thread's policy, or `None` when the broker does not engage.
    ///
    /// Engagement is the whole of the behaviour change this feature makes:
    /// `NONE` and `CUSTOM` are brokered, and an undeclared `exec` or a declared
    /// `PRESET` keeps the satellite's own `PATH` exactly as it does today.
    #[must_use]
    pub fn for_thread(
        thread_id: &str,
        permissions: Option<&Permissions>,
        spool: PathBuf,
        search_path: Vec<PathBuf>,
    ) -> Option<Self> {
        let permissions = permissions?;
        let exec = ExecAccess::try_from(permissions.exec).unwrap_or(ExecAccess::Unspecified);

        let runtime = match exec {
            // Unspecified means the documented default and the documented
            // default is the preset. Neither is brokered, so neither can change
            // behaviour for a thread that asked for nothing.
            ExecAccess::Unspecified | ExecAccess::Preset => return None,

            // No commands at all, so no shell on the floor either. What the
            // harness needs to run is all that resolves.
            ExecAccess::None => floor(&[]),

            // The allowed commands run through a shell, so the shell is part of
            // being able to run them.
            ExecAccess::Custom => floor(&SHELLS),
        };

        // `allowed_commands` is additive on top of whatever base `exec` set,
        // which is the contract's own rule, so a thread that turned the shell
        // off and then named one command has named a grant rather than a
        // contradiction. Trimmed and emptied exactly as the advisory layer
        // trims them, so the two layers read one list the same way.
        let allowed = permissions
            .allowed_commands
            .iter()
            .map(|command| command.trim())
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
            .collect();

        Some(Self {
            thread_id: thread_id.to_owned(),
            allowed,
            runtime,
            search_path,
            spool,
        })
    }

    /// Whether this policy permits an invocation.
    ///
    /// `name` is the command as it was invoked and `arguments` is everything
    /// after it. See [`entry_permits`] for what an entry means.
    #[must_use]
    pub fn permits(&self, name: &str, arguments: &[String]) -> bool {
        self.runtime.iter().any(|floor| floor == name)
            || self
                .allowed
                .iter()
                .any(|entry| entry_permits(entry, name, arguments))
    }

    /// Every name the shim directory must carry for this policy.
    ///
    /// The union of what the image can run and what the policy names. The image
    /// half is what makes a denial *reportable*: a command with no shim is a
    /// `PATH` miss that says "not found" and records nothing, and the whole
    /// point of a `blocked` incident is that an agent working around a denial is
    /// visible. The policy half covers an entry naming something this image does
    /// not carry, which is refused as not installed rather than silently absent.
    #[must_use]
    pub fn shim_names(&self, image_commands: &BTreeSet<String>) -> BTreeSet<String> {
        let mut names = image_commands.clone();

        names.extend(self.runtime.iter().cloned());
        names.extend(
            self.allowed
                .iter()
                .filter_map(|entry| entry.split_whitespace().next())
                .map(str::to_owned),
        );

        names
    }
}

/// The names that resolve whatever a policy says, plus `also`.
///
/// One list rather than a set, and it may hold a name twice if a satellite is
/// pointed at a stand-in harness called `node`. A duplicate on the floor costs
/// one redundant comparison and is not worth a set's ordering surprise in a file
/// a human reads.
fn floor(also: &[&str]) -> Vec<String> {
    HARNESS_RUNTIME
        .iter()
        .chain(also.iter())
        .copied()
        .map(str::to_owned)
        .chain(harness_names())
        .collect()
}

/// Whether one allowlist entry permits an invocation.
///
/// An entry is split on whitespace into tokens. The invocation matches when its
/// leading tokens equal the entry's tokens, one for one: no globbing, no
/// substring matching, and no shell parsing. So `gh` permits `gh pr list`,
/// `yarn install` permits `yarn install --frozen-lockfile` and refuses
/// `yarn build`, and `yarn` never matches `yarnpkg`.
///
/// This is the same reading the advisory layer gives an entry, which emits
/// `Bash(yarn install)` and `Bash(yarn install *)` as a pair for exactly this
/// reason. An operator whose allowlist meant one thing to the CLI and another to
/// the broker would be debugging a disagreement rather than a policy.
///
/// Whole-argv equality was the other candidate and is worse: `gh` would then
/// permit only the bare word, and every useful entry would have to enumerate its
/// own subcommands.
fn entry_permits(entry: &str, name: &str, arguments: &[String]) -> bool {
    let mut tokens = entry.split_whitespace();

    let Some(head) = tokens.next() else {
        return false;
    };

    if head != name {
        return false;
    }

    let required: Vec<&str> = tokens.collect();

    arguments.len() >= required.len()
        && required
            .iter()
            .zip(arguments)
            .all(|(want, got)| *want == got.as_str())
}

/// The satellite's exec broker: the shim directories and the deny spools.
///
/// Cheap to clone: it holds one path. Held by the provisioner, which installs a
/// thread's directory, the runner, which points a harness at it and drains its
/// spool, and the collector, which removes it with the workspace.
#[derive(Debug, Clone)]
pub struct Broker {
    root: PathBuf,
}

impl Broker {
    /// A broker keeping its shim directories under `root`.
    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The broker a container boot uses.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::at(PathBuf::from(
            std::env::var("ARSOX_BROKER_ROOT")
                .unwrap_or_else(|_ignored| DEFAULT_BROKER_ROOT.to_owned()),
        ))
    }

    /// Where one thread's shims live, whether or not any are installed.
    #[must_use]
    pub fn shim_directory(&self, thread_id: &str) -> PathBuf {
        self.thread_directory(thread_id).join("bin")
    }

    /// Where one thread's refusals are written down.
    #[must_use]
    pub fn spool_directory(&self, thread_id: &str) -> PathBuf {
        self.thread_directory(thread_id).join("denied")
    }

    fn thread_directory(&self, thread_id: &str) -> PathBuf {
        self.root.join(thread_id)
    }

    /// Installs a thread's shim directory, and reports whether one was needed.
    ///
    /// Returns the directory that becomes the agent's `PATH`, or `None` when
    /// this thread is not brokered: either it declared no policy the broker
    /// engages for, or this satellite cannot separate privilege and therefore
    /// cannot hold a gate. Both are ordinary outcomes rather than failures.
    ///
    /// Idempotent. A thread reprovisioned after a restart gets a directory built
    /// from its current settings rather than merged onto whatever was there.
    ///
    /// Building a shim directory is a file per command name the image carries,
    /// which is a thousand small writes on an ordinary image, so the work runs
    /// on the blocking pool rather than on a runtime thread that has turns to
    /// drive.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the directory cannot be built. The
    /// caller reports it and leaves the thread unbrokered rather than
    /// half-brokered.
    pub async fn install(
        &self,
        thread_id: &str,
        permissions: Option<&Permissions>,
    ) -> std::io::Result<Option<PathBuf>> {
        if !crate::privilege::descent().enforces() {
            return Ok(None);
        }

        let shims = self.shim_directory(thread_id);
        let Some(policy) = Policy::for_thread(
            thread_id,
            permissions,
            self.spool_directory(thread_id),
            // The satellite's own, which is unrestricted. Resolved at install
            // rather than at invocation because the shim runs with the agent's
            // `PATH`, and that is the shim directory alone.
            search_path(&shims),
        ) else {
            return Ok(None);
        };

        let thread_root = self.thread_directory(thread_id);
        blocking(move || install::install(&thread_root, &policy)).await?;

        Ok(Some(shims))
    }

    /// Removes a thread's shim directory, spool and all.
    ///
    /// A directory that is already gone is a success, exactly as an already
    /// removed workspace is: collection can reach a thread by more than one
    /// path.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the directory exists and will not
    /// be removed.
    pub async fn remove(&self, thread_id: &str) -> std::io::Result<()> {
        let thread_root = self.thread_directory(thread_id);

        blocking(move || match std::fs::remove_dir_all(thread_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        })
        .await
    }

    /// Reads and clears every refusal a thread has accumulated.
    ///
    /// Drained by the runner after each harness session, which is what lets a
    /// refusal be attributed to the turn that caused it. See [`spool::drain`].
    pub async fn drain_denials(&self, thread_id: &str) -> Vec<spool::Denial> {
        let spool = self.spool_directory(thread_id);

        blocking(move || Ok(spool::drain(&spool)))
            .await
            .unwrap_or_default()
    }
}

/// Runs blocking filesystem work off the runtime, keeping its error.
///
/// A pool thread that panics reports as a join failure, which is a satellite
/// bug and reads as one rather than being flattened into "the directory could
/// not be built".
async fn blocking<T, F>(work: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(std::io::Error::other)?
}

/// The directories the shim should look for a real binary in.
///
/// The satellite's own `PATH`, minus the shim directory itself. Without that
/// subtraction a shim would resolve to a shim and exec itself until the process
/// ran out of file descriptors.
fn search_path(shims: &Path) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    std::env::split_paths(&path)
        .filter(|directory| directory != shims)
        .collect()
}

/// Every command name the given directories can resolve.
///
/// Reads the directories rather than being handed a list, because the set is a
/// fact about the image this satellite is running in and an image grows tooling
/// on its own schedule. A directory that cannot be read contributes nothing and
/// is not an error: a `PATH` naming somewhere that does not exist is ordinary.
#[must_use]
pub fn image_commands(search_path: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

    for directory in search_path {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };

        for entry in entries.flatten() {
            // Anything but a directory. Executability is not checked: the mode
            // bits are the kernel's business at `exec`, and a shim for a name
            // that turns out not to run is refused with a readable reason
            // rather than being quietly missing.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }

            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_owned());
            }
        }
    }

    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's permissions, with only the exec policy varying.
    fn permissions(exec: ExecAccess, allowed: &[&str]) -> Permissions {
        Permissions {
            exec: exec.into(),
            allowed_commands: allowed.iter().copied().map(str::to_owned).collect(),
            ..Permissions::default()
        }
    }

    /// The policy a thread with these permissions runs under.
    fn policy(exec: ExecAccess, allowed: &[&str]) -> Option<Policy> {
        Policy::for_thread(
            "019fd32f-a25f-7611-a4fe-c93cc2a6d782",
            Some(&permissions(exec, allowed)),
            PathBuf::from("/opt/arsox/threads/x/denied"),
            vec![PathBuf::from("/usr/bin")],
        )
    }

    /// An invocation, split the way the shim splits one.
    fn arguments(words: &[&str]) -> Vec<String> {
        words.iter().copied().map(str::to_owned).collect()
    }

    #[test]
    fn a_thread_that_declared_nothing_is_not_brokered() {
        // The whole opt-in claim. A thread that asked for no policy keeps the
        // satellite's own PATH, exactly as it does today.
        assert!(
            Policy::for_thread(
                "019fd32f-a25f-7611-a4fe-c93cc2a6d782",
                None,
                PathBuf::new(),
                Vec::new()
            )
            .is_none()
        );
        assert!(policy(ExecAccess::Unspecified, &[]).is_none());
    }

    #[test]
    fn declaring_the_preset_reads_the_same_as_declaring_nothing() {
        // The contract says unspecified means the preset, so the two cannot
        // diverge without that default rule quietly breaking. Bringing the
        // preset under the broker would change the default for every thread
        // that never asked for a policy, and that is its own decision.
        assert!(policy(ExecAccess::Preset, &["yarn install"]).is_none());
    }

    #[test]
    fn naming_commands_or_refusing_them_engages_the_broker() {
        assert!(policy(ExecAccess::Custom, &["yarn install"]).is_some());
        assert!(policy(ExecAccess::None, &[]).is_some());
    }

    #[test]
    fn an_entry_permits_the_bare_command_and_the_command_with_arguments() {
        // Granting only one form would refuse the bare invocation of the very
        // command the thread allowed, which is why the advisory layer emits two
        // rules for one entry.
        let policy = policy(ExecAccess::Custom, &["gh"]).expect("brokered");

        assert!(policy.permits("gh", &[]));
        assert!(policy.permits("gh", &arguments(&["pr", "list"])));
    }

    #[test]
    fn an_entry_naming_a_subcommand_permits_only_that_subcommand() {
        let policy = policy(ExecAccess::Custom, &["yarn install"]).expect("brokered");

        assert!(policy.permits("yarn", &arguments(&["install"])));
        assert!(policy.permits("yarn", &arguments(&["install", "--frozen-lockfile"])));

        assert!(!policy.permits("yarn", &arguments(&["build"])));
        assert!(
            !policy.permits("yarn", &[]),
            "a bare `yarn` is not the `yarn install` that was allowed"
        );
    }

    #[test]
    fn matching_is_token_for_token_rather_than_textual() {
        // The failure this rules out is a substring match, where `yarn` would
        // permit `yarnpkg` and `git` would permit `git-crypt`.
        let policy = policy(ExecAccess::Custom, &["git status"]).expect("brokered");

        assert!(!policy.permits("git-crypt", &arguments(&["status"])));
        assert!(!policy.permits("gi", &arguments(&["status"])));
        assert!(!policy.permits("git", &arguments(&["statuses"])));
        assert!(!policy.permits("git", &arguments(&["push"])));
    }

    #[test]
    fn an_entry_a_human_typed_loosely_still_means_what_it_says() {
        // Trimmed and blank-skipped exactly as the advisory layer trims them,
        // so one list cannot read two ways.
        let policy =
            policy(ExecAccess::Custom, &["  yarn   install  ", "", "   "]).expect("brokered");

        assert_eq!(policy.allowed, vec!["yarn   install"]);
        assert!(policy.permits("yarn", &arguments(&["install"])));
    }

    #[test]
    fn a_thread_with_no_commands_still_gets_what_the_harness_needs_to_start() {
        // A policy that took `node` away from a Node CLI would not be a
        // restricted agent, it would be a satellite that cannot run a turn. The
        // harness itself is on the floor for the same reason: the satellite
        // spawns it by name.
        let policy = policy(ExecAccess::None, &[]).expect("brokered");

        assert!(policy.permits("node", &arguments(&["--version"])));
        assert!(policy.permits("env", &arguments(&["node"])));
        assert!(policy.permits("claude", &arguments(&["--print"])));
        assert!(policy.permits("codex", &arguments(&["exec"])));

        assert!(!policy.permits("git", &arguments(&["push"])));
    }

    #[test]
    fn a_shell_is_on_the_floor_only_where_commands_have_to_run_through_one() {
        // Under CUSTOM the harness runs an allowed command through a shell, so
        // taking the shell away would refuse every command just permitted.
        // Under NONE the shell is a deny shim, which records a refusal where a
        // missing shell would produce silence.
        assert!(
            policy(ExecAccess::Custom, &["yarn install"])
                .expect("brokered")
                .permits("sh", &arguments(&["-c", "yarn install"]))
        );
        assert!(
            !policy(ExecAccess::None, &[])
                .expect("brokered")
                .permits("sh", &arguments(&["-c", "anything"]))
        );
    }

    #[test]
    fn commands_are_additive_on_top_of_whatever_the_exec_policy_set() {
        // The contract's own rule: naming commands beside a base of nothing is
        // a grant rather than a contradiction to resolve.
        let policy = policy(ExecAccess::None, &["git status"]).expect("brokered");

        assert!(policy.permits("git", &arguments(&["status"])));
        assert!(!policy.permits("git", &arguments(&["push"])));
    }

    #[test]
    fn the_shim_set_covers_the_image_as_well_as_the_allowlist() {
        // The image half is what makes a denial reportable. A command with no
        // shim is a PATH miss that says "not found" and records nothing, and an
        // agent quietly working around a denial is the thing a blocked incident
        // exists to make visible.
        let policy = policy(ExecAccess::Custom, &["yarn install"]).expect("brokered");
        let image = ["git", "node", "docker"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let names = policy.shim_names(&image);

        assert!(names.contains("docker"), "a denied command needs a shim");
        assert!(names.contains("yarn"), "an allowed command needs a shim");
        assert!(names.contains("sh"), "the floor needs a shim");
        assert!(names.contains("node"));
    }

    #[test]
    fn a_shim_is_named_for_the_command_rather_than_the_whole_entry() {
        // `yarn install` is one entry and one binary. A file called
        // "yarn install" would be on PATH under a name nothing can invoke.
        let policy = policy(ExecAccess::Custom, &["yarn install"]).expect("brokered");

        let names = policy.shim_names(&BTreeSet::new());

        assert!(names.contains("yarn"));
        assert!(!names.iter().any(|name| name.contains(' ')));
    }

    #[test]
    fn the_preset_names_the_image_tooling_and_refuses_the_machine() {
        // The curation principle, asserted rather than only written down: an
        // ordinary development task fails without these, and each is confined
        // to the workspace when run unprivileged.
        for expected in [
            "git", "node", "yarn", "npm", "python3", "pip3", "gh", "jira", "rg", "fd", "jq", "sh",
            "make", "grep", "sed",
        ] {
            assert!(
                PRESET_COMMANDS.contains(&expected),
                "{expected} should be in the preset"
            );
        }

        for excluded in [
            "sudo",
            "su",
            "apt",
            "apt-get",
            "dpkg",
            "docker",
            "systemctl",
            "mount",
            "useradd",
            "chown",
        ] {
            assert!(
                !PRESET_COMMANDS.contains(&excluded),
                "{excluded} changes something outside the workspace, or changes who the agent is"
            );
        }
    }

    #[test]
    fn the_preset_names_each_command_once() {
        // A duplicate is harmless and is also a sign nobody read the list
        // before adding to it.
        let unique: BTreeSet<&&str> = PRESET_COMMANDS.iter().collect();

        assert_eq!(unique.len(), PRESET_COMMANDS.len());
    }

    #[test]
    fn the_shim_directory_never_appears_in_its_own_search_path() {
        // A shim that searched a path containing itself would exec itself until
        // the process ran out of descriptors.
        let shims = PathBuf::from("/opt/arsox/threads/x/bin");

        assert!(!search_path(&shims).contains(&shims));
    }

    #[test]
    fn a_broker_names_one_directory_per_thread() {
        let broker = Broker::at(PathBuf::from("/opt/arsox/threads"));

        assert!(broker.shim_directory("abc").ends_with("abc/bin"));
        assert!(broker.spool_directory("abc").ends_with("abc/denied"));
    }

    #[tokio::test]
    async fn removing_a_broker_directory_that_is_already_gone_succeeds() {
        // Collection can reach a thread by more than one path, exactly as it
        // can for a workspace.
        let broker = Broker::at(std::env::temp_dir().join("arsox-broker-absent"));

        broker
            .remove("019fd32f-0000-7000-8000-000000000000")
            .await
            .expect("an absent directory is not a failure");
    }

    #[tokio::test]
    async fn an_unenforcing_satellite_installs_nothing_however_the_thread_declared() {
        // The opt-in has two halves and this is the second: a shim directory
        // the agent could delete is not a gate. Every test runner is this case,
        // and so is every bare-metal satellite.
        assert!(
            !crate::privilege::descent().enforces(),
            "a test runner is not expected to be a root satellite"
        );

        let broker =
            Broker::at(std::env::temp_dir().join(format!("arsox-broker-{}", uuid::Uuid::now_v7())));

        let installed = broker
            .install(
                "019fd32f-a25f-7611-a4fe-c93cc2a6d782",
                Some(&permissions(ExecAccess::Custom, &["yarn install"])),
            )
            .await
            .expect("declining to install is not a failure");

        assert_eq!(installed, None);
    }

    #[test]
    fn the_image_command_set_is_read_from_the_directories_it_is_given() {
        // A PATH entry that does not exist contributes nothing rather than
        // failing the enumeration, because a PATH naming somewhere absent is
        // ordinary on every machine.
        let missing = std::env::temp_dir().join("arsox-not-a-directory-019fd32f");

        assert!(image_commands(&[missing]).is_empty());
    }
}
