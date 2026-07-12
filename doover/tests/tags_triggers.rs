//! The `log_on` trigger DSL end-to-end: the `#[tag(log_on(...))]` attribute
//! grammar (compile-pass + declared-trigger assertions), trigger
//! registration through `TagsCollection::attach`, and the fire→log
//! behaviour against the in-process fake device agent — a fired trigger
//! promotes the set to the immediate-log bucket, which `commit_tags`
//! flushes as a `tag_values` channel message (pydoover
//! `TestTriggerEndToEnd`).
//!
//! Also covers containers inside `#[derive(Ui)]` trees: depth-first
//! position assignment and recursive interaction collection.
#![cfg(feature = "macros")]

mod common;

use std::sync::Arc;

use serde_json::{json, Value};

use common::spawn_fake_agent;
use doover::tags::{LogTrigger, SetTagOptions, Tag, TagsCollection, TagsRuntime};
use doover::ui::{Button, Container, NumericVariable, Submodule, UiElement, UiTree};
use doover::{DeviceAgentClient, SubscriptionHub, Tags, Ui};

/// The full `log_on(...)` attribute grammar in one collection.
#[derive(Tags)]
struct TriggerTags {
    #[tag(live, log_on(cross(100.0)))]
    voltage: Tag<f64>,
    #[tag(log_on(cross(50, 100, deadband = 4.0)))]
    temp: Tag<f64>,
    #[tag(log_on(rise(100.0), fall(10.0)))]
    combined: Tag<f64>,
    #[tag(log_on(delta(amount = 5.0)))]
    abs_delta: Tag<f64>,
    #[tag(log_on(delta(percent = 10)))]
    pct_delta: Tag<i64>,
    #[tag(default = None, log_on(any_change))]
    fault: Tag<bool>,
    #[tag(log_on(enter("error"), exit("ok")))]
    state: Tag<String>,
}

#[test]
fn attribute_grammar_declares_pydoover_equivalent_triggers() {
    let tags = TriggerTags::detached();

    assert_eq!(tags.voltage.log_on(), &[LogTrigger::cross(100.0)]);
    assert!(tags.voltage.is_live());

    // thresholds are sorted; deadband carried (pydoover `_Crossing.__init__`)
    assert_eq!(
        tags.temp.log_on(),
        &[LogTrigger::Cross { thresholds: vec![50.0, 100.0], deadband: 4.0 }]
    );
    assert_eq!(
        tags.combined.log_on(),
        &[LogTrigger::rise(100.0), LogTrigger::fall(10.0)]
    );
    assert_eq!(tags.abs_delta.log_on(), &[LogTrigger::delta_amount(5.0)]);
    assert_eq!(tags.pct_delta.log_on(), &[LogTrigger::delta_percent(10.0)]);
    assert_eq!(tags.fault.log_on(), &[LogTrigger::any_change()]);
    assert_eq!(
        tags.state.log_on(),
        &[LogTrigger::enter(json!("error")), LogTrigger::exit(json!("ok"))]
    );
}

#[test]
fn with_log_on_rejects_mismatched_types_at_declaration() {
    // The derive rejects these at compile time; the runtime builder panics
    // like pydoover's declaration-time TypeError.
    let err = std::panic::catch_unwind(|| {
        Tag::<bool>::declared("fault", false, None)
            .with_log_on(vec![LogTrigger::delta_amount(1.0)]);
    })
    .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("log_on accepts"), "{msg}");
}

async fn setup_runtime(
    state: &Arc<common::FakeAgentState>,
    uri: &str,
) -> (Arc<TagsRuntime>, SubscriptionHub) {
    state.seed_aggregate("dv-ui-sub", json!({}));
    let client = DeviceAgentClient::connect(uri.to_string()).await.unwrap().with_app_id("test_app");
    let hub = SubscriptionHub::new(client.clone());
    let tags = Arc::new(TagsRuntime::new(client, "test_app"));
    tags.setup(&hub).await;
    (tags, hub)
}

fn tag_messages(state: &common::FakeAgentState) -> Vec<Value> {
    state
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.channel == "tag_values")
        .map(|m| m.data.clone())
        .collect()
}

/// Flush a throwaway tag through the runtime so the *periodic* log clock is
/// primed (pydoover's `_last_tag_log_time` starts at 0, so the first commit
/// always emits a periodic log; after this, periodic flushes are gated for
/// 15 minutes and every further message must be an immediate — i.e.
/// trigger/`log=true` — log).
async fn prime_periodic_log(state: &Arc<common::FakeAgentState>, runtime: &TagsRuntime) {
    runtime
        .set_nested_tags(json!({"test_app": {"prime": 1}}), &SetTagOptions::default())
        .await
        .unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(state), vec![json!({"test_app": {"prime": 1}})]);
    state.messages.lock().unwrap().clear();
}

/// pydoover `TestTriggerEndToEnd::
/// test_threshold_crossing_creates_message_via_docker_manager`.
#[tokio::test]
async fn threshold_crossing_creates_message_via_runtime() {
    let (state, uri) = spawn_fake_agent().await;
    state.seed_aggregate("tag_values", json!({}));
    let (runtime, _hub) = setup_runtime(&state, &uri).await;

    let tags = TriggerTags::attach(runtime.clone());
    prime_periodic_log(&state, &runtime).await;

    // Below the threshold: aggregate write only, no logged message.
    tags.voltage.set(80.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert!(tag_messages(&state).is_empty(), "80 is below the 100 threshold");

    // Crossing up promotes the set to an immediate log message.
    tags.voltage.set(120.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state), vec![json!({"test_app": {"voltage": 120.0}})]);

    // Staying above does not re-log.
    tags.voltage.set(130.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state).len(), 1);

    // Crossing back down logs again.
    tags.voltage.set(70.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state).last(), Some(&json!({"test_app": {"voltage": 70.0}})));
}

