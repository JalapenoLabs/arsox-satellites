// Copyright © 2026 Jalapeno Labs

//! The satellite's setup script: the host application's own install script,
//! run as root.
//!
//! The image cannot ship every tool every host needs, so a host supplies a
//! script with `PUT /v1/setup` and the satellite runs it. It belongs to the
//! satellite rather than to any thread, and it runs:
//!
//! - **once per container start**, because a replaced container has lost
//!   everything outside `/var/arsox` and `/workspace`, which is where installed
//!   tooling lives;
//! - **whenever it changes**, at once. Setting the script the satellite already
//!   holds changes nothing. A different script stops a run in progress and
//!   starts over; an empty one clears it.
//!
//! Running it again is the script's problem to make cheap, not the satellite's
//! to avoid: a script that checks before it downloads costs a moment on every
//! start after the first.
//!
//! # New work waits while it runs
//!
//! While the script runs, no turn is claimed and no thread starts provisioning.
//! Turns already running carry on. The turn half of that rule is in the claim
//! query, beside `PAUSED` and `PROVISIONING`, so it holds for whatever does the
//! claiming. Provisioning waits on a [`Gate`], which mirrors the same row.
//!
//! # Failure does not stop work
//!
//! A script that fails leaves a satellite missing some tooling, which is
//! degraded rather than unusable: most turns will not touch what was missing,
//! and holding the whole queue on a script the host can fix and resend would
//! turn one bad line into an outage. So work proceeds, and a satellite-scoped
//! `SETUP_FAILED` incident carries the hash, the exit code, and the tail of the
//! output.
//!
//! # One writer
//!
//! This module is the only thing that writes the setup row. A run that was
//! replaced or cleared while it ran drops its outcome rather than recording it,
//! so a superseded script's failure can never overwrite its replacement's state.
//!
//! See [the setup doc](../../../docs/setup.md).

mod process;

pub use process::SETUP_TIMEOUT;

use crate::api::{Protobuf, store_failure};
use crate::redaction::Redactor;
use crate::store::{SetupOutcome, Store, StoreError};
use crate::stream::{EventBus, control_event};
use crate::{Satellite, protobuf};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::control_event::Payload;
use arsox_sdk::proto::event::v1::{SetupFinished, SetupStarted};
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use arsox_sdk::proto::satellite::v1::{
    SetSetupScriptRequest, SetSetupScriptResponse, SetupState, SetupStatus,
};
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::put;
use process::{Ending, Run};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, oneshot, watch};
use tokio::task::JoinHandle;

/// The script's file name, beside the database.
///
/// The database directory is `/var/arsox` in the image: root-owned, `0700`, and
/// a volume, so the script is somewhere only root can read and somewhere an
/// agent cannot rewrite between one start and the next.
const SCRIPT_FILE: &str = "setup.sh";

/// Runs the satellite's setup script and holds new work while it does.
///
/// Cheap to clone: clones share one state.
#[derive(Debug, Clone)]
pub struct Setup {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    bus: EventBus,
    script_path: PathBuf,

    /// Nudged when a run ends, so a turn that queued behind the script starts
    /// now rather than at the runner's next idle poll.
    work_queued: Arc<Notify>,

    /// True while a script runs. Mirrors the row the claim query reads, and is
    /// written right after it by the same code, for the provisioner to wait on
    /// without polling the database.
    running: watch::Sender<bool>,

    /// The run in progress, serialized so two settings cannot interleave.
    current: Mutex<Current>,
}

/// Which run is the current one.
#[derive(Debug, Default)]
struct Current {
    /// Bumped by every start and every clear. A run whose generation is no
    /// longer this one was replaced or cleared, and its outcome is dropped.
    generation: u64,

    /// Absent when nothing is running.
    run: Option<ActiveRun>,
}

/// A run in progress, and the way to stop it.
#[derive(Debug)]
struct ActiveRun {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// Holds provisioning while the setup script runs.
///
/// Turns are held by the claim query. Provisioning is not a claim, so it waits
/// here instead, on the same fact.
#[derive(Debug, Clone)]
pub struct Gate {
    running: watch::Receiver<bool>,
}

impl Gate {
    /// A gate that never closes, for a provisioner with no setup to wait on.
    #[must_use]
    pub fn open() -> Self {
        let (sender, running) = watch::channel(false);
        drop(sender);

        Self { running }
    }

