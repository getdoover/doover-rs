//! Channel events delivered over `ChannelEventSubscription`.
//!
//! The agent sends the whole event payload as a JSON string in `data_json`
//! (we always subscribe with `WIRE_FORMAT_JSON_ONLY`, so the protobuf Struct
//! is never built — matching pydoover's default). Rather than model every
//! payload shape as a rigid struct (they drift), we keep the parsed JSON and
//! expose typed accessors for the fields apps actually read.

use serde_json::Value;

/// The `event_name` strings the agent emits.
pub mod names {
    pub const AGGREGATE_UPDATE: &str = "AggregateUpdate";
    pub const MESSAGE_CREATE: &str = "MessageCreate";
    pub const MESSAGE_UPDATE: &str = "MessageUpdate";
    pub const ONE_SHOT_MESSAGE: &str = "OneShotMessage";
    pub const CHANNEL_SYNC: &str = "ChannelSync";
}

#[derive(Debug, Clone)]
pub struct Event {
    pub event_name: String,
    pub channel: String,
    /// The full decoded payload (`data_json`).
    pub payload: Value,
}

impl Event {
    pub fn is_aggregate_update(&self) -> bool {
        self.event_name == names::AGGREGATE_UPDATE
    }
    pub fn is_message_create(&self) -> bool {
        self.event_name == names::MESSAGE_CREATE
    }
    pub fn is_one_shot(&self) -> bool {
        self.event_name == names::ONE_SHOT_MESSAGE
    }
    pub fn is_channel_sync(&self) -> bool {
        self.event_name == names::CHANNEL_SYNC
    }

    /// For AggregateUpdate / ChannelSync: the full merged aggregate data
    /// (`payload.aggregate.data`).
    pub fn aggregate_data(&self) -> Option<&Value> {
        self.payload.get("aggregate")?.get("data")
    }

    /// For AggregateUpdate: just the diff that triggered this event
    /// (`payload.request_data.data`).
    pub fn aggregate_diff(&self) -> Option<&Value> {
        self.payload.get("request_data")?.get("data")
    }

    /// For MessageCreate / MessageUpdate / OneShotMessage: the message body
    /// (`payload.data`).
    pub fn message_data(&self) -> Option<&Value> {
        self.payload.get("data")
    }

    /// The message / one-shot id, if present.
    pub fn message_id(&self) -> Option<u64> {
        as_u64(self.payload.get("id")?)
    }

    /// The author id (agent id or cloud-user id) if present.
    pub fn author_id(&self) -> Option<u64> {
        as_u64(self.payload.get("author_id")?)
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
