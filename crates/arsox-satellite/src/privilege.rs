// Copyright © 2026 Jalapeno Labs

//! Who the satellite runs as, and who it hands its children down to.
//!
//! The README's bar for every permission control is enforcement by
//! infrastructure the agent cannot reach, and nothing reaches that bar while the
//! agent and the enforcement point are the same user. So inside the image the
//! satellite starts as `root` and every process it spawns is handed down to the
//! unprivileged `arsox` account before its first instruction runs. What the
//! satellite owns, the agent cannot rewrite; what the agent owns is its own work
//! under `/workspace`.
//!
//! # One drop site, not one per spawn
//!
//! The drop is applied inside [`crate::harness::spawn::scrubbed_command`], which
//! every spawn site already goes through for the credential scrub. A harness, a
//! `git` clone, a repo's setup command, and a checker are all spawned through
//! it, so a new spawn site inherits the drop by construction rather than by
//! somebody remembering to add it.
//!
//! # It is one-way, and it takes the environment with it
//!
//! `uid` and `gid` are applied by the kernel between `fork` and `exec`, so the
//! child has never been privileged and cannot climb back. `HOME`, `USER`, and
//! `LOGNAME` are rewritten at the same moment, because a child left holding
//! `HOME=/root` spends its turn failing to write files it was never allowed to
//! write, in errors that say nothing about why.
//!
//! # Bare metal gets the advisory layer, and is told so
//!
//! A developer does not run a satellite as root, and on a machine where the
//! satellite and the agent are the same user there is nothing to drop to and
//! nothing an agent could not overwrite. Rather than dropping decoratively, the
//! deterministic layer does not engage at all: [`descent`] resolves to
//! [`Descent::Impossible`], every spawn keeps the satellite's own identity, and
//! [`announce`] says so loudly once at boot.
//!
//! See [the enforcement doc](../../../docs/enforcement.md) for the whole model.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The unprivileged account the image creates for agents.
///
/// Matches the `useradd` in the Dockerfile. The two are one fact in two files,
/// and an image that renamed the account would fall back to the advisory layer
/// with a warning rather than silently running agents as root.
pub const AGENT_ACCOUNT: &str = "arsox";

/// Where the account list lives on a Unix system.
#[cfg(unix)]
const PASSWD_PATH: &str = "/etc/passwd";

/// An unprivileged account a spawned child can be handed down to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,

    /// The account's home directory, which becomes the child's `HOME`.
    pub home: PathBuf,
}

/// Whether this satellite can hand a child down, and to whom.
///
/// Resolved once at boot from two facts that do not change while a process runs:
/// its effective uid, and whether the agent account exists. Holding the answer
/// rather than recomputing it means every spawn site agrees, and it means the
/// boot log can state the posture the satellite will actually run under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descent {
    /// Every spawned child runs as this account.
    To(Account),

    /// Nothing to hand down to. The reason completes "the satellite cannot hand
    /// its children down because it ...".
    Impossible(&'static str),
}

impl Descent {
    /// The account children run as, when there is one.
    #[must_use]
    pub fn account(&self) -> Option<&Account> {
        match self {
            Self::To(account) => Some(account),
            Self::Impossible(_reason) => None,
        }
    }

    /// Whether the deterministic layer has the privilege it needs to enforce.
    ///
    /// The exec broker asks this before it installs anything. A shim directory
    /// the agent could delete is not a gate, so a satellite that cannot separate
    /// privilege does not pretend to broker.
    #[must_use]
    pub fn enforces(&self) -> bool {
        matches!(self, Self::To(_))
    }
}

/// Decides the descent from the two facts it depends on.
///
/// Pure, so the decision can be asserted on a platform that has neither a uid of
/// its own worth reading nor an `/etc/passwd` to read. `passwd` is the contents
/// of the account file, absent on a system that has none.
///
/// Both conditions have to hold. Being root without the account means there is
/// nothing to become; having the account without being root means the change
/// would be refused by the kernel at `exec`, which is a turn that fails to
/// launch rather than a turn that runs unprivileged.
#[must_use]
pub fn descent_for(effective_uid: u32, passwd: Option<&str>, account: &str) -> Descent {
    if effective_uid != 0 {
        return Descent::Impossible("is not running as root");
    }

    let Some(passwd) = passwd else {
        return Descent::Impossible("has no account list to resolve the agent account from");
    };

    match account_in(passwd, account) {
        Some(found) => Descent::To(found),
        None => Descent::Impossible("has no agent account to hand a child down to"),
    }
}

