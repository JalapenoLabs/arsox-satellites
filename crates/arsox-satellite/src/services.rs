// Copyright © 2026 Jalapeno Labs

//! The long-running processes a thread declares, one set per turn.
//!
//! A host application sometimes needs a helper running beside the agent rather
//! than a command the agent runs: a headless editor bridge and the MCP server
//! that talks to it, a dev server a browser is pointed at. A thread declares
//! them in `ThreadSettings.services`, and this module is the one place that
//! decides what a declaration must look like, what each service is called in an
//! environment, and which loopback port it listens on.
//!
//! # One set per turn, never shared
//!
//! Before a turn's harness starts, [`Running::start`] starts the thread's
//! services in declaration order, each waiting on its readiness probe, and
//! [`Running::stop`] stops all of them when the turn ends, however it ends. Two
//! threads running at once therefore never share a service process or anything
//! it holds in memory, and a thread idling for a week holds no process at all.
//! What a service keeps in memory does not survive into the next turn; anything
//! worth keeping belongs in the workspace.
//!
//! # Ports are leased, satellite wide
//!
//! A service with no declared port is given one. Asking the kernel for a free
//! port and then releasing it for the service to bind leaves a window in which
//! anything else can take it, and the likeliest thing to take it is another
//! turn's service starting at the same moment. So every port a turn's services
//! use, assigned or declared, is held in [`Leases`] for the whole turn, and no
//! other turn on the satellite is handed it. A process outside the satellite can
//! still race for an assigned port in that window; the service then fails its
//! readiness probe and says so, rather than silently serving somebody else.
//!
//! The same lease is what keeps a declared port honest. Two threads may both
//! declare port 8000, and the second one to start is refused the port rather
//! than having its readiness probe answered by the first thread's process.
//!
//! # Names become variables
//!
//! Every service is told its own port as `PORT`, and every service after it and
//! the harness are told where it listens as `ARSOX_SERVICE_<NAME>_PORT` and
//! `ARSOX_SERVICE_<NAME>_URL`. `<NAME>` is the declared name upper-cased with `-`
//! written as `_`, so [`refusal`] requires names to stay unique through that
//! mapping: `web-app` and `web_app` would otherwise be one set of variables
//! naming two services.
//!
//! These are the one `ARSOX_` family an agent is given. Every other `ARSOX_*`
//! variable is scrubbed from every spawn, and a thread may not declare one, so a
//! declared variable cannot forge a service address either.

mod running;

pub use running::{Running, TurnScope};

use crate::harness::spawn::AgentVar;
use arsox_sdk::proto::settings::v1::{Service, ServiceIsolation, ThreadSettings};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Most services one thread may declare.
///
/// Each is a process started before every turn and waited on in order, so a
/// long list is a slow start to every turn. Sixteen is far past any helper set
/// a real host application runs, and a limit reached here is worth raising
/// deliberately.
pub const MAX_SERVICES: usize = 16;

/// Longest service name, the same bound an MCP server name has.
pub const MAX_NAME: usize = 64;

/// Longest service command, in bytes.
///
/// A command is a process argument, and Linux caps one argument at 128 KiB.
/// Sixteen KiB holds any real launch line with room to spare, and keeps a
/// thread's services from spending the argument space its harness shares.
pub const MAX_COMMAND: usize = 16 * 1024;

/// The lowest port a service may declare.
///
/// Services run as the unprivileged agent account, which cannot bind below this,
/// so a lower port would be a service that fails every turn for a reason its
/// declaration could have been told about.
pub const MIN_DECLARED_PORT: u32 = 1024;

/// Longest path a readiness probe or a service's MCP endpoint may name.
pub const MAX_PATH: usize = 2048;

/// The variable every service reads its own port from.
///
/// The convention nearly every server framework already honours, so a service
/// command is usually just the framework's own start command.
pub const PORT_VARIABLE: &str = "PORT";

