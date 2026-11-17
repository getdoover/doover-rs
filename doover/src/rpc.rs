//! RPC over Doover channels — the port of `pydoover/rpc.py`.
//!
//! Request/response communication between applications using channel messages
//! as the transport. A request is a message on a channel (default `dv-rpc`)
//! with the payload
//! `{"type": "rpc", "method": …, "request": …, "status": {"code": "sent"},
//! "response": {}}`; the responder updates the *same message* with a final
//! status (`success` / `error`), optionally passing through intermediate
//! `acknowledged` / `deferred` statuses. The caller resolves its pending
//! future from the `MessageUpdate` event.
//!
//! [`RpcManager`] is transport-agnostic: it writes through a
//! [`ChannelBackend`] and receives events via [`RpcManager::handle_event`],
//! which the docker runtime wires to the
//! [`SubscriptionHub`](crate::SubscriptionHub) (see
//! [`wire_rpc`](crate::docker::application::wire_rpc)).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use tokio::sync::{oneshot, Notify};

use crate::channel_backend::{ChannelBackend, UpdateMessageOptions};
use crate::error::{DooverError, Result};
use crate::events::Event;
use crate::utils::unix_millis_from_snowflake;

/// The default RPC channel (pydoover `DEFAULT_CHANNEL` / `RPC_KEY`).
pub const RPC_CHANNEL: &str = "dv-rpc";

/// Default `call` timeout (pydoover: 30 s).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// An error returned by (or to) an RPC handler (pydoover `RPCError`).
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    /// The error a cancelled command unwinds with (pydoover `RPCCancelled`).
    pub fn cancelled(method: &str) -> Self {
        Self {
            code: "CANCELLED".to_string(),
            message: format!("RPC call '{method}' was cancelled by the caller"),
        }
    }

    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

