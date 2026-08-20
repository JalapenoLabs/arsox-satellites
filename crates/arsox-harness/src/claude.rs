// Copyright © 2026 Jalapeno Labs

//! Maps Claude CLI `stream-json` output into the canonical contract.
//!
//! The satellite drives the CLI with `--print --output-format stream-json`, and
//! every line of stdout is one native event. [`map_line`] turns each into zero
//! or more canonical events.
//!
//! # What the native vocabulary looks like
//!
//! Five line types matter, and two of them are not shaped the way the canonical
//! contract is:
//!
//! - A tool call arrives as an `assistant` line whose `content` array holds a
//!   `tool_use` block, and its outcome arrives later as a **`user`** line
//!   holding a `tool_result` block. Claude models a tool result as something the
//!   user said. The contract models it as `tool.completed`, paired to its start
//!   by `tool_call_id`.
//! - One `assistant` line can carry prose and a tool call together, so a single
//!   native line becomes two canonical events.
//!
//! Neither shape is wrong; they are just a different model. Absorbing that
//! difference is the entire job of this module.

use crate::json::{string_at, to_struct, u64_at};
use crate::{HarnessResult, MappedEvent, Mapping};
use arsox_sdk::proto::common::v1::{Duration, Money, Timestamp};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{
    AgentMessage, AgentThinking, Author, AuthorKind, RateLimitReported, ToolCompleted, ToolStarted,
};
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::turn::v1::{StopReason, TurnTiming};
use arsox_sdk::proto::usage::v1::{
    CostEstimate, ModelStatistics, RateLimitStatus, RateLimitWindow, ServerToolUsage, TokenUsage,
};
use serde_json::Value;

/// Billionths in one whole unit, which is how `Money` stores its fraction.
const NANOS_PER_UNIT: f64 = 1_000_000_000.0;

/// Maps one line of `stream-json` into whatever it represents.
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

    let occurred_at = timestamp(event.get("timestamp").and_then(Value::as_str));
    let author = author(event.get("parent_tool_use_id").and_then(Value::as_str));

    match event.get("type").and_then(Value::as_str) {
        // Announces the session and the environment. Carries no canonical
        // event: everything in it is either configuration the satellite already
        // knows or capability detail reported through `GET /v1/harness`.
        Some("system") => Mapping {
            harness_session_id: event
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Mapping::default()
        },

        Some("rate_limit_event") => Mapping {
            events: map_rate_limit(&event, occurred_at),
            ..Mapping::default()
        },

        Some("assistant") => Mapping {
            events: map_assistant(&event, &author, occurred_at.as_ref()),
            ..Mapping::default()
        },

        Some("user") => Mapping {
            events: map_tool_results(&event, &author, occurred_at.as_ref()),
            ..Mapping::default()
        },

        Some("result") => Mapping {
            result: Some(map_result(&event)),
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
/// Claude identifies a sub-agent by the id of the tool call that spawned it
/// rather than by a member id, so one is derived from it. Deriving is what keeps
/// `parent_tool_use_id` out of the contract: a consumer sees a stable member id
/// and never learns which harness produced its stream.
fn author(parent_tool_use_id: Option<&str>) -> Author {
    match parent_tool_use_id {
        None => Author {
            kind: AuthorKind::Agent.into(),
            ..Author::default()
        },
        Some(parent) => Author {
            kind: AuthorKind::Subagent.into(),
            member_id: Some(format!("subagent-{parent}")),
            ..Author::default()
        },
    }
}

fn map_assistant(
    event: &Value,
    author: &Author,
    occurred_at: Option<&Timestamp>,
) -> Vec<MappedEvent> {
    let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };

    // One native line, many canonical events: prose and a tool call frequently
    // arrive in the same message.
    blocks
        .iter()
        .filter_map(|block| {
            let payload = match block.get("type").and_then(Value::as_str)? {
                "text" => Payload::AgentMessage(AgentMessage {
                    author: Some(author.clone()),
                    text: string_at(block, "text"),
                }),
                "thinking" => Payload::AgentThinking(AgentThinking {
                    author: Some(author.clone()),
                    text: string_at(block, "thinking"),
                }),
                "tool_use" => Payload::ToolStarted(ToolStarted {
                    author: Some(author.clone()),
                    tool_call_id: string_at(block, "id"),
                    tool_name: string_at(block, "name"),
                    // Tool inputs are open-ended by nature, so they travel as a
                    // Struct and are redacted like any other content.
                    input: block.get("input").and_then(to_struct),
                }),
                _unrecognized => return None,
            };

            Some(MappedEvent::new(
                payload,
                author.member_id.clone(),
                occurred_at.cloned(),
            ))
        })
        .collect()
}

/// Maps the `user` line that carries tool results.
///
/// The name is the surprising part of the native protocol. A tool's outcome is
/// not something a user said, and the contract does not pretend otherwise.
fn map_tool_results(
    event: &Value,
    author: &Author,
    occurred_at: Option<&Timestamp>,
) -> Vec<MappedEvent> {
    let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };

    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            let payload = Payload::ToolCompleted(ToolCompleted {
                author: Some(author.clone()),
                tool_call_id: string_at(block, "tool_use_id"),
                // The native result names only the call, not the tool. The turn
                // runner pairs it back to its `tool.started` by `tool_call_id`.
                tool_name: String::new(),
                ok: !block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                output_preview: block.get("content").map(|content| match content.as_str() {
                    Some(text) => text.to_owned(),
                    None => content.to_string(),
                }),
                // Absent from the native event. The turn runner knows when the
                // matching `tool.started` was emitted and fills this in.
                elapsed: None,
            });

            MappedEvent::new(payload, author.member_id.clone(), occurred_at.cloned())
        })
        .collect()
}

