// Copyright © 2026 Jalapeno Labs

//! The machinery behind the fixture suite: a transcript in, reviewable JSON out.
//!
//! A recorded transcript and the canonical output it must produce are the whole
//! conformance claim. Asserting that claim in prose, one field at a time, is how
//! a mapper starts dropping a field nobody wrote an assertion for. So every
//! fixture is asserted whole: `<scenario>.stdout.jsonl` maps to
//! `<scenario>.events.json` exactly, and a mapper that changes anything fails
//! with a diff.
//!
//! The rendering here exists because the canonical types are protobuf structs
//! with no JSON representation of their own. It is deliberately explicit rather
//! than derived: a field added to the contract does not silently appear in a
//! fixture, it appears when somebody renders it and re-records the expectation.

use crate::{MappedEvent, Mapping};
use arsox_sdk::proto::common::v1::{Duration, Money, Timestamp};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{Author, AuthorKind};
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::usage::v1::{
    CostEstimate, ModelStatistics, RateLimitStatus, ServerToolUsage, TokenUsage,
};
use serde_json::{Value, json};

use crate::HarnessResult;

/// Maps every non-empty line of a transcript with the mapper under test.
pub(crate) fn map_all(transcript: &str, map_line: fn(&str) -> Mapping) -> Vec<Mapping> {
    transcript
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(map_line)
        .collect()
}

/// Asserts a transcript's canonical output against its recorded expectation.
///
/// # Panics
///
/// When the output differs from the fixture, or when the fixture is not JSON.
pub(crate) fn assert_matches(mappings: &[Mapping], expected: &str) {
    let expected: Value =
        serde_json::from_str(expected).expect("the expectation fixture should be valid JSON");
    let actual = render(mappings);

    assert_eq!(
        actual,
        expected,
        "canonical output drifted from the recorded fixture.\n\nactual:\n{}\n",
        serde_json::to_string_pretty(&actual).unwrap_or_default()
    );
}

/// Everything a transcript produced, in one reviewable document.
fn render(mappings: &[Mapping]) -> Value {
    json!({
        "harness_session_id": mappings
            .iter()
            .find_map(|mapping| mapping.harness_session_id.clone()),
        "events": mappings
            .iter()
            .flat_map(|mapping| &mapping.events)
            .map(render_event)
            .collect::<Vec<Value>>(),
        "result": mappings
            .iter()
            .find_map(|mapping| mapping.result.as_ref())
            .map(render_result),
    })
}

fn render_event(event: &MappedEvent) -> Value {
    json!({
        "type": event.type_name,
        "member_id": event.member_id,
        "occurred_at": event.occurred_at.as_ref().map(render_timestamp),
        "payload": render_payload(&event.payload),
    })
}

fn render_payload(payload: &Payload) -> Value {
    match payload {
        Payload::AgentMessage(message) => json!({
            "author": message.author.as_ref().map(render_author),
            "text": message.text,
        }),
        Payload::AgentThinking(thinking) => json!({
            "author": thinking.author.as_ref().map(render_author),
            "text": thinking.text,
        }),
        Payload::ToolStarted(started) => json!({
            "author": started.author.as_ref().map(render_author),
            "tool_call_id": started.tool_call_id,
            "tool_name": started.tool_name,
            "input": started.input.as_ref().map(render_struct),
        }),
        Payload::ToolCompleted(completed) => json!({
            "author": completed.author.as_ref().map(render_author),
            "tool_call_id": completed.tool_call_id,
            "tool_name": completed.tool_name,
            "ok": completed.ok,
            "output_preview": completed.output_preview,
            "elapsed": completed.elapsed.as_ref().map(render_duration),
        }),
        Payload::RateLimitReported(reported) => json!({
            "endpoint_name": reported.endpoint_name,
            "status": reported.status.as_ref().map(render_rate_limit),
        }),
        Payload::Incident(incident) => json!({
            "code": enum_name(ErrorCode::try_from(incident.code).ok()),
            "disposition": enum_name(Disposition::try_from(incident.disposition).ok()),
            "retryable": incident.retryable,
            "message": incident.message,
        }),
        // A payload no mapper emits yet. Rendering it as null would let a new
        // one slip into a fixture unnoticed, which is the opposite of the point.
        _unrendered => json!({ "unrendered_payload": true }),
    }
}

