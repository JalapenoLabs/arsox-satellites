// Copyright © 2026 Jalapeno Labs

//! Maps Codex CLI `exec --json` output into the canonical contract.
//!
//! The satellite drives the CLI with `codex exec --json`, and every line of
//! stdout is one native event. [`map_line`] turns each into zero or more
//! canonical events.
//!
//! **`--json` is not optional.** Without it `codex exec` writes a human report,
//! and a mapper pointed at that would parse prose as protocol. The flag belongs
//! to the spawn, but the consequence belongs here: this module reads JSONL and
//! nothing else.
//!
//! # What the native vocabulary looks like
//!
//! Codex models a run as a thread that contains turns, and a turn as a list of
//! *items*. Lifecycle events (`thread.started`, `turn.started`,
//! `turn.completed`, `turn.failed`) bracket the run, and everything the agent
//! actually did arrives as `item.started`, `item.updated` and `item.completed`
//! carrying one of eight item shapes.
//!
//! Three of those shapes disagree with the canonical contract, and each is a
//! place a naive mapper produces something wrong rather than something missing:
//!
//! - **The usage event names no model.** `turn.completed` carries token counts
//!   and nothing else, so a mapper that required a model and a count on one
//!   object would discard every count Codex reports. Usage is mapped without a
//!   model, and `by_model` is left empty for the runner, which knows which
//!   endpoint answered.
//! - **`input_tokens` includes the cached tokens.** Anthropic excludes them and
//!   the canonical shape follows Anthropic, so the cached count is subtracted
//!   here. Copying the field across would overstate a cached run by most of its
//!   prompt. See [`token_usage`].
//! - **A to-do list, a patch, and a web search are items rather than tool
//!   calls.** Claude delivers the same three things as tool calls, and the
//!   contract has one shape for a tool call. So they map to `tool.started` and
//!   `tool.completed` like anything else the agent invoked, rather than to
//!   nothing at all.
//!
//! # What this mapper cannot see
//!
//! Codex stamps a `client_metadata` object on its API requests carrying
//! `session_id`, `thread_id`, `turn_id` and `turn_started_at_unix_ms`. None of
//! it reaches stdout, so the thread id here comes from `thread.started`, which
//! is also the id `codex exec resume` takes.
//!
//! The closing message is an ordinary `agent_message` item rather than part of
//! `turn.completed`. Mapping is a pure function of one line and carries no state
//! between lines, so [`HarnessResult::summary`] is left empty and the runner
//! takes the summary from the last `agent.message` it saw.

use crate::json::{string_at, to_struct, u64_at};
use crate::{HarnessResult, MappedEvent, Mapping};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{
    AgentMessage, AgentThinking, Author, AuthorKind, ToolCompleted, ToolStarted,
};
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::usage::v1::{CostEstimate, TokenUsage};
use serde_json::Value;

/// Maps one line of `exec --json` into whatever it represents.
///
/// A line that is not valid JSON, or whose `type` this mapper does not know,
/// produces a `degraded` incident rather than an error or a silent skip. A
/// harness adding an event type must never take a satellite down, and must never
/// pass unnoticed either.
pub fn map_line(line: &str) -> Mapping {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Mapping {
            events: vec![MappedEvent::incident(
                ErrorCode::Internal,
                Disposition::Degraded,
                "harness emitted a line that is not valid JSON",
            )],
            ..Mapping::default()
        };
    };

    match event.get("type").and_then(Value::as_str) {
        // The id `codex exec resume` takes, which is what joins an Arsox thread
        // to the harness-side transcript on disk.
        Some("thread.started") => Mapping {
            harness_session_id: event
                .get("thread_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Mapping::default()
        },

        // Neither carries a canonical event, for two different reasons.
        //
        // A harness turn is one prompt and its answer; an Arsox turn is a whole
        // unit of work that the satellite started and is already timing, so
        // reporting `turn.started` would put the harness's meaning on the
        // contract's word.
        //
        // An update restates an item the stream delivers in full when it
        // completes: a command's output as it accumulates, a to-do list as its
        // steps tick over. One canonical event per restatement would put the
        // same tool call on the stream a dozen times, so the terminal
        // `item.completed` is the one that speaks.
        Some("turn.started" | "item.updated") => Mapping::default(),

        Some("turn.completed") => Mapping {
            result: Some(completed_turn(&event)),
            ..Mapping::default()
        },

        Some("turn.failed") => Mapping {
            result: Some(failed_turn(&event)),
            ..Mapping::default()
        },

        Some("item.started") => Mapping {
            events: map_item_started(&event),
            ..Mapping::default()
        },

        Some("item.completed") => Mapping {
            events: map_item_completed(&event),
            ..Mapping::default()
        },

        // Recorded from a real run: transport retries arrive here as
        // `Reconnecting... 2/5 (...)` and the turn goes on to succeed. So this
        // is degraded rather than fatal, and the terminal failure, when there is
        // one, arrives separately as `turn.failed`.
        Some("error") => Mapping {
            events: vec![MappedEvent::incident(
                ErrorCode::Internal,
                Disposition::Degraded,
                &format!(
                    "harness reported an error: {}",
                    string_at(&event, "message")
                ),
            )],
            ..Mapping::default()
        },

        unknown => Mapping {
            events: vec![MappedEvent::incident(
                ErrorCode::Internal,
                Disposition::Degraded,
                &format!(
                    "harness emitted an unrecognized event type {:?}, which was recorded and dropped",
                    unknown.unwrap_or("<absent>")
                ),
            )],
            ..Mapping::default()
        },
    }
}

