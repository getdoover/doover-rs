//! `ProcessorTags` — the faithful port of pydoover's `TagsManagerProcessor`
//! (`pydoover/tags/manager.py`, bottom of file): per-invocation
//! dirty/touched buffers over the seeded `tag_values` snapshot, flushed by a
//! single [`ProcessorTags::commit_tags`] at the end of the invocation.
//!
//! This is deliberately its own type — the docker `TagsRuntime`
//! (`TagsManagerDocker` port) has loop-oriented buffering (max-age, periodic
//! log buckets, live tags) that has no meaning in a one-shot Lambda.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::channel_backend::{AggregateOptions, ChannelBackend};
use crate::error::Result;
use crate::tags::TAG_CHANNEL_NAME;

/// How [`ProcessorTags::commit_tags`] decides what to write to *logged
/// history* (pydoover `LogMode`). The live aggregate always tracks current
/// values regardless of mode; `Never` is the one mode that also suppresses
/// logging forced by `record_tag_update` or a per-set `log=true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogMode {
    /// Log the full app aggregate on every commit that has changes — every
    /// tag, changed or not. Dense history; the historical default.
    #[default]
    Always,
    /// Never write logged history. The live aggregate still updates.
    Never,
    /// Log only tags whose value differs from the stored value.
    OnlyChanged,
    /// Log every tag that was set this invocation, even when re-set to the
    /// same value — captures "the app asserted this".
    OnlySet,
}

/// Options for [`ProcessorTags::set_tag_with`].
#[derive(Debug, Clone, Default)]
pub struct SetProcessorTagOptions {
    /// Write under another app's key (marks the commit as touching external
    /// tags, widening the published scope).
    pub app_key: Option<String>,
    /// Request a logged data point for this update at commit time.
    pub log: bool,
}

struct TagState {
    /// The full `{app_key: {tag: value}}` payload, seeded from
    /// `SubscriptionInfo.tag_values` and mutated in place.
    tag_values: Value,
    update_tags: bool,
    record_tag_update: bool,
    update_external_tags: bool,
    log_mode: LogMode,
    /// Tags whose value actually moved this invocation.
    dirty: Map<String, Value>,
    /// Every tag `set()` this invocation, changed or not.
    touched: Map<String, Value>,
}

/// Buffered tag reads/writes for one processor invocation.
pub struct ProcessorTags {
    backend: Arc<dyn ChannelBackend>,
    app_key: String,
    agent_id: u64,
    state: Mutex<TagState>,
}

