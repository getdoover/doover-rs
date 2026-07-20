//! The `Application` trait + `doover::run` runner — the Rust equivalent of
//! pydoover's `pydoover.docker.Application` + `run_app`.
//!
//! An app declares its typed `Config` / `Tags` / `Ui` as associated types and
//! is constructed by the runner via [`Application::create`] once those are
//! loaded/attached/built. It implements `setup` (once, after the agent is
//! healthy) and `main_loop` (every `loop_target_period`, drift-corrected),
//! plus optional channel-event callbacks (`on_message_create`,
//! `on_aggregate_update`, …) for channels subscribed via
//! [`AppContext::subscribe`], and [`Application::on_ui_command`] for user
//! commands on its UI interactions. On a `main_loop` error the runner marks
//! the app unhealthy, waits, and exits — the container restarts it, exactly
//! like the Python harness.
//!
//! The binary entry point is [`run`], which also implements the built-in
//! `export` subcommand (`my-app export [doover_config.json] [--app-name N]`)
//! writing the config + UI schemas without connecting to an agent.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::channel_backend::ChannelBackend;
use crate::config::{write_config_schema, write_ui_schema, Config, ConfigSchema, TagRef};
use crate::docker::device_agent::{AggregateOptions, DeviceAgentClient};
use crate::docker::healthcheck::{spawn_healthcheck_server, HealthState};
use crate::docker::subscriptions::SubscriptionHub;
use crate::error::Result;
use crate::events::{Event, EventSubscription};
use crate::models::{Notification, NOTIFICATIONS_CHANNEL};
use crate::rpc::RpcManager;
use crate::tags::{KeyPath, RemoteTag, SetTagOptions, TagValue, TagsCollection, TagsRuntime};
use crate::ui::runtime::resolve_config_refs;
use crate::ui::{UiApplicationInfo, UiBuild, UiCommand, UiRuntime, UiTree};

/// Identity handed to the app by the deployment config (pydoover sets
/// `device_agent.agent_id` and `app_display_name` on every config update).
#[derive(Default)]
struct AppMeta {
    agent_id: Option<Value>,
    app_display_name: String,
}

/// Well-known channels (pydoover constants).
pub mod channels {
    pub const DEPLOYMENT_CONFIG: &str = "deployment_config";
    pub const TAG_VALUES: &str = "tag_values";
    pub const UI_STATE: &str = "ui_state";
    pub const UI_CMDS: &str = "ui_cmds";
    pub const UI_SUB: &str = "dv-ui-sub";
    pub const RPC: &str = "dv-rpc";
    pub const NOTIFICATIONS: &str = "notifications";
}

/// Events routed into the run loop so they can be delivered to `&mut app`.
enum RunnerEvent {
    /// From a channel the app subscribed to via `AppContext::subscribe`.
    App(Event),
    /// From the runner's internal `deployment_config` subscription.
    ConfigUpdate(Event),
    /// A user command for one of this app's UI interactions.
    UiCommand(UiCommand),
}

/// Runtime handles handed to every app callback.
#[derive(Clone)]
pub struct AppContext {
    client: DeviceAgentClient,
    hub: SubscriptionHub,
    config: Arc<RwLock<Config>>,
    app_key: String,
    tags: Arc<TagsRuntime>,
    rpc: Arc<RpcManager>,
    ui: Arc<UiRuntime>,
    meta: Arc<Mutex<AppMeta>>,
    events_tx: mpsc::UnboundedSender<RunnerEvent>,
}

impl AppContext {
    pub fn client(&self) -> &DeviceAgentClient {
        &self.client
    }

    /// A snapshot of the current (raw) config. The config can be re-injected
    /// live when the `deployment_config` channel changes (see
    /// [`Application::on_config_update`]), so this returns an owned copy
    /// rather than a reference.
    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    /// The agent id from the deployment config (`AGENT_ID`), if known.
    pub fn agent_id(&self) -> Option<Value> {
        self.meta.lock().unwrap().agent_id.clone()
    }