/// Attributes an event to the agent that produced it.
///
/// Codex's exec stream has no sub-agent tier and carries no member id, so there
/// is nothing to derive one from. Under team mode the satellite runs one process
/// per member and already knows which member it spawned.
fn author() -> Author {
    Author {
        kind: AuthorKind::Agent.into(),
        ..Author::default()
    }
}

fn map_item_started(event: &Value) -> Vec<MappedEvent> {
    let Some(item) = event.get("item") else {
        return Vec::new();
    };

    // Prose and reasoning start empty and are worth reporting only once they
    // are finished, and an error item is the failure itself rather than the
    // start of one.
    let payload = match item.get("type").and_then(Value::as_str) {
        Some("agent_message" | "reasoning" | "error") | None => return Vec::new(),
        Some(kind) => Payload::ToolStarted(ToolStarted {
            author: Some(author()),
            tool_call_id: string_at(item, "id"),
            tool_name: tool_name(kind, item),
            input: tool_input(kind, item),
        }),
    };

    vec![MappedEvent::new(payload, None, None)]
}

fn map_item_completed(event: &Value) -> Vec<MappedEvent> {
    let Some(item) = event.get("item") else {
        return Vec::new();
    };

    let payload = match item.get("type").and_then(Value::as_str) {
        None => return Vec::new(),

        Some("agent_message") => Payload::AgentMessage(AgentMessage {
            author: Some(author()),
            text: string_at(item, "text"),
        }),

        // Codex reports a reasoning summary rather than raw reasoning, which is
        // the same thing Claude's `thinking` block carries.
        Some("reasoning") => Payload::AgentThinking(AgentThinking {
            author: Some(author()),
            text: string_at(item, "text"),
        }),

        // A non-fatal failure the harness chose to surface as an item. It is
        // exactly what an incident is for, and dropping it would leave the turn
        // looking clean.
        Some("error") => {
            return vec![MappedEvent::incident(
                ErrorCode::Internal,
                Disposition::Degraded,
                &format!("harness reported an error: {}", string_at(item, "message")),
            )];
        }

        Some(kind) => Payload::ToolCompleted(ToolCompleted {
            author: Some(author()),
            tool_call_id: string_at(item, "id"),
            tool_name: tool_name(kind, item),
            ok: succeeded(item),
            output_preview: output_preview(kind, item),
            // Absent from the native event. The turn runner knows when the
            // matching `tool.started` was emitted and fills this in.
            elapsed: None,
        }),
    };

    vec![MappedEvent::new(payload, None, None)]
}

