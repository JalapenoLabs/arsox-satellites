// Copyright © 2026 Jalapeno Labs

//! The other end of a shim: the satellite, run as a command an agent typed.
//!
//! Every file in a thread's shim directory holds one line:
//!
//! ```text
//! #!/usr/local/bin/arsox-satellite --exec-shim
//! ```
//!
//! So `git status`, typed into a shell whose `PATH` is that directory, execs the
//! satellite. The kernel hands it the path of the file it invoked, which is how
//! this learns both which command was asked for and which thread's policy
//! applies: the command is the file's basename, and the policy sits beside the
//! directory it came out of.
//!
//! # Why a shebang rather than a symlink
//!
//! A symlink per name would be cheaper and loses the one fact this needs most.
//! When a shell resolves `git` through `PATH`, `argv[0]` is the bare word `git`,
//! and `/proc/self/exe` resolves through the symlink to the satellite binary.
//! Neither says which directory the shim came out of, so neither says which
//! thread's policy to apply. A shebang script is passed to its interpreter *by
//! path*, so this is told exactly where it lives.
//!
//! It also closes an `argv[0]` question. The name is taken from the file the
//! kernel executed rather than from `argv[0]`, which any caller can set to
//! anything, so lying about `argv[0]` cannot make this run a different binary.
//! The worst it achieves is running the command it named.
//!
//! # It fails closed
//!
//! A policy that cannot be read, a spool that will not take a record, a binary
//! that is not there: none of them let the command through. The one thing a
//! failure here must never do is run something the policy did not permit.

use super::Policy;
use std::path::{Path, PathBuf};

/// The argument a shim's shebang line carries.
///
/// The kernel passes everything after the interpreter path as one argument, so
/// this is a single word by necessity as well as by taste.
pub const SHIM_FLAG: &str = "--exec-shim";

/// Exit code for a command the policy refused.
///
/// 126 is the shell's own code for "found and could not be run", which is
/// exactly what happened, and it is distinct from the 127 a missing command
/// gets. An agent reading its tool output can tell the two apart without
/// parsing the message.
const DENIED: i32 = 126;

/// Exit code for a command this image does not carry.
const NOT_INSTALLED: i32 = 127;

/// Exit code for a shim that could not read its own policy.
///
/// Its own code because it is a satellite fault rather than a permission
/// decision, and an operator seeing it should look at the shim directory rather
/// than at the allowlist.
const BROKEN: i32 = 125;

/// One command, as the kernel handed it to a shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The shim file that was executed, which names the command and the thread.
    pub shim: PathBuf,

    /// Everything after the command.
    pub arguments: Vec<String>,
}

impl Invocation {
    /// The command as it was invoked.
    #[must_use]
    pub fn name(&self) -> &str {
        self.shim
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
    }

    /// The whole invocation, the command first, as it reaches an incident.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.name().to_owned())
            .chain(self.arguments.iter().cloned())
            .collect()
    }

    /// Where the thread's policy sits, relative to this shim.
    ///
    /// Beside the shim directory rather than inside it, so the policy is not
    /// itself a name on the agent's `PATH`.
    #[must_use]
    pub fn policy_path(&self) -> Option<PathBuf> {
        Some(self.shim.parent()?.parent()?.join(super::POLICY_FILE))
    }
}

/// Whether this process was started as a shim, and what it was asked to run.
///
/// The kernel builds the interpreter's argv as the interpreter path, the
/// shebang's own argument, the script path, and then the original arguments. So
/// a shim invocation is recognized by its second word and reads its own path
/// from the third.
///
/// Pure, so what the kernel does can be asserted without asking it to do it.
#[must_use]
pub fn invoked_as(arguments: &[String]) -> Option<Invocation> {
    if arguments.get(1).map(String::as_str) != Some(SHIM_FLAG) {
        return None;
    }

    Some(Invocation {
        shim: PathBuf::from(arguments.get(2)?),
        arguments: arguments.get(3..).unwrap_or_default().to_vec(),
    })
}

/// What a shim should do about one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The policy permits it. The real binary is still to be resolved.
    Allow,

    /// The policy refuses it. The reason completes "the command ...".
    Deny(&'static str),
}

/// Decides one invocation against a thread's policy.
///
/// Separated from [`run`] so the decision can be asserted without a filesystem,
/// a shim directory, or an `exec`. It is the same matching the advisory layer
/// applies to the same entries, through [`Policy::permits`].
#[must_use]
pub fn verdict_for(policy: &Policy, name: &str, arguments: &[String]) -> Verdict {
    if policy.permits(name, arguments) {
        Verdict::Allow
    } else {
        Verdict::Deny("is not on this thread's exec allowlist")
    }
}

