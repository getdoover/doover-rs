//! `DeviceAgentClient` — the Rust equivalent of pydoover's
//! `DeviceAgentInterface`. A thin, async wrapper over the generated gRPC stub
//! that speaks the lossless `data_json` encoding, maps response headers to
//! typed errors, and exposes the operations a device app needs.
//!
//! The underlying tonic `Channel` multiplexes many concurrent requests over
//! one HTTP/2 connection, so a single cloned `DeviceAgentClient` is all an app
//! (or a load generator) needs — unlike the pre-#108 pydoover client that
//! opened a channel per call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tonic::transport::{Channel, Endpoint};

use doover_proto::device_agent as pb;
use pb::device_agent_client::DeviceAgentClient as GenClient;

use crate::error::{DooverError, Result};
use crate::events::Event;

// Re-exported from their new home so existing `doover::docker::device_agent`
// (and `doover::AggregateOptions`) paths keep working.
pub use crate::channel_backend::{AggregateOptions, UpdateMessageOptions};

const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Connection-state flags derived from every `ResponseHeader` the agent
/// returns (pydoover `update_dda_status`): `available` tracks whether the
/// last request succeeded, `online` tracks the agent's `cloud_synced` flag.
#[derive(Debug, Default)]
pub struct DdaStatus {
    available: AtomicBool,
    online: AtomicBool,
    been_online: AtomicBool,
}

impl DdaStatus {
    /// Whether the last request to the agent succeeded
    /// (pydoover `is_dda_available`).
    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    /// Whether the agent currently reports being synced with the cloud
    /// (pydoover `is_dda_online`).
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Whether the agent has reported cloud sync at least once since this
    /// client was created (pydoover `has_dda_been_online`).
    pub fn has_been_online(&self) -> bool {
        self.been_online.load(Ordering::Relaxed)
    }

    fn update(&self, header: &pb::ResponseHeader) {
        self.available.store(header.success, Ordering::Relaxed);
        if header.cloud_synced {
            if !self.been_online.swap(true, Ordering::Relaxed) {
                tracing::info!("device agent is online");
            }
            self.online.store(true, Ordering::Relaxed);
        } else {
            self.online.store(false, Ordering::Relaxed);
        }
    }
}

#[derive(Clone)]
pub struct DeviceAgentClient {
    inner: GenClient<Channel>,
    app_id: Option<String>,
    status: Arc<DdaStatus>,
}

/// A persisted channel message (pydoover `Message`, decoded from
/// `data_json`).
#[derive(Debug, Clone)]
pub struct Message {
    pub message_id: u64,
    pub author_id: u64,
    pub channel_name: String,
    pub data: Value,
}

impl Message {
    fn from_proto(m: pb::Message) -> Self {
        let data = if m.data_json.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&m.data_json).unwrap_or(Value::Null)
        };
        Self {
            message_id: m.message_id,
            author_id: m.author_id,
            channel_name: m.channel.map(|c| c.name).unwrap_or_default(),
            data,
        }
    }
}

/// WebRTC TURN credentials for camera streaming (pydoover `TurnCredential`).
#[derive(Debug, Clone)]
pub struct TurnCredential {
    pub username: String,
    pub credential: String,
    pub ttl: u64,
    pub expires_at: u64,
    pub uris: Vec<String>,
}

/// Query bounds for `list_messages` (pydoover accepts ints or datetimes;
/// convert a timestamp with `utils::generate_snowflake_id_at`).
#[derive(Debug, Clone, Default)]
pub struct ListMessagesOptions {
    /// Only messages with an id below this snowflake.
    pub before: Option<u64>,
    /// Only messages with an id above this snowflake.
    pub after: Option<u64>,
    pub limit: Option<u32>,
    /// Restrict the returned payloads to these top-level fields.
    pub field_names: Vec<String>,
}