fn map_rate_limit(event: &Value, occurred_at: Option<Timestamp>) -> Vec<MappedEvent> {
    let Some(info) = event.get("rate_limit_info") else {
        return Vec::new();
    };

    let window = RateLimitWindow {
        window: string_at(info, "rateLimitType"),
        // The native event reports a status and a reset time but no level, so
        // the level is genuinely absent rather than zero.
        percent_used: None,
        resets_at: info
            .get("resetsAt")
            .and_then(Value::as_i64)
            .map(|seconds| Timestamp {
                epoch_seconds: seconds,
                nanos: 0,
                timezone: arsox_sdk::helpers::DEFAULT_TIMEZONE.to_owned(),
            }),
    };

    let payload = Payload::RateLimitReported(RateLimitReported {
        status: Some(RateLimitStatus {
            windows: vec![window],
            throttled: info.get("status").and_then(Value::as_str) != Some("allowed"),
        }),
        // The harness reports the provider's quota without naming which
        // configured endpoint it came from. The turn runner knows which endpoint
        // is active and fills this in.
        endpoint_name: String::new(),
    });

    vec![MappedEvent::new(payload, None, occurred_at)]
}

fn map_result(event: &Value) -> HarnessResult {
    let usage = event.get("usage");

    HarnessResult {
        is_error: event
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        summary: string_at(event, "result"),
        tokens: usage.map(token_usage).unwrap_or_default(),
        cost: CostEstimate {
            amount: event
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .map(money_from_usd),
            is_partial: false,
        },
        by_model: map_model_usage(event),
        timing: TurnTiming {
            total: millis(event.get("duration_ms")),
            llm: millis(event.get("duration_api_ms")),
            time_to_first_token: millis(event.get("ttft_ms")),
            // Named for what it is. The harness calls this `num_turns`, but a
            // harness turn is one model round trip and an Arsox turn is a whole
            // unit of work.
            model_round_trips: event
                .get("num_turns")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok()),
        },
        stop_reason: event
            .get("stop_reason")
            .and_then(Value::as_str)
            .and_then(stop_reason),
        permission_denials: event
            .get("permission_denials")
            .and_then(Value::as_array)
            .map(|denials| denials.iter().map(ToString::to_string).collect())
            .unwrap_or_default(),
    }
}

