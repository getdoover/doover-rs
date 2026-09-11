//! Typed processor invocation events — pydoover
//! `pydoover/models/data/events.py`, decoded from the invocation's `op` +
//! `d` fields by the runner (`Application._dispatch_invocation`).
//!
//! Snowflake IDs coerce from JSON numbers *or* decimal strings (the cloud
//! stringifies 64-bit IDs for JS clients); nested payload data stays
//! [`Value`] — processors read what they need.

use serde_json::Value;

use crate::error::{DooverError, Result};
use crate::models::value_as_id;

/// `{"agent_id": …, "name": …}` — pydoover `ChannelID`.
#[derive(Debug, Clone)]
pub struct ChannelId {
    pub agent_id: u64,
    pub name: String,
}

impl ChannelId {
    fn from_value(data: &Value) -> Result<Self> {
        Ok(Self {
            agent_id: data.get("agent_id").and_then(value_as_id).unwrap_or_default(),
            name: data
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| DooverError::InvalidPayload("channel missing name".into()))?
                .to_string(),
        })
    }
}

/// A channel message riding in an event — pydoover `Message` (the subset
/// events carry; attachments stay in `raw`).
#[derive(Debug, Clone)]
pub struct EventMessage {
    pub id: u64,
    pub author_id: u64,
    pub channel: ChannelId,
    pub data: Value,
}

impl EventMessage {
    fn from_value(data: &Value) -> Result<Self> {
        Ok(Self {
            id: data.get("id").and_then(value_as_id).unwrap_or_default(),
            author_id: data.get("author_id").and_then(value_as_id).unwrap_or_default(),
            channel: ChannelId::from_value(
                data.get("channel")
                    .ok_or_else(|| DooverError::InvalidPayload("message missing channel".into()))?,
            )?,
            data: data.get("data").cloned().unwrap_or(Value::Null),
        })
    }
}

/// `op = "on_message_create"` — pydoover `MessageCreateEvent.from_dict`:
/// `d` is either `{"message": {…}}` or the message itself.
#[derive(Debug, Clone)]
pub struct MessageCreateEvent {
    pub channel: ChannelId,
    pub message: EventMessage,
}

impl MessageCreateEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        let message = EventMessage::from_value(d.get("message").unwrap_or(d))?;
        Ok(Self { channel: message.channel.clone(), message })
    }
}

/// `op = "on_aggregate_update"` — pydoover `AggregateUpdateEvent`.
/// `aggregate.data` is the full merged state; `request_data.data` is the
/// diff that triggered the event.
#[derive(Debug, Clone)]
pub struct AggregateUpdateEvent {
    pub author_id: u64,
    pub channel: ChannelId,
    /// Full merged aggregate data.
    pub aggregate_data: Value,
    /// The write that triggered this event.
    pub request_data: Value,
    pub organisation_id: Option<u64>,
}

impl AggregateUpdateEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        Ok(Self {
            author_id: d.get("author_id").and_then(value_as_id).unwrap_or_default(),
            channel: ChannelId::from_value(
                d.get("channel")
                    .ok_or_else(|| DooverError::InvalidPayload("event missing channel".into()))?,
            )?,
            aggregate_data: d
                .get("aggregate")
                .and_then(|a| a.get("data"))
                .cloned()
                .unwrap_or(Value::Null),
            request_data: d
                .get("request_data")
                .and_then(|a| a.get("data"))
                .cloned()
                .unwrap_or(Value::Null),
            organisation_id: d.get("organisation_id").and_then(value_as_id),
        })
    }
}

/// An alarm's state (pydoover `AlarmState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmState {
    NoData,
    Ok,
    Alarm,
    /// The condition is met but the debounce window (`alarm_pending_ms`) has
    /// not yet elapsed.
    AlarmPending,
}

impl AlarmState {
    pub fn wire(self) -> &'static str {
        match self {
            Self::NoData => "NoData",
            Self::Ok => "OK",
            Self::Alarm => "Alarm",
            Self::AlarmPending => "AlarmPending",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "NoData" => Some(Self::NoData),
            "OK" => Some(Self::Ok),
            "Alarm" => Some(Self::Alarm),
            "AlarmPending" => Some(Self::AlarmPending),
            _ => None,
        }
    }
}