impl DeviceAgentClient {
    /// Connect to the local device agent (default `http://127.0.0.1:50051`).
    pub async fn connect(uri: impl Into<String>) -> Result<Self> {
        let endpoint = Endpoint::from_shared(uri.into())
            .map_err(|e| DooverError::Other(format!("bad dda uri: {e}")))?
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(10));
        let channel = endpoint.connect().await?;
        Ok(Self::with_channel(channel))
    }

    pub fn with_channel(channel: Channel) -> Self {
        let inner = GenClient::new(channel)
            .max_decoding_message_size(DEFAULT_MAX_MESSAGE_SIZE)
            .max_encoding_message_size(DEFAULT_MAX_MESSAGE_SIZE);
        Self { inner, app_id: None, status: Arc::new(DdaStatus::default()) }
    }

    /// Set the `app_id` stamped into every request header (the app key).
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    /// Connection-state flags, updated on every response from the agent.
    /// Shared across clones of this client.
    pub fn status(&self) -> &Arc<DdaStatus> {
        &self.status
    }

    fn header(&self) -> Option<pb::RequestHeader> {
        self.app_id.clone().map(|app_id| pb::RequestHeader { app_id: Some(app_id) })
    }

    /// Update the DDA status flags then map the header to a typed error
    /// (pydoover `process_response` + `update_dda_status`).
    fn check_header(&self, header: Option<pb::ResponseHeader>) -> Result<()> {
        if let Some(h) = &header {
            self.status.update(h);
        }
        check_header(header)
    }

    /// Liveness echo (`TestComms`).
    pub async fn test_comms(&self, message: impl Into<String>) -> Result<String> {
        let resp = self
            .inner
            .clone()
            .test_comms(pb::TestCommsRequest { header: self.header(), message: message.into() })
            .await?
            .into_inner();
        Ok(resp.response)
    }

    /// `update_channel_aggregate` — the core state write. Merges `data` into
    /// the channel aggregate (unless `replace_data`) and routes it to the
    /// cloud per the max-age / save_log rules.
    pub async fn update_channel_aggregate(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()> {
        validate_payload(data)?;
        if !opts.replace_keys.is_empty() {
            // Divergence from the HTTP backend: the device-agent proto has no
            // replace-keys field, so the write degrades to a plain merge.
            tracing::warn!(
                "AggregateOptions.replace_keys is not supported by the device agent; \
                 merging normally on '{channel}'"
            );
        }
        let req = pb::UpdateAggregateRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            data: None,
            data_json: serde_json::to_string(data)?,
            files: vec![],
            clear_attachments: Some(opts.clear_attachments),
            replace_data: Some(opts.replace_data),
            max_age_secs: opts.max_age_secs,
            save_log: opts.save_log,
            // most callers discard the echo — skip the encode.
            return_aggregate: Some(false),
        };
        let resp = self.inner.clone().update_aggregate(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        Ok(())
    }

    /// Fetch the current aggregate data for a channel, or `None` if it does
    /// not exist.
    pub async fn fetch_channel_aggregate(&self, channel: &str) -> Result<Option<Value>> {
        let req = pb::GetAggregateRequest {
            header: self.header(),
            channel_name: channel.to_string(),
        };
        let resp = self.inner.clone().get_aggregate(req).await?.into_inner();
        match self.check_header(resp.response_header) {
            Ok(()) => Ok(resp.aggregate.map(|a| decode_aggregate(&a))),
            Err(DooverError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Append a message to a channel log stamped now; returns the minted
    /// message id (pydoover stamps `datetime.now()` when no timestamp is
    /// given).
    pub async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.create_message_at(channel, data, now_ms).await
    }

    /// Append a message stamped with an explicit unix-millisecond timestamp
    /// (used for backdated log points).
    pub async fn create_message_at(
        &self,
        channel: &str,
        data: &Value,
        timestamp_ms: u64,
    ) -> Result<u64> {
        validate_payload(data)?;
        let req = pb::CreateMessageRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            data: None,
            data_json: serde_json::to_string(data)?,
            files: vec![],
            timestamp: timestamp_ms,
        };
        let resp = self.inner.clone().create_message(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        Ok(resp.message_id)
    }

    /// Send an ephemeral one-shot message (WSS-only; requires cloud).
    pub async fn send_one_shot_message(&self, channel: &str, data: &Value) -> Result<()> {
        validate_payload(data)?;
        let req = pb::SendOneShotMessageRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            data: None,
            data_json: serde_json::to_string(data)?,
            timestamp: None,
        };
        let resp = self.inner.clone().send_one_shot_message(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        Ok(())
    }

    /// Fetch a single message by id (pydoover `fetch_message`).
    pub async fn fetch_message(&self, channel: &str, message_id: u64) -> Result<Message> {
        let req = pb::GetMessageRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            message_id,
        };
        let resp = self.inner.clone().get_message(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        resp.message
            .map(Message::from_proto)
            .ok_or_else(|| DooverError::NotFound(format!("message {message_id} on '{channel}'")))
    }

    /// List messages on a channel, bounded by snowflake ids
    /// (pydoover `list_messages`).
    pub async fn list_messages(
        &self,
        channel: &str,
        opts: &ListMessagesOptions,
    ) -> Result<Vec<Message>> {
        let req = pb::GetMessagesRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            before: opts.before,
            after: opts.after,
            limit: opts.limit,
            field_names: opts.field_names.clone(),
        };
        let resp = self.inner.clone().get_messages(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        Ok(resp.messages.into_iter().map(Message::from_proto).collect())
    }

    /// Update an existing message's payload (pydoover `update_message`).
    pub async fn update_message(
        &self,
        channel: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
    ) -> Result<Message> {
        validate_payload(data)?;
        let req = pb::UpdateMessageRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            message_id: message_id.to_string(),
            data: None,
            data_json: serde_json::to_string(data)?,
            files: vec![],
            clear_attachments: Some(opts.clear_attachments),
            replace_data: Some(opts.replace_data),
        };
        let resp = self.inner.clone().update_message(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        resp.message
            .map(Message::from_proto)
            .ok_or_else(|| DooverError::Other("update_message returned no message".into()))
    }

    /// Download a message attachment (pydoover `fetch_message_attachment`).
    /// Takes and returns raw proto types (`doover::proto`), as attachments
    /// are an advanced use case.
    pub async fn fetch_message_attachment(&self, attachment: pb::Attachment) -> Result<pb::File> {
        let req = pb::FetchAttachmentRequest {
            header: self.header(),
            attachment: Some(attachment),
        };
        let resp = self.inner.clone().fetch_attachment(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        resp.file.ok_or_else(|| DooverError::NotFound("attachment file".into()))
    }

    /// Fetch WebRTC TURN credentials (pydoover `fetch_turn_token`).
    pub async fn fetch_turn_token(&self, camera_name: &str) -> Result<TurnCredential> {
        let req = pb::TurnCredentialRequest {
            header: self.header(),
            camera_name: camera_name.to_string(),
        };
        let resp = self.inner.clone().get_turn_credential(req).await?.into_inner();
        self.check_header(resp.response_header)?;
        let c = resp
            .turn_credential
            .ok_or_else(|| DooverError::NotFound("turn credential".into()))?;
        Ok(TurnCredential {
            username: c.username,
            credential: c.credential,
            ttl: c.ttl,
            expires_at: c.expires_at,
            uris: c.uris,
        })
    }

    /// Subscribe to a channel's events. The returned stream yields typed
    /// `Event`s until the agent ends the stream (graceful shutdown, or the
    /// per-subscriber queue overflowing) or the connection drops; callers
    /// should reconnect, exactly as pydoover does.
    pub async fn subscribe_events(
        &self,
        channel: &str,
    ) -> Result<impl Stream<Item = Result<Event>>> {
        let req = pb::ChannelEventSubscriptionRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            wire_format: pb::WireFormat::JsonOnly as i32,
        };
        let stream = self.inner.clone().channel_event_subscription(req).await?.into_inner();
        let channel = channel.to_string();
        let status = self.status.clone();
        Ok(stream.map(move |item| {
            let resp = item?;
            if let Some(h) = &resp.response_header {
                status.update(h);
                if !h.success {
                    // pydoover raises here, tearing the stream down so the
                    // caller reconnects.
                    return Err(DooverError::Other(format!(
                        "subscription to '{channel}' rejected: {}",
                        h.response_message.clone().unwrap_or_default()
                    )));
                }
            }
            let payload: Value = if resp.data_json.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&resp.data_json)?
            };
            Ok(Event {
                event_name: resp.event_name,
                channel: channel.clone(),
                payload,
            })
        }))
    }
}