    /// Whether a setup script is running now.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.running.borrow()
    }

    /// Waits until no setup script is running.
    pub async fn wait_until_open(&self) {
        let mut running = self.running.clone();

        // An error means the sender went away, which only happens when the
        // satellite itself is going away. Nothing is left to wait for.
        if running.wait_for(|running| !running).await.is_err() {
            tracing::debug!(
                event.name = "setup.gate.abandoned",
                "the setup gate closed with no setup behind it, proceeding",
            );
        }
    }
}

impl Setup {
    /// A setup that keeps its script beside the database at `database_path`.
    #[must_use]
    pub fn new(
        store: Store,
        bus: EventBus,
        database_path: &std::path::Path,
        work_queued: Arc<Notify>,
    ) -> Self {
        let directory = database_path
            .parent()
            .map_or_else(PathBuf::new, std::path::Path::to_path_buf);

        Self {
            inner: Arc::new(Inner {
                store,
                bus,
                script_path: directory.join(SCRIPT_FILE),
                work_queued,
                running: watch::Sender::new(false),
                current: Mutex::new(Current::default()),
            }),
        }
    }

    /// The gate provisioning waits on.
    #[must_use]
    pub fn gate(&self) -> Gate {
        Gate {
            running: self.inner.running.subscribe(),
        }
    }

    /// The script's status as it stands now.
    ///
    /// # Errors
    ///
    /// Returns a database error if the read fails.
    pub async fn status(&self) -> Result<SetupStatus, StoreError> {
        Ok(self
            .inner
            .store
            .setup()
            .await?
            .map_or_else(unset, |stored| stored.status))
    }

    /// Sets the script, and runs it if it changed.
    ///
    /// The same script again changes nothing, whatever its last run did. A
    /// different one is stored as running and started at once; a run in
    /// progress is stopped first. An empty one, or one of only whitespace,
    /// clears the script.
    ///
    /// # Errors
    ///
    /// Returns a database error if the script cannot be read or stored.
    pub async fn set_script(&self, script: String) -> Result<SetupStatus, StoreError> {
        let mut current = self.inner.current.lock().await;

        if script.trim().is_empty() {
            return self.clear(&mut current).await;
        }

        let script_sha256 = sha256_hex(&script);

        if let Some(stored) = self.inner.store.setup().await?
            && stored.status.script_sha256 == script_sha256
        {
            return Ok(stored.status);
        }

        self.start(&mut current, script, script_sha256).await
    }

    /// Runs the stored script again, because this is a new container.
    ///
    /// Awaited before the runner starts, so the row reads `RUNNING` before
    /// anything can claim a turn. Whatever the last run did, including a run a
    /// crash left marked `RUNNING`, this one starts over.
    pub async fn resume_at_boot(&self) {
        let stored = match self.inner.store.setup().await {
            Ok(Some(stored)) => stored,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(
                    event.name = "setup.resume.failed",
                    "could not read the setup script at boot: {error}",
                );
                return;
            }
        };

        tracing::info!(
            event.name = "setup.resumed",
            setup.script_sha256 = %stored.status.script_sha256,
            "running the setup script again for this container",
        );

        let mut current = self.inner.current.lock().await;

