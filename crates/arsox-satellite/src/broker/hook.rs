// Copyright © 2026 Jalapeno Labs

//! The other end of a `pre-push` hook: the satellite, run by git.
//!
//! A thread with something to enforce about pushing gets a hooks directory
//! holding one file:
//!
//! ```text
//! #!/usr/local/bin/arsox-satellite --pre-push
//! ```
//!
//! `core.hooksPath` points every checkout at that directory, so `git push`,
//! typed by an agent, execs the satellite. It learns which thread it belongs to
//! the same way an exec shim does: from the path of the file the kernel ran,
//! with the policy sitting beside the directory it came out of.
//!
//! # What git tells it, and when
//!
//! On argv: the remote's name and its URL. On stdin: one line per ref, carrying
//! the local and remote shas. The remote sha is the fact that makes the ordering
//! matter, because git can only know it by asking the remote first.
//!
//! **So a push authenticates, then runs this, then sends anything.** Credentials
//! are acquired during ref discovery, before the hook; objects go out during the
//! upload, after it. The hook is therefore a complete gate on content leaving
//! the satellite, and is not a gate on a credential being released. See the
//! [enforcement doc](../../../../docs/enforcement.md) for what that means for
//! the credential helper the README describes.
//!
//! # It fails closed
//!
//! A policy that cannot be read, stdin that is not the format git documents, a
//! `git` that cannot be resolved, a satellite that does not answer a scan: every
//! one of them refuses the push. A scan that did not happen is not a scan that
//! found nothing.
//!
//! # It is a gate, not a jail
//!
//! The hook runs as the agent, because the push does. An agent that sets out to
//! get around it can pass `--no-verify`, point `core.hooksPath` somewhere else
//! on its own command line, or clone the repository somewhere the config does
//! not reach. What the gate holds against is the ordinary path: every push an
//! agent makes without deliberately disabling the check, which is every push an
//! agent makes. The limits are stated in full in the enforcement doc, because a
//! control whose limits are unwritten is one somebody will over-trust.

use super::spool::Kind;
use super::{POLICY_FILE, Policy, push, scan};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The argument a `pre-push` hook's shebang line carries.
pub const HOOK_FLAG: &str = "--pre-push";

/// The name git looks for in the hooks directory.
pub const PRE_PUSH: &str = "pre-push";

/// Exit code that lets the push proceed.
const ALLOWED: i32 = 0;

/// Exit code for a push the policy refused.
///
/// git aborts on anything nonzero and reports nothing of its own, so the code
/// itself is not read by anybody. The line printed above it is.
const REFUSED: i32 = 1;

/// Exit code for a hook that could not decide.
///
/// The same code an exec shim uses for the same condition, because it means the
/// same thing: a satellite fault rather than a permission decision, and an
/// operator seeing it should look at the broker directory rather than at the
/// thread's settings.
const BROKEN: i32 = 125;

/// Most stdin a push may hand the hook.
///
/// A megabyte is roughly ten thousand ref lines, which is far past any real
/// push. Input beyond it is refused rather than truncated: a truncated read is
/// a protected ref that never reaches the check.
const MOST_UPDATES: u64 = 1024 * 1024;

/// One push, as git handed it to the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The hook file that was executed, which names the thread.
    pub hook: PathBuf,

    /// The remote's name, or the URL when the push named no remote.
    pub remote: String,
}

impl Invocation {
    /// Where the thread's policy sits, relative to this hook.
    ///
    /// Beside the hooks directory rather than inside it, so the policy is not
    /// itself a file git would try to run as a hook.
    #[must_use]
    pub fn policy_path(&self) -> Option<PathBuf> {
        Some(self.hook.parent()?.parent()?.join(POLICY_FILE))
    }
}

/// Whether this process was started as a `pre-push` hook, and for which thread.
///
/// The kernel builds the interpreter's argv as the interpreter path, the
/// shebang's own argument, the script path, and then git's own arguments: the
/// remote name and the remote URL.
///
/// **The URL is deliberately dropped here.** A caller may push to a URL with a
/// credential written into it, and everything this returns can reach an
/// incident that outlives the thread. The remote's name says which remote
/// without carrying anything worth hiding.
///
/// Pure, so what the kernel does can be asserted without asking it to do it.
#[must_use]
pub fn invoked_as(arguments: &[String]) -> Option<Invocation> {
    if arguments.get(1).map(String::as_str) != Some(HOOK_FLAG) {
        return None;
    }

    Some(Invocation {
        hook: PathBuf::from(arguments.get(2)?),
        remote: arguments.get(3).cloned().unwrap_or_default(),
    })
}