    /// The app's display name from the deployment config
    /// (`APP_DISPLAY_NAME`), if known.
    pub fn app_display_name(&self) -> String {
        self.meta.lock().unwrap().app_display_name.clone()
    }

    /// Whether the last request to the device agent succeeded.
    pub fn is_dda_available(&self) -> bool {
        self.client.status().is_available()
    }

    /// Whether the device agent currently reports cloud sync.
    pub fn is_dda_online(&self) -> bool {
        self.client.status().is_online()
    }

    /// Whether the device agent has been online at least once.
    pub fn has_dda_been_online(&self) -> bool {
        self.client.status().has_been_online()
    }

    /// Subscribe the app to all events on a channel; they are delivered to
    /// the `on_message_create` / `on_aggregate_update` / … callbacks between
    /// loop iterations (pydoover `add_event_callback`).
    pub fn subscribe(&self, channel: &str) {
        self.subscribe_filtered(channel, EventSubscription::ALL)
    }

    /// Subscribe with an event-kind filter.
    pub fn subscribe_filtered(&self, channel: &str, events: EventSubscription) {
        let tx = self.events_tx.clone();
        self.hub.subscribe(
            channel,
            events,
            Arc::new(move |ev: &Event| {
                let _ = tx.send(RunnerEvent::App(ev.clone()));
            }),
        );
    }

    /// Fetch a channel's aggregate data — served from the subscription cache
    /// when the channel is subscribed, else a gRPC round-trip.
    pub async fn fetch_channel_data(&self, channel: &str) -> Result<Option<Value>> {
        self.hub.fetch_channel_data(channel).await
    }

    /// Wait for subscribed channels to complete their initial sync.
    pub async fn wait_for_channels_sync(&self, channels: &[&str], timeout: Duration) -> bool {
        self.hub.wait_for_channels_sync(channels, timeout).await
    }

    /// The tags runtime: buffered reads/writes, log points, subscriptions,
    /// and observation state.
    pub fn tags(&self) -> &Arc<TagsRuntime> {
        &self.tags
    }

    /// The RPC manager (pydoover `self.rpc`): `call` / `fire_and_forget`
    /// plus runtime handler registration over the `dv-rpc` channel (or any
    /// other).
    pub fn rpc(&self) -> &Arc<RpcManager> {
        &self.rpc
    }

    /// The UI runtime: cached `ui_cmds` values ([`UiRuntime::get_value`])
    /// and interaction write-back ([`UiRuntime::set_value`]).
    pub fn ui(&self) -> &Arc<UiRuntime> {
        &self.ui
    }

    /// Send a notification via the `notifications` channel (pydoover
    /// `send_notification`); the cloud fans it out to matching subscriptions.
    /// Returns the created message id.
    pub async fn send_notification(&self, notification: impl Into<Notification>) -> Result<u64> {
        self.client
            .create_message(NOTIFICATIONS_CHANNEL, &notification.into().to_json())
            .await
    }

    /// Whether some user currently has `tag_name` open in live mode
    /// (pydoover `is_live_tag_open`). Tags are qualified `<app_key>.<tag>`.
    pub fn is_live_tag_open(&self, tag_name: &str) -> bool {
        self.tags.is_live_tag_open(tag_name, None)
    }

    /// Whether any user has an active claim on this agent (any `dv-ui-sub`
    /// bucket has a fresh entry) — pydoover `is_being_observed`.
    pub fn is_being_observed(&self) -> bool {
        self.tags.is_being_observed()
    }