/// Why a thread's service declarations may not be used, when they may not.
///
/// Two refusals live here. A service declared on a repo is refused outright,
/// because services are declared on the thread and nothing reads a repo's list;
/// accepting one would be a declaration that silently does nothing. And every
/// thread service must have a name that maps to unique variables, a command, a
/// usable port if it names one, a usable probe path, and an isolation the
/// satellite implements.
///
/// # Errors
///
/// Returns the first reason found, naming the settings field and the service,
/// which is enough for a caller to fix and resubmit.
pub fn refusal(settings: &ThreadSettings) -> Result<(), String> {
    for repo in &settings.repos {
        if !repo.services.is_empty() {
            return Err(format!(
                "settings.repos: repo {:?} declares services, which are declared on the thread \
                 in settings.services and started for each of its turns",
                repo.name
            ));
        }
    }

    let declared = &settings.services;

    if declared.len() > MAX_SERVICES {
        return Err(format!(
            "settings.services: declares {} services, and a thread may declare at most \
             {MAX_SERVICES}",
            declared.len()
        ));
    }

    let mut variables = BTreeSet::new();
    let mut ports = BTreeSet::new();

    for service in declared {
        if let Some(reason) = service_refusal(service) {
            return Err(format!("settings.services: {reason}"));
        }

        if !variables.insert(variable_stem(&service.name)) {
            return Err(format!(
                "settings.services: service {:?} maps to the same {} variables as another \
                 service, once upper-cased with '-' written as '_'",
                service.name,
                variable_stem(&service.name)
            ));
        }

        if let Some(port) = service.port
            && !ports.insert(port)
        {
            return Err(format!(
                "settings.services: service {:?} declares port {port}, which another service \
                 on this thread already declares",
                service.name
            ));
        }
    }

    Ok(())
}

/// Why one service may not be started, completing "settings.services: ...".
fn service_refusal(service: &Service) -> Option<String> {
    let name = &service.name;

    if name.is_empty() || name.len() > MAX_NAME || !crate::harness::mcp::is_identifier(name) {
        return Some(format!(
            "service {name:?} needs a name of 1 to {MAX_NAME} ASCII letters, digits, '_', and '-'"
        ));
    }

    let command = service.command.trim();
    if command.is_empty() {
        return Some(format!("service {name:?} needs a command to run"));
    }

    // A NUL cannot travel in a process argument at all, so the launch would fail
    // every turn for a reason nothing in the failure would explain.
    if service.command.len() > MAX_COMMAND || service.command.contains('\0') {
        return Some(format!(
            "service {name:?} needs a command of at most {MAX_COMMAND} bytes with no NUL"
        ));
    }

    if let Some(port) = service.port
        && !(MIN_DECLARED_PORT..=u32::from(u16::MAX)).contains(&port)
    {
        return Some(format!(
            "service {name:?} declares port {port}, and a declared port must be from \
             {MIN_DECLARED_PORT} to 65535, since the agent account cannot bind below that"
        ));
    }

    if let Some(path) = service
        .ready_when
        .as_ref()
        .and_then(|probe| probe.http_get.as_deref())
        && let Some(reason) = path_refusal(path)
    {
        return Some(format!(
            "service {name:?} has a readiness path that {reason}"
        ));
    }

    match ServiceIsolation::try_from(service.isolation) {
        Ok(ServiceIsolation::Unspecified | ServiceIsolation::Shared) => None,
        Ok(ServiceIsolation::PerMember) => Some(format!(
            "service {name:?} asks for PER_MEMBER isolation, which is not implemented: each \
             turn starts one instance shared by its agents, so declare SHARED or leave it unset"
        )),
        Err(_unknown) => Some(format!(
            "service {name:?} names an isolation this satellite does not know"
        )),
    }
}

/// Why a path on a service's loopback port may not be used, when it may not.
///
/// Completes "the path ...". Shared by a readiness probe and by an MCP server
/// reached through a service, which both become `http://127.0.0.1:<port><path>`.
/// A path that parsed onto any other host would send a probe, or an agent's
/// MCP client, somewhere the declaration never named.
pub(crate) fn path_refusal(path: &str) -> Option<&'static str> {
    if !path.starts_with('/') {
        return Some("does not start with '/'");
    }

    if path.len() > MAX_PATH {
        return Some("is longer than 2048 characters");
    }

    if path
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Some("contains whitespace or a control character");
    }

    // Claude expands `${NAME}` in a URL from the harness's environment, where
    // other servers' header values live.
    if path.contains("${") {
        return Some("contains \"${\", which the harness would expand");
    }

    // Any port serves here. What is checked is that nothing in the path can
    // move the host or the port a client parses out of the whole URL.
    let parsed = reqwest::Url::parse(&format!("http://127.0.0.1:1024{path}")).ok();
    let stays_on_loopback =
        parsed.is_some_and(|url| url.host_str() == Some("127.0.0.1") && url.port() == Some(1024));
    if !stays_on_loopback {
        return Some("does not stay on the service's own address");
    }

    None
}

