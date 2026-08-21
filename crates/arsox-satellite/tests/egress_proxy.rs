// Copyright © 2026 Jalapeno Labs

//! The egress proxy, driven over a real socket against real destinations.
//!
//! What these assert is the property the gate exists for: a host the thread's
//! policy names is reached, a host it does not name is refused with a code the
//! agent can read and an incident the operator can query, and one turn's
//! admission cannot spend another turn's policy.
//!
//! **The wire is driven by hand for the tunnel cases** and by a real HTTP client
//! for the relayed one. Raw TCP is what makes a `CONNECT` assertion exact: the
//! status line, the bytes relayed after it, and the refusal that arrives instead
//! are all read as they were written. The `reqwest` case covers the half raw TCP
//! cannot, which is whether an ordinary client pointed at this proxy through a
//! URL actually presents the credentials that URL carries.
//!
//! **The `NO_PROXY` exemption is not here**, deliberately. It is honoured by the
//! client rather than by the proxy, so nothing this listener does could prove it.
//! What is provable is that the variable is handed over correctly, and that is
//! asserted where it is built, in `harness::spawn`'s
//! `model_traffic_keeps_its_own_chokepoint`.

use arsox_satellite::egress::policy::WebPolicy;
use arsox_satellite::egress::{EgressProxy, Grant, Ticket};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use base64::Engine as _;
use std::collections::BTreeSet;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

/// A policy naming exactly these hosts.
fn allowing(hosts: &[&str]) -> WebPolicy {
    WebPolicy::Only(
        hosts
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<BTreeSet<String>>(),
    )
}

/// A proxy with one turn admitted, and the channel its refusals arrive on.
async fn admitted(
    thread_id: &str,
    policy: WebPolicy,
) -> (EgressProxy, Ticket, UnboundedReceiver<Incident>) {
    let proxy = EgressProxy::start().await.expect("should start the proxy");
    let (incidents, reported) = tokio::sync::mpsc::unbounded_channel();

    let ticket = proxy
        .admit(Grant::new(thread_id, "the-turn", policy).reporting_to(incidents))
        .await;

    (proxy, ticket, reported)
}

/// The `Proxy-Authorization` value a client derives from a proxy URL.
///
/// Extracted from the URL rather than from the proxy's internals, because the
/// URL is the whole of what an agent is handed and anything a test could read
/// past it would be testing something no client can see.
fn credentials(ticket: &Ticket) -> String {
    let url = ticket.proxy_url();
    let userinfo = url
        .trim_start_matches("http://")
        .rsplit_once('@')
        .map(|(userinfo, _address)| userinfo.to_owned())
        .expect("the proxy url carries credentials");

    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(userinfo)
    )
}

/// A destination that answers whatever is said to it.
///
/// Stands in for an origin without needing TLS, a certificate, or a name: the
/// proxy relays bytes once a tunnel is open, so an echo is the whole of what a
/// tunnel test needs on the far end.
async fn echo_destination() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the destination");
    let port = listener
        .local_addr()
        .expect("should have an address")
        .port();

    tokio::spawn(async move {
        while let Ok((mut stream, _peer)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0_u8; 1024];

                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 || stream.write_all(&buffer[..read]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    port
}

/// Opens a connection to the proxy and writes one request head.
async fn request(proxy: &EgressProxy, head: &str) -> TcpStream {
    let mut client = TcpStream::connect(proxy.address())
        .await
        .expect("should reach the proxy");

    client
        .write_all(head.as_bytes())
        .await
        .expect("should send the request");

    client
}

/// A `CONNECT` head bound for `port` on loopback, with credentials or without.
fn connect_head(port: u16, ticket: Option<&Ticket>) -> String {
    let authorization = ticket.map_or_else(String::new, |ticket| {
        format!("Proxy-Authorization: {}\r\n", credentials(ticket))
    });

    format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{authorization}\r\n")
}

/// Reads the response head the proxy wrote, and nothing past it.
async fn read_head(client: &mut TcpStream) -> String {
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];

    while !seen.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).await.expect("should read a reply");
        assert!(read != 0, "the proxy closed without answering");
        seen.extend_from_slice(&byte[..read]);
    }

    String::from_utf8_lossy(&seen).into_owned()
}

/// Reads everything on a connection the proxy answers and then closes.
async fn read_answer(client: &mut TcpStream) -> String {
    let mut seen = Vec::new();
    client
        .read_to_end(&mut seen)
        .await
        .expect("should read the answer");

    String::from_utf8_lossy(&seen).into_owned()
}

