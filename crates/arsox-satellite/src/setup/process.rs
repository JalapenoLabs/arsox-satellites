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
//! its lock until the container stops. So the script runs as the leader of its
//! own process group, and stopping it signals the group: `SIGTERM` first, then
//! `SIGKILL` to whatever is still there after [`STOP_GRACE`].
//!
//! A script that exits on its own is not chased: anything it deliberately left
//! running in the background keeps running. Its output stops being read shortly
//! after the script exits, because a background process holding the pipe open
//! would otherwise keep the run from ever finishing.

use crate::commands::{MAX_CAPTURED_OUTPUT, tail};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// How long a setup script may run before it is stopped.
///
/// Setup exists for downloads and package installs, and a large toolchain over a
/// slow link is minutes of work, so this is generous: the same thirty minutes a
/// repo's setup command gets. Every turn and every provisioning on the satellite
/// waits while it runs, so a script that hangs holds the whole satellite until
/// this passes, which is the argument against anything longer.
pub const SETUP_TIMEOUT: Duration = Duration::from_mins(30);

/// How long a stopped script's process group gets to exit after `SIGTERM`.
///
/// Long enough for `apt-get` to release its lock and a download to close its
/// file; short enough that replacing a hung script does not keep its
/// replacement waiting. Whatever is left afterwards is sent `SIGKILL`.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long output is still read after the script itself exits.
///
/// A process the script started in the background inherits its stdout, so the
/// pipe can stay open long after the script is gone. Reading until end of file
/// would then never finish. A moment is enough for whatever the script itself
/// wrote last to arrive.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);

/// Bytes read from a pipe at a time.
const READ_CHUNK: usize = 8 * 1024;

/// How one run of the script ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The script exited with this code.
    Exited(i32),

    /// A signal stopped the script without the satellite asking it to.
    Signalled,

    /// The script ran past its bound and was stopped.
    TimedOut(Duration),

    /// The script was replaced or cleared while it ran, and was stopped.
    Stopped,

    /// The script could not be started at all.
    NotLaunched(String),
}

/// What one run of the script did.
#[derive(Debug, Clone)]
pub struct Run {
    pub ending: Ending,

    /// Standard output and standard error as they arrived, interleaved, the
    /// oldest bytes dropped first past the capture cap.
    pub output: String,
}

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
        .current_dir("/")
        // An installer that reads stdin would block forever on a terminal that
        // is not there.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The backstop for every path that drops the child without stopping it
        // first. It reaches the shell only, which is why every path here stops
        // the group itself.
        .kill_on_drop(true);

    // Its own process group, with the shell as the leader, so one signal
    // reaches everything the script started.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Run {
                ending: Ending::NotLaunched(error.to_string()),
                output: format!("could not start the setup script: {error}"),
            };
        }
    };

    // Read now, while the shell is certainly alive. Once it has been reaped the
    // child no longer reports an id, and the rest of its group can still be
    // running with nothing left to address it by.
    let group = child.id();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut reader = tokio::spawn(read_output(
        child.stdout.take(),
        child.stderr.take(),
        Arc::clone(&captured),
    ));

    let ending = tokio::select! {
        waited = child.wait() => ending_of(waited),
        () = tokio::time::sleep(bound) => {
            stop_group(group, &mut child).await;
            Ending::TimedOut(bound)
        }
        _stopped = stop => {
            stop_group(group, &mut child).await;
            Ending::Stopped
        }
    };

    // Bounded, and abandoned past the bound, because a background process the
    // script left running can hold the pipe open indefinitely.
    if tokio::time::timeout(OUTPUT_DRAIN, &mut reader)
        .await
        .is_err()
    {
        reader.abort();
        tracing::debug!(
            event.name = "setup.output.abandoned",
            "the setup script exited and something it started still holds its output open",
        );
    }

    let bytes = captured
        .lock()
        .map(|captured| captured.clone())
        .unwrap_or_default();

    let mut output = tail(&String::from_utf8_lossy(&bytes), MAX_CAPTURED_OUTPUT);

    // Said in the output as well as in the ending, because the tail is what a
    // host reads, and a log that simply stops mid-download reads like a crash.
    if let Ending::TimedOut(bound) = ending {
        output =
            format!("the setup script was stopped after {bound:?} without finishing\n{output}");
    }

    Run { ending, output }
}