        if let Err(error) = self
            .start(&mut current, stored.script, stored.status.script_sha256)
            .await
        {
            tracing::error!(
                event.name = "setup.resume.failed",
                "could not restart the setup script at boot: {error}",
            );
        }
    }

    /// Stores a script as running and starts it, stopping any run before it.
    async fn start(
        &self,
        current: &mut Current,
        script: String,
        script_sha256: String,
    ) -> Result<SetupStatus, StoreError> {
        // The row first: it is what the claim query reads, so the gate is shut
        // before anything else happens.
        let status = self
            .inner
            .store
            .begin_setup(&script, &script_sha256)
            .await?;
        self.inner.running.send_replace(true);

        current.generation += 1;

        // Stopped rather than awaited here, so the request that replaced it is
        // answered at once. The new run waits for the old one to be gone before
        // it writes its file or starts, so two scripts never run together.
        let previous = current.run.take().map(|previous| {
            // Refused only when the run already ended on its own, which leaves
            // nothing to stop.
            let _already_ended = previous.stop.send(());

            tracing::info!(
                event.name = "setup.replaced",
                setup.script_sha256 = %script_sha256,
                "stopping the running setup script to start its replacement",
            );

            previous.task
        });

        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(self.clone().run(
            current.generation,
            script,
            script_sha256.clone(),
            stopped,
            previous,
        ));
        current.run = Some(ActiveRun { stop, task });

        tracing::info!(
            event.name = "setup.started",
            setup.script_sha256 = %script_sha256,
            "the setup script is running, new work waits until it finishes",
        );

        self.inner.bus.publish_control(control_event(
            "setup.started",
            Payload::SetupStarted(SetupStarted { script_sha256 }),
        ));

        Ok(status)
    }

    /// Forgets the script, stopping it if it runs.
    async fn clear(&self, current: &mut Current) -> Result<SetupStatus, StoreError> {
        current.generation += 1;

        let was_running = current.run.take().is_some_and(|previous| {
            let _already_ended = previous.stop.send(());
            true
        });

        self.inner.store.clear_setup().await?;
        self.inner.running.send_replace(false);
        self.inner.work_queued.notify_one();

        match tokio::fs::remove_file(&self.inner.script_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                event.name = "setup.clear.file_kept",
                "the setup script was cleared and its file could not be removed: {error}",
            ),
        }

        tracing::info!(
            event.name = "setup.cleared",
            setup.was_running = was_running,
            "the setup script was cleared",
        );

        // Only a run that was going needs its end announced. A consumer that saw
        // it start is waiting to hear it stopped.
        if was_running {
            self.inner.bus.publish_control(control_event(
                "setup.finished",
                Payload::SetupFinished(SetupFinished {
                    setup: Some(unset()),
                }),
            ));
        }

        Ok(unset())
    }

    /// Runs one script to its end, then records how it went.
    async fn run(
        self,
        generation: u64,
        script: String,
        script_sha256: String,
        mut stopped: oneshot::Receiver<()>,
        previous: Option<JoinHandle<()>>,
    ) {
        if let Some(previous) = previous {
            // The previous run is already stopping its process group. Waiting
            // for it is what keeps two installers from fighting over one lock.
            drop(previous.await);
        }

        // Replaced again while the one before it was still stopping. There is
        // nothing to start and nothing to record: the row is not this run's.
        if !matches!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return;
        }

        let run = match process::write_script(&self.inner.script_path, &script).await {
            Ok(()) => process::run(&self.inner.script_path, SETUP_TIMEOUT, stopped).await,
            Err(error) => Run {
                ending: Ending::NotLaunched(error.to_string()),
                output: format!("could not write the setup script: {error}"),
            },
        };

        self.finish(generation, &script_sha256, run).await;
    }

    /// Records a finished run, unless something replaced it while it ran.
    async fn finish(&self, generation: u64, script_sha256: &str, run: Run) {
        let mut current = self.inner.current.lock().await;

        if current.generation != generation {
            tracing::info!(
                event.name = "setup.superseded",
                setup.script_sha256 = %script_sha256,
                "a setup script was replaced or cleared while it ran, its outcome is dropped",
            );
            return;
        }

        current.run = None;

        let judged = judge(&run);

        if let Err(error) = self.inner.store.finish_setup(&judged.outcome).await {
            // The row still reads RUNNING, so the claim query still holds every
            // queue. That is a broken volume rather than anything a retry here
            // would fix, and it is loud for that reason.
            tracing::error!(
                event.name = "setup.record.failed",
                "the setup script finished and its outcome could not be recorded: {error}",
            );
        }

        self.inner.running.send_replace(false);
        self.inner.work_queued.notify_one();

        match &judged.failure {
            None => tracing::info!(
                event.name = "setup.succeeded",
                setup.script_sha256 = %script_sha256,
                "the setup script succeeded, work resumes",
            ),
            Some(message) => {
                tracing::error!(
                    event.name = "setup.failed",
                    setup.script_sha256 = %script_sha256,
                    setup.exit_code = judged.outcome.exit_code,
                    error.message = %message,
                    "{{error.message}}, work resumes without it",
                );

                self.report_failure(script_sha256, message, &judged.outcome)
                    .await;
            }
        }

        let status = match self.status().await {
            Ok(status) => status,
            Err(error) => {
                tracing::error!(
                    event.name = "setup.status.failed",
                    "could not read back the setup status to announce it: {error}",
                );
                return;
            }
        };

        self.inner.bus.publish_control(control_event(
            "setup.finished",
            Payload::SetupFinished(SetupFinished {
                setup: Some(status),
            }),
        ));
    }

    /// Records a failed run as a satellite-scoped incident.
    ///
    /// It belongs to no thread, so it reaches no thread stream; it is recorded,
    /// and the control stream's `setup.finished` carries the same status live.
    async fn report_failure(&self, script_sha256: &str, message: &str, outcome: &SetupOutcome) {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("script_sha256".to_owned(), text(script_sha256.to_owned()));
        if let Some(exit_code) = outcome.exit_code {
            fields.insert(
                "exit_code".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::NumberValue(f64::from(exit_code))),
                },
            );
        }
        if !outcome.output_tail.is_empty() {
            fields.insert("output".to_owned(), text(outcome.output_tail.clone()));
        }

        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            sequence: None,
            thread_id: None,
            turn_id: None,
            member_id: None,
            code: ErrorCode::SetupFailed.into(),
            disposition: Disposition::Degraded.into(),
            // The same script fails the same way next time. What changes the
            // answer is the host sending a different one.
            retryable: false,
            message: message.to_owned(),
            details: Some(prost_types::Struct {
                fields: fields.into_iter().collect(),
            }),
            occurred_at: Some(Timestamp::now()),
        };

        // No thread, so no thread's secrets to mask. A credential the host wrote
        // into its own script can reach this output; the setup doc says not to.
        if let Err(error) = self
            .inner
            .store
            .report_incident(incident, &Redactor::none())
            .await
        {
            tracing::error!(
                event.name = "incident.record.failed",
                "could not record the setup failure: {error}",
            );
        }
    }
}

