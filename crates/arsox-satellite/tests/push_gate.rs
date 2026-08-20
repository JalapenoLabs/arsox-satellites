// Copyright © 2026 Jalapeno Labs

//! The `pre-push` hook, driven the way git drives it.
//!
//! git runs the hook as a program with the remote on argv and one line per ref
//! on stdin, so that is exactly how these tests run it: the satellite binary,
//! invoked in hook mode against a broker directory built in a temporary
//! directory, with the refspecs written to its stdin.
//!
//! # What a temporary directory stands in for
//!
//! In the image the broker directory is root-owned and the agent cannot write
//! it. Ownership is not what these tests are about and is not what the hook
//! reads: the hook reads a policy file and writes a refusal into a spool, and
//! both behave identically whoever owns them. So the whole mechanism runs on an
//! ordinary CI runner, and only the privilege underneath it waits for the
//! container. See `docs/enforcement.md` for what remains proven only there.

use arsox_satellite::broker::Policy;
use arsox_satellite::broker::push::PushPolicy;
use arsox_satellite::broker::spool::{self, Kind};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A thread id shaped like the ones the satellite generates.
const THREAD: &str = "019fd32f-a25f-7611-a4fe-c93cc2a6d782";

/// Forty zeros: git's "this ref does not exist".
const ABSENT: &str = "0000000000000000000000000000000000000000";

/// Shas shaped like git's, standing in for commits nothing here creates.
const LOCAL: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
const REMOTE: &str = "9876543210fedcba9876543210fedcba98765432";

/// One thread's broker directory, with a `pre-push` hook installed in it.
struct Gate {
    root: PathBuf,
}

impl Gate {
    /// Builds the directory a hook reads its thread from.
    fn new() -> Self {
        let gate = Self {
            root: std::env::temp_dir().join(format!("arsox-gate-{}", uuid::Uuid::now_v7())),
        };

        std::fs::create_dir_all(gate.root.join("hooks"))
            .expect("should create the hooks directory");
        std::fs::create_dir_all(gate.spool()).expect("should create the spool");

        // Written as the installer writes it, shebang and all. Nothing here runs
        // it through the shebang: these tests invoke the satellite the way the
        // kernel would rather than asking a kernel to.
        std::fs::write(
            gate.hook(),
            format!("#!{} --pre-push\n", satellite().display()),
        )
        .expect("should write the hook");

        gate
    }

    /// A gate whose thread has exactly this push policy.
    fn gating(push: PushPolicy) -> Self {
        let gate = Self::new();
        gate.declare(push);
        gate
    }

    /// Writes the policy the hook will read.
    fn declare(&self, push: PushPolicy) {
        let policy = Policy {
            thread_id: THREAD.to_owned(),
            exec: None,
            push: Some(push),
            search_path: search_path(),
            spool: self.spool(),
        };

        std::fs::write(
            self.root.join("policy.json"),
            serde_json::to_vec(&policy).expect("a policy serializes"),
        )
        .expect("should write the policy");
    }

    fn hook(&self) -> PathBuf {
        self.root.join("hooks").join("pre-push")
    }

    fn spool(&self) -> PathBuf {
        self.root.join("denied")
    }

    /// Runs the hook over one push, from `working_dir`.
    ///
    /// git runs a hook from the top of the working tree, which is what lets the
    /// hook read the outgoing commits without being told where they are.
    fn push_from(&self, working_dir: &Path, updates: &str) -> Output {
        let mut hook = Command::new(satellite())
            .arg("--pre-push")
            .arg(self.hook())
            .arg("origin")
            .arg("https://example.test/acme/api.git")
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("should launch the satellite in hook mode");

        hook.stdin
            .take()
            .expect("stdin was piped")
            .write_all(updates.as_bytes())
            .expect("should write the refspecs");

        hook.wait_with_output().expect("the hook should finish")
    }

    /// Runs the hook over one push, from nowhere in particular.
    ///
    /// For the refusals decided before anything is read, which need no
    /// repository at all.
    fn push(&self, updates: &str) -> Output {
        self.push_from(&self.root, updates)
    }

    /// Every refusal the hook recorded, oldest first.
    fn refusals(&self) -> Vec<spool::Denial> {
        spool::drain(&self.spool())
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.root));
    }
}

/// The satellite binary this test run built.
fn satellite() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_arsox-satellite"))
}

/// Where the hook should look for a real `git`.
///
/// This process's own `PATH`, which is what the broker records for a thread.
fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

