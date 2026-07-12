//! `DataClient` — async HTTP client for the Doover cloud data API.
//!
//! Ports the processor-facing subset of pydoover's `AsyncDataClient`
//! (`pydoover/api/data/_async.py`) plus the `ProcessorDataClient` overrides
//! (`pydoover/processor/data_client.py`): channels/aggregates/messages, the
//! processor token-upgrade endpoints, connection pings, and notifications.
//! Faithfully reproduced behaviours:
//!
//! - endpoint paths and verbs (`/agents/{agent_id}/channels/{name}/…`,
//!   `/processors/subscriptions/{id}`, …), query encoding (bools lowercased,
//!   lists repeated, `None` filtered — `BaseClient._build_query`);
//! - `Authorization: Bearer <token>` + optional `X-Doover-Organisation`;
//! - gzip *request* compression for JSON bodies ≥ 50 bytes at level 6
//!   (`pydoover/api/_compress.py`), on by default;
//! - retry with exponential backoff on 5xx and transport errors
//!   (`max_retries`, `retry_delay * 2^attempt`);
//! - error mapping: 404 → [`DooverError::NotFound`], other non-2xx →
//!   [`DooverError::Http`] (`models/data/exceptions.py`);
//! - the anti-recursion invoking-channel guard (`_check_invoking_channel`):
//!   while handling a `MessageCreate`/`AggregateUpdate` invocation, writes to
//!   the triggering channel are refused unless it is `tag_values` scoped to
//!   this app's key.

use std::io::Write as _;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::channel_backend::{AggregateOptions, ChannelBackend, UpdateMessageOptions};
use crate::error::{DooverError, Result};
use crate::models::{
    value_as_id, ConnectionDetermination, ConnectionStatus, Notification, SubscriptionInfo,
    NOTIFICATIONS_CHANNEL,
};

use super::auth::BearerAuth;

/// pydoover `DEFAULT_DATA_ENDPOINT`.
pub const DEFAULT_DATA_ENDPOINT: &str = "https://data.doover.com/api";
/// Env var that overrides the base URL (pydoover `DOOVER_DATA_ENDPOINT`).
pub const DATA_ENDPOINT_ENV: &str = "DOOVER_DATA_ENDPOINT";

/// pydoover `MIN_COMPRESS_SIZE` — JSON bodies below this are sent plain.
const MIN_COMPRESS_SIZE: usize = 50;
/// pydoover's default gzip level.
const GZIP_LEVEL: u32 = 6;

/// A channel as returned by `GET /agents/{agent_id}/channels/{name}`
/// (pydoover `models/data/channel.py::Channel`), keeping the raw payload for
/// fields not modelled here.
#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub owner_id: u64,
    pub is_private: bool,
    /// `aggregate.data` when `include_aggregate` was requested and the
    /// channel has one.
    pub aggregate_data: Option<Value>,
    pub raw: Value,
}

impl Channel {
    fn from_value(raw: Value) -> Result<Self> {
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| DooverError::InvalidPayload("channel missing name".into()))?
            .to_string();
        let owner_id = raw.get("owner_id").and_then(value_as_id).unwrap_or_default();
        let is_private = raw.get("is_private").and_then(Value::as_bool).unwrap_or(false);
        let aggregate_data = raw
            .get("aggregate")
            .and_then(|a| a.get("data"))
            .filter(|d| !d.is_null())
            .cloned();
        Ok(Self { name, owner_id, is_private, aggregate_data, raw })
    }
}

/// Pagination/filter query for [`DataClient::list_messages`].
#[derive(Debug, Clone, Default)]
pub struct ListMessagesQuery {
    /// Snowflake ID upper bound (exclusive).
    pub before: Option<u64>,
    /// Snowflake ID lower bound (exclusive).
    pub after: Option<u64>,
    pub limit: Option<u32>,
    /// Repeated `field_name=` filters.
    pub field_names: Vec<String>,
}

/// Arguments for [`DataClient::ping_connection_at`] (pydoover
/// `ProcessorDataClient.ping_connection_at`).
#[derive(Debug, Clone)]
pub struct PingConnectionArgs {
    /// ms since epoch the agent was last known online.
    pub online_at_ms: u64,
    pub connection_status: ConnectionStatus,
    pub determination: ConnectionDetermination,
    /// Defaults to now.
    pub ping_at_ms: Option<u64>,
    pub user_agent: Option<String>,
    /// Divergence: pydoover looks the public IP up via
    /// checkip.amazonaws.com when unset; doover-rs sends `null` instead
    /// (TODO if the dashboards ever need it).
    pub ip_address: Option<String>,
    pub agent_id: Option<u64>,
}