/// Finds one account in the contents of an `/etc/passwd` file.
///
/// The format is seven colon-separated fields per line, of which four matter
/// here: the name, the uid, the gid, and the home directory. A line that does
/// not parse is skipped rather than failing the read, because one malformed
/// entry from some unrelated package must not take away the satellite's ability
/// to find its own account.
fn account_in(passwd: &str, name: &str) -> Option<Account> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');

        let found = fields.next()?;
        if found != name {
            return None;
        }

        // Field 2 is the password placeholder, which has been an `x` pointing at
        // the shadow file for thirty years and is of no interest either way.
        let _password = fields.next()?;
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        let _gecos = fields.next()?;
        let home = fields.next()?;

        Some(Account {
            name: found.to_owned(),
            uid,
            gid,
            home: PathBuf::from(home),
        })
    })
}

/// The descent this process runs under, resolved once.
///
/// A process's effective uid does not change, and neither does the account list
/// under it in any way a satellite should react to mid-run, so this is read once
/// and shared. Threading it through every spawn site instead would put the
/// answer in a parameter that a new call site could pass wrongly, on the one
/// path where being wrong means an agent runs as root.
#[must_use]
pub fn descent() -> &'static Descent {
    static RESOLVED: OnceLock<Descent> = OnceLock::new();

    RESOLVED.get_or_init(resolve)
}

#[cfg(unix)]
fn resolve() -> Descent {
    // SAFETY: `geteuid` takes no arguments, touches no memory the caller owns,
    // and cannot fail. It is `unsafe` only because it crosses the FFI boundary.
    let effective_uid = unsafe { libc::geteuid() };

    descent_for(
        effective_uid,
        std::fs::read_to_string(PASSWD_PATH).ok().as_deref(),
        AGENT_ACCOUNT,
    )
}

#[cfg(not(unix))]
fn resolve() -> Descent {
    Descent::Impossible("is not running on a platform with unprivileged accounts")
}

/// States the enforcement posture this satellite boots under.
///
/// Loud rather than incidental. An operator who believes the deterministic
/// controls are holding, on a satellite where they cannot, is worse off than one
/// running with no controls at all, so the case where nothing enforces is a
/// warning that names exactly which condition failed.
pub fn announce() {
    match descent() {
        Descent::To(account) => tracing::info!(
            event.name = "satellite.boot.enforcement_ready",
            agent.account = %account.name,
            agent.uid = account.uid,
            "the satellite is root and hands every child down to {{agent.account}}",
        ),
        Descent::Impossible(reason) => tracing::warn!(
            event.name = "satellite.boot.enforcement_unavailable",
            enforcement.reason = reason,
            "the satellite {{enforcement.reason}}, so permissions are advisory only: \
             agents run as this process's own user and the exec broker does not engage. \
             Run the published image to get the deterministic layer.",
        ),
    }
}

/// Hands a child down to the agent account, where there is one.
///
/// Applied to every process the satellite spawns. A no-op when nothing can be
/// dropped to, which is what keeps a bare-metal run and the test suite working
/// on the same code path the container uses.
#[cfg(unix)]
pub fn hand_down(command: &mut tokio::process::Command) {
    let Some(account) = descent().account() else {
        return;
    };

    // The group first. Dropping the uid first would leave the process
    // unprivileged and unable to set its groups, which is the classic ordering
    // bug in this operation. `Command` applies them in its own order between
    // fork and exec, and it gets that ordering right; setting both here is what
    // makes the intent unambiguous either way.
    command.gid(account.gid);
    command.uid(account.uid);

    // A child left holding the satellite's `HOME` writes into a directory it
    // does not own, which surfaces as a harness failing on its own state file
    // rather than as anything that names a permission.
    command.env("HOME", &account.home);
    command.env("USER", &account.name);
    command.env("LOGNAME", &account.name);
}

/// The same, on a platform with no unprivileged account to descend to.
#[cfg(not(unix))]
pub fn hand_down(_command: &mut tokio::process::Command) {}

/// Creates a directory the agent account can write in.
///
/// The satellite creates a thread's workspace and the agent works in it, so a
/// directory created by a root satellite and left root-owned is a workspace the
/// agent cannot write a file into. Only the directory named is handed over, not
/// anything already inside it: everything this is called on is new.
///
/// # Errors
///
/// Returns the underlying I/O error when the directory cannot be created, or
/// when it cannot be handed to the agent account.
pub async fn create_dir_for_agent(path: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(path).await?;
    give_to_agent(path)
}

