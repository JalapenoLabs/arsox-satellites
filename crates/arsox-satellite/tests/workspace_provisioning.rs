// Copyright © 2026 Jalapeno Labs

//! Workspace provisioning, driven against real git repositories.
//!
//! Every remote here is a `file://` URL pointing at a repository this test
//! created moments earlier, so the whole suite runs with no network and no
//! credentials. What is local is the remote, not the plumbing: git really
//! clones, setup commands really run in the checkout, and the incidents really
//! land in the database.

use arsox_satellite::store::{NewThread, NewTurn, Store};
use arsox_satellite::workspace::{Provisioner, provision_repos};
use arsox_sdk::proto::common::v1::Secret;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::settings::v1::{GitAuth, Repo, ThreadSettings, git_auth::Credential};
use arsox_sdk::proto::thread::v1::ThreadState;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A scratch directory, since every one of these writes real files.
mod scratch {
    pub struct Dir(std::path::PathBuf);

    impl Dir {
        pub fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("arsox-{label}-{}", unique()));
            std::fs::create_dir_all(&path).expect("should create a scratch directory");
            Self(path)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            // A git checkout on Windows holds read-only pack files, which a
            // plain remove refuses. Best effort either way: a leftover temp
            // directory is noise, not a test failure.
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    /// Distinguishes directories created in the same clock tick.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique() -> u128 {
        let counter = u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();

        now.wrapping_mul(1_000).wrapping_add(counter)
    }
}

/// A thread id shaped the way the satellite generates them.
fn thread_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Runs one git command, insisting it worked.
fn git(working_dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
        .expect("git should be installed");

    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Builds a repository with one commit and returns its `file://` URL.
///
/// Identity and default branch are passed per invocation rather than read from
/// whatever git config the machine happens to carry, so the fixture is the same
/// on a developer's laptop as on a CI runner.
fn origin(directory: &Path, file_name: &str) -> String {
    std::fs::create_dir_all(directory).expect("should create the origin");
    git(directory, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(directory.join(file_name), b"the original content").expect("should write");
    git(directory, &["add", "."]);
    git(
        directory,
        &[
            "-c",
            "user.email=tests@arsox.invalid",
            "-c",
            "user.name=Arsox Tests",
            "commit",
            "-m",
            "first",
        ],
    );

    file_url(directory)
}

fn file_url(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");

    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        // A Windows path begins with a drive letter, which needs the third
        // slash to sit where an empty authority would.
        format!("file:///{text}")
    }
}

fn repo(name: &str, url: &str) -> Repo {
    Repo {
        name: name.to_owned(),
        url: url.to_owned(),
        ..Repo::default()
    }
}

/// Every file in a tree, so a secret can be hunted for rather than assumed
/// absent from the one place it was expected.
fn read_every_file(root: &Path) -> String {
    let mut found = String::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(contents) = std::fs::read(&path) {
                found.push_str(&String::from_utf8_lossy(&contents));
            }
        }
    }

    found
}

#[tokio::test]
async fn a_declared_repo_is_cloned_into_the_thread_workspace() {
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let url = origin(&fixtures.path().join("service"), "README.md");
    let thread = thread_id();

    let report = provision_repos(workspace.path(), &thread, &[repo("api", &url)])
        .await
        .expect("provisioning should not fail on the satellite's side");

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.cloned, vec!["api"]);

    // Named for the repo rather than for the URL it came from, which is what
    // makes the directory addressable from settings and from service names.
    let checkout = workspace.path().join(&thread).join("repos").join("api");
    assert!(checkout.join("README.md").is_file());
    assert!(checkout.join(".git").exists(), "a real clone, not a copy");
}

#[tokio::test]
async fn a_repo_with_no_submodules_clones_cleanly_under_recursion() {
    // Every clone asks for submodules, so the overwhelmingly common repo, the
    // one that has none, has to be the case that cannot break.
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let url = origin(&fixtures.path().join("plain"), "main.rs");
    let thread = thread_id();

    let report = provision_repos(workspace.path(), &thread, &[repo("plain", &url)])
        .await
        .expect("should provision");

    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let checkout = workspace.path().join(&thread).join("repos").join("plain");
    assert!(checkout.join("main.rs").is_file());
    assert!(!checkout.join(".gitmodules").exists());
}

#[tokio::test]
async fn several_repos_each_get_their_own_checkout() {
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let api = origin(&fixtures.path().join("api"), "api.txt");
    let web = origin(&fixtures.path().join("web"), "web.txt");
    let thread = thread_id();

    let report = provision_repos(
        workspace.path(),
        &thread,
        &[repo("api", &api), repo("web", &web)],
    )
    .await
    .expect("should provision");

    assert_eq!(report.cloned, vec!["api", "web"]);

    let repos = workspace.path().join(&thread).join("repos");
    assert!(repos.join("api/api.txt").is_file());
    assert!(repos.join("web/web.txt").is_file());
}