#[derive(Default)]
struct ClientState {
    agent_id: Option<u64>,
    organisation_id: Option<String>,
    app_key: Option<String>,
    /// For `on_message_create`/`on_aggregate_update` invocations: the channel
    /// that triggered this invocation, so we don't publish back to it and
    /// recurse (pydoover `_invoking_channel_name`).
    invoking_channel: Option<String>,
}

/// Async Doover data API client. Cheap to share behind an [`std::sync::Arc`];
/// the underlying `reqwest::Client` pools connections and should be reused
/// across warm Lambda invocations (pass it via [`DataClient::with_client`]).
pub struct DataClient {
    http: reqwest::Client,
    base_url: String,
    auth: BearerAuth,
    state: Mutex<ClientState>,
    /// Attempts per request (pydoover default 3).
    pub max_retries: u32,
    /// Base backoff delay (pydoover default 1s; doubled per attempt).
    pub retry_delay: Duration,
    /// gzip-compress JSON request bodies ≥ 50 bytes (pydoover default on).
    pub compress: bool,
}

impl DataClient {
    /// Client against `DOOVER_DATA_ENDPOINT` or the production default.
    pub fn new() -> Self {
        let base = std::env::var(DATA_ENDPOINT_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_DATA_ENDPOINT.to_string());
        Self::with_client(reqwest::Client::new(), base)
    }

