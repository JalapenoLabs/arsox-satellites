// Copyright © 2026 Jalapeno Labs

//! Running host-supplied code to its end, its bound, or a stop, and taking
//! everything it started with it.
//!
//! Two things the host hands the satellite run this way: the setup script, as
//! root, and a thread's turn end hooks, as the agent. Both are code the
//! satellite did not write, both can start children of their own, and both must
//! end when the satellite says so. The caller builds the command, which decides
//! who it runs as and what it sees; this decides how it ends.
//!
//! # The whole tree goes, not just the leader
//!
//! `sh` starts `apt-get`, which starts `dpkg`; a hook starts Blender, which can
//! start a render worker. Killing the leader alone would orphan the rest, and an
//! orphan keeps running until the container stops. So the command runs as the
//! leader of its own process group, and stopping it signals the group:
//! `SIGTERM` first, then `SIGKILL` to whatever is still there after
//! [`STOP_GRACE`].
//!
//! A command that exits on its own is not chased: anything it deliberately left
//! running in the background keeps running. Its output stops being read shortly
//! after it exits, because a background process holding the pipe open would
//! otherwise keep the run from ever finishing.

use crate::commands::{MAX_CAPTURED_OUTPUT, tail};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// How long a stopped process group gets to exit after `SIGTERM`.
///
/// Long enough for `apt-get` to release its lock, a download to close its file,
/// and an exporter to finish the file it is writing; short enough that replacing
/// a hung script does not keep its replacement waiting. Whatever is left
/// afterwards is sent `SIGKILL`.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long output is still read after the leader itself exits.
///
/// A process started in the background inherits the leader's stdout, so the
/// pipe can stay open long after the leader is gone. Reading until end of file
/// would then never finish. A moment is enough for whatever the leader itself
/// wrote last to arrive.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);

/// Bytes read from a pipe at a time.
const READ_CHUNK: usize = 8 * 1024;

/// How one run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The leader exited with this code.
    Exited(i32),

    /// A signal ended the leader without the satellite sending one.
    Signalled,

    /// It ran past its bound and its group was stopped.
    TimedOut(Duration),

    /// Its stop was asked for while it ran, and its group was stopped.
    Stopped,

    /// It could not be started at all.
    NotLaunched(String),
}

/// What one run did.
#[derive(Debug, Clone)]
pub struct Run {
    pub ending: Ending,

    /// Standard output and standard error as they arrived, interleaved, the
    /// oldest bytes dropped first past the capture cap. Unmasked: the caller
    /// knows whose secrets could be in it.
    pub output: String,

    /// From launch until the run ended, its group stop included.
    pub elapsed: Duration,
}

/// Runs `command` to its end, `bound`, or `stop`, whichever comes first.
///
/// `command` is taken as the caller built it, identity and environment
/// included; this arranges its pipes, makes it the leader of its own process
/// group, and gives it no standard input, since a program that reads one would
/// otherwise block forever on a terminal that is not there. `label` names it in
/// the satellite's own logs.
///
/// `stop` resolving means whatever asked for the run no longer wants it, and
/// the whole group is stopped.
pub async fn run(
    mut command: tokio::process::Command,
    bound: Duration,
    stop: impl Future<Output = ()>,
    label: &'static str,
) -> Run {
    let started = tokio::time::Instant::now();

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The backstop for every path that drops the child without stopping it
        // first. It reaches the leader only, which is why every path here stops
        // the group itself.
        .kill_on_drop(true);

    // Its own process group, with the leader's pid as the group's id, so one
    // signal reaches everything it started.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Run {
                output: format!("could not start it: {error}"),
                ending: Ending::NotLaunched(error.to_string()),
                elapsed: started.elapsed(),
            };
        }
    };

    // Read now, while the leader is certainly alive. Once it has been reaped the
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
            stop_group(group, &mut child, label).await;
            Ending::TimedOut(bound)
        }
        () = stop => {
            stop_group(group, &mut child, label).await;
            Ending::Stopped
        }
    };

    // Bounded, and abandoned past the bound, because a background process the
    // leader left running can hold the pipe open indefinitely.
    if tokio::time::timeout(OUTPUT_DRAIN, &mut reader)
        .await
        .is_err()
    {
        reader.abort();
        tracing::debug!(
            event.name = "supervised.output.abandoned",
            process.label = label,
            "{{process.label}} exited and something it started still holds its output open",
        );
    }

    let bytes = captured
        .lock()
        .map(|captured| captured.clone())
        .unwrap_or_default();

    Run {
        ending,
        output: tail(&String::from_utf8_lossy(&bytes), MAX_CAPTURED_OUTPUT),
        elapsed: started.elapsed(),
    }
}

/// Reads what a finished `wait` says about how the leader ended.
fn ending_of(waited: std::io::Result<ExitStatus>) -> Ending {
    match waited {
        // `None` means a signal ended it, and the satellite sent none.
        Ok(status) => status.code().map_or(Ending::Signalled, Ending::Exited),
        Err(error) => Ending::NotLaunched(format!("could not wait on it: {error}")),
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
/// front was cut and says so. A run that prints for thirty minutes costs this
/// much memory and no more.
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

/// Stops the leader and everything it started.
///
/// `SIGTERM` to the group, a grace period for the leader to exit, then `SIGKILL`
/// to whatever of the group remains. The second signal is sent whether or not
/// the leader went quietly, because a child that ignored the first is exactly
/// the process that would otherwise outlive the run.
///
/// `group` is the leader's pid, read at spawn. The leader leads its own group,
/// so that pid is the group's id, and it stays the right address after the
/// leader itself has been reaped.
async fn stop_group(group: Option<u32>, child: &mut tokio::process::Child, label: &'static str) {
    signal_group(group, child, GroupSignal::Terminate, label);

    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            event.name = "supervised.stop.forced",
            process.label = label,
            grace.seconds = STOP_GRACE.as_secs(),
            "{{process.label}} ignored SIGTERM for {{grace.seconds}} seconds, sending SIGKILL",
        );
    }

    signal_group(group, child, GroupSignal::Kill, label);

    // A kill is not something a process can decline, so this cannot hang.
    drop(child.wait().await);
}

/// The two signals a stop sends.
#[derive(Debug, Clone, Copy)]
enum GroupSignal {
    Terminate,
    Kill,
}

/// Signals the leader's whole process group.
///
/// A group that has already gone is not an error: a leader that exits during
/// its grace period leaves nothing to kill.
#[cfg(unix)]
fn signal_group(
    group: Option<u32>,
    _child: &mut tokio::process::Child,
    signal: GroupSignal,
    label: &'static str,
) {
    use rustix::process::{Pid, Signal, kill_process_group};

    let Some(group) = group
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        // Only reachable when the spawn reported no id, which tokio does for a
        // child it has already reaped. There is nothing left to address.
        tracing::debug!(
            event.name = "supervised.stop.no_group",
            process.label = label,
            "{{process.label}} reported no process id, so its group cannot be signalled",
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
            event.name = "supervised.stop.signal_failed",
            process.label = label,
            "could not signal the process group of {{process.label}}: {error}",
        ),
    }
}

/// The same, on a platform with no process groups: the leader alone.
#[cfg(not(unix))]
fn signal_group(
    _group: Option<u32>,
    child: &mut tokio::process::Child,
    _signal: GroupSignal,
    _label: &'static str,
) {
    drop(child.start_kill());
}