    /// Whether some user has this app expanded on the customer-site.
    pub fn is_app_open(&self) -> bool {
        self.tags.is_app_open()
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

    /// Set one tag namespaced under this app_key (pydoover's `set_tag`).
    /// Buffered — the runner flushes once per loop with the max-age rule
    /// (3s while the app is open, 15min otherwise). Use
    /// [`AppContext::tags`] for flush/log/only_if_changed control.
    pub async fn set_tag(&self, name: &str, value: Value) -> Result<()> {
        self.tags
            .set_tag(&self.app_key, name, value, &SetTagOptions::default())
            .await
    }

    /// Set many tags at once (buffered, like [`AppContext::set_tag`]).
    pub async fn set_tags(&self, tags: impl IntoIterator<Item = (String, Value)>) -> Result<()> {
        let mut inner = Map::new();
        for (k, v) in tags {
            inner.insert(k, v);
        }
        let nested = if self.app_key.is_empty() {
            Value::Object(inner)
        } else {
            let mut outer = Map::new();
            outer.insert(self.app_key.clone(), Value::Object(inner));
            Value::Object(outer)
        };
        self.tags.set_nested_tags(nested, &SetTagOptions::default()).await
    }

    /// Read a tag (cached channel state overlaid with pending writes),
    /// namespaced under this app_key.
    pub fn get_tag(&self, name: &str) -> Option<Value> {
        self.tags.get_tag(&self.app_key, name)
    }

    /// Read a single tag published by **another** app by its app key — the
    /// imperative cross-app read (pydoover `get_tag(tag_key, app_key=…)`).
    /// Served from the local `tag_values` cache; the runtime subscribes to
    /// the whole channel, so remote values stay fresh.
    pub fn get_remote_tag(&self, app_key: &str, name: &str) -> Option<Value> {
        self.tags.get_tag(app_key, name)
    }

    /// Resolve a [`TagRef`] config binding into a typed, read-only
    /// [`RemoteTag`] — the declarative cross-app tag (pydoover `RemoteTag` +
    /// `config.TagRef`). `None` when the operator hasn't configured the
    /// reference. `default` is the fallback value when the upstream tag is
    /// absent/null.
    pub fn remote_tag<T: TagValue>(
        &self,
        tag_ref: &TagRef,
        default: Option<Value>,
    ) -> Option<RemoteTag<T>> {
        RemoteTag::resolve(self.tags.clone(), tag_ref, default)
    }

    /// Bind another app's entire declared tag schema (a `#[derive(Tags)]`
    /// type) to its app key, read-only — for reading a known sibling app
    /// (e.g. another instance of the same app) whose key you already have.
    pub fn remote_tags<C: TagsCollection>(&self, app_key: &str) -> C {
        C::attach_remote(self.tags.clone(), app_key)
    }

    /// Extract `applications.<app_key>` from a `deployment_config` aggregate
    /// and re-inject it (pydoover `_on_deployment_config_update`).
    fn apply_deployment_config(&self, full_aggregate: &Value) {
        let app_config = full_aggregate
            .get("applications")
            .and_then(|a| a.get(&self.app_key))
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!("application key {} not found in deployment config", self.app_key);
                Value::Object(Map::new())
            });
        {
            let mut meta = self.meta.lock().unwrap();
            meta.agent_id = app_config.get("AGENT_ID").cloned();
            meta.app_display_name = app_config
                .get("APP_DISPLAY_NAME")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            tracing::info!("agent id set: {:?}", meta.agent_id);
        }
        tracing::info!("deployment config updated");
        *self.config.write().unwrap() = Config::from_value(app_config);
    }
}

