// Copyright © 2026 Jalapeno Labs

//! The event stream, over a real socket against a real satellite.
//!
//! These start an actual server on an ephemeral port and connect a real
//! WebSocket client, because the property that matters most here is a handoff
//! between two code paths and testing it against a reimplementation of that
//! handoff would prove nothing.

use arsox_satellite::redaction::Redactor;
use arsox_satellite::store::{AppendEvent, NewThread, Store};
use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::proto::event::v1::ThreadEvent;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use futures_util::StreamExt as _;
use prost::Message as _;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_tungstenite::tungstenite;

/// Distinguishes scratch directories created in the same clock tick.
///
/// A timestamp alone is not unique: Windows clocks tick at 100 nanoseconds and
/// these tests run in parallel, so two of them can name the same directory,
/// share a database file, and race each other's migration.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const SECRET: &str = "test-secret";

struct Running {
    port: u16,
    store: Store,
}

/// Starts a satellite on an ephemeral port and hands back a store pointed at the
/// same database.
async fn start() -> Running {
    let unique = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let directory = std::env::temp_dir().join(format!(
        "arsox-stream-{unique}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("should create a scratch directory");

    let database = directory.join("arsox.db");
    let options = ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: database.to_string_lossy().into_owned(),
        workspace_root: directory.to_string_lossy().into_owned(),
        max_concurrent_threads: 1,
        // Long enough that no test races the collector.
        collect_interval: std::time::Duration::from_hours(1),
    };

    let assembled = assemble(options).await.expect("should assemble");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("should bind an ephemeral port");
    let port = listener
        .local_addr()
        .expect("should have an address")
        .port();

    // The satellite's own store, not a second handle on the same file. Live
    // delivery is in-process: a separate handle would write the same rows and
    // publish to nobody, and the stream would look correct on replay and empty
    // afterwards.
    let store = assembled.store.clone();

    tokio::spawn(async move {
        drop(axum::serve(listener, assembled.router).await);
    });

    Running { port, store }
}

/// Opens a stream, optionally resuming after a sequence.
async fn connect(
    port: u16,
    thread_id: &str,
    from_sequence: Option<u64>,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let query = from_sequence.map_or_else(String::new, |after| format!("?from_sequence={after}"));
    let url = format!("ws://127.0.0.1:{port}/v1/threads/{thread_id}/stream{query}");

    let mut request = tungstenite::client::IntoClientRequest::into_client_request(url)
        .expect("should build a request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {SECRET}")
            .parse()
            .expect("header should be valid"),
    );

    let (socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("should connect");

    socket
}

async fn seed_thread(store: &Store) -> String {
    store
        .create_thread(NewThread {
            settings: ThreadSettings::default(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread
        .thread_id
}

async fn append(store: &Store, thread_id: &str, text: &str) {
    append_masked(store, thread_id, text, Redactor::none()).await;
}

/// Appends an agent message under a thread's own secrets.
async fn append_masked(store: &Store, thread_id: &str, text: &str, redactor: Redactor) {
    store
        .append_event(AppendEvent {
            thread_id: thread_id.to_owned(),
            turn_id: None,
            member_id: None,
            type_name: "agent.message".to_owned(),
            occurred_at: None,
            payload: arsox_sdk::proto::event::v1::thread_event::Payload::AgentMessage(
                arsox_sdk::proto::event::v1::AgentMessage {
                    author: None,
                    text: text.to_owned(),
                },
            ),
            redactor,
        })
        .await
        .expect("should append");
}

/// Reads the next event, failing rather than hanging if none arrives.
async fn next_event(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ThreadEvent {
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("should not time out")
        .expect("the socket should stay open")
        .expect("the frame should be readable");

    match frame {
        tungstenite::Message::Binary(bytes) => {
            ThreadEvent::decode(bytes.as_ref()).expect("should decode as a ThreadEvent")
        }
        other => panic!("expected a binary protobuf frame, got {other:?}"),
    }
}

#[tokio::test]
async fn a_secret_never_reaches_a_consumer_live_or_on_replay() {
    // The stream is the obvious channel and the one a host application logs
    // wholesale. Both the frame published to a connected consumer and the row a
    // reconnecting one replays come from the same masked bytes, so a leak here
    // would have to be a leak in both.
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    let settings = ThreadSettings {
        env: vec![arsox_sdk::proto::settings::v1::EnvVar {
            key: "DEPLOY_TOKEN".to_owned(),
            value: Some(arsox_sdk::proto::common::v1::Secret {
                value: Some("ghp_the_real_token".to_owned()),
                display: None,
            }),
            // Absent, which means secret.
            is_secret: None,
        }],
        ..ThreadSettings::default()
    };
    let redactor = Redactor::for_thread(&settings);

    let mut socket = connect(running.port, &thread_id, None).await;
    append_masked(
        &running.store,
        &thread_id,
        "pushed with ghp_the_real_token",
        redactor,
    )
    .await;

    let live = next_event(&mut socket).await;
    let rendered = format!("{live:?}");
    assert!(
        !rendered.contains("ghp_the_real_token"),
        "the token reached a live consumer: {rendered}"
    );
    assert!(
        rendered.contains("pushed with ******"),
        "the message should survive with the token masked out: {rendered}"
    );

    // The persisted copy is the same bytes, so a consumer that reconnects and
    // replays cannot be told a different story than one that stayed connected.
    let replayed = running
        .store
        .events_after(&thread_id, 0, 10)
        .await
        .expect("should replay");
    assert_eq!(format!("{:?}", replayed[0]), rendered);
}

#[tokio::test]
async fn a_new_consumer_replays_everything_recorded_so_far() {
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    for text in ["one", "two", "three"] {
        append(&running.store, &thread_id, text).await;
    }

    let mut socket = connect(running.port, &thread_id, None).await;

    for expected in 1..=3 {
        assert_eq!(next_event(&mut socket).await.sequence, expected);
    }
}

#[tokio::test]
async fn resuming_delivers_only_what_came_after() {
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    for text in ["one", "two", "three"] {
        append(&running.store, &thread_id, text).await;
    }

    // Exclusive: a consumer passes the last sequence it actually handled.
    let mut socket = connect(running.port, &thread_id, Some(2)).await;

    assert_eq!(next_event(&mut socket).await.sequence, 3);
}

#[tokio::test]
async fn events_recorded_after_connecting_arrive_live() {
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    let mut socket = connect(running.port, &thread_id, None).await;
    append(&running.store, &thread_id, "live").await;

    let event = next_event(&mut socket).await;
    assert_eq!(event.sequence, 1);
}

#[tokio::test]
async fn nothing_is_lost_in_the_handoff_from_replay_to_live() {
    // The property this whole design exists to protect. A consumer that read
    // history first and subscribed afterwards would lose whatever was published
    // in between, and the loss would be invisible: the sequences it receives are
    // contiguous with what it read, just missing the middle.
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    for text in ["one", "two"] {
        append(&running.store, &thread_id, text).await;
    }

    let mut socket = connect(running.port, &thread_id, None).await;

    for text in ["three", "four", "five"] {
        append(&running.store, &thread_id, text).await;
    }

    let sequences: Vec<u64> = {
        let mut seen = Vec::new();
        for _expected in 0..5 {
            seen.push(next_event(&mut socket).await.sequence);
        }
        seen
    };

    assert_eq!(
        sequences,
        vec![1, 2, 3, 4, 5],
        "the stream must be gapless across the handoff"
    );
}

#[tokio::test]
async fn every_consumer_receives_every_event() {
    // A horizontally scaled host application has several replicas watching one
    // thread. A design that delivered to one would force a designated replica.
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    let mut first = connect(running.port, &thread_id, None).await;
    let mut second = connect(running.port, &thread_id, None).await;

    append(&running.store, &thread_id, "shared").await;

    assert_eq!(next_event(&mut first).await.sequence, 1);
    assert_eq!(next_event(&mut second).await.sequence, 1);
}

#[tokio::test]
async fn a_stream_carries_only_its_own_thread() {
    let running = start().await;
    let watched = seed_thread(&running.store).await;
    let other = seed_thread(&running.store).await;

    let mut socket = connect(running.port, &watched, None).await;

    append(&running.store, &other, "not yours").await;
    append(&running.store, &watched, "yours").await;

    let event = next_event(&mut socket).await;
    assert_eq!(event.thread_id, watched);
    assert_eq!(event.r#type, "agent.message");
}

#[tokio::test]
async fn asking_for_json_frames_is_refused_by_name() {
    // Delivering protobuf to a client that asked for text would have it decoding
    // garbage rather than learning the subprotocol does not exist yet.
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    let url = format!(
        "ws://127.0.0.1:{}/v1/threads/{thread_id}/stream",
        running.port
    );
    let mut request = tungstenite::client::IntoClientRequest::into_client_request(url)
        .expect("should build a request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {SECRET}").parse().expect("valid"),
    );
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "arsox.json.v1".parse().expect("valid"),
    );

    let outcome = tokio_tungstenite::connect_async(request).await;
    assert!(outcome.is_err(), "the handshake should be refused");
}

#[tokio::test]
async fn an_unauthenticated_stream_is_refused() {
    let running = start().await;
    let thread_id = seed_thread(&running.store).await;

    let url = format!(
        "ws://127.0.0.1:{}/v1/threads/{thread_id}/stream",
        running.port
    );

    let outcome = tokio_tungstenite::connect_async(url).await;
    assert!(
        outcome.is_err(),
        "a stream is not a way around the bearer check"
    );
}

#[tokio::test]
async fn a_stream_for_an_unknown_thread_is_refused() {
    let running = start().await;

    let url = format!("ws://127.0.0.1:{}/v1/threads/nope/stream", running.port);
    let mut request = tungstenite::client::IntoClientRequest::into_client_request(url)
        .expect("should build a request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {SECRET}").parse().expect("valid"),
    );

    let outcome = tokio_tungstenite::connect_async(request).await;
    assert!(outcome.is_err(), "an unknown thread should not upgrade");
}