#[tokio::test]
async fn setup_commands_run_in_the_checkout_with_barrier_semantics() {
    // The README's rule, end to end: newlines run together and do not fail
    // fast, a semicolon is a barrier, and nothing below a failed barrier runs.
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let url = origin(&fixtures.path().join("service"), "README.md");
    let thread = thread_id();

    let with_setup = Repo {
        setup_commands: "echo one > one.txt;\n\
                         exit 1\n\
                         echo two > two.txt;\n\
                         echo three > three.txt"
            .to_owned(),
        ..repo("api", &url)
    };

    let report = provision_repos(workspace.path(), &thread, &[with_setup])
        .await
        .expect("should provision");

    let checkout = workspace.path().join(&thread).join("repos").join("api");
    assert!(checkout.join("one.txt").is_file(), "the first barrier ran");
    assert!(
        checkout.join("two.txt").is_file(),
        "a sibling of a failing command still runs: newlines do not fail fast"
    );
    assert!(
        !checkout.join("three.txt").exists(),
        "a failed barrier stops everything below it"
    );

    // A setup failure degrades the workspace rather than ending it: the
    // checkout is there and the thread can still work in it.
    assert!(!report.is_fatal());
    assert_eq!(report.cloned, vec!["api"]);

    let codes: Vec<ErrorCode> = report.failures.iter().map(|failure| failure.code).collect();
    assert_eq!(codes, vec![ErrorCode::RepoSetupFailed; 2]);

    let failed = &report.failures[0];
    assert_eq!(failed.command.as_deref(), Some("exit 1"));
    assert_eq!(failed.exit_code, Some(1));

    // What never ran is reported rather than omitted. "Not run" and "found
    // nothing" are different facts.
    assert!(report.failures[1].message.contains("never ran"));
}

#[tokio::test]
async fn a_repo_that_will_not_clone_ends_provisioning_and_says_it_may_be_retried() {
    let workspace = scratch::Dir::new("workspace");
    let thread = thread_id();
    let missing = file_url(&workspace.path().join("no-such-origin"));

    let report = provision_repos(
        workspace.path(),
        &thread,
        &[repo("api", &missing), repo("web", &missing)],
    )
    .await
    .expect("a refused clone is a reported failure, not a satellite error");

    assert!(report.is_fatal());
    assert!(report.cloned.is_empty());
    // One failure, not two. A workspace the thread cannot work in is settled
    // after the first repo, so the rest are never attempted.
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].code, ErrorCode::RepoCloneFailed);
    assert!(
        report.failures[0].retryable,
        "a clone fails for reasons that pass"
    );
}

#[tokio::test]
async fn a_clone_failure_becomes_an_incident_and_parks_the_thread_with_its_queue() {
    let workspace = scratch::Dir::new("workspace");
    let store = Store::open_in_memory().await.expect("should open");
    let missing = file_url(&workspace.path().join("no-such-origin"));

    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings {
                repos: vec![repo("api", &missing)],
                ..ThreadSettings::default()
            },
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    // A thread with repos opens PROVISIONING, and the claim query holds its
    // queue there rather than starting a turn in a workspace with no repos.
    assert_eq!(thread.state, i32::from(ThreadState::Provisioning));

    store
        .create_turn(NewTurn {
            thread_id: thread.thread_id.clone(),
            prompt: "work".to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
        .expect("work may be queued while a thread provisions");

    assert!(
        store.claim_next_turn().await.expect("claim").is_none(),
        "no turn may run in a workspace that is still being built"
    );

    provisioner(&store, &workspace)
        .provision(&thread.thread_id, &[repo("api", &missing)])
        .await;

    // Paused rather than destroyed: the thread keeps its queue and its
    // settings, so fixing the remote and resuming picks up the waiting turns.
    let parked = store.thread(&thread.thread_id).await.expect("should read");
    assert_eq!(parked.state, i32::from(ThreadState::Paused));
    assert!(
        store.claim_next_turn().await.expect("claim").is_none(),
        "a parked thread still refuses its queue"
    );

    let incidents = store
        .incidents_for_thread(&thread.thread_id)
        .await
        .expect("should read");
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].code, i32::from(ErrorCode::RepoCloneFailed));
    assert_eq!(incidents[0].disposition, i32::from(Disposition::Fatal));
    assert!(incidents[0].retryable);
    // Provisioning happens before any turn exists, so there is none to blame.
    assert_eq!(incidents[0].turn_id, None);

    // And it reached the stream, not only the table. An incident nobody can
    // watch for is half a record.
    let events = store
        .events_after(&thread.thread_id, 0, 100)
        .await
        .expect("should replay");
    let streamed = events
        .iter()
        .find_map(|event| match event.payload.as_ref() {
            Some(Payload::Incident(incident)) => Some(incident),
            _other => None,
        })
        .expect("the failure should be on the thread's stream");
    assert_eq!(streamed.code, i32::from(ErrorCode::RepoCloneFailed));

    // The row carries where the frame landed, so a query result can be located
    // in the stream and a stream frame looked up afterwards.
    assert_eq!(incidents[0].sequence, Some(events[0].sequence));
}