/// Names the tool an item represents.
///
/// Codex's own names are used where it has one, so a command shows up as
/// `shell` and a patch as `apply_patch`, which is what an operator reading the
/// stream will also see in the harness's own logs. An MCP call is qualified by
/// its server, because two servers may serve a tool of the same name and a
/// consumer grouping by tool name would otherwise merge them.
fn tool_name(kind: &str, item: &Value) -> String {
    match kind {
        "command_execution" => "shell".to_owned(),
        "file_change" => "apply_patch".to_owned(),
        "mcp_tool_call" => format!("{}/{}", string_at(item, "server"), string_at(item, "tool")),
        other => other.to_owned(),
    }
}

/// The arguments the tool was invoked with, in the contract's open-ended Struct.
///
/// Built field by field rather than by handing the native item over whole: the
/// item carries its own envelope (`id`, `type`, `status`) and letting that into
/// the contract is exactly the harness leak the contract exists to prevent.
fn tool_input(kind: &str, item: &Value) -> Option<prost_types::Struct> {
    let input = match kind {
        "command_execution" => serde_json::json!({ "command": item.get("command")? }),
        "file_change" => serde_json::json!({ "changes": item.get("changes")? }),
        "web_search" => serde_json::json!({ "query": item.get("query")? }),
        "todo_list" => serde_json::json!({ "items": item.get("items")? }),
        // MCP arguments are the tool's own shape and are already a JSON object.
        "mcp_tool_call" => item.get("arguments")?.clone(),
        _unrecognized => return None,
    };

    to_struct(&input)
}

/// Whether the item reports having done what it set out to do.
///
/// A command that ran to completion and exited nonzero is a failed tool call,
/// not a successful one, so the exit code decides the outcome wherever the
/// harness reports one.
fn succeeded(item: &Value) -> bool {
    let exit_code = item.get("exit_code").and_then(Value::as_i64);

    match item.get("status").and_then(Value::as_str) {
        Some("failed") => false,
        // No status at all: the item has no way to fail, e.g. a web search.
        _completed_or_absent => exit_code.unwrap_or(0) == 0,
    }
}

/// The head of the result, for display.
///
/// A patch and a to-do list are rendered rather than dumped, because their
/// native shape is a list of objects and a preview is read by a human.
fn output_preview(kind: &str, item: &Value) -> Option<String> {
    match kind {
        "command_execution" => Some(string_at(item, "aggregated_output")),

        "file_change" => Some(
            item.get("changes")?
                .as_array()?
                .iter()
                .map(|change| {
                    format!(
                        "{} {}",
                        string_at(change, "kind"),
                        string_at(change, "path")
                    )
                })
                .collect::<Vec<String>>()
                .join("\n"),
        ),

        "todo_list" => Some(
            item.get("items")?
                .as_array()?
                .iter()
                .map(|entry| {
                    let done = entry.get("completed").and_then(Value::as_bool) == Some(true);
                    format!(
                        "[{}] {}",
                        if done { "x" } else { " " },
                        string_at(entry, "text")
                    )
                })
                .collect::<Vec<String>>()
                .join("\n"),
        ),

        "mcp_tool_call" => match item.get("error") {
            Some(error) => Some(string_at(error, "message")),
            None => Some(item.get("result")?.to_string()),
        },

        // A web search reports the query it ran and no result, so there is
        // genuinely nothing to preview.
        _no_result => None,
    }
}

fn completed_turn(event: &Value) -> HarnessResult {
    HarnessResult {
        tokens: event.get("usage").map(token_usage).unwrap_or_default(),
        // Codex reports no cost, so there is no estimate rather than a
        // confident zero. The satellite prices the run from the token counts and
        // the endpoint that answered.
        cost: CostEstimate {
            amount: None,
            is_partial: false,
        },
        // The usage event names no model. Splitting by model here would mean
        // inventing one, and the runner already knows which endpoint answered.
        by_model: Vec::new(),
        // The native event reports neither a duration nor a round trip count.
        // The runner times the turn it started.
        ..HarnessResult::default()
    }
}

fn failed_turn(event: &Value) -> HarnessResult {
    HarnessResult {
        is_error: true,
        summary: event
            .get("error")
            .map(|error| string_at(error, "message"))
            .unwrap_or_default(),
        ..HarnessResult::default()
    }
}