/// Runs the shim to its end, and never returns.
///
/// On the permitted path this `exec`s the real binary, replacing the process, so
/// there is one extra process start on the way in and no wrapper left behind
/// afterwards. On every other path it exits with a code that says which of them
/// happened.
///
/// # Panics
///
/// Never. It exits.
pub fn run(invocation: &Invocation) -> ! {
    let name = invocation.name();

    let Some(policy) = read_policy(invocation) else {
        // Failing closed. A shim that ran the command because it could not find
        // its own policy would be a gate that opens when it breaks.
        eprintln!(
            "arsox: this exec shim could not read its thread's policy, so `{name}` was not run"
        );
        std::process::exit(BROKEN);
    };

    match verdict_for(&policy, name, &invocation.arguments) {
        Verdict::Allow => {}
        Verdict::Deny(reason) => refuse(&policy, invocation, reason),
    }

    let Some(binary) = resolve(&policy.search_path, name) else {
        // Not a permission decision, so it is not an incident. The command was
        // allowed and this image does not carry it, which is a fact about the
        // image and reads as one.
        eprintln!(
            "arsox: `{name}` is allowed on this thread but is not installed on this satellite"
        );
        std::process::exit(NOT_INSTALLED);
    };

    launch(&binary, name, &invocation.arguments)
}

/// Refuses a command, recording it before saying so, and never returns.
///
/// The record goes down first. A refusal the operator never learns about is the
/// silent failure the incident system exists to prevent, and the agent is going
/// to be told either way.
fn refuse(policy: &Policy, invocation: &Invocation, reason: &'static str) -> ! {
    let denial = super::spool::Denial::now(
        &policy.thread_id,
        super::spool::Kind::CommandDenied,
        invocation.name(),
        invocation.argv(),
        reason,
    );

    if let Err(error) = super::spool::record(&policy.spool, &denial) {
        // Said on stderr because there is nowhere else: this process is about
        // to end and the satellite is the thing that could not be reached.
        eprintln!("arsox: could not record this refusal for the satellite: {error}");
    }

    // Named with the code a caller would match on, so an agent reading its own
    // tool output learns it was denied rather than meeting an unexplained
    // failure and working around it.
    eprintln!(
        "arsox: PERMISSION_COMMAND_DENIED: `{}` {reason}",
        denial.invocation()
    );

    std::process::exit(DENIED);
}

/// Reads the thread's policy from beside the shim.
fn read_policy(invocation: &Invocation) -> Option<Policy> {
    let body = std::fs::read(invocation.policy_path()?).ok()?;

    serde_json::from_slice(&body).ok()
}