impl From<RpcError> for DooverError {
    fn from(e: RpcError) -> Self {
        DooverError::Other(e.to_string())
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The absolute expiry (unix ms) of an RPC command, or `None` if it never
/// expires — pydoover `command_expires_at`.
///
/// Derived from the request message's creation time plus its `expires_after`
/// (milliseconds). The creation time comes out of the snowflake id, exactly as
/// pydoover's `Message.timestamp` does.
pub fn command_expires_at(message_id: u64, data: &Value) -> Option<i64> {
    let expires_after = data.get("expires_after")?.as_f64()?;
    Some(unix_millis_from_snowflake(message_id) as i64 + expires_after as i64)
}

/// Whether an RPC command's expiry has passed — pydoover
/// `command_is_expired`. A command with no `expires_after` never expires.
pub fn command_is_expired(message_id: u64, data: &Value) -> bool {
    match command_expires_at(message_id, data) {
        Some(expires_at) => now_unix_ms() >= expires_at,
        None => false,
    }
}

/// Whether a command `status` block marks the command as cancelled (pydoover
/// `status_is_cancelled`).
///
/// There is no dedicated status code: the site's cancel patches the command
/// message to a terminal `error` carrying a marker in its `message` body
/// (customer-site `handleCancelCommand`):
///
/// ```json
/// {"status": {"code": "error",
///             "message": {"info": "Command cancelled",
///                         "cancelled_at": 1787812081613,
///                         "cancelled_by": {}}}}
/// ```
///
/// So a cancellation is a *kind of* error rather than a status of its own.
/// The checks below mirror the site's own reader (`isCancelledStatus`),
/// including its tolerance of the single-l spelling and of a bare string
/// message from older writers — if the two ever disagree, the site's version
/// is canonical.
pub fn status_is_cancelled(status: &Value) -> bool {
    fn is_cancelled_text(s: &str) -> bool {
        let s = s.trim().to_ascii_lowercase();
        s == "command cancelled" || s == "command canceled"
    }

    if status.get("code").and_then(Value::as_str) != Some("error") {
        return false;
    }
    match status.get("message") {
        Some(Value::String(text)) => is_cancelled_text(text),
        Some(Value::Object(message)) => {
            if message.contains_key("cancelled_at") || message.contains_key("cancelled_by") {
                return true;
            }
            message
                .get("info")
                .and_then(Value::as_str)
                .is_some_and(is_cancelled_text)
        }
        _ => false,
    }
}

/// Whether an RPC command's message has been cancelled by its issuer
/// (pydoover `command_is_cancelled`).
pub fn command_is_cancelled(data: &Value) -> bool {
    data.get("status").is_some_and(status_is_cancelled)
}

/// Context handed to an RPC handler (pydoover `RPCContext`): identifies the
/// request message and lets the handler send intermediate statuses.
#[derive(Clone)]
pub struct RpcContext {
    pub method: String,
    pub channel: String,
    /// `None` for one-shot requests, which cannot be responded to.
    pub message_id: Option<u64>,
    /// The request message's id, set for one-shots too (unlike
    /// [`message_id`](Self::message_id), which gates responding).
    pub request_message_id: Option<u64>,
    /// The whole request message payload, backing the audit accessors below
    /// (pydoover reads these off `RPCContext.message.data`).
    pub data: Value,
    backend: Arc<dyn ChannelBackend>,
    /// Set when the issuer withdraws this command while the handler is still
    /// running, along with the cancelling update's `status.message` body
    /// (which carries who cancelled it and when).
    cancelled: Arc<Mutex<Option<Value>>>,
    /// Fires once on cancellation, for [`RpcContext::wait_cancelled`].
    cancel_tx: Arc<Notify>,
}

impl RpcContext {
    /// Audit info (`{"id", "name", "email"}`) of who issued the command, if
    /// any — pydoover `RPCContext.actor`.
    pub fn actor(&self) -> Option<&Value> {
        self.data.get("actor")
    }

    /// The audit reason supplied with this command, if any.
    pub fn reason(&self) -> Option<&str> {
        self.data.get("reason")?.as_str()
    }

    /// The value observed before this command was issued, if provided.
    pub fn old_value(&self) -> Option<&Value> {
        self.data.get("old_value")
    }

    /// Message id of the shorter-lived command this retry replaces, if any.
    /// pydoover types this as a string, so it is returned verbatim.
    pub fn retry_of(&self) -> Option<&str> {
        self.data.get("retry_of")?.as_str()
    }

    /// Command lifetime from its creation timestamp, if set.
    pub fn expires_after(&self) -> Option<Duration> {
        let ms = self.data.get("expires_after")?.as_f64()?;
        Some(Duration::from_millis(ms.max(0.0) as u64))
    }

    /// The absolute time (unix ms) this command expires, or `None` if it never
    /// does / the request has no id to date it from.
    pub fn expires_at(&self) -> Option<i64> {
        command_expires_at(self.request_message_id?, &self.data)
    }

    /// Whether this command's expiry has passed. Always `false` for a command
    /// with no `expires_after`.
    pub fn is_expired(&self) -> bool {
        match self.request_message_id {
            Some(id) => command_is_expired(id, &self.data),
            None => false,
        }
    }

    // -- cancellation ----------------------------------------------------
    //
    // A long-running handler — a pump pre-start warning, a panel reboot, a
    // firmware push — outlives the operator's patience, so the site lets them
    // withdraw a command that is still in flight. That arrives as an update to
    // the command's own message, which the manager routes here.
    //
    // Cancellation is cooperative: nothing interrupts the handler. A handler
    // that ignores it behaves exactly as it does today. This is deliberate —
    // killing a half-finished sequence mid-step (part-way through putting a
    // panel into Auto, say) is rarely safer than letting it decide where it
    // can safely stop.

    /// Whether the issuer has withdrawn this command since it started.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.lock().unwrap().is_some()
    }

    /// Block until this command is cancelled — for handlers waiting on
    /// something else anyway (`tokio::select!` over this and the real work).
    pub async fn wait_cancelled(&self) {
        loop {
            // Register before testing the flag: a cancellation landing between
            // the two would otherwise be missed and this would wait forever.
            let notified = self.cancel_tx.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// `Err(RpcError)` with pydoover's `CANCELLED` code if the command has
    /// been cancelled — for handlers that step through phases and want to bail
    /// at each boundary (pydoover `raise_if_cancelled`).
    pub fn error_if_cancelled(&self) -> std::result::Result<(), RpcError> {
        if self.is_cancelled() {
            return Err(RpcError::cancelled(&self.method));
        }
        Ok(())
    }

    /// When the command was cancelled, in unix milliseconds, if it was and the
    /// issuer said so.
    ///
    /// The timestamp is whatever the canceller wrote, so a nonsensical value
    /// yields `None`: a handler reading this for an audit line must not be
    /// able to blow up over a malformed field.
    pub fn cancelled_at(&self) -> Option<i64> {
        let guard = self.cancelled.lock().unwrap();
        let raw = guard.as_ref()?.get("cancelled_at")?;
        raw.as_i64().or_else(|| raw.as_f64().map(|f| f as i64))
    }

    /// Audit info for whoever cancelled the command, if the issuer said so.
    pub fn cancelled_by(&self) -> Option<Value> {
        self.cancelled.lock().unwrap().as_ref()?.get("cancelled_by").cloned()
    }

    fn mark_cancelled(&self, status: &Value) {
        let body = match status.get("message") {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => json!({}),
        };
        *self.cancelled.lock().unwrap() = Some(body);
        self.cancel_tx.notify_waiters();
    }

    /// Whether it is still our place to write to this command's message
    /// (pydoover `_may_write`).
    ///
    /// Once cancelled, the command carries the canceller's terminal status and
    /// the site renders it from that. A later `progress` would put it back to
    /// `pending` and quietly un-cancel it in the UI — so every status write
    /// stops here, not just the terminal one.
    fn may_write(&self) -> bool {
        !self.is_cancelled()
    }

    /// Report intermediate progress on a command that is still running
    /// (pydoover `RPCContext.progress`).
    ///
    /// Handlers that take a while should call this as they move between
    /// phases, so the operator watches the sequence advance instead of a bare
    /// spinner. The `pending` status is non-terminal: the command stays in
    /// flight and the site keeps the control locked until the handler returns.
    ///
    /// `text` is the one-line summary shown beside the spinner; `fields` ride
    /// alongside it for consumers that want structure rather than prose.
    ///
    /// Reporting progress also tells the site the device is alive, so the
    /// longer `command_pending_timeout` governs from the first report onwards,
    /// in place of the short "no response from device" window. Call it at
    /// least that often.
    pub async fn progress(&self, text: Option<&str>, fields: Value) -> Result<()> {
        let Some(id) = self.message_id else {
            return Err(DooverError::Other(
                "cannot report progress on a one-shot rpc request".into(),
            ));
        };
        if !self.may_write() {
            return Ok(());
        }
        let mut message = match fields {
            Value::Object(m) => m,
            Value::Null => serde_json::Map::new(),
            other => {
                return Err(DooverError::Other(format!(
                    "progress fields must be a JSON object, got {other}"
                )))
            }
        };
        if let Some(text) = text {
            message.insert("text".to_string(), json!(text));
        }
        let payload = json!({"status": {"code": "pending", "message": message}});
        self.backend
            .update_message(
                &self.channel,
                id,
                &payload,
                &UpdateMessageOptions::default(),
            )
            .await
    }

    /// Mark the request as received but not yet complete — pydoover
    /// `RPCContext.acknowledge`, ms timestamp included.
    pub async fn acknowledge(&self) -> Result<()> {
        let Some(id) = self.message_id else {
            return Err(DooverError::Other("cannot acknowledge a one-shot rpc request".into()));
        };
        if !self.may_write() {
            return Ok(());
        }
        let payload = json!({
            "status": {
                "code": "acknowledged",
                "message": {"timestamp": now_unix_ms()},
            }
        });
        self.backend
            .update_message(&self.channel, id, &payload, &UpdateMessageOptions::default())
            .await
    }

    /// Tell the caller to expect the result in ~`seconds` — pydoover
    /// `RPCContext.defer` (`until`/`at` ms timestamps).
    pub async fn defer(&self, seconds: f64) -> Result<()> {
        let Some(id) = self.message_id else {
            return Err(DooverError::Other("cannot defer a one-shot rpc request".into()));
        };
        if !self.may_write() {
            return Ok(());
        }
        let now = now_unix_ms();
        let payload = json!({
            "status": {
                "code": "deferred",
                "message": {
                    "until": now + (seconds * 1000.0) as i64,
                    "at": now,
                },
            }
        });
        self.backend
            .update_message(&self.channel, id, &payload, &UpdateMessageOptions::default())
            .await
    }
}

/// Options for an outgoing RPC request — the keyword arguments of pydoover's
/// `RPCManager.call`.
///
/// [`Default`] targets the [`RPC_CHANNEL`] with no app key, no audit metadata
/// and the [`DEFAULT_CALL_TIMEOUT`], i.e. the bare `call(method, params)`.
#[derive(Debug, Clone)]
pub struct CallOptions {
    /// Channel to send the request on.
    pub channel: String,
    /// Target a specific app on the receiving agent.
    pub app_key: Option<String>,
    /// Response deadline; `None` uses [`DEFAULT_CALL_TIMEOUT`]. Irrelevant to
    /// [`send_with`](RpcManager::send_with), which never waits.
    pub timeout: Option<Duration>,
    /// Audit info (`{"id", "name", "email"}`) of who issued the command.
    pub actor: Option<Value>,
    /// An audit reason recorded alongside the command.
    pub reason: Option<String>,
    /// The value observed before this command was issued. `Some(Value::Null)`
    /// sends an explicit `null`; `None` omits the key — pydoover distinguishes
    /// the two with its `NOT_GIVEN` sentinel.
    pub old_value: Option<Value>,
    /// Command lifetime from its creation timestamp. A receiver refuses to act
    /// on the command once `create time + expires_after` has passed.
    pub expires_after: Option<Duration>,
    /// Message id of the shorter-lived command this retry replaces.
    pub retry_of: Option<String>,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            channel: RPC_CHANNEL.to_string(),
            app_key: None,
            timeout: None,
            actor: None,
            reason: None,
            old_value: None,
            expires_after: None,
            retry_of: None,
        }
    }
}

impl CallOptions {
    /// Options targeting `channel` (e.g. `ui_cmds`), everything else default.
    pub fn on(channel: impl Into<String>) -> Self {
        Self { channel: channel.into(), ..Default::default() }
    }

