// Copyright © 2026 Jalapeno Labs

//! Starting one turn's services, keeping them up, and stopping them.
//!
//! # Started in order, each ready before the next
//!
//! A later service may depend on an earlier one, the way an MCP server depends
//! on the editor it drives, so each is started only once the one before it has
//! passed its readiness probe or given up. A service that never becomes ready is
//! stopped and recorded as a degraded `SERVICE_START_FAILED` incident carrying
//! its log tail, and the next one starts anyway. **Degraded rather than fatal**:
//! the harness can still do most of what it was asked without a helper, an MCP
//! server pointing at the missing service simply fails to connect, which both
//! harnesses tolerate, and failing the whole turn would throw away work to
//! report one missing process.
//!
//! # Each service is its own process group
//!
//! A service runs through `sh -c`, so the process that listens is frequently a
//! grandchild of the satellite, and it may start children of its own. Killing
//! the shell alone would leave all of them running. Every service is therefore
//! started as the leader of a new process group, and stopping it signals the
//! whole group: `SIGTERM`, a grace period, then `SIGKILL` for whatever is left.
//! That is what makes "nothing lingers after the turn" true of the service's
//! children too. On a platform without process groups only the shell is
//! stopped.
//!
//! # Restarted a bounded number of times
//!
//! A service that exits after it was ready is started again on the same port,
//! after a short backoff that doubles each time, at most
//! [`RESTARTS_PER_TURN`] times in one turn. A restart that brings it back is
//! recorded as a recovered `SERVICE_EXITED` incident, because a service crashing
//! every few minutes and being quietly revived is a pattern nobody sees
//! otherwise. Past the bound it stays down and a degraded one says so.
//!
//! # Stopped however the turn ends
//!
//! [`Running::stop`] is called on every ending a turn has. [`Running`] also
//! kills every group it still holds when it drops, which covers the endings
//! `stop` cannot: a turn future dropped by a satellite shutting down, or a
//! panic. That backstop is `SIGKILL` alone, since `Drop` cannot wait out a
//! grace period.

use super::{Address, Addresses, Lease, LeaseError, Leases, PORT_VARIABLE};
use crate::harness::spawn::AgentVar;
use crate::redaction::Redactor;
use crate::store::{AppendEvent, Store};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{LogChannel, ServiceLog, ServiceStarted};
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use arsox_sdk::proto::settings::v1::{Service, ThreadSettings};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How many times one service is started again within one turn.
///
/// Enough to ride out a crash or two in a long turn, and bounded because a
/// service that dies on every start would otherwise spend the whole turn
/// restarting and filling the stream with the same failure.
pub const RESTARTS_PER_TURN: u32 = 3;

/// How long to wait before the first restart, doubled for each one after it.
///
/// Short, because the agent may be waiting on the service right now, and long
/// enough that a service failing at once on a port still held by its dying
/// predecessor gets the moment the kernel needs to release it.
const RESTART_BACKOFF: Duration = Duration::from_millis(500);

/// How long a service is given to exit after `SIGTERM` before its group is
/// killed.
///
/// Long enough for a server to close its listeners and flush what it holds, and
/// short enough that ending a turn is not held up by a process ignoring the
/// request.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often a readiness probe is tried.
const PROBE_INTERVAL: Duration = Duration::from_millis(200);

/// How long one probe attempt may take before it counts as a failure.
///
/// A service accepting connections and never answering them is not ready, and
/// an attempt without a bound would hold the probe past its own timeout.
const PROBE_ATTEMPT: Duration = Duration::from_secs(2);

/// How long a stopped service's pipes are read before the readers are dropped.
///
/// A process that left the group, with `setsid` for instance, can hold a pipe
/// open after everything in the group is gone, and waiting on it would hold the
/// turn open for the life of that process.
const DRAIN_BOUND: Duration = Duration::from_secs(2);

/// How many of a service's last lines are kept as evidence.
///
/// The tail an incident carries. Enough for a stack trace and the line before
/// it, bounded because the alternative is a whole server log in an incident row.
const TAIL_LINES: usize = 40;

