//! HTTP contract tests for `doover::api::DataClient` against a wiremock
//! server: endpoint paths/verbs, bearer auth, query encoding, gzip request
//! compression, retry-on-5xx, and error mapping — mirroring pydoover's
//! `AsyncDataClient` behaviour (`pydoover/api/data/_async.py`).

use std::io::Read as _;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use doover::api::data::{DataClient, ListMessagesQuery};
use doover::channel_backend::{AggregateOptions, ChannelBackend, UpdateMessageOptions};
use doover::DooverError;

fn client(server: &MockServer) -> DataClient {
    let mut c = DataClient::with_base_url(server.uri());
    c.retry_delay = Duration::from_millis(5);
    c.set_token("tok-1");
    c.set_agent_id(42);
    c
}

/// Decode a recorded request body, un-gzipping when the request was
/// compressed (pydoover compresses JSON bodies ≥ 50 bytes).
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

#[tokio::test]
async fn fetch_channel_path_auth_and_parse() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/tank_level"))
        .and(query_param("include_aggregate", "true"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "tank_level",
            "owner_id": "42",
            "is_private": false,
            "aggregate_schema": null,
            "message_schema": null,
            "aggregate": {"data": {"level": 3.5}, "attachments": [], "last_updated": 1000},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = client(&server).fetch_channel("tank_level").await.unwrap();
    assert_eq!(channel.name, "tank_level");
    assert_eq!(channel.owner_id, 42);
    assert!(!channel.is_private);
    assert_eq!(channel.aggregate_data, Some(json!({"level": 3.5})));
}

#[tokio::test]
async fn organisation_header_is_sent_when_set() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/c/aggregate"))
        .and(header("x-doover-organisation", "7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.set_organisation_id("7");
    c.fetch_channel_aggregate_raw("c", None).await.unwrap();
}

#[tokio::test]
async fn aggregate_404_maps_to_not_found_and_backend_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/missing/aggregate"))
        .respond_with(ResponseTemplate::new(404).set_body_string("channel not found"))
        .mount(&server)
        .await;

    let c = client(&server);
    let err = c.fetch_channel_aggregate_raw("missing", None).await.unwrap_err();
    assert!(matches!(err, DooverError::NotFound(_)), "got {err:?}");

    // Through the ChannelBackend trait a missing channel reads as None.
    let agg = ChannelBackend::fetch_channel_aggregate(&c, "missing").await.unwrap();
    assert_eq!(agg, None);
}

#[tokio::test]
async fn client_errors_map_to_http_with_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/c/messages/9"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let err = client(&server).fetch_message("c", 9, None).await.unwrap_err();
    match err {
        DooverError::Http { code, message } => {
            assert_eq!(code, 403);
            assert_eq!(message, "forbidden");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn server_errors_are_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/c/aggregate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/c/aggregate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"ok": true}})))
        .expect(1)
        .mount(&server)
        .await;

    let agg = client(&server).fetch_channel_aggregate_raw("c", None).await.unwrap();
    assert_eq!(agg["data"]["ok"], json!(true));
}

#[tokio::test]
async fn update_aggregate_patch_with_flags_and_replace_keys() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/agents/42/channels/ui_state/aggregate"))
        .and(query_param("log_update", "true"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let opts = AggregateOptions {
        save_log: true,
        replace_keys: vec!["state.children.my_app".to_string(), "state.other".to_string()],
        ..Default::default()
    };
    client(&server)
        .update_channel_aggregate_http("ui_state", &json!({"state": {}}), &opts, None)
        .await
        .unwrap();

    // The repeated `replace=` list rides in the query (pydoover doseq).
    let reqs = server.received_requests().await.unwrap();
    let replaces: Vec<String> = reqs[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "replace")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(replaces, vec!["state.children.my_app", "state.other"]);
}

#[tokio::test]
async fn replace_data_uses_put() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/42/channels/c/aggregate"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let opts = AggregateOptions { replace_data: true, ..Default::default() };
    client(&server)
        .update_channel_aggregate_http("c", &json!({"a": 1}), &opts, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_message_wraps_data_and_ts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents/42/channels/c/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "12345678901234567",
            "author_id": "42",
            "channel": {"agent_id": 42, "name": "c"},
            "data": {"v": 1},
        })))
        .expect(2)
        .mount(&server)
        .await;

    let c = client(&server);
    c.create_message_http("c", &json!({"v": 1}), Some(1751000000000), None).await.unwrap();
    // Trait path: returns the (string) snowflake id parsed to u64.
    let id = ChannelBackend::create_message(&c, "c", &json!({"v": 1})).await.unwrap();
    assert_eq!(id, 12345678901234567);

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(body_json(&reqs[0]), json!({"data": {"v": 1}, "ts": 1751000000000u64}));
    assert_eq!(body_json(&reqs[1]), json!({"data": {"v": 1}}));
}

#[tokio::test]
async fn update_message_patch_wraps_data() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/agents/42/channels/c/messages/77"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    ChannelBackend::update_message(
        &client(&server),
        "c",
        77,
        &json!({"v": 2}),
        &UpdateMessageOptions::default(),
    )
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(body_json(&reqs[0]), json!({"data": {"v": 2}}));
}