/// What a run amounts to: the row to write, and why it failed if it did.
#[derive(Debug)]
struct Judged {
    outcome: SetupOutcome,

    /// The incident message. Absent when the run succeeded.
    failure: Option<String>,
}

/// Decides how a run is recorded.
fn judge(run: &Run) -> Judged {
    let failed = |exit_code: Option<i32>, message: String| Judged {
        outcome: SetupOutcome {
            state: SetupState::Failed,
            exit_code,
            output_tail: run.output.clone(),
        },
        failure: Some(message),
    };

    match &run.ending {
        Ending::Exited(0) => Judged {
            outcome: SetupOutcome {
                state: SetupState::Succeeded,
                exit_code: Some(0),
                output_tail: run.output.clone(),
            },
            failure: None,
        },
        Ending::Exited(code) => failed(Some(*code), format!("the setup script exited with {code}")),
        Ending::Signalled => failed(None, "the setup script was ended by a signal".to_owned()),
        Ending::TimedOut(bound) => failed(
            None,
            format!(
                "the setup script ran past its {} minute bound and was stopped",
                bound.as_secs() / 60
            ),
        ),
        // Only reachable for a run nothing replaced, which means the satellite
        // itself was stopping. Recorded as what it was rather than dropped.
        Ending::Stopped => failed(
            None,
            "the setup script was stopped before it finished".to_owned(),
        ),
        Ending::NotLaunched(reason) => failed(
            None,
            format!("the setup script could not be started: {reason}"),
        ),
    }
}

/// The status of a satellite with no script.
fn unset() -> SetupStatus {
    SetupStatus {
        state: SetupState::None.into(),
        ..SetupStatus::default()
    }
}

/// The lowercase hex SHA-256 a host compares its own script against.
fn sha256_hex(script: &str) -> String {
    let digest = Sha256::digest(script.as_bytes());

    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut hex, byte| {
            // Writing into a String cannot fail.
            let _infallible = write!(hex, "{byte:02x}");
            hex
        })
}