    /// Client with an explicit base URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_client(reqwest::Client::new(), base_url)
    }

    /// Client reusing an existing `reqwest::Client` (connection pool).
    pub fn with_client(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http,
            base_url,
            auth: BearerAuth::default(),
            state: Mutex::new(ClientState::default()),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            compress: true,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn set_token(&self, token: impl Into<String>) {
        self.auth.set_token(token);
    }

    pub fn token(&self) -> Option<String> {
        self.auth.token()
    }

    pub fn set_agent_id(&self, agent_id: u64) {
        self.state.lock().unwrap().agent_id = Some(agent_id);
    }

    pub fn agent_id(&self) -> Option<u64> {
        self.state.lock().unwrap().agent_id
    }

    /// Sets the `X-Doover-Organisation` header for subsequent requests.
    pub fn set_organisation_id(&self, organisation_id: impl Into<String>) {
        self.state.lock().unwrap().organisation_id = Some(organisation_id.into());
    }

    pub fn set_app_key(&self, app_key: impl Into<String>) {
        self.state.lock().unwrap().app_key = Some(app_key.into());
    }

    pub fn app_key(&self) -> Option<String> {
        self.state.lock().unwrap().app_key.clone()
    }

    /// Arm the anti-recursion guard for this invocation's trigger channel.
    pub fn set_invoking_channel(&self, channel: Option<String>) {
        self.state.lock().unwrap().invoking_channel = channel;
    }

    fn resolve_agent_id(&self, agent_id: Option<u64>) -> Result<u64> {
        agent_id.or_else(|| self.agent_id()).ok_or_else(|| {
            DooverError::Other(
                "agent_id must be provided either as a method argument or set on the client"
                    .into(),
            )
        })
    }

    /// pydoover `ProcessorDataClient._check_invoking_channel`.
    fn check_invoking_channel(&self, channel: &str, data: &Value) -> Result<()> {
        let (invoking, app_key) = {
            let st = self.state.lock().unwrap();
            (st.invoking_channel.clone(), st.app_key.clone())
        };
        if invoking.as_deref() != Some(channel) {
            return Ok(());
        }
        if channel != crate::tags::TAG_CHANNEL_NAME {
            return Err(DooverError::Other("Cannot publish to the invoking channel.".into()));
        }
        let outside_scope = data
            .as_object()
            .is_some_and(|m| m.keys().any(|k| Some(k.as_str()) != app_key.as_deref()));
        if outside_scope {
            return Err(DooverError::Other(
                "Cannot publish to tag_values outside the scope of this app without explicit enable."
                    .into(),
            ));
        }
        Ok(())
    }

    // -- Core request ------------------------------------------------------

    /// One HTTP round trip with retries; `Ok(None)` for empty response
    /// bodies. `query` pairs are appended as-is (repeat a key for list
    /// params).
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        query: &[(&str, String)],
    ) -> Result<Option<Value>> {
        let url = format!("{}{}", self.base_url, path);

        // Compress the JSON body once (not per retry), like pydoover.
        let mut plain_body: Option<Value> = None;
        let mut gzip_body: Option<Vec<u8>> = None;
        if let Some(data) = body {
            let raw = serde_json::to_vec(data)?;
            if self.compress && raw.len() >= MIN_COMPRESS_SIZE {
                let mut enc = flate2::write::GzEncoder::new(
                    Vec::new(),
                    flate2::Compression::new(GZIP_LEVEL),
                );
                enc.write_all(&raw)
                    .and_then(|_| enc.finish())
                    .map(|out| gzip_body = Some(out))
                    .map_err(|e| DooverError::Other(format!("gzip encode failed: {e}")))?;
            } else {
                plain_body = Some(data.clone());
            }
        }

        let mut last_err: Option<DooverError> = None;
        for attempt in 0..self.max_retries {
            let mut req = self.http.request(method.clone(), &url).query(query);
            if let Some(authorization) = self.auth.authorization() {
                req = req.header(reqwest::header::AUTHORIZATION, authorization);
            }
            if let Some(org) = self.state.lock().unwrap().organisation_id.clone() {
                req = req.header("X-Doover-Organisation", org);
            }
            if let Some(gz) = &gzip_body {
                req = req
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::CONTENT_ENCODING, "gzip")
                    .body(gz.clone());
            } else if let Some(data) = &plain_body {
                req = req.json(data);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status >= 500 {
                        let text = resp.text().await.unwrap_or_default();
                        tracing::info!(
                            "server error {status} on {method} {url}: {} attempt={}/{}",
                            text.chars().take(200).collect::<String>(),
                            attempt + 1,
                            self.max_retries
                        );
                        last_err = Some(map_status(status, text));
                    } else if status >= 400 {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(map_status(status, text));
                    } else {
                        let bytes = resp.bytes().await?;
                        if bytes.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some(serde_json::from_slice(&bytes)?));
                    }
                }
                Err(e) => {
                    tracing::info!(
                        "client error on {method} {url}: {e} attempt={}/{}",
                        attempt + 1,
                        self.max_retries
                    );
                    last_err = Some(e.into());
                }
            }

            if attempt + 1 < self.max_retries {
                let delay = self.retry_delay * 2u32.pow(attempt);
                tracing::info!("retrying {method} {url} in {delay:?}...");
                tokio::time::sleep(delay).await;
            }
        }
        Err(last_err
            .unwrap_or_else(|| DooverError::Other(format!("request to {url} failed"))))
    }

    // -- Channels ----------------------------------------------------------

    /// `GET /agents/{agent_id}/channels/{name}` (aggregate included).
    pub async fn fetch_channel(&self, channel_name: &str) -> Result<Channel> {
        self.fetch_channel_with(channel_name, true, None).await
    }

    pub async fn fetch_channel_with(
        &self,
        channel_name: &str,
        include_aggregate: bool,
        agent_id: Option<u64>,
    ) -> Result<Channel> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        let raw = self
            .request(
                reqwest::Method::GET,
                &format!("/agents/{agent_id}/channels/{channel_name}"),
                None,
                &[("include_aggregate", bool_param(include_aggregate))],
            )
            .await?
            .ok_or_else(|| DooverError::InvalidPayload("empty channel response".into()))?;
        Channel::from_value(raw)
    }

    // -- Aggregates ---------------------------------------------------------

    /// `GET /agents/{agent_id}/channels/{name}/aggregate` — the full
    /// aggregate payload (`{"data": …, "attachments": …, "last_updated": …}`).
    pub async fn fetch_channel_aggregate_raw(
        &self,
        channel_name: &str,
        agent_id: Option<u64>,
    ) -> Result<Value> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        self.request(
            reqwest::Method::GET,
            &format!("/agents/{agent_id}/channels/{channel_name}/aggregate"),
            None,
            &[],
        )
        .await?
        .ok_or_else(|| DooverError::InvalidPayload("empty aggregate response".into()))
    }

    /// `PATCH`/`PUT /agents/{agent_id}/channels/{name}/aggregate`.
    /// `opts.save_log` → `?log_update=true`, `opts.replace_keys` →
    /// `?replace=<key>` (repeated), `opts.replace_data` → `PUT`.
    /// `opts.max_age_secs` has no HTTP analogue (writes are immediate).
    pub async fn update_channel_aggregate_http(
        &self,
        channel_name: &str,
        data: &Value,
        opts: &AggregateOptions,
        agent_id: Option<u64>,
    ) -> Result<Option<Value>> {
        self.check_invoking_channel(channel_name, data)?;
        let agent_id = self.resolve_agent_id(agent_id)?;
        let method = if opts.replace_data { reqwest::Method::PUT } else { reqwest::Method::PATCH };
        // pydoover filters falsy params out entirely.
        let mut query: Vec<(&str, String)> = Vec::new();
        if opts.clear_attachments {
            query.push(("clear_attachments", bool_param(true)));
        }
        if opts.save_log {
            query.push(("log_update", bool_param(true)));
        }
        for key in &opts.replace_keys {
            query.push(("replace", key.clone()));
        }
        self.request(
            method,
            &format!("/agents/{agent_id}/channels/{channel_name}/aggregate"),
            Some(data),
            &query,
        )
        .await
    }

    // -- Messages ------------------------------------------------------------

    /// `POST /agents/{agent_id}/channels/{name}/messages` — returns the
    /// created message payload.
    pub async fn create_message_http(
        &self,
        channel_name: &str,
        data: &Value,
        timestamp_ms: Option<u64>,
        agent_id: Option<u64>,
    ) -> Result<Value> {
        self.check_invoking_channel(channel_name, data)?;
        let agent_id = self.resolve_agent_id(agent_id)?;
        let mut payload = Map::new();
        payload.insert("data".into(), data.clone());
        if let Some(ts) = timestamp_ms {
            payload.insert("ts".into(), json!(ts));
        }
        self.request(
            reqwest::Method::POST,
            &format!("/agents/{agent_id}/channels/{channel_name}/messages"),
            Some(&Value::Object(payload)),
            &[],
        )
        .await?
        .ok_or_else(|| DooverError::InvalidPayload("empty create_message response".into()))
    }

    /// `GET /agents/{agent_id}/channels/{name}/messages/{id}`.
    pub async fn fetch_message(
        &self,
        channel_name: &str,
        message_id: u64,
        agent_id: Option<u64>,
    ) -> Result<Value> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        self.request(
            reqwest::Method::GET,
            &format!("/agents/{agent_id}/channels/{channel_name}/messages/{message_id}"),
            None,
            &[],
        )
        .await?
        .ok_or_else(|| DooverError::InvalidPayload("empty message response".into()))
    }

    /// `GET /agents/{agent_id}/channels/{name}/messages`.
    pub async fn list_messages(
        &self,
        channel_name: &str,
        query: &ListMessagesQuery,
        agent_id: Option<u64>,
    ) -> Result<Vec<Value>> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(before) = query.before {
            params.push(("before", before.to_string()));
        }
        if let Some(after) = query.after {
            params.push(("after", after.to_string()));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        for name in &query.field_names {
            params.push(("field_name", name.clone()));
        }
        let data = self
            .request(
                reqwest::Method::GET,
                &format!("/agents/{agent_id}/channels/{channel_name}/messages"),
                None,
                &params,
            )
            .await?
            .unwrap_or(Value::Array(vec![]));
        match data {
            Value::Array(items) => Ok(items),
            other => Err(DooverError::InvalidPayload(format!(
                "expected message list, got: {other}"
            ))),
        }
    }

    /// `PATCH`/`PUT /agents/{agent_id}/channels/{name}/messages/{id}`.
    pub async fn update_message_http(
        &self,
        channel_name: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
        agent_id: Option<u64>,
    ) -> Result<Option<Value>> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        let method = if opts.replace_data { reqwest::Method::PUT } else { reqwest::Method::PATCH };
        let mut query: Vec<(&str, String)> = Vec::new();
        if opts.clear_attachments {
            query.push(("clear_attachments", bool_param(true)));
        }
        self.request(
            method,
            &format!("/agents/{agent_id}/channels/{channel_name}/messages/{message_id}"),
            Some(&json!({ "data": data })),
            &query,
        )
        .await
    }

    // -- Processor token upgrade ---------------------------------------------

    /// `GET /processors/subscriptions/{subscription_id}` — trade the initial
    /// event JWT for the full processor token + seeded channels.
    pub async fn fetch_subscription_info(&self, subscription_id: &str) -> Result<SubscriptionInfo> {
        let data = self
            .request(
                reqwest::Method::GET,
                &format!("/processors/subscriptions/{subscription_id}"),
                None,
                &[],
            )
            .await?
            .ok_or_else(|| DooverError::InvalidPayload("empty subscription info".into()))?;
        SubscriptionInfo::from_value(&data)
    }

    /// `GET /processors/schedules/{schedule_id}`.
    pub async fn fetch_schedule_info(&self, schedule_id: &str) -> Result<SubscriptionInfo> {
        let data = self
            .request(
                reqwest::Method::GET,
                &format!("/processors/schedules/{schedule_id}"),
                None,
                &[],
            )
            .await?
            .ok_or_else(|| DooverError::InvalidPayload("empty schedule info".into()))?;
        SubscriptionInfo::from_value(&data)
    }

    // -- Connection ----------------------------------------------------------

    /// Publish a connection ping: a `doover_connection` message *and*
    /// aggregate write with the same payload (pydoover
    /// `ProcessorDataClient.ping_connection_at`).
    pub async fn ping_connection_at(&self, args: &PingConnectionArgs) -> Result<()> {
        let ping_at = args.ping_at_ms.unwrap_or_else(now_ms);
        let user_agent =
            args.user_agent.clone().unwrap_or_else(|| "doover-rs-processor".to_string());
        let payload = json!({
            "status": {
                "status": args.connection_status.as_str(),
                "last_online": args.online_at_ms,
                "last_ping": ping_at,
                "user_agent": user_agent,
                "ip": args.ip_address,
            },
            "determination": args.determination.as_str(),
        });
        self.create_message_http("doover_connection", &payload, None, args.agent_id).await?;
        self.update_channel_aggregate_http(
            "doover_connection",
            &payload,
            &AggregateOptions::default(),
            args.agent_id,
        )
        .await?;
        Ok(())
    }

    /// Write a connection config (`{"config": …}`) as a `doover_connection`
    /// message + aggregate (pydoover `update_connection_config`).
    pub async fn update_connection_config(
        &self,
        config: &Value,
        agent_id: Option<u64>,
    ) -> Result<()> {
        let payload = json!({ "config": config });
        self.create_message_http("doover_connection", &payload, None, agent_id).await?;
        self.update_channel_aggregate_http(
            "doover_connection",
            &payload,
            &AggregateOptions::default(),
            agent_id,
        )
        .await?;
        Ok(())
    }

    // -- Notifications ---------------------------------------------------------

    /// Publish a [`Notification`] to the agent's `notifications` channel; the
    /// cloud fans it out to matching subscriptions.
    pub async fn send_notification(
        &self,
        notification: impl Into<Notification>,
        agent_id: Option<u64>,
    ) -> Result<Value> {
        self.create_message_http(
            NOTIFICATIONS_CHANNEL,
            &notification.into().to_json(),
            None,
            agent_id,
        )
        .await
    }
}