/// Finds the real binary for `name`, in the order the policy recorded.
///
/// Searched rather than inherited from `PATH`, because a gate runs with the
/// agent's `PATH`, which on a brokered thread is the shim directory and nothing
/// else. A shim that searched its own `PATH` would find itself, and the
/// `pre-push` hook needs a `git` that is not a shim it would first have to be
/// allowed to run.
pub(super) fn resolve(search_path: &[PathBuf], name: &str) -> Option<PathBuf> {
    search_path
        .iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Replaces this process with the real binary, and never returns.
///
/// `arg0` is set to the name the command was invoked by rather than to the
/// resolved path, because a program that reads its own `argv[0]` expects the
/// former, and a few decide what to be from it.
#[cfg(unix)]
fn launch(binary: &Path, name: &str, arguments: &[String]) -> ! {
    use std::os::unix::process::CommandExt as _;

    let error = std::process::Command::new(binary)
        .arg0(name)
        .args(arguments)
        .exec();

    // `exec` only returns when it failed, and it returns the reason.
    eprintln!("arsox: could not run `{name}`: {error}");
    std::process::exit(BROKEN);
}

/// The same, on a platform with no `exec`.
///
/// Unreachable in practice: a satellite installs shims only where it can
/// separate privilege, and Windows is not being asked to. The arm exists so the
/// module compiles and is read on the machine it is developed on.
#[cfg(not(unix))]
fn launch(_binary: &Path, name: &str, _arguments: &[String]) -> ! {
    eprintln!("arsox: exec shims need a Unix host, so `{name}` was not run");
    std::process::exit(BROKEN);
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::settings::v1::{ExecAccess, Permissions};

    /// The argv the kernel builds for `git status` through a shim.
    fn kernel_argv(words: &[&str]) -> Vec<String> {
        words.iter().copied().map(str::to_owned).collect()
    }

    #[test]
    fn a_shim_reads_its_command_and_its_thread_from_the_path_the_kernel_gave_it() {
        // The whole reason these are shebang scripts. `argv[0]` here is the
        // satellite's own path and says nothing; the script path says both.
        let invocation = invoked_as(&kernel_argv(&[
            "/usr/local/bin/arsox-satellite",
            SHIM_FLAG,
            "/opt/arsox/threads/019fd32f-a25f-7611-a4fe-c93cc2a6d782/bin/git",
            "status",
            "--short",
        ]))
        .expect("should be recognized as a shim invocation");

        assert_eq!(invocation.name(), "git");
        assert_eq!(invocation.arguments, ["status", "--short"]);
        assert_eq!(invocation.argv(), ["git", "status", "--short"]);
        assert_eq!(
            invocation.policy_path().expect("beside the shim directory"),
            PathBuf::from("/opt/arsox/threads/019fd32f-a25f-7611-a4fe-c93cc2a6d782/policy.json")
        );
    }

    #[test]
    fn a_shim_with_no_arguments_is_still_a_shim() {
        let invocation = invoked_as(&kernel_argv(&[
            "/usr/local/bin/arsox-satellite",
            SHIM_FLAG,
            "/opt/arsox/threads/x/bin/node",
        ]))
        .expect("should be recognized");

        assert_eq!(invocation.name(), "node");
        assert!(invocation.arguments.is_empty());
    }

    #[test]
    fn an_ordinary_boot_is_not_a_shim_invocation() {
        // The satellite is its own shim interpreter, so this is what keeps a
        // boot, a health check, and a shim apart.
        assert_eq!(invoked_as(&kernel_argv(&["arsox-satellite"])), None);
        assert_eq!(
            invoked_as(&kernel_argv(&["arsox-satellite", "--health-check"])),
            None
        );
        // The flag in any position but the kernel's is not one.
        assert_eq!(
            invoked_as(&kernel_argv(&["arsox-satellite", "serve", SHIM_FLAG])),
            None
        );
        // And the flag with nothing after it names no shim to run.
        assert_eq!(
            invoked_as(&kernel_argv(&["arsox-satellite", SHIM_FLAG])),
            None
        );
    }

    /// The policy a verdict test decides against.
    fn policy(exec: ExecAccess, allowed: &[&str]) -> Policy {
        Policy::for_thread(
            "019fd32f-a25f-7611-a4fe-c93cc2a6d782",
            Some(&Permissions {
                exec: exec.into(),
                allowed_commands: allowed.iter().copied().map(str::to_owned).collect(),
                ..Permissions::default()
            }),
            false,
            crate::broker::Locations {
                spool: PathBuf::from("/opt/arsox/threads/x/denied"),
                scan: PathBuf::from("/opt/arsox/threads/x/scan.sock"),
                search_path: vec![PathBuf::from("/usr/bin")],
            },
        )
        .expect("these policies are brokered")
    }

    #[test]
    fn an_allowed_command_is_run_and_anything_else_is_refused() {
        let policy = policy(ExecAccess::Custom, &["yarn install"]);

        assert_eq!(
            verdict_for(&policy, "yarn", &kernel_argv(&["install"])),
            Verdict::Allow
        );
        assert!(matches!(
            verdict_for(&policy, "docker", &kernel_argv(&["build", "."])),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            verdict_for(&policy, "yarn", &kernel_argv(&["publish"])),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn the_refusal_reason_completes_a_sentence_about_the_command() {
        // It is printed as "`yarn publish` <reason>" for the agent to read, so
        // a reason that did not read as a predicate would be a garbled line in
        // the one place an agent learns why it was stopped.
        let Verdict::Deny(reason) = verdict_for(
            &policy(ExecAccess::None, &[]),
            "docker",
            &kernel_argv(&["build"]),
        ) else {
            panic!("a thread with no commands refuses docker");
        };

        assert!(reason.starts_with("is "), "{reason}");
    }

    #[test]
    fn a_binary_is_resolved_from_the_policy_rather_than_from_the_agents_path() {
        // The shim runs with the agent's PATH, which is the shim directory
        // alone, so a shim that searched it would find itself and exec forever.
        let directory = std::env::temp_dir().join(format!("arsox-shim-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&directory).expect("should create");
        std::fs::write(directory.join("thing"), b"pretend binary").expect("should write");

        assert_eq!(
            resolve(std::slice::from_ref(&directory), "thing"),
            Some(directory.join("thing"))
        );
        assert_eq!(resolve(std::slice::from_ref(&directory), "absent"), None);
        assert_eq!(resolve(&[], "thing"), None);

        drop(std::fs::remove_dir_all(&directory));
    }
}