/// The gRPC device agent is a [`ChannelBackend`](crate::ChannelBackend) with
/// a persistent (streaming) connection.
#[async_trait::async_trait]
impl crate::channel_backend::ChannelBackend for DeviceAgentClient {
    async fn fetch_channel_aggregate(&self, channel: &str) -> Result<Option<Value>> {
        DeviceAgentClient::fetch_channel_aggregate(self, channel).await
    }

    async fn update_channel_aggregate(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()> {
        DeviceAgentClient::update_channel_aggregate(self, channel, data, opts).await
    }

    async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        DeviceAgentClient::create_message(self, channel, data).await
    }

    async fn update_message(
        &self,
        channel: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
    ) -> Result<()> {
        DeviceAgentClient::update_message(self, channel, message_id, data, opts).await?;
        Ok(())
    }

    fn has_persistent_connection(&self) -> bool {
        true
    }
}

/// Map a `ResponseHeader` to a typed error (pydoover `process_response`).
fn check_header(header: Option<pb::ResponseHeader>) -> Result<()> {
    let Some(h) = header else { return Ok(()) };
    if h.success {
        return Ok(());
    }
    let code = h.response_code.unwrap_or(500);
    let message = h.response_message.unwrap_or_default();
    if code == 404 {
        Err(DooverError::NotFound(message))
    } else {
        Err(DooverError::Http { code, message })
    }
}