#[tokio::test]
async fn an_allowed_host_is_tunnelled_and_never_read() {
    // The whole of the no-interception promise: the proxy decides on the host in
    // the CONNECT line and then relays bytes it does not look at.
    let destination = echo_destination().await;
    let (proxy, ticket, mut reported) = admitted("thread-a", allowing(&["127.0.0.1"])).await;

    let mut client = request(&proxy, &connect_head(destination, Some(&ticket))).await;
    let established = read_head(&mut client).await;

    assert!(
        established.starts_with("HTTP/1.1 200 Connection Established"),
        "{established}"
    );

    client
        .write_all(b"bytes the proxy never reads")
        .await
        .expect("should write into the tunnel");

    let mut echoed = [0_u8; 27];
    client
        .read_exact(&mut echoed)
        .await
        .expect("should read what the destination echoed");

    assert_eq!(&echoed, b"bytes the proxy never reads");

    // Nothing was refused, so nothing was recorded. A gate that reported what it
    // allowed would bury the refusals an operator is actually looking for.
    let _nothing = reported.try_recv().expect_err("nothing should be reported");
}

#[tokio::test]
async fn a_denied_host_is_refused_with_403_and_recorded_as_blocked() {
    // Both halves of what the contract promises for a refusal: the agent gets a
    // code it can read in its own tool output, and the operator gets the
    // evidence in a query.
    let destination = echo_destination().await;
    let (proxy, ticket, mut reported) = admitted("thread-a", allowing(&["github.com"])).await;

    let mut client = request(&proxy, &connect_head(destination, Some(&ticket))).await;
    let refusal = read_answer(&mut client).await;

    assert!(refusal.starts_with("HTTP/1.1 403 Forbidden"), "{refusal}");
    assert!(refusal.contains("PERMISSION_DOMAIN_DENIED"), "{refusal}");
    assert!(refusal.contains("127.0.0.1"), "{refusal}");

    let incident = reported.try_recv().expect("a refusal is never silent");

    assert_eq!(incident.code, i32::from(ErrorCode::PermissionDomainDenied));
    assert_eq!(incident.disposition, i32::from(Disposition::Blocked));
    assert!(!incident.retryable, "a policy will not change on a retry");
    assert_eq!(incident.thread_id.as_deref(), Some("thread-a"));
    assert_eq!(incident.turn_id.as_deref(), Some("the-turn"));

    // `details.host` is the field the README names, and the one an operator
    // widening an allowlist reads.
    assert_eq!(
        detail(&incident, "host"),
        Some(prost_types::value::Kind::StringValue(
            "127.0.0.1".to_owned()
        ))
    );
    assert_eq!(
        detail(&incident, "method"),
        Some(prost_types::value::Kind::StringValue("CONNECT".to_owned()))
    );
}

/// One field of an incident's details, as the consumer reads it.
fn detail(incident: &Incident, field: &str) -> Option<prost_types::value::Kind> {
    incident.details.as_ref()?.fields.get(field)?.kind.clone()
}

#[tokio::test]
async fn one_turn_cannot_spend_another_turns_policy() {
    // The reason a turn is identified by its credentials rather than by a port.
    // Both turns share this satellite's one listener, and the only thing that
    // decides which policy applies is which admission was presented.
    let destination = echo_destination().await;

    let proxy = EgressProxy::start().await.expect("should start the proxy");
    let (narrow_incidents, mut narrow_reported) = tokio::sync::mpsc::unbounded_channel();

    let narrow = proxy
        .admit(
            Grant::new("thread-a", "turn-a", allowing(&["github.com"]))
                .reporting_to(narrow_incidents),
        )
        .await;
    let wide = proxy
        .admit(Grant::new("thread-b", "turn-b", WebPolicy::Everything))
        .await;

    // Thread B may reach the destination.
    let mut allowed = request(&proxy, &connect_head(destination, Some(&wide))).await;
    assert!(
        read_head(&mut allowed)
            .await
            .starts_with("HTTP/1.1 200 Connection Established")
    );

    // Thread A may not, on the same listener, one connection later.
    let mut refused = request(&proxy, &connect_head(destination, Some(&narrow))).await;
    assert!(
        read_answer(&mut refused)
            .await
            .starts_with("HTTP/1.1 403 Forbidden")
    );

    // And the refusal lands on the thread that made it rather than on whichever
    // turn happened to be admitted first.
    let incident = narrow_reported.try_recv().expect("a refusal is recorded");
    assert_eq!(incident.thread_id.as_deref(), Some("thread-a"));
}