/// A Doover device application over the declarative framework.
///
/// Associated types declare the app's typed config
/// ([`ConfigSchema`], usually `#[derive(Config)]`), tags
/// ([`TagsCollection`], `#[derive(Tags)]`) and UI
/// ([`UiTree`] + [`UiBuild`], `#[derive(Ui)]`); all three default to
/// `()`-style no-ops for apps that don't need them. The runner loads the
/// deployment config into `Config`, attaches `Tags` to the live runtime,
/// builds `Ui` from the tags, and hands all three to
/// [`create`](Application::create).
#[async_trait]
pub trait Application: Send + Sized + 'static {
    /// Typed deployment config; `()` for config-less apps.
    type Config: ConfigSchema + Send;
    /// Declared tags; `()` for tag-less apps.
    type Tags: TagsCollection + Send + Sync;
    /// Declared UI; `()` for UI-less apps (nothing is published to
    /// `ui_state`).
    type Ui: UiTree + UiBuild<Tags = Self::Tags> + Send;

    /// Construct the app from its loaded config, attached tags and built UI.
    fn create(config: Self::Config, tags: Self::Tags, ui: Self::Ui) -> Self;

    /// The app's UI, if it has one. Apps with `type Ui = ()` keep the
    /// default (`None`); apps with a real UI return the field
    /// [`create`](Application::create) stored.
    fn ui(&self) -> Option<&Self::Ui> {
        None
    }

    /// Mutable access to the UI (used by the runner to `finalize` before
    /// each publish). Must return `Some` whenever [`ui`](Application::ui)
    /// does.
    fn ui_mut(&mut self) -> Option<&mut Self::Ui> {
        None
    }

    /// The root `uiApplication` literals for the published/exported schema
    /// (pydoover `UI.__init_subclass__` kwargs). Override to set icon /
    /// colour / display-name overrides.
    fn ui_info() -> UiApplicationInfo {
        UiApplicationInfo::default()
    }

    /// Loop period; default 1s (pydoover default). Read once after `setup`.
    fn loop_target_period(&self) -> Duration {
        Duration::from_secs(1)
    }

    /// Runs once after the agent is healthy and config is loaded.
    async fn setup(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    /// Runs every `loop_target_period`.
    async fn main_loop(&mut self, ctx: &AppContext) -> Result<()>;

    /// A user issued a command on one of this app's UI interactions.
    ///
    /// After this returns `Ok`, the runner writes the value back into the
    /// `ui_cmds` aggregate (plus a log message) and marks the command
    /// message successful — pydoover's default `Interaction.handler` +
    /// response flow. Returning `Err` sends an error status instead and
    /// skips the write-back. Match commands with
    /// `cmd.is(&self.ui().unwrap().my_button)` / [`UiCommand::value_as`].
    async fn on_ui_command(&mut self, _ctx: &AppContext, _cmd: &UiCommand) -> Result<()> {
        Ok(())
    }

    /// A message was created on a channel subscribed via
    /// [`AppContext::subscribe`].
    async fn on_message_create(&mut self, _ctx: &AppContext, _event: &Event) -> Result<()> {
        Ok(())
    }

    /// A message was updated on a subscribed channel.
    async fn on_message_update(&mut self, _ctx: &AppContext, _event: &Event) -> Result<()> {
        Ok(())
    }

    /// A subscribed channel's aggregate changed.
    async fn on_aggregate_update(&mut self, _ctx: &AppContext, _event: &Event) -> Result<()> {
        Ok(())
    }

    /// A one-shot (non-persisted) message arrived on a subscribed channel.
    async fn on_oneshot_message(&mut self, _ctx: &AppContext, _event: &Event) -> Result<()> {
        Ok(())
    }

    /// A subscribed channel completed its initial sync; the event carries the
    /// boot-time aggregate.
    async fn on_channel_sync(&mut self, _ctx: &AppContext, _event: &Event) -> Result<()> {
        Ok(())
    }

    /// The deployment config changed and has been re-injected: `config` is
    /// the freshly-parsed typed config and `ctx.config()` returns the new
    /// raw values.
    async fn on_config_update(&mut self, _ctx: &AppContext, _config: Self::Config) -> Result<()> {
        Ok(())
    }

    /// Called once on SIGINT/SIGTERM before the runner returns.
    async fn on_shutdown(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }
}

/// Runtime options resolved from env vars and CLI args (pydoover
/// `parse_args`): `--app-key`/`APP_KEY`, `--dda-uri`/`DDA_URI`,
/// `--plt-uri`/`PLT_URI`, `--modbus-uri`/`MODBUS_URI`,
/// `--config-fp`/`CONFIG_FP`, `--healthcheck-port`/`HEALTHCHECK_PORT`,
/// `--remote-dev`/`REMOTE_DEV` (rewrites `localhost`/`127.0.0.1` in every
/// URI), `--debug`/`DEBUG=1`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub dda_uri: String,
    pub plt_uri: String,
    pub modbus_uri: String,
    pub app_key: String,
    pub config_fp: Option<String>,
    pub healthcheck_port: u16,
    pub debug: bool,
    pub error_wait: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self::from_env_and_args(std::iter::empty::<String>())
    }
}