#[tokio::test]
async fn list_messages_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/42/channels/c/messages"))
        .and(query_param("before", "100"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": "1"}])))
        .expect(1)
        .mount(&server)
        .await;

    let query = ListMessagesQuery {
        before: Some(100),
        limit: Some(5),
        field_names: vec!["a".into(), "b".into()],
        ..Default::default()
    };
    let messages = client(&server).list_messages("c", &query, None).await.unwrap();
    assert_eq!(messages.len(), 1);

    let reqs = server.received_requests().await.unwrap();
    let fields: Vec<String> = reqs[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "field_name")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(fields, vec!["a", "b"]);
}

#[tokio::test]
async fn subscription_and_schedule_info_endpoints() {
    let server = MockServer::start().await;
    let info = json!({
        "agent_id": "42",
        "organisation_id": 7,
        "app_key": "my_app",
        "deployment_config": {"APP_ID": "app-1"},
        "ui_state": null,
        "ui_cmds": null,
        "tag_values": {"my_app": {}},
        "connection_data": {},
        "token": "full-token",
    });
    // SNS subscription IDs are ARNs — colons ride in the path unescaped.
    Mock::given(method("GET"))
        .and(path("/processors/subscriptions/arn:aws:sns:ap-southeast-2:1:t:u"))
        .respond_with(ResponseTemplate::new(200).set_body_json(info.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/processors/schedules/sched-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(info))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    let sub = c.fetch_subscription_info("arn:aws:sns:ap-southeast-2:1:t:u").await.unwrap();
    assert_eq!(sub.agent_id, 42);
    assert_eq!(sub.organisation_id, Some(7));
    assert_eq!(sub.app_key, "my_app");
    assert_eq!(sub.token, "full-token");

    let sched = c.fetch_schedule_info("sched-1").await.unwrap();
    assert_eq!(sched.app_key, "my_app");
}

#[tokio::test]
async fn gzip_compresses_large_bodies_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents/42/channels/c/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
        .expect(2)
        .mount(&server)
        .await;

    let c = client(&server);
    let big = json!({"blob": "x".repeat(200)});
    c.create_message_http("c", &big, None, None).await.unwrap();
    let small = json!({"v": 1});
    c.create_message_http("c", &small, None, None).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let enc = |r: &wiremock::Request| {
        r.headers
            .get("content-encoding")
            .map(|v| v.to_str().unwrap_or_default().to_string())
    };
    // ≥ 50 bytes → gzip-encoded body that decompresses to the JSON payload.
    assert_eq!(enc(&reqs[0]).as_deref(), Some("gzip"));
    assert_eq!(body_json(&reqs[0]), json!({"data": big}));
    // < 50 bytes → sent plain.
    assert_eq!(enc(&reqs[1]), None);
    assert_eq!(body_json(&reqs[1]), json!({"data": small}));
}

#[tokio::test]
async fn ping_connection_writes_message_and_aggregate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents/42/channels/doover_connection/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/agents/42/channels/doover_connection/aggregate"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let args = doover::api::PingConnectionArgs {
        online_at_ms: 1751000000000,
        connection_status: doover::models::ConnectionStatus::PeriodicUnknown,
        determination: doover::models::ConnectionDetermination::Online,
        ping_at_ms: Some(1751000001000),
        user_agent: Some("doover-rs-processor,app_key=my_app".into()),
        ip_address: None,
        agent_id: None,
    };
    client(&server).ping_connection_at(&args).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let expected = json!({
        "status": {
            "status": "PeriodicUnknown",
            "last_online": 1751000000000u64,
            "last_ping": 1751000001000u64,
            "user_agent": "doover-rs-processor,app_key=my_app",
            "ip": null,
        },
        "determination": "Online",
    });
    assert_eq!(body_json(&reqs[0]), json!({"data": expected}));
    assert_eq!(body_json(&reqs[1]), expected);
}

#[tokio::test]
async fn invoking_channel_guard_blocks_recursion() {
    // No mocks mounted: the guard must reject before any HTTP happens.
    let server = MockServer::start().await;
    let c = client(&server);
    c.set_app_key("my_app");
    c.set_invoking_channel(Some("some_channel".to_string()));

    let err = c.create_message_http("some_channel", &json!({"x": 1}), None, None).await;
    assert!(err.is_err(), "publishing to the invoking channel must fail");

    // tag_values is allowed — but only within this app's key.
    c.set_invoking_channel(Some("tag_values".to_string()));
    let err = c
        .create_message_http("tag_values", &json!({"other_app": {"x": 1}}), None, None)
        .await;
    assert!(err.is_err(), "cross-app tag_values write must fail");

    Mock::given(method("POST"))
        .and(path("/agents/42/channels/tag_values/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
        .expect(1)
        .mount(&server)
        .await;
    c.create_message_http("tag_values", &json!({"my_app": {"x": 1}}), None, None)
        .await
        .unwrap();
}
