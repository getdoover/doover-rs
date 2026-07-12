//! End-to-end processor pipeline tests: representative SNS-wrapped and raw
//! invocation events (shapes from pydoover `processor/handler.py` +
//! `processor/application.py`) driven through `handle_event_with` against a
//! wiremock data API. Asserts the token-upgrade sequence, the exact
//! invocation-summary body (camelCase `requestId`, stringified `agent_id`,
//! `$app_id` substitution), skip reasons, and the single tag commit.

use std::io::Read as _;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use doover::async_trait;
use doover::error::Result;
use doover::processor::{
    handle_event_with, DeploymentEvent, EventPayload, Handled, LambdaMeta, MessageCreateEvent,
    Processor, ProcessorContext, ProcessorOptions, ScheduleEvent,
};

const SUB_ARN: &str = "arn:aws:sns:ap-southeast-2:123:doover-topic:sub-uuid";

/// Decode a recorded body (transparently un-gzipping — the client
/// compresses JSON bodies ≥ 50 bytes).
fn body_json(req: &wiremock::Request) -> Value {
    let gzipped = req
        .headers
        .get("content-encoding")
        .is_some_and(|v| v.to_str().unwrap_or_default() == "gzip");
    if gzipped {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(req.body.as_slice()).read_to_end(&mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    } else {
        serde_json::from_slice(&req.body).unwrap()
    }
}

fn subscription_info() -> Value {
    json!({
        "agent_id": "1",
        "organisation_id": 7,
        "app_key": "my_app",
        "deployment_config": {
            "APP_DISPLAY_NAME": "My App",
            "APP_ID": "app-uuid-1",
        },
        "ui_state": null,
        "ui_cmds": {"my_app": {}},
        "tag_values": {"my_app": {"counter": 1}},
        "connection_data": {"config": {"offline_after": 120}, "status": {}},
        "token": "upgraded-token",
    })
}

/// Mount the endpoints a successful invocation touches.
async fn mount_common(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/processors/subscriptions/{SUB_ARN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(subscription_info()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/processors/schedules/sched-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(subscription_info()))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/agents/1/channels/tag_values/aggregate"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/agents/1/channels/tag_values/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "9001"})))
        .mount(server)
        .await;
    // The default inv_target with $app_id substituted from APP_ID.
    Mock::given(method("POST"))
        .and(path("/agents/1/channels/dv-proc-inv-app-uuid-1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "9002"})))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/agents/1/channels/ui_state/aggregate"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

fn message_create_event(channel: &str, data: Value) -> Value {
    json!({
        "op": "on_message_create",
        "token": "initial-token",
        "d": {
            "organisation_id": 7,
            "message": {
                "id": "12345678901234567",
                "author_id": "99",
                "channel": {"agent_id": 1, "name": channel},
                "data": data,
            },
        },
    })
}

fn sns_wrap(inner: &Value) -> Value {
    json!({
        "Records": [{
            "EventSource": "aws:sns",
            "EventSubscriptionArn": SUB_ARN,
            "Sns": {"Message": inner.to_string()},
        }]
    })
}

fn lambda_options(server: &MockServer) -> ProcessorOptions {
    ProcessorOptions {
        base_url: Some(server.uri()),
        http_client: None,
        lambda: LambdaMeta {
            request_id: Some("req-abc".to_string()),
            function_name: Some("my-processor".to_string()),
            function_version: Some("3".to_string()),
        },
    }
}

// -- Processors under test ---------------------------------------------------

/// Handles message-create by bumping a tag.
#[derive(Default)]
struct CounterProcessor;

#[async_trait]
impl Processor for CounterProcessor {
    async fn on_message_create(
        &mut self,
        ctx: &ProcessorContext,
        event: &MessageCreateEvent,
    ) -> Result<Handled> {
        assert_eq!(event.message.id, 12345678901234567);
        assert_eq!(ctx.app_key, "my_app");
        assert_eq!(ctx.agent_id, 1);
        assert_eq!(ctx.display_name.as_deref(), Some("My App"));
        let current = ctx.get_tag("counter").and_then(|v| v.as_i64()).unwrap_or(0);
        ctx.set_tag("counter", json!(current + 1));
        Ok(Handled::Done)
    }
}

/// No handlers overridden at all.
#[derive(Default)]
struct EmptyProcessor;

impl Processor for EmptyProcessor {}

/// Rejects everything in the pre-hook.
#[derive(Default)]
struct PreHookRejects;

#[async_trait]
impl Processor for PreHookRejects {
    async fn pre_hook_filter(&mut self, _event: &EventPayload) -> bool {
        false
    }
}

/// Rejects everything after setup.
#[derive(Default)]
struct PostSetupRejects;

#[async_trait]
impl Processor for PostSetupRejects {
    async fn on_message_create(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &MessageCreateEvent,
    ) -> Result<Handled> {
        panic!("handler must not run when post_setup_filter rejects");
    }

    async fn post_setup_filter(&mut self, _ctx: &ProcessorContext, _event: &EventPayload) -> bool {
        false
    }
}

/// Handles message-create (used for the self-loop test).
#[derive(Default)]
struct LoopProcessor;

#[async_trait]
impl Processor for LoopProcessor {
    async fn on_message_create(
        &mut self,
        _ctx: &ProcessorContext,
        _event: &MessageCreateEvent,
    ) -> Result<Handled> {
        panic!("handler must not run for a tag_values self-loop");
    }
}

/// Handles schedules.
#[derive(Default)]
struct ScheduleProcessor;

#[async_trait]
impl Processor for ScheduleProcessor {
    async fn on_schedule(
        &mut self,
        _ctx: &ProcessorContext,
        event: &ScheduleEvent,
    ) -> Result<Handled> {
        assert_eq!(event.schedule_id, "sched-1");
        Ok(Handled::Done)
    }
}

/// Publishes a UI schema on deployment.
#[derive(Default)]
struct DeployProcessor;

#[async_trait]
impl Processor for DeployProcessor {
    async fn on_deployment(
        &mut self,
        ctx: &ProcessorContext,
        event: &DeploymentEvent,
    ) -> Result<Handled> {
        assert_eq!(event.app_key, "my_app");
        ctx.publish_ui_schema(&json!({"type": "uiContainer"}), true).await?;
        Ok(Handled::Done)
    }
}

// -- Tests --------------------------------------------------------------------

#[tokio::test]
async fn sns_message_create_full_pipeline() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<CounterProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    // Exact summary shape: pydoover's field names and order, camelCase
    // requestId, agent_id stringified.
    let keys: Vec<&str> =
        summary.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "app_key",
            "app_id",
            "agent_id",
            "event_type",
            "subscription_id",
            "schedule_id",
            "ingestion_id",
            "started_at",
            "duration_ms",
            "status",
            "skip_reason",
            "error",
            "requestId",
            "function_name",
            "function_version",
        ]
    );
    assert!(summary["started_at"].is_u64());
    assert!(summary["duration_ms"].is_u64());
    let mut expected = json!({
        "app_key": "my_app",
        "app_id": "app-uuid-1",
        "agent_id": "1",
        "event_type": "on_message_create",
        "subscription_id": SUB_ARN,
        "schedule_id": null,
        "ingestion_id": null,
        "started_at": summary["started_at"],
        "duration_ms": summary["duration_ms"],
        "status": "success",
        "skip_reason": null,
        "error": null,
        "requestId": "req-abc",
        "function_name": "my-processor",
        "function_version": "3",
    });
    // json! macro sorts nothing; compare value-wise.
    for (k, v) in expected.as_object_mut().unwrap() {
        assert_eq!(&summary[k.as_str()], v, "summary field {k}");
    }

    // Request sequence: token upgrade with the initial JWT, then everything
    // else with the upgraded token.
    let reqs = server.received_requests().await.unwrap();
    let auth = |r: &wiremock::Request| {
        r.headers.get("authorization").unwrap().to_str().unwrap().to_string()
    };
    assert_eq!(reqs[0].url.path(), format!("/processors/subscriptions/{SUB_ARN}"));
    assert_eq!(auth(&reqs[0]), "Bearer initial-token");

    // Single tag commit: aggregate write then (LogMode::Always) full log
    // message, both scoped to this app key.
    assert_eq!(reqs[1].method.as_str(), "PATCH");
    assert_eq!(reqs[1].url.path(), "/agents/1/channels/tag_values/aggregate");
    assert_eq!(auth(&reqs[1]), "Bearer upgraded-token");
    assert_eq!(body_json(&reqs[1]), json!({"my_app": {"counter": 2}}));

    assert_eq!(reqs[2].url.path(), "/agents/1/channels/tag_values/messages");
    assert_eq!(body_json(&reqs[2]), json!({"data": {"my_app": {"counter": 2}}}));

    // Invocation summary fanned out to the default target with $app_id
    // substituted.
    assert_eq!(reqs[3].url.path(), "/agents/1/channels/dv-proc-inv-app-uuid-1/messages");
    assert_eq!(auth(&reqs[3]), "Bearer upgraded-token");
    assert_eq!(body_json(&reqs[3]), json!({"data": summary}));
    assert_eq!(reqs.len(), 4);
}

#[tokio::test]
async fn raw_schedule_event_uses_schedule_info() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    // EventBridge schedules arrive unwrapped (no SNS Records).
    let event = json!({
        "op": "on_schedule",
        "token": "sched-token",
        "d": {"schedule_id": "sched-1", "organisation_id": 7},
    });
    let summary =
        handle_event_with::<ScheduleProcessor>(event, lambda_options(&server)).await.unwrap();

    assert_eq!(summary["status"], "success");
    assert_eq!(summary["event_type"], "on_schedule");
    assert_eq!(summary["subscription_id"], Value::Null);
    assert_eq!(summary["schedule_id"], "sched-1");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/processors/schedules/sched-1");
    // No tag writes (nothing set) — straight to the summary.
    assert_eq!(reqs[1].url.path(), "/agents/1/channels/dv-proc-inv-app-uuid-1/messages");
    assert_eq!(reqs.len(), 2);
}

#[tokio::test]
async fn inline_upgrade_payload_skips_info_endpoint() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let mut event = message_create_event("some_channel", json!({"hello": 1}));
    event["d"]["upgrade"] = subscription_info();
    let summary = handle_event_with::<CounterProcessor>(event, lambda_options(&server))
        .await
        .unwrap();
    assert_eq!(summary["status"], "success");

    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs.iter().all(|r| !r.url.path().starts_with("/processors/")),
        "inline upgrade payload must not hit the info endpoints"
    );
}

