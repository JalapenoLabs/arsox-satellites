// Copyright © 2026 Jalapeno Labs

//! Normalizing what an agent harness emits, with no I/O of its own.
//!
//! # Why this is its own crate
//!
//! An application that already schedules its own containers, supervises its own
//! processes, and owns its own storage still wants one thing from Arsox: the
//! answer to what a harness just did, in one shape, per harness. Getting that
//! from the satellite would mean compiling an HTTP server, a `SQLite` store, a
//! job queue and an LLM proxy into a binary that needs none of them.
//!
//! For a consumer that runs the harness inside a locked-down container those
//! are not merely unused, they are attack surface beside a model running with
//! tool approval disabled. So the knowledge lives here and the plumbing does
//! not: nothing in this crate spawns a process, opens a socket, or touches the
//! filesystem. Feeding it a recorded line is the whole interface, which is also
//! what makes the conformance suite in `fixtures/` possible.
//!
//! This is the module the whole project exists for. Each harness speaks its own
//! event vocabulary, and each mapper here turns that vocabulary into the one
//! canonical contract so an application is written once and never rewritten
//! when the harness changes.
//!
//! # The input is parsed leniently, the output is strongly typed
//!
//! Mappers read native events as loose JSON rather than into strict structs.
//! That is deliberate and it is not a lowering of standards: a harness will add
//! fields on its own schedule, and a strict deserializer would turn every such
//! addition into a hard failure in a satellite that was working yesterday. What
//! must be rigid is what leaves here, and that is a generated protobuf type the
//! compiler checks.
//!
//! # Nothing is dropped
//!
//! A native event a mapper does not recognize becomes a `degraded` incident
//! rather than being skipped. A mapping layer that silently discards what it
//! does not understand looks correct right up until somebody reconciles a bill
//! against it, and silence is exactly what makes that class of bug survive.

pub mod claude;

use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::event::v1::thread_event::Payload;

/// One canonical event, before the event log assigns it a sequence number.
///
/// Sequence numbers are the event log's to hand out, because they must be
/// unique and monotonic per thread and a mapper sees only one harness process.
#[derive(Debug, Clone)]
pub struct MappedEvent {
    /// The event's stable wire name, e.g. `agent.message`.
    ///
    /// Carried alongside the payload for the same reason the contract carries
    /// it: an SDK built against an older proto minor cannot decode a payload
    /// arm it has never heard of, but it can still name and forward the event.
    pub type_name: &'static str,

    /// Which team member produced it, hoisted so a consumer can filter without
    /// decoding the payload.
    pub member_id: Option<String>,

    /// When the harness said it happened. Absent when the native event carried
    /// no timestamp, in which case the event log stamps arrival time instead.
    pub occurred_at: Option<Timestamp>,

    pub payload: Payload,
}

/// Everything one native line produced.
///
/// A single native event can fan out into several canonical ones: a harness
/// message carrying both prose and a tool call is one line in and two events
/// out. The mapping is deliberately one-to-many rather than one-to-one, because
/// forcing it to be one-to-one is how a harness's envelope leaks into the
/// contract.
#[derive(Debug, Clone, Default)]
pub struct Mapping {
    pub events: Vec<MappedEvent>,

    /// The harness's own session identifier, when this line announced it.
    ///
    /// Recorded on the thread so an Arsox thread can be joined to the
    /// harness-side transcripts on disk, which is the first thing wanted when a
    /// run goes wrong.
    pub harness_session_id: Option<String>,

    /// Present only on the line that ends the harness run.
    pub result: Option<HarnessResult>,
}

/// What a harness reported when its run ended.
///
/// Deliberately not a `TurnResult`. A turn is bigger than a harness run: it also
/// covers checkers, self-review, artifact scanning, and the suggestions stage,
/// none of which a harness knows about. The turn runner folds this into the
/// full result rather than the mapper pretending to produce one.
#[derive(Debug, Clone, Default)]
pub struct HarnessResult {
    /// Whether the harness itself considered the run a failure.
    pub is_error: bool,

    /// The agent's closing message.
    pub summary: String,

    /// Totals across every model the run touched.
    pub tokens: arsox_sdk::proto::usage::v1::TokenUsage,

    pub cost: arsox_sdk::proto::usage::v1::CostEstimate,

    /// Split by the model that actually answered, so a failover's cost is
    /// visible rather than folded into the total.
    pub by_model: Vec<arsox_sdk::proto::usage::v1::ModelStatistics>,

    pub timing: arsox_sdk::proto::turn::v1::TurnTiming,

    /// Why the agent stopped. Absent when the harness did not say.
    pub stop_reason: Option<arsox_sdk::proto::turn::v1::StopReason>,

    /// Commands the harness refused to run.
    ///
    /// These become `blocked` incidents. A member that tried something, was
    /// denied, and quietly worked around it is exactly what an operator wants to
    /// see, because it almost always means the allowlist or the setup script is
    /// wrong.
    pub permission_denials: Vec<String>,
}
