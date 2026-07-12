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

/// Which event kinds a subscriber wants delivered — pydoover's
/// `EventSubscription` flag, hand-rolled to avoid a bitflags dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSubscription(u8);

impl EventSubscription {
    pub const NONE: Self = Self(0);
    pub const MESSAGE_CREATE: Self = Self(1 << 0);
    pub const MESSAGE_UPDATE: Self = Self(1 << 1);
    pub const AGGREGATE_UPDATE: Self = Self(1 << 2);
    pub const ONESHOT_MESSAGE: Self = Self(1 << 3);
    pub const CHANNEL_SYNC: Self = Self(1 << 4);
    pub const ALL: Self = Self(0b1_1111);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for EventSubscription {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for EventSubscription {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl Default for EventSubscription {
    fn default() -> Self {
        Self::ALL
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub event_name: String,
    pub channel: String,
    /// The full decoded payload (`data_json`).
    pub payload: Value,
}

impl Event {
    /// Synthesize a `ChannelSync` event from the initial aggregate fetched on
    /// subscription, so subscribers get the channel's boot state through the
    /// same delivery path as live events (pydoover `ChannelSyncEvent`).
    pub fn channel_sync(channel: impl Into<String>, aggregate_data: Value) -> Self {
        let payload = serde_json::json!({ "aggregate": { "data": aggregate_data } });
        Self {
            event_name: names::CHANNEL_SYNC.to_string(),
            channel: channel.into(),
            payload,
        }
    }

    pub fn is_aggregate_update(&self) -> bool {
        self.event_name == names::AGGREGATE_UPDATE
    }
    pub fn is_message_create(&self) -> bool {
        self.event_name == names::MESSAGE_CREATE
    }
    pub fn is_message_update(&self) -> bool {
        self.event_name == names::MESSAGE_UPDATE
    }
    pub fn is_one_shot(&self) -> bool {
        self.event_name == names::ONE_SHOT_MESSAGE
    }
    pub fn is_channel_sync(&self) -> bool {
        self.event_name == names::CHANNEL_SYNC
    }

    /// The `EventSubscription` flag this event corresponds to, or `NONE` for
    /// an unrecognized event name (pydoover `_event_type_to_flag`).
    pub fn subscription_flag(&self) -> EventSubscription {
        match self.event_name.as_str() {
            names::MESSAGE_CREATE => EventSubscription::MESSAGE_CREATE,
            names::MESSAGE_UPDATE => EventSubscription::MESSAGE_UPDATE,
            names::AGGREGATE_UPDATE => EventSubscription::AGGREGATE_UPDATE,
            names::ONE_SHOT_MESSAGE => EventSubscription::ONESHOT_MESSAGE,
            names::CHANNEL_SYNC => EventSubscription::CHANNEL_SYNC,
            _ => EventSubscription::NONE,
        }
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