fn decode_aggregate(a: &pb::Aggregate) -> Value {
    if !a.data_json.is_empty() {
        serde_json::from_str(&a.data_json).unwrap_or(Value::Null)
    } else {
        // No lossless field: the caller is on an old agent. We do not decode
        // the lossy Struct here (ints >2^53 are already corrupted); return null.
        Value::Null
    }
}

/// pydoover `validate_payload`: root must be an object; keys match
/// `^[a-zA-Z0-9_-]+$`; values are only object/array/string/number/bool/null.
pub fn validate_payload(data: &Value) -> Result<()> {
    let Value::Object(map) = data else {
        return Err(DooverError::InvalidPayload("payload root must be an object".into()));
    };
    for (k, v) in map {
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(DooverError::InvalidPayload(format!("invalid key: {k:?}")));
        }
        validate_value(v)?;
    }
    Ok(())
}

fn validate_value(v: &Value) -> Result<()> {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
        Value::Array(items) => items.iter().try_for_each(validate_value),
        Value::Object(map) => {
            for (k, v) in map {
                if k.is_empty()
                    || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(DooverError::InvalidPayload(format!("invalid nested key: {k:?}")));
                }
                validate_value(v)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_ok() {
        assert!(validate_payload(&json!({"a_b-1": 1, "n": {"x": [1, "s", true, null]}})).is_ok());
    }

    #[test]
    fn validate_rejects_non_object_root() {
        assert!(validate_payload(&json!([1, 2, 3])).is_err());
        assert!(validate_payload(&json!(5)).is_err());
    }

    #[test]
    fn validate_rejects_bad_keys() {
        assert!(validate_payload(&json!({"bad key": 1})).is_err());
        assert!(validate_payload(&json!({"dots.bad": 1})).is_err());
    }
}