#[tokio::test]
async fn a_request_with_no_admission_is_challenged_rather_than_answered() {
    // An absent token and an unknown one get the same answer. Saying which would
    // tell a caller whether it had guessed a live turn.
    let destination = echo_destination().await;
    let proxy = EgressProxy::start().await.expect("should start the proxy");

    let mut unauthenticated = request(&proxy, &connect_head(destination, None)).await;
    let challenge = read_answer(&mut unauthenticated).await;

    assert!(
        challenge.starts_with("HTTP/1.1 407 Proxy Authentication Required"),
        "{challenge}"
    );
    // The challenge is what makes a client that does not present credentials
    // preemptively try again with them.
    assert!(
        challenge.contains("Proxy-Authenticate: Basic"),
        "{challenge}"
    );

    let forged = base64::engine::general_purpose::STANDARD.encode("arsox:not-a-real-admission");
    let mut guessing = request(
        &proxy,
        &format!(
            "CONNECT 127.0.0.1:{destination} HTTP/1.1\r\n\
             Proxy-Authorization: Basic {forged}\r\n\r\n"
        ),
    )
    .await;

    assert!(read_answer(&mut guessing).await.starts_with("HTTP/1.1 407"),);
}

#[tokio::test]
async fn an_admission_stops_working_the_moment_its_turn_ends() {
    // A grant that outlived its turn would let a process that kept the token
    // keep reaching the network after the work stopped.
    let destination = echo_destination().await;
    let (proxy, ticket, _reported) = admitted("thread-a", WebPolicy::Everything).await;
    let head = connect_head(destination, Some(&ticket));

    let mut before = request(&proxy, &head).await;
    assert!(
        read_head(&mut before)
            .await
            .starts_with("HTTP/1.1 200 Connection Established")
    );

    drop(ticket);
    tokio::task::yield_now().await;

    let mut after = request(&proxy, &head).await;
    assert!(read_answer(&mut after).await.starts_with("HTTP/1.1 407"));
}

/// A destination that answers one plaintext request, and reports what it saw.
async fn http_destination() -> (u16, std::sync::Arc<tokio::sync::Mutex<Option<String>>>) {
    use axum::extract::State;
    use axum::routing::get;

    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None));

    let router = axum::Router::new()
        .route(
            "/hello",
            get(
                |State(seen): State<std::sync::Arc<tokio::sync::Mutex<Option<String>>>>,
                 headers: axum::http::HeaderMap| async move {
                    *seen.lock().await = headers
                        .get("host")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);

                    "answered"
                },
            ),
        )
        .with_state(std::sync::Arc::clone(&seen));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the destination");
    let port = listener
        .local_addr()
        .expect("should have an address")
        .port();

    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });

    (port, seen)
}

/// A client pointed at the proxy exactly as an agent's tooling is.
fn client_through(ticket: &Ticket) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(ticket.proxy_url()).expect("should accept the proxy url"))
        .build()
        .expect("should build a client")
}

#[tokio::test]
async fn an_ordinary_client_reaches_an_allowed_origin_through_the_proxy() {
    // What raw TCP cannot prove: that a real client handed the proxy URL an
    // agent is handed presents the credentials that URL carries, and that the
    // request the origin receives is one it can answer.
    let (destination, seen) = http_destination().await;
    let (_proxy, ticket, _reported) = admitted("thread-a", allowing(&["127.0.0.1"])).await;

    let response = client_through(&ticket)
        .get(format!("http://127.0.0.1:{destination}/hello"))
        .send()
        .await
        .expect("should reach the origin");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.expect("should read"), "answered");

    // The origin was spoken to as an origin: the request line was rewritten out
    // of absolute form and the Host header names the destination.
    assert_eq!(
        seen.lock().await.as_deref(),
        Some(format!("127.0.0.1:{destination}").as_str())
    );
}

#[tokio::test]
async fn an_ordinary_client_is_refused_when_the_origin_is_not_allowed() {
    // The plaintext half of the refusal. An agent's `curl` reads the body, so
    // the code is in it rather than only in the status line.
    let (destination, _seen) = http_destination().await;
    let (_proxy, ticket, mut reported) = admitted("thread-a", allowing(&["github.com"])).await;

    let response = client_through(&ticket)
        .get(format!("http://127.0.0.1:{destination}/hello"))
        .send()
        .await
        .expect("the proxy answers rather than hanging up");

    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(
        response
            .text()
            .await
            .expect("should read")
            .contains("PERMISSION_DOMAIN_DENIED")
    );

    let incident = reported.try_recv().expect("a refusal is never silent");
    assert_eq!(incident.code, i32::from(ErrorCode::PermissionDomainDenied));
    assert_eq!(
        detail(&incident, "method"),
        Some(prost_types::value::Kind::StringValue("GET".to_owned()))
    );
}
