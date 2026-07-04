//! `DeviceAgentClient` — the Rust equivalent of pydoover's
//! `DeviceAgentInterface`. A thin, async wrapper over the generated gRPC stub
//! that speaks the lossless `data_json` encoding, maps response headers to
//! typed errors, and exposes the operations a device app needs.
//!
//! The underlying tonic `Channel` multiplexes many concurrent requests over
//! one HTTP/2 connection, so a single cloned `DeviceAgentClient` is all an app
//! (or a load generator) needs — unlike the pre-#108 pydoover client that
//! opened a channel per call.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tonic::transport::{Channel, Endpoint};

use doover_proto::device_agent as pb;
use pb::device_agent_client::DeviceAgentClient as GenClient;

use crate::error::{DooverError, Result};
use crate::events::Event;

const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct DeviceAgentClient {
    inner: GenClient<Channel>,
    app_id: Option<String>,
}

/// Options for an aggregate write (`update_channel_aggregate`).
#[derive(Debug, Clone, Default)]
pub struct AggregateOptions {
    /// Coalesce with other writes to this channel and flush at most this old
    /// (seconds). 0 = publish immediately.
    pub max_age_secs: f32,
    /// Log a historical datapoint in addition to updating current state.
    pub save_log: bool,
    /// Replace the whole aggregate rather than merge-patch it.
    pub replace_data: bool,
    pub clear_attachments: bool,
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
        Self { inner, app_id: None }
    }

    /// Set the `app_id` stamped into every request header (the app key).
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    fn header(&self) -> Option<pb::RequestHeader> {
        self.app_id.clone().map(|app_id| pb::RequestHeader { app_id: Some(app_id) })
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
        check_header(resp.response_header)?;
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
        match check_header(resp.response_header) {
            Ok(()) => Ok(resp.aggregate.map(|a| decode_aggregate(&a))),
            Err(DooverError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Append a message to a channel log; returns the minted message id.
    pub async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        validate_payload(data)?;
        let req = pb::CreateMessageRequest {
            header: self.header(),
            channel_name: channel.to_string(),
            data: None,
            data_json: serde_json::to_string(data)?,
            files: vec![],
            timestamp: 0,
        };
        let resp = self.inner.clone().create_message(req).await?.into_inner();
        check_header(resp.response_header)?;
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
        check_header(resp.response_header)?;
        Ok(())
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
        Ok(stream.map(move |item| {
            let resp = item?;
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
