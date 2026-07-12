//! The [`Processor`] trait and [`ProcessorContext`] — the Rust counterpart
//! to pydoover's `pydoover.processor.Application`
//! (`pydoover/processor/application.py`).
//!
//! A processor is an event-driven cloud app: one invocation handles one
//! event (message/aggregate/deployment/schedule/ingestion/manual), talking
//! to `data.doover.com` over HTTP instead of a local device agent. The
//! runner (`runner.rs`) drives the pipeline; handlers get a
//! [`ProcessorContext`] with the upgraded credentials, the [`DataClient`],
//! and the per-invocation [`ProcessorTags`].

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::api::data::{DataClient, PingConnectionArgs};
use crate::api::Channel;
use crate::channel_backend::AggregateOptions;
use crate::error::Result;
use crate::models::{ConnectionDetermination, ConnectionStatus, Notification};

use super::config::ProcConfig;
use super::events::{
    AggregateUpdateEvent, DeploymentEvent, EventPayload, IngestionEndpointEvent,
    ManualInvokeEvent, MessageCreateEvent, ScheduleEvent,
};
use super::tags::{ProcessorTags, SetProcessorTagOptions};

/// pydoover `DEFAULT_OFFLINE_AFTER` — 1 hour.
pub const DEFAULT_OFFLINE_AFTER_SECS: u64 = 60 * 60;

/// Whether a handler actually handled the event.
///
/// pydoover skips an invocation as `no_handler` when the subclass didn't
/// override the handler (it compares method identity). Rust can't reflect on
/// overrides, so the default trait methods return
/// [`Handled::NotImplemented`] and the runner reproduces the skip from the
/// sentinel. Overridden handlers return [`Handled::Done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The event was handled by user code.
    Done,
    /// Default-method sentinel: no user handler for this event type.
    NotImplemented,
}

/// Why an invocation was recorded as a deliberate no-op (pydoover
/// `SkipReason`, serialized into the invocation summary's `skip_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NoHandler,
    PreHookFilter,
    TagValuesSelfLoop,
    PostSetupFilter,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoHandler => "no_handler",
            Self::PreHookFilter => "pre_hook_filter",
            Self::TagValuesSelfLoop => "tag_values_self_loop",
            Self::PostSetupFilter => "post_setup_filter",
        }
    }
}

/// Options for [`ProcessorContext::ping_connection`] (pydoover
/// `Application.ping_connection`).
#[derive(Debug, Clone, Default)]
pub struct PingOptions {
    /// When the device was last known online (ms since epoch); defaults to
    /// now.
    pub online_at_ms: Option<u64>,
    /// Defaults to `PeriodicUnknown`.
    pub connection_status: ConnectionStatus,
    /// Expected next-online deadline; when set, `offline_after` is derived
    /// from it (and written back to the connection config if it changed).
    pub offline_at_ms: Option<u64>,
}

struct ConnectionData {
    config: Value,
    status: Value,
}

/// Per-invocation runtime handed to every handler: upgraded identity
/// (`agent_id` / `app_key` / token), the HTTP [`DataClient`], the
/// [`ProcessorTags`] buffer, and the pydoover `Application` helper methods.
pub struct ProcessorContext {
    api: Arc<DataClient>,
    tags: Arc<ProcessorTags>,
    pub agent_id: u64,
    pub app_key: String,
    /// `deployment_config["APP_ID"]`.
    pub app_id: Option<String>,
    pub organisation_id: Option<u64>,
    /// `deployment_config["APP_DISPLAY_NAME"]`.
    pub display_name: Option<String>,
    /// The app's full deployment config from the token upgrade.
    pub deployment_config: Value,
    /// Parsed `dv_proc_config`.
    pub proc_config: ProcConfig,
    connection: Mutex<ConnectionData>,
}

