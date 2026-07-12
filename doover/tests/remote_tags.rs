//! Cross-app (remote) tag resolution: reading tags published by *another*
//! application off the shared `tag_values` aggregate. Covers the imperative
//! `RemoteTag::from_parts`, the declarative `TagRef`-driven
//! `RemoteTag::resolve`, whole-collection `attach_remote`, and the read-only
//! guarantee (a remote handle refuses `set`). Mirrors the pydoover
//! `RemoteTag` / `config.TagRef` contract.
#![cfg(feature = "macros")]

mod common;

use std::sync::Arc;

use serde_json::json;

use common::spawn_fake_agent;
use doover::config::TagRef;
use doover::tags::{RemoteTag, Tag, TagsCollection, TagsRuntime};
use doover::{DeviceAgentClient, SubscriptionHub, Tags};

/// A declared schema we'll bind to *another* app's namespace.
#[derive(Tags)]
struct UpstreamTags {
    #[tag(default = None)]
    ai_reading: Tag<f64>,
    #[tag(default = None)]
    online: Tag<bool>,
}

async fn setup_runtime(state: &Arc<common::FakeAgentState>, uri: &str) -> (Arc<TagsRuntime>, SubscriptionHub) {
    state.seed_aggregate("dv-ui-sub", json!({}));
    // Two apps publish into the shared tag_values aggregate; "test_app" is us.
    state.seed_aggregate(
        "tag_values",
        json!({
            "platform_interface_1": {"ai_reading": 12.5, "online": true},
            "test_app": {"own": 1.0},
        }),
    );
    let client = DeviceAgentClient::connect(uri.to_string()).await.unwrap().with_app_id("test_app");
    let hub = SubscriptionHub::new(client.clone());
    let tags = Arc::new(TagsRuntime::new(client, "test_app"));
    tags.setup(&hub).await;
    (tags, hub)
}

/// `RemoteTag::from_parts` reads another app's slot; a missing tag falls back
/// to the declared default.
#[tokio::test]
async fn remote_tag_reads_other_app_namespace() {
    let (state, uri) = spawn_fake_agent().await;
    let (rt, _hub) = setup_runtime(&state, &uri).await;

    let level = RemoteTag::<f64>::from_parts(rt.clone(), "platform_interface_1", "ai_reading", None);
    assert_eq!(level.get(), Some(12.5));
    assert_eq!(level.app_key(), "platform_interface_1");
    assert_eq!(level.tag_name(), "ai_reading");

    let online = RemoteTag::<bool>::from_parts(rt.clone(), "platform_interface_1", "online", None);
    assert_eq!(online.get(), Some(true));

    // Absent upstream tag → declared default (None here, or the supplied one).
    let missing = RemoteTag::<f64>::from_parts(rt.clone(), "platform_interface_1", "nope", None);
    assert_eq!(missing.get(), None);
    let missing_defaulted =
        RemoteTag::<f64>::from_parts(rt.clone(), "platform_interface_1", "nope", Some(json!(-1.0)));
    assert_eq!(missing_defaulted.get(), Some(-1.0));

    // Reading a non-existent app is just an absent value, not an error.
    let no_app = RemoteTag::<f64>::from_parts(rt, "ghost_app", "ai_reading", None);
    assert_eq!(no_app.get(), None);
}

/// `RemoteTag::resolve` drives cross-app resolution off a `config.TagRef`
/// binding the operator filled in.
#[tokio::test]
async fn tag_ref_resolution() {
    let (state, uri) = spawn_fake_agent().await;
    let (rt, _hub) = setup_runtime(&state, &uri).await;

    let configured = TagRef {
        reference_name: "sensor_source".into(),
        agent_id: None,
        app_name: "platform_interface_1".into(),
        tag_name: "ai_reading".into(),
    };
    let resolved = RemoteTag::<f64>::resolve(rt.clone(), &configured, None).expect("configured");
    assert_eq!(resolved.get(), Some(12.5));

    // Unconfigured reference (optional remote tag) → None.
    assert!(RemoteTag::<f64>::resolve(rt.clone(), &TagRef::default(), None).is_none());

    // Cross-agent references are not resolved yet → None (not a panic).
    let cross_agent = TagRef {
        agent_id: Some("999".into()),
        app_name: "platform_interface_1".into(),
        tag_name: "ai_reading".into(),
        ..Default::default()
    };
    assert!(RemoteTag::<f64>::resolve(rt, &cross_agent, None).is_none());
}

/// A whole `#[derive(Tags)]` schema can be bound read-only to another app's
/// key, and its handles refuse writes.
#[tokio::test]
async fn attach_remote_collection_is_read_only() {
    let (state, uri) = spawn_fake_agent().await;
    let (rt, _hub) = setup_runtime(&state, &uri).await;

    let upstream: UpstreamTags = UpstreamTags::attach_remote(rt.clone(), "platform_interface_1");
    assert!(upstream.ai_reading.is_remote());
    assert_eq!(upstream.ai_reading.get(), Some(12.5));
    assert_eq!(upstream.online.get(), Some(true));

    // Writing a remote tag is refused rather than clobbering another app.
    let err = upstream.ai_reading.set(99.0).await.unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    // The same schema attached to our own app is writable and reads our slot.
    let own: UpstreamTags = UpstreamTags::attach(rt);
    assert!(!own.ai_reading.is_remote());
    assert!(own.ai_reading.set(3.0).await.is_ok());
}