#[tokio::test]
async fn no_handler_skip_after_setup() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<EmptyProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    assert_eq!(summary["status"], "skipped");
    assert_eq!(summary["skip_reason"], "no_handler");
    assert_eq!(summary["error"], Value::Null);

    // No tag commit happened; the summary still gets published (the token
    // upgrade ran, so app_id is known).
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().all(|r| !r.url.path().contains("tag_values")));
    assert!(reqs
        .iter()
        .any(|r| r.url.path() == "/agents/1/channels/dv-proc-inv-app-uuid-1/messages"));
}

#[tokio::test]
async fn unknown_op_skips_as_no_handler_without_any_api_calls() {
    let server = MockServer::start().await;
    let event = json!({"op": "on_wibble", "token": "t", "d": {}});
    let summary =
        handle_event_with::<CounterProcessor>(event, lambda_options(&server)).await.unwrap();

    assert_eq!(summary["status"], "skipped");
    assert_eq!(summary["skip_reason"], "no_handler");
    // Setup never ran → no app_id → summary publish is skipped, like
    // pydoover (whose $app_id substitution raises and is swallowed).
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn pre_hook_filter_skips_before_any_api_call() {
    let server = MockServer::start().await;
    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<PreHookRejects>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    assert_eq!(summary["status"], "skipped");
    assert_eq!(summary["skip_reason"], "pre_hook_filter");
    assert_eq!(summary["app_key"], Value::Null);
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn post_setup_filter_skips_after_setup() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<PostSetupRejects>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    assert_eq!(summary["status"], "skipped");
    assert_eq!(summary["skip_reason"], "post_setup_filter");
    assert_eq!(summary["app_key"], "my_app");
}

#[tokio::test]
async fn tag_values_self_loop_is_skipped() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    // A message this app itself published to tag_values: data is keyed by
    // our own app_key.
    let inner = message_create_event("tag_values", json!({"my_app": {"counter": 5}}));
    let summary = handle_event_with::<LoopProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    assert_eq!(summary["status"], "skipped");
    assert_eq!(summary["skip_reason"], "tag_values_self_loop");

    // Another app's tag write is NOT a self-loop... but then the handler
    // runs (and panics in LoopProcessor) — use the counter processor to
    // prove it dispatches. Its tag commit is legal: same-app tag_values
    // writes pass the invoking-channel guard.
    let inner = message_create_event("tag_values", json!({"other_app": {"x": 1}}));
    let summary = handle_event_with::<CounterProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();
    assert_eq!(summary["status"], "success");
}

