// Copyright © 2026 Jalapeno Labs

//! The egress proxy every request an agent makes traverses.
//!
//! # It is not the LLM proxy, and the two stay apart
//!
//! The satellite runs two chokepoints and they answer different questions.
//! [`crate::proxy`] is the **model** chokepoint: it holds the provider
//! credential an agent must never see and it counts the tokens a turn spends.
//! This is the **network** chokepoint: it decides which hosts an agent may
//! reach at all.
//!
//! Keeping them apart is deliberate rather than incidental. Model traffic is
//! exempt from this proxy, by way of `NO_PROXY` naming loopback, so a completion
//! never pays for a second hop and never has its allowlist decision made twice.
//! Folding the two together would also put a live provider credential inside the
//! component whose whole job is relaying whatever an agent asked for, which is
//! the last place it belongs. The preset therefore names no provider host: an
//! agent that reached one directly would be making a model request outside every
//! ceiling the satellite enforces. See [`policy::PRESET_DOMAINS`].
//!
//! # An ordinary HTTP forward proxy, and nothing cleverer
//!
//! Agents are pointed at this listener with `HTTP_PROXY` and `HTTPS_PROXY`, and
//! it speaks what those variables mean: absolute-form requests for plaintext
//! HTTP, and `CONNECT` tunnels for everything wearing TLS.
//!
//! **There is no TLS interception.** The proxy sees the host named in the
//! `CONNECT` line and never the bytes inside the tunnel, which is exactly the
//! granularity the contract's policy needs: `additional_domains` is a list of
//! hosts, so a decision made on the host is a decision made on everything the
//! policy can express. Terminating TLS would buy nothing the policy could use
//! and would cost a certificate authority every agent had to trust, which is a
//! far larger thing to hold than the gate it was meant to serve.
//!
//! **A plaintext request is relayed and the connection is then closed.** The
//! request line is rewritten from absolute form to origin form and
//! `Connection: close` is set, so exactly one exchange happens per connection.
//! The alternative is keeping the connection alive and re-deciding the policy for
//! every pipelined request on it, where a second request naming a different host
//! is a hole the first request opened. Nearly all real traffic is a `CONNECT`
//! tunnel, which pays none of this, so the cost is a keep-alive that plaintext
//! HTTP rarely wanted.
//!
//! # A turn is identified by its credentials, never by its port
//!
//! One listener for the satellite, and a per-turn token carried in
//! `Proxy-Authorization`, following the same grant pattern the model chokepoint
//! already uses.
//!
//! A port per thread was the other candidate and it cannot hold. Every agent on
//! a satellite runs as the same unprivileged account in the same network
//! namespace, so a listening port is discoverable by anything that can open a
//! socket: thread A points its own `HTTPS_PROXY` at thread B's port and spends
//! B's policy, with nothing to forge and nothing to steal. A token is not
//! discoverable. It is minted per turn, handed only to that turn's own child
//! processes, and withdrawn when the turn ends, so the window in which one even
//! exists is the length of one turn.
//!
//! That is a real improvement and it is not a wall, which is worth saying
//! plainly: sibling agents share a uid, so one of them can read another's
//! environment out of `/proc` while that turn is running. The same is true of
//! the model chokepoint's token today. Closing it needs a uid per member, which
//! is the same future work that filesystem scope per member waits on.
//!
//! # What it does not decide
//!
//! Pointing a process at a proxy is an environment variable, and an environment
//! variable is advisory: a process can unset it, and Node's built-in `fetch`
//! never read it in the first place. What makes the allowlist a gate rather than
//! a suggestion is the deployment closing the route around it, which is
//! configuration outside this process. Both layers, and exactly what each one
//! guarantees, are in [the enforcement doc](../../../docs/enforcement.md).

pub mod policy;

use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use base64::Engine as _;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::sync::mpsc::UnboundedSender;

/// The user half of the credentials an agent's proxy URL carries.
///
/// The token is the password half. A user is spelled at all because a proxy URL
/// with an empty one is parsed inconsistently by the clients that have to read
/// it, and a name that says who issued the credential costs nothing.
const PROXY_USER: &str = "arsox";

/// The largest request head the proxy will read before refusing.
///
/// Generous for a real request, whose head is a few hundred bytes, and bounded
/// so a client that opens a connection and never sends a blank line cannot make
/// the satellite buffer without limit.
const MAX_HEAD: usize = 32 * 1024;