/// How much of any one kept line is kept.
const TAIL_LINE_CHARS: usize = 500;

/// Longest line a `service.log` event carries, in characters.
///
/// A service printing a minified bundle as one line would otherwise put a
/// megabyte into one event, and the head of a line says what it was.
const MAX_LOG_LINE: usize = 4096;

/// The turn a set of services belongs to, and where they run.
#[derive(Debug, Clone)]
pub struct TurnScope {
    pub store: Store,
    pub thread_id: String,
    pub turn_id: String,

    /// The thread's secrets, masked out of every event and incident a service
    /// produces. A service runs with the thread's declared variables and prints
    /// whatever it likes.
    pub redactor: Redactor,

    /// The thread's workspace root, where every service runs.
    pub working_dir: PathBuf,
}

/// One turn's services, running, and the ports they hold.
///
/// Empty for a thread that declared none, which costs that thread nothing.
#[derive(Debug, Default)]
pub struct Running {
    addresses: Addresses,
    supervised: Vec<Supervised>,

    /// Held for the whole turn, including for a service that failed, so no
    /// other turn is handed a port this turn's agent was told about.
    leases: Vec<Lease>,
}

/// One service being kept up, and the handles that stop it.
#[derive(Debug)]
struct Supervised {
    name: String,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,

    /// The service's current process group, or zero while it has none. Read
    /// by the drop backstop, which cannot ask the task.
    group: Arc<AtomicI32>,
}

impl Running {
    /// Starts a thread's services for one turn, in declaration order.
    ///
    /// Each is started once the one before it has become ready or given up, and
    /// told the addresses of every service before it. Nothing here fails the
    /// turn: a service that cannot be given a port, cannot be launched, or never
    /// becomes ready is recorded as a degraded incident, and the rest start
    /// regardless.
    pub async fn start(settings: &ThreadSettings, scope: TurnScope, leases: &Leases) -> Self {
        let mut running = Self::default();

        if settings.services.is_empty() {
            return running;
        }

        let reporting = Arc::new(Reporting {
            // Absent means included, the documented default for every event
            // type the stream settings name.
            include_logs: settings
                .stream
                .as_ref()
                .and_then(|stream| stream.include_service_logs)
                .unwrap_or(true),
            scope,
        });

        // Reaches loopback only, so the probe never goes through a proxy the
        // satellite's own environment might name.
        let client = match reqwest::Client::builder()
            .no_proxy()
            .timeout(PROBE_ATTEMPT)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(
                    event.name = "service.probe.unavailable",
                    thread.id = %reporting.scope.thread_id,
                    "could not build the readiness probe client, so no service is started: {error}",
                );
                reporting
                    .incident(
                        ErrorCode::Internal,
                        Disposition::Degraded,
                        &format!(
                            "no service was started this turn, because the readiness probe could \
                             not be built: {error}"
                        ),
                        None,
                    )
                    .await;
                return running;
            }
        };

        let declared_environment = crate::harness::spawn::declared_environment(&settings.env);

        for service in &settings.services {
            let lease = match lease_for(service, leases) {
                Ok(lease) => lease,
                Err(error) => {
                    reporting
                        .start_failed(
                            &service.name,
                            &format!("could not be given a port: {error}"),
                            None,
                        )
                        .await;
                    continue;
                }
            };

            let recipe = Recipe::for_service(
                service,
                lease.port(),
                &reporting.scope.working_dir,
                &declared_environment,
                &running.addresses,
            );

            running.addresses.push(Address {
                name: service.name.clone(),
                port: lease.port(),
            });
            running.leases.push(lease);

            let watched = Watched {
                reporting: Arc::clone(&reporting),
                client: client.clone(),
                tail: Arc::new(Mutex::new(Tail::default())),
                group: Arc::new(AtomicI32::new(0)),
            };

            // The receiver is what a restart waits on alongside its backoff and
            // its probe. Nothing sends on it while the turn is still starting.
            let (stop, mut stopped) = watch::channel(false);

            match start_once(&recipe, &watched, &mut stopped).await {
                Started::Ready(process) => {
                    watched.announce(&recipe).await;

                    running.supervised.push(Supervised {
                        name: recipe.name.clone(),
                        stop,
                        group: Arc::clone(&watched.group),
                        task: tokio::spawn(supervise(process, recipe, watched, stopped)),
                    });
                }
                Started::Failed(reason) => {
                    reporting
                        .start_failed(&recipe.name, &reason, watched.tail_now())
                        .await;
                }
                Started::Stopped => {
                    tracing::debug!(
                        event.name = "service.start.stopped",
                        service.name = %recipe.name,
                        "a service was stopped while it was still starting",
                    );
                }
            }
        }

        running
    }

    /// Where every service that was given a port listens.
    #[must_use]
    pub const fn addresses(&self) -> &Addresses {
        &self.addresses
    }

    /// Stops every service and releases every port, waiting until each is gone.
    ///
    /// All of them are asked at once, so ending a turn costs one grace period
    /// rather than one per service. Returns once every group has been killed and
    /// its output read, so the `service.log` lines a service wrote on its way
    /// out land before the turn's own result.
    pub async fn stop(&mut self) {
        for supervised in &self.supervised {
            supervised.stop.send_replace(true);
        }

        for supervised in self.supervised.drain(..) {
            if let Err(error) = supervised.task.await {
                tracing::error!(
                    event.name = "service.stop.failed",
                    service.name = %supervised.name,
                    "a service's supervisor ended abnormally, so its group is killed directly: \
                     {error}",
                );
                signal_group(supervised.group.load(Ordering::Acquire), Signal::Kill);
            }
        }

        self.leases.clear();
    }
}