/// Runs the hook to its end, and never returns.
///
/// # Panics
///
/// Never. It exits.
pub fn run(invocation: &Invocation) -> ! {
    let Some(policy) = read_policy(invocation) else {
        // Failing closed. A hook that allowed the push because it could not
        // find its own policy would be a gate that opens when it breaks.
        eprintln!(
            "arsox: this pre-push hook could not read its thread's policy, so nothing was pushed"
        );
        std::process::exit(BROKEN);
    };

    let Some(gate) = policy.push.clone() else {
        // A hook left behind by settings that have since dropped the gate. The
        // policy is the truth about what a thread enforces, and this one
        // enforces nothing about pushing.
        std::process::exit(ALLOWED);
    };

    let Some(updates) = read_updates().and_then(|written| push::updates_from(&written)) else {
        eprintln!(
            "arsox: this pre-push hook could not read what git was about to push, \
             so nothing was pushed"
        );
        std::process::exit(BROKEN);
    };

    match push::verdict_for(&gate, &updates) {
        push::Verdict::PushDenied => refuse(
            &policy,
            invocation,
            &updates,
            Kind::PushDenied,
            "is refused because pushing is disabled for this thread",
            BTreeMap::new(),
        ),
        push::Verdict::BranchProtected(reference) => {
            let mut details = BTreeMap::new();
            details.insert("ref".to_owned(), reference.clone());

            refuse(
                &policy,
                invocation,
                &updates,
                Kind::BranchProtected,
                &format!("is refused because `{reference}` is protected on this thread"),
                details,
            )
        }
        push::Verdict::Permitted(ranges) => {
            scan_or_refuse(&policy, &gate, invocation, &updates, &ranges)
        }
    }
}

/// Scans what the push would send, and refuses it if a secret is in it.
fn scan_or_refuse(
    policy: &Policy,
    gate: &push::PushPolicy,
    invocation: &Invocation,
    updates: &[push::RefUpdate],
    ranges: &[push::ScanRange],
) -> ! {
    // A thread with no secrets has nothing to scan for, and a push that only
    // deletes refs sends nothing to scan.
    let Some(socket) = gate.scan.as_deref().filter(|_asked| !ranges.is_empty()) else {
        std::process::exit(ALLOWED);
    };

    // By path rather than by name. The hook runs with the agent's `PATH`, which
    // on a brokered thread is the shim directory, so resolving `git` by name
    // would put the scan behind the very allowlist the thread may have refused
    // it in.
    let Some(git) = super::shim::resolve(&policy.search_path, "git") else {
        eprintln!(
            "arsox: this satellite has no `git` to read the push with, so nothing was pushed"
        );
        std::process::exit(BROKEN);
    };

    match scan::ask(socket, &git, ranges) {
        Ok(scan::Outcome::Clean) => std::process::exit(ALLOWED),
        Ok(scan::Outcome::SecretFound) => refuse(
            policy,
            invocation,
            updates,
            Kind::SecretInPush,
            "is refused because it carries one of this thread's secrets",
            BTreeMap::new(),
        ),
        Err(error) => {
            eprintln!(
                "arsox: this push could not be scanned for secrets, so nothing was pushed: {error}"
            );
            std::process::exit(BROKEN);
        }
    }
}

/// Refuses a push, recording it before saying so, and never returns.
///
/// The record goes down first, exactly as it does for a refused command: a
/// refusal the operator never learns about is the silent failure the incident
/// system exists to prevent, and the agent is going to be told either way.
fn refuse(
    policy: &Policy,
    invocation: &Invocation,
    updates: &[push::RefUpdate],
    kind: Kind,
    reason: &str,
    details: BTreeMap<String, String>,
) -> ! {
    let denial = super::spool::Denial::now(
        &policy.thread_id,
        kind,
        "git",
        argv_for(invocation, updates),
        reason,
    )
    .with_details(details);

    if let Err(error) = super::spool::record(&policy.spool, &denial) {
        eprintln!("arsox: could not record this refusal for the satellite: {error}");
    }

    // Named with the code a caller would match on, so an agent reading its own
    // tool output learns which gate closed rather than meeting an unexplained
    // failure and working around it.
    eprintln!("arsox: {}: `{}` {reason}", kind.name(), denial.invocation());

    std::process::exit(REFUSED);
}

/// The push as a command line, for the incident's `details.argv`.
///
/// **Reconstructed rather than observed.** git tells a hook the remote and the
/// refs and never the argv the agent typed, so this is what the push does
/// rather than how it was spelled. It is the form an operator widening a policy
/// needs, and the form the exec broker's own refusals already take.
fn argv_for(invocation: &Invocation, updates: &[push::RefUpdate]) -> Vec<String> {
    let mut argv = vec!["git".to_owned(), "push".to_owned()];

    if !invocation.remote.is_empty() {
        argv.push(invocation.remote.clone());
    }

    argv.extend(updates.iter().map(|update| update.remote_ref.clone()));

    argv
}

/// Reads the thread's policy from beside the hook.
fn read_policy(invocation: &Invocation) -> Option<Policy> {
    let body = std::fs::read(invocation.policy_path()?).ok()?;

    serde_json::from_slice(&body).ok()
}