impl ProcessorTags {
    pub fn new(
        app_key: impl Into<String>,
        backend: Arc<dyn ChannelBackend>,
        agent_id: u64,
        tag_values: Value,
        record_tag_update: bool,
    ) -> Self {
        let tag_values = if tag_values.is_object() { tag_values } else { Value::Object(Map::new()) };
        Self {
            backend,
            app_key: app_key.into(),
            agent_id,
            state: Mutex::new(TagState {
                tag_values,
                update_tags: false,
                record_tag_update,
                update_external_tags: false,
                log_mode: LogMode::default(),
                dirty: Map::new(),
                touched: Map::new(),
            }),
        }
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    pub fn agent_id(&self) -> u64 {
        self.agent_id
    }

    pub fn log_mode(&self) -> LogMode {
        self.state.lock().unwrap().log_mode
    }

    pub fn set_log_mode(&self, mode: LogMode) {
        self.state.lock().unwrap().log_mode = mode;
    }

    /// Read a tag from the in-memory payload (this app's key).
    pub fn get_tag(&self, key: &str) -> Option<Value> {
        self.get_tag_scoped(&self.app_key, key)
    }

    /// Read a tag under an explicit app key.
    pub fn get_tag_scoped(&self, app_key: &str, key: &str) -> Option<Value> {
        let st = self.state.lock().unwrap();
        st.tag_values.get(app_key).and_then(|m| m.get(key)).cloned()
    }

    /// Set a tag under this app's key. Buffered; nothing is published until
    /// [`commit_tags`](Self::commit_tags).
    pub fn set_tag(&self, key: &str, value: Value) {
        self.set_tag_with(key, value, &SetProcessorTagOptions::default())
    }

    /// Set a tag with an explicit scope / log request — pydoover
    /// `TagsManagerProcessor.set_tag`. (Synchronous: unlike pydoover this
    /// does no I/O; only `commit_tags` talks to the API.)
    pub fn set_tag_with(&self, key: &str, value: Value, opts: &SetProcessorTagOptions) {
        let app_key = opts.app_key.as_deref().unwrap_or(&self.app_key);
        let mut st = self.state.lock().unwrap();

        let current = st
            .tag_values
            .get(app_key)
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(Value::Null);

        insert_nested(&mut st.touched, app_key, key, value.clone());

        // Python compares `current == value` where a missing key reads as
        // None — so re-setting a missing tag to null is also "unchanged".
        if current == value {
            if opts.log || st.log_mode == LogMode::OnlySet {
                st.update_tags = true;
                if opts.log {
                    st.record_tag_update = true;
                }
                if app_key != self.app_key {
                    st.update_external_tags = true;
                }
            }
            return;
        }

        match st.tag_values.get_mut(app_key).and_then(Value::as_object_mut) {
            Some(map) => {
                map.insert(key.to_string(), value.clone());
            }
            None => {
                if let Some(root) = st.tag_values.as_object_mut() {
                    let mut inner = Map::new();
                    inner.insert(key.to_string(), value.clone());
                    root.insert(app_key.to_string(), Value::Object(inner));
                }
            }
        }

        insert_nested(&mut st.dirty, app_key, key, value);
        st.update_tags = true;
        if opts.log {
            st.record_tag_update = true;
        }
        if app_key != self.app_key {
            st.update_external_tags = true;
        }
    }

    /// Flush buffered changes back to the data API — the single write per
    /// invocation. The aggregate carries full current state (merge
    /// semantics) but is only pushed when a value actually moved; the logged
    /// message payload follows [`LogMode`].
    pub async fn commit_tags(&self) -> Result<()> {
        self.commit_tags_with(false).await
    }

    /// `commit_tags(record_log=…)` — `record_log` forces a logged message
    /// even when no `set_tag` requested one (still subject to
    /// [`LogMode::Never`]).
    pub async fn commit_tags_with(&self, record_log: bool) -> Result<()> {
        // Snapshot + reset under the lock; awaits happen outside it.
        let (update, log_payload) = {
            let mut st = self.state.lock().unwrap();
            if !st.update_tags {
                return Ok(());
            }

            // Python truthiness gates the whole update: an empty/None own
            // payload publishes nothing (`update = update and {...}`).
            let update: Option<Value> = if st.update_external_tags {
                Some(st.tag_values.clone()).filter(|v| !is_falsy(v))
            } else {
                st.tag_values.get(&self.app_key).filter(|own| !is_falsy(own)).map(|own| {
                    let mut outer = Map::new();
                    outer.insert(self.app_key.clone(), own.clone());
                    Value::Object(outer)
                })
            };

            let aggregate_update =
                if !st.dirty.is_empty() { update.clone() } else { None };

            let log_payload = if (st.record_tag_update || record_log)
                && st.log_mode != LogMode::Never
            {
                match st.log_mode {
                    LogMode::Always => update,
                    LogMode::OnlyChanged => {
                        scope_payload(&st.dirty, &self.app_key, st.update_external_tags)
                    }
                    LogMode::OnlySet => {
                        scope_payload(&st.touched, &self.app_key, st.update_external_tags)
                    }
                    LogMode::Never => unreachable!(),
                }
            } else {
                None
            };

            st.update_tags = false;
            st.dirty = Map::new();
            st.touched = Map::new();
            (aggregate_update, log_payload)
        };

        if let Some(data) = update {
            self.backend
                .update_channel_aggregate(TAG_CHANNEL_NAME, &data, &AggregateOptions::default())
                .await?;
        }
        if let Some(payload) = log_payload {
            self.backend.create_message(TAG_CHANNEL_NAME, &payload).await?;
        }
        Ok(())
    }
}

/// `buffer[app_key][key] = value` on a `{app_key: {tag: value}}` map.
fn insert_nested(buffer: &mut Map<String, Value>, app_key: &str, key: &str, value: Value) {
    match buffer.get_mut(app_key).and_then(Value::as_object_mut) {
        Some(inner) => {
            inner.insert(key.to_string(), value);
        }
        None => {
            let mut inner = Map::new();
            inner.insert(key.to_string(), value);
            buffer.insert(app_key.to_string(), Value::Object(inner));
        }
    }
}

/// Python-dict truthiness for tag payloads: null / empty object are falsy.
fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// pydoover `_scope_payload`: trim a `{app_key: {tag: value}}` buffer to
/// what this commit publishes — external keys only when explicitly written.
fn scope_payload(source: &Map<String, Value>, app_key: &str, external: bool) -> Option<Value> {
    if external {
        if source.is_empty() {
            return None;
        }
        return Some(Value::Object(source.clone()));
    }
    let own = source.get(app_key)?;
    if is_falsy(own) {
        return None;
    }
    let mut outer = Map::new();
    outer.insert(app_key.to_string(), own.clone());
    Some(Value::Object(outer))
}