/// How long a client has to finish sending its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the proxy waits for an origin to accept a connection.
///
/// Short enough that a black-holed host fails while the agent is still waiting
/// on its own tool call, rather than sitting until some outer bound tears the
/// turn down with nothing to show for it.
const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the accept loop waits after a failure before trying again.
///
/// A transient accept error, a descriptor limit most often, must not spin the
/// loop at full speed and must not take the listener down: a proxy that stopped
/// listening would be an agent whose every connection is refused, which fails
/// closed but says nothing about why.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// What the proxy answers a client that established a tunnel.
const TUNNEL_ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

/// What one turn is allowed to reach through the proxy.
#[derive(Debug, Clone)]
pub struct Grant {
    thread_id: String,
    turn_id: String,

    /// Which hosts this turn's agents may reach.
    policy: policy::WebPolicy,

    /// Where a refusal is sent to be recorded.
    ///
    /// The proxy holds no database, exactly as the model chokepoint does not,
    /// and for the same reason: it sees requests rather than the turn they belong
    /// to the end of. What it finds travels to the runner, which owns the turn's
    /// lifetime and is the one thing that records against it.
    ///
    /// **Direct rather than through the deny spool**, and the difference is
    /// worth stating because the other two gates use the spool. The spool exists
    /// because an exec shim and a `pre-push` hook are separate processes running
    /// as the agent, so a refusal has to cross a privilege boundary, and it
    /// carries no turn id of its own. This proxy runs inside the satellite, was
    /// handed the turn by the token that authorized the request, and can put the
    /// incident on the runner's own channel the instant it happens. Writing a
    /// file for the satellite to read back would be a round trip through the
    /// filesystem to reach a receiver already in scope.
    incidents: UnboundedSender<Incident>,
}

impl Grant {
    /// Admits one turn to the network under `policy`, reporting nowhere.
    #[must_use]
    pub fn new(thread_id: &str, turn_id: &str, policy: policy::WebPolicy) -> Self {
        let (nowhere, _nobody_listening) = tokio::sync::mpsc::unbounded_channel();

        Self {
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.to_owned(),
            policy,
            incidents: nowhere,
        }
    }

    /// Sends what the proxy refuses to `incidents`, which the runner records.
    #[must_use]
    pub fn reporting_to(mut self, incidents: UnboundedSender<Incident>) -> Self {
        self.incidents = incidents;
        self
    }

    /// Records a request the allowlist refused.
    ///
    /// **Blocked**, which is the disposition for a permission gate that closed as
    /// designed. It is not a malfunction and it does not end the turn: the agent
    /// gets a 403 it can read, and the operator gets the evidence that a thread
    /// tried to reach somewhere its policy does not name. That is almost always
    /// how you learn an allowlist is too narrow, which is the whole reason
    /// `blocked` exists.
    fn report_denied(&self, method: &str, destination: &Authority) {
        tracing::info!(
            event.name = "egress.host.denied",
            thread.id = self.thread_id,
            turn.id = self.turn_id,
            server.address = destination.host,
            server.port = destination.port,
            http.request.method = method,
            "refusing an agent request to {{server.address}}: it is not on this thread's \
             web allowlist",
        );

        let mut details = BTreeMap::new();
        details.insert("host".to_owned(), text(&destination.host));
        details.insert("port".to_owned(), number(destination.port));
        details.insert("method".to_owned(), text(method));

        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            // Assigned by the log if this reaches the stream. The proxy appends
            // no events, so it cannot know one.
            sequence: None,
            thread_id: Some(self.thread_id.clone()),
            turn_id: Some(self.turn_id.clone()),
            // The gate is per thread. Attributing a refusal to a member waits for
            // members to exist and for the proxy to be able to tell which one
            // opened a socket.
            member_id: None,
            code: ErrorCode::PermissionDomainDenied.into(),
            disposition: Disposition::Blocked.into(),
            retryable: false,
            message: format!(
                "`{}` is not on this thread's web allowlist, so the request was refused \
                 with 403",
                destination.host
            ),
            details: Some(prost_types::Struct {
                fields: details.into_iter().collect(),
            }),
            occurred_at: Some(Timestamp::now()),
        };

