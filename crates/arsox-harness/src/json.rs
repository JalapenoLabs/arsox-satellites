// Copyright © 2026 Jalapeno Labs

//! Reading loose native JSON, and carrying it into the contract's `Struct`.
//!
//! Every mapper reads its harness leniently, so every mapper needs the same
//! small vocabulary for pulling a field out of a value that may not have it.
//! Keeping that vocabulary here rather than in each mapper means a second
//! harness costs a mapper rather than a mapper plus a copy of these.

use serde_json::Value;

/// The string at `key`, or an empty string when it is absent or not a string.
///
/// The contract's non-optional string fields are empty when unreported, so a
/// missing field and an empty one are already the same statement.
pub(crate) fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The count at `key`, or zero when it is absent or not a number.
///
/// Only for fields the contract carries as a bare count. Anything the contract
/// marks `optional` must read the value itself, because absent and zero are
/// different statements there.
pub(crate) fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

/// Converts arbitrary JSON into the protobuf `Struct` the contract carries.
///
/// Returns `None` for anything that is not a JSON object, since `Struct` has no
/// representation for a bare scalar at the top level.
pub(crate) fn to_struct(value: &Value) -> Option<prost_types::Struct> {
    let object = value.as_object()?;

    Some(prost_types::Struct {
        fields: object
            .iter()
            .map(|(key, value)| (key.clone(), to_proto_value(value)))
            .collect(),
    })
}

fn to_proto_value(value: &Value) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(flag) => Kind::BoolValue(*flag),
        Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap_or_default()),
        Value::String(text) => Kind::StringValue(text.clone()),
        Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(to_proto_value).collect(),
        }),
        Value::Object(fields) => Kind::StructValue(prost_types::Struct {
            fields: fields
                .iter()
                .map(|(key, value)| (key.clone(), to_proto_value(value)))
                .collect(),
        }),
    };

    prost_types::Value { kind: Some(kind) }
}
