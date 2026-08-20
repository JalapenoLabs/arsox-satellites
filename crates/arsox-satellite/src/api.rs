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
use arsox_sdk::proto::incident::v1::{ListIncidentsRequest, ListIncidentsResponse};
use arsox_sdk::proto::thread::v1::{
    CreateThreadRequest, CreateThreadResponse, DestroyThreadResponse, DrainThreadResponse,
    GetThreadResponse, ListThreadsResponse, PauseThreadResponse, ResumeThreadResponse, Thread,
    ThreadOrder,
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

use crate::store::{IncidentFilter, NewThread, NewTurn, StoreError, ThreadFilter};
use arsox_sdk::proto::event::v1::control_event::Payload;
use arsox_sdk::proto::event::v1::{ThreadCreated, ThreadEndReason};

/// Stamps a satellite lifecycle event.
///
/// The sequence is left at zero: control events are a live feed of what is
/// happening now, not a log to replay, and a number nothing can resume from
/// would only look like one.
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

/// The thread as a response is allowed to carry it.
///
/// Every response shape below carries a whole [`Thread`], and a thread carries
/// the settings it was created with, credentials included. This is the one place
/// they are masked, because it is the one place the settings are on their way to
/// a client: the provisioner and the spawn read the same stored settings to
/// clone private repos and to build an agent's environment, so a scrub any
/// deeper would leave a restart cloning with `******` for a token.
///
/// `ListThreads` is deliberately absent. It returns `ThreadSummary`, which
/// carries no settings at all, and that is the stronger answer: an operational
/// listing has no configuration in it to mask.
fn scrubbed(mut thread: Thread) -> Thread {
    if let Some(settings) = thread.settings.as_mut() {
        crate::redaction::scrub_settings(settings);
    }

    thread
}

/// Renders a store failure as the contract error it already knows itself to be.
pub(crate) fn store_failure(error: &StoreError) -> Response {
    let status = match error.code() {
        ErrorCode::ThreadNotFound | ErrorCode::TurnNotFound => StatusCode::NOT_FOUND,
        // Gone rather than not found: the thread existed, and saying so is what
        // lets a caller tell "my id is wrong" from "my thread was collected".
        ErrorCode::ThreadDestroyed | ErrorCode::ThreadExpired => StatusCode::GONE,
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

    // A repo's name becomes a directory under the thread's workspace, so one
    // that could reach outside it is refused here rather than discovered
    // minutes later in a provisioning incident. This is the last point at which
    // the caller is still listening and can fix it.
    for repo in &settings.repos {
        if let Err(error) = crate::workspace::directory_name(repo) {
            return contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestFieldInvalid,
                &format!("settings.repos: {error}"),
            );
        }
    }

    // A declared variable is set on top of the scrubbed environment, so one
    // named like a satellite setting or a provider credential would hand an
    // agent back exactly what the scrub exists to withhold. Refused here rather
    // than dropped mid-turn, so the caller learns which key was wrong while it
    // is still listening.
    //
    // The key is named and the value never is: half of these are credentials by
    // definition, and an error body is a log line somewhere.
    for declared in &settings.env {
        if let Some(refusal) = crate::harness::spawn::declared_key_refusal(&declared.key) {
            return contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestFieldInvalid,
                &format!("settings.env: {} {refusal}", declared.key),
            );
        }
    }

    // Kept before the settings are handed to the store, because provisioning
    // reads them once the thread id exists.
    let workspace_settings = settings.clone();

    match satellite
        .store
        .create_thread(NewThread {
            settings,
            metadata: request.metadata.into_iter().collect(),
            idempotency_key: request.idempotency_key,
        })
        .await
    {
        Ok(stored) => {
            // Only a genuinely new thread is announced. A deduplicated create
            // did not change the satellite's state, and reporting it as a
            // creation would make a retry look like a second thread.
            if stored.created {
                satellite.bus.publish_control(crate::stream::control_event(
                    "thread.created",
                    Payload::ThreadCreated(ThreadCreated {
                        thread_id: stored.thread.thread_id.clone(),
                    }),
                ));

                // A deduplicated create fills nothing: the workspace it would
                // build already exists, and building it twice would clone over a
                // checkout the thread may already be working in.
                if workspace_settings.repos.is_empty() {
                    // Nothing to clone, so this thread is IDLE the moment it is
                    // created and a turn may be queued against it the moment the
                    // caller reads this response. Its instruction files are three
                    // small writes, and deferring them would race that first turn
                    // for no gain.
                    satellite
                        .provisioner
                        .write_instructions(&stored.thread.thread_id, &workspace_settings)
                        .await;
                } else {
                    // Behind the response rather than inside it. Cloning three
                    // repos and running their installs is minutes of work, and
                    // holding the create open for it would make the satellite's
                    // most ordinary call its slowest. The thread opens
                    // PROVISIONING and the claim query holds its queue until
                    // this finishes.
                    satellite
                        .provisioner
                        .spawn(stored.thread.thread_id.clone(), workspace_settings);
                }
            }

            protobuf(&CreateThreadResponse {
                thread: Some(scrubbed(stored.thread)),
                deduplicated: !stored.created,
            })
        }
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
        order_by: ThreadOrder::try_from(request.order_by).unwrap_or(ThreadOrder::Unspecified),
        descending: request.descending,
    };

    match satellite.store.list_threads(&filter).await {
        Ok(listing) => protobuf(&ListThreadsResponse {
            threads: listing.threads,
            page: Some(PageResponse {
                // Encodes the sort key alongside the id, because ordering by
                // last activity is not unique and an id-only cursor would repeat
                // or skip whatever shares a timestamp with the page boundary.
                next_cursor: listing.next_cursor,
                // Counting every match would mean a second scan on every page.
                // Absent says "not computed" rather than claiming a total of
                // zero.
                total: None,
            }),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn pause_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.pause_thread(&thread_id).await {
        Ok(thread) => protobuf(&PauseThreadResponse {
            thread: Some(scrubbed(thread)),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn resume_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.resume_thread(&thread_id).await {
        Ok(thread) => {
            // A resumed thread may have work waiting, and the runner should not
            // sit through its idle poll before noticing.
            satellite.work_queued.notify_one();

            protobuf(&ResumeThreadResponse {
                thread: Some(scrubbed(thread)),
            })
        }
        Err(error) => store_failure(&error),
    }
}

async fn drain_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.drain_thread(&thread_id).await {
        Ok(drained) => protobuf(&DrainThreadResponse {
            cancelled_turn_ids: drained.cancelled_turn_ids,
            running_turn_id: drained.running_turn_id,
        }),
        Err(error) => store_failure(&error),
    }
}

async fn get_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    match satellite.store.thread(&thread_id).await {
        Ok(thread) => protobuf(&GetThreadResponse {
            thread: Some(scrubbed(thread)),
        }),
        Err(error) => store_failure(&error),
    }
}

async fn destroy_thread(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
) -> Response {
    // Read first, so destroying an unknown or already collected thread answers
    // with the code for what actually happened rather than succeeding twice.
    if let Err(error) = satellite.store.thread(&thread_id).await {
        return store_failure(&error);
    }

    // Through the collector rather than the store, because destroying a thread
    // has to take its workspace with it. A tombstone whose files remain is a
    // leak nothing later looks for.
    match satellite
        .collector
        .collect(&thread_id, ThreadEndReason::Destroyed)
        .await
    {
        Ok(thread) => protobuf(&DestroyThreadResponse {
            thread: Some(scrubbed(thread)),
        }),
        Err(crate::collector::CollectError::Store(error)) => store_failure(&error),
        Err(error) => contract_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("could not collect the thread: {error}"),
        ),
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
    let order = arsox_sdk::proto::turn::v1::TurnOrder::try_from(request.order_by)
        .unwrap_or(arsox_sdk::proto::turn::v1::TurnOrder::Unspecified);

    match satellite
        .store
        .list_turns(&thread_id, &request.statuses, order, request.descending)
        .await
    {
        Ok(turns) => protobuf(&ListTurnsResponse {
            turns,
            page: Some(PageResponse::default()),
        }),
        Err(error) => store_failure(&error),
    }
}

/// Reads every incident on the satellite, filtered and paged.
///
/// Deliberately not scoped to a live thread. An incident from a collected thread
/// is exactly the incident an operator came looking for, so a filter naming a
/// tombstone answers with its evidence rather than with `THREAD_EXPIRED`.
async fn list_incidents(
    State(satellite): State<Arc<Satellite>>,
    Protobuf(request): Protobuf<ListIncidentsRequest>,
) -> Response {
    incidents(&satellite, request, None).await
}

/// The same listing, scoped to one thread.
///
/// The path wins over any `thread_ids` in the body, so the URL says what it
/// looks like it says. A caller wanting several threads at once has the
/// satellite-wide endpoint.
async fn list_thread_incidents(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Protobuf(request): Protobuf<ListIncidentsRequest>,
) -> Response {
    incidents(&satellite, request, Some(thread_id)).await
}

/// Serves an incident listing, optionally pinned to one thread.
async fn incidents(
    satellite: &Satellite,
    request: ListIncidentsRequest,
    scoped_to: Option<String>,
) -> Response {
    let page = request.page.unwrap_or_default();

    let filter = IncidentFilter {
        thread_ids: scoped_to.map_or(request.thread_ids, |thread_id| vec![thread_id]),
        turn_ids: request.turn_ids,
        member_ids: request.member_ids,
        codes: request.codes,
        dispositions: request.dispositions,
        occurred_after: request.occurred_after.as_ref().map(crate::store::to_nanos),
        occurred_before: request.occurred_before.as_ref().map(crate::store::to_nanos),
        after: if page.cursor.is_empty() {
            None
        } else {
            Some(page.cursor)
        },
        limit: page.limit,
    };

    match satellite.store.list_incidents(&filter).await {
        Ok(listing) => protobuf(&ListIncidentsResponse {
            incidents: listing.incidents,
            page: Some(PageResponse {
                // Carries the sort key alongside the id, because incidents sort
                // by a timestamp and a timestamp is not unique.
                next_cursor: listing.next_cursor,
                // Counting every match would mean a second scan on every page.
                // Absent says "not computed" rather than claiming zero.
                total: None,
            }),
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
        .route("/v1/threads/{thread_id}/pause", post(pause_thread))
        .route("/v1/threads/{thread_id}/resume", post(resume_thread))
        .route("/v1/threads/{thread_id}/drain", post(drain_thread))
        .route("/v1/incidents", get(list_incidents))
        .route(
            "/v1/threads/{thread_id}/incidents",
            get(list_thread_incidents),
        )
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