/// What the hook said, for an assertion that fails readably.
fn said(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One update line, as git writes it.
fn update(reference: &str, local: &str, remote: &str) -> String {
    format!("{reference} {local} {reference} {remote}\n")
}

/// A policy that gates on pushing and scans nothing.
fn unscanned(allowed: bool, protected: &[&str]) -> PushPolicy {
    PushPolicy {
        allowed,
        protected: protected.iter().copied().map(str::to_owned).collect(),
        scan: None,
    }
}

#[test]
fn a_thread_that_denies_pushing_is_refused_before_git_sends_anything() {
    let gate = Gate::gating(unscanned(false, &[]));

    let output = gate.push(&update("refs/heads/topic", LOCAL, REMOTE));

    assert!(!output.status.success(), "{}", said(&output));
    assert!(
        said(&output).contains("PERMISSION_PUSH_DENIED"),
        "an agent reads its own tool output to learn which gate closed: {}",
        said(&output)
    );

    let refusals = gate.refusals();
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].kind, Kind::PushDenied);
    assert_eq!(refusals[0].thread_id, THREAD);
    assert_eq!(
        refusals[0].argv,
        ["git", "push", "origin", "refs/heads/topic"],
        "the incident carries the push an operator would need to read"
    );
}

#[test]
fn a_protected_ref_is_refused_and_the_refusal_names_it() {
    // `details.ref` is what the README promises this code carries.
    let gate = Gate::gating(unscanned(true, &["main"]));

    let output = gate.push(&format!(
        "{}{}",
        update("refs/heads/topic", LOCAL, REMOTE),
        update("refs/heads/main", LOCAL, REMOTE)
    ));

    assert!(!output.status.success(), "{}", said(&output));
    assert!(
        said(&output).contains("PERMISSION_BRANCH_PROTECTED"),
        "{}",
        said(&output)
    );

    let refusals = gate.refusals();
    assert_eq!(refusals[0].kind, Kind::BranchProtected);
    assert_eq!(
        refusals[0].details.get("ref").map(String::as_str),
        Some("refs/heads/main")
    );
}

#[test]
fn deleting_a_protected_branch_is_refused_like_writing_to_one() {
    let gate = Gate::gating(unscanned(true, &["refs/heads/main"]));

    let output = gate.push(&update("refs/heads/main", ABSENT, REMOTE));

    assert!(!output.status.success(), "{}", said(&output));
    assert_eq!(gate.refusals()[0].kind, Kind::BranchProtected);
}

#[test]
fn a_push_the_policy_permits_is_let_through_and_recorded_nowhere() {
    // A gate that refused what it permits would be noticed immediately. A gate
    // that recorded a refusal it never made would not be, and would fill an
    // operator's incident query with pushes that worked.
    let gate = Gate::gating(unscanned(true, &["main"]));

    let output = gate.push(&update("refs/heads/topic", LOCAL, REMOTE));

    assert!(output.status.success(), "{}", said(&output));
    assert!(gate.refusals().is_empty());
}

#[test]
fn a_hook_that_cannot_read_its_policy_refuses_the_push() {
    // Failing closed. A hook that allowed the push because it could not find
    // its own policy would be a gate that opens when it breaks.
    let gate = Gate::gating(unscanned(false, &[]));
    std::fs::remove_file(gate.root.join("policy.json")).expect("should remove");

    let output = gate.push(&update("refs/heads/topic", LOCAL, REMOTE));

    assert_eq!(output.status.code(), Some(125), "{}", said(&output));
    assert!(said(&output).contains("policy"), "{}", said(&output));
}

#[test]
fn input_that_is_not_the_documented_format_refuses_rather_than_guesses() {
    // A misread ref is a protected branch that looks unprotected.
    let gate = Gate::gating(unscanned(true, &["main"]));

    let output = gate.push("refs/heads/main\n");

    assert_eq!(output.status.code(), Some(125), "{}", said(&output));
    assert!(gate.refusals().is_empty(), "nothing was decided to record");
}

/// The half that needs a unix socket and a real repository.
///
/// The satellite answers a scan over a socket and the hook reads the outgoing
/// commits with git, so this is where a satellite that enforces anything runs.
#[cfg(unix)]
mod scanned {
    use super::{ABSENT, Gate, PushPolicy, said, update};
    use arsox_satellite::broker::spool::Kind;
    use arsox_satellite::broker::{Policy, scan};
    use arsox_satellite::redaction::Redactor;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The secret the scanned push carries.
    const SECRET: &str = "ghp_the_real_token_for_this_thread";

    /// A gate that scans pushes, and the satellite answering it.
    fn scanning() -> (Gate, scan::Scanner) {
        let gate = Gate::new();
        let socket = gate.root.join("scan.sock");

        gate.declare(PushPolicy {
            allowed: true,
            protected: Vec::new(),
            scan: Some(socket.clone()),
        });

        let scanner =
            scan::Scanner::bind(socket, Redactor::for_values(vec![SECRET.to_owned()], None))
                .expect("should start answering scans");

        (gate, scanner)
    }