/// Reads what git wrote to the hook's stdin, or `None` for more than it should.
///
/// Bounded because this is the one thing a hook is handed that it did not ask
/// for the size of. Refused rather than truncated: a truncated read is a
/// protected ref that never reaches the check.
fn read_updates() -> Option<String> {
    use std::io::Read as _;

    let mut written = String::new();
    let read = std::io::stdin()
        .lock()
        .take(MOST_UPDATES + 1)
        .read_to_string(&mut written)
        .ok()?;

    (u64::try_from(read).ok()? <= MOST_UPDATES).then_some(written)
}

/// Where a hook file lives for a thread whose broker directory is `thread_root`.
///
/// One definition, so the installer and anything reasoning about a hook agree
/// on where it is.
#[must_use]
pub fn path_in(hooks: &Path) -> PathBuf {
    hooks.join(PRE_PUSH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv the kernel builds for a hook git ran.
    fn kernel_argv(words: &[&str]) -> Vec<String> {
        words.iter().copied().map(str::to_owned).collect()
    }

    /// One ref update of `refs/heads/topic`.
    fn update(reference: &str) -> push::RefUpdate {
        push::RefUpdate {
            local_ref: reference.to_owned(),
            local_sha: "a".repeat(40),
            remote_ref: reference.to_owned(),
            remote_sha: "b".repeat(40),
        }
    }

    #[test]
    fn a_hook_reads_its_thread_from_the_path_the_kernel_gave_it() {
        // The whole reason this is a shebang script rather than a symlink, and
        // the same reason an exec shim is one.
        let invocation = invoked_as(&kernel_argv(&[
            "/usr/local/bin/arsox-satellite",
            HOOK_FLAG,
            "/opt/arsox/threads/019fd32f-a25f-7611-a4fe-c93cc2a6d782/hooks/pre-push",
            "origin",
            "https://github.com/acme/api.git",
        ]))
        .expect("should be recognized as a hook invocation");

        assert_eq!(invocation.remote, "origin");
        assert_eq!(
            invocation
                .policy_path()
                .expect("beside the hooks directory"),
            PathBuf::from("/opt/arsox/threads/019fd32f-a25f-7611-a4fe-c93cc2a6d782/policy.json")
        );
    }

    #[test]
    fn the_remote_url_never_leaves_the_invocation() {
        // A caller may push to `https://user:token@host/repo.git`, and
        // everything an invocation carries can reach an incident that outlives
        // the thread.
        let invocation = invoked_as(&kernel_argv(&[
            "/usr/local/bin/arsox-satellite",
            HOOK_FLAG,
            "/opt/arsox/threads/x/hooks/pre-push",
            "origin",
            "https://user:ghp_secret@github.com/acme/api.git",
        ]))
        .expect("should be recognized");

        let rendered = format!("{invocation:?}");
        assert!(!rendered.contains("ghp_secret"), "{rendered}");
    }

    #[test]
    fn an_ordinary_boot_is_not_a_hook_invocation() {
        assert_eq!(invoked_as(&kernel_argv(&["arsox-satellite"])), None);
        assert_eq!(
            invoked_as(&kernel_argv(&["arsox-satellite", "--health-check"])),
            None
        );
        assert_eq!(
            invoked_as(&kernel_argv(&[
                "arsox-satellite",
                super::super::shim::SHIM_FLAG,
                "/x/bin/git"
            ])),
            None
        );
        // And the flag with nothing after it names no hook.
        assert_eq!(
            invoked_as(&kernel_argv(&["arsox-satellite", HOOK_FLAG])),
            None
        );
    }

    #[test]
    fn a_refusal_reports_what_the_push_did_rather_than_how_it_was_spelled() {
        // git never tells a hook the argv the agent typed, so the incident
        // carries the push as an operator would need to read it.
        let invocation = Invocation {
            hook: PathBuf::from("/opt/arsox/threads/x/hooks/pre-push"),
            remote: "origin".to_owned(),
        };

        assert_eq!(
            argv_for(
                &invocation,
                &[update("refs/heads/main"), update("refs/heads/topic")]
            ),
            [
                "git",
                "push",
                "origin",
                "refs/heads/main",
                "refs/heads/topic"
            ]
        );
    }

    #[test]
    fn a_push_that_named_no_remote_still_reads_as_a_push() {
        let invocation = Invocation {
            hook: PathBuf::from("/opt/arsox/threads/x/hooks/pre-push"),
            remote: String::new(),
        };

        assert_eq!(
            argv_for(&invocation, &[update("refs/heads/main")]),
            ["git", "push", "refs/heads/main"]
        );
    }

    #[test]
    fn a_hook_at_the_root_of_the_filesystem_names_no_policy() {
        // Not reachable from an installed hook, and the arm has to exist
        // because the path it walks up is the one thing here that came from
        // outside.
        assert_eq!(
            Invocation {
                hook: PathBuf::from("pre-push"),
                remote: String::new(),
            }
            .policy_path(),
            None
        );
    }
}