        let _unheard = self.incidents.send(incident);
    }

    /// Records a request the allowlist let through.
    ///
    /// At info for a thread that declared `ALL`, and at debug for every other
    /// policy. A thread on `ALL` asked for visibility instead of enforcement, so
    /// the log **is** the product and hiding it behind a level nobody turns on
    /// would leave that thread with neither. A thread with an allowlist already
    /// has the enforcement, and one line per request at info would bury the
    /// refusals that matter.
    fn note_allowed(&self, method: &str, destination: &Authority) {
        if matches!(self.policy, policy::WebPolicy::Everything) {
            tracing::info!(
                event.name = "egress.host.allowed",
                thread.id = self.thread_id,
                turn.id = self.turn_id,
                server.address = destination.host,
                server.port = destination.port,
                http.request.method = method,
                "an agent reached {{server.address}}, which this thread's policy allows \
                 unconditionally",
            );
            return;
        }

        tracing::debug!(
            event.name = "egress.host.allowed",
            thread.id = self.thread_id,
            turn.id = self.turn_id,
            server.address = destination.host,
            server.port = destination.port,
            http.request.method = method,
            "an agent reached {{server.address}}",
        );
    }
}

/// The forward proxy an agent's ordinary network traffic travels through.
///
/// Cheap to clone: it holds one address and a shared grant table. Held by the
/// runner, which admits a turn when its thread declared a web policy.
#[derive(Clone)]
pub struct EgressProxy {
    /// Live admissions, keyed by the token that presents one.
    ///
    /// Never rendered. See this type's `Debug`.
    grants: Arc<RwLock<HashMap<String, Grant>>>,

    /// Where the proxy listens. Loopback only: it is reachable by anything
    /// inside the container and must be reachable by nothing outside it.
    address: SocketAddr,
}

/// Renders the proxy without its grant table.
///
/// Manual rather than derived, because the table is keyed by the live turn
/// tokens and a derived `Debug` would print every one of them into any log line
/// that ever formatted a proxy. Per M-PUBLIC-DEBUG.
impl std::fmt::Debug for EgressProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressProxy")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl EgressProxy {
    /// Binds the proxy to loopback and starts serving.
    ///
    /// Started unconditionally at boot rather than when the first thread declares
    /// a policy. A listener with no admissions lets nobody through, so an idle
    /// one costs a socket and nothing else, and starting it lazily would mean the
    /// first turn to declare a policy races the bind that has to precede it.
    ///
    /// # Errors
    ///
    /// Returns an error when the loopback listener cannot be bound.
    pub async fn start() -> anyhow::Result<Self> {
        // Port zero: nothing outside the container connects to this and no
        // operator configures it, so a fixed port would only be a collision
        // waiting to happen on a host running several satellites.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;

        let proxy = Self {
            grants: Arc::new(RwLock::new(HashMap::new())),
            address,
        };

        tokio::spawn(accept_forever(listener, proxy.clone()));

        tracing::info!(
            event.name = "satellite.boot.egress_ready",
            server.address = %address,
            "egress proxy listening",
        );

        Ok(proxy)
    }

    /// Where the proxy listens.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Admits one turn, handing back the ticket that withdraws the admission.
    ///
    /// The token is a UUID rather than anything derived from the turn, because a
    /// token an agent can predict is a token it can mint for a turn that is not
    /// its own.
    pub async fn admit(&self, grant: Grant) -> Ticket {
        let token = uuid::Uuid::now_v7().to_string();

        self.grants.write().await.insert(token.clone(), grant);

        Ticket {
            proxy: self.clone(),
            token,
        }
    }

    /// Withdraws a token the moment its turn is over.
    async fn revoke(&self, token: &str) {
        self.grants.write().await.remove(token);
    }

    async fn grant_for(&self, token: &str) -> Option<Grant> {
        // A hash lookup rather than a constant-time comparison, unlike the
        // satellite's own bearer check. What that would defend against is a
        // process inside the container recovering a live token by timing, and
        // that process is a sibling agent sharing a uid, which has the far
        // cheaper route of reading the environment out of `/proc`. Spending a
        // linear scan of every live grant on each request to close the slower of
        // two open doors would be theatre.
        self.grants.read().await.get(token).cloned()
    }
}

/// One turn's admission to the network, withdrawn when it drops.
///
/// The withdrawal rides on the ticket rather than sitting at the end of the
/// runner's happy path, because a turn can end by cancellation, by a harness
/// crash, or by any failure added later, and each of those is a path somebody
/// could forget. A ticket cannot be held without also holding its revoke.
pub struct Ticket {
    proxy: EgressProxy,
    token: String,
}

/// Renders a ticket without the token that presents it.
///
/// The token authorizes a turn's network access, so a derived `Debug` would put
/// it in any log line that ever formatted a turn's context. Per M-PUBLIC-DEBUG.
impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("address", &self.proxy.address)
            .finish_non_exhaustive()
    }
}

