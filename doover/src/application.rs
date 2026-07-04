//! The `Application` trait + `run_app` runner — the Rust equivalent of
//! pydoover's `pydoover.docker.Application` + `run_app`.
//!
//! An app author implements `setup` (once, after the agent is healthy) and
//! `main_loop` (every `loop_target_period`, drift-corrected). The runner wires
//! up the `DeviceAgentClient`, loads config, and drives the loop, surviving
//! transient errors by pausing and retrying (the container restarts on a hard
//! failure, exactly like the Python harness).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use tokio::time::{Instant, MissedTickBehavior};

use crate::client::{AggregateOptions, DeviceAgentClient};
use crate::config::Config;
use crate::error::Result;

/// The `dv-ui-sub` observation-claim freshness window (pydoover
/// `UI_SUB_FRESH_MS`): the customer-site re-stamps every 120 s while a tab is
/// visible, so an older stamp means the claim was dropped.
const UI_SUB_FRESH_MS: f64 = 120_000.0;

fn now_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Shared tag bookkeeping for live mode: the latest value written for each
/// tag, the set of tags declared `live`, and the latest `dv-ui-sub`
/// observation aggregate (maintained by a background subscription).
#[derive(Default)]
struct TagState {
    values: HashMap<String, Value>,
    live_tags: HashSet<String>,
    ui_sub: Value,
}

/// Qualified live-tag names (`<app_key>.<tag>`) some user currently has open
/// in live mode — pydoover `_live_tags_opened`, with the 120 s freshness gate.
fn live_tags_opened(ui_sub: &Value) -> HashSet<String> {
    let mut opened = HashSet::new();
    let now = now_unix_ms();
    let Some(bucket) = ui_sub.get("live_tag_open").and_then(Value::as_object) else {
        return opened;
    };
    for entry in bucket.values() {
        let fresh = entry
            .get("ts")
            .and_then(Value::as_f64)
            .is_some_and(|ts| now - ts < UI_SUB_FRESH_MS);
        if !fresh {
            continue;
        }
        if let Some(tags) = entry.get("tags").and_then(Value::as_array) {
            opened.extend(tags.iter().filter_map(|t| t.as_str().map(str::to_string)));
        }
    }
    opened
}

/// Well-known channels (pydoover constants).
pub mod channels {
    pub const DEPLOYMENT_CONFIG: &str = "deployment_config";
    pub const TAG_VALUES: &str = "tag_values";
    pub const UI_STATE: &str = "ui_state";
    pub const UI_CMDS: &str = "ui_cmds";
}

/// Runtime handles handed to every app callback.
#[derive(Clone)]
pub struct AppContext {
    client: DeviceAgentClient,
    config: Config,
    app_key: String,
    tags: Arc<Mutex<TagState>>,
}

