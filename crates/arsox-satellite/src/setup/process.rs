// Copyright © 2026 Jalapeno Labs

//! Running the setup script once: spawning it as root, bounding it, and
//! stopping everything it started.
//!
//! # `sh <file>`, not the command parser
//!
//! A repo's setup commands go through [`crate::commands`], where a newline means
//! "run in parallel". An install script is the opposite: line two depends on
//! line one. So the script is written to a file and handed to `sh` whole, and it
//! means exactly what a shell says it means, `set -e` and heredocs included.
//!
//! # The whole tree goes, not just the shell
//!
//! `sh` starts `apt-get`, which starts `dpkg`, which starts maintainer scripts.
//! Killing the shell alone would orphan the rest, and an orphaned `dpkg` holds
//! its lock until the container stops. So the script runs through
//! [`crate::supervise`], as the leader of its own process group, and stopping it
//! stops the group. Turn end hooks run the same way, as the agent.

pub use crate::supervise::{Ending, Run};

use std::path::Path;
use std::time::Duration;

/// How long a setup script may run before it is stopped.
///
/// Setup exists for downloads and package installs, and a large toolchain over a
/// slow link is minutes of work, so this is generous: the same thirty minutes a
/// repo's setup command gets. Every turn and every provisioning on the satellite
/// waits while it runs, so a script that hangs holds the whole satellite until
/// this passes, which is the argument against anything longer.
pub const SETUP_TIMEOUT: Duration = Duration::from_mins(30);

/// Writes the script where it will run from, readable by root alone.
///
/// Staged beside the destination and renamed into place, so a new file is a new
/// inode. `sh` reads a script as it goes rather than all at once, and a shell
/// still winding down from a replaced script must never read its successor's
/// lines halfway through.
///
/// # Errors
///
/// Returns the underlying I/O error when the file cannot be written.
pub async fn write_script(path: &Path, script: &str) -> std::io::Result<()> {
    let staged = path.with_extension("sh.new");

    if let Some(directory) = path.parent() {
        tokio::fs::create_dir_all(directory).await?;
    }

    // Removed first because a file's mode is only applied when it is created,
    // and a leftover from a crashed write could carry any mode at all.
    match tokio::fs::remove_file(&staged).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);

    // Set as it is opened rather than changed afterwards, so there is no moment
    // when the script is readable by anybody but its owner.
    #[cfg(unix)]
    options.mode(0o700);

    let mut file = options.open(&staged).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, script.as_bytes()).await?;
    file.sync_all().await?;
    drop(file);

    tokio::fs::rename(&staged, path).await
}

/// Runs the script at `path` to its end, its bound, or `stop`, whichever comes
/// first.
///
/// `stop` resolving, whether it was sent to or dropped, means the script was
/// replaced or cleared, and the whole process group is stopped.
pub async fn run(path: &Path, bound: Duration, stop: tokio::sync::oneshot::Receiver<()>) -> Run {
    let mut command = crate::privilege::root_command_for_host("sh");
    command
        .arg(path)
        // `/` rather than wherever the satellite was started, so a relative
        // path in the script means the same thing on every satellite.
        .current_dir("/");

    let stopped = async {
        // Sent or dropped, it means the same thing: this run is not wanted.
        let _replaced_or_cleared = stop.await;
    };

    let mut run = crate::supervise::run(command, bound, stopped, "the setup script").await;

    // Said in the output as well as in the ending, because the tail is what a
    // host reads, and a log that simply stops mid-download reads like a crash.
    match &run.ending {
        Ending::TimedOut(bound) => {
            run.output = format!(
                "the setup script was stopped after {bound:?} without finishing\n{}",
                run.output
            );
        }
        Ending::NotLaunched(reason) => {
            run.output = format!("could not start the setup script: {reason}");
        }
        Ending::Exited(_) | Ending::Signalled | Ending::Stopped => {}
    }

    run
}