/// Splits usage by the model that actually answered.
///
/// Without this, failover is invisible in the accounting: a run where the first
/// endpoint burned tokens failing looks identical to a clean run on the second.
fn map_model_usage(event: &Value) -> Vec<ModelStatistics> {
    let Some(per_model) = event.get("modelUsage").and_then(Value::as_object) else {
        return Vec::new();
    };

    per_model
        .iter()
        .map(|(model, stats)| ModelStatistics {
            model: model.clone(),
            tokens: Some(TokenUsage {
                input_tokens: u64_at(stats, "inputTokens"),
                output_tokens: u64_at(stats, "outputTokens"),
                total_tokens: u64_at(stats, "inputTokens") + u64_at(stats, "outputTokens"),
                cache_read_tokens: stats.get("cacheReadInputTokens").and_then(Value::as_u64),
                cache_write_tokens: stats
                    .get("cacheCreationInputTokens")
                    .and_then(Value::as_u64),
                // This harness folds reasoning into output rather than reporting
                // it separately, so the field is absent rather than zero.
                reasoning_output_tokens: None,
            }),
            cost: Some(CostEstimate {
                amount: stats
                    .get("costUSD")
                    .and_then(Value::as_f64)
                    .map(money_from_usd),
                is_partial: false,
            }),
            server_tools: stats
                .get("webSearchRequests")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .map(|requests| ServerToolUsage {
                    web_search_requests: Some(requests),
                    web_fetch_requests: None,
                }),
        })
        .collect()
}

fn token_usage(usage: &Value) -> TokenUsage {
    let input = u64_at(usage, "input_tokens");
    let output = u64_at(usage, "output_tokens");

    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        // The native event reports no total, so it is derived. Cache tokens are
        // deliberately excluded: they are already counted in `input_tokens` and
        // adding them would double-count every cached request.
        total_tokens: input + output,
        cache_read_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_output_tokens: None,
    }
}

/// Converts a floating point dollar amount into exact integer units.
///
/// The harness reports cost as an `f64`, which is the last place a float is
/// allowed to exist. Everything downstream accumulates in integers, because a
/// ceiling that drifts from the invoice it was meant to predict is the failure
/// this type exists to prevent.
fn money_from_usd(amount: f64) -> Money {
    let whole = amount.trunc();
    let fraction = ((amount - whole) * NANOS_PER_UNIT).round();

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a cost beyond i64 dollars or i32 billionths is not a real invoice"
    )]
    Money {
        currency_code: "USD".to_owned(),
        units: whole as i64,
        nanos: fraction as i32,
    }
}

fn stop_reason(raw: &str) -> Option<StopReason> {
    match raw {
        "end_turn" => Some(StopReason::EndTurn),
        "max_tokens" => Some(StopReason::MaxTokens),
        "stop_sequence" => Some(StopReason::StopSequence),
        "refusal" => Some(StopReason::Refusal),
        // `tool_use` is an intermediate stop, not a terminal one, and any value
        // added later is better absent than guessed at.
        _intermediate_or_unknown => None,
    }
}

fn timestamp(raw: Option<&str>) -> Option<Timestamp> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw?).ok()?;

    Some(Timestamp {
        epoch_seconds: parsed.timestamp(),
        nanos: parsed.timestamp_subsec_nanos(),
        timezone: arsox_sdk::helpers::DEFAULT_TIMEZONE.to_owned(),
    })
}

