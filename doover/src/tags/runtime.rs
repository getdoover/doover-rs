//! `TagsRuntime` — the port of pydoover's `TagsManagerDocker`.
//!
//! All state sits behind one mutex; locks are never held across awaits.
//! Publishing follows pydoover exactly: buffered writes flushed once per
//! loop, `only_if_changed` diffing against cache+pending, immediate vs
//! 15-minute periodic log buckets, and a 3 s / 900 s aggregate max-age
//! depending on whether a user has the app open.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::docker::device_agent::{AggregateOptions, DeviceAgentClient};
use crate::docker::subscriptions::SubscriptionHub;
use crate::error::Result;
use crate::events::{Event, EventSubscription};
use crate::utils::{apply_diff, apply_diff_in_place, generate_diff};

use super::{
    strip_paths, KeyPath, LogTrigger, TriggerSet, LIVE_TAG_CHANNEL_NAME, TAG_CHANNEL_NAME,
    TAG_CLOUD_MAX_AGE, TAG_OBSERVED_MAX_AGE, UI_SUB_CHANNEL_NAME, UI_SUB_FRESH_MS,
};

/// Callback for a tag subscription: `(path, changed_value)`. The value is
/// the changed subtree from the update diff (`None` when the path itself
/// vanished from the diff — matching pydoover's `lookup_dict` semantics).
pub type TagCallback = Arc<dyn Fn(&KeyPath, Option<&Value>) + Send + Sync>;

/// Options for a tag write (pydoover `set_tags` keyword args).
#[derive(Debug, Clone)]
pub struct SetTagOptions {
    /// Skip the write when the value matches the cached+pending state.
    pub only_if_changed: bool,
    /// Publish immediately instead of waiting for the end-of-loop commit.
    pub flush: bool,
    /// Record this update as a logged data point at the end of this loop
    /// rather than waiting for the 15-minute periodic log flush.
    pub log: bool,
}

impl Default for SetTagOptions {
    fn default() -> Self {
        Self { only_if_changed: true, flush: false, log: false }
    }
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_none_or(|m| m.is_empty())
}

fn now_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

struct TagsState {
    /// Cached `tag_values` aggregate, kept current by the channel
    /// subscription and by applying our own flushes.
    tag_values: Value,
    /// Buffered writes awaiting the end-of-loop flush.
    pending_aggregate: Value,
    /// Changes awaiting the periodic (15-min) log flush.
    pending_log: Value,
    /// Changes promoted to logging at the end of this loop (`log=true`).
    pending_immediate_log: Value,
    dirty: bool,
    last_log_flush: Option<Instant>,
    live_tag_keys: Vec<KeyPath>,
    ui_sub: Value,
    subscriptions: Vec<(KeyPath, TagCallback)>,
    /// `log_on` triggers per tag path, evaluated on every single-tag set
    /// (pydoover keeps the equivalent state on the `Tags` instance).
    log_triggers: Vec<(KeyPath, TriggerSet)>,
}

impl Default for TagsState {
    fn default() -> Self {
        Self {
            tag_values: empty_object(),
            pending_aggregate: empty_object(),
            pending_log: empty_object(),
            pending_immediate_log: empty_object(),
            dirty: false,
            last_log_flush: None,
            live_tag_keys: Vec::new(),
            ui_sub: empty_object(),
            subscriptions: Vec::new(),
            log_triggers: Vec::new(),
        }
    }
}

pub struct TagsRuntime {
    client: DeviceAgentClient,
    app_key: String,
    log_interval: Duration,
    state: Mutex<TagsState>,
}