/// Kills whatever [`Running::stop`] did not get to.
///
/// A turn future dropped by a satellite shutting down, or by a panic, never
/// reaches `stop`. The supervisors are aborted and every group still held is
/// killed outright, since `Drop` cannot wait out a grace period.
impl Drop for Running {
    fn drop(&mut self) {
        for supervised in &self.supervised {
            supervised.task.abort();
            signal_group(supervised.group.load(Ordering::Acquire), Signal::Kill);
        }
    }
}

/// Leases the port a service declared, or one the satellite chooses.
fn lease_for(service: &Service, leases: &Leases) -> Result<Lease, LeaseError> {
    let Some(declared) = service.port else {
        return leases.assign();
    };

    // Refused at thread creation, so only settings stored before that check
    // can reach this.
    let port = u16::try_from(declared).map_err(|_overflow| LeaseError::NotAPort(declared))?;

    leases.claim(port)
}

/// Everything it takes to start one service, again and again.
#[derive(Debug, Clone)]
struct Recipe {
    name: String,
    command: String,
    port: u16,
    working_dir: PathBuf,
    env: Vec<AgentVar>,
    probe: Probe,
    ready_within: Duration,
}

/// How a service shows it is ready.
#[derive(Debug, Clone)]
enum Probe {
    /// A GET on this path answers 2xx.
    Http(String),

    /// A TCP connection to the port is accepted.
    Connect,
}

impl Recipe {
    /// What one declared service is started with.
    ///
    /// The environment is layered the way a harness's is: the thread's declared
    /// variables first, then everything the satellite decides, so a declared
    /// `PORT` cannot point a service away from the port it was leased.
    fn for_service(
        service: &Service,
        port: u16,
        working_dir: &std::path::Path,
        declared: &[AgentVar],
        earlier: &Addresses,
    ) -> Self {
        let mut env = declared.to_vec();
        env.extend(earlier.environment());
        env.push(AgentVar {
            key: PORT_VARIABLE.to_owned(),
            value: port.to_string(),
            secret: false,
        });

        let probe = service.ready_when.as_ref();

        Self {
            name: service.name.clone(),
            command: service.command.clone(),
            port,
            working_dir: working_dir.to_path_buf(),
            env,
            probe: probe
                .and_then(|probe| probe.http_get.clone())
                .map_or(Probe::Connect, Probe::Http),
            ready_within: crate::timeouts::bound(
                probe.and_then(|probe| probe.timeout.as_ref()),
                crate::timeouts::DEFAULT_SERVICE_READY,
            ),
        }
    }
}