fn render_result(result: &HarnessResult) -> Value {
    json!({
        "is_error": result.is_error,
        "summary": result.summary,
        "tokens": render_tokens(&result.tokens),
        "cost": render_cost(&result.cost),
        "by_model": result.by_model.iter().map(render_model).collect::<Vec<Value>>(),
        "timing": json!({
            "total": result.timing.total.as_ref().map(render_duration),
            "llm": result.timing.llm.as_ref().map(render_duration),
            "time_to_first_token": result
                .timing
                .time_to_first_token
                .as_ref()
                .map(render_duration),
            "model_round_trips": result.timing.model_round_trips,
        }),
        "stop_reason": result.stop_reason.map(|reason| reason.as_str_name()),
        "permission_denials": result.permission_denials,
    })
}

fn render_tokens(tokens: &TokenUsage) -> Value {
    json!({
        "input_tokens": tokens.input_tokens,
        "output_tokens": tokens.output_tokens,
        "total_tokens": tokens.total_tokens,
        "cache_read_tokens": tokens.cache_read_tokens,
        "cache_write_tokens": tokens.cache_write_tokens,
        "reasoning_output_tokens": tokens.reasoning_output_tokens,
    })
}

fn render_cost(cost: &CostEstimate) -> Value {
    json!({
        "amount": cost.amount.as_ref().map(render_money),
        "is_partial": cost.is_partial,
    })
}

fn render_model(model: &ModelStatistics) -> Value {
    json!({
        "model": model.model,
        "tokens": model.tokens.as_ref().map(render_tokens),
        "cost": model.cost.as_ref().map(render_cost),
        "server_tools": model.server_tools.as_ref().map(render_server_tools),
    })
}

fn render_server_tools(tools: &ServerToolUsage) -> Value {
    json!({
        "web_search_requests": tools.web_search_requests,
        "web_fetch_requests": tools.web_fetch_requests,
    })
}

fn render_rate_limit(status: &RateLimitStatus) -> Value {
    json!({
        "throttled": status.throttled,
        "windows": status
            .windows
            .iter()
            .map(|window| json!({
                "window": window.window,
                "percent_used": window.percent_used,
                "resets_at": window.resets_at.as_ref().map(render_timestamp),
            }))
            .collect::<Vec<Value>>(),
    })
}

fn render_author(author: &Author) -> Value {
    json!({
        "kind": enum_name(AuthorKind::try_from(author.kind).ok()),
        "member_id": author.member_id,
        "role": author.role,
        "parent_member_id": author.parent_member_id,
    })
}

fn render_timestamp(timestamp: &Timestamp) -> Value {
    json!({
        "epoch_seconds": timestamp.epoch_seconds,
        "nanos": timestamp.nanos,
        "timezone": timestamp.timezone,
    })
}

fn render_duration(duration: &Duration) -> Value {
    json!({ "seconds": duration.seconds, "nanos": duration.nanos })
}

fn render_money(money: &Money) -> Value {
    json!({
        "currency_code": money.currency_code,
        "units": money.units,
        "nanos": money.nanos,
    })
}

/// Renders the protobuf `Struct` a tool input travels in back to plain JSON.
fn render_struct(value: &prost_types::Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, field)| (key.clone(), render_proto_value(field)))
            .collect(),
    )
}

fn render_proto_value(value: &prost_types::Value) -> Value {
    use prost_types::value::Kind;

    match &value.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(flag)) => Value::Bool(*flag),
        Some(Kind::NumberValue(number)) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
        Some(Kind::StringValue(text)) => Value::String(text.clone()),
        Some(Kind::ListValue(list)) => {
            Value::Array(list.values.iter().map(render_proto_value).collect())
        }
        Some(Kind::StructValue(fields)) => render_struct(fields),
    }
}

/// The proto's own name for an enum value, or a marker for one it does not know.
///
/// Rendering an unknown discriminant as its number would make a fixture read
/// like a contract change nobody made.
fn enum_name<T: ProtoEnum>(value: Option<T>) -> &'static str {
    value.map_or("<unrecognized>", |value| value.name())
}

/// The generated enums all carry `as_str_name`, but through no shared trait.
trait ProtoEnum: Copy {
    fn name(self) -> &'static str;
}

macro_rules! proto_enum {
    ($type:ty) => {
        impl ProtoEnum for $type {
            fn name(self) -> &'static str {
                self.as_str_name()
            }
        }
    };
}

proto_enum!(ErrorCode);
proto_enum!(Disposition);
proto_enum!(AuthorKind);