    /// Builds a repository whose single commit contains `contents`.
    ///
    /// Returns its path and the sha of that commit. The identity is set on the
    /// command line rather than read from a config, so the test does not depend
    /// on whoever runs it having one.
    fn repository(contents: &str) -> (PathBuf, String) {
        let path = std::env::temp_dir().join(format!("arsox-push-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("should create the repository");

        git(&path, &["init", "--quiet", "--initial-branch", "main"]);
        std::fs::write(path.join("config.txt"), contents).expect("should write");
        git(&path, &["add", "config.txt"]);
        git(
            &path,
            &[
                "-c",
                "user.name=Arsox Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "--message",
                "add the config",
            ],
        );

        let sha = git(&path, &["rev-parse", "HEAD"]);

        (path, sha)
    }

    /// Runs one git command in `at` and returns its trimmed stdout.
    fn git(at: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(at)
            .output()
            .expect("the test host has git");

        assert!(
            output.status.success(),
            "git {arguments:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// The satellite's own policy shape, for a test that has to rewrite one.
    fn policy_of(gate: &Gate) -> Policy {
        serde_json::from_slice(
            &std::fs::read(gate.root.join("policy.json")).expect("should read the policy"),
        )
        .expect("should parse the policy")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_carrying_a_secret_is_refused_and_a_clean_one_is_not() {
        let (gate, _scanner) = scanning();

        let (leaking, leaked) = repository(&format!("token = {SECRET}\n"));
        let refused = tokio::task::block_in_place(|| {
            gate.push_from(&leaking, &update("refs/heads/main", &leaked, ABSENT))
        });

        assert!(!refused.status.success(), "{}", said(&refused));
        assert!(
            said(&refused).contains("SECRET_IN_PUSH_BLOCKED"),
            "{}",
            said(&refused)
        );
        assert_eq!(gate.refusals()[0].kind, Kind::SecretInPush);

        let (clean, unremarkable) = repository("token = read it from the environment\n");
        let allowed = tokio::task::block_in_place(|| {
            gate.push_from(&clean, &update("refs/heads/main", &unremarkable, ABSENT))
        });

        assert!(allowed.status.success(), "{}", said(&allowed));
        assert!(gate.refusals().is_empty());

        drop(std::fs::remove_dir_all(&leaking));
        drop(std::fs::remove_dir_all(&clean));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_that_could_not_be_scanned_is_refused_rather_than_allowed() {
        // A scan that did not happen is not a scan that found nothing. Dropping
        // the scanner is what a satellite restarting mid turn looks like from
        // inside a hook.
        let (gate, scanner) = scanning();
        drop(scanner);

        let (working, sha) = repository("nothing secret here\n");
        let output = tokio::task::block_in_place(|| {
            gate.push_from(&working, &update("refs/heads/main", &sha, ABSENT))
        });

        assert_eq!(output.status.code(), Some(125), "{}", said(&output));

        drop(std::fs::remove_dir_all(&working));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_satellite_answers_a_scan_without_running_anything() {
        // The protocol in one test: bytes up, one word back. Nothing about it
        // depends on where the bytes came from, which is what lets the satellite
        // hold the secrets without ever touching the agent's repository.
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (gate, _scanner) = scanning();
        let socket = policy_of(&gate)
            .push
            .expect("gated on pushing")
            .scan
            .expect("scanning");

        for (sent, expected) in [
            (format!("+password = {SECRET}\n"), "secret"),
            ("+fn main() {}\n".to_owned(), "clean"),
        ] {
            let mut stream = tokio::net::UnixStream::connect(&socket)
                .await
                .expect("the satellite should be listening");

            stream
                .write_all(sent.as_bytes())
                .await
                .expect("should send the push");
            stream.shutdown().await.expect("should end the input");

            let mut answer = String::new();
            stream
                .read_to_string(&mut answer)
                .await
                .expect("should be answered");

            assert_eq!(answer.trim(), expected);
        }
    }

    /// Every checkout a thread owns is pointed at its hooks, and a thread with
    /// nothing to enforce is pointed at nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_checkout_is_pointed_at_the_hooks_the_agent_cannot_write() {
        let workspace = std::env::temp_dir().join(format!("arsox-hooked-{}", uuid::Uuid::now_v7()));
        let thread = super::THREAD;
        let repos = workspace.join(thread).join("repos");
        std::fs::create_dir_all(&repos).expect("should create the workspace");

        let (checkout, _sha) = repository("ordinary\n");
        let moved = repos.join("api");
        std::fs::rename(&checkout, &moved).expect("should move the checkout into the workspace");

        let hooks = PathBuf::from("/opt/arsox/threads")
            .join(thread)
            .join("hooks");
        let unpointed =
            arsox_satellite::workspace::point_hooks_at(&workspace, thread, &hooks).await;

        assert!(unpointed.is_empty(), "{unpointed:?}");
        assert_eq!(
            git(&moved, &["config", "core.hooksPath"]),
            hooks.display().to_string()
        );

        drop(std::fs::remove_dir_all(&workspace));
    }
}