/// pydoover evaluates triggers *before* the manager's `only_if_changed`
/// check: a fired trigger whose value matches the stored state writes and
/// logs nothing (`set_tags` returns early).
#[tokio::test]
async fn fired_trigger_on_unchanged_value_logs_nothing() {
    let (state, uri) = spawn_fake_agent().await;
    state.seed_aggregate("tag_values", json!({"test_app": {"abs_delta": 5.0}}));
    let (runtime, _hub) = setup_runtime(&state, &uri).await;

    let tags = TriggerTags::attach(runtime.clone());
    prime_periodic_log(&state, &runtime).await;
    let writes_after_prime = state.aggregate_writes.lock().unwrap().len();

    // Delta's first evaluation always fires, but the value equals the synced
    // aggregate → only_if_changed drops the write, message included.
    tags.abs_delta.set(5.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert!(tag_messages(&state).is_empty());
    assert_eq!(
        state.aggregate_writes.lock().unwrap().len(),
        writes_after_prime,
        "unchanged value must not publish"
    );

    // ... and the baseline was still seeded: a +4 move stays silent, +5 logs.
    tags.abs_delta.set(9.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert!(tag_messages(&state).is_empty(), "diff 4 < amount 5");
    tags.abs_delta.set(10.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state), vec![json!({"test_app": {"abs_delta": 10.0}})]);
}

/// An explicit `set_logged` still advances trigger state (pydoover always
/// evaluates, then ORs `log`).
#[tokio::test]
async fn explicit_log_combines_with_triggers() {
    let (state, uri) = spawn_fake_agent().await;
    state.seed_aggregate("tag_values", json!({}));
    let (runtime, _hub) = setup_runtime(&state, &uri).await;
    let tags = TriggerTags::attach(runtime.clone());
    prime_periodic_log(&state, &runtime).await;

    tags.voltage.set_logged(120.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state).len(), 1);

    // The crossing state advanced during the logged set: staying above is
    // silent, so no second message.
    tags.voltage.set(125.0).await.unwrap();
    runtime.commit_tags().await.unwrap();
    assert_eq!(tag_messages(&state).len(), 1);
}

/// Multi-tag writes (`set_nested_tags`) bypass trigger evaluation, exactly
/// like pydoover, where only `Tags._set_tag_value` (single tag) consults
/// `log_on`.
#[tokio::test]
async fn nested_multi_tag_writes_skip_triggers() {
    let (state, uri) = spawn_fake_agent().await;
    state.seed_aggregate("tag_values", json!({}));
    let (runtime, _hub) = setup_runtime(&state, &uri).await;
    let _tags = TriggerTags::attach(runtime.clone());
    prime_periodic_log(&state, &runtime).await;

    runtime
        .set_nested_tags(json!({"test_app": {"voltage": 500.0}}), &SetTagOptions::default())
        .await
        .unwrap();
    runtime.commit_tags().await.unwrap();
    assert!(tag_messages(&state).is_empty(), "bulk writes do not consult log_on");
}

// ---------------------------------------------------------------------------
// Containers in #[derive(Ui)] trees
// ---------------------------------------------------------------------------

#[derive(Ui)]
struct ContainerUi {
    top: NumericVariable,
    group: Submodule,
    stop: Button,
}

impl doover::ui::UiBuild for ContainerUi {
    type Tags = ();

    fn build(_tags: &()) -> Self {
        Self {
            top: NumericVariable::new("Top"),
            group: Submodule::new("Pump Details")
                .child(NumericVariable::new("Speed"))
                .child(Container::new("Inner").child(Button::new("Deep Reset")))
                .child(Button::new("Reset")),
            stop: Button::new("Stop"),
        }
    }
}

#[test]
fn container_fields_finalize_depth_first_and_collect_interactions() {
    let mut ui = <ContainerUi as doover::ui::UiBuild>::build(&());
    ui.finalize();

    // pydoover construction order: top(51); group's children speed(52),
    // inner's deep_reset(53), inner(54), reset(55); group(56); stop(57).
    assert_eq!(UiElement::position(&ui.top), Some(51));
    assert_eq!(UiElement::position(&ui.group), Some(56));
    assert_eq!(UiElement::position(&ui.stop), Some(57));
    let group_json = ui.group.to_json();
    assert_eq!(group_json["children"]["speed"]["position"], json!(52));
    assert_eq!(group_json["children"]["inner"]["position"], json!(54));
    assert_eq!(group_json["children"]["inner"]["children"]["deep_reset"]["position"], json!(53));
    assert_eq!(group_json["children"]["reset"]["position"], json!(55));

    // Interactions are collected recursively (pydoover UI.get_interactions).
    assert_eq!(ui.interaction_names(), ["deep_reset", "reset", "stop"]);

    // finalize is idempotent.
    ui.finalize();
    assert_eq!(UiElement::position(&ui.stop), Some(57));
}
