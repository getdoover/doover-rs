//! Integration tests driving the real gRPC transport against an in-process
//! fake device agent (`tests/common/mod.rs`): SubscriptionHub seeding /
//! dispatch / reconnect, TagsRuntime buffering / commit semantics, the
//! declarative Application runtime (ui_state publish + ui_cmds command
//! dispatch), and the RPC manager.

mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

use common::spawn_fake_agent;
use doover::tags::{KeyPath, SetTagOptions, TagsRuntime};
use doover::{DeviceAgentClient, Event, EventSubscription, SubscriptionHub};

const WAIT: Duration = Duration::from_secs(5);

async fn recv_event(rx: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    timeout(WAIT, rx.recv()).await.expect("timed out waiting for event").expect("channel closed")
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64
}

#[tokio::test]
async fn hub_seeds_missing_channel_and_dispatches_events() {
    let (state, uri) = spawn_fake_agent().await;
    let client = DeviceAgentClient::connect(uri).await.unwrap().with_app_id("test_app");
    let hub = SubscriptionHub::new(client);

    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.subscribe(
        "chan_a",
        EventSubscription::ALL,
        Arc::new(move |ev: &Event| {
            let _ = tx.send(ev.clone());
        }),
    );

    // The channel didn't exist: the hub must create it (empty aggregate) and
    // deliver a synthetic ChannelSync with the boot state.
    let sync = recv_event(&mut rx).await;
    assert!(sync.is_channel_sync());
    assert_eq!(sync.aggregate_data(), Some(&json!({})));
    assert!(hub.wait_for_channels_sync(&["chan_a"], WAIT).await);
    {
        let writes = state.aggregate_writes.lock().unwrap();
        assert_eq!(writes[0].channel, "chan_a");
        assert_eq!(writes[0].data, json!({}));
    }

    // A live aggregate update reaches the callback and refreshes the cache.
    timeout(WAIT, async {
        loop {
            if state.stream_count("chan_a") > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("hub never opened its event stream");
    state
        .publish_aggregate_update("chan_a", json!({"x": 1}), json!({"x": 1}))
        .await;
    let update = recv_event(&mut rx).await;
    assert!(update.is_aggregate_update());
    assert_eq!(update.aggregate_data(), Some(&json!({"x": 1})));
    assert_eq!(hub.cached_aggregate("chan_a"), Some(json!({"x": 1})));
}

#[tokio::test]
async fn hub_reconnects_after_stream_drop() {
    let (state, uri) = spawn_fake_agent().await;
    state.seed_aggregate("chan_b", json!({"seed": true}));
    let client = DeviceAgentClient::connect(uri).await.unwrap();
    let hub = SubscriptionHub::new(client);

    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.subscribe(
        "chan_b",
        EventSubscription::ALL,
        Arc::new(move |ev: &Event| {
            let _ = tx.send(ev.clone());
        }),
    );
    let sync = recv_event(&mut rx).await;
    assert_eq!(sync.aggregate_data(), Some(&json!({"seed": true})));

    // Kill the live stream; the hub should reconnect with backoff and open a
    // second stream, after which events flow again.
    timeout(WAIT, async {
        while state.stream_count("chan_b") == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no initial stream");
    state.drop_streams();
    timeout(Duration::from_secs(10), async {
        while state.stream_count("chan_b") == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("hub did not reconnect after stream drop");

    state
        .publish_aggregate_update("chan_b", json!({"seed": true, "y": 2}), json!({"y": 2}))
        .await;
    let update = recv_event(&mut rx).await;
    assert!(update.is_aggregate_update());
    assert_eq!(update.aggregate_data(), Some(&json!({"seed": true, "y": 2})));
}

async fn setup_tags(
    state: &Arc<common::FakeAgentState>,
    uri: &str,
    app_key: &str,
) -> (Arc<TagsRuntime>, SubscriptionHub) {
    state.seed_aggregate("tag_values", json!({}));
    state.seed_aggregate("dv-ui-sub", json!({}));
    let client = DeviceAgentClient::connect(uri.to_string()).await.unwrap().with_app_id(app_key);
    let hub = SubscriptionHub::new(client.clone());
    let tags = Arc::new(TagsRuntime::new(client, app_key));
    tags.setup(&hub).await;
    (tags, hub)
}

fn tag_value_writes(state: &common::FakeAgentState) -> Vec<Value> {
    state
        .aggregate_writes
        .lock()
        .unwrap()
        .iter()
        .filter(|w| w.channel == "tag_values")
        .map(|w| w.data.clone())
        .collect()
}

#[tokio::test]
async fn tags_buffer_until_commit_and_skip_unchanged() {
    let (state, uri) = spawn_fake_agent().await;
    let (tags, _hub) = setup_tags(&state, &uri, "my_app").await;

    tags.set_tag("my_app", "level", json!(5.5), &SetTagOptions::default()).await.unwrap();
    assert_eq!(tag_value_writes(&state).len(), 0, "set_tag must buffer, not write");
    assert_eq!(tags.get_tag("my_app", "level"), Some(json!(5.5)), "pending overlay readable");

    tags.commit_tags().await.unwrap();
    let writes = tag_value_writes(&state);
    assert_eq!(writes, vec![json!({"my_app": {"level": 5.5}})]);
    // Nobody has the app open → the slow (15-min) max-age applies.
    assert_eq!(
        state.aggregate_writes.lock().unwrap().last().unwrap().max_age_secs,
        60.0 * 15.0
    );

    // Re-setting the same value is suppressed by only_if_changed.
    tags.set_tag("my_app", "level", json!(5.5), &SetTagOptions::default()).await.unwrap();
    tags.commit_tags().await.unwrap();
    assert_eq!(tag_value_writes(&state).len(), 1, "unchanged value must not re-publish");

    // A changed value publishes again.
    tags.set_tag("my_app", "level", json!(6.0), &SetTagOptions::default()).await.unwrap();
    tags.commit_tags().await.unwrap();
    assert_eq!(tag_value_writes(&state).last(), Some(&json!({"my_app": {"level": 6.0}})));
}

#[tokio::test]
async fn logged_tags_become_messages_on_commit() {
    let (state, uri) = spawn_fake_agent().await;
    let (tags, _hub) = setup_tags(&state, &uri, "my_app").await;

    let log_opts = SetTagOptions { log: true, ..Default::default() };
    tags.set_tag("my_app", "pump_on", json!(true), &log_opts).await.unwrap();
    assert!(state.messages.lock().unwrap().is_empty());

    tags.commit_tags().await.unwrap();
    let messages = state.messages.lock().unwrap();
    assert_eq!(messages.len(), 1, "log=true flushes as an immediate message");
    assert_eq!(messages[0].channel, "tag_values");
    assert_eq!(messages[0].data, json!({"my_app": {"pump_on": true}}));
}

#[tokio::test]
async fn live_tags_stream_as_oneshots_while_watched() {
    let (state, uri) = spawn_fake_agent().await;
    // A user has live mode open on my_app.level (fresh timestamp).
    state.seed_aggregate(
        "dv-ui-sub",
        json!({"live_tag_open": {"user1": {"ts": now_ms(), "tags": ["my_app.level"]}}}),
    );
    state.seed_aggregate("tag_values", json!({}));
    let client = DeviceAgentClient::connect(uri.clone()).await.unwrap().with_app_id("my_app");
    let hub = SubscriptionHub::new(client.clone());
    let tags = Arc::new(TagsRuntime::new(client, "my_app"));
    tags.setup(&hub).await;
    tags.set_live_tags([KeyPath::scoped("my_app", ["level"])]);

    tags.set_tag("my_app", "level", json!(7.25), &SetTagOptions::default()).await.unwrap();
    tags.commit_tags().await.unwrap();

    // App open (live claim counts as observed) → fast max-age is NOT implied;
    // but the live tag must go out as a one-shot.
    let oneshots = state.oneshots.lock().unwrap();
    assert_eq!(oneshots.len(), 1);
    assert_eq!(oneshots[0].channel, "tag_values");
    assert_eq!(oneshots[0].data, json!({"my_app": {"level": 7.25}}));
    assert!(tags.is_live_tag_open("level", None));
    assert!(tags.is_being_observed());
    assert!(!tags.is_app_open(), "live-tag claim alone does not open the app");
}

// ---------------------------------------------------------------------------
// RPC manager over the fake agent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_request_and_call_round_trip() {
    let (state, uri) = spawn_fake_agent().await;
    let client = DeviceAgentClient::connect(uri).await.unwrap().with_app_id("test_app");
    let hub = SubscriptionHub::new(client.clone());
    let backend: Arc<dyn doover::ChannelBackend> = Arc::new(client.clone());
    let rpc = Arc::new(doover::RpcManager::new(backend, Some("test_app".to_string())));
    doover::docker::wire_rpc(&rpc, &hub);

    // Handler side: registering subscribes dv-rpc through the hub.
    rpc.register(Some("dv-rpc"), "add", |_ctx, payload: Value| async move {
        let a = payload.get("a").and_then(Value::as_i64).unwrap_or(0);
        let b = payload.get("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(json!({"sum": a + b}))
    });
    timeout(WAIT, async {
        while state.stream_count("dv-rpc") == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("rpc manager never subscribed dv-rpc");

    // An incoming request dispatches the handler and updates the request
    // message with pydoover's success payload.
    state
        .publish_event(
            "dv-rpc",
            "MessageCreate",
            json!({
                "id": 42,
                "author_id": 9,
                "data": {
                    "type": "rpc",
                    "method": "add",
                    "request": {"a": 1, "b": 2},
                    "status": {"code": "sent"},
                    "response": {},
                },
            }),
        )
        .await;
    timeout(WAIT, async {
        while state.message_updates.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("no rpc response message update");
    {
        let updates = state.message_updates.lock().unwrap();
        assert_eq!(updates[0].channel, "dv-rpc");
        assert_eq!(updates[0].message_id, 42);
        assert!(!updates[0].replace_data, "responses merge into the request message");
        assert_eq!(
            serde_json::to_string(&updates[0].data).unwrap(),
            r#"{"status":{"code":"success","message":null},"response":{"sum":3}}"#
        );
    }

    // Requests stamped for another app are ignored.
    state
        .publish_event(
            "dv-rpc",
            "MessageCreate",
            json!({
                "id": 43,
                "data": {
                    "type": "rpc",
                    "method": "add",
                    "app_key": "someone_else",
                    "request": {"a": 5, "b": 5},
                    "status": {"code": "sent"},
                    "response": {},
                },
            }),
        )
        .await;

    // Caller side: `call` creates the request message (pydoover's exact
    // shape, app_key last) and resolves from the MessageUpdate event.
    let caller = rpc.clone();
    let call = tokio::spawn(async move {
        caller
            .call(
                "ping",
                Some(json!({"x": 1})),
                "dv-rpc",
                Some("other_app"),
                Some(Duration::from_secs(5)),
            )
            .await
    });
    let message_id = timeout(WAIT, async {
        loop {
            let id = {
                let messages = state.messages.lock().unwrap();
                messages
                    .iter()
                    .position(|m| {
                        m.channel == "dv-rpc"
                            && m.data.get("method").and_then(Value::as_str) == Some("ping")
                    })
                    .map(|idx| (idx + 1) as u64) // fake ids are 1-based creation order
            };
            if let Some(id) = id {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("call never created its request message");
    {
        let messages = state.messages.lock().unwrap();
        let request = &messages[(message_id - 1) as usize];
        assert_eq!(
            serde_json::to_string(&request.data).unwrap(),
            r#"{"type":"rpc","method":"ping","request":{"x":1},"status":{"code":"sent"},"response":{},"app_key":"other_app"}"#
        );
    }

    // An intermediate `acknowledged` status must not resolve the call.
    state
        .publish_event(
            "dv-rpc",
            "MessageUpdate",
            json!({
                "id": message_id,
                "data": {"status": {"code": "acknowledged", "message": {"timestamp": 1}}},
            }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!call.is_finished(), "acknowledged must keep the call pending");

    state
        .publish_event(
            "dv-rpc",
            "MessageUpdate",
            json!({
                "id": message_id,
                "data": {
                    "status": {"code": "success", "message": null},
                    "response": {"pong": true},
                },
            }),
        )
        .await;
    let result = timeout(WAIT, call).await.expect("call timed out").unwrap().unwrap();
    assert_eq!(result, json!({"pong": true}));
}

#[tokio::test]
async fn rpc_fire_and_forget_shape() {
    let (state, uri) = spawn_fake_agent().await;
    let client = DeviceAgentClient::connect(uri).await.unwrap();
    let backend: Arc<dyn doover::ChannelBackend> = Arc::new(client);
    let id = doover::RpcManager::fire_and_forget(
        backend.as_ref(),
        "dv-rpc",
        "reboot",
        Some(json!({"delay": 5})),
    )
    .await
    .unwrap();
    assert_eq!(id, 1);
    let messages = state.messages.lock().unwrap();
    // pydoover's fire_and_forget key order differs from `call`:
    // request before method.
    assert_eq!(
        serde_json::to_string(&messages[0].data).unwrap(),
        r#"{"type":"rpc","request":{"delay":5},"method":"reboot","status":{"code":"sent"},"response":{}}"#
    );
}

// ---------------------------------------------------------------------------
// Declarative Application runtime: ui_state publish + ui_cmds dispatch
// ---------------------------------------------------------------------------

#[cfg(feature = "macros")]
mod declarative_app {
    use super::*;
    use std::sync::Mutex;

    use doover::tags::Tag;
    use doover::ui::{NumericVariable, Switch, UiBuild};
    use doover::{AppContext, Application, RunOptions, UiCommand};

    /// Commands received by `on_ui_command` (apps are constructed by the
    /// runner, so tests observe them through a static).
    static COMMANDS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

    #[derive(doover::Tags)]
    struct TestTags {
        #[tag(live, default = None)]
        level: Tag<f64>,
    }

    #[derive(doover::Ui)]
    struct TestUi {
        level: NumericVariable,
        pump: Switch,
    }

    impl UiBuild for TestUi {
        type Tags = TestTags;

        fn build(tags: &TestTags) -> Self {
            Self {
                level: NumericVariable::new("Level").units("%").value(&tags.level),
                pump: Switch::new("Pump"),
            }
        }
    }

    #[derive(doover::Config)]
    struct TestConfig {
        #[config(default = 2.0)]
        gain: f64,
    }

    struct TestApp {
        config: TestConfig,
        tags: TestTags,
        ui: TestUi,
        iterations: u64,
    }

    #[doover::async_trait]
    impl Application for TestApp {
        type Config = TestConfig;
        type Tags = TestTags;
        type Ui = TestUi;

        fn create(config: TestConfig, tags: TestTags, ui: TestUi) -> Self {
            Self { config, tags, ui, iterations: 0 }
        }

        fn ui(&self) -> Option<&TestUi> {
            Some(&self.ui)
        }

        fn ui_mut(&mut self) -> Option<&mut TestUi> {
            Some(&mut self.ui)
        }

        fn loop_target_period(&self) -> Duration {
            Duration::from_millis(50)
        }

        async fn main_loop(&mut self, _ctx: &AppContext) -> doover::Result<()> {
            self.iterations += 1;
            if self.iterations == 2 {
                // Runtime UI mutation: the runner must detect the schema
                // change and re-publish ui_state.
                self.ui.level.common.units = Some("m".to_string());
            }
            self.tags.level.set(self.config.gain).await
        }

        async fn on_ui_command(&mut self, _ctx: &AppContext, cmd: &UiCommand) -> doover::Result<()> {
            assert!(cmd.is(&self.ui.pump) || cmd.is(&self.ui.level));
            COMMANDS.lock().unwrap().push((cmd.name.clone(), cmd.value.clone()));
            Ok(())
        }
    }

    /// Spawn the full runner against a fake agent; returns the fake state and
    /// the runner task.
    async fn spawn_app(
        healthcheck_port: u16,
    ) -> (Arc<common::FakeAgentState>, tokio::task::JoinHandle<doover::Result<()>>) {
        let (state, uri) = spawn_fake_agent().await;
        state.seed_aggregate("tag_values", json!({}));
        state.seed_aggregate("dv-ui-sub", json!({}));

        let mut config_path = std::env::temp_dir();
        config_path.push(format!("doover-rs-runtime-app-{healthcheck_port}-{}.json", std::process::id()));
        std::fs::write(
            &config_path,
            r#"{"gain": 3.0, "APP_KEY": "test_app", "APP_DISPLAY_NAME": "Test App"}"#,
        )
        .unwrap();

        let opts = RunOptions {
            dda_uri: uri,
            plt_uri: String::new(),
            modbus_uri: String::new(),
            app_key: "test_app".to_string(),
            config_fp: Some(config_path.to_string_lossy().into_owned()),
            healthcheck_port,
            debug: false,
            error_wait: Duration::from_secs(1),
        };
        let handle = tokio::spawn(doover::run_with::<TestApp>(opts));
        (state, handle)
    }

    fn ui_state_writes(state: &common::FakeAgentState) -> Vec<Value> {
        state
            .aggregate_writes
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.channel == "ui_state")
            .map(|w| w.data.clone())
            .collect()
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, predicate: F) {
        timeout(Duration::from_secs(10), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[tokio::test]
    async fn ui_state_published_after_setup_and_on_change() {
        let (state, handle) = spawn_app(49301).await;

        // pydoover's double publish: clear the app's subtree, then set it.
        wait_for("initial ui_state double publish", || ui_state_writes(&state).len() >= 2).await;
        let writes = ui_state_writes(&state);
        assert_eq!(writes[0], json!({"state": {"children": {"test_app": null}}}));
        {
            let all = state.aggregate_writes.lock().unwrap();
            let ui_writes: Vec<_> = all.iter().filter(|w| w.channel == "ui_state").collect();
            assert_eq!(ui_writes[0].max_age_secs, -1.0, "pydoover publishes with max_age=-1");
            assert_eq!(ui_writes[1].max_age_secs, -1.0);
        }

        // The published schema has $config refs resolved against the live
        // config, and $tag/$cmds refs left for the site to resolve.
        let schema = &writes[1]["state"]["children"]["test_app"];
        assert_eq!(schema["displayString"], json!("Test App"));
        assert_eq!(schema["hidden"], json!(false), "doubled :boolean:false quirk resolves false");
        assert_eq!(schema["position"], json!(100));
        assert_eq!(schema["defaultOpen"], Value::Null);
        assert_eq!(schema["type"], json!("uiApplication"));
        assert_eq!(schema["name"], json!("test_app"));
        assert_eq!(
            schema["children"]["level"]["currentValue"],
            json!("$tag.app().level:number:null")
        );
        assert_eq!(schema["children"]["level"]["live"], json!(true));
        assert_eq!(schema["children"]["level"]["position"], json!(51));
        assert_eq!(schema["children"]["pump"]["currentValue"], json!("$cmds.app().pump"));
        assert_eq!(schema["children"]["pump"]["type"], json!("uiSwitch"));
        assert_eq!(schema["children"]["pump"]["position"], json!(52));

        // After the app mutates its UI in main_loop, the runner re-publishes
        // (another clear+set pair) — and only then.
        wait_for("re-publish after UI mutation", || ui_state_writes(&state).len() >= 4).await;
        let writes = ui_state_writes(&state);
        assert_eq!(writes[2], json!({"state": {"children": {"test_app": null}}}));
        assert_eq!(
            writes[3]["state"]["children"]["test_app"]["children"]["level"]["units"],
            json!("m")
        );

        // No further publishes while the schema is unchanged.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ui_state_writes(&state).len(), 4, "unchanged schema must not re-publish");

        // The typed config drove the tag write.
        wait_for("tag committed", || {
            state
                .aggregate_writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w.channel == "tag_values" && w.data == json!({"test_app": {"level": 3.0}}))
        })
        .await;

        handle.abort();
    }

    #[tokio::test]
    async fn ui_command_dispatches_and_writes_back() {
        let (state, handle) = spawn_app(49302).await;
        wait_for("setup publish", || ui_state_writes(&state).len() >= 2).await;
        wait_for("ui_cmds stream open", || state.stream_count("ui_cmds") > 0).await;

        // The exact command shape pydoover's UICommandsManager handles: an
        // rpc-typed MessageCreate on ui_cmds whose method is the interaction
        // name.
        state
            .publish_event(
                "ui_cmds",
                "MessageCreate",
                json!({
                    "id": 555,
                    "author_id": 9,
                    "data": {
                        "type": "rpc",
                        "method": "pump",
                        "request": true,
                        "status": {"code": "sent"},
                        "response": {},
                    },
                }),
            )
            .await;

        wait_for("command response", || !state.message_updates.lock().unwrap().is_empty()).await;

        // 1. on_ui_command fired with the command.
        assert!(COMMANDS
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "pump" && value == &json!(true)));

        // 2. Write-back into the ui_cmds aggregate ({app_key: {name: value}}).
        wait_for("ui_cmds aggregate write-back", || {
            state
                .aggregate_writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w.channel == "ui_cmds" && w.data == json!({"test_app": {"pump": true}}))
        })
        .await;

        // 3. The pydoover log message.
        wait_for("ui_cmds log message", || {
            state.messages.lock().unwrap().iter().any(|m| {
                m.channel == "ui_cmds"
                    && serde_json::to_string(&m.data).unwrap()
                        == r#"{"type":"log","app_key":"test_app","key":"pump","value":true}"#
            })
        })
        .await;

        // 4. The request message marked successful.
        {
            let updates = state.message_updates.lock().unwrap();
            let update = updates.iter().find(|u| u.message_id == 555).expect("success update");
            assert_eq!(update.channel, "ui_cmds");
            assert_eq!(
                serde_json::to_string(&update.data).unwrap(),
                r#"{"status":{"code":"success","message":null},"response":{}}"#
            );
        }

        // Commands for unknown methods or other apps are ignored entirely.
        state
            .publish_event(
                "ui_cmds",
                "MessageCreate",
                json!({
                    "id": 556,
                    "data": {"type": "rpc", "method": "unknown_thing", "request": 1,
                             "status": {"code": "sent"}, "response": {}},
                }),
            )
            .await;
        state
            .publish_event(
                "ui_cmds",
                "MessageCreate",
                json!({
                    "id": 557,
                    "data": {"type": "rpc", "method": "pump", "app_key": "other_app",
                             "request": false, "status": {"code": "sent"}, "response": {}},
                }),
            )
            .await;
        // And an aggregate update refreshes cached values without dispatching.
        state
            .publish_aggregate_update(
                "ui_cmds",
                json!({"test_app": {"pump": true}}),
                json!({"test_app": {"pump": true}}),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state.message_updates.lock().unwrap().len(), 1, "ignored commands must not respond");
        assert_eq!(COMMANDS.lock().unwrap().iter().filter(|(n, _)| n == "unknown_thing").count(), 0);

        handle.abort();
    }
}