impl Ticket {
    /// The proxy URL this turn's agents are pointed at.
    ///
    /// The admission travels as the URL's credentials, which is how every client
    /// that reads `HTTP_PROXY` learns to send `Proxy-Authorization`. The token is
    /// a UUID, so it carries no character that would need escaping in userinfo.
    #[must_use]
    pub fn proxy_url(&self) -> String {
        format!("http://{PROXY_USER}:{}@{}", self.token, self.proxy.address)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // `Drop` cannot await, so the revoke is handed to the runtime. It runs
        // before anything could present the token again: the agent processes
        // that held it are already gone by the time a turn's context drops.
        let proxy = self.proxy.clone();
        let token = std::mem::take(&mut self.token);
        tokio::spawn(async move { proxy.revoke(&token).await });
    }
}

/// Accepts connections until the listener fails permanently.
async fn accept_forever(listener: tokio::net::TcpListener, proxy: EgressProxy) {
    loop {
        match listener.accept().await {
            Ok((client, _peer)) => {
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle(client, proxy).await {
                        // Debug rather than warn: a client that hangs up
                        // mid-request is ordinary, and a proxy that warned about
                        // every abandoned connection would drown the refusals
                        // that matter.
                        tracing::debug!(
                            event.name = "egress.connection.failed",
                            "an agent connection ended early: {error}",
                        );
                    }
                });
            }
            Err(error) => {
                tracing::warn!(
                    event.name = "egress.accept.failed",
                    "could not accept an agent connection: {error}",
                );
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// Serves one client connection: authorize, decide, then relay or refuse.
async fn handle(mut client: TcpStream, proxy: EgressProxy) -> std::io::Result<()> {
    let Some((head, pending)) = read_head(&mut client).await? else {
        return answer(
            &mut client,
            "431 Request Header Fields Too Large",
            "the request head exceeded what the Arsox egress proxy will read",
        )
        .await;
    };

    let Some(request) = std::str::from_utf8(&head).ok().and_then(Head::parse) else {
        return answer(
            &mut client,
            "400 Bad Request",
            "the Arsox egress proxy could not read this request",
        )
        .await;
    };

    // An absent token and an unknown one are the same answer. Saying which would
    // tell a caller whether it had guessed a live turn.
    let Some(presented) = request.presented_token() else {
        return challenge(&mut client).await;
    };
    let Some(grant) = proxy.grant_for(&presented).await else {
        return challenge(&mut client).await;
    };

    let Some(bound) = request.destination() else {
        return answer(
            &mut client,
            "400 Bad Request",
            "the Arsox egress proxy takes absolute-form http requests and CONNECT tunnels",
        )
        .await;
    };

    let destination = bound.authority();

    if !grant.policy.permits(&destination.host) {
        grant.report_denied(request.method, destination);

        return answer(
            &mut client,
            "403 Forbidden",
            &format!(
                "PERMISSION_DOMAIN_DENIED: this thread's web policy does not allow {}",
                destination.host
            ),
        )
        .await;
    }

    grant.note_allowed(request.method, destination);

    let mut origin = match dial(destination).await {
        Ok(origin) => origin,
        Err(error) => {
            tracing::debug!(
                event.name = "egress.origin.unreachable",
                server.address = destination.host,
                server.port = destination.port,
                "could not reach an allowed host: {error}",
            );

            return answer(
                &mut client,
                "502 Bad Gateway",
                &format!(
                    "the Arsox egress proxy could not reach {}: {error}",
                    destination.host
                ),
            )
            .await;
        }
    };

    match &bound {
        // The tunnel is the client's from here. Nothing inside it is read, which
        // is the whole of the no-interception promise.
        Bound::Tunnel(_authority) => client.write_all(TUNNEL_ESTABLISHED).await?,
        Bound::Plain { path, authority } => {
            origin
                .write_all(request.origin_form(path, authority).as_bytes())
                .await?;
        }
    }

    // Whatever the client pipelined behind its head: a request body, or a TLS
    // handshake that arrived in the same segment as the CONNECT. Dropping it
    // would wedge every client that writes both at once.
    if !pending.is_empty() {
        origin.write_all(&pending).await?;
    }

    // Either half closing ends the relay. A reset here is an ordinary way for a
    // transfer to end and is not worth reporting as a proxy failure, so the
    // count is taken and the outcome discarded.
    let _relayed = tokio::io::copy_bidirectional(&mut client, &mut origin).await;

    Ok(())
}

/// Opens a connection to an allowed origin, bounded.
async fn dial(destination: &Authority) -> std::io::Result<TcpStream> {
    tokio::time::timeout(
        ORIGIN_CONNECT_TIMEOUT,
        TcpStream::connect((destination.host.as_str(), destination.port)),
    )
    .await
    .map_err(|_elapsed| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no answer within {ORIGIN_CONNECT_TIMEOUT:?}"),
        )
    })?
}

