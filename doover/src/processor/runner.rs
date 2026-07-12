//! The invocation pipeline — pydoover `Application._handle_event` /
//! `_dispatch_invocation` / `_setup` / `_publish_invocation_summary`.
//!
//! Flow per invocation: decode `op` into a typed [`EventPayload`] →
//! `pre_hook_filter` → token-upgrade setup (initial JWT + subscription /
//! schedule info → full token, agent_id, app_key, seeded channels) →
//! `tag_values` self-loop guard → user `setup` → `post_setup_filter` →
//! handler → `commit_tags` → user `close` → publish the invocation summary
//! to every `dv_proc_config.inv_targets` destination.
//!
//! Known divergences from pydoover (documented, deliberate):
//! - `no_handler` is detected via the [`Handled::NotImplemented`] sentinel
//!   *after* setup and dispatch (Rust cannot compare method overrides), so a
//!   handler-less event still performs the token upgrade before being
//!   recorded as skipped. pydoover skips before any API call.
//! - No declarative UI / RPC managers yet: the on-deployment
//!   `publish_ui_schema` auto-publish and the `ui_manager`/`rpc` event hooks
//!   are not run. `ProcessorContext::publish_ui_schema` is available for
//!   manual publishing from `on_deployment`.
//! - `dv_proc_config.log_level` / `log_overrides` are parsed but not applied
//!   to the `tracing` subscriber.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::api::data::DataClient;
use crate::channel_backend::ChannelBackend;
use crate::error::DooverError;
use crate::models::SubscriptionInfo;
use crate::tags::TAG_CHANNEL_NAME;

use super::application::{now_ms, Handled, Processor, ProcessorContext, SkipReason};
use super::config::ProcConfig;
use super::events::{
    id_string, AggregateUpdateEvent, DeploymentEvent, EventPayload, IngestionEndpointEvent,
    ManualInvokeEvent, MessageCreateEvent, ScheduleEvent,
};
use super::tags::ProcessorTags;

/// Lambda invocation metadata surfaced in the summary (`requestId`,
/// `function_name`, `function_version`) — pydoover captures these from the
/// Lambda context in `handler.run_app`.
#[derive(Debug, Clone, Default)]
pub struct LambdaMeta {
    pub request_id: Option<String>,
    pub function_name: Option<String>,
    pub function_version: Option<String>,
}

/// Mutable per-invocation bookkeeping that feeds the summary (the fields
/// pydoover keeps on `self`).
struct InvocationState {
    subscription_id: Option<String>,
    schedule_id: Option<String>,
    ingestion_id: Option<String>,
    event_type: Option<String>,
    agent_id: Option<u64>,
    app_key: Option<String>,
    app_id: Option<String>,
    proc_config: ProcConfig,
}

/// How a dispatch ended when it didn't succeed.
enum DispatchEnd {
    Skipped(SkipReason),
    Failed { error_type: String, message: String },
}

impl From<DooverError> for DispatchEnd {
    fn from(e: DooverError) -> Self {
        Self::Failed { error_type: e.type_name().to_string(), message: e.to_string() }
    }
}