impl Default for DataClient {
    fn default() -> Self {
        Self::new()
    }
}

/// pydoover `_build_query` renders bools as `"true"`/`"false"`.
fn bool_param(v: bool) -> String {
    if v { "true".into() } else { "false".into() }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// pydoover `_raise_for_status`.
fn map_status(status: u16, text: String) -> DooverError {
    if status == 404 {
        DooverError::NotFound(text)
    } else {
        DooverError::Http { code: status as i32, message: text }
    }
}

/// The cloud HTTP client is a [`ChannelBackend`] without a persistent
/// connection — the same trait the docker gRPC client implements, so
/// tags/RPC/UI managers run over either transport (pydoover shares them by
/// duck typing).
#[async_trait::async_trait]
impl ChannelBackend for DataClient {
    async fn fetch_channel_aggregate(&self, channel: &str) -> Result<Option<Value>> {
        match self.fetch_channel_aggregate_raw(channel, None).await {
            Ok(payload) => Ok(payload.get("data").cloned()),
            Err(DooverError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn update_channel_aggregate(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()> {
        // max_age_secs has no meaning over HTTP (no local agent buffer).
        self.update_channel_aggregate_http(channel, data, opts, None).await?;
        Ok(())
    }

    async fn create_message(&self, channel: &str, data: &Value) -> Result<u64> {
        let created = self.create_message_http(channel, data, None, None).await?;
        Ok(created.get("id").and_then(value_as_id).unwrap_or_default())
    }

    async fn update_message(
        &self,
        channel: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
    ) -> Result<()> {
        self.update_message_http(channel, message_id, data, opts, None).await?;
        Ok(())
    }

    fn has_persistent_connection(&self) -> bool {
        false
    }
}