/// What a service's events and incidents are attributed to.
#[derive(Debug)]
struct Reporting {
    scope: TurnScope,

    /// Whether each line a service writes becomes a `service.log` event.
    include_logs: bool,
}

impl Reporting {
    /// Appends one event to the turn.
    async fn event(&self, type_name: &str, payload: Payload) {
        let append = AppendEvent {
            thread_id: self.scope.thread_id.clone(),
            turn_id: Some(self.scope.turn_id.clone()),
            member_id: None,
            type_name: type_name.to_owned(),
            occurred_at: None,
            payload,
            redactor: self.scope.redactor.clone(),
        };

        if let Err(error) = self.scope.store.append_event(append).await {
            tracing::error!(
                event.name = "event.append.failed",
                thread.id = %self.scope.thread_id,
                "could not append a service event: {error}",
            );
        }
    }

    /// Records an incident against the turn, and streams it.
    async fn incident(
        &self,
        code: ErrorCode,
        disposition: Disposition,
        message: &str,
        details: Option<prost_types::Struct>,
    ) {
        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            sequence: None,
            thread_id: Some(self.scope.thread_id.clone()),
            turn_id: Some(self.scope.turn_id.clone()),
            member_id: None,
            code: code.into(),
            disposition: disposition.into(),
            // A service that will not start is a fact about its command and its
            // host, and the same turn submitted again meets the same command.
            retryable: false,
            message: message.to_owned(),
            details,
            occurred_at: Some(Timestamp::now()),
        };

        if let Err(error) = self
            .scope
            .store
            .report_incident(incident, &self.scope.redactor)
            .await
        {
            tracing::error!(
                event.name = "incident.record.failed",
                "could not record a service incident: {error}",
            );
        }
    }

    /// Records a service that did not become ready.
    async fn start_failed(&self, name: &str, reason: &str, tail: Option<String>) {
        tracing::warn!(
            event.name = "service.start.failed",
            thread.id = %self.scope.thread_id,
            turn.id = %self.scope.turn_id,
            service.name = name,
            "a service did not start, and the turn goes on without it: {reason}",
        );

        self.incident(
            ErrorCode::ServiceStartFailed,
            Disposition::Degraded,
            &format!("service {name:?} {reason}, so the turn goes on without it"),
            Some(evidence(name, None, tail)),
        )
        .await;
    }
}

/// What one service's supervisor shares with the process it watches.
#[derive(Debug, Clone)]
struct Watched {
    reporting: Arc<Reporting>,
    client: reqwest::Client,

    /// The service's last lines, across every restart in the turn.
    tail: Arc<Mutex<Tail>>,

    /// The current process group, for the drop backstop.
    group: Arc<AtomicI32>,
}

impl Watched {
    /// The last lines the service wrote, if it wrote any.
    fn tail_now(&self) -> Option<String> {
        self.tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .joined()
    }

    /// Says that a service is ready, on the stream and in the log.
    async fn announce(&self, recipe: &Recipe) {
        let url = format!("http://127.0.0.1:{}", recipe.port);

        tracing::info!(
            event.name = "service.started",
            thread.id = %self.reporting.scope.thread_id,
            turn.id = %self.reporting.scope.turn_id,
            service.name = %recipe.name,
            service.url = %url,
            "service {{service.name}} is ready at {{service.url}}",
        );

        self.reporting
            .event(
                "service.started",
                Payload::ServiceStarted(ServiceStarted {
                    // A thread-level service belongs to no repo.
                    repo: String::new(),
                    service_name: recipe.name.clone(),
                    url,
                    auto_promoted: false,
                    member_id: None,
                }),
            )
            .await;
    }
}