fn text(value: String) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value)),
    }
}

/// Sets the setup script.
///
/// Answers at once with the status as it stands: `RUNNING` for a new script,
/// `NONE` for a cleared one, and whatever the last run left for the script the
/// satellite already held. Progress after that is on `GET /v1/status` and the
/// control stream.
async fn set_setup_script(
    State(satellite): State<Arc<Satellite>>,
    Protobuf(request): Protobuf<SetSetupScriptRequest>,
) -> Response {
    match satellite.setup.set_script(request.script).await {
        Ok(status) => protobuf(&SetSetupScriptResponse {
            setup: Some(status),
        }),
        Err(error) => store_failure(&error),
    }
}

/// The setup route, authenticated like every other.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new().route("/v1/setup", put(set_setup_script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_hash_is_the_lowercase_hex_a_host_computes_itself() {
        // The empty string's SHA-256, which any tool a host has will agree on.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex("apt-get install -y jq\n").len(), 64);
    }

    #[test]
    fn only_a_clean_exit_succeeds() {
        let judged = |ending: Ending| {
            judge(&Run {
                ending,
                output: "tail".to_owned(),
            })
        };

        let clean = judged(Ending::Exited(0));
        assert_eq!(clean.outcome.state, SetupState::Succeeded);
        assert!(clean.failure.is_none());

        let exited = judged(Ending::Exited(3));
        assert_eq!(exited.outcome.state, SetupState::Failed);
        assert_eq!(exited.outcome.exit_code, Some(3));
        assert_eq!(exited.outcome.output_tail, "tail");

        // An exit code is only ever one the script chose. A run that was
        // stopped, signalled, or never started has none, and must not read as
        // if it exited zero or one.
        for ending in [
            Ending::Signalled,
            Ending::TimedOut(SETUP_TIMEOUT),
            Ending::NotLaunched("no sh".to_owned()),
        ] {
            let failed = judged(ending);
            assert_eq!(failed.outcome.state, SetupState::Failed);
            assert_eq!(failed.outcome.exit_code, None);
            assert!(failed.failure.is_some());
        }

        let timed_out = judged(Ending::TimedOut(SETUP_TIMEOUT));
        assert!(
            timed_out
                .failure
                .is_some_and(|message| message.contains("30 minute")),
            "a timed out script says which bound it ran past"
        );
    }

    #[tokio::test]
    async fn an_open_gate_never_waits() {
        let gate = Gate::open();

        assert!(!gate.is_closed());
        gate.wait_until_open().await;
    }
}

/// Real scripts through a real shell, against an in-memory store.
///
/// Unix only: the script is `sh`, and the tree teardown is a process group.
#[cfg(all(test, unix))]
mod runs {
    use super::*;
    use crate::store::IncidentFilter;
    use std::path::Path;
    use std::time::Duration;

    /// How long a test waits for a short script to finish before failing.
    const SETTLE: Duration = Duration::from_secs(20);