impl RunOptions {
    /// Resolve options from the process environment plus explicit args
    /// (each CLI flag beats its env var, which beats the default).
    pub fn from_env(args: impl IntoIterator<Item = String>) -> Self {
        Self::from_env_and_args(args)
    }

    fn from_env_and_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut cli: HashMap<String, String> = HashMap::new();
        let mut debug_flag = false;
        let mut it = args.into_iter().peekable();
        while let Some(arg) = it.next() {
            if arg == "--debug" {
                debug_flag = true;
            } else if let Some(key) = arg.strip_prefix("--") {
                if let Some(value) = it.next() {
                    cli.insert(key.to_string(), value);
                }
            }
        }
        let pick = |flag: &str, env: &str, default: &str| {
            cli.get(flag)
                .cloned()
                .or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| default.to_string())
        };

        let mut dda_uri = pick("dda-uri", "DDA_URI", "localhost:50051");
        let mut plt_uri = pick("plt-uri", "PLT_URI", "localhost:50053");
        let mut modbus_uri = pick("modbus-uri", "MODBUS_URI", "localhost:50054");
        let remote_dev = cli
            .get("remote-dev")
            .cloned()
            .or_else(|| std::env::var("REMOTE_DEV").ok().filter(|s| !s.is_empty()));
        if let Some(host) = remote_dev {
            for uri in [&mut dda_uri, &mut plt_uri, &mut modbus_uri] {
                *uri = uri.replace("localhost", &host).replace("127.0.0.1", &host);
            }
        }

        let config_fp = cli
            .get("config-fp")
            .cloned()
            .or_else(|| std::env::var("CONFIG_FP").ok().filter(|s| !s.is_empty()));
        let healthcheck_port = pick("healthcheck-port", "HEALTHCHECK_PORT", "49200")
            .parse()
            .unwrap_or(49200);
        let debug = debug_flag || std::env::var("DEBUG").is_ok_and(|v| v == "1");

        Self {
            dda_uri,
            plt_uri,
            modbus_uri,
            app_key: pick("app-key", "APP_KEY", ""),
            config_fp,
            healthcheck_port,
            debug,
            error_wait: Duration::from_secs(10),
        }
    }
}

/// Ensure a URI has a scheme for tonic (`host:port` → `http://host:port`).
pub(crate) fn normalize_uri(uri: &str) -> String {
    if uri.contains("://") {
        uri.to_string()
    } else {
        format!("http://{uri}")
    }
}

/// Route the [`RpcManager`]'s channel subscriptions through a
/// [`SubscriptionHub`]: every channel the manager needs gets a hub
/// subscription whose events are dispatched back into
/// [`RpcManager::handle_event`] on a spawned task. Called by the runner; also
/// usable directly when composing the pieces by hand (e.g. tests).
pub fn wire_rpc(rpc: &Arc<RpcManager>, hub: &SubscriptionHub) {
    let hub = hub.clone();
    // Weak breaks the manager → subscriber-closure → manager cycle.
    let weak = Arc::downgrade(rpc);
    rpc.set_subscriber(move |channel: &str| {
        let weak = weak.clone();
        hub.subscribe(
            channel,
            EventSubscription::MESSAGE_CREATE
                | EventSubscription::MESSAGE_UPDATE
                | EventSubscription::ONESHOT_MESSAGE,
            Arc::new(move |ev: &Event| {
                let Some(manager) = weak.upgrade() else { return };
                let ev = ev.clone();
                tokio::spawn(async move { manager.handle_event(&ev).await });
            }),
        );
    });
}