/// How one attempt at starting a service went.
#[derive(Debug)]
enum Started {
    Ready(Process),

    /// It did not become ready, and why, completing "service `name` ...".
    Failed(String),

    /// The turn ended while it was starting.
    Stopped,
}

/// Launches a service and waits for it to become ready.
///
/// A service that does not become ready is gone by the time this returns,
/// whichever way it failed, so a half-started process never outlives the answer.
async fn start_once(
    recipe: &Recipe,
    watched: &Watched,
    stopped: &mut watch::Receiver<bool>,
) -> Started {
    let mut process = match Process::spawn(recipe, watched) {
        Ok(process) => process,
        Err(error) => return Started::Failed(format!("could not be launched: {error}")),
    };

    let readiness = tokio::select! {
        status = process.child.wait() => Readiness::Exited(described(status)),
        () = probing(&watched.client, recipe) => Readiness::Ready,
        () = tokio::time::sleep(recipe.ready_within) => Readiness::TimedOut,
        () = stop_requested(stopped) => Readiness::Stopped,
    };

    match readiness {
        Readiness::Ready => Started::Ready(process),
        Readiness::Exited(status) => {
            process.clear_group(watched).await;
            Started::Failed(format!("exited with {status} before it became ready"))
        }
        Readiness::TimedOut => {
            process.terminate(watched).await;
            Started::Failed(format!(
                "did not pass its readiness probe within {:?}",
                recipe.ready_within
            ))
        }
        Readiness::Stopped => {
            process.terminate(watched).await;
            Started::Stopped
        }
    }
}

/// Resolves once the turn has asked its services to stop.
///
/// A sender that is gone counts as asking: it lives in [`Running`], and once
/// that is gone nothing is left to want the service. The guard `wait_for`
/// answers with is dropped here rather than held by the caller, because it is
/// a lock and a supervisor holding one across its own shutdown could not be
/// moved between threads.
async fn stop_requested(stopped: &mut watch::Receiver<bool>) {
    drop(stopped.wait_for(|stop| *stop).await);
}

/// How the wait for readiness ended.
#[derive(Debug)]
enum Readiness {
    Ready,
    Exited(String),
    TimedOut,
    Stopped,
}

/// Probes until the service answers, however long that takes.
///
/// Bounded by the caller, which races it against the service exiting and the
/// probe's own timeout.
async fn probing(client: &reqwest::Client, recipe: &Recipe) {
    loop {
        let passed = match &recipe.probe {
            Probe::Http(path) => client
                .get(format!("http://127.0.0.1:{}{path}", recipe.port))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success()),
            Probe::Connect => tokio::time::timeout(
                PROBE_ATTEMPT,
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, recipe.port)),
            )
            .await
            .is_ok_and(|connected| connected.is_ok()),
        };

        if passed {
            return;
        }

        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Keeps one ready service up until the turn ends.