#[tokio::test]
async fn a_provisioned_thread_goes_idle_and_releases_the_turns_that_waited() {
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let store = Store::open_in_memory().await.expect("should open");
    let url = origin(&fixtures.path().join("service"), "README.md");
    let repos = vec![repo("api", &url)];

    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings {
                repos: repos.clone(),
                ..ThreadSettings::default()
            },
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    store
        .create_turn(NewTurn {
            thread_id: thread.thread_id.clone(),
            prompt: "work".to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
        .expect("should queue");

    provisioner(&store, &workspace)
        .provision(&thread.thread_id, &repos)
        .await;

    let ready = store.thread(&thread.thread_id).await.expect("should read");
    assert_eq!(ready.state, i32::from(ThreadState::Idle));
    assert!(
        store.claim_next_turn().await.expect("claim").is_some(),
        "the turn that waited out provisioning should now run"
    );

    assert!(
        store
            .incidents_for_thread(&thread.thread_id)
            .await
            .expect("should read")
            .is_empty(),
        "a clean provision has nothing to report"
    );
}

#[tokio::test]
async fn a_thread_with_no_repos_opens_idle_exactly_as_it_did_before() {
    // Repos are not required to perform any work. A thread that declares none
    // must not pay for a state it has no way to leave.
    let store = Store::open_in_memory().await.expect("should open");

    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings::default(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    assert_eq!(thread.state, i32::from(ThreadState::Idle));
}

#[tokio::test]
async fn a_personal_access_token_never_reaches_the_cloned_repository() {
    // The failure this guards against is a clone that authenticates by having
    // the token written into its remote URL, which puts a live credential in
    // .git/config for the life of the workspace, readable by every agent
    // working in it.
    const TOKEN: &str = "ghp_ThisTokenMustNeverTouchTheDisk";

    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let url = origin(&fixtures.path().join("private"), "README.md");
    let thread = thread_id();

    let authenticated = Repo {
        auth: Some(GitAuth {
            credential: Some(Credential::PersonalAccessToken(Secret {
                value: Some(TOKEN.to_owned()),
                display: None,
            })),
        }),
        ..repo("api", &url)
    };

    let report = provision_repos(workspace.path(), &thread, &[authenticated])
        .await
        .expect("should provision");
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let checkout = workspace.path().join(&thread).join("repos").join("api");
    let config = std::fs::read_to_string(checkout.join(".git/config")).expect("should read");
    assert!(
        !config.contains(TOKEN),
        "the token was written into .git/config: {config}"
    );

    // Not only the file it was most likely to land in. A credential anywhere
    // under the workspace is a credential an agent can read.
    assert!(
        !read_every_file(workspace.path()).contains(TOKEN),
        "the token reached the workspace"
    );
}

#[tokio::test]
async fn a_workspace_left_half_built_by_a_restart_is_rebuilt_rather_than_stranded() {
    // Nothing else ever revisits PROVISIONING, so a thread the satellite
    // stopped halfway through would hold its queue for the life of the process
    // while reporting itself perfectly healthy.
    let fixtures = scratch::Dir::new("origin");
    let workspace = scratch::Dir::new("workspace");
    let store = Store::open_in_memory().await.expect("should open");
    let url = origin(&fixtures.path().join("service"), "README.md");
    let repos = vec![repo("api", &url)];

    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings {
                repos: repos.clone(),
                ..ThreadSettings::default()
            },
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    // Stand in for what a killed process leaves: a checkout of unknown
    // completeness, and a thread nothing is driving.
    let checkout = workspace
        .path()
        .join(&thread.thread_id)
        .join("repos")
        .join("api");
    std::fs::create_dir_all(&checkout).expect("should create");
    std::fs::write(checkout.join("half-written"), b"...").expect("should write");

    provisioner(&store, &workspace).resume_interrupted().await;

    // Spawned rather than awaited, because that is what a boot does.
    let ready = await_state(&store, &thread.thread_id, ThreadState::Idle).await;
    assert!(ready, "the thread should have been rebuilt and released");

    assert!(checkout.join("README.md").is_file(), "a clean clone");
    assert!(
        !checkout.join("half-written").exists(),
        "the half-built subtree goes before the clone is attempted again"
    );
}

/// Waits for a thread to reach a state, or gives up.
async fn await_state(store: &Store, thread_id: &str, expected: ThreadState) -> bool {
    for _attempt in 0..100 {
        if store
            .thread(thread_id)
            .await
            .is_ok_and(|thread| thread.state == i32::from(expected))
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    false
}

/// A provisioner writing into `workspace`, with nothing waiting on its nudge.
fn provisioner(store: &Store, workspace: &scratch::Dir) -> Provisioner {
    Provisioner::new(
        store.clone(),
        PathBuf::from(workspace.path()),
        Arc::new(tokio::sync::Notify::new()),
    )
}