/// The alarm an [`AlarmTriggerEvent`] fired for (pydoover `Alarm`).
///
/// Only the fields a processor reads are typed; the whole payload stays on
/// `raw` so a server-side addition doesn't need a port before it can be read.
#[derive(Debug, Clone)]
pub struct Alarm {
    pub id: u64,
    pub name: String,
    /// Dotted path into the channel aggregate that this alarm watches.
    pub key: String,
    pub state: AlarmState,
    pub channel_name: String,
    /// Present only on a rate-of-change alarm; its presence is what makes an
    /// alarm one (pydoover `Alarm.is_rate_alarm`).
    pub rate_window_ms: Option<i64>,
    pub raw: Value,
}

impl Alarm {
    pub fn from_value(d: &Value) -> Result<Self> {
        let state = d
            .get("state")
            .and_then(Value::as_str)
            .and_then(AlarmState::from_wire)
            .ok_or_else(|| DooverError::InvalidPayload("alarm has no known state".into()))?;
        Ok(Self {
            id: d.get("id").and_then(value_as_id).unwrap_or_default(),
            name: d
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            key: d
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            state,
            // Defaulted server-side for rows written before it existed, so a
            // payload in flight can legitimately omit it.
            channel_name: d
                .get("channel_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            rate_window_ms: d.get("rate_window_ms").and_then(Value::as_i64),
            raw: d.clone(),
        })
    }

    /// Whether this is a rate-of-change alarm rather than a threshold alarm.
    pub fn is_rate_alarm(&self) -> bool {
        self.rate_window_ms.is_some()
    }
}

/// `op = "on_alarm_trigger"` — an alarm on a subscribed channel changed state
/// (pydoover `AlarmTriggerEvent`).
///
/// Fired for every transition, including the ones whose user-facing
/// notification is suppressed by the alarm's `messages` overrides — the
/// processor fan-out is independent of notification delivery.
#[derive(Debug, Clone)]
pub struct AlarmTriggerEvent {
    pub channel: ChannelId,
    pub alarm: Alarm,
    pub old_state: AlarmState,
    pub new_state: AlarmState,
    /// Full merged aggregate data at trigger time.
    pub aggregate_data: Value,
    /// The write that triggered the evaluation.
    pub request_data: Value,
    pub organisation_id: Option<u64>,
}

impl AlarmTriggerEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        let state = |key: &str| -> Result<AlarmState> {
            d.get(key)
                .and_then(Value::as_str)
                .and_then(AlarmState::from_wire)
                .ok_or_else(|| DooverError::InvalidPayload(format!("event missing {key}")))
        };
        Ok(Self {
            channel: ChannelId::from_value(
                d.get("channel")
                    .ok_or_else(|| DooverError::InvalidPayload("event missing channel".into()))?,
            )?,
            alarm: Alarm::from_value(
                d.get("alarm")
                    .ok_or_else(|| DooverError::InvalidPayload("event missing alarm".into()))?,
            )?,
            old_state: state("old_state")?,
            new_state: state("new_state")?,
            aggregate_data: d
                .get("aggregate")
                .and_then(|a| a.get("data"))
                .cloned()
                .unwrap_or(Value::Null),
            request_data: d
                .get("request_data")
                .and_then(|a| a.get("data"))
                .cloned()
                .unwrap_or(Value::Null),
            organisation_id: d.get("organisation_id").and_then(value_as_id),
        })
    }

    /// Whether the alarm has just entered the (fully debounced) alarm state.
    pub fn is_alarm(&self) -> bool {
        self.new_state == AlarmState::Alarm
    }

    /// Whether the alarm has just recovered to OK.
    pub fn is_cleared(&self) -> bool {
        self.new_state == AlarmState::Ok
    }

    /// The value at the alarm's `key` in the aggregate at trigger time.
    pub fn value(&self) -> Option<&Value> {
        let mut current = &self.aggregate_data;
        for part in self.alarm.key.split('.') {
            current = current.as_object()?.get(part)?;
        }
        Some(current)
    }
}