/// The prefix every variable naming one service starts with.
///
/// `ARSOX_SERVICE_` and the name upper-cased with `-` written as `_`. Those are
/// the only characters a name may hold besides ASCII letters and digits, so the
/// result is always a variable name every shell accepts.
#[must_use]
pub fn variable_stem(name: &str) -> String {
    format!(
        "ARSOX_SERVICE_{}",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// Where one service listens for this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub name: String,
    pub port: u16,
}

impl Address {
    /// The URL a client reaches the service at.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The two variables that tell a process where this service listens.
    ///
    /// Neither is a credential: a loopback address is worth reading in a log
    /// when an agent cannot reach its service.
    fn variables(&self) -> [AgentVar; 2] {
        let stem = variable_stem(&self.name);

        [
            AgentVar {
                key: format!("{stem}_PORT"),
                value: self.port.to_string(),
                secret: false,
            },
            AgentVar {
                key: format!("{stem}_URL"),
                value: self.url(),
                secret: false,
            },
        ]
    }
}

/// Where a turn's services listen, in declaration order.
///
/// Every declared service that was given a port is here, whether or not it
/// became ready. An agent told nothing about a service that failed would go
/// looking for it; an agent told its address meets a refused connection, which
/// says exactly what happened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addresses(Vec<Address>);

impl Addresses {
    /// Where the named service listens, when it was given a port.
    ///
    /// Matched exactly, the way the name was declared.
    #[must_use]
    pub fn port_of(&self, name: &str) -> Option<u16> {
        self.0
            .iter()
            .find(|address| address.name == name)
            .map(|address| address.port)
    }

    /// The variables that tell a process where every one of these listens.
    #[must_use]
    pub fn environment(&self) -> Vec<AgentVar> {
        self.0.iter().flat_map(Address::variables).collect()
    }

    fn push(&mut self, address: Address) {
        self.0.push(address);
    }
}

impl FromIterator<Address> for Addresses {
    fn from_iter<Iterated: IntoIterator<Item = Address>>(iter: Iterated) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// How many times [`Leases::assign`] asks the kernel before giving up.
///
/// A port the kernel hands out is one already leased by another turn only when
/// the kernel has cycled back to it, which is rare; eight in a row means
/// something is wrong with the host rather than unlucky.
const ASSIGN_ATTEMPTS: usize = 8;

/// The loopback ports this satellite's running services hold.
///
/// Shared by every turn, which is the point: a port leased to one turn is never
/// handed to another until the first turn's services have stopped.
#[derive(Debug, Clone, Default)]
pub struct Leases(Arc<Mutex<BTreeSet<u16>>>);

/// Why a port could not be leased.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("port {0} is held by a service in another turn on this satellite")]
    Leased(u16),

    #[error("port {port} is already in use on this host: {source}")]
    InUse { port: u16, source: std::io::Error },

    #[error("no free loopback port could be found: {0}")]
    Exhausted(std::io::Error),

    #[error("{0} is not a port")]
    NotAPort(u32),
}

impl Leases {
    /// Leases a free loopback port the kernel chooses.
    ///
    /// The kernel is asked by binding port zero, and the listener is dropped at
    /// once so the service can bind the port itself. That release is the window
    /// the module docs describe, and the lease is what keeps every other turn on
    /// this satellite out of it.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Exhausted`] when the kernel will not hand out a
    /// port, or keeps handing out ones already leased.
    pub fn assign(&self) -> Result<Lease, LeaseError> {
        let mut last_error = std::io::Error::other("every port offered was already leased");

        for _attempt in 0..ASSIGN_ATTEMPTS {
            let offered = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .and_then(|listener| listener.local_addr());

            let port = match offered {
                Ok(address) => address.port(),
                Err(error) => {
                    last_error = error;
                    continue;
                }
            };

            if self.lock().insert(port) {
                return Ok(Lease {
                    port,
                    leases: self.clone(),
                });
            }
        }

        Err(LeaseError::Exhausted(last_error))
    }

    /// Leases a port a service declared, when it is free.
    ///
    /// Checked by binding it, which is the only honest test of "free". A port
    /// already bound is refused rather than handed over, because a readiness
    /// probe against it would be answered by whatever holds it and the agent
    /// would be told that process was its service.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Leased`] when another turn's service holds it, and
    /// [`LeaseError::InUse`] when something outside the satellite does.
    pub fn claim(&self, port: u16) -> Result<Lease, LeaseError> {
        let mut held = self.lock();

        if held.contains(&port) {
            return Err(LeaseError::Leased(port));
        }

        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|source| LeaseError::InUse { port, source })?;

        held.insert(port);

        Ok(Lease {
            port,
            leases: self.clone(),
        })
    }

    /// The set, recovered from a poisoned lock.
    ///
    /// Every write is a single insert or remove, so a panic elsewhere cannot
    /// leave it half-changed, and refusing every future turn a port over
    /// somebody else's panic would be the worse failure.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<u16>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One port held for one turn, released when it drops.
///
/// A guard rather than a release call, because a turn leaves its services by
/// every path a turn can end on, and a port that stayed leased after its turn
/// would be a port no later turn could use.
#[derive(Debug)]
pub struct Lease {
    port: u16,
    leases: Leases,
}

impl Lease {
    /// The port held.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.leases.lock().remove(&self.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::settings::v1::{ReadinessProbe, Repo};

    /// A service as the SDK would send one.
    fn service(name: &str) -> Service {
        Service {
            name: name.to_owned(),
            command: "python3 -m http.server $PORT".to_owned(),
            ..Service::default()
        }
    }

    fn settings_with(services: Vec<Service>) -> ThreadSettings {
        ThreadSettings {
            services,
            ..ThreadSettings::default()
        }
    }

    #[test]
    fn an_ordinary_set_of_services_is_accepted() {
        let services = vec![
            service("blender"),
            Service {
                port: Some(8000),
                ready_when: Some(ReadinessProbe {
                    http_get: Some("/health".to_owned()),
                    timeout: None,
                }),
                isolation: ServiceIsolation::Shared.into(),
                ..service("blender-mcp")
            },
        ];

        assert_eq!(refusal(&settings_with(services)), Ok(()));
        assert_eq!(refusal(&ThreadSettings::default()), Ok(()));
    }

    #[test]
    fn a_service_declared_on_a_repo_is_refused_rather_than_ignored() {
        // Nothing reads a repo's list, so accepting it would be a declaration
        // that silently does nothing.
        let settings = ThreadSettings {
            repos: vec![Repo {
                name: "web".to_owned(),
                services: vec![service("dev")],
                ..Repo::default()
            }],
            ..ThreadSettings::default()
        };

        let error = refusal(&settings).expect_err("should be refused");
        assert!(error.starts_with("settings.repos:"), "{error}");
        assert!(error.contains("settings.services"), "{error}");
    }

    #[test]
    fn a_name_is_an_identifier_of_bounded_length() {
        for name in ["", "has.dot", "has space", "quote\"d", "ünïcode"] {
            let error = refusal(&settings_with(vec![service(name)])).expect_err("should refuse");
            assert!(error.contains("name"), "{name:?}: {error}");
        }

        let long = "a".repeat(MAX_NAME + 1);
        refusal(&settings_with(vec![service(&long)])).expect_err("a long name should be refused");
    }

    #[test]
    fn two_names_that_map_to_one_set_of_variables_are_refused() {
        // `web-app` and `WEB_APP` are both ARSOX_SERVICE_WEB_APP_*, so one of
        // them would silently shadow the other in every environment.
        let error = refusal(&settings_with(vec![service("web-app"), service("WEB_APP")]))
            .expect_err("should be refused");

        assert!(error.contains("ARSOX_SERVICE_WEB_APP"), "{error}");
    }

    #[test]
    fn a_service_needs_a_command() {
        for command in ["", "   \n\t", "run\0this"] {
            let declared = Service {
                command: command.to_owned(),
                ..service("web")
            };
            refusal(&settings_with(vec![declared])).expect_err("should be refused");
        }
    }

    #[test]
    fn a_declared_port_is_unprivileged_in_range_and_unique() {
        for port in [0, 80, 1023, 65_536] {
            let declared = Service {
                port: Some(port),
                ..service("web")
            };
            let error = refusal(&settings_with(vec![declared])).expect_err("should be refused");
            assert!(error.contains("port"), "{port}: {error}");
        }

        let twice = vec![
            Service {
                port: Some(8000),
                ..service("one")
            },
            Service {
                port: Some(8000),
                ..service("two")
            },
        ];
        let error = refusal(&settings_with(twice)).expect_err("should be refused");
        assert!(error.contains("already declares"), "{error}");
    }

    #[test]
    fn per_member_isolation_is_refused_until_it_exists() {
        let declared = Service {
            isolation: ServiceIsolation::PerMember.into(),
            ..service("web")
        };

        let error = refusal(&settings_with(vec![declared])).expect_err("should be refused");
        assert!(error.contains("PER_MEMBER"), "{error}");

        let unknown = Service {
            isolation: 99,
            ..service("web")
        };
        refusal(&settings_with(vec![unknown])).expect_err("an unknown value should be refused");
    }

    #[test]
    fn a_readiness_path_stays_on_the_services_own_address() {
        for path in [
            "health",
            "/has space",
            "/line\nbreak",
            "/${HOME}",
            &format!("/{}", "a".repeat(MAX_PATH)),
        ] {
            let declared = Service {
                ready_when: Some(ReadinessProbe {
                    http_get: Some(path.to_owned()),
                    timeout: None,
                }),
                ..service("web")
            };

            let error = refusal(&settings_with(vec![declared])).expect_err("should be refused");
            assert!(error.contains("readiness path"), "{path:?}: {error}");
        }

        for path in [
            "/",
            "/health",
            "/v1/ready?deep=true",
            "/@elsewhere.example.com",
        ] {
            assert_eq!(path_refusal(path), None, "{path:?}");
        }
    }

    #[test]
    fn the_limit_is_counted() {
        let many = (0..=MAX_SERVICES)
            .map(|index| service(&format!("service-{index}")))
            .collect();

        refusal(&settings_with(many)).expect_err("too many services should be refused");
    }

    #[test]
    fn a_name_becomes_one_variable_stem() {
        assert_eq!(variable_stem("blender"), "ARSOX_SERVICE_BLENDER");
        assert_eq!(variable_stem("blender-mcp"), "ARSOX_SERVICE_BLENDER_MCP");
        assert_eq!(variable_stem("Web_2"), "ARSOX_SERVICE_WEB_2");
    }

    #[test]
    fn addresses_render_a_port_and_a_url_per_service_in_declaration_order() {
        let addresses: Addresses = [
            Address {
                name: "blender".to_owned(),
                port: 41_000,
            },
            Address {
                name: "blender-mcp".to_owned(),
                port: 41_001,
            },
        ]
        .into_iter()
        .collect();

        let pairs: Vec<(String, String)> = addresses
            .environment()
            .into_iter()
            .map(|variable| (variable.key, variable.value))
            .collect();

        assert_eq!(
            pairs,
            [
                ("ARSOX_SERVICE_BLENDER_PORT", "41000"),
                ("ARSOX_SERVICE_BLENDER_URL", "http://127.0.0.1:41000"),
                ("ARSOX_SERVICE_BLENDER_MCP_PORT", "41001"),
                ("ARSOX_SERVICE_BLENDER_MCP_URL", "http://127.0.0.1:41001"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert_eq!(addresses.port_of("blender-mcp"), Some(41_001));
        assert_eq!(addresses.port_of("BLENDER"), None, "matched as declared");
    }

    #[test]
    fn an_assigned_port_is_never_handed_to_a_second_lease_while_the_first_holds_it() {
        let leases = Leases::default();

        let first = leases.assign().expect("should assign a port");
        let second = leases.assign().expect("should assign another port");
        assert_ne!(first.port(), second.port());

        // Held by this satellite, so claiming it for another turn is refused
        // even though nothing is listening on it yet.
        assert!(matches!(
            leases.claim(first.port()),
            Err(LeaseError::Leased(port)) if port == first.port()
        ));

        let released = first.port();
        drop(first);
        drop(
            leases
                .claim(released)
                .expect("a released port can be leased again"),
        );
    }

    #[test]
    fn a_declared_port_something_else_is_bound_to_is_refused() {
        let holder = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("should bind a port");
        let port = holder.local_addr().expect("should read the port").port();

        assert!(matches!(
            Leases::default().claim(port),
            Err(LeaseError::InUse { .. })
        ));
    }
}
