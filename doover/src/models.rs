//! Data models shared across transports — the [`Notification`] payload
//! (pydoover `pydoover/models/data/notification.py`) plus, with the
//! `cloud-api` feature, the processor/data-API models
//! ([`SubscriptionInfo`], connection enums — pydoover
//! `pydoover/models/data/processor_info.py` / `connection.py`).

use serde_json::{Map, Value};

/// The channel notifications are published on.
pub const NOTIFICATIONS_CHANNEL: &str = "notifications";

/// Notification severity (pydoover `NotificationSeverity`). Subscribers only
/// receive notifications at or above their subscription severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationSeverity {
    Trace = 3,
    Debug = 4,
    Info = 5,
    Warn = 6,
    Critical = 7,
}

/// A notification message sent via the `notifications` channel — mirrors the
/// server-side `NotificationChannelMessagePayload`. Publishing a message with
/// this payload causes the Doover cloud to fan the notification out to
/// matching subscriptions (email / SMS / web push / http).
#[derive(Debug, Clone)]
pub struct Notification {
    /// The notification body. Required.
    pub message: String,
    /// Optional title / headline.
    pub title: Option<String>,
    pub severity: Option<NotificationSeverity>,
    /// Optional topic string matched against subscription `topic_filter`s.
    pub topic: Option<String>,
}

impl Notification {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), title: None, severity: None, topic: None }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn severity(mut self, severity: NotificationSeverity) -> Self {
        self.severity = Some(severity);
        self
    }

    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    /// pydoover `Notification.to_dict()`: `message[, title][, severity][, topic]`
    /// — set keys only, severity as its integer value.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("message".into(), Value::String(self.message.clone()));
        if let Some(title) = &self.title {
            m.insert("title".into(), Value::String(title.clone()));
        }
        if let Some(severity) = self.severity {
            m.insert("severity".into(), Value::from(severity as i64));
        }
        if let Some(topic) = &self.topic {
            m.insert("topic".into(), Value::String(topic.clone()));
        }
        Value::Object(m)
    }
}

impl From<&str> for Notification {
    fn from(message: &str) -> Self {
        Notification::new(message)
    }
}

impl From<String> for Notification {
    fn from(message: String) -> Self {
        Notification::new(message)
    }
}

/// Coerce a Doover snowflake ID that may arrive as a JSON number *or* a
/// decimal string (the cloud stringifies 64-bit IDs to survive JS clients).
#[cfg(feature = "cloud-api")]
pub(crate) fn value_as_id(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The token-upgrade payload returned by the processor info endpoints
/// (`GET /processors/subscriptions/{id}` / `GET /processors/schedules/{id}`)
/// or embedded in the event as `d.upgrade` — pydoover `SubscriptionInfo`.
///
/// Carries the full bearer token plus the seeded channels the processor
/// almost always needs (`ui_state`, `ui_cmds`, `tag_values`,
/// `deployment_config`), saving one round-trip each.
#[cfg(feature = "cloud-api")]
#[derive(Debug, Clone)]
pub struct SubscriptionInfo {
    pub agent_id: u64,
    pub organisation_id: Option<u64>,
    pub app_key: String,
    pub deployment_config: Value,
    pub ui_state: Value,
    pub ui_cmds: Value,
    pub tag_values: Value,
    /// `{"config": {...}, "status": {...}}` — absent for org processors and
    /// freshly-created devices.
    pub connection_data: Value,
    pub token: String,
}

#[cfg(feature = "cloud-api")]
impl SubscriptionInfo {
    /// pydoover `SubscriptionInfo.from_dict` — `agent_id`/`organisation_id`
    /// coerce from number or string; the channel seeds default to null.
    pub fn from_value(data: &Value) -> crate::error::Result<Self> {
        let get = |k: &str| data.get(k).cloned().unwrap_or(Value::Null);
        let agent_id = data.get("agent_id").and_then(value_as_id).ok_or_else(|| {
            crate::error::DooverError::InvalidPayload("SubscriptionInfo missing agent_id".into())
        })?;
        let app_key = data
            .get("app_key")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                crate::error::DooverError::InvalidPayload("SubscriptionInfo missing app_key".into())
            })?
            .to_string();
        let token = data
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                crate::error::DooverError::InvalidPayload("SubscriptionInfo missing token".into())
            })?
            .to_string();
        Ok(Self {
            agent_id,
            organisation_id: data.get("organisation_id").and_then(value_as_id),
            app_key,
            deployment_config: get("deployment_config"),
            ui_state: get("ui_state"),
            ui_cmds: get("ui_cmds"),
            tag_values: get("tag_values"),
            connection_data: get("connection_data"),
            token,
        })
    }
}

/// pydoover `ConnectionDetermination` — the server-facing verdict attached
/// to a connection ping.
#[cfg(feature = "cloud-api")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionDetermination {
    Online,
    Offline,
}

#[cfg(feature = "cloud-api")]
impl ConnectionDetermination {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Offline => "Offline",
        }
    }
}

/// pydoover `ConnectionStatus` — how the agent's link is currently classed.
#[cfg(feature = "cloud-api")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    ContinuousOnline,
    ContinuousOnlineNoPing,
    ContinuousOffline,
    ContinuousPending,
    /// The default for processor pings (pydoover `periodic_unknown`).
    #[default]
    PeriodicUnknown,
    Unknown,
}

#[cfg(feature = "cloud-api")]
impl ConnectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContinuousOnline => "ContinuousOnline",
            Self::ContinuousOnlineNoPing => "ContinuousOnlineNoPing",
            Self::ContinuousOffline => "ContinuousOffline",
            Self::ContinuousPending => "ContinuousPending",
            Self::PeriodicUnknown => "PeriodicUnknown",
            Self::Unknown => "Unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shape_matches_pydoover() {
        let n = Notification::new("tank low")
            .title("Level alert")
            .severity(NotificationSeverity::Warn)
            .topic("levels");
        assert_eq!(
            serde_json::to_string(&n.to_json()).unwrap(),
            r#"{"message":"tank low","title":"Level alert","severity":6,"topic":"levels"}"#
        );
        // bare message: only the required key
        assert_eq!(
            serde_json::to_string(&Notification::new("hi").to_json()).unwrap(),
            r#"{"message":"hi"}"#
        );
    }
}
