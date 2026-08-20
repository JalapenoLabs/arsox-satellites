// Copyright © 2026 Jalapeno Labs

//! Building one thread's shim directory on disk.
//!
//! Everything here is owned by the satellite, which inside the image means root,
//! and none of it is writable by the agent. What the modes buy is stated with
//! each one below, because a directory whose permissions are decorative is the
//! whole failure mode this module exists to avoid.

use super::Policy;
use std::io;
use std::path::Path;

/// The shim directory, and the thread directory above it.
///
/// Readable and executable by the agent, writable by nobody else. The agent has
/// to be able to resolve and run a shim; it must never be able to add one.
const READABLE: u32 = 0o755;

/// The deny spool.
///
/// Write and execute without read, plus the sticky bit. Write and execute let
/// the agent create a record; the missing read bit means it cannot list what is
/// there; sticky means it cannot unlink a record it did not create. So an agent
/// can be denied a command and cannot then delete the evidence.
const APPEND_ONLY: u32 = 0o1733;

/// The policy file, which the agent may read and may not write.
const READ_ONLY: u32 = 0o644;

/// Builds `thread_root` into a shim directory for `policy`.
///
/// Idempotent, and cheap when nothing changed: a directory already holding
/// exactly this policy is left alone. Otherwise it is **rebuilt rather than
/// merged**, so a name allowed by the settings a change replaced cannot survive
/// into the settings that replaced them. The spool is left alone either way,
/// because a refusal recorded a moment ago is evidence and not stale state.
///
/// # Errors
///
/// Returns the underlying I/O error when any part of the directory cannot be
/// built, and an error of its own when the satellite binary's path cannot be
/// resolved or carries whitespace. A shebang line is split on whitespace by the
/// kernel, so an interpreter path with a space in it produces shims that fail to
/// launch with no explanation at all.
pub(super) fn install(thread_root: &Path, policy: &Policy) -> io::Result<()> {
    let satellite = std::env::current_exe()?;
    let Some(interpreter) = satellite.to_str() else {
        return Err(io::Error::other(
            "the satellite's own path is not valid UTF-8, so it cannot be a shim interpreter",
        ));
    };

    if interpreter.split_whitespace().count() != 1 {
        return Err(io::Error::other(format!(
            "the satellite is installed at `{interpreter}`, which contains whitespace: \
             the kernel splits a shebang line on whitespace, so no shim could launch it"
        )));
    }

    // Every turn re-asserts its thread's policy, and a rebuild is a file per
    // command name the image carries. A thread whose settings have not moved
    // should not pay for one on every turn it runs.
    if already_installed(thread_root, policy) {
        return Ok(());
    }

    let shims = thread_root.join("bin");

    match std::fs::remove_dir_all(&shims) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    create_directory(thread_root, READABLE)?;
    create_directory(&shims, READABLE)?;
    create_directory(&policy.spool, APPEND_ONLY)?;

    write_file(
        &thread_root.join(super::POLICY_FILE),
        // Pretty rather than compact. It is read by a human debugging what a
        // thread was actually allowed far more often than by the shim, and the
        // shim does not care either way.
        &serde_json::to_string_pretty(policy).map_err(io::Error::other)?,
        READ_ONLY,
    )?;

    // Two lines: the shebang the kernel acts on, and a sentence for whoever
    // opens one of these wondering what it is.
    let shim = format!(
        "#!{interpreter} {}\n# An Arsox exec shim. See docs/enforcement.md.\n",
        super::shim::SHIM_FLAG
    );

    for name in policy.shim_names(&super::image_commands(&policy.search_path)) {
        write_file(&shims.join(name), &shim, READABLE)?;
    }

    Ok(())
}

/// Whether the directory already holds exactly this policy.
///
/// Compared against the policy that was written rather than against a
/// timestamp, so a settings change is what earns a rebuild and nothing else
/// does. A missing shim directory always earns one: the broker root is not a
/// volume, so a container replacement leaves a policy file with nothing beside
/// it, and a thread resumed there must not silently lose its gate.
fn already_installed(thread_root: &Path, policy: &Policy) -> bool {
    thread_root.join("bin").is_dir()
        && std::fs::read(thread_root.join(super::POLICY_FILE))
            .ok()
            .and_then(|body| serde_json::from_slice::<Policy>(&body).ok())
            .is_some_and(|written| &written == policy)
}

/// Creates a directory at exactly `mode`, whatever the umask says.
///
/// Set after creation rather than asked for at creation, because `create_dir`
/// masks the mode it is given and the spool's `0733` would come out `0711` under
/// an ordinary umask. That difference is the whole gate: without the write bit
/// the agent cannot record a refusal, and the denial the incident system exists
/// to surface goes missing.
fn create_directory(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, mode)
}

/// Writes a file at exactly `mode`, replacing whatever was there.
fn write_file(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The same, where the mode bits do not exist.
///
/// Windows has no equivalent of the three gates above, and a satellite there
/// never installs a broker: [`super::Broker::install`] refuses before reaching
/// this, because the deterministic layer needs a privilege separation Windows is
/// not being asked to provide. The arm exists so the code path compiles and can
/// be exercised on a developer machine rather than first meeting CI.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature matches the Unix arm, so callers need no cfg of their own"
)]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