impl AppContext {
    pub fn client(&self) -> &DeviceAgentClient {
        &self.client
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    /// Whether some user currently has `tag_name` open in live mode
    /// (pydoover `is_live_tag_open`). Tags are qualified `<app_key>.<tag>`.
    pub fn is_live_tag_open(&self, tag_name: &str) -> bool {
        let qualified = if self.app_key.is_empty() {
            tag_name.to_string()
        } else {
            format!("{}.{}", self.app_key, tag_name)
        };
        let st = self.tags.lock().unwrap();
        live_tags_opened(&st.ui_sub).contains(&qualified)
    }

    /// Whether any user has an active claim on this agent (any `dv-ui-sub`
    /// bucket has a fresh entry) — pydoover `is_being_observed`.
    pub fn is_being_observed(&self) -> bool {
        let st = self.tags.lock().unwrap();
        let now = now_unix_ms();
        ["agent_open", "group_open", "app_open", "live_tag_open"].iter().any(|b| {
            st.ui_sub
                .get(*b)
                .and_then(Value::as_object)
                .is_some_and(|m| {
                    m.values().any(|e| {
                        e.get("ts").and_then(Value::as_f64).is_some_and(|ts| now - ts < UI_SUB_FRESH_MS)
                    })
                })
        })
    }

    /// Merge-write to a channel aggregate (immediate).
    pub async fn update_channel_aggregate(&self, channel: &str, data: &Value) -> Result<()> {
        self.client
            .update_channel_aggregate(channel, data, &AggregateOptions::default())
            .await
    }

    /// Merge-write with options (max-age coalescing, save_log, …).
    pub async fn update_channel_aggregate_with(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()> {
        self.client.update_channel_aggregate(channel, data, opts).await
    }

    pub async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        self.client.create_message(channel, data).await
    }

    /// Set one tag on the `tag_values` channel, namespaced under this app_key
    /// (pydoover's `set_tag`). Coalesced with a 3s max-age like pydoover's
    /// tag commit.
    pub async fn set_tag(&self, name: &str, value: Value) -> Result<()> {
        self.set_tags([(name.to_string(), value)]).await
    }

    /// Set many tags at once (one aggregate write). Values are also recorded
    /// so live mode can re-stream them each loop.
    pub async fn set_tags(&self, tags: impl IntoIterator<Item = (String, Value)>) -> Result<()> {
        let mut inner = Map::new();
        for (k, v) in tags {
            inner.insert(k, v);
        }
        {
            let mut st = self.tags.lock().unwrap();
            for (k, v) in &inner {
                st.values.insert(k.clone(), v.clone());
            }
        }
        let mut outer = Map::new();
        outer.insert(self.app_key.clone(), Value::Object(inner));
        self.client
            .update_channel_aggregate(
                channels::TAG_VALUES,
                &Value::Object(outer),
                &AggregateOptions { max_age_secs: 3.0, ..Default::default() },
            )
            .await
    }

    /// Publish the current values of `live`-declared tags as a one-shot to
    /// `tag_values` — but only the ones some user has open in live mode
    /// (pydoover `flush_live_tags`). Called automatically after every
    /// `main_loop`, so a watched tag updates at the loop rate. Best-effort:
    /// errors (e.g. cloud not ready) are swallowed.
    async fn flush_live_tags(&self) {
        let payload = {
            let st = self.tags.lock().unwrap();
            if st.live_tags.is_empty() {
                return;
            }
            let opened = live_tags_opened(&st.ui_sub);
            if opened.is_empty() {
                return;
            }
            let mut inner = Map::new();
            for tag in &st.live_tags {
                let qualified = if self.app_key.is_empty() {
                    tag.clone()
                } else {
                    format!("{}.{}", self.app_key, tag)
                };
                if !opened.contains(&qualified) {
                    continue;
                }
                if let Some(v) = st.values.get(tag) {
                    inner.insert(tag.clone(), v.clone());
                }
            }
            if inner.is_empty() {
                return;
            }
            let mut outer = Map::new();
            outer.insert(self.app_key.clone(), Value::Object(inner));
            Value::Object(outer)
        };
        if let Err(e) = self.client.send_one_shot_message(channels::TAG_VALUES, &payload).await {
            tracing::trace!("live tag flush skipped: {e}");
        }
    }
}

/// Background task: keep `TagState.ui_sub` current by subscribing to the
/// `dv-ui-sub` observation channel (reconnecting on stream end).
fn spawn_ui_sub_watcher(client: DeviceAgentClient, tags: Arc<Mutex<TagState>>) {
    tokio::spawn(async move {
        loop {
            if let Ok(mut stream) = client.subscribe_events("dv-ui-sub").await {
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(ev) => {
                            if let Some(data) = ev.aggregate_data() {
                                tags.lock().unwrap().ui_sub = data.clone();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[async_trait]
pub trait Application: Send {
    /// Loop period; default 1s (pydoover default). Read once after `setup`.
    fn loop_target_period(&self) -> Duration {
        Duration::from_secs(1)
    }

    /// Tags that should stream in live mode: when a user has one open in the
    /// UI's live mode, its value is re-published as a one-shot after every
    /// `main_loop` (i.e. at the loop rate). Default: none.
    fn live_tags(&self) -> Vec<String> {
        Vec::new()
    }

    /// Runs once after the agent is healthy and config is loaded.
    async fn setup(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    /// Runs every `loop_target_period`.
    async fn main_loop(&mut self, ctx: &AppContext) -> Result<()>;
}

/// Runtime options resolved from env/args (pydoover `parse_args`).
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub dda_uri: String,
    pub app_key: String,
    pub config_fp: Option<String>,
    pub error_wait: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            dda_uri: std::env::var("DDA_URI")
                .unwrap_or_else(|_| "http://127.0.0.1:50051".to_string()),
            app_key: std::env::var("APP_KEY").unwrap_or_default(),
            config_fp: std::env::var("CONFIG_FP").ok(),
            error_wait: Duration::from_secs(10),
        }
    }
}

/// Connect, load config, and drive the app loop until SIGINT/SIGTERM.
pub async fn run_app<A: Application>(mut app: A) -> Result<()> {
    run_app_with(app_options_normalized(), &mut app).await
}

fn app_options_normalized() -> RunOptions {
    let mut o = RunOptions::default();
    // accept "host:port" as well as a full URL
    if !o.dda_uri.contains("://") {
        o.dda_uri = format!("http://{}", o.dda_uri);
    }
    o
}

pub async fn run_app_with<A: Application>(opts: RunOptions, app: &mut A) -> Result<()> {
    tracing::info!("connecting to device agent at {}", opts.dda_uri);
    let client = connect_with_retry(&opts.dda_uri).await?.with_app_id(opts.app_key.clone());

    let config = match &opts.config_fp {
        Some(fp) => {
            tracing::info!("loading config from {fp}");
            Config::from_file(fp)?
        }
        None if !opts.app_key.is_empty() => {
            load_deployment_config(&client, &opts.app_key).await
        }
        None => Config::default(),
    };

    // Live-mode plumbing: register the app's live tags and start watching the
    // observation channel so `flush_live_tags` knows who is watching.
    let tags = Arc::new(Mutex::new(TagState::default()));
    {
        let mut st = tags.lock().unwrap();
        st.live_tags = app.live_tags().into_iter().collect();
    }
    let has_live = !tags.lock().unwrap().live_tags.is_empty();
    let ctx = AppContext { client: client.clone(), config, app_key: opts.app_key.clone(), tags: tags.clone() };
    if has_live {
        spawn_ui_sub_watcher(client, tags);
    }

    app.setup(&ctx).await?;
    tracing::info!("setup complete; entering main loop");

    let period = app.loop_target_period();
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut stop = shutdown_signal();
    loop {
        tokio::select! {
            _ = &mut stop => {
                tracing::info!("stop signal received; exiting main loop");
                return Ok(());
            }
            _ = ticker.tick() => {
                let started = Instant::now();
                if let Err(e) = app.main_loop(&ctx).await {
                    tracing::error!("main_loop error: {e}; pausing {:?}", opts.error_wait);
                    tokio::time::sleep(opts.error_wait).await;
                } else {
                    // Stream live tags to any watcher at the loop rate.
                    ctx.flush_live_tags().await;
                    if started.elapsed() > period.mul_f64(1.2) {
                        tracing::warn!(
                            "main_loop took {:?}, exceeding target period {:?}",
                            started.elapsed(), period
                        );
                    }
                }
            }
        }
    }
}

/// Bootstrap config from the `deployment_config` channel (pydoover's
/// production path): fetch the aggregate and pull out
/// `applications.<app_key>`. Falls back to an empty config on any miss so the
/// app still starts (and can apply its own defaults).
async fn load_deployment_config(client: &DeviceAgentClient, app_key: &str) -> Config {
    match client.fetch_channel_aggregate(channels::DEPLOYMENT_CONFIG).await {
        Ok(Some(agg)) => {
            match agg.get("applications").and_then(|a| a.get(app_key)) {
                Some(cfg) => {
                    tracing::info!("loaded deployment config for app_key '{app_key}'");
                    Config::from_value(cfg.clone())
                }
                None => {
                    tracing::warn!("no deployment config for app_key '{app_key}'; using defaults");
                    Config::default()
                }
            }
        }
        Ok(None) => {
            tracing::warn!("deployment_config channel is empty; using defaults");
            Config::default()
        }
        Err(e) => {
            tracing::warn!("could not fetch deployment_config ({e}); using defaults");
            Config::default()
        }
    }
}

async fn connect_with_retry(uri: &str) -> Result<DeviceAgentClient> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut attempt = 0u32;
    loop {
        match DeviceAgentClient::connect(uri.to_string()).await {
            Ok(client) => {
                // one echo to confirm the service is actually answering
                match client.test_comms("hello from doover-rs").await {
                    Ok(_) => return Ok(client),
                    Err(e) => tracing::debug!("agent not answering yet: {e}"),
                }
            }
            Err(e) => tracing::debug!("connect failed: {e}"),
        }
        if Instant::now() >= deadline {
            return Err(crate::error::DooverError::Other(
                "device agent did not become healthy within 300s".into(),
            ));
        }
        attempt += 1;
        let backoff = Duration::from_millis(200u64.saturating_mul(2u64.pow(attempt.min(5))))
            .min(Duration::from_secs(5));
        tokio::time::sleep(backoff).await;
    }
}

/// Resolves on SIGINT or SIGTERM (or Ctrl-C on non-unix).
fn shutdown_signal() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    })
}
