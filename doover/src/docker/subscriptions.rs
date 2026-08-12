//! `SubscriptionHub` — one event-stream task per channel, distributing events
//! to registered callbacks. The Rust port of the subscription half of
//! pydoover's `DeviceAgentInterface` (`add_event_callback`,
//! `_run_channel_stream`, `stream_channel_events`).
//!
//! Semantics mirrored from pydoover:
//! - The first subscriber to a channel starts its stream task; later
//!   subscribers share it (and, like pydoover, miss the initial
//!   `ChannelSync` if they register after it fired).
//! - On task start the aggregate cache is seeded (creating the channel with
//!   an empty aggregate on 404), then a synthetic [`Event::channel_sync`] is
//!   delivered so subscribers see boot state through the same path as live
//!   events. Only *own* channels are created on read: a channel absent on
//!   another agent stays absent, since creating it there would invent a channel
//!   on a device we don't own.
//! - The stream reconnects forever with exponential backoff (reset on a
//!   successful connect, capped at 10s — pydoover's
//!   `time_between_connection_attempts`).
//! - `AggregateUpdate` events refresh the cache before dispatch.
//!
//! Callbacks are synchronous and must be cheap (push to a queue, update
//! shared state); spawn a task for real work.
//!
//! Every map here is keyed by [`ChannelRef::cache_key`], so a channel on another
//! agent never collides with a same-named own channel (pydoover `_channel_key`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::docker::device_agent::{AggregateOptions, ChannelRef, DeviceAgentClient};
use crate::error::Result;
use crate::events::{Event, EventSubscription};

pub type EventCallback = Arc<dyn Fn(&Event) + Send + Sync>;

const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ChannelState {
    callbacks: Vec<(EventSubscription, EventCallback)>,
    task: Option<JoinHandle<()>>,
    synced: bool,
}

#[derive(Default)]
struct HubState {
    channels: HashMap<String, ChannelState>,
    aggregates: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct SubscriptionHub {
    client: DeviceAgentClient,
    state: Arc<Mutex<HubState>>,
}

impl SubscriptionHub {
    pub fn new(client: DeviceAgentClient) -> Self {
        Self { client, state: Arc::new(Mutex::new(HubState::default())) }
    }

    /// Register a callback for events on a channel, filtered by `events`.
    /// Starts the channel's stream task if it isn't running yet.
    ///
    /// Pass a [`ChannelRef::on_agent`] to subscribe to another agent's channel;
    /// a bare `&str` is this device's own, as before.
    pub fn subscribe<'a>(
        &self,
        channel: impl Into<ChannelRef<'a>>,
        events: EventSubscription,
        callback: EventCallback,
    ) {
        let channel = channel.into();
        let key = channel.cache_key();
        let mut st = self.state.lock().unwrap();
        let ch = st.channels.entry(key).or_default();
        ch.callbacks.push((events, callback));
        if ch.task.is_none() {
            let hub = self.clone();
            let name = channel.name.to_string();
            let agent_id = channel.agent_id;
            ch.task =
                Some(tokio::spawn(async move { hub.run_channel_stream(name, agent_id).await }));
        }
    }