#[tokio::test]
async fn deployment_without_handler_still_succeeds() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let inner = json!({
        "op": "on_deployment",
        "token": "initial-token",
        "d": {
            "organisation_id": 7,
            "agent_id": "1",
            "app_id": "123",
            "app_install_id": "456",
            "app_key": "my_app",
            "app_display_name": "My App",
        },
    });
    let summary = handle_event_with::<EmptyProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    // pydoover never skips deployments as no_handler.
    assert_eq!(summary["status"], "success");
    assert_eq!(summary["skip_reason"], Value::Null);
}

#[tokio::test]
async fn deployment_handler_can_publish_ui_schema_with_replace() {
    let server = MockServer::start().await;
    mount_common(&server).await;

    let inner = json!({
        "op": "on_deployment",
        "token": "initial-token",
        "d": {
            "organisation_id": 7,
            "agent_id": "1",
            "app_id": "123",
            "app_install_id": "456",
            "app_key": "my_app",
            "app_display_name": "My App",
        },
    });
    let summary = handle_event_with::<DeployProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();
    assert_eq!(summary["status"], "success");

    let reqs = server.received_requests().await.unwrap();
    let ui = reqs
        .iter()
        .find(|r| r.url.path() == "/agents/1/channels/ui_state/aggregate")
        .expect("ui_state write");
    // publish_ui_schema(clear=true) → replace_keys=["state.children.<app_key>"].
    let replaces: Vec<String> = ui
        .url
        .query_pairs()
        .filter(|(k, _)| k == "replace")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(replaces, vec!["state.children.my_app"]);
    assert_eq!(
        body_json(ui),
        json!({"state": {"children": {"my_app": {"type": "uiContainer"}}}})
    );
}

