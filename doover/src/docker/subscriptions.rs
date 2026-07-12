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
//!   events.
//! - The stream reconnects forever with exponential backoff (reset on a
//!   successful connect, capped at 10s — pydoover's
//!   `time_between_connection_attempts`).
//! - `AggregateUpdate` events refresh the cache before dispatch.
//!
//! Callbacks are synchronous and must be cheap (push to a queue, update
//! shared state); spawn a task for real work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::docker::device_agent::{AggregateOptions, DeviceAgentClient};
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
    pub fn subscribe(&self, channel: &str, events: EventSubscription, callback: EventCallback) {
        let mut st = self.state.lock().unwrap();
        let ch = st.channels.entry(channel.to_string()).or_default();
        ch.callbacks.push((events, callback));
        if ch.task.is_none() {
            let hub = self.clone();
            let name = channel.to_string();
            ch.task = Some(tokio::spawn(async move { hub.run_channel_stream(name).await }));
        }
    }

    /// The cached aggregate data for a subscribed channel, if synced.
    pub fn cached_aggregate(&self, channel: &str) -> Option<Value> {
        self.state.lock().unwrap().aggregates.get(channel).cloned()
    }

    /// Fetch a channel's aggregate — from the cache when the channel is
    /// subscribed, falling back to a gRPC call (pydoover
    /// `fetch_channel_aggregate`).
    pub async fn fetch_channel_aggregate(&self, channel: &str) -> Result<Option<Value>> {
        if let Some(v) = self.cached_aggregate(channel) {
            return Ok(Some(v));
        }
        self.client.fetch_channel_aggregate(channel).await
    }

    /// Whether a subscribed channel has completed its initial sync
    /// (pydoover `is_channel_synced`).
    pub fn is_channel_synced(&self, channel: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.channels
            .get(channel)
            .is_some_and(|ch| !ch.callbacks.is_empty() && ch.synced)
    }

    /// Wait until every named channel is synced, or `timeout` elapses
    /// (pydoover `wait_for_channels_sync`). Returns whether all synced.
    pub async fn wait_for_channels_sync(&self, channels: &[&str], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if channels.iter().all(|c| self.is_channel_synced(c)) {
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

    async fn run_channel_stream(self, channel: String) {
        // Seed the aggregate cache, creating the channel if it doesn't exist.
        let seeded: Option<Value> = match self.client.fetch_channel_aggregate(&channel).await {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                tracing::info!("channel '{channel}' not found, creating with empty aggregate");
                let empty = Value::Object(Map::new());
                match self
                    .client
                    .update_channel_aggregate(&channel, &empty, &AggregateOptions::default())
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
                tracing::error!("failed to seed aggregate cache for '{channel}': {e}");
                None
            }
        };
        {
            let mut st = self.state.lock().unwrap();
            if let Some(v) = &seeded {
                st.aggregates.insert(channel.clone(), v.clone());
            }
            if let Some(ch) = st.channels.get_mut(&channel) {
                ch.synced = true;
            }
        }
        if let Some(v) = seeded {
            self.dispatch(&channel, &Event::channel_sync(&channel, v));
        }

        let mut backoff = Duration::from_secs(1);
        loop {
            match self.client.subscribe_events(&channel).await {
                Ok(mut stream) => {
                    backoff = Duration::from_secs(1);
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(event) => {
                                if event.is_aggregate_update() {
                                    if let Some(data) = event.aggregate_data() {
                                        let mut st = self.state.lock().unwrap();
                                        st.aggregates.insert(channel.clone(), data.clone());
                                        if let Some(ch) = st.channels.get_mut(&channel) {
                                            ch.synced = true;
                                        }
                                    }
                                }
                                self.dispatch(&channel, &event);
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

    fn dispatch(&self, channel: &str, event: &Event) {
        let flag = event.subscription_flag();
        if flag == EventSubscription::NONE {
            // Unknown event names are dropped, as in pydoover.
            return;
        }
        let callbacks: Vec<EventCallback> = {
            let st = self.state.lock().unwrap();
            st.channels
                .get(channel)
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
