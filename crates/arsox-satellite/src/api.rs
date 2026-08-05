// Copyright © 2026 Jalapeno Labs

//! Thread and turn endpoints.
//!
//! Every handler decodes a protobuf request, talks to the store, and encodes a
//! protobuf response. Failures render as the one contract error shape, with the
//! code carried up from the store rather than re-derived here, so a missing
//! thread is one concept from the query to the response.

use crate::{Satellite, contract_error, protobuf};
use arsox_sdk::proto::common::v1::{PageRequest, PageResponse};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::thread::v1::{
    CreateThreadRequest, CreateThreadResponse, DestroyThreadResponse, GetThreadResponse,
    ListThreadsResponse,
};
use arsox_sdk::proto::turn::v1::{
    CancelTurnResponse, GetTurnResponse, ListTurnsResponse, StartTurnRequest, StartTurnResponse,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::store::{NewThread, NewTurn, StoreError, ThreadFilter};

/// A protobuf request body.
///
/// Protobuf is what the SDK always sends. JSON is offered on responses as a
/// debugging affordance, and a JSON request body is refused explicitly rather
/// than being decoded as protobuf and failing with something unhelpful.
pub struct Protobuf<T>(pub T);

impl<S, T> FromRequest<S> for Protobuf<T>
where
    T: prost::Message + Default,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let declared = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned());

        // An absent Content-Type is accepted as protobuf, because that is the
        // default the contract documents. Anything else named is refused by
        // name so the caller learns which of the two mistakes they made.
        if let Some(kind) = declared.as_deref()
            && !kind.is_empty()
            && kind != "application/protobuf"
            && kind != "application/x-protobuf"
        {
            return Err(contract_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::RequestContentTypeUnsupported,
                &format!("{kind} is not a supported request encoding, send application/protobuf"),
            ));
        }

        let bytes = Bytes::from_request(req, state).await.map_err(|_ignored| {
            contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                "the request body could not be read",
            )
        })?;

        T::decode(bytes).map(Protobuf).map_err(|error| {
            contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                &format!("the request body did not decode as protobuf: {error}"),
            )
        })
    }
}

/// Renders a store failure as the contract error it already knows itself to be.
fn store_failure(error: &StoreError) -> Response {
    let status = match error.code() {
        ErrorCode::ThreadNotFound | ErrorCode::TurnNotFound => StatusCode::NOT_FOUND,
        ErrorCode::ThreadDestroyed => StatusCode::GONE,
        ErrorCode::TurnQueueFull => StatusCode::TOO_MANY_REQUESTS,
        _internal => StatusCode::INTERNAL_SERVER_ERROR,
    };

    // Logged here rather than at every call site, because an internal failure
    // that only reaches the client is a failure nobody operating the satellite
    // ever sees.
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        tracing::error!(
            event.name = "store.query.failed",
            error.type = "store",
            "{error}",
        );
    }

    crate::contract_error_retryable(status, error.code(), &error.to_string(), error.retryable())
}

async fn create_thread(
    State(satellite): State<Arc<Satellite>>,
    Protobuf(request): Protobuf<CreateThreadRequest>,
) -> Response {
    let Some(settings) = request.settings else {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestFieldMissing,
            "settings is required: a thread must declare its budget and idle TTL",
        );
    };

    // Required, always, as the safety net against forgotten workspaces filling a
    // disk. Refusing here is cheaper than collecting an immortal thread later.
    if settings.idle_ttl.is_none() {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestFieldMissing,
            "settings.idle_ttl is required so a forgotten thread is eventually collected",
        );
    }
    if settings.budget.is_none() {
        return contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestFieldMissing,
            "settings.budget is required: an unbounded spend must be typed out, not defaulted into",
        );
    }

    match satellite
        .store
        .create_thread(NewThread {
            settings,
            metadata: request.metadata.into_iter().collect(),
            idempotency_key: request.idempotency_key,
        })
        .await
    {
        Ok(stored) => protobuf(&CreateThreadResponse {
            thread: Some(stored.thread),
            deduplicated: !stored.created,
        }),
        Err(error) => store_failure(&error),
    }
}