#[tokio::test]
async fn setup_failure_is_an_error_status() {
    let server = MockServer::start().await;
    // Info endpoint 404s: the token upgrade fails.
    Mock::given(method("GET"))
        .and(path(format!("/processors/subscriptions/{SUB_ARN}")))
        .respond_with(ResponseTemplate::new(404).set_body_string("no such subscription"))
        .mount(&server)
        .await;

    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<CounterProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();

    assert_eq!(summary["status"], "error");
    assert_eq!(summary["error"]["type"], "NotFoundError");
    assert!(summary["error"]["message"].as_str().unwrap().contains("no such subscription"));
    assert_eq!(summary["skip_reason"], Value::Null);
}

#[tokio::test]
async fn missing_token_is_an_error() {
    let server = MockServer::start().await;
    let event = json!({"op": "on_manual_invoke", "d": {"organisation_id": 7, "payload": {}}});
    let summary =
        handle_event_with::<CounterProcessor>(event, lambda_options(&server)).await.unwrap();
    assert_eq!(summary["status"], "error");
    assert_eq!(summary["error"]["type"], "RuntimeError");
    assert_eq!(summary["error"]["message"], "Initial token has not been set.");
}

#[tokio::test]
async fn token_upgrade_updates_bearer_for_subsequent_requests() {
    let server = MockServer::start().await;
    // Only match the info fetch on the initial token, and the tag write on
    // the upgraded one — a wrong header 404s and fails the run.
    Mock::given(method("GET"))
        .and(path(format!("/processors/subscriptions/{SUB_ARN}")))
        .and(header("authorization", "Bearer initial-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(subscription_info()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/agents/1/channels/tag_values/aggregate"))
        .and(header("authorization", "Bearer upgraded-token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/agents/1/channels/tag_values/messages"))
        .and(header("authorization", "Bearer upgraded-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/agents/1/channels/dv-proc-inv-app-uuid-1/messages"))
        .and(header("authorization", "Bearer upgraded-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "2"})))
        .expect(1)
        .mount(&server)
        .await;

    let inner = message_create_event("some_channel", json!({"hello": 1}));
    let summary = handle_event_with::<CounterProcessor>(sns_wrap(&inner), lambda_options(&server))
        .await
        .unwrap();
    assert_eq!(summary["status"], "success");
}