impl ProcessorContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        api: Arc<DataClient>,
        tags: Arc<ProcessorTags>,
        agent_id: u64,
        app_key: String,
        app_id: Option<String>,
        organisation_id: Option<u64>,
        display_name: Option<String>,
        deployment_config: Value,
        proc_config: ProcConfig,
        connection_config: Value,
        connection_status: Value,
    ) -> Self {
        Self {
            api,
            tags,
            agent_id,
            app_key,
            app_id,
            organisation_id,
            display_name,
            deployment_config,
            proc_config,
            connection: Mutex::new(ConnectionData {
                config: connection_config,
                status: connection_status,
            }),
        }
    }

    /// The cloud data API client (upgraded token installed).
    pub fn api(&self) -> &Arc<DataClient> {
        &self.api
    }

    /// The per-invocation tag buffer (committed once by the runner).
    pub fn tags(&self) -> &Arc<ProcessorTags> {
        &self.tags
    }

    /// The current (upgraded) bearer token.
    pub fn token(&self) -> Option<String> {
        self.api.token()
    }

    /// The raw connection config from the token upgrade
    /// (`connection_data.config`).
    pub fn connection_config(&self) -> Value {
        self.connection.lock().unwrap().config.clone()
    }

    /// The raw connection status from the token upgrade
    /// (`connection_data.status`).
    pub fn connection_status(&self) -> Value {
        self.connection.lock().unwrap().status.clone()
    }

    /// pydoover `Application.get_tag`.
    pub fn get_tag(&self, key: &str) -> Option<Value> {
        self.tags.get_tag(key)
    }

    /// pydoover `Application.set_tag` (buffered; published by the runner's
    /// single end-of-invocation commit).
    pub fn set_tag(&self, key: &str, value: Value) {
        self.tags.set_tag(key, value)
    }

    /// `set_tag` with `log=true` — requests a logged data point at commit.
    pub fn set_tag_logged(&self, key: &str, value: Value) {
        self.tags.set_tag_with(
            key,
            value,
            &SetProcessorTagOptions { log: true, ..Default::default() },
        )
    }

    /// pydoover `Application.fetch_channel` — fetch a channel (with
    /// aggregate) on this agent by name.
    pub async fn fetch_channel(&self, channel_name: &str) -> Result<Channel> {
        self.api.fetch_channel(channel_name).await
    }

    /// pydoover `Application.send_notification` — publish to the
    /// `notifications` channel; the cloud fans it out to matching
    /// subscriptions. Returns the created message payload.
    pub async fn send_notification(&self, notification: impl Into<Notification>) -> Result<Value> {
        self.api.send_notification(notification, None).await
    }

    /// pydoover `Application.publish_ui_schema` — write a UI schema under
    /// this app's key in the `ui_state` aggregate. With `clear`, the app's
    /// subtree is *replaced* (`replace_keys=["state.children.<app_key>"]`)
    /// rather than merged, dropping stale elements.
    pub async fn publish_ui_schema(&self, schema: &Value, clear: bool) -> Result<()> {
        let data = json!({"state": {"children": {self.app_key.clone(): schema}}});
        let opts = if clear {
            AggregateOptions {
                replace_keys: vec![format!("state.children.{}", self.app_key)],
                ..Default::default()
            }
        } else {
            AggregateOptions::default()
        };
        self.api.update_channel_aggregate_http("ui_state", &data, &opts, None).await?;
        Ok(())
    }

    /// pydoover `Application.ping_connection` — publish a `doover_connection`
    /// ping. `offline_after` comes from `opts.offline_at_ms` when given
    /// (writing the changed value back to the connection config), else the
    /// agent's connection config, else 1 hour; the determination flips to
    /// `Offline` once `online_at` is older than that.
    pub async fn ping_connection(&self, opts: &PingOptions) -> Result<()> {
        let now = now_ms();
        let online_at = opts.online_at_ms.unwrap_or(now);

        let offline_after_secs: u64 = if let Some(offline_at) = opts.offline_at_ms {
            offline_at.saturating_sub(online_at) / 1000
        } else {
            self.connection
                .lock()
                .unwrap()
                .config
                .get("offline_after")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_OFFLINE_AFTER_SECS)
        };

        let determination = if now.saturating_sub(online_at) > offline_after_secs * 1000 {
            ConnectionDetermination::Offline
        } else {
            ConnectionDetermination::Online
        };

        // Persist a user-supplied offline_after when it differs from the
        // stored connection config (pydoover only does this when a config
        // already exists).
        if opts.offline_at_ms.is_some() {
            let updated = {
                let mut conn = self.connection.lock().unwrap();
                let has_config =
                    conn.config.as_object().is_some_and(|m| !m.is_empty());
                let stored =
                    conn.config.get("offline_after").and_then(Value::as_u64);
                if has_config && stored != Some(offline_after_secs) {
                    conn.config["offline_after"] = json!(offline_after_secs);
                    Some(conn.config.clone())
                } else {
                    None
                }
            };
            if let Some(config) = updated {
                self.api.update_connection_config(&config, Some(self.agent_id)).await?;
            }
        }

        self.api
            .ping_connection_at(&PingConnectionArgs {
                online_at_ms: online_at,
                connection_status: opts.connection_status,
                determination,
                ping_at_ms: None,
                user_agent: Some(format!("doover-rs-processor,app_key={}", self.app_key)),
                // Divergence: pydoover resolves the public IP via
                // checkip.amazonaws.com; doover-rs sends null (TODO).
                ip_address: None,
                agent_id: Some(self.agent_id),
            })
            .await
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// An event-driven Doover cloud processor (pydoover
/// `pydoover.processor.Application`).
///
/// Implement the handlers for the events your `dv_proc_config` subscribes
/// to; unimplemented events are recorded as skipped (`no_handler`) in the
/// invocation summary. Overridden handlers must return
/// `Ok(`[`Handled::Done`]`)` — returning the default
/// `Ok(Handled::NotImplemented)` from your own implementation would count as
/// "no handler".
///
/// Construction: one fresh value per invocation via [`Default`] (warm Lambda
/// containers share the HTTP connection pool, never processor state).
#[async_trait]
pub trait Processor: Send {
    /// Runs after the token upgrade, before the event handler. Errors are
    /// logged and the pipeline continues (pydoover behaviour).
    async fn setup(&mut self, _ctx: &ProcessorContext) -> Result<()> {
        Ok(())
    }