    pub fn app_key(mut self, app_key: impl Into<String>) -> Self {
        self.app_key = Some(app_key.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn actor(mut self, actor: Value) -> Self {
        self.actor = Some(actor);
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn old_value(mut self, old_value: Value) -> Self {
        self.old_value = Some(old_value);
        self
    }

    pub fn expires_after(mut self, expires_after: Duration) -> Self {
        self.expires_after = Some(expires_after);
        self
    }

    pub fn retry_of(mut self, retry_of: impl Into<String>) -> Self {
        self.retry_of = Some(retry_of.into());
        self
    }

    /// The exact pydoover request payload (and key order): `type, method,
    /// request, status, response[, app_key][, actor][, reason][, old_value]
    /// [, expires_after][, retry_of]`.
    fn request_payload(&self, method: &str, params: Option<Value>) -> Value {
        let mut data = Map::new();
        data.insert("type".into(), json!("rpc"));
        data.insert("method".into(), json!(method));
        data.insert("request".into(), params.unwrap_or_else(|| json!({})));
        data.insert("status".into(), json!({"code": "sent"}));
        data.insert("response".into(), json!({}));
        if let Some(app_key) = &self.app_key {
            data.insert("app_key".into(), json!(app_key));
        }
        if let Some(actor) = &self.actor {
            data.insert("actor".into(), actor.clone());
        }
        if let Some(reason) = &self.reason {
            data.insert("reason".into(), json!(reason));
        }
        if let Some(old_value) = &self.old_value {
            data.insert("old_value".into(), old_value.clone());
        }
        if let Some(expires_after) = self.expires_after {
            data.insert("expires_after".into(), json!(expires_after.as_millis() as i64));
        }
        if let Some(retry_of) = &self.retry_of {
            data.insert("retry_of".into(), json!(retry_of));
        }
        Value::Object(data)
    }
}

/// A registered handler: takes the context and the `request` payload,
/// returns the `response` payload (or a typed error sent back to the caller).
pub type RpcHandler =
    Arc<dyn Fn(RpcContext, Value) -> BoxFuture<'static, std::result::Result<Value, RpcError>>
        + Send
        + Sync>;

type PendingSender = oneshot::Sender<std::result::Result<Value, RpcError>>;
type SubscribeFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct RpcState {
    /// `(channel, method)` → handler; a `None` channel matches any channel
    /// (pydoover's global handlers).
    handlers: HashMap<(Option<String>, String), RpcHandler>,
    pending: HashMap<u64, PendingSender>,
    /// Inbound commands we are currently serving, by message id, so an update
    /// to one (notably a cancellation) can be routed to the running handler.
    /// Always removed again once the handler returns: a leak would pin every
    /// command's context for the life of the app.
    inflight: HashMap<u64, RpcContext>,
    subscribed: HashSet<String>,
    /// Installed by the runtime; called once per newly-needed channel so the
    /// transport can route that channel's events into [`RpcManager::handle_event`].
    subscriber: Option<SubscribeFn>,
}

/// Orchestrates RPC over channel messages (pydoover `RPCManager`).
pub struct RpcManager {
    backend: Arc<dyn ChannelBackend>,
    /// Used to reject incoming requests stamped with a different app's key.
    app_key: Option<String>,
    state: Mutex<RpcState>,
}

impl RpcManager {
    pub fn new(backend: Arc<dyn ChannelBackend>, app_key: Option<String>) -> Self {
        Self { backend, app_key, state: Mutex::new(RpcState::default()) }
    }

    /// Install the channel-subscription hook (the docker runtime passes a
    /// closure that subscribes the [`SubscriptionHub`](crate::SubscriptionHub)
    /// and forwards events to [`handle_event`](Self::handle_event)). Channels
    /// already requested are (re)announced to the new subscriber.
    pub fn set_subscriber(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        let already: Vec<String> = {
            let mut st = self.state.lock().unwrap();
            st.subscriber = Some(Arc::new(f));
            st.subscribed.iter().cloned().collect()
        };
        for channel in already {
            self.notify_subscriber(&channel);
        }
    }

    fn notify_subscriber(&self, channel: &str) {
        let subscriber = self.state.lock().unwrap().subscriber.clone();
        if let Some(f) = subscriber {
            f(channel);
        }
    }

    /// Subscribe to RPC events on a channel (idempotent). Without a
    /// persistent connection this is a no-op, like pydoover's processor path.
    pub fn subscribe(&self, channel: &str) {
        if !self.backend.has_persistent_connection() {
            return;
        }
        let newly = self.state.lock().unwrap().subscribed.insert(channel.to_string());
        if newly {
            self.notify_subscriber(channel);
            tracing::info!("RPC subscribed to channel: {channel}");
        }
    }

    /// Register a boxed handler for `method` (optionally restricted to a
    /// channel; `None` matches requests on any subscribed channel). A `Some`
    /// channel is auto-subscribed, mirroring pydoover `register_handlers`.
    pub fn register_handler(&self, channel: Option<&str>, method: &str, handler: RpcHandler) {
        tracing::info!("registering RPC handler: {method} (channel={channel:?})");
        self.state
            .lock()
            .unwrap()
            .handlers
            .insert((channel.map(str::to_string), method.to_string()), handler);
        if let Some(channel) = channel {
            self.subscribe(channel);
        }
    }

    /// Register an async closure as a handler — the ergonomic form of
    /// [`register_handler`](Self::register_handler).
    pub fn register<F, Fut>(&self, channel: Option<&str>, method: &str, f: F)
    where
        F: Fn(RpcContext, Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Value, RpcError>> + Send + 'static,
    {
        self.register_handler(channel, method, Arc::new(move |ctx, v| Box::pin(f(ctx, v))));
    }

    // -- caller side ---------------------------------------------------------

    /// Make an RPC call and wait for the response (pydoover `RPCManager.call`).
    ///
    /// `app_key` optionally targets a specific app on the receiving agent;
    /// `timeout` defaults to [`DEFAULT_CALL_TIMEOUT`] when `None`. For the
    /// audit / expiry metadata, use [`call_with`](Self::call_with).
    pub async fn call(
        &self,
        method: &str,
        params: Option<Value>,
        channel: &str,
        app_key: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let opts = CallOptions {
            channel: channel.to_string(),
            app_key: app_key.map(str::to_string),
            timeout,
            ..Default::default()
        };
        self.call_with(method, params, &opts).await
    }

    /// Send an RPC request and return its message id *without* waiting for the
    /// response — pydoover's `call(..., wait_for_response=False)`.
    ///
    /// The request is a normal one the handler still processes and replies to;
    /// this side simply never registers or awaits the reply. Use it when the
    /// caller doesn't care about the outcome and must not block on it. Unlike
    /// [`fire_and_forget`](Self::fire_and_forget) this keeps `call`'s field
    /// order and carries the audit metadata.
    pub async fn send_with(
        &self,
        method: &str,
        params: Option<Value>,
        opts: &CallOptions,
    ) -> Result<u64> {
        // No response subscription needed — nothing is going to await one.
        let data = opts.request_payload(method, params);
        self.backend.create_message(&opts.channel, &data).await
    }

    /// Make an RPC call with the full pydoover option set (audit actor/reason,
    /// `old_value`, command expiry, `retry_of`) and wait for the response.
    pub async fn call_with(
        &self,
        method: &str,
        params: Option<Value>,
        opts: &CallOptions,
    ) -> Result<Value> {
        let channel = opts.channel.as_str();
        self.subscribe(channel);

        let data = opts.request_payload(method, params);
        let message_id = self.backend.create_message(channel, &data).await?;
        let (tx, rx) = oneshot::channel();
        self.state.lock().unwrap().pending.insert(message_id, tx);

        let timeout = opts.timeout.unwrap_or(DEFAULT_CALL_TIMEOUT);
        let result = tokio::time::timeout(timeout, rx).await;
        self.state.lock().unwrap().pending.remove(&message_id);
        match result {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(e))) => Err(DooverError::Other(e.to_string())),
            // sender dropped without a result — treat as a timeout-class error
            Ok(Err(_)) => Err(DooverError::Other(format!("RPC call '{method}' was abandoned"))),
            Err(_) => Err(DooverError::Other(format!(
                "TIMEOUT: RPC call '{method}' timed out after {}s",
                timeout.as_secs_f64()
            ))),
        }
    }

    /// Send an RPC request without waiting for a response (pydoover
    /// `RPCManager.fire_and_forget`). Note pydoover's field order differs
    /// from `call` here (`request` before `method`) — replicated verbatim.
    pub async fn fire_and_forget(
        backend: &dyn ChannelBackend,
        channel: &str,
        method: &str,
        params: Option<Value>,
    ) -> Result<u64> {
        let data = json!({
            "type": "rpc",
            "request": params.unwrap_or_else(|| json!({})),
            "method": method,
            "status": {"code": "sent"},
            "response": {},
        });
        backend.create_message(channel, &data).await
    }

    // -- event handling ------------------------------------------------------

    /// Route an incoming channel event: requests (`MessageCreate` /
    /// `OneShotMessage`) to handler dispatch, responses (`MessageUpdate`) to
    /// pending-future resolution.
    pub async fn handle_event(&self, event: &Event) {
        if event.is_message_create() || event.is_one_shot() {
            self.handle_request(event).await;
        } else if event.is_message_update() {
            self.handle_response(event);
        }
    }

    fn get_handler(&self, channel: &str, method: &str) -> Option<RpcHandler> {
        let st = self.state.lock().unwrap();
        st.handlers
            .get(&(Some(channel.to_string()), method.to_string()))
            .or_else(|| st.handlers.get(&(None, method.to_string())))
            .cloned()
    }

    async fn handle_request(&self, event: &Event) {
        let Some(data) = event.message_data() else { return };
        if data.get("type").and_then(Value::as_str) != Some("rpc") {
            tracing::debug!("skipping non-rpc event on '{}'", event.channel);
            return;
        }
        let Some(method) = data.get("method").and_then(Value::as_str) else { return };

        // Requests stamped for a different app are not ours.
        if let Some(target) = data.get("app_key").and_then(Value::as_str) {
            if self.app_key.as_deref() != Some(target) {
                tracing::debug!(
                    "skipping RPC request for app_key={target:?} (ours={:?})",
                    self.app_key
                );
                return;
            }
        }

        let Some(payload) = data.get("request") else {
            tracing::info!("received malformed RPC request: {data}");
            return;
        };

        // Drop expired commands: if the message was created longer ago than its
        // `expires_after` lifetime, it's stale and must not be acted upon
        // (pydoover drops these before dispatch, and does not reply).
        let request_message_id = event.message_id();
        if let Some(id) = request_message_id {
            if command_is_expired(id, data) {
                tracing::info!(
                    "skipping expired RPC command '{method}' (message {id}, expired at {:?})",
                    command_expires_at(id, data)
                );
                return;
            }
        }

        // Drop commands that were already withdrawn before we got to them — a
        // backlog delivered after a reconnect can carry both the command and
        // the cancellation, and the create event may well arrive second.
        if command_is_cancelled(data) {
            tracing::info!(
                "skipping cancelled RPC command '{method}' (message {request_message_id:?})"
            );
            return;
        }

        let Some(handler) = self.get_handler(&event.channel, method) else { return };

        // One-shots are fire-and-forget: there is no persisted message to
        // update with a response (pydoover `can_respond`).
        let message_id = if event.is_one_shot() { None } else { request_message_id };
        let ctx = RpcContext {
            method: method.to_string(),
            channel: event.channel.clone(),
            message_id,
            request_message_id,
            data: data.clone(),
            backend: self.backend.clone(),
            cancelled: Arc::new(Mutex::new(None)),
            cancel_tx: Arc::new(Notify::new()),
        };

        // Track the command before awaiting the handler, so a cancellation
        // arriving mid-flight finds it. Only responses for a command that is
        // still live get written: a cancellation already put the message in a
        // terminal `error` state, and the site renders it as "Cancelled" from
        // that — writing our own outcome over the top would relabel a command
        // the operator cancelled as having succeeded.
        if let Some(id) = request_message_id {
            self.state.lock().unwrap().inflight.insert(id, ctx.clone());
        }
        let result = handler(ctx.clone(), payload.clone()).await;
        if let Some(id) = request_message_id {
            self.state.lock().unwrap().inflight.remove(&id);
        }

        let should_respond = message_id.filter(|_| !ctx.is_cancelled());
        match result {
            Ok(response) => {
                if let Some(id) = should_respond {
                    if let Err(e) = self.send_result(&event.channel, id, response).await {
                        tracing::error!("failed to send RPC result: {e}");
                    }
                }
            }
            Err(rpc_err) if ctx.is_cancelled() => {
                // The handler chose to unwind via `error_if_cancelled`. The
                // command already carries the canceller's terminal status, so
                // there is nothing to report back and this is not a failure.
                tracing::info!(
                    "RPC handler for '{method}' stopped: command was cancelled ({rpc_err})"
                );
            }
            Err(rpc_err) => {
                tracing::error!("error in RPC handler '{method}': {rpc_err}");
                if let Some(id) = should_respond {
                    if let Err(e) =
                        self.send_error(&event.channel, id, &rpc_err.code, &rpc_err.message).await
                    {
                        tracing::error!("failed to send RPC error: {e}");
                    }
                }
            }
        }
    }

    fn handle_response(&self, event: &Event) {
        let Some(data) = event.message_data() else { return };
        let Some(status) = data.get("status") else {
            tracing::debug!("failed to get status from RPC message; ignoring");
            return;
        };
        let Some(message_id) = event.message_id() else { return };

        let cancelled = status_is_cancelled(status);

        // Inbound: a command we're serving has been withdrawn by its issuer.
        // Updates arrive for both directions, keyed by message id, and the two
        // id spaces are disjoint, so the lookups can't collide.
        if cancelled {
            let ctx = self.state.lock().unwrap().inflight.get(&message_id).cloned();
            if let Some(ctx) = ctx {
                tracing::info!(
                    "RPC command '{}' (message {message_id}) was cancelled by the issuer; \
                     notifying the running handler",
                    ctx.method
                );
                ctx.mark_cancelled(status);
                return;
            }
        }

        let code = status.get("code").and_then(Value::as_str).unwrap_or_default();
        // Intermediate statuses keep the future pending (pydoover).
        if matches!(code, "sent" | "acknowledged" | "deferred" | "pending") {
            return;
        }

        // A cancellation reaches us as a terminal `error`, so it must be
        // tested before the generic error branch or it would surface as an
        // opaque RpcError instead.
        if cancelled {
            let method = data
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if let Some(tx) = self.state.lock().unwrap().pending.remove(&message_id) {
                let _ = tx.send(Err(RpcError::cancelled(method)));
            }
            return;
        }

        let result = match code {
            "success" => Ok(data.get("response").cloned().unwrap_or_else(|| json!({}))),
            "error" => {
                let err = status.get("message");
                let (code, message) = match err {
                    Some(Value::Object(m)) => (
                        m.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN").to_string(),
                        m.get("message").map(value_to_message).unwrap_or_default(),
                    ),
                    Some(other) => ("UNKNOWN".to_string(), value_to_message(other)),
                    None => ("UNKNOWN".to_string(), String::new()),
                };
                Err(RpcError { code, message })
            }
            _ => return,
        };

        if let Some(tx) = self.state.lock().unwrap().pending.remove(&message_id) {
            let _ = tx.send(result);
        }
    }

    // -- response helpers ----------------------------------------------------

    async fn send_result(&self, channel: &str, message_id: u64, response: Value) -> Result<()> {
        let data = json!({
            "status": {
                "code": "success",
                "message": null,
            },
            "response": response,
        });
        self.backend
            .update_message(channel, message_id, &data, &UpdateMessageOptions::default())
            .await
    }

    async fn send_error(
        &self,
        channel: &str,
        message_id: u64,
        code: &str,
        message: &str,
    ) -> Result<()> {
        let data = json!({
            "status": {
                "code": "error",
                "message": {"code": code, "message": message},
            },
            "response": {},
        });
        self.backend
            .update_message(channel, message_id, &data, &UpdateMessageOptions::default())
            .await
    }
}

fn value_to_message(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_request_shape_matches_pydoover() {
        // The exact dict pydoover's `call` builds, key order included.
        let mut data = Map::new();
        data.insert("type".into(), json!("rpc"));
        data.insert("method".into(), json!("get_di"));
        data.insert("request".into(), json!({"pin": 1}));
        data.insert("status".into(), json!({"code": "sent"}));
        data.insert("response".into(), json!({}));
        assert_eq!(
            serde_json::to_string(&Value::Object(data)).unwrap(),
            r#"{"type":"rpc","method":"get_di","request":{"pin":1},"status":{"code":"sent"},"response":{}}"#
        );
    }

    #[test]
    fn error_payload_shape() {
        let data = json!({
            "status": {
                "code": "error",
                "message": {"code": "INTERNAL_ERROR", "message": "boom"},
            },
            "response": {},
        });
        assert_eq!(
            serde_json::to_string(&data).unwrap(),
            r#"{"status":{"code":"error","message":{"code":"INTERNAL_ERROR","message":"boom"}},"response":{}}"#
        );
    }

    #[test]
    fn audit_metadata_key_order_matches_pydoover() {
        let opts = CallOptions::on("ui_cmds")
            .app_key("pump_app")
            .actor(json!({"id": 7, "name": "Jarrod", "email": "j@example.com"}))
            .reason("manual override")
            .old_value(json!(false))
            .expires_after(Duration::from_secs(90))
            .retry_of("123456789");
        let payload = opts.request_payload("pump", Some(json!({"on": true})));
        let keys: Vec<_> = payload.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "type",
                "method",
                "request",
                "status",
                "response",
                "app_key",
                "actor",
                "reason",
                "old_value",
                "expires_after",
                "retry_of"
            ]
        );
        // pydoover puts `expires_after` on the wire in milliseconds.
        assert_eq!(payload["expires_after"], json!(90_000));
        assert_eq!(payload["retry_of"], json!("123456789"));
    }