    struct Harness {
        setup: Setup,
        store: Store,
        directory: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.directory));
        }
    }

    async fn harness() -> Harness {
        let directory = std::env::temp_dir().join(format!("arsox-setup-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&directory).expect("should create");

        let store = Store::open_in_memory().await.expect("should open");
        let setup = Setup::new(
            store.clone(),
            EventBus::new(),
            &directory.join("arsox.db"),
            Arc::new(Notify::new()),
        );

        Harness {
            setup,
            store,
            directory,
        }
    }

    /// Waits for the script to stop running, and returns where it landed.
    async fn settled(setup: &Setup) -> SetupStatus {
        let deadline = tokio::time::Instant::now() + SETTLE;

        loop {
            let status = setup.status().await.expect("should read");
            if status.state() != SetupState::Running {
                return status;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "the setup script was still running after {SETTLE:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Whether a process is still alive, by asking the kernel to signal it
    /// with nothing.
    fn is_alive(pid: i32) -> bool {
        rustix::process::Pid::from_raw(pid)
            .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
    }

    /// Reads a pid the script wrote, once it has written it.
    async fn pid_in(path: &Path) -> i32 {
        let deadline = tokio::time::Instant::now() + SETTLE;

        loop {
            if let Ok(written) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = written.trim().parse()
            {
                return pid;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "the script never wrote its pid"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn a_script_runs_to_success_and_releases_the_gate() {
        let harness = harness().await;
        let gate = harness.setup.gate();

        let started = harness
            .setup
            .set_script("echo installed-something\n".to_owned())
            .await
            .expect("should set");

        assert_eq!(started.state(), SetupState::Running);
        assert_eq!(
            started.script_sha256,
            sha256_hex("echo installed-something\n")
        );

        let finished = settled(&harness.setup).await;

        assert_eq!(finished.state(), SetupState::Succeeded);
        assert_eq!(finished.exit_code, Some(0));
        assert!(finished.output_tail.contains("installed-something"));
        assert!(finished.finished_at.is_some());
        assert!(
            !gate.is_closed(),
            "a finished script must release provisioning"
        );
    }

    #[tokio::test]
    async fn setting_the_same_script_again_runs_nothing() {
        // The host resends its script on every boot of its own. Running it again
        // each time would hold the satellite's queue for nothing.
        let harness = harness().await;
        let script = format!(
            "echo once >> {}\n",
            harness.directory.join("ran.txt").display()
        );

        harness
            .setup
            .set_script(script.clone())
            .await
            .expect("should set");
        let first = settled(&harness.setup).await;

        let again = harness.setup.set_script(script).await.expect("should set");

        assert_eq!(again.state(), SetupState::Succeeded);
        assert_eq!(
            again.started_at, first.started_at,
            "the same script was started again"
        );

        let ran = tokio::fs::read_to_string(harness.directory.join("ran.txt"))
            .await
            .expect("the script ran once");
        assert_eq!(ran.lines().count(), 1);
    }

    #[tokio::test]
    async fn a_failing_script_is_recorded_and_reported_without_a_thread() {
        let harness = harness().await;

        harness
            .setup
            .set_script("echo no-such-package >&2\nexit 3\n".to_owned())
            .await
            .expect("should set");

        let failed = settled(&harness.setup).await;

        assert_eq!(failed.state(), SetupState::Failed);
        assert_eq!(failed.exit_code, Some(3));
        assert!(failed.output_tail.contains("no-such-package"));

        let incidents = harness
            .store
            .list_incidents(&IncidentFilter {
                codes: vec![ErrorCode::SetupFailed.into()],
                ..IncidentFilter::default()
            })
            .await
            .expect("should list")
            .incidents;

        assert_eq!(incidents.len(), 1);
        let incident = &incidents[0];
        assert_eq!(
            incident.thread_id, None,
            "a setup failure belongs to the satellite"
        );
        assert_eq!(incident.disposition(), Disposition::Degraded);
        assert!(!incident.retryable);
    }

    #[tokio::test]
    async fn the_script_gets_a_clean_environment_and_nothing_of_the_satellite() {
        // The test process carries plenty of its own variables, CARGO_* among
        // them. None may reach the script: only the fixed set, and what `sh`
        // sets for itself.
        let harness = harness().await;

        harness
            .setup
            .set_script("env\n".to_owned())
            .await
            .expect("should set");
        let finished = settled(&harness.setup).await;

        let keys: Vec<&str> = finished
            .output_tail
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _value)| key))
            .collect();

        let allowed = [
            "PATH",
            "HOME",
            "LANG",
            "DEBIAN_FRONTEND",
            "PWD",
            "SHLVL",
            "_",
            "OLDPWD",
        ];
        for key in &keys {
            assert!(allowed.contains(key), "{key} leaked into the setup script");
        }
        assert!(
            finished
                .output_tail
                .contains("DEBIAN_FRONTEND=noninteractive")
        );
        assert!(
            finished
                .output_tail
                .contains(&format!("PATH={}", crate::privilege::ROOT_PATH))
        );
    }

    #[tokio::test]
    async fn a_replaced_script_stops_with_everything_it_started() {
        // The old script leaves a background child behind. Stopping the shell
        // alone would orphan it, which for a real install is a `dpkg` holding
        // its lock until the container stops.
        let harness = harness().await;
        let pidfile = harness.directory.join("child.pid");

        harness
            .setup
            .set_script(format!(
                "sleep 300 &\necho $! > {}\nwait\n",
                pidfile.display()
            ))
            .await
            .expect("should set");

        let child = pid_in(&pidfile).await;
        assert!(is_alive(child));

        harness
            .setup
            .set_script("echo the-replacement\n".to_owned())
            .await
            .expect("should set");

        let finished = settled(&harness.setup).await;

        assert_eq!(finished.state(), SetupState::Succeeded);
        assert!(finished.output_tail.contains("the-replacement"));
        assert!(
            !is_alive(child),
            "the replaced script's background child outlived it"
        );

        // The stopped run must not have recorded its own ending over its
        // replacement's, and must not have left an incident behind.
        let incidents = harness
            .store
            .list_incidents(&IncidentFilter::default())
            .await
            .expect("should list")
            .incidents;
        assert!(
            incidents.is_empty(),
            "a replaced script is not a failed one"
        );
    }

    #[tokio::test]
    async fn clearing_a_running_script_stops_it_and_opens_the_gate() {
        let harness = harness().await;
        let gate = harness.setup.gate();
        let pidfile = harness.directory.join("shell.pid");

        harness
            .setup
            .set_script(format!("echo $$ > {}\nsleep 300\n", pidfile.display()))
            .await
            .expect("should set");

        let shell = pid_in(&pidfile).await;
        assert!(gate.is_closed(), "a running script must hold provisioning");

        let cleared = harness
            .setup
            .set_script("  \n".to_owned())
            .await
            .expect("should clear");

        assert_eq!(cleared.state(), SetupState::None);
        assert!(cleared.script_sha256.is_empty());
        assert!(!gate.is_closed());
        assert!(harness.store.setup().await.expect("should read").is_none());

        let deadline = tokio::time::Instant::now() + SETTLE;
        while is_alive(shell) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the cleared script kept running"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn a_new_container_runs_the_stored_script_again() {
        // Installed tooling lives outside the volumes, so a replaced container
        // has lost it. What survives is the row.
        let harness = harness().await;
        let log = harness.directory.join("runs.txt");
        let script = format!("echo run >> {}\n", log.display());

        harness.setup.set_script(script).await.expect("should set");
        settled(&harness.setup).await;

        let rebooted = Setup::new(
            harness.store.clone(),
            EventBus::new(),
            &harness.directory.join("arsox.db"),
            Arc::new(Notify::new()),
        );
        rebooted.resume_at_boot().await;

        assert_eq!(
            rebooted.status().await.expect("should read").state(),
            SetupState::Running,
            "the row must read RUNNING before boot goes on to start the runner"
        );

        assert_eq!(settled(&rebooted).await.state(), SetupState::Succeeded);

        let runs = tokio::fs::read_to_string(&log).await.expect("should read");
        assert_eq!(runs.lines().count(), 2);
    }

    #[tokio::test]
    async fn a_script_past_its_bound_is_stopped_and_says_so() {
        let directory =
            std::env::temp_dir().join(format!("arsox-setup-bound-{}", uuid::Uuid::now_v7()));
        let script = directory.join("setup.sh");
        process::write_script(&script, "echo before-the-hang\nsleep 300\n")
            .await
            .expect("should write");

        let (_keep, stop) = oneshot::channel();
        let started = std::time::Instant::now();

        let run = process::run(&script, Duration::from_millis(500), stop).await;

        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(run.ending, Ending::TimedOut(Duration::from_millis(500)));
        assert!(
            run.output.contains("before-the-hang"),
            "got {:?}",
            run.output
        );
        assert!(run.output.contains("without finishing"));

        drop(std::fs::remove_dir_all(&directory));
    }

    #[tokio::test]
    async fn the_script_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            std::env::temp_dir().join(format!("arsox-setup-mode-{}", uuid::Uuid::now_v7()));
        let script = directory.join("setup.sh");

        process::write_script(&script, "exit 0\n")
            .await
            .expect("should write");

        let mode = std::fs::metadata(&script)
            .expect("should stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        drop(std::fs::remove_dir_all(&directory));
    }
}