    /// Runs after the event handler, before the invocation ends. Errors are
    /// logged, not fatal.
    async fn close(&mut self, _ctx: &ProcessorContext) -> Result<()> {
        Ok(())
    }

    async fn on_message_create(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &MessageCreateEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    async fn on_aggregate_update(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &AggregateUpdateEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    /// Invoked when the app is (re)deployed to an agent. Unlike other
    /// events, a deployment runs the full pipeline even without an
    /// overridden handler (matching pydoover).
    async fn on_deployment(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &DeploymentEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    async fn on_schedule(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &ScheduleEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    async fn on_ingestion_endpoint(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &IngestionEndpointEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    async fn on_manual_invoke(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &ManualInvokeEvent,
    ) -> Result<Handled> {
        Ok(Handled::NotImplemented)
    }

    /// Early, cheap filter run *before* the token upgrade — return `false`
    /// to reject the event (`skip_reason = pre_hook_filter`). No context is
    /// available yet.
    async fn pre_hook_filter(&mut self, _event: &EventPayload) -> bool {
        true
    }

    /// Filter run after the token upgrade and `setup` — return `false` to
    /// reject (`skip_reason = post_setup_filter`). Prefer
    /// [`pre_hook_filter`](Self::pre_hook_filter) when you don't need the
    /// API.
    async fn post_setup_filter(&mut self, _ctx: &ProcessorContext, _event: &EventPayload) -> bool {
        true
    }

    /// Decode an ingestion-endpoint payload. doover-data wraps the raw
    /// request body in base64, so the default decodes base64 then parses
    /// JSON; override for e.g. C-packed structs (still base64-decode
    /// first!).
    fn parse_ingestion_payload(&self, payload: &str) -> Result<Value> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| crate::error::DooverError::InvalidPayload(format!("bad base64: {e}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