/// `op = "on_deployment"` — the app was (re)deployed to an agent.
#[derive(Debug, Clone)]
pub struct DeploymentEvent {
    pub agent_id: u64,
    pub app_id: Value,
    pub app_install_id: Value,
    pub app_key: String,
    pub app_display_name: String,
}

impl DeploymentEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        Ok(Self {
            agent_id: d.get("agent_id").and_then(value_as_id).unwrap_or_default(),
            app_id: d.get("app_id").cloned().unwrap_or(Value::Null),
            app_install_id: d.get("app_install_id").cloned().unwrap_or(Value::Null),
            app_key: d
                .get("app_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            app_display_name: d
                .get("app_display_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}

/// `op = "on_schedule"` — an EventBridge schedule fired.
#[derive(Debug, Clone)]
pub struct ScheduleEvent {
    pub schedule_id: String,
}

impl ScheduleEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        Ok(Self { schedule_id: id_string(d.get("schedule_id")).unwrap_or_default() })
    }
}

/// `op = "on_ingestion_endpoint"` — an HTTP ingestion endpoint was hit.
/// `payload` is the raw (base64-wrapped) body from doover-data; decode it
/// via [`crate::processor::Processor::parse_ingestion_payload`] (default:
/// base64 → JSON).
#[derive(Debug, Clone)]
pub struct IngestionEndpointEvent {
    pub ingestion_id: String,
    pub agent_id: u64,
    pub organisation_id: Option<u64>,
    /// Raw payload as delivered (base64-encoded body bytes).
    pub payload: String,
    /// Decoded payload (see `parse_ingestion_payload`).
    pub data: Value,
    pub invocation_url: Option<String>,
    pub content_type: Option<String>,
}

impl IngestionEndpointEvent {
    pub fn from_value(d: &Value, parsed: Value) -> Result<Self> {
        Ok(Self {
            ingestion_id: id_string(d.get("ingestion_id")).unwrap_or_default(),
            agent_id: d.get("agent_id").and_then(value_as_id).unwrap_or_default(),
            organisation_id: d.get("organisation_id").and_then(value_as_id),
            payload: d
                .get("payload")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: parsed,
            invocation_url: d
                .get("invocation_url")
                .and_then(Value::as_str)
                .map(str::to_string),
            content_type: d.get("content_type").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// `op = "on_manual_invoke"` — invoked by hand from the Doover UI/CLI.
#[derive(Debug, Clone)]
pub struct ManualInvokeEvent {
    pub organisation_id: Option<u64>,
    pub payload: Value,
}

impl ManualInvokeEvent {
    pub fn from_value(d: &Value) -> Result<Self> {
        Ok(Self {
            organisation_id: d.get("organisation_id").and_then(value_as_id),
            payload: d.get("payload").cloned().unwrap_or(Value::Null),
        })
    }
}

/// The decoded, typed invocation payload — one variant per `op`. This is
/// what the filters (`pre_hook_filter` / `post_setup_filter`) see.
#[derive(Debug, Clone)]
pub enum EventPayload {
    MessageCreate(MessageCreateEvent),
    AggregateUpdate(AggregateUpdateEvent),
    AlarmTrigger(AlarmTriggerEvent),
    Deployment(DeploymentEvent),
    Schedule(ScheduleEvent),
    IngestionEndpoint(IngestionEndpointEvent),
    ManualInvoke(ManualInvokeEvent),
}

impl EventPayload {
    /// The channel that triggered this invocation, for message/aggregate
    /// events (drives the anti-recursion guard).
    pub fn invoking_channel(&self) -> Option<&str> {
        match self {
            Self::MessageCreate(e) => Some(e.channel.name.as_str()),
            Self::AggregateUpdate(e) => Some(e.channel.name.as_str()),
            // Writing back to the alarm's channel re-fires the aggregate
            // update that evaluated it, so guard the same way.
            Self::AlarmTrigger(e) => Some(e.channel.name.as_str()),
            _ => None,
        }
    }
}

/// Render an ID that may be a JSON string or number as a string.
pub(crate) fn id_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