/// Run one invocation end to end and return the invocation-summary body
/// (which is also what gets published to the `inv_targets`). This is
/// pydoover `_handle_event`: dispatch, then always publish the summary and
/// close, whatever happened.
pub(crate) async fn run_invocation<P: Processor>(
    processor: &mut P,
    event: &Value,
    subscription_id: Option<String>,
    api: Arc<DataClient>,
    lambda: &LambdaMeta,
) -> Value {
    let started_at_ms = now_ms();
    let started = Instant::now();

    let mut state = InvocationState {
        subscription_id,
        schedule_id: None,
        ingestion_id: None,
        event_type: event.get("op").and_then(Value::as_str).map(str::to_string),
        agent_id: None,
        app_key: None,
        app_id: None,
        proc_config: ProcConfig::default(),
    };

    tracing::info!("initialising processor task (op={:?})", state.event_type);
    let outcome = dispatch_invocation(processor, event, &api, &mut state).await;

    let (status, skip_reason, error): (&str, Option<SkipReason>, Option<Value>) = match &outcome {
        Ok(()) => ("success", None, None),
        Err(DispatchEnd::Skipped(reason)) => ("skipped", Some(*reason), None),
        Err(DispatchEnd::Failed { error_type, message }) => {
            tracing::error!("unhandled error in invocation: {message}");
            ("error", None, Some(json!({"type": error_type, "message": message})))
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    tracing::info!("finished invocation: status={status} duration={duration_ms}ms");

    // Field names and order match pydoover byte-for-byte: note the one
    // camelCase key (`requestId`) and agent_id stringified because Doover
    // IDs are 64-bit and JS truncates above 2^53.
    let body = json!({
        "app_key": state.app_key,
        "app_id": state.app_id,
        "agent_id": state.agent_id.map(|a| a.to_string()),
        "event_type": state.event_type,
        "subscription_id": state.subscription_id,
        "schedule_id": state.schedule_id,
        "ingestion_id": state.ingestion_id,
        "started_at": started_at_ms,
        "duration_ms": duration_ms,
        "status": status,
        "skip_reason": skip_reason.map(SkipReason::as_str),
        "error": error,
        "requestId": lambda.request_id,
        "function_name": lambda.function_name,
        "function_version": lambda.function_version,
    });

    publish_invocation_summary(&api, &state, &body).await;
    body
}

/// pydoover `_publish_invocation_summary`. Errors are logged per target;
/// a missing `app_id` (setup never ran, or the deployment config lacks
/// `APP_ID`) skips publishing — pydoover's `str.replace(…, None)` raises
/// there and the exception is swallowed by the caller.
async fn publish_invocation_summary(api: &DataClient, state: &InvocationState, body: &Value) {
    if state.proc_config.inv_targets.is_empty() {
        return;
    }
    let Some(app_id) = &state.app_id else {
        tracing::error!("failed to publish invocation summary: app_id is not set");
        return;
    };
    for target in &state.proc_config.inv_targets {
        let channel = target.channel.replace("$app_id", app_id);
        if let Err(e) = api.create_message_http(&channel, body, None, target.agent_id).await {
            tracing::error!(
                "failed to post invocation summary to {:?}/{channel}: {e}",
                target.agent_id
            );
        }
    }
}

/// pydoover `_dispatch_invocation` — the pipeline body. `Ok` means success;
/// skip/error ends flow through [`DispatchEnd`].
async fn dispatch_invocation<P: Processor>(
    processor: &mut P,
    event: &Value,
    api: &Arc<DataClient>,
    state: &mut InvocationState,
) -> Result<(), DispatchEnd> {
    let d = event.get("d").cloned().unwrap_or_else(|| json!({}));
    state.schedule_id = id_string(d.get("schedule_id"));
    state.ingestion_id = id_string(d.get("ingestion_id"));
    // org ID should be set in both schedules and subscriptions; the upgrade
    // payload may correct it later.
    let event_organisation_id = d.get("organisation_id").and_then(crate::models::value_as_id);

    // The initial token: temporary (subscription) or long-lived (schedule);
    // either way it can only access the info endpoint.
    let initial_token = event
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| DispatchEnd::Failed {
            error_type: "RuntimeError".to_string(),
            message: "Initial token has not been set.".to_string(),
        })?
        .to_string();
    // Can be set during testing; normally it is signed into the JWT.
    state.agent_id = event.get("agent_id").and_then(crate::models::value_as_id);

    // Decode `op` into the typed payload. An unknown op has no handler at
    // all — pydoover skips it as no_handler before doing anything else.
    let op = state.event_type.clone().unwrap_or_default();
    let payload = decode_payload(processor, &op, &d)?;
    let Some(payload) = payload else {
        tracing::info!("unknown event type {op:?}; skipping (no handler)");
        return Err(DispatchEnd::Skipped(SkipReason::NoHandler));
    };
    let is_deployment = matches!(payload, EventPayload::Deployment(_));

    // Anti-recursion: remember the channel that triggered us.
    api.set_invoking_channel(payload.invoking_channel().map(str::to_string));

    if !processor.pre_hook_filter(&payload).await {
        tracing::info!("pre-hook filter rejected event");
        return Err(DispatchEnd::Skipped(SkipReason::PreHookFilter));
    }

    // Token upgrade + context construction (pydoover `_setup`).
    let setup_started = Instant::now();
    let ctx = setup_context(event, &d, api, state, initial_token, event_organisation_id).await?;
    tracing::info!("setup took {:?}", setup_started.elapsed());

    // Reject events this app itself published to tag_values (the classic
    // processor infinite loop).
    if is_tag_values_self_loop(&payload, &ctx.app_key) {
        tracing::info!("rejecting event publishing to tag_values within this app key");
        return Err(DispatchEnd::Skipped(SkipReason::TagValuesSelfLoop));
    }

    if let Err(e) = processor.setup(&ctx).await {
        tracing::error!("error attempting to setup processor: {e}");
    }

    if !processor.post_setup_filter(&ctx, &payload).await {
        tracing::info!("post-setup filter rejected event");
        return Err(DispatchEnd::Skipped(SkipReason::PostSetupFilter));
    }

    // pydoover publishes the UI schema to ui_state here on deployment (for
    // non-static UIs) and pumps ui_manager/rpc event hooks. doover-rs has no
    // declarative processor UI yet — publish manually from `on_deployment`
    // via `ctx.publish_ui_schema` if needed.

    // Handler. User errors are logged and the invocation still counts as
    // success (pydoover wraps the call in try/except and keeps going).
    let handler_started = Instant::now();
    let handled = match dispatch_handler(processor, &ctx, &payload).await {
        Ok(handled) => handled,
        Err(e) => {
            tracing::error!("error attempting to process event: {e}");
            Handled::Done
        }
    };
    tracing::info!("processing event took {:?}", handler_started.elapsed());

    if handled == Handled::NotImplemented && !is_deployment {
        tracing::info!("skipping {op} event as no overridden handler found");
        return Err(DispatchEnd::Skipped(SkipReason::NoHandler));
    }

    // The single tag flush per invocation; a failure here is an invocation
    // error (pydoover awaits it outside any try block).
    ctx.tags().commit_tags().await.map_err(DispatchEnd::from)?;

    if let Err(e) = processor.close(&ctx).await {
        tracing::error!("error attempting to close processor: {e}");
    }

    Ok(())
}

/// Decode `op` + `d` into a typed payload; `Ok(None)` for an unknown op.
fn decode_payload<P: Processor>(
    processor: &P,
    op: &str,
    d: &Value,
) -> Result<Option<EventPayload>, DispatchEnd> {
    let payload = match op {
        "on_message_create" => {
            EventPayload::MessageCreate(MessageCreateEvent::from_value(d).map_err(decode_err)?)
        }
        "on_aggregate_update" => {
            EventPayload::AggregateUpdate(AggregateUpdateEvent::from_value(d).map_err(decode_err)?)
        }
        "on_deployment" => {
            EventPayload::Deployment(DeploymentEvent::from_value(d).map_err(decode_err)?)
        }
        "on_schedule" => EventPayload::Schedule(ScheduleEvent::from_value(d).map_err(decode_err)?),
        "on_ingestion_endpoint" => {
            let raw = d.get("payload").and_then(Value::as_str).unwrap_or_default();
            let parsed = processor.parse_ingestion_payload(raw).map_err(decode_err)?;
            EventPayload::IngestionEndpoint(
                IngestionEndpointEvent::from_value(d, parsed).map_err(decode_err)?,
            )
        }
        "on_manual_invoke" => {
            EventPayload::ManualInvoke(ManualInvokeEvent::from_value(d).map_err(decode_err)?)
        }
        _ => return Ok(None),
    };
    Ok(Some(payload))
}

fn decode_err(e: DooverError) -> DispatchEnd {
    DispatchEnd::Failed { error_type: e.type_name().to_string(), message: e.to_string() }
}

/// pydoover `Application._setup`: install the initial JWT, resolve the
/// [`SubscriptionInfo`] (inline `d.upgrade` beats the info endpoints),
/// upgrade the token, and build the [`ProcessorContext`] with the seeded
/// channels.
async fn setup_context(
    _event: &Value,
    d: &Value,
    api: &Arc<DataClient>,
    state: &mut InvocationState,
    initial_token: String,
    event_organisation_id: Option<u64>,
) -> Result<ProcessorContext, DispatchEnd> {
    api.set_token(initial_token);

    // Always prioritise the inline upgrade payload; otherwise ask the info
    // endpoint that matches the invocation source.
    let upgrade = d.get("upgrade").filter(|u| !u.is_null());
    let info: SubscriptionInfo = if let Some(upgrade) = upgrade {
        SubscriptionInfo::from_value(upgrade).map_err(DispatchEnd::from)?
    } else if let Some(subscription_id) = &state.subscription_id {
        api.fetch_subscription_info(subscription_id).await.map_err(DispatchEnd::from)?
    } else if let Some(schedule_id) = &state.schedule_id {
        api.fetch_schedule_info(schedule_id).await.map_err(DispatchEnd::from)?
    } else {
        // Ingestion events are invoked directly with the upgrade payload
        // pre-loaded; reaching here without one is an error.
        return Err(DispatchEnd::Failed {
            error_type: "ValueError".to_string(),
            message: "No subscription or schedule ID provided.".to_string(),
        });
    };

    state.agent_id = Some(info.agent_id);
    state.app_key = Some(info.app_key.clone());

    api.set_agent_id(info.agent_id);
    api.set_token(info.token.clone());
    api.set_app_key(info.app_key.clone());
    // The upgrade payload's org should match the event's, but if they
    // disagree the upgrade is the source of truth.
    let organisation_id = info.organisation_id.or(event_organisation_id);
    if let Some(org) = organisation_id {
        api.set_organisation_id(org.to_string());
    }

    let tags = Arc::new(ProcessorTags::new(
        info.app_key.clone(),
        api.clone() as Arc<dyn ChannelBackend>,
        info.agent_id,
        info.tag_values.clone(),
        true,
    ));

    // Connection config/status aren't valid for org processors, and fresh
    // devices won't have them either.
    let connection_config = info
        .connection_data
        .get("config")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let connection_status = info
        .connection_data
        .get("status")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let deployment_config = info.deployment_config.clone();
    let display_name = deployment_config
        .get("APP_DISPLAY_NAME")
        .and_then(Value::as_str)
        .map(str::to_string);
    let app_id = deployment_config.get("APP_ID").and_then(Value::as_str).map(str::to_string);
    let proc_config = ProcConfig::from_value(deployment_config.get("dv_proc_config"));

    state.app_id = app_id.clone();
    state.proc_config = proc_config.clone();

    Ok(ProcessorContext::new(
        api.clone(),
        tags,
        info.agent_id,
        info.app_key,
        app_id,
        organisation_id,
        display_name,
        deployment_config,
        proc_config,
        connection_config,
        connection_status,
    ))
}

/// The `tag_values` self-loop guard: an event caused by *this app's* own tag
/// write must not re-trigger it.
fn is_tag_values_self_loop(payload: &EventPayload, app_key: &str) -> bool {
    match payload {
        EventPayload::AggregateUpdate(e) => {
            e.channel.name == TAG_CHANNEL_NAME
                && e.request_data.as_object().is_some_and(|m| m.contains_key(app_key))
        }
        EventPayload::MessageCreate(e) => {
            e.channel.name == TAG_CHANNEL_NAME
                && e.message.data.as_object().is_some_and(|m| m.contains_key(app_key))
        }
        _ => false,
    }
}

async fn dispatch_handler<P: Processor>(
    processor: &mut P,
    ctx: &ProcessorContext,
    payload: &EventPayload,
) -> crate::error::Result<Handled> {
    match payload {
        EventPayload::MessageCreate(e) => processor.on_message_create(ctx, e).await,
        EventPayload::AggregateUpdate(e) => processor.on_aggregate_update(ctx, e).await,
        EventPayload::Deployment(e) => processor.on_deployment(ctx, e).await,
        EventPayload::Schedule(e) => processor.on_schedule(ctx, e).await,
        EventPayload::IngestionEndpoint(e) => processor.on_ingestion_endpoint(ctx, e).await,
        EventPayload::ManualInvoke(e) => processor.on_manual_invoke(ctx, e).await,
    }
}