/// Reads one request head, returning it and whatever arrived behind it.
///
/// `None` when the client sent more head than the proxy will read, which is
/// answered rather than dropped so the agent learns what happened.
async fn read_head(client: &mut TcpStream) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        if let Some(end) = head_ends_at(&buffer) {
            let pending = buffer.split_off(end);
            return Ok(Some((buffer, pending)));
        }

        if buffer.len() >= MAX_HEAD {
            return Ok(None);
        }

        let read = tokio::time::timeout(HEAD_TIMEOUT, client.read(&mut chunk))
            .await
            .map_err(|_elapsed| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no request head within {HEAD_TIMEOUT:?}"),
                )
            })??;

        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the client closed before finishing its request head",
            ));
        }

        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Where the blank line ends the head, if it has arrived.
fn head_ends_at(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// Asks the client for credentials it did not present, or presented wrongly.
async fn challenge(client: &mut TcpStream) -> std::io::Result<()> {
    // The challenge is what makes a client that does not send credentials
    // preemptively try again with them, which several of the image's tools only
    // do once asked.
    let body = "the Arsox egress proxy requires this turn's proxy credentials";

    let response = format!(
        "HTTP/1.1 407 Proxy Authentication Required\r\n\
         Proxy-Authenticate: Basic realm=\"arsox\"\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

/// Writes one final response and closes.
async fn answer(client: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

/// One request head, as a proxy reads it.
#[derive(Debug)]
struct Head<'a> {
    method: &'a str,
    target: &'a str,
    version: &'a str,
    headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Head<'a> {
    /// Splits a head into its request line and its header fields.
    ///
    /// Deliberately permissive about what it keeps and strict about what it
    /// requires: every field is carried through to the origin untouched, and a
    /// head without a well-formed request line is refused rather than guessed at.
    fn parse(text: &'a str) -> Option<Self> {
        let mut lines = text.split("\r\n");

        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?;
        let target = request_line.next()?;
        let version = request_line.next()?;

        if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
            return None;
        }

        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim(), value.trim()))
            .collect();

        Some(Self {
            method,
            target,
            version,
            headers,
        })
    }

    /// The first value of one header field, matched without regard to case.
    fn value(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(field, _value)| field.eq_ignore_ascii_case(name))
            .map(|(_field, value)| *value)
    }

    /// The turn token the client presented, if it presented one.
    ///
    /// Basic credentials rather than a bearer token, because a client derives
    /// this header from the userinfo in `HTTP_PROXY` and that is always Basic.
    /// The user half is not checked: the token is the whole of the secret, and
    /// treating the username as a second factor would only be a second thing to
    /// get wrong in a proxy URL.
    fn presented_token(&self) -> Option<String> {
        let credentials = self.value("proxy-authorization")?;
        let encoded = credentials
            .split_once(' ')
            .filter(|(scheme, _rest)| scheme.eq_ignore_ascii_case("basic"))
            .map(|(_scheme, rest)| rest.trim())?;

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;

        let decoded = String::from_utf8(decoded).ok()?;
        let (_user, token) = decoded.split_once(':')?;

        (!token.is_empty()).then(|| token.to_owned())
    }

    /// Where this request is bound, or `None` when the proxy cannot say.
    ///
    /// A request that reaches a proxy in origin form is a client that thinks it
    /// is talking to an origin server. There is no host to decide on and nowhere
    /// to send it, so it is refused rather than routed by guesswork.
    fn destination(&self) -> Option<Bound> {
        if self.method.eq_ignore_ascii_case("CONNECT") {
            // The port is mandatory in a CONNECT target, and 443 stands in for
            // the clients that omit it anyway.
            return Authority::parse(self.target, 443).map(Bound::Tunnel);
        }

        let rest = self.target.strip_prefix("http://")?;

        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };

        // Userinfo in a request target is nobody's business but the origin's,
        // and it is not part of the host the policy decides on.
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_userinfo, host)| host);

        let path = if tail.is_empty() {
            "/".to_owned()
        } else if tail.starts_with('/') {
            tail.to_owned()
        } else {
            format!("/{tail}")
        };

        Authority::parse(authority, 80).map(|authority| Bound::Plain { path, authority })
    }

    /// This request rewritten for the origin that will answer it.
    ///
    /// Three changes and no others. The target becomes origin form, because an
    /// absolute-form request line means "you are a proxy" and the origin is not
    /// one. The proxy's own hop headers come off, because they end here.
    /// `Connection: close` goes on, so exactly one exchange happens on this
    /// connection and a second request naming a different host cannot ride in
    /// behind a decision made for the first.
    ///
    /// Everything else is carried through untouched, `Transfer-Encoding`
    /// included: the body is relayed as bytes, so a framing the origin needs to
    /// read it has to survive.
    fn origin_form(&self, path: &str, authority: &Authority) -> String {
        let mut head = format!("{} {} {}\r\n", self.method, path, self.version);
        let mut carried_host = false;

        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("proxy-authorization")
                || name.eq_ignore_ascii_case("proxy-connection")
                || name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("keep-alive")
            {
                continue;
            }

            carried_host |= name.eq_ignore_ascii_case("host");
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }

        // A client sending an absolute-form request may leave the header out,
        // and an HTTP/1.1 origin refuses a request without one.
        if !carried_host {
            head.push_str("Host: ");
            head.push_str(&authority.to_string());
            head.push_str("\r\n");
        }

        head.push_str("Connection: close\r\n\r\n");
        head
    }
}