/// Maps Codex's token counts onto the canonical shape.
///
/// The two vendors disagree about what `input_tokens` means. Codex reports the
/// cached tokens **inside** it and breaks the cached count out beside it;
/// Anthropic reports input and cache separately. The canonical shape follows
/// Anthropic, so the cached count is subtracted here. Without the subtraction a
/// cached run is overstated by most of its prompt, and the overstatement lands
/// in a billing-adjacent number where nobody notices it until a reconciliation.
fn token_usage(usage: &Value) -> TokenUsage {
    let cached = cached_input(usage);
    let input = u64_at(usage, "input_tokens").saturating_sub(cached.unwrap_or_default());
    let output = u64_at(usage, "output_tokens");

    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        // Derived, and derived from the corrected input. Cache reads are
        // deliberately excluded: they are reported separately in this shape and
        // adding them back would restore the double count just removed.
        total_tokens: input + output,
        cache_read_tokens: cached,

        // Genuinely absent rather than zero, which `usage.proto` calls out by
        // name: this provider has no cache-write concept, so a zero it reports
        // is a structural placeholder rather than a measurement. Claiming "this
        // run wrote nothing to cache" is a different and false statement. A
        // count above zero is a real measurement and is carried through, so a
        // provider that grows the concept is already mapped.
        cache_write_tokens: usage
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0),

        // Reported separately from output rather than folded into it, which is
        // the distinction a consumer reconciling a bill needs.
        reasoning_output_tokens: usage
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
            }),
    }
}