    #[test]
    fn old_value_none_is_omitted_but_null_is_sent() {
        // pydoover's NOT_GIVEN sentinel: `None` is a meaningful value, so only
        // an unset `old_value` drops the key.
        let bare = CallOptions::default().request_payload("m", None);
        assert!(bare.get("old_value").is_none());
        let explicit = CallOptions::default().old_value(Value::Null).request_payload("m", None);
        assert_eq!(explicit["old_value"], Value::Null);
    }

    #[test]
    fn expiry_is_message_timestamp_plus_lifetime() {
        // A snowflake minted 10 minutes ago with a 60s lifetime is expired; the
        // same id with an hour's lifetime is not.
        let ten_min_ago = crate::utils::generate_snowflake_id_at(
            (now_unix_ms() - 600_000) as u64,
            crate::utils::SnowflakeType::Message,
            0,
            0,
            false,
        );
        assert!(command_is_expired(ten_min_ago, &json!({"expires_after": 60_000})));
        assert!(!command_is_expired(ten_min_ago, &json!({"expires_after": 3_600_000})));
        // No `expires_after` at all: never expires (pydoover returns None).
        assert!(!command_is_expired(ten_min_ago, &json!({})));
        assert_eq!(command_expires_at(ten_min_ago, &json!({})), None);
        // The absolute expiry is the creation stamp plus the lifetime.
        assert_eq!(
            command_expires_at(ten_min_ago, &json!({"expires_after": 60_000})),
            Some(unix_millis_from_snowflake(ten_min_ago) as i64 + 60_000)
        );
    }

    #[test]
    fn success_payload_shape() {
        let data = json!({
            "status": {"code": "success", "message": null},
            "response": {"ok": true},
        });
        assert_eq!(
            serde_json::to_string(&data).unwrap(),
            r#"{"status":{"code":"success","message":null},"response":{"ok":true}}"#
        );
    }
}