/// Where a request is bound, and how it will get there.
#[derive(Debug, PartialEq, Eq)]
enum Bound {
    /// A tunnel the proxy opens and never reads.
    Tunnel(Authority),

    /// One plaintext exchange the proxy relays.
    Plain { path: String, authority: Authority },
}

impl Bound {
    /// The host and port the policy decides on.
    fn authority(&self) -> &Authority {
        match self {
            Self::Tunnel(authority) | Self::Plain { authority, .. } => authority,
        }
    }
}

/// One host and port a request names.
#[derive(Debug, PartialEq, Eq)]
struct Authority {
    host: String,
    port: u16,
}

impl Authority {
    /// Reads `host`, `host:port`, or `[v6]:port`, defaulting the port.
    ///
    /// An IPv6 literal parses here and is refused by the policy, because nothing
    /// in the contract can name one. That is a stated limit rather than a hole:
    /// the request is denied, and the incident names the address that was tried.
    fn parse(text: &str, default_port: u16) -> Option<Self> {
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            (host, tail.strip_prefix(':'))
        } else if let Some((host, port)) = text.rsplit_once(':') {
            (host, Some(port))
        } else {
            (text, None)
        };

        if host.is_empty() {
            return None;
        }

        let port = match port {
            Some(declared) => declared.parse().ok().filter(|port| *port != 0)?,
            None => default_port,
        };

        Some(Self {
            host: host.to_owned(),
            port,
        })
    }
}

impl std::fmt::Display for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// One string field of an incident's details.
fn text(value: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value.to_owned())),
    }
}