fn millis(raw: Option<&Value>) -> Option<Duration> {
    let total = raw?.as_i64()?;

    Some(Duration {
        seconds: total / 1_000,
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a millisecond remainder is always under one billion nanoseconds"
        )]
        nanos: ((total % 1_000) * 1_000_000) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::conformance;

    fn map_all(transcript: &str) -> Vec<Mapping> {
        conformance::map_all(transcript, map_line)
    }

    fn all_events(transcript: &str) -> Vec<MappedEvent> {
        map_all(transcript)
            .into_iter()
            .flat_map(|mapping| mapping.events)
            .collect()
    }

    fn event_names(transcript: &str) -> Vec<&'static str> {
        all_events(transcript)
            .iter()
            .map(|event| event.type_name)
            .collect()
    }

    fn result_of(transcript: &str) -> HarnessResult {
        map_all(transcript)
            .into_iter()
            .find_map(|mapping| mapping.result)
            .expect("the transcript should end with a result")
    }

    /// Recorded from Claude CLI 2.1.221, and kept after 2.1.237 was recorded.
    ///
    /// An output shape belongs to a version, so a newer recording proves the
    /// newer CLI and nothing else. These fixtures go on proving the older one,
    /// which is what turns a shape that shifts on upgrade into a diff rather
    /// than a silent change.
    mod v2_1_221 {
        use super::*;

        /// A real `stream-json` transcript, captured from the CLI and scrubbed
        /// of the capturing machine's identifiers.
        ///
        /// Its value is that nobody wrote it from imagination. Every field, and
        /// every place the native shape disagrees with the contract, is
        /// something the harness actually emitted.
        const TOOL_CALL_TRANSCRIPT: &str =
            include_str!("../fixtures/claude/2.1.221/tool-call.stdout.jsonl");

        /// The canonical output that transcript must produce, byte for byte.
        const TOOL_CALL_EVENTS: &str =
            include_str!("../fixtures/claude/2.1.221/tool-call.events.json");

        #[test]
        fn the_transcript_produces_the_recorded_canonical_output() {
            // The whole conformance claim in one assertion: this transcript,
            // these canonical events, exactly. A mapper that starts dropping,
            // reordering, or renaming anything fails here with a diff rather
            // than passing a narrower test that happened not to look at the
            // field it broke.
            conformance::assert_matches(&map_all(TOOL_CALL_TRANSCRIPT), TOOL_CALL_EVENTS);
        }

        #[test]
        fn the_transcript_maps_to_the_expected_canonical_sequence() {
            // Six native lines in, four canonical events out. The `system` line
            // carries configuration rather than an event, and the `result` line
            // becomes a HarnessResult rather than an event.
            assert_eq!(
                event_names(TOOL_CALL_TRANSCRIPT),
                vec![
                    "rate_limit.reported",
                    "tool.started",
                    "tool.completed",
                    "agent.message",
                ]
            );
        }

        #[test]
        fn a_tool_result_arrives_as_a_user_line_and_still_pairs_with_its_call() {
            // The most surprising thing about the native protocol: a tool's
            // outcome is delivered as something the *user* said. If this pairing
            // ever breaks, every tool call in the stream is orphaned.
            let events = all_events(TOOL_CALL_TRANSCRIPT);

            let Payload::ToolStarted(started) = &events[1].payload else {
                panic!("expected the second event to be a tool call");
            };
            let Payload::ToolCompleted(completed) = &events[2].payload else {
                panic!("expected the third event to be a tool result");
            };

            assert_eq!(started.tool_name, "Bash");
            assert!(!started.tool_call_id.is_empty());
            assert_eq!(started.tool_call_id, completed.tool_call_id);
            assert!(completed.ok);

            // The command survives into the Struct rather than being flattened
            // to a string, which is what will let the exec broker inspect it.
            let input = started
                .input
                .as_ref()
                .expect("tool call should carry input");
            assert!(input.fields.contains_key("command"));
        }

        #[test]
        fn the_harness_session_id_is_captured_from_the_init_line() {
            let session = map_all(TOOL_CALL_TRANSCRIPT)
                .into_iter()
                .find_map(|mapping| mapping.harness_session_id);

            assert_eq!(
                session.as_deref(),
                Some("0199c0de-1111-7000-8000-000000000001")
            );
        }

        #[test]
        fn the_result_line_yields_usage_cost_and_timing() {
            let result = result_of(TOOL_CALL_TRANSCRIPT);

            assert!(!result.is_error);
            assert_eq!(result.stop_reason, Some(StopReason::EndTurn));

            // Cost crosses from the harness's f64 into exact integer units here,
            // and this is the only place a float is allowed to touch a cost.
            let amount = result.cost.amount.expect("a priced run should carry cost");
            assert_eq!(amount.currency_code, "USD");
            assert!(amount.units > 0 || amount.nanos > 0);

            assert!(result.timing.total.is_some());
            assert!(result.timing.time_to_first_token.is_some());
            // Two round trips, not one: the model was called once to issue the
            // tool call and again to answer after seeing its result. This is
            // exactly the distinction the field exists to draw, and it is why it
            // counts model round trips rather than Arsox turns, of which there
            // was one.
            assert_eq!(result.timing.model_round_trips, Some(2));
        }

        #[test]
        fn cache_tokens_are_reported_beside_input_rather_than_inside_it() {
            let result = result_of(TOOL_CALL_TRANSCRIPT);

            assert!(result.tokens.cache_read_tokens.is_some());
            assert_eq!(
                result.tokens.total_tokens,
                result.tokens.input_tokens + result.tokens.output_tokens,
                "cache tokens are already counted in input and must not be added again"
            );
        }

        #[test]
        fn usage_is_split_by_the_model_that_answered() {
            // The run used more than one model. Folding them into a single total
            // is what makes a failover's cost invisible.
            let result = result_of(TOOL_CALL_TRANSCRIPT);

            assert!(
                result.by_model.len() >= 2,
                "expected a per-model breakdown, got {:?}",
                result.by_model
            );
            assert!(result.by_model.iter().all(|model| model.tokens.is_some()));
        }
    }

    /// Recorded from Claude CLI 2.1.237, live, four scenarios in one sitting.
    ///
    /// Every transcript here is unedited except for the `system` init line,
    /// which is the only place the CLI reports the capturing machine's installed
    /// tooling rather than its own behaviour. See `fixtures/README.md`.
    mod v2_1_237 {
        use super::*;

        /// A prose answer and nothing else: no tool, no reasoning, one message.
        const PLAIN_TEXT_TRANSCRIPT: &str =
            include_str!("../fixtures/claude/2.1.237/plain-text.stdout.jsonl");
        const PLAIN_TEXT_EVENTS: &str =
            include_str!("../fixtures/claude/2.1.237/plain-text.events.json");

        /// A shell command, its result, and the answer that followed it.
        const TOOL_CALL_TRANSCRIPT: &str =
            include_str!("../fixtures/claude/2.1.237/tool-call.stdout.jsonl");
        const TOOL_CALL_EVENTS: &str =
            include_str!("../fixtures/claude/2.1.237/tool-call.events.json");

        /// A run the CLI refused to start, resumed against a session id that
        /// does not exist. One line of stdout, and every byte of it is a failure
        /// the harness reported structurally rather than as prose.
        const ERROR_RESULT_TRANSCRIPT: &str =
            include_str!("../fixtures/claude/2.1.237/error-result.stdout.jsonl");
        const ERROR_RESULT_EVENTS: &str =
            include_str!("../fixtures/claude/2.1.237/error-result.events.json");

        /// A turn that reasoned, spoke, used a tool, and spoke again.
        const MULTI_MESSAGE_TRANSCRIPT: &str =
            include_str!("../fixtures/claude/2.1.237/multi-message.stdout.jsonl");
        const MULTI_MESSAGE_EVENTS: &str =
            include_str!("../fixtures/claude/2.1.237/multi-message.events.json");

        #[test]
        fn the_plain_text_transcript_produces_the_recorded_canonical_output() {
            conformance::assert_matches(&map_all(PLAIN_TEXT_TRANSCRIPT), PLAIN_TEXT_EVENTS);
        }

        #[test]
        fn the_tool_call_transcript_produces_the_recorded_canonical_output() {
            conformance::assert_matches(&map_all(TOOL_CALL_TRANSCRIPT), TOOL_CALL_EVENTS);
        }

        #[test]
        fn the_error_result_transcript_produces_the_recorded_canonical_output() {
            conformance::assert_matches(&map_all(ERROR_RESULT_TRANSCRIPT), ERROR_RESULT_EVENTS);
        }

        #[test]
        fn the_multi_message_transcript_produces_the_recorded_canonical_output() {
            conformance::assert_matches(&map_all(MULTI_MESSAGE_TRANSCRIPT), MULTI_MESSAGE_EVENTS);
        }

        #[test]
        fn a_prose_answer_produces_one_message_and_no_tool_call() {
            // The simplest turn there is, and the one a mapper built around tool
            // calls is most likely to fumble.
            assert_eq!(
                event_names(PLAIN_TEXT_TRANSCRIPT),
                vec!["rate_limit.reported", "agent.message"]
            );
            assert_eq!(result_of(PLAIN_TEXT_TRANSCRIPT).summary, "CONFORMANCE");
        }

        #[test]
        fn a_tool_call_still_pairs_its_start_with_a_result_delivered_as_a_user_line() {
            let events = all_events(TOOL_CALL_TRANSCRIPT);

            let Payload::ToolStarted(started) = &events[1].payload else {
                panic!("expected the second event to be a tool call");
            };
            let Payload::ToolCompleted(completed) = &events[2].payload else {
                panic!("expected the third event to be a tool result");
            };

            assert_eq!(started.tool_name, "Bash");
            assert_eq!(started.tool_call_id, completed.tool_call_id);
            assert!(completed.ok);
            assert_eq!(completed.output_preview.as_deref(), Some("arsox-probe"));
        }

        #[test]
        fn a_reasoning_turn_reports_thinking_beside_its_messages() {
            // One turn, four kinds of event. A mapper that handled only prose
            // and tool calls would drop the reasoning entirely, and a consumer
            // watching the stream would see the agent go quiet and then act.
            assert_eq!(
                event_names(MULTI_MESSAGE_TRANSCRIPT),
                vec![
                    "rate_limit.reported",
                    "agent.thinking",
                    "agent.message",
                    "tool.started",
                    "tool.completed",
                    "agent.message",
                ]
            );
        }

        #[test]
        fn a_redacted_thinking_block_is_still_an_event() {
            // The recorded block carries a signature and no text: this model
            // returns its reasoning encrypted rather than in the clear. The
            // event is emitted anyway, because "the agent reasoned here" is true
            // and dropping it would make the turn look like it acted without
            // thinking.
            let events = all_events(MULTI_MESSAGE_TRANSCRIPT);

            let Payload::AgentThinking(thinking) = &events[1].payload else {
                panic!("expected the second event to be reasoning");
            };

            assert_eq!(thinking.text, "");
        }

        #[test]
        fn a_run_the_harness_refused_to_start_is_an_error_result_and_no_events() {
            // The CLI writes one line and exits nonzero. There is no session
            // line, no message, and nothing the agent did, so the whole turn is
            // the failure it reported.
            let result = result_of(ERROR_RESULT_TRANSCRIPT);

            assert!(all_events(ERROR_RESULT_TRANSCRIPT).is_empty());
            assert!(result.is_error);
            assert_eq!(result.stop_reason, None);
        }
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
    fn dollars_convert_to_exact_integer_units() {
        assert_eq!(money_from_usd(0.0).units, 0);

        let fraction = money_from_usd(0.114_034);
        assert_eq!(fraction.units, 0);
        assert_eq!(fraction.nanos, 114_034_000);

        let whole = money_from_usd(40.5);
        assert_eq!(whole.units, 40);
        assert_eq!(whole.nanos, 500_000_000);
    }

    #[test]
    fn millisecond_durations_split_into_seconds_and_nanos() {
        let duration = millis(Some(&serde_json::json!(1_793))).expect("should parse");

        assert_eq!(duration.seconds, 1);
        assert_eq!(duration.nanos, 793_000_000);
    }
}