    /// The cached aggregate data for a subscribed channel, if synced.
    pub fn cached_aggregate<'a>(&self, channel: impl Into<ChannelRef<'a>>) -> Option<Value> {
        let key = channel.into().cache_key();
        self.state.lock().unwrap().aggregates.get(&key).cloned()
    }

    /// Fetch a channel's aggregate data — from the cache when the channel is
    /// subscribed, falling back to a gRPC call (pydoover
    /// `fetch_channel_aggregate`). Data only: the cache is fed by aggregate
    /// update events, which carry no attachments. For the whole aggregate, go
    /// to [`DeviceAgentClient::fetch_channel_aggregate`].
    pub async fn fetch_channel_data<'a>(
        &self,
        channel: impl Into<ChannelRef<'a>>,
    ) -> Result<Option<Value>> {
        let channel = channel.into();
        if let Some(v) = self.cached_aggregate(channel) {
            return Ok(Some(v));
        }
        self.client.fetch_channel_data(channel).await
    }

    /// Whether a subscribed channel has completed its initial sync
    /// (pydoover `is_channel_synced`).
    pub fn is_channel_synced<'a>(&self, channel: impl Into<ChannelRef<'a>>) -> bool {
        let key = channel.into().cache_key();
        let st = self.state.lock().unwrap();
        st.channels
            .get(&key)
            .is_some_and(|ch| !ch.callbacks.is_empty() && ch.synced)
    }

    /// Wait until every named channel is synced, or `timeout` elapses
    /// (pydoover `wait_for_channels_sync`). Returns whether all synced.
    pub async fn wait_for_channels_sync(&self, channels: &[&str], timeout: Duration) -> bool {
        self.wait_for_refs_sync(&channels.iter().map(|c| ChannelRef::own(c)).collect::<Vec<_>>(),
            timeout).await
    }

    /// As [`wait_for_channels_sync`](Self::wait_for_channels_sync), for channels
    /// that may belong to other agents.
    pub async fn wait_for_refs_sync(&self, channels: &[ChannelRef<'_>], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if channels.iter().all(|c| self.is_channel_synced(*c)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Cancel all stream tasks (pydoover `close`).
    pub fn close(&self) {
        let mut st = self.state.lock().unwrap();
        for ch in st.channels.values_mut() {
            if let Some(task) = ch.task.take() {
                task.abort();
            }
        }
    }

    async fn run_channel_stream(self, channel: String, agent_id: Option<u64>) {
        let target = ChannelRef { name: &channel, agent_id };
        let key = target.cache_key();
        // Seed the aggregate cache, creating the channel if it doesn't exist.
        let seeded: Option<Value> = match self.client.fetch_channel_data(target).await {
            Ok(Some(v)) => Some(v),
            // Only our own missing channels are created on read. A channel
            // absent on ANOTHER agent stays absent: creating it there would
            // invent a channel on a device we do not own, so subscribe without a
            // seeded aggregate and let the stream deliver whatever appears.
            Ok(None) if agent_id.is_some() => {
                tracing::info!(
                    "channel '{channel}' not found on agent {}; subscribing without a seeded \
                     aggregate",
                    agent_id.unwrap_or_default()
                );
                None
            }
            Ok(None) => {
                tracing::info!("channel '{channel}' not found, creating with empty aggregate");
                let empty = Value::Object(Map::new());
                match self
                    .client
                    .update_channel_aggregate(target, &empty, &AggregateOptions::default())
                    .await
                {
                    Ok(()) => Some(empty),
                    Err(e) => {
                        tracing::error!("failed to create channel '{channel}': {e}");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!("failed to seed aggregate cache for '{key}': {e}");
                None
            }
        };
        {
            let mut st = self.state.lock().unwrap();
            if let Some(v) = &seeded {
                st.aggregates.insert(key.clone(), v.clone());
            }
            if let Some(ch) = st.channels.get_mut(&key) {
                ch.synced = true;
            }
        }
        if let Some(v) = seeded {
            self.dispatch(&key, &Event::channel_sync(&channel, v));
        }

        let mut backoff = Duration::from_secs(1);
        loop {
            match self.client.subscribe_events(target).await {
                Ok(mut stream) => {
                    backoff = Duration::from_secs(1);
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(event) => {
                                if event.is_aggregate_update() {
                                    if let Some(data) = event.aggregate_data() {
                                        let mut st = self.state.lock().unwrap();
                                        st.aggregates.insert(key.clone(), data.clone());
                                        if let Some(ch) = st.channels.get_mut(&key) {
                                            ch.synced = true;
                                        }
                                    }
                                }
                                self.dispatch(&key, &event);
                            }
                            Err(e) => {
                                tracing::warn!("event stream error on '{channel}': {e}");
                                break;
                            }
                        }
                    }
                    tracing::debug!("event stream for '{channel}' ended; reconnecting");
                }
                Err(e) => tracing::warn!("failed to subscribe to '{channel}': {e}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
        }
    }

    /// `key` is a [`ChannelRef::cache_key`], not a bare channel name — the
    /// callbacks are registered under it.
    fn dispatch(&self, key: &str, event: &Event) {
        let flag = event.subscription_flag();
        if flag == EventSubscription::NONE {
            // Unknown event names are dropped, as in pydoover.
            return;
        }
        let callbacks: Vec<EventCallback> = {
            let st = self.state.lock().unwrap();
            st.channels
                .get(key)
                .map(|ch| {
                    ch.callbacks
                        .iter()
                        .filter(|(events, _)| events.contains(flag))
                        .map(|(_, cb)| cb.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        for cb in callbacks {
            cb(event);
        }
    }
}