async fn list_threads(
    State(satellite): State<Arc<Satellite>>,
    Protobuf(request): Protobuf<arsox_sdk::proto::thread::v1::ListThreadsRequest>,
) -> Response {
    let page = request.page.unwrap_or(PageRequest::default());

    let filter = ThreadFilter {
        states: request.states,
        metadata: request.metadata.into_iter().collect::<BTreeMap<_, _>>(),
        after: if page.cursor.is_empty() {
            None
        } else {
            Some(page.cursor)
        },
        limit: page.limit,
    };

    match satellite.store.list_threads(&filter).await {
        Ok(threads) => {
            // The cursor is the last id returned. Thread ids are UUIDv7 and sort
            // chronologically, so the cursor needs to carry nothing else.
            let next_cursor = threads
                .last()
                .map(|summary| summary.thread_id.clone())
                .unwrap_or_default();

            protobuf(&ListThreadsResponse {
                threads,
                page: Some(PageResponse {
                    next_cursor,
                    // Counting every match would mean a second scan on every
                    // page. Absent says "not computed" rather than claiming a
                    // total of zero.
                    total: None,
                }),
            })
        }
        Err(error) => store_failure(&error),
    }
}

async fn get_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.thread(&thread_id).await {
        Ok(thread) => protobuf(&GetThreadResponse {
            thread: Some(thread),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn destroy_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.destroy_thread(&thread_id).await {
        Ok(thread) => protobuf(&DestroyThreadResponse {
            thread: Some(thread),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn start_turn(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Protobuf(request): Protobuf<StartTurnRequest>,
) -> Response {
    match satellite
        .store
        .create_turn(NewTurn {
            thread_id,
            prompt: request.prompt,
            metadata: request.metadata.into_iter().collect(),
            idempotency_key: request.idempotency_key,
            // Only a pull request watch starts a turn the SDK did not ask for,
            // and that path does not come through here.
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
    {
        Ok(stored) => {
            // The runner polls as a safety net, but a queued turn should start
            // now rather than within a poll interval.
            satellite.work_queued.notify_one();

            protobuf(&StartTurnResponse {
                turn: Some(stored.turn),
            })
        }
        Err(error) => store_failure(&error),
    }
}

async fn list_turns(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Protobuf(request): Protobuf<arsox_sdk::proto::turn::v1::ListTurnsRequest>,
) -> Response {
    match satellite
        .store
        .list_turns(&thread_id, &request.statuses)
        .await
    {
        Ok(turns) => protobuf(&ListTurnsResponse {
            turns,
            page: Some(PageResponse::default()),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn get_turn(
    State(satellite): State<Arc<Satellite>>,
    Path((thread_id, turn_id)): Path<(String, String)>,
) -> Response {
    match satellite.store.turn(&thread_id, &turn_id).await {
        Ok((turn, result)) => protobuf(&GetTurnResponse {
            turn: Some(turn),
            result,
        }),
        Err(error) => store_failure(&error),
    }
}

async fn cancel_turn(
    State(satellite): State<Arc<Satellite>>,
    Path((thread_id, turn_id)): Path<(String, String)>,
) -> Response {
    match satellite.store.cancel_turn(&thread_id, &turn_id).await {
        Ok(turn) => protobuf(&CancelTurnResponse { turn: Some(turn) }),
        Err(error) => store_failure(&error),
    }
}

/// Every authenticated thread and turn route.
pub fn routes() -> Router<Arc<Satellite>> {
    Router::new()
        .route("/v1/threads", post(create_thread).get(list_threads))
        .route("/v1/threads/{thread_id}", get(get_thread))
        .route("/v1/threads/{thread_id}", delete(destroy_thread))
        .route(
            "/v1/threads/{thread_id}/turns",
            post(start_turn).get(list_turns),
        )
        .route("/v1/threads/{thread_id}/turns/{turn_id}", get(get_turn))
        .route(
            "/v1/threads/{thread_id}/turns/{turn_id}/cancel",
            post(cancel_turn),
        )
}