///
/// Waits on two things: the service exiting, which earns it a restart while the
/// turn has any left, and the turn ending, which stops it.
async fn supervise(
    mut process: Process,
    recipe: Recipe,
    watched: Watched,
    mut stopped: watch::Receiver<bool>,
) {
    let mut restarts = 0_u32;

    loop {
        let status = tokio::select! {
            status = process.child.wait() => described(status),
            () = stop_requested(&mut stopped) => {
                process.terminate(&watched).await;
                return;
            }
        };

        // The leader is gone, and anything it left behind in its group goes
        // with it, so a restart never shares a port with its own predecessor.
        process.clear_group(&watched).await;

        if restarts == RESTARTS_PER_TURN {
            watched
                .reporting
                .incident(
                    ErrorCode::ServiceExited,
                    Disposition::Degraded,
                    &format!(
                        "service {:?} exited with {status} after {RESTARTS_PER_TURN} restarts \
                         this turn, and is not started again until the next one",
                        recipe.name
                    ),
                    Some(evidence(&recipe.name, Some(&status), watched.tail_now())),
                )
                .await;
            return;
        }

        let backoff = RESTART_BACKOFF.saturating_mul(2_u32.saturating_pow(restarts));
        restarts += 1;

        tracing::warn!(
            event.name = "service.exited",
            thread.id = %watched.reporting.scope.thread_id,
            service.name = %recipe.name,
            service.status = %status,
            "service {{service.name}} exited with {{service.status}} and is being restarted",
        );

        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = stop_requested(&mut stopped) => return,
        }

        process = match start_once(&recipe, &watched, &mut stopped).await {
            Started::Ready(restarted) => {
                // Recovered, and recorded because it recovered: a service
                // revived every few minutes looks exactly like a healthy one.
                watched
                    .reporting
                    .incident(
                        ErrorCode::ServiceExited,
                        Disposition::Recovered,
                        &format!(
                            "service {:?} exited with {status} and was restarted",
                            recipe.name
                        ),
                        Some(evidence(&recipe.name, Some(&status), watched.tail_now())),
                    )
                    .await;
                watched.announce(&recipe).await;
                restarted
            }
            Started::Failed(reason) => {
                watched
                    .reporting
                    .start_failed(
                        &recipe.name,
                        &format!("exited with {status}, and when restarted {reason}"),
                        watched.tail_now(),
                    )
                    .await;
                return;
            }
            Started::Stopped => return,
        };
    }
}

/// One launched service process, and the tasks reading its output.
#[derive(Debug)]
struct Process {
    child: tokio::process::Child,

    /// The process group the service leads, which is its own pid.
    group: i32,

    readers: Vec<JoinHandle<()>>,
}

impl Process {
    /// Launches a service as the agent account, in a process group of its own.
    ///
    /// Through [`crate::harness::spawn::scrubbed_command`], like every process
    /// the satellite starts, so it carries none of the satellite's credentials
    /// and runs as the unprivileged account.
    fn spawn(recipe: &Recipe, watched: &Watched) -> std::io::Result<Self> {
        let (program, flag) = crate::commands::shell();

        let mut command = crate::harness::spawn::scrubbed_command(program);
        command
            .arg(flag)
            .arg(&recipe.command)
            .current_dir(&recipe.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The backstop for a supervisor aborted without stopping its
            // service. It reaches only the shell, which is why the group is
            // what everything else signals.
            .kill_on_drop(true);

        // A group of its own, led by the shell, so its children are stopped
        // with it.
        #[cfg(unix)]
        command.process_group(0);

        for variable in &recipe.env {
            command.env(&variable.key, &variable.value);
        }

        let mut child = command.spawn()?;

        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .unwrap_or_default();
        watched.group.store(group, Ordering::Release);

        let mut readers = Vec::with_capacity(2);

        if let Some(stdout) = child.stdout.take() {
            readers.push(tokio::spawn(relay_lines(
                stdout,
                LogChannel::Stdout,
                recipe.name.clone(),
                watched.clone(),
            )));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(tokio::spawn(relay_lines(
                stderr,
                LogChannel::Stderr,
                recipe.name.clone(),
                watched.clone(),
            )));
        }

        Ok(Self {
            child,
            group,
            readers,
        })
    }

    /// Asks the service's whole group to stop, then makes sure it has.
    async fn terminate(&mut self, watched: &Watched) {
        signal_group(self.group, Signal::Terminate);

        // Anything but a clean exit inside the grace period is answered the same
        // way below, so the result is only worth knowing for the log.
        if tokio::time::timeout(STOP_GRACE, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                event.name = "service.stop.forced",
                thread.id = %watched.reporting.scope.thread_id,
                "a service outlived its {STOP_GRACE:?} grace period and is being killed",
            );
        }

        self.clear_group(watched).await;
    }

    /// Kills whatever is left in the group, reaps the leader, and reads the
    /// pipes to their end.
    async fn clear_group(&mut self, watched: &Watched) {
        signal_group(self.group, Signal::Kill);

        // Only the leader, where there is no group to signal. A no-op for a
        // leader that has already exited.
        drop(self.child.start_kill());
        drop(self.child.wait().await);

        watched.group.store(0, Ordering::Release);

        for reader in self.readers.drain(..) {
            let aborter = reader.abort_handle();

            if tokio::time::timeout(DRAIN_BOUND, reader).await.is_err() {
                aborter.abort();
            }
        }
    }
}