/// Reads what a finished `wait` says about how the script ended.
fn ending_of(waited: std::io::Result<ExitStatus>) -> Ending {
    match waited {
        // `None` means a signal ended it, and the satellite sent none.
        Ok(status) => status.code().map_or(Ending::Signalled, Ending::Exited),
        Err(error) => Ending::NotLaunched(format!("could not wait on the setup script: {error}")),
    }
}

/// Reads both pipes into one buffer, in the order the bytes arrive.
///
/// One buffer rather than two, so the tail shows an error beside the lines that
/// led to it rather than every error after every line of ordinary output.
async fn read_output(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    captured: Arc<Mutex<Vec<u8>>>,
) {
    tokio::join!(read_into(stdout, &captured), read_into(stderr, &captured),);
}

/// Reads one pipe to its end, keeping at most twice the capture cap.
///
/// Twice rather than exactly the cap so the final [`tail`] still knows the
/// front was cut and says so. An install that prints for thirty minutes costs
/// this much memory and no more.
async fn read_into<R: AsyncRead + Unpin>(pipe: Option<R>, captured: &Mutex<Vec<u8>>) {
    let Some(mut pipe) = pipe else {
        return;
    };

    let mut chunk = vec![0; READ_CHUNK];

    // A read error is a pipe that went away, which the exit status already
    // describes. Whatever arrived before it is kept.
    while let Ok(read) = pipe.read(&mut chunk).await
        && read > 0
    {
        let Ok(mut captured) = captured.lock() else {
            return;
        };

        captured.extend_from_slice(&chunk[..read]);

        let keep = 2 * MAX_CAPTURED_OUTPUT;
        if captured.len() > keep {
            let excess = captured.len() - keep;
            captured.drain(..excess);
        }
    }
}

/// Stops the script and everything it started.
///
/// `SIGTERM` to the group, a grace period for the shell to exit, then `SIGKILL`
/// to whatever of the group remains. The second signal is sent whether or not
/// the shell went quietly, because a child that ignored the first is exactly
/// the process that would otherwise outlive the run.
///
/// `group` is the shell's pid, read at spawn. The shell leads its own group, so
/// that pid is the group's id, and it stays the right address after the shell
/// itself has been reaped.
async fn stop_group(group: Option<u32>, child: &mut tokio::process::Child) {
    signal_group(group, child, GroupSignal::Terminate);

    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            event.name = "setup.stop.forced",
            grace.seconds = STOP_GRACE.as_secs(),
            "the setup script ignored SIGTERM for {{grace.seconds}} seconds, sending SIGKILL",
        );
    }

    signal_group(group, child, GroupSignal::Kill);

    // A kill is not something a process can decline, so this cannot hang.
    drop(child.wait().await);
}

/// The two signals a stop sends.
#[derive(Debug, Clone, Copy)]
enum GroupSignal {
    Terminate,
    Kill,
}

/// Signals the script's whole process group.
///
/// A group that has already gone is not an error: a script that exits during
/// its grace period leaves nothing to kill.
#[cfg(unix)]
fn signal_group(group: Option<u32>, _child: &mut tokio::process::Child, signal: GroupSignal) {
    use rustix::process::{Pid, Signal, kill_process_group};

    let Some(group) = group
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        // Only reachable when the spawn reported no id, which tokio does for a
        // child it has already reaped. There is nothing left to address.
        tracing::debug!(
            event.name = "setup.stop.no_group",
            "the setup script reported no process id, so its group cannot be signalled",
        );
        return;
    };

    let signal = match signal {
        GroupSignal::Terminate => Signal::TERM,
        GroupSignal::Kill => Signal::KILL,
    };

    match kill_process_group(group, signal) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::SRCH => {}
        Err(error) => tracing::warn!(
            event.name = "setup.stop.signal_failed",
            "could not signal the setup script's process group: {error}",
        ),
    }
}

/// The same, on a platform with no process groups: the shell alone.
#[cfg(not(unix))]
fn signal_group(_group: Option<u32>, child: &mut tokio::process::Child, _signal: GroupSignal) {
    drop(child.start_kill());
}