// Unix only, and it needs no root. Ownership is not what this module sets: every
// path here is owned by whoever built it, which in the image is the satellite and
// therefore root. What it sets is the three modes, and those are the same three
// whether a test builds the directory or a satellite does. So the whole mechanism
// runs on an ordinary Linux CI runner, and only the privilege underneath it waits
// for the container.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A directory holding an image's worth of commands, for a test.
    fn image(commands: &[&str]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("arsox-image-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("should create");

        for command in commands {
            std::fs::write(path.join(command), b"pretend binary").expect("should write");
        }

        path
    }

    /// A brokered thread, with an image of exactly `commands` behind it.
    fn thread(allowed: &[&str], commands: &[&str]) -> (std::path::PathBuf, Policy) {
        let root = std::env::temp_dir().join(format!("arsox-broker-{}", uuid::Uuid::now_v7()));

        let policy = Policy {
            thread_id: "019fd32f-a25f-7611-a4fe-c93cc2a6d782".to_owned(),
            allowed: allowed.iter().copied().map(str::to_owned).collect(),
            runtime: vec!["node".to_owned(), "env".to_owned(), "sh".to_owned()],
            search_path: vec![image(commands)],
            spool: root.join("denied"),
        };

        (root, policy)
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("should exist")
            .permissions()
            .mode()
            // The file type bits ride along in the same word and are not
            // permissions.
            & 0o7777
    }

    #[test]
    fn a_shim_directory_carries_the_allowlist_the_floor_and_the_image() {
        // The image half is what makes a denial reportable: a command with no
        // shim is a PATH miss that says "not found" and records nothing.
        let (root, policy) = thread(&["yarn install"], &["git", "docker", "node"]);

        install(&root, &policy).expect("should build the shim directory");

        let shims = root.join("bin");
        for expected in ["yarn", "node", "env", "sh", "git", "docker"] {
            assert!(shims.join(expected).is_file(), "{expected} needs a shim");
        }

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn a_shim_is_a_shebang_pointing_back_at_the_satellite() {
        // This is the whole mechanism. A shim that did not name the satellite
        // as its interpreter would run nothing, and one that did not carry the
        // flag would be read as an ordinary boot.
        let (root, policy) = thread(&[], &["git"]);

        install(&root, &policy).expect("should build");

        let shim = std::fs::read_to_string(root.join("bin/git")).expect("should read");
        let interpreter = std::env::current_exe().expect("a test has a path");

        assert!(shim.starts_with("#!"), "{shim}");
        assert!(shim.contains(&interpreter.display().to_string()), "{shim}");
        assert!(shim.contains(super::super::shim::SHIM_FLAG), "{shim}");

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn the_policy_sits_beside_the_shim_directory_rather_than_inside_it() {
        // Inside, it would be a name on the agent's own PATH.
        let (root, policy) = thread(&["gh"], &[]);

        install(&root, &policy).expect("should build");

        let written: Policy =
            serde_json::from_slice(&std::fs::read(root.join(super::super::POLICY_FILE)).unwrap())
                .expect("should round trip");

        assert_eq!(written, policy);
        assert!(!root.join("bin").join(super::super::POLICY_FILE).exists());

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn the_spool_takes_a_record_and_gives_nothing_back() {
        // Write and execute without read, plus sticky: the agent may drop a
        // refusal, may not list the directory, and may not unlink a record it
        // did not write. Set after creation because `create_dir` masks the mode
        // it is given, and a umask that took the write bit off would be a
        // refusal nobody ever hears about.
        let (root, policy) = thread(&[], &[]);

        install(&root, &policy).expect("should build");

        assert_eq!(mode_of(&policy.spool), APPEND_ONLY);
        assert_eq!(mode_of(&root.join("bin")), READABLE);
        assert_eq!(mode_of(&root.join("bin/node")), READABLE);
        assert_eq!(
            mode_of(&root.join(super::super::POLICY_FILE)),
            READ_ONLY,
            "the agent reads its policy and never writes it"
        );

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn reinstalling_rebuilds_rather_than_merges() {
        // A name allowed by the settings a restart replaced must not survive
        // into the settings that replaced them.
        let (root, mut policy) = thread(&["docker"], &[]);
        install(&root, &policy).expect("should build");
        assert!(root.join("bin/docker").is_file());

        policy.allowed = vec!["gh".to_owned()];
        install(&root, &policy).expect("should rebuild");

        assert!(root.join("bin/gh").is_file());
        assert!(
            !root.join("bin/docker").is_file(),
            "a shim from the previous settings survived the rebuild"
        );

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn an_unchanged_thread_does_not_pay_for_a_rebuild_every_turn() {
        // Every turn re-asserts its policy, and a rebuild is a file per command
        // name the image carries.
        let (root, policy) = thread(&["gh"], &[]);
        install(&root, &policy).expect("should build");

        let marker = root.join("bin/left-behind-by-the-test");
        std::fs::write(&marker, b"").expect("should write");
        install(&root, &policy).expect("should decline to rebuild");

        assert!(
            marker.is_file(),
            "the directory was rebuilt for a policy that had not changed"
        );

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn a_shim_directory_that_went_missing_is_built_again() {
        // The broker root is not a volume, so a container replacement leaves a
        // policy file with nothing beside it. A thread resumed there must not
        // silently lose its gate.
        let (root, policy) = thread(&["gh"], &[]);
        install(&root, &policy).expect("should build");
        std::fs::remove_dir_all(root.join("bin")).expect("should remove");

        install(&root, &policy).expect("should rebuild");

        assert!(root.join("bin/gh").is_file());

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn a_rebuild_keeps_refusals_nobody_has_read_yet() {
        // A restart mid-turn must not throw away the evidence of what the agent
        // was denied before it.
        let (root, policy) = thread(&[], &[]);
        install(&root, &policy).expect("should build");

        std::fs::write(policy.spool.join("pending.json"), b"{}").expect("should write");
        install(&root, &policy).expect("should rebuild");

        assert!(policy.spool.join("pending.json").is_file());

        drop(std::fs::remove_dir_all(&root));
    }
}