/// One numeric field of an incident's details.
///
/// `google.protobuf.Value` has only a double, which holds every port exactly.
fn number(value: u16) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::NumberValue(f64::from(value))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request head as a client writes one.
    fn head(lines: &[&str]) -> String {
        format!("{}\r\n\r\n", lines.join("\r\n"))
    }

    #[test]
    fn a_connect_names_the_host_the_policy_decides_on() {
        // The whole of what a tunnelled request discloses, and exactly the
        // granularity the contract's domain list needs.
        let text = head(&["CONNECT github.com:443 HTTP/1.1", "Host: github.com:443"]);
        let request = Head::parse(&text).expect("should parse");

        assert_eq!(
            request.destination(),
            Some(Bound::Tunnel(Authority {
                host: "github.com".to_owned(),
                port: 443,
            }))
        );
    }

    #[test]
    fn an_absolute_form_request_yields_its_host_and_its_path() {
        let text = head(&[
            "GET http://example.com/a/b?c=d HTTP/1.1",
            "Host: example.com",
        ]);
        let request = Head::parse(&text).expect("should parse");

        assert_eq!(
            request.destination(),
            Some(Bound::Plain {
                path: "/a/b?c=d".to_owned(),
                authority: Authority {
                    host: "example.com".to_owned(),
                    port: 80,
                },
            })
        );
    }

    #[test]
    fn an_absolute_form_request_without_a_path_is_bound_for_the_root() {
        // `http://example.com` and `http://example.com?q=1` both name the root,
        // and an origin refuses a request line with an empty target.
        for target in ["http://example.com", "http://example.com?q=1"] {
            let text = head(&[&format!("GET {target} HTTP/1.1")]);
            let request = Head::parse(&text).expect("should parse");

            let Some(Bound::Plain { path, .. }) = request.destination() else {
                panic!("{target} should be a plain request");
            };

            assert!(path.starts_with('/'), "{target} produced {path}");
        }
    }

    #[test]
    fn userinfo_in_a_target_is_not_part_of_the_host() {
        // Otherwise `http://github.com@evil.invalid/` would be decided as though
        // it named GitHub, which is the oldest URL confusion there is.
        let text = head(&["GET http://github.com@evil.invalid/x HTTP/1.1"]);
        let request = Head::parse(&text).expect("should parse");
        let bound = request.destination().expect("should name a host");

        assert_eq!(bound.authority().host, "evil.invalid");
    }

    #[test]
    fn a_request_that_is_not_for_a_proxy_names_nowhere() {
        // Origin form means the client believes it is talking to an origin
        // server. There is no host to decide on, so it is refused rather than
        // routed by guesswork.
        let text = head(&["GET /a/b HTTP/1.1", "Host: example.com"]);
        let request = Head::parse(&text).expect("should parse");

        assert_eq!(request.destination(), None);
    }

    #[test]
    fn an_authority_reads_a_port_when_it_is_given_and_defaults_when_it_is_not() {
        assert_eq!(
            Authority::parse("example.com:8443", 80),
            Some(Authority {
                host: "example.com".to_owned(),
                port: 8443,
            })
        );
        assert_eq!(
            Authority::parse("example.com", 443).map(|authority| authority.port),
            Some(443)
        );
        assert_eq!(
            Authority::parse("[::1]:8080", 443).map(|authority| authority.host),
            Some("::1".to_owned())
        );

        // Nothing usable, rather than a host of "" or a port of zero.
        assert_eq!(Authority::parse("", 80), None);
        assert_eq!(Authority::parse(":443", 80), None);
        assert_eq!(Authority::parse("example.com:0", 80), None);
        assert_eq!(Authority::parse("example.com:http", 80), None);
    }

    #[test]
    fn a_basic_credential_yields_the_token_it_carries() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("arsox:the-turns-token");
        let text = head(&[
            "CONNECT example.com:443 HTTP/1.1",
            &format!("Proxy-Authorization: Basic {encoded}"),
        ]);

        assert_eq!(
            Head::parse(&text).expect("should parse").presented_token(),
            Some("the-turns-token".to_owned())
        );
    }

    #[test]
    fn anything_that_is_not_a_basic_credential_presents_no_token() {
        // Each of these has to reach the 407, not a lookup for a token that was
        // never really presented.
        for credential in [
            "Bearer the-turns-token",
            "Basic not-base64!!",
            "Basic",
            "Basic aGFzLW5vLWNvbG9u",
        ] {
            let text = head(&[
                "CONNECT example.com:443 HTTP/1.1",
                &format!("Proxy-Authorization: {credential}"),
            ]);

            assert_eq!(
                Head::parse(&text).expect("should parse").presented_token(),
                None,
                "{credential}"
            );
        }

        let text = head(&["CONNECT example.com:443 HTTP/1.1"]);
        assert_eq!(
            Head::parse(&text).expect("should parse").presented_token(),
            None
        );
    }

    #[test]
    fn the_user_half_of_the_credential_is_not_a_second_factor() {
        // The token is the whole of the secret. Checking the username would only
        // be a second thing to get wrong in a proxy URL.
        let encoded = base64::engine::general_purpose::STANDARD.encode("somebody:the-turns-token");
        let text = head(&[
            "CONNECT example.com:443 HTTP/1.1",
            &format!("Proxy-Authorization: Basic {encoded}"),
        ]);

        assert_eq!(
            Head::parse(&text).expect("should parse").presented_token(),
            Some("the-turns-token".to_owned())
        );
    }

    #[test]
    fn a_relayed_request_loses_the_proxy_hop_and_keeps_everything_else() {
        let text = head(&[
            "POST http://example.com/submit HTTP/1.1",
            "Host: example.com",
            "Proxy-Authorization: Basic YTpi",
            "Proxy-Connection: keep-alive",
            "Connection: keep-alive",
            "Transfer-Encoding: chunked",
            "User-Agent: curl/8.5.0",
        ]);
        let request = Head::parse(&text).expect("should parse");

        let Some(Bound::Plain { path, authority }) = request.destination() else {
            panic!("should be a plain request");
        };
        let rewritten = request.origin_form(&path, &authority);

        // Origin form: an absolute target means "you are a proxy" and the origin
        // is not one.
        assert!(
            rewritten.starts_with("POST /submit HTTP/1.1\r\n"),
            "{rewritten}"
        );

        assert!(!rewritten.contains("Proxy-Authorization"), "{rewritten}");
        assert!(!rewritten.contains("Proxy-Connection"), "{rewritten}");
        assert!(!rewritten.contains("keep-alive"), "{rewritten}");

        // The framing the origin needs in order to read the body it is about to
        // be handed as bytes.
        assert!(
            rewritten.contains("Transfer-Encoding: chunked\r\n"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("User-Agent: curl/8.5.0\r\n"),
            "{rewritten}"
        );

        // One exchange per connection, so a second request naming another host
        // cannot ride in behind a decision made for the first.
        assert!(
            rewritten.ends_with("Connection: close\r\n\r\n"),
            "{rewritten}"
        );
    }

    #[test]
    fn a_relayed_request_carries_a_host_the_client_left_out() {
        // An HTTP/1.1 origin refuses a request without one, and a client sending
        // absolute form has already said where it is going.
        let text = head(&["GET http://example.com:8080/x HTTP/1.1"]);
        let request = Head::parse(&text).expect("should parse");

        let Some(Bound::Plain { path, authority }) = request.destination() else {
            panic!("should be a plain request");
        };

        assert!(
            request
                .origin_form(&path, &authority)
                .contains("Host: example.com:8080\r\n")
        );
    }

    #[test]
    fn a_head_without_a_request_line_is_refused_rather_than_guessed_at() {
        for malformed in [
            "",
            "GET\r\n\r\n",
            "GET / SPDY/3\r\n\r\n",
            "not a request\r\n\r\n",
        ] {
            assert!(Head::parse(malformed).is_none(), "{malformed:?}");
        }
    }

    #[test]
    fn the_head_ends_at_the_blank_line_and_the_rest_is_the_client_s() {
        // The bytes behind the head are a request body, or a TLS handshake that
        // arrived in the same segment as the CONNECT. Losing them wedges the
        // client that wrote both at once.
        let stream = b"GET http://a/ HTTP/1.1\r\n\r\nbody-bytes";

        assert_eq!(
            head_ends_at(stream),
            Some(stream.len() - "body-bytes".len())
        );
        assert_eq!(head_ends_at(b"GET http://a/ HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn a_ticket_carries_the_admission_in_its_url() {
        let proxy = EgressProxy::start().await.expect("should start");
        let ticket = proxy
            .admit(Grant::new("thread", "turn", policy::WebPolicy::Everything))
            .await;

        let url = ticket.proxy_url();

        assert!(url.starts_with("http://arsox:"), "{url}");
        assert!(url.ends_with(&format!("@{}", proxy.address())), "{url}");
    }

    #[tokio::test]
    async fn a_ticket_never_renders_the_admission_it_carries() {
        // A derived `Debug` would put a live turn's network credential into any
        // log line that ever formatted the turn's context.
        let proxy = EgressProxy::start().await.expect("should start");
        let ticket = proxy
            .admit(Grant::new("thread", "turn", policy::WebPolicy::Everything))
            .await;

        let token = ticket
            .proxy_url()
            .rsplit_once('@')
            .and_then(|(credentials, _address)| credentials.rsplit_once(':'))
            .map(|(_user, token)| token.to_owned())
            .expect("the url carries a token");

        assert!(!format!("{ticket:?}").contains(&token));
        assert!(!format!("{proxy:?}").contains(&token));
    }

    #[tokio::test]
    async fn an_admission_is_withdrawn_when_its_ticket_drops() {
        // A grant that outlived its turn would let a process that kept a handle
        // to the token keep reaching the network after the work stopped.
        let proxy = EgressProxy::start().await.expect("should start");
        let ticket = proxy
            .admit(Grant::new("thread", "turn", policy::WebPolicy::Everything))
            .await;

        let token = ticket
            .proxy_url()
            .rsplit_once('@')
            .and_then(|(credentials, _address)| credentials.rsplit_once(':'))
            .map(|(_user, token)| token.to_owned())
            .expect("the url carries a token");

        assert!(proxy.grant_for(&token).await.is_some());

        drop(ticket);
        tokio::task::yield_now().await;

        assert!(proxy.grant_for(&token).await.is_none());
    }
}