impl TagsRuntime {
    pub fn new(client: DeviceAgentClient, app_key: impl Into<String>) -> Self {
        Self {
            client,
            app_key: app_key.into(),
            log_interval: Duration::from_secs_f32(TAG_CLOUD_MAX_AGE),
            state: Mutex::new(TagsState::default()),
        }
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    /// Register the `tag_values` + `dv-ui-sub` subscriptions and wait for
    /// their initial sync (pydoover `TagsManagerDocker.setup`).
    pub async fn setup(self: &Arc<Self>, hub: &SubscriptionHub) {
        let rt = self.clone();
        hub.subscribe(
            TAG_CHANNEL_NAME,
            EventSubscription::AGGREGATE_UPDATE | EventSubscription::CHANNEL_SYNC,
            Arc::new(move |ev: &Event| {
                if ev.is_channel_sync() {
                    rt.on_tag_sync(ev);
                } else {
                    rt.on_tag_update(ev);
                }
            }),
        );
        let rt = self.clone();
        hub.subscribe(
            UI_SUB_CHANNEL_NAME,
            EventSubscription::AGGREGATE_UPDATE | EventSubscription::CHANNEL_SYNC,
            Arc::new(move |ev: &Event| {
                if let Some(data) = ev.aggregate_data() {
                    rt.state.lock().unwrap().ui_sub = data.clone();
                }
            }),
        );
        hub.wait_for_channels_sync(
            &[TAG_CHANNEL_NAME, UI_SUB_CHANNEL_NAME],
            Duration::from_secs(10),
        )
        .await;
    }

    fn on_tag_sync(&self, event: &Event) {
        let data = event.aggregate_data().cloned().unwrap_or_else(empty_object);
        self.state.lock().unwrap().tag_values = data;
    }

    fn on_tag_update(&self, event: &Event) {
        let Some(new_values) = event.aggregate_data() else {
            return;
        };
        // Compute what changed, refresh the cache, then notify subscribers
        // outside the lock.
        let matches: Vec<(KeyPath, TagCallback, Option<Value>)> = {
            let mut st = self.state.lock().unwrap();
            let diff = generate_diff(&st.tag_values, new_values, false);
            st.tag_values = new_values.clone();
            if is_empty_object(&diff) {
                return;
            }
            st.subscriptions
                .iter()
                .filter(|(kp, _)| kp.in_value(&diff))
                .map(|(kp, cb)| (kp.clone(), cb.clone(), kp.lookup(&diff).cloned()))
                .collect()
        };
        for (kp, cb, value) in matches {
            cb(&kp, value.as_ref());
        }
    }

    /// Register a callback for updates to a tag path, scoped under
    /// `app_key` (pass this app's key for own tags, `""` for global paths).
    pub fn subscribe_to_tag(
        &self,
        app_key: &str,
        key: &str,
        callback: TagCallback,
    ) {
        let kp = KeyPath::scoped(app_key, [key]);
        let mut st = self.state.lock().unwrap();
        st.subscriptions.retain(|(existing, _)| existing != &kp);
        st.subscriptions.push((kp, callback));
    }

    /// Register `log_on` triggers for a tag path (called by
    /// [`Tag::attached`](super::Tag) for every declared tag with triggers).
    /// `default` is the tag's declared default — the `prev` fallback during
    /// evaluation, mirroring pydoover `Tags._get_tag_value` (`None` ==
    /// pydoover `NotSet`). Re-registering a path replaces its triggers and
    /// resets their state.
    pub fn register_log_triggers(
        &self,
        app_key: &str,
        key: &str,
        triggers: Vec<LogTrigger>,
        default: Option<Value>,
    ) {
        let kp = KeyPath::scoped(app_key, [key]);
        let mut st = self.state.lock().unwrap();
        st.log_triggers.retain(|(existing, _)| existing != &kp);
        st.log_triggers.push((kp, TriggerSet::new(triggers, default)));
    }

    /// Evaluate the registered triggers for one tag path against an
    /// incoming value — pydoover `Tags._set_tag_value`: `prev` is the
    /// current cached+pending value, falling back to the declared default.
    /// Every trigger runs (state must advance) even when the caller already
    /// requested `log=true`.
    fn evaluate_log_triggers(&self, kp: &KeyPath, new: &Value) -> bool {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        let Some(idx) = st.log_triggers.iter().position(|(existing, _)| existing == kp) else {
            return false;
        };
        let current = apply_diff(&st.tag_values, &st.pending_aggregate, false);
        let prev = kp.lookup(&current).cloned();
        st.log_triggers[idx].1.evaluate(prev.as_ref(), new)
    }

    /// Register the fully-qualified paths of `live=true` tags
    /// (pydoover `set_live_tags`).
    pub fn set_live_tags(&self, keys: impl IntoIterator<Item = KeyPath>) {
        self.state.lock().unwrap().live_tag_keys = keys.into_iter().collect();
    }

    // ---- observation (`dv-ui-sub`) helpers ----

    fn fresh_entries(ui_sub: &Value, bucket: &str) -> Vec<Value> {
        let now = now_unix_ms();
        ui_sub
            .get(bucket)
            .and_then(Value::as_object)
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| {
                        let ts = match entry {
                            Value::Object(m) => m.get("ts").and_then(Value::as_f64),
                            other => other.as_f64(),
                        };
                        ts.is_some_and(|ts| now - ts < UI_SUB_FRESH_MS)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether *any* user has an active claim on this agent.
    pub fn is_being_observed(&self) -> bool {
        let st = self.state.lock().unwrap();
        ["agent_open", "group_open", "app_open", "live_tag_open"]
            .iter()
            .any(|b| !Self::fresh_entries(&st.ui_sub, b).is_empty())
    }

    /// Whether some user has this agent's page open.
    pub fn is_agent_open(&self) -> bool {
        let st = self.state.lock().unwrap();
        !Self::fresh_entries(&st.ui_sub, "agent_open").is_empty()
    }

    /// Whether some user is viewing a context that renders this agent.
    pub fn is_group_open(&self) -> bool {
        let st = self.state.lock().unwrap();
        !Self::fresh_entries(&st.ui_sub, "group_open").is_empty()
    }

    /// Whether some user has this app expanded on the customer-site —
    /// drives the aggregate max-age.
    pub fn is_app_open(&self) -> bool {
        if self.app_key.is_empty() {
            return false;
        }
        let st = self.state.lock().unwrap();
        Self::fresh_entries(&st.ui_sub, "app_open").iter().any(|entry| {
            entry
                .get("apps")
                .and_then(Value::as_array)
                .is_some_and(|apps| apps.iter().any(|a| a.as_str() == Some(&self.app_key)))
        })
    }

    fn live_tags_opened(ui_sub: &Value) -> std::collections::HashSet<String> {
        let mut opened = std::collections::HashSet::new();
        for entry in Self::fresh_entries(ui_sub, "live_tag_open") {
            if let Some(tags) = entry.get("tags").and_then(Value::as_array) {
                opened.extend(tags.iter().filter_map(|t| t.as_str().map(str::to_string)));
            }
        }
        opened
    }

    /// Whether some user has this tag in live mode (qualified
    /// `<app_key>.<tag>` on the wire).
    pub fn is_live_tag_open(&self, tag_name: &str, app_key: Option<&str>) -> bool {
        let app_key = app_key.unwrap_or(&self.app_key);
        let qualified = if app_key.is_empty() {
            tag_name.to_string()
        } else {
            format!("{app_key}.{tag_name}")
        };
        let st = self.state.lock().unwrap();
        Self::live_tags_opened(&st.ui_sub).contains(&qualified)
    }

    fn max_age_secs(&self) -> f32 {
        if self.is_app_open() {
            TAG_OBSERVED_MAX_AGE
        } else {
            TAG_CLOUD_MAX_AGE
        }
    }

    // ---- reads ----

    /// Read a tag from the cached channel state overlaid with pending
    /// writes. `app_key` of `""` reads a global (unscoped) tag.
    pub fn get_tag(&self, app_key: &str, key: &str) -> Option<Value> {
        let kp = KeyPath::scoped(app_key, [key]);
        let st = self.state.lock().unwrap();
        let current = apply_diff(&st.tag_values, &st.pending_aggregate, false);
        kp.lookup(&current).cloned()
    }

    // ---- writes ----

    /// Set a single tag scoped under `app_key` (`""` for global). Any
    /// registered `log_on` triggers are evaluated first and a fired trigger
    /// promotes the write to `log=true` (pydoover `Tags._set_tag_value`) —
    /// note pydoover's `only_if_changed` check still runs afterwards, so a
    /// fired trigger whose value didn't actually change writes nothing.
    pub async fn set_tag(
        &self,
        app_key: &str,
        key: &str,
        value: Value,
        opts: &SetTagOptions,
    ) -> Result<()> {
        let kp = KeyPath::scoped(app_key, [key]);
        let fired = self.evaluate_log_triggers(&kp, &value);
        let opts = SetTagOptions { log: opts.log || fired, ..opts.clone() };
        let nested = kp.construct(value);
        self.set_nested_tags(nested, &opts).await
    }

    /// Publish multiple tag values; `tags` is the already-nested
    /// `{app_key: {tag: value}}` shape (pydoover `set_tags`).
    pub async fn set_nested_tags(&self, tags: Value, opts: &SetTagOptions) -> Result<()> {
        let flush_payload: Option<(Value, f32)> = {
            let mut st = self.state.lock().unwrap();
            if opts.only_if_changed {
                let current = apply_diff(&st.tag_values, &st.pending_aggregate, false);
                let diff = generate_diff(&current, &tags, false);
                if is_empty_object(&diff) {
                    tracing::debug!("set_tags: value did not change existing values");
                    return Ok(());
                }
            }

            if opts.log {
                // Promote to the immediate-log bucket and dedupe the
                // periodic bucket so the same change isn't logged twice.
                let tags_clone = tags.clone();
                apply_diff_in_place(&mut st.pending_immediate_log, &tags_clone, false);
                let mut pending_log = std::mem::replace(&mut st.pending_log, Value::Null);
                strip_paths(&mut pending_log, &tags);
                st.pending_log = pending_log;
            } else {
                // Preserve nulls so `set(None)` propagates as "clear this
                // tag" rather than silently disappearing.
                apply_diff_in_place(&mut st.pending_log, &tags, false);
            }

            apply_diff_in_place(&mut st.pending_aggregate, &tags, false);
            if opts.flush {
                Some((st.pending_aggregate.clone(), 0.0))
            } else {
                st.dirty = true;
                None
            }
        };

        if let Some((payload, _)) = flush_payload {
            tracing::debug!("set_tags: flushing to dda");
            let max_age = self.max_age_secs();
            self.client
                .update_channel_aggregate(
                    TAG_CHANNEL_NAME,
                    &payload,
                    &AggregateOptions { max_age_secs: max_age, ..Default::default() },
                )
                .await?;
            let mut st = self.state.lock().unwrap();
            let pending = st.pending_aggregate.clone();
            apply_diff_in_place(&mut st.tag_values, &pending, true);
            st.dirty = false;
        }
        Ok(())
    }

    /// End-of-loop commit: flush buffered writes, stream live tags, and
    /// write any due log messages (pydoover `commit_tags`).
    pub async fn commit_tags(&self) -> Result<()> {
        self.flush_tags().await?;
        self.flush_live_tags().await;
        self.flush_immediate_logs().await?;

        let periodic_due = {
            let st = self.state.lock().unwrap();
            !is_empty_object(&st.pending_log)
                && st
                    .last_log_flush
                    .is_none_or(|at| at.elapsed() >= self.log_interval)
        };
        if periodic_due {
            self.flush_logs().await?;
        }
        Ok(())
    }

    /// Flush buffered tag changes to the aggregate.
    pub async fn flush_tags(&self) -> Result<()> {
        let data = {
            let mut st = self.state.lock().unwrap();
            if !st.dirty {
                return Ok(());
            }
            st.dirty = false;
            std::mem::replace(&mut st.pending_aggregate, empty_object())
        };
        let max_age = self.max_age_secs();
        self.client
            .update_channel_aggregate(
                TAG_CHANNEL_NAME,
                &data,
                &AggregateOptions { max_age_secs: max_age, ..Default::default() },
            )
            .await?;
        let mut st = self.state.lock().unwrap();
        apply_diff_in_place(&mut st.tag_values, &data, true);
        Ok(())
    }

    /// Publish current values of `live=true` tags as a one-shot message —
    /// only the subset some user has live mode enabled on. Best-effort.
    pub async fn flush_live_tags(&self) {
        let payload = {
            let st = self.state.lock().unwrap();
            if st.live_tag_keys.is_empty() {
                return;
            }
            let opened = Self::live_tags_opened(&st.ui_sub);
            if opened.is_empty() {
                return;
            }
            let current = apply_diff(&st.tag_values, &st.pending_aggregate, false);
            let mut payload = empty_object();
            for kp in &st.live_tag_keys {
                if !opened.contains(&kp.qualified_name()) {
                    continue;
                }
                let Some(value) = kp.lookup(&current) else {
                    continue;
                };
                let nested = kp.construct(value.clone());
                apply_diff_in_place(&mut payload, &nested, false);
            }
            if is_empty_object(&payload) {
                return;
            }
            payload
        };
        if let Err(e) = self.client.send_one_shot_message(LIVE_TAG_CHANNEL_NAME, &payload).await {
            tracing::trace!("live tag flush skipped: {e}");
        }
    }

    /// Flush tag updates marked `log=true` as a channel message.
    pub async fn flush_immediate_logs(&self) -> Result<()> {
        let data = {
            let mut st = self.state.lock().unwrap();
            if is_empty_object(&st.pending_immediate_log) {
                return Ok(());
            }
            std::mem::replace(&mut st.pending_immediate_log, empty_object())
        };
        self.client.create_message(TAG_CHANNEL_NAME, &data).await?;
        Ok(())
    }

    /// Flush the periodic log buffer as a channel message.
    pub async fn flush_logs(&self) -> Result<()> {
        let data = {
            let mut st = self.state.lock().unwrap();
            if is_empty_object(&st.pending_log) {
                return Ok(());
            }
            st.last_log_flush = Some(Instant::now());
            std::mem::replace(&mut st.pending_log, empty_object())
        };
        self.client.create_message(TAG_CHANNEL_NAME, &data).await?;
        Ok(())
    }

    /// Backfill historical logged tag values: one message per
    /// `(unix_millis, tags)` point, scoped under `app_key` (pydoover
    /// `log_history`). Returns the number of messages written.
    pub async fn log_history(
        &self,
        app_key: &str,
        points: impl IntoIterator<Item = (u64, Value)>,
    ) -> Result<usize> {
        let mut count = 0;
        for (timestamp_ms, tags) in points {
            if is_empty_object(&tags) {
                continue;
            }
            let payload = if app_key.is_empty() {
                tags
            } else {
                let mut map = Map::new();
                map.insert(app_key.to_string(), tags);
                Value::Object(map)
            };
            self.client
                .create_message_at(TAG_CHANNEL_NAME, &payload, timestamp_ms)
                .await?;
            count += 1;
        }
        Ok(count)
    }
}