/// Subscribe the [`UiRuntime`] to `ui_cmds` (pydoover
/// `UICommandsManager.subscribe`): aggregate events refresh the cached
/// values, message events that parse as commands are queued to the runner.
fn wire_ui_cmds(
    ui: &Arc<UiRuntime>,
    hub: &SubscriptionHub,
    events_tx: &mpsc::UnboundedSender<RunnerEvent>,
) {
    let ui = ui.clone();
    let tx = events_tx.clone();
    hub.subscribe(
        channels::UI_CMDS,
        EventSubscription::MESSAGE_CREATE
            | EventSubscription::ONESHOT_MESSAGE
            | EventSubscription::AGGREGATE_UPDATE
            | EventSubscription::CHANNEL_SYNC,
        Arc::new(move |ev: &Event| {
            if let Some(cmd) = ui.handle_event(ev) {
                let _ = tx.send(RunnerEvent::UiCommand(cmd));
            }
        }),
    );
}

/// Entry point for an application binary: handles the built-in `export`
/// subcommand (before touching the network), otherwise connects and drives
/// the app until SIGINT/SIGTERM.
///
/// `export [path] [--app-name NAME]` writes `A::Config`'s JSON schema and
/// `A::Ui`'s schema into `path` (default `./doover_config.json`,
/// read-merge-write preserving other keys). `NAME` defaults to the `APP_KEY`
/// env var or the binary name.
pub async fn run<A: Application>() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("export") {
        let mut path = "./doover_config.json".to_string();
        let mut app_name: Option<String> = None;
        let mut it = argv[2..].iter();
        while let Some(arg) = it.next() {
            if arg == "--app-name" {
                app_name = it.next().cloned();
            } else if !arg.starts_with("--") {
                path = arg.clone();
            }
        }
        let app_name = app_name
            .or_else(|| std::env::var("APP_KEY").ok().filter(|s| !s.is_empty()))
            .or_else(|| {
                Path::new(&argv[0])
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "app".to_string());
        let wrote_ui = write_export::<A>(&path, &app_name)?;
        println!(
            "wrote config_schema{} for '{app_name}' to {path}",
            if wrote_ui { " + ui_schema" } else { "" }
        );
        return Ok(());
    }
    run_with::<A>(RunOptions::from_env(argv.into_iter().skip(1))).await
}

/// Write `A`'s config schema (and UI schema, when the UI has elements) into
/// a `doover_config.json` via the pydoover-compatible read-merge-write.
/// Config references in the UI schema stay unresolved (`$config.app().…`),
/// exactly like pydoover's `UI.export()`. Returns whether a `ui_schema` was
/// written.
pub fn write_export<A: Application>(path: impl AsRef<Path>, app_name: &str) -> Result<bool> {
    let path = path.as_ref();
    write_config_schema(path, app_name, A::Config::schema().to_json())?;

    let tags = A::Tags::detached();
    let mut ui = A::Ui::build(&tags);
    ui.finalize();
    if ui.children().is_empty() {
        return Ok(false);
    }
    write_ui_schema(path, app_name, ui.to_schema(&A::ui_info()))?;
    Ok(true)
}