/// The cached input count, under either name the vendor spells it.
///
/// `codex exec --json` flattens it to `cached_input_tokens`. The provider's own
/// usage object, which is what the Arsox LLM proxy sees on the way past, nests
/// it under `input_tokens_details`. Knowing only one spelling would report a
/// cached run as having read nothing from cache, and then subtract nothing from
/// an input count that included it.
fn cached_input(usage: &Value) -> Option<u64> {
    usage
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;

    /// A `codex exec --json` transcript for a single shell command.
    ///
    /// Its lifecycle lines are recorded from Codex 0.147.0; the rest is built
    /// from the event schema published with that exact version. See
    /// `fixtures/README.md`, which says precisely which lines are which.
    const TOOL_CALL_TRANSCRIPT: &str =
        include_str!("../fixtures/codex/0.147.0/tool-call.stdout.jsonl");

    /// The canonical output that transcript must produce, byte for byte.
    const TOOL_CALL_EVENTS: &str = include_str!("../fixtures/codex/0.147.0/tool-call.events.json");

    /// A run whose credentials had expired, recorded from Codex 0.147.0 in full.
    ///
    /// Every line of it is something the CLI actually wrote, which is what makes
    /// it evidence: a stream error is not fatal on its own, and the failure that
    /// ends the turn arrives separately as `turn.failed`.
    const AUTH_FAILURE_TRANSCRIPT: &str =
        include_str!("../fixtures/codex/0.147.0/auth-failure.stdout.jsonl");

    const AUTH_FAILURE_EVENTS: &str =
        include_str!("../fixtures/codex/0.147.0/auth-failure.events.json");

    fn map_all(transcript: &str) -> Vec<Mapping> {
        conformance::map_all(transcript, map_line)
    }

    fn all_events(transcript: &str) -> Vec<MappedEvent> {
        map_all(transcript)
            .into_iter()
            .flat_map(|mapping| mapping.events)
            .collect()
    }

    fn result_of(transcript: &str) -> HarnessResult {
        map_all(transcript)
            .into_iter()
            .find_map(|mapping| mapping.result)
            .expect("the transcript should end with a result")
    }

    fn usage_of(json: &str) -> TokenUsage {
        token_usage(&serde_json::from_str::<Value>(json).expect("the usage should be valid JSON"))
    }

    #[test]
    fn the_tool_call_transcript_produces_the_recorded_canonical_output() {
        // The whole conformance claim in one assertion: this transcript, these
        // canonical events, exactly. A mapper that starts dropping, reordering,
        // or renaming anything fails here with a diff rather than passing a
        // narrower test that happened not to look at the field it broke.
        conformance::assert_matches(&map_all(TOOL_CALL_TRANSCRIPT), TOOL_CALL_EVENTS);
    }

    #[test]
    fn the_auth_failure_transcript_produces_the_recorded_canonical_output() {
        conformance::assert_matches(&map_all(AUTH_FAILURE_TRANSCRIPT), AUTH_FAILURE_EVENTS);
    }

    #[test]
    fn the_transcript_maps_to_the_expected_canonical_sequence() {
        let names: Vec<&str> = all_events(TOOL_CALL_TRANSCRIPT)
            .iter()
            .map(|event| event.type_name)
            .collect();

        // Ten native lines in, four canonical events out. The lifecycle lines
        // carry no events of their own: `thread.started` carries the session id,
        // `turn.started` carries nothing, and `turn.completed` becomes a
        // HarnessResult. An item's start and its updates are silent for
        // everything whose content only exists once it has finished.
        assert_eq!(
            names,
            vec![
                "agent.thinking",
                "tool.started",
                "tool.completed",
                "agent.message",
            ]
        );
    }

    #[test]
    fn a_command_pairs_its_start_with_its_completion() {
        let events = all_events(TOOL_CALL_TRANSCRIPT);

        let Payload::ToolStarted(started) = &events[1].payload else {
            panic!("expected the second event to be a tool call");
        };
        let Payload::ToolCompleted(completed) = &events[2].payload else {
            panic!("expected the third event to be a tool result");
        };

        assert_eq!(started.tool_name, "shell");
        assert_eq!(started.tool_call_id, completed.tool_call_id);
        assert!(completed.ok);
        assert_eq!(
            completed.output_preview.as_deref(),
            Some("arsox-probe\n"),
            "the command's output is what a consumer reads"
        );

        // The command survives into the Struct rather than being flattened to a
        // string, which is what will let the exec broker inspect it.
        let input = started
            .input
            .as_ref()
            .expect("tool call should carry input");
        assert!(input.fields.contains_key("command"));
    }

    #[test]
    fn the_harness_session_id_is_captured_from_the_thread_line() {
        let session = map_all(TOOL_CALL_TRANSCRIPT)
            .into_iter()
            .find_map(|mapping| mapping.harness_session_id);

        assert_eq!(
            session.as_deref(),
            Some("01a01bb8-0c55-7ef2-9774-39703eec4482")
        );
    }

    #[test]
    fn cached_tokens_are_subtracted_from_the_input_count() {
        // The defect this prevents: Codex counts cached tokens inside
        // `input_tokens` and Anthropic counts them beside it, so a mapper that
        // copies the field across overstates a cached run by most of its
        // prompt, in a number somebody eventually reconciles against a bill.
        let tokens = result_of(TOOL_CALL_TRANSCRIPT).tokens;

        assert_eq!(tokens.cache_read_tokens, Some(11_008));
        assert_eq!(
            tokens.input_tokens, 1_337,
            "12,345 reported minus 11,008 cached"
        );
        assert_eq!(
            tokens.total_tokens,
            tokens.input_tokens + tokens.output_tokens,
            "cache reads are reported beside input and must not be added back"
        );
    }

    #[test]
    fn the_cached_count_is_read_under_either_spelling() {
        // The exec stream flattens it; the provider's own usage object, which
        // the LLM proxy sees, nests it. Knowing one spelling and not the other
        // means subtracting nothing from an input count that included it.
        let nested = usage_of(
            r#"{"input_tokens":12345,"input_tokens_details":{"cached_tokens":11008},"output_tokens":57}"#,
        );

        assert_eq!(nested.cache_read_tokens, Some(11_008));
        assert_eq!(nested.input_tokens, 1_337);
    }

    #[test]
    fn cache_writes_are_absent_rather_than_zero() {
        // This provider has no cache-write concept, so the zero it reports is a
        // placeholder rather than a measurement, and "this run wrote nothing to
        // cache" is a claim the contract must not make on its behalf.
        let reported_zero = usage_of(r#"{"input_tokens":10,"cache_write_input_tokens":0}"#);
        let unreported = usage_of(r#"{"input_tokens":10}"#);
        let measured = usage_of(r#"{"input_tokens":10,"cache_write_input_tokens":4096}"#);

        assert_eq!(reported_zero.cache_write_tokens, None);
        assert_eq!(unreported.cache_write_tokens, None);
        // A real count is still carried, so a provider that grows the concept
        // needs no change here.
        assert_eq!(measured.cache_write_tokens, Some(4096));
    }

    #[test]
    fn reasoning_tokens_are_reported_rather_than_folded_into_output() {
        let tokens = result_of(TOOL_CALL_TRANSCRIPT).tokens;

        assert_eq!(tokens.reasoning_output_tokens, Some(32));
        assert_eq!(
            usage_of(r#"{"output_tokens":57}"#).reasoning_output_tokens,
            None,
            "a harness that does not separate reasoning reports absence, not zero"
        );
    }

    #[test]
    fn usage_is_mapped_even_though_the_event_names_no_model() {
        // The trap this test guards: `turn.completed` carries counts and no
        // model, so requiring both on one object discards every count Codex
        // reports.
        let result = result_of(TOOL_CALL_TRANSCRIPT);

        assert!(result.by_model.is_empty());
        assert!(result.tokens.total_tokens > 0);
        assert_eq!(
            result.cost.amount, None,
            "the harness publishes no cost, and a confident zero would be worse than none"
        );
    }

    #[test]
    fn a_failed_turn_reports_the_error_it_ended_on() {
        let result = result_of(AUTH_FAILURE_TRANSCRIPT);

        assert!(result.is_error);
        assert!(result.summary.contains("refresh token"));
    }

    #[test]
    fn a_stream_error_degrades_rather_than_ending_the_turn() {
        // A stream `error` is not the end of a turn: the recorded reconnect
        // messages are retries the run survives. Treating one as fatal would
        // fail turns that went on to succeed, so the terminal failure is left to
        // `turn.failed`.
        let mapping = map_line(r#"{"type":"error","message":"Reconnecting... 2/5"}"#);

        let [event] = mapping.events.as_slice() else {
            panic!("a stream error should produce exactly one incident");
        };
        let Payload::Incident(incident) = &event.payload else {
            panic!("a stream error should produce an incident");
        };

        assert!(mapping.result.is_none());
        assert_eq!(incident.disposition, Disposition::Degraded as i32);
        assert!(incident.message.contains("Reconnecting"));
    }

    #[test]
    fn an_unrecognized_event_type_is_recorded_rather_than_dropped() {
        let mapping = map_line(r#"{"type":"some_future_event","payload":{}}"#);

        let [event] = mapping.events.as_slice() else {
            panic!("an unknown type should produce exactly one incident");
        };
        let Payload::Incident(incident) = &event.payload else {
            panic!("an unknown type should produce an incident");
        };

        assert_eq!(incident.disposition, Disposition::Degraded as i32);
        assert!(incident.message.contains("some_future_event"));
    }

    #[test]
    fn an_unrecognized_item_type_is_recorded_rather_than_dropped() {
        // An item shape added in a later Codex still becomes a tool call rather
        // than nothing, because the alternative is a run whose stream is missing
        // work the agent actually did.
        let mapping = map_line(
            r#"{"type":"item.completed","item":{"id":"item_9","type":"some_future_item","status":"completed"}}"#,
        );

        let [event] = mapping.events.as_slice() else {
            panic!("an unknown item should still produce one event");
        };
        let Payload::ToolCompleted(completed) = &event.payload else {
            panic!("an unknown item should be reported as a tool call");
        };

        assert_eq!(completed.tool_name, "some_future_item");
        assert_eq!(completed.tool_call_id, "item_9");
    }

    #[test]
    fn a_malformed_line_degrades_rather_than_failing_the_turn() {
        // A harness writing a partial line as it crashes must not take the
        // satellite down with it.
        let mapping = map_line("{not json");

        let [event] = mapping.events.as_slice() else {
            panic!("a malformed line should produce exactly one incident");
        };
        assert!(matches!(event.payload, Payload::Incident(_)));
    }

    #[test]
    fn a_command_that_exits_nonzero_is_a_failed_tool_call() {
        let mapping = map_line(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"false","aggregated_output":"","exit_code":1,"status":"completed"}}"#,
        );

        let [event] = mapping.events.as_slice() else {
            panic!("a command should produce one event");
        };
        let Payload::ToolCompleted(completed) = &event.payload else {
            panic!("a completed command should be a tool result");
        };

        assert!(
            !completed.ok,
            "a command that ran and exited nonzero failed, whatever the item status says"
        );
    }
}
