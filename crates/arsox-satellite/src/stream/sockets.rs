// Copyright © 2026 Jalapeno Labs

//! The two `WebSocket`s.
//!
//! Both are unidirectional, server to client. Nothing is ever sent up them:
//! every command is an ordinary HTTP request, and the socket only reports what
//! happened.
//!
//! Frames are binary protobuf. A JSON subprotocol is documented for hand-driven
//! clients and is refused by name until it exists, rather than being silently
//! ignored and delivering protobuf to something expecting text.

use crate::{Satellite, contract_error};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::{ControlEvent, ThreadEvent};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use prost::Message as _;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// How many stored events to read per replay query.
const REPLAY_BATCH: u32 = 256;

/// The subprotocol a hand-driven client asks for when it wants JSON frames.
const JSON_SUBPROTOCOL: &str = "arsox.json.v1";

/// A policy failure, in the WebSocket close-code sense.
///
/// The contract code travels in the reason so a client matches on the same
/// vocabulary it uses everywhere else, rather than on a numeric close code that
/// means something different in every protocol.
const POLICY_VIOLATION: u16 = 1008;

/// Everything ended as intended.
const NORMAL_CLOSURE: u16 = 1000;

#[derive(Debug, Deserialize)]
pub(crate) struct StreamQuery {
    /// Resume after this sequence, exclusive.
    ///
    /// A consumer passes the last sequence it actually handled and receives
    /// everything since. Absent starts from the beginning of retained history,
    /// which for a young thread is all of it.
    #[serde(default)]
    from_sequence: u64,
}

/// Upgrades a request into a thread's event stream.
async fn thread_stream(
    upgrade: WebSocketUpgrade,
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    // Refused before the upgrade, because a client that asked for JSON and got
    // binary would decode garbage rather than learn it asked for something
    // unavailable.
    if json_frames_requested(&headers) {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::StreamSubprotocolUnsupported,
            "JSON frames are not implemented yet, connect without a subprotocol for protobuf",
        );
    }

    // A stream for a thread that does not exist is a mistake worth reporting as
    // an ordinary HTTP error, while the client can still read a status code.
    if let Err(error) = satellite.store.thread(&thread_id).await {
        return contract_error(StatusCode::NOT_FOUND, error.code(), &error.to_string());
    }

    upgrade.on_upgrade(move |socket| {
        drive_thread_stream(socket, satellite, thread_id, query.from_sequence)
    })
}

async fn drive_thread_stream(
    mut socket: WebSocket,
    satellite: Arc<Satellite>,
    thread_id: String,
    from_sequence: u64,
) {
    // Subscribed before a single stored event is read. A consumer that replays
    // first and subscribes afterwards loses everything published in between, and
    // the gap is invisible: the sequences it receives are contiguous with what it
    // read, just missing the middle.
    let mut live = satellite.bus.subscribe();

    // Asking to resume from before retained history means the consumer has lost
    // events it will never see. Saying so beats delivering a stream that quietly
    // skips them.
    match satellite.store.oldest_sequence(&thread_id).await {
        Ok(Some(oldest)) if from_sequence + 1 < oldest => {
            close(socket, POLICY_VIOLATION, ErrorCode::StreamSequenceExpired).await;
            return;
        }
        Ok(_within_retention) => {}
        Err(_unreadable) => {
            close(socket, POLICY_VIOLATION, ErrorCode::Internal).await;
            return;
        }
    }

    let mut last_sent = from_sequence;

    loop {
        let batch = match satellite
            .store
            .events_after(&thread_id, last_sent, REPLAY_BATCH)
            .await
        {
            Ok(batch) => batch,
            Err(_unreadable) => {
                close(socket, POLICY_VIOLATION, ErrorCode::Internal).await;
                return;
            }
        };

        if batch.is_empty() {
            break;
        }

        for event in batch {
            last_sent = event.sequence;
            if !send(&mut socket, &event).await {
                return;
            }
        }
    }

    loop {
        match live.recv().await {
            Ok(event) => {
                // Anything the replay already covered, and anything belonging to
                // another thread.
                if event.thread_id != thread_id || event.sequence <= last_sent {
                    continue;
                }

                last_sent = event.sequence;
                if !send(&mut socket, &event).await {
                    return;
                }
            }
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(
                    event.name = "stream.consumer.lagged",
                    thread.id = %thread_id,
                    event.missed = missed,
                    "closing a consumer that fell behind",
                );
                close(socket, POLICY_VIOLATION, ErrorCode::StreamConsumerLagged).await;
                return;
            }
            Err(RecvError::Closed) => {
                close(socket, NORMAL_CLOSURE, ErrorCode::Unspecified).await;
                return;
            }
        }
    }
}

/// Upgrades a request into the satellite's control stream.
async fn control_stream(
    upgrade: WebSocketUpgrade,
    State(satellite): State<Arc<Satellite>>,
    headers: HeaderMap,
) -> Response {
    if json_frames_requested(&headers) {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::StreamSubprotocolUnsupported,
            "JSON frames are not implemented yet, connect without a subprotocol for protobuf",
        );
    }

    upgrade.on_upgrade(move |socket| drive_control_stream(socket, satellite))
}

async fn drive_control_stream(mut socket: WebSocket, satellite: Arc<Satellite>) {
    let mut live = satellite.bus.subscribe_control();

    // No replay: the control stream carries satellite lifecycle, which is state
    // a client re-reads with GET /v1/status rather than reconstructs from
    // history.
    loop {
        match live.recv().await {
            Ok(event) => {
                if !send_control(&mut socket, &event).await {
                    return;
                }
            }
            Err(RecvError::Lagged(_missed)) => {
                close(socket, POLICY_VIOLATION, ErrorCode::StreamConsumerLagged).await;
                return;
            }
            Err(RecvError::Closed) => {
                close(socket, NORMAL_CLOSURE, ErrorCode::Unspecified).await;
                return;
            }
        }
    }
}

/// Whether the handshake asked for JSON frames.
fn json_frames_requested(headers: &HeaderMap) -> bool {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|offered| {
            offered
                .split(',')
                .any(|protocol| protocol.trim() == JSON_SUBPROTOCOL)
        })
}

/// Sends one event, reporting whether the socket is still usable.
async fn send(socket: &mut WebSocket, event: &ThreadEvent) -> bool {
    socket
        .send(Message::Binary(event.encode_to_vec().into()))
        .await
        .is_ok()
}

async fn send_control(socket: &mut WebSocket, event: &ControlEvent) -> bool {
    socket
        .send(Message::Binary(event.encode_to_vec().into()))
        .await
        .is_ok()
}

/// Closes with a reason a client can match on.
async fn close(mut socket: WebSocket, code: u16, reason: ErrorCode) {
    let reason = match reason {
        // A clean shutdown has no contract code to report.
        ErrorCode::Unspecified => String::new(),
        named => format!("{named:?}").to_uppercase(),
    };

    drop(
        socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await,
    );
}

/// Both sockets, to be mounted behind authentication.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new()
        .route("/v1/threads/{thread_id}/stream", any(thread_stream))
        .route("/v1/stream", any(control_stream))
}