/// Connect, load config, and drive the app loop until SIGINT/SIGTERM, with
/// explicit options (see [`run`] for the argv/env entry point).
pub async fn run_with<A: Application>(opts: RunOptions) -> Result<()> {
    // Healthcheck comes up first so the container's HEALTHCHECK has an
    // endpoint during startup (unhealthy until the first good loop).
    let health = HealthState::default();
    spawn_healthcheck_server(opts.healthcheck_port, health.clone()).await;

    let dda_uri = normalize_uri(&opts.dda_uri);
    tracing::info!("connecting to device agent at {dda_uri}");
    let client = connect_with_retry(&dda_uri).await?.with_app_id(opts.app_key.clone());
    let hub = SubscriptionHub::new(client.clone());
    let backend: Arc<dyn ChannelBackend> = Arc::new(client.clone());

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let tags_runtime = Arc::new(TagsRuntime::new(client.clone(), opts.app_key.clone()));

    let rpc = Arc::new(RpcManager::new(
        backend.clone(),
        (!opts.app_key.is_empty()).then(|| opts.app_key.clone()),
    ));
    wire_rpc(&rpc, &hub);
    let ui_runtime = Arc::new(UiRuntime::new(backend.clone(), opts.app_key.clone()));

    let ctx = AppContext {
        client: client.clone(),
        hub: hub.clone(),
        config: Arc::new(RwLock::new(Config::default())),
        app_key: opts.app_key.clone(),
        tags: tags_runtime.clone(),
        rpc,
        ui: ui_runtime.clone(),
        meta: Arc::new(Mutex::new(AppMeta::default())),
        events_tx: events_tx.clone(),
    };

    // Config bootstrap: file path in dev, live deployment_config channel in
    // production (kept subscribed so updates re-inject, pydoover `_run`).
    match &opts.config_fp {
        Some(fp) => {
            tracing::info!("loading config from {fp}");
            *ctx.config.write().unwrap() = Config::from_file(fp)?;
        }
        None if !opts.app_key.is_empty() => {
            let tx = events_tx.clone();
            hub.subscribe(
                channels::DEPLOYMENT_CONFIG,
                EventSubscription::AGGREGATE_UPDATE,
                Arc::new(move |ev: &Event| {
                    let _ = tx.send(RunnerEvent::ConfigUpdate(ev.clone()));
                }),
            );
            hub.wait_for_channels_sync(&[channels::DEPLOYMENT_CONFIG], Duration::from_secs(5))
                .await;
            match hub.fetch_channel_data(channels::DEPLOYMENT_CONFIG).await {
                Ok(Some(agg)) => ctx.apply_deployment_config(&agg),
                _ => tracing::warn!("no initial deployment config available from DDA"),
            }
        }
        None => {}
    }

    // Typed config: a missing required key is a hard startup error naming
    // the key (pydoover raises the same from `_inject_deployment_config`).
    let typed_config = A::Config::from_value(ctx.config().root()).map_err(|e| {
        tracing::error!("failed to load application config: {e}");
        e
    })?;

    // Tag plumbing: subscribe tag_values + dv-ui-sub and wait for their
    // initial sync (pydoover `TagsManagerDocker.setup`), then register the
    // app's declared live-mode tags.
    tags_runtime.setup(&hub).await;
    tags_runtime.set_live_tags(
        A::Tags::live_tag_names()
            .into_iter()
            .map(|t| KeyPath::scoped(&opts.app_key, [t])),
    );

    let tags = A::Tags::attach(tags_runtime.clone());
    let ui = A::Ui::build(&tags);
    let mut app = A::create(typed_config, tags, ui);

    // UI command plumbing (pydoover `_setup`: `ui_manager.subscribe("ui_cmds")`
    // + `_set_interactions`). Interactions are collected again after `setup`
    // in publish_ui-adjacent code below, but names are fixed at build time.
    if let Some(ui) = app.ui_mut() {
        ui.finalize();
    }
    ui_runtime.set_interactions(interaction_names(&app));
    wire_ui_cmds(&ui_runtime, &hub, &events_tx);

    if let Err(e) = app.setup(&ctx).await {
        tracing::error!("error in setup function: {e}");
        tracing::warn!("waiting {:?} before restarting app", opts.error_wait);
        tokio::time::sleep(opts.error_wait).await;
        return Err(e);
    }

    // Publish the runtime-resolved UI schema (pydoover's double publish in
    // `_setup`, after the UI's own setup mutations).
    ui_runtime.set_interactions(interaction_names(&app));
    publish_ui(&mut app, &ctx).await;
    tracing::info!("setup complete; entering main loop");

    let period = app.loop_target_period();
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut stop = shutdown_signal();
    loop {
        tokio::select! {
            _ = &mut stop => {
                tracing::info!("stop signal received; exiting main loop");
                if let Err(e) = app.on_shutdown(&ctx).await {
                    tracing::error!("error in on_shutdown: {e}");
                }
                return Ok(());
            }
            Some(event) = events_rx.recv() => {
                handle_runner_event(&mut app, &ctx, event).await;
            }
            _ = ticker.tick() => {
                let started = Instant::now();
                // pydoover runs main_loop + commit_tags in one try block: an
                // unhandled error from either marks the app unhealthy, waits,
                // and exits so the container restarts it.
                let result = match app.main_loop(&ctx).await {
                    Ok(()) => ctx.tags.commit_tags().await,
                    Err(e) => Err(e),
                };
                if let Err(e) = result {
                    tracing::error!("error in loop function: {e}");
                    tracing::warn!("waiting {:?} before restarting app", opts.error_wait);
                    health.set_healthy(false);
                    tokio::time::sleep(opts.error_wait).await;
                    return Err(e);
                }
                // Re-publish ui_state when the app mutated its UI this loop
                // (compare-and-publish; a no-op when nothing changed).
                publish_ui(&mut app, &ctx).await;
                health.set_healthy(true);
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

/// The names of the app's UI interactions (the commands it accepts),
/// including interactions nested inside containers (pydoover
/// `UI.get_interactions` recurses).
fn interaction_names<A: Application>(app: &A) -> Vec<String> {
    app.ui().map(|ui| ui.interaction_names()).unwrap_or_default()
}

/// Serialize the UI (config refs resolved against the live deployment
/// config) and publish it to `ui_state` when it changed. Apps without UI
/// elements publish nothing. Publish errors are logged, not fatal —
/// matching pydoover's tolerance of transient DDA errors outside the loop
/// body.
async fn publish_ui<A: Application>(app: &mut A, ctx: &AppContext) {
    let Some(ui) = app.ui_mut() else { return };
    ui.finalize();
    if ui.children().is_empty() {
        return;
    }
    let schema = ui.to_schema(&A::ui_info());
    let resolved = resolve_config_refs(&schema, ctx.config().root());
    match ctx.ui().publish_schema(&resolved).await {
        Ok(true) => tracing::info!("updated ui_state with runtime-generated schema"),
        Ok(false) => {}
        Err(e) => tracing::error!("failed to publish ui_state schema: {e}"),
    }
}

async fn handle_runner_event<A: Application>(app: &mut A, ctx: &AppContext, event: RunnerEvent) {
    match event {
        RunnerEvent::ConfigUpdate(event) => {
            if let Some(data) = event.aggregate_data() {
                ctx.apply_deployment_config(data);
                match A::Config::from_value(ctx.config().root()) {
                    Ok(config) => {
                        if let Err(e) = app.on_config_update(ctx, config).await {
                            tracing::error!("error in on_config_update: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("updated deployment config failed to load: {e}");
                    }
                }
            }
        }
        RunnerEvent::UiCommand(cmd) => {
            tracing::debug!("ui command '{}' = {}", cmd.name, cmd.value);
            match app.on_ui_command(ctx, &cmd).await {
                Ok(()) => {
                    // pydoover's default Interaction.handler: write the value
                    // back into ui_cmds (+ log message), then mark the request
                    // message successful.
                    if let Err(e) = ctx.ui().set_value(&cmd.name, cmd.value.clone(), true).await {
                        tracing::error!("failed to write back ui command '{}': {e}", cmd.name);
                    }
                    if let Err(e) = ctx.ui().respond_success(&cmd).await {
                        tracing::error!("failed to respond to ui command '{}': {e}", cmd.name);
                    }
                }
                Err(e) => {
                    tracing::error!("error in on_ui_command for '{}': {e}", cmd.name);
                    if let Err(e2) =
                        ctx.ui().respond_error(&cmd, "INTERNAL_ERROR", &e.to_string()).await
                    {
                        tracing::error!("failed to send ui command error: {e2}");
                    }
                }
            }
        }
        RunnerEvent::App(event) => {
            let result = if event.is_message_create() {
                app.on_message_create(ctx, &event).await
            } else if event.is_message_update() {
                app.on_message_update(ctx, &event).await
            } else if event.is_aggregate_update() {
                app.on_aggregate_update(ctx, &event).await
            } else if event.is_one_shot() {
                app.on_oneshot_message(ctx, &event).await
            } else if event.is_channel_sync() {
                app.on_channel_sync(ctx, &event).await
            } else {
                Ok(())
            };
            if let Err(e) = result {
                tracing::error!(
                    "error in {} handler for channel '{}': {e}",
                    event.event_name,
                    event.channel
                );
            }
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
