//! Test doubles (feature `testing`) — currently [`MockBackend`], an
//! in-memory [`ChannelBackend`] that records every call, in the style of the
//! `FakeAgentState` recorder used by the gRPC integration tests (and
//! pydoover's `FakeTagClient` in `tests/test_tags.py`).
//!
//! Use it to unit-test anything written against the trait (processor tags,
//! RPC/UI managers) without a network or a fake server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::channel_backend::{AggregateOptions, ChannelBackend, UpdateMessageOptions};
use crate::error::Result;

/// One recorded `update_channel_aggregate` call.
#[derive(Debug, Clone)]
pub struct RecordedAggregateWrite {
    pub channel: String,
    pub data: Value,
    pub opts: AggregateOptions,
}

/// One recorded `create_message` call.
#[derive(Debug, Clone)]
pub struct RecordedMessage {
    pub channel: String,
    pub data: Value,
}

/// One recorded `update_message` call.
#[derive(Debug, Clone)]
pub struct RecordedMessageUpdate {
    pub channel: String,
    pub message_id: u64,
    pub data: Value,
    pub opts: UpdateMessageOptions,
}

/// In-memory channel state + call recorder.
#[derive(Default)]
pub struct MockBackend {
    /// Current aggregates by channel (merge-patched on update).
    pub aggregates: Mutex<HashMap<String, Value>>,
    pub aggregate_writes: Mutex<Vec<RecordedAggregateWrite>>,
    pub messages: Mutex<Vec<RecordedMessage>>,
    pub message_updates: Mutex<Vec<RecordedMessageUpdate>>,
    next_message_id: AtomicU64,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed a channel aggregate.
    pub fn seed_aggregate(&self, channel: &str, data: Value) {
        self.aggregates.lock().unwrap().insert(channel.to_string(), data);
    }

    /// Snapshot of the recorded aggregate writes.
    pub fn aggregate_writes(&self) -> Vec<RecordedAggregateWrite> {
        self.aggregate_writes.lock().unwrap().clone()
    }

    /// Snapshot of the recorded messages.
    pub fn messages(&self) -> Vec<RecordedMessage> {
        self.messages.lock().unwrap().clone()
    }
}

/// Merge-patch `diff` into `base` (objects merge recursively, anything else
/// replaces) — how the real agent and cloud both merge aggregate writes.
fn merge(base: &mut Value, diff: &Value) {
    match (base.as_object_mut(), diff.as_object()) {
        (Some(base_map), Some(diff_map)) => {
            for (k, v) in diff_map {
                match base_map.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                    _ => {
                        base_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *base = diff.clone(),
    }
}

#[async_trait::async_trait]
impl ChannelBackend for MockBackend {
    async fn fetch_channel_data(&self, channel: &str) -> Result<Option<Value>> {
        Ok(self.aggregates.lock().unwrap().get(channel).cloned())
    }

    async fn update_channel_aggregate(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()> {
        {
            let mut aggregates = self.aggregates.lock().unwrap();
            let entry =
                aggregates.entry(channel.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if opts.replace_data {
                *entry = data.clone();
            } else {
                merge(entry, data);
            }
        }
        self.aggregate_writes.lock().unwrap().push(RecordedAggregateWrite {
            channel: channel.to_string(),
            data: data.clone(),
            opts: opts.clone(),
        });
        Ok(())
    }

    async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        self.messages
            .lock()
            .unwrap()
            .push(RecordedMessage { channel: channel.to_string(), data: data.clone() });
        Ok(self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    async fn update_message(
        &self,
        channel: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
    ) -> Result<()> {
        self.message_updates.lock().unwrap().push(RecordedMessageUpdate {
            channel: channel.to_string(),
            message_id,
            data: data.clone(),
            opts: opts.clone(),
        });
        Ok(())
    }

    fn has_persistent_connection(&self) -> bool {
        false
    }
}