/// Reads one of a service's pipes, line by line, until it closes.
///
/// Every line is kept in the tail, and becomes a `service.log` event unless the
/// thread switched them off. Read as bytes and decoded lossily, because a
/// service printing something that is not UTF-8 must not stop the reader: a
/// pipe nobody reads fills and blocks the service writing to it.
async fn relay_lines(
    pipe: impl AsyncRead + Unpin,
    channel: LogChannel,
    name: String,
    watched: Watched,
) {
    let mut reader = BufReader::new(pipe);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();

        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_read) => {}
        }

        let text = String::from_utf8_lossy(&buffer);
        let line = text.trim_end_matches(['\n', '\r']);

        if line.trim().is_empty() {
            continue;
        }

        watched
            .tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remember(line);

        if watched.reporting.include_logs {
            watched
                .reporting
                .event(
                    "service.log",
                    Payload::ServiceLog(ServiceLog {
                        service_name: name.clone(),
                        channel: channel.into(),
                        line: line.chars().take(MAX_LOG_LINE).collect(),
                    }),
                )
                .await;
        }
    }
}

/// The last lines a service wrote.
#[derive(Debug, Default)]
struct Tail(VecDeque<String>);

impl Tail {
    /// Keeps `line`, dropping the oldest once the ring is full.
    fn remember(&mut self, line: &str) {
        self.0
            .push_back(line.chars().take(TAIL_LINE_CHARS).collect());

        if self.0.len() > TAIL_LINES {
            self.0.pop_front();
        }
    }

    /// What it amounts to, or nothing when the service said nothing.
    fn joined(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| {
            self.0
                .iter()
                .map(String::as_str)
                .collect::<Vec<&str>>()
                .join("\n")
        })
    }
}

/// The evidence an incident about a service carries.
fn evidence(name: &str, status: Option<&str>, tail: Option<String>) -> prost_types::Struct {
    let text = |value: String| prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value)),
    };

    let mut fields = std::collections::BTreeMap::new();
    fields.insert("service".to_owned(), text(name.to_owned()));

    if let Some(status) = status {
        fields.insert("status".to_owned(), text(status.to_owned()));
    }
    if let Some(tail) = tail {
        fields.insert("output_tail".to_owned(), text(tail));
    }

    prost_types::Struct {
        fields: fields.into_iter().collect(),
    }
}

/// A process's exit, as the platform describes it.
fn described(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => status.to_string(),
        Err(error) => format!("a status that could not be read ({error})"),
    }
}

/// What a service's group is asked to do.
#[derive(Debug, Clone, Copy)]
enum Signal {
    /// `SIGTERM`: stop, cleanly.
    Terminate,

    /// `SIGKILL`: stop, now.
    Kill,
}

/// Signals every process in a service's group.
///
/// **Never a group below two.** Zero means the service has no group right now,
/// and `kill(-1)` signals every process the satellite may signal, which inside
/// the image is every process there is. A group that has already emptied is
/// not a failure: that is the state this is trying to reach.
#[cfg(unix)]
fn signal_group(group: i32, signal: Signal) {
    let Some(leader) = (group > 1)
        .then_some(group)
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };

    let sent = match signal {
        Signal::Terminate => rustix::process::Signal::TERM,
        Signal::Kill => rustix::process::Signal::KILL,
    };

    match rustix::process::kill_process_group(leader, sent) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => tracing::warn!(
            event.name = "service.signal.failed",
            service.group = group,
            "could not signal a service's process group: {error}",
        ),
    }
}

/// The same, on a platform with no process groups, where the caller's own
/// kill of the leader is all there is.
#[cfg(not(unix))]
const fn signal_group(_group: i32, _signal: Signal) {}
