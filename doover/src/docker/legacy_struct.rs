//! `google.protobuf.Struct` payloads, for device agents built before
//! 2026-06-10 that have no `data_json` field: they read and write only the
//! Struct. Struct numbers are doubles, so integers above 2^53 are lossy on
//! that path; current agents always send `data_json` and never use this.

use std::sync::atomic::{AtomicU8, Ordering};

use prost_types::value::Kind;
use serde_json::{Map, Number, Value};

// 0 = not yet known (the `Default`).
const JSON: u8 = 1;
const LEGACY: u8 = 2;

/// Which payload encoding the connected agent speaks, learned from its
/// responses. Until it is known, writes carry both encodings (a current
/// agent reads `data_json` and ignores the Struct).
#[derive(Debug, Default)]
pub(crate) struct WireMode(AtomicU8);

impl WireMode {
    /// Whether writes should also carry the Struct.
    pub(crate) fn send_struct(&self) -> bool {
        self.0.load(Ordering::Relaxed) != JSON
    }

    /// Record a payload the agent sent back. A current agent always fills
    /// `data_json`; an old one leaves it empty and fills the Struct. Empty
    /// both ways (error frames) says nothing.
    pub(crate) fn observe(&self, data_json: &str, data: Option<&prost_types::Struct>) {
        let mode = if !data_json.is_empty() {
            JSON
        } else if data.is_some_and(|s| !s.fields.is_empty()) {
            LEGACY
        } else {
            return;
        };
        if self.0.swap(mode, Ordering::Relaxed) != mode && mode == LEGACY {
            tracing::warn!(
                "device agent predates data_json (built before 2026-06-10); using \
                 protobuf Struct payloads, integers above 2^53 may be lossy"
            );
        }
    }
}

/// Decode a dual-format payload: `data_json` when present, else the Struct.
pub(crate) fn decode(data_json: &str, data: Option<&prost_types::Struct>) -> Option<Value> {
    if !data_json.is_empty() {
        return serde_json::from_str(data_json).ok();
    }
    data.map(struct_to_value)
}

pub(crate) fn value_to_struct(v: &Value) -> prost_types::Struct {
    let fields = match v {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), value_to_pb(v))).collect(),
        _ => Default::default(),
    };
    prost_types::Struct { fields }
}

fn value_to_pb(v: &Value) -> prost_types::Value {
    let kind = match v {
        Value::Null => Kind::NullValue(0),
        Value::Bool(b) => Kind::BoolValue(*b),
        Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => Kind::StringValue(s.clone()),
        Value::Array(a) => {
            Kind::ListValue(prost_types::ListValue { values: a.iter().map(value_to_pb).collect() })
        }
        Value::Object(_) => Kind::StructValue(value_to_struct(v)),
    };
    prost_types::Value { kind: Some(kind) }
}

pub(crate) fn struct_to_value(s: &prost_types::Struct) -> Value {
    Value::Object(s.fields.iter().map(|(k, v)| (k.clone(), pb_to_value(v))).collect::<Map<_, _>>())
}

fn pb_to_value(v: &prost_types::Value) -> Value {
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::NumberValue(f)) => number(*f),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(pb_to_value).collect()),
        Some(Kind::StructValue(s)) => struct_to_value(s),
    }
}

/// Whole doubles become integers, so typed config (`u32`, `i64`, …) still
/// deserialises from what was an integer before the Struct turned it into
/// a double.
fn number(f: f64) -> Value {
    const SAFE: f64 = 9_007_199_254_740_992.0; // 2^53
    if f.fract() == 0.0 && f.abs() <= SAFE {
        return Value::Number(Number::from(f as i64));
    }
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_and_restores_integers() {
        let v = json!({
            "applications": {"app_1": {"AGENT_ID": "175046099306866951", "modbus_id": 11,
                "serial_timeout": 0.3, "neg": -4, "list": [1, "a", null, true], "empty": {}}}
        });
        assert_eq!(struct_to_value(&value_to_struct(&v)), v);
    }

    #[test]
    fn decode_prefers_json() {
        let s = value_to_struct(&json!({"a": 1}));
        assert_eq!(decode(r#"{"b":2}"#, Some(&s)), Some(json!({"b": 2})));
        assert_eq!(decode("", Some(&s)), Some(json!({"a": 1})));
        assert_eq!(decode("", None), None);
    }

    #[test]
    fn wire_mode_detection() {
        let m = WireMode::default();
        assert!(m.send_struct(), "unknown agent gets both encodings");
        m.observe("", Some(&prost_types::Struct::default()));
        assert!(m.send_struct(), "an empty error frame decides nothing");
        m.observe("{}", None);
        assert!(!m.send_struct(), "current agent: data_json only");
        m.observe("", Some(&value_to_struct(&json!({"a": 1}))));
        assert!(m.send_struct(), "old agent: Struct too");
        m.observe("{}", None);
        assert!(!m.send_struct(), "agent upgraded underneath us");
    }
}