/// Hands an existing path to the agent account, where there is one.
///
/// Deliberately synchronous, and called from both async and blocking code. A
/// `chown` on one inode is a single system call that touches no data, so
/// offloading it would cost more in scheduling than it saves in blocking.
///
/// # Errors
///
/// Returns the underlying I/O error when ownership cannot be changed.
#[cfg(unix)]
pub fn give_to_agent(path: &Path) -> std::io::Result<()> {
    let Some(account) = descent().account() else {
        return Ok(());
    };

    std::os::unix::fs::chown(path, Some(account.uid), Some(account.gid))
}

/// The same, on a platform with no ownership to hand over.
///
/// # Errors
///
/// Never. The signature matches the Unix arm so callers need no `cfg` of their
/// own.
#[cfg(not(unix))]
pub fn give_to_agent(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account file shaped like the satellite image's own.
    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
                          arsox:x:10001:10001::/home/arsox:/usr/sbin/nologin\n";

    #[test]
    fn a_root_satellite_with_the_account_hands_children_down_to_it() {
        let descent = descent_for(0, Some(PASSWD), "arsox");

        assert_eq!(
            descent,
            Descent::To(Account {
                name: "arsox".to_owned(),
                uid: 10001,
                gid: 10001,
                home: PathBuf::from("/home/arsox"),
            })
        );
        assert!(descent.enforces());
    }

    #[test]
    fn a_satellite_that_is_not_root_enforces_nothing() {
        // The bare-metal case, and the case the test suite itself runs in. It
        // has to be a clean no-op rather than a failure: a developer running a
        // satellite is not doing anything wrong.
        let descent = descent_for(1000, Some(PASSWD), "arsox");

        assert_eq!(descent.account(), None);
        assert!(!descent.enforces());
        assert!(matches!(descent, Descent::Impossible(reason) if reason.contains("root")));
    }

    #[test]
    fn root_without_the_agent_account_enforces_nothing_either() {
        // An image built without the `useradd`, or one that renamed the
        // account. Running agents as root because the account was missing would
        // be the worst of the three outcomes, so it is refused rather than
        // defaulted.
        let descent = descent_for(0, Some(PASSWD), "somebody-else");

        assert!(!descent.enforces());
        assert!(matches!(descent, Descent::Impossible(reason) if reason.contains("agent account")));

        assert!(!descent_for(0, None, "arsox").enforces());
    }

    #[test]
    fn an_account_is_read_by_name_rather_than_by_position() {
        // A prefix must not match: `arsox` and `arsox-agent` are two accounts,
        // and handing children to the wrong one is a silent privilege change.
        assert_eq!(account_in(PASSWD, "root").expect("present").uid, 0);
        assert_eq!(account_in(PASSWD, "arso"), None);
        assert_eq!(account_in(PASSWD, "arsoxx"), None);
        assert_eq!(account_in("", "arsox"), None);
    }

    #[test]
    fn a_malformed_line_does_not_hide_the_account_below_it() {
        // One broken entry from an unrelated package must not take away the
        // satellite's ability to find its own account.
        let ragged = format!("truncated:x:5\n\nnot-even-colons\n{PASSWD}");

        assert_eq!(account_in(&ragged, "arsox").expect("present").uid, 10001);
    }

    #[test]
    fn a_non_numeric_id_is_skipped_rather_than_guessed() {
        // Defaulting a bad uid to zero would hand every child root, which is
        // precisely inverted from what this module is for.
        assert_eq!(
            account_in("arsox:x:ten:10001::/home/arsox:/bin/sh\n", "arsox"),
            None
        );
    }

    #[test]
    fn the_resolved_descent_is_stable_within_a_process() {
        // Read twice because every spawn site reads it, and two spawns
        // disagreeing about who they run as would be the worst possible way to
        // find out this was not cached.
        assert_eq!(descent(), descent());
    }

    #[tokio::test]
    async fn a_directory_made_for_an_agent_exists_whether_or_not_ownership_moved() {
        // On a test runner there is nothing to hand it to, and the directory
        // still has to be there: this is the same call the workspace makes.
        let path = std::env::temp_dir().join(format!("arsox-privilege-{}", uuid::Uuid::now_v7()));

        create_dir_for_agent(&path)
            .await
            .expect("should create the directory");

        assert!(path.is_dir());

        drop(tokio::fs::remove_dir_all(&path).await);
    }
}
