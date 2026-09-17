// Copyright © 2026 Jalapeno Labs

//! The WebSocket a host application answers relayed tool calls through.
//!
//! `GET /v1/threads/{id}/relay`, behind the same bearer check as every other
//! authenticated route. Unlike the two event streams this socket carries frames
//! both ways: calls and cancellations down, results up. Every frame is one
//! binary protobuf message, `SatelliteRelayFrame` down and `ClientRelayFrame`
//! up, and the JSON subprotocol is refused by name exactly as it is on the
//! event streams.
//!
//! Refused before the upgrade, as ordinary contract errors: an unknown thread
//! answers 404, and a thread that declared no relayed servers answers
//! `409 RELAY_NOT_DECLARED`, which is permanent.

use super::{Connection, Hub, Outbound, REPLACED_CLOSE_CODE};
use crate::stream::sockets::json_frames_requested;
use crate::{Satellite, contract_error};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::relay::v1::{ClientRelayFrame, client_relay_frame};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::any;
use prost::Message as _;
use std::sync::Arc;

/// Upgrades a request into a thread's relay.
async fn relay(
    upgrade: WebSocketUpgrade,
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if json_frames_requested(&headers) {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::StreamSubprotocolUnsupported,
            "JSON frames are not implemented yet, connect without a subprotocol for protobuf",
        );
    }

    // Before the upgrade, while the client can still read a status code. A relay
    // for a thread that does not exist would otherwise sit open answering
    // nothing, which looks exactly like a thread whose agents made no calls.
    let thread = match satellite.store.thread(&thread_id).await {
        Ok(thread) => thread,
        Err(error) => return crate::api::store_failure(&error),
    };

    // A thread with nothing to relay is refused by a code of its own rather than
    // accepted into a socket that never carries a frame. A thread's settings do
    // not change, so the refusal is permanent, and saying so is what lets a
    // client stop reconnecting instead of retrying forever.
    let declares_relayed_servers = thread
        .settings
        .is_some_and(|settings| !settings.relayed_mcp_servers.is_empty());
    if !declares_relayed_servers {
        return contract_error(
            StatusCode::CONFLICT,
            ErrorCode::RelayNotDeclared,
            "this thread declared no relayed_mcp_servers, so its relay has nothing to carry",
        );
    }

    let hub = satellite.relay.clone();

    upgrade.on_upgrade(move |socket| drive(socket, hub, thread_id))
}

/// Serves one attached client until it leaves or is replaced.
async fn drive(mut socket: WebSocket, hub: Hub, thread_id: String) {
    let Connection { id, mut outbound } = hub.attach(&thread_id);

    tracing::info!(
        event.name = "relay.client.attached",
        thread.id = %thread_id,
        "a relay client attached",
    );

    loop {
        tokio::select! {
            sending = outbound.recv() => {
                match sending {
                    Some(Outbound::Frame(frame)) => {
                        let bytes = frame.encode_to_vec();
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Replaced) => {
                        close_replaced(socket).await;
                        // Already detached: the table holds the client that
                        // replaced this one, and detaching by id is a no-op.
                        return;
                    }
                    // The hub dropped this attachment's sender, which only
                    // replacement does, and replacement says so first.
                    None => break,
                }
            }

            received = socket.recv() => {
                match received {
                    Some(Ok(Message::Binary(bytes))) => receive(&hub, &thread_id, id, &bytes),
                    // A close, a transport failure, or the end of the stream all
                    // mean the same thing here: this client is gone.
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    // Ping and pong are answered by the socket itself, and text
                    // frames are not part of the contract.
                    Some(Ok(_other)) => {}
                }
            }
        }
    }

    hub.detach(&thread_id, id);

    tracing::info!(
        event.name = "relay.client.detached",
        thread.id = %thread_id,
        "a relay client detached",
    );
}

/// Hands one frame from the client to the hub.
///
/// A frame that does not decode is logged and skipped rather than closing the
/// socket. Closing would fail every other call the client is in the middle of
/// answering, to punish one frame that cannot have been an answer to any of
/// them.
fn receive(hub: &Hub, thread_id: &str, connection: u64, bytes: &[u8]) {
    let frame = match ClientRelayFrame::decode(bytes) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::warn!(
                event.name = "relay.frame.undecodable",
                thread.id = thread_id,
                "skipping a relay frame that does not decode: {error}",
            );
            return;
        }
    };

    match frame.frame {
        Some(client_relay_frame::Frame::Result(result)) => {
            hub.deliver(thread_id, connection, result);
        }
        None => tracing::warn!(
            event.name = "relay.frame.empty",
            thread.id = thread_id,
            "skipping a relay frame that carries nothing this satellite knows",
        ),
    }
}

/// Closes a socket whose client was replaced, saying so by code and by name.
async fn close_replaced(mut socket: WebSocket) {
    let reason = ErrorCode::RelayClientReplaced
        .as_str_name()
        .trim_start_matches("ERROR_CODE_");

    drop(
        socket
            .send(Message::Close(Some(CloseFrame {
                code: REPLACED_CLOSE_CODE,
                reason: reason.into(),
            })))
            .await,
    );
}

/// The relay route, to be mounted behind authentication.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new().route("/v1/threads/{thread_id}/relay", any(relay))
}
