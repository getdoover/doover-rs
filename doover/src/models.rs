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
///
/// The discriminants are the integers the database stores. The server has a
/// hand-written deserialiser for severity that accepts *either* the integer or
/// the variant name, so [`Notification::to_json`] keeps sending the historical
/// integer; [`wire`](Self::wire) gives the name for the paths that need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationSeverity {
    Trace = 3,
    Debug = 4,
    Info = 5,
    Warn = 6,
    Critical = 7,
}

impl NotificationSeverity {
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Critical,
    ];

    /// The variant name — how the API represents this member in JSON.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Trace => "Trace",
            Self::Debug => "Debug",
            Self::Info => "Info",
            Self::Warn => "Warn",
            Self::Critical => "Critical",
        }
    }

    /// The stored integer discriminant.
    pub fn value(self) -> i64 {
        self as i64
    }

    /// Parse from the integer discriminant.
    pub fn from_value(value: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.value() == value)
    }
}

/// Parse a severity from its name, case-insensitively, plus the misspellings
/// people reach for — pydoover `_NameWireEnum._missing_` and
/// `NotificationSeverity._aliases`.
///
/// `Warn` and `Critical` are the two guessed wrong most often, since Python's
/// logging module spells them `warning`/`critical` and most people write
/// `error`. The server matches names exactly and rejects all three — and on the
/// notifications channel that rejection is *silent*, the payload being replaced
/// by its own raw JSON, which surfaces as an unreadable notification on a
/// subscriber's phone. Resolving it here is what stops that.
impl std::str::FromStr for NotificationSeverity {
    type Err = crate::error::DooverError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        if let Some(found) = Self::ALL
            .into_iter()
            .find(|m| m.wire().to_lowercase() == key)
        {
            return Ok(found);
        }
        match key.as_str() {
            "warning" => Ok(Self::Warn),
            "error" | "err" | "fatal" | "crit" => Ok(Self::Critical),
            _ => Err(crate::error::DooverError::InvalidPayload(format!(
                "{s:?} is not a valid NotificationSeverity — expected one of {}",
                Self::ALL.map(Self::wire).join(", ")
            ))),
        }
    }
}

/// The transport a notification endpoint delivers over (pydoover
/// `NotificationType`).
///
/// Unlike [`NotificationSeverity`], the server has *no* integer deserialiser
/// for this one — anything sent as a type must be the variant name, which is
/// what [`wire`](Self::wire) returns. The discriminants are the database's
/// internal storage only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationType {
    Email = 1,
    Sms = 2,
    WebPush = 3,
    Http = 4,
    Placeholder = 5,
    FirebasePush = 6,
}

impl NotificationType {
    pub const ALL: [Self; 6] = [
        Self::Email,
        Self::Sms,
        Self::WebPush,
        Self::Http,
        Self::Placeholder,
        Self::FirebasePush,
    ];

    /// The value to put on the wire for this member — the variant name.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Email => "Email",
            Self::Sms => "Sms",
            Self::WebPush => "WebPush",
            Self::Http => "Http",
            Self::Placeholder => "Placeholder",
            Self::FirebasePush => "FirebasePush",
        }
    }

    /// The stored integer discriminant.
    pub fn value(self) -> i64 {
        self as i64
    }

    pub fn from_value(value: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.value() == value)
    }
}

impl std::str::FromStr for NotificationType {
    type Err = crate::error::DooverError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|m| m.wire().to_lowercase() == key)
            .ok_or_else(|| {
                crate::error::DooverError::InvalidPayload(format!(
                    "{s:?} is not a valid NotificationType — expected one of {}",
                    Self::ALL.map(Self::wire).join(", ")
                ))
            })
    }
}

/// A notification message sent via the `notifications` channel — mirrors the
/// server-side `NotificationChannelMessagePayload`. Publishing a message with
/// this payload causes the Doover cloud to fan the notification out to
/// matching subscriptions (email / SMS / web push / http).
///
/// Rust's types cover most of what pydoover has to check at runtime; what's
/// left is [`validate`](Self::validate), which the send paths call. A payload
/// the server cannot deserialise is *not* rejected — it is quietly replaced by
/// one whose message is the raw JSON, surfacing as an unreadable notification on
/// a subscriber's phone long after the fact. Failing before the write keeps the
/// mistake in the application, where it is visible.
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
        Self {
            message: message.into(),
            title: None,
            severity: None,
            topic: None,
        }
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

    /// Reject a payload the server would silently mangle — pydoover validates
    /// the same things in `Notification.__init__`. Called by the send paths
    /// ([`AppContext::send_notification`](crate::AppContext::send_notification)
    /// and the processor's), so applications rarely call it directly.
    ///
    /// The only runtime check Rust's types don't already make is an empty
    /// message; severity is an enum here, and the string parse
    /// ([`NotificationSeverity::from_str`](std::str::FromStr)) has already
    /// failed loudly by this point.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.message.trim().is_empty() {
            return Err(crate::error::DooverError::InvalidPayload(
                "notification message must not be empty".into(),
            ));
        }
        Ok(())
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

    #[test]
    fn empty_message_is_rejected() {
        assert!(Notification::new("hi").validate().is_ok());
        assert!(Notification::new("").validate().is_err());
        assert!(Notification::new("   ").validate().is_err());
    }

    #[test]
    fn severity_parses_names_and_common_misspellings() {
        use std::str::FromStr;
        // Names, case-insensitively.
        assert_eq!(
            NotificationSeverity::from_str("Warn").unwrap(),
            NotificationSeverity::Warn
        );
        assert_eq!(
            NotificationSeverity::from_str(" info ").unwrap(),
            NotificationSeverity::Info
        );
        // The aliases people actually reach for, which the server rejects.
        for alias in ["warning", "WARNING"] {
            assert_eq!(
                NotificationSeverity::from_str(alias).unwrap(),
                NotificationSeverity::Warn
            );
        }
        for alias in ["error", "err", "fatal", "crit", "critical"] {
            assert_eq!(
                NotificationSeverity::from_str(alias).unwrap(),
                NotificationSeverity::Critical,
                "{alias}"
            );
        }
        assert!(NotificationSeverity::from_str("chatty").is_err());
        // Severity goes on the wire as the historical integer, but `wire()`
        // gives the name for the endpoints that need it.
        assert_eq!(NotificationSeverity::Warn.value(), 6);
        assert_eq!(NotificationSeverity::Warn.wire(), "Warn");
        assert_eq!(
            NotificationSeverity::from_value(6),
            Some(NotificationSeverity::Warn)
        );
    }

    #[test]
    fn notification_type_is_name_on_the_wire() {
        use std::str::FromStr;
        // The server has no integer deserialiser for type — names only.
        assert_eq!(NotificationType::WebPush.wire(), "WebPush");
        assert_eq!(NotificationType::FirebasePush.wire(), "FirebasePush");
        assert_eq!(NotificationType::FirebasePush.value(), 6);
        assert_eq!(
            NotificationType::from_str("email").unwrap(),
            NotificationType::Email
        );
        assert_eq!(
            NotificationType::from_str("firebasepush").unwrap(),
            NotificationType::FirebasePush
        );
        assert!(NotificationType::from_str("carrier-pigeon").is_err());
    }
}
