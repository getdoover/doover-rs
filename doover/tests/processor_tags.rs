//! `ProcessorTags` semantics over the in-memory `MockBackend` — a direct
//! port of pydoover `tests/test_tags.py::TestProcessorImmediateLog`
//! (immediate-log gating + the full LogMode matrix).

use std::sync::Arc;

use serde_json::{json, Value};

use doover::processor::{LogMode, ProcessorTags, SetProcessorTagOptions};
use doover::testing::MockBackend;

const TAG_CHANNEL: &str = "tag_values";

fn manager(
    backend: &Arc<MockBackend>,
    tag_values: Value,
    record_tag_update: bool,
) -> ProcessorTags {
    ProcessorTags::new(
        "test_app",
        backend.clone() as Arc<dyn doover::ChannelBackend>,
        1,
        tag_values,
        record_tag_update,
    )
}

fn set_logged(tags: &ProcessorTags, key: &str, value: Value) {
    tags.set_tag_with(key, value, &SetProcessorTagOptions { log: true, ..Default::default() });
}

#[tokio::test]
async fn log_true_forces_message_when_record_disabled() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({}), false);

    set_logged(&tags, "voltage", json!(13.2));
    tags.commit_tags().await.unwrap();

    let messages = backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].channel, TAG_CHANNEL);
    assert_eq!(messages[0].data, json!({"test_app": {"voltage": 13.2}}));
}

#[tokio::test]
async fn log_false_respects_record_disabled() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({}), false);

    tags.set_tag("voltage", json!(13.2));
    tags.commit_tags().await.unwrap();

    // Aggregate updates but no message is created.
    assert!(backend.messages().is_empty());
    assert!(!backend.aggregate_writes().is_empty());
}

#[tokio::test]
async fn default_mode_is_always_and_logs_full_aggregate() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0, "current": 5.0}}), true);
    assert_eq!(tags.log_mode(), LogMode::Always);

    tags.set_tag("voltage", json!(13.2));
    tags.commit_tags().await.unwrap();

    // ALWAYS (the historical default) logs the whole aggregate.
    let messages = backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, json!({"test_app": {"voltage": 13.2, "current": 5.0}}));
}

#[tokio::test]
async fn only_changed_logs_dirty_subset() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0, "current": 5.0}}), true);
    tags.set_log_mode(LogMode::OnlyChanged);

    // voltage moves; current is re-set to the same value.
    tags.set_tag("voltage", json!(13.2));
    tags.set_tag("current", json!(5.0));
    tags.commit_tags().await.unwrap();

    // Aggregate carries full current state...
    let writes = backend.aggregate_writes();
    assert_eq!(
        writes.last().unwrap().data,
        json!({"test_app": {"voltage": 13.2, "current": 5.0}})
    );
    // ...but only the changed tag is logged.
    let messages = backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, json!({"test_app": {"voltage": 13.2}}));
}

#[tokio::test]
async fn only_changed_skips_everything_when_nothing_changed() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0}}), true);
    tags.set_log_mode(LogMode::OnlyChanged);

    tags.set_tag("voltage", json!(12.0)); // same value
    tags.commit_tags().await.unwrap();

    assert!(backend.messages().is_empty());
    assert!(backend.aggregate_writes().is_empty()); // nothing to store either
}

#[tokio::test]
async fn only_set_logs_reasserted_values() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0, "current": 5.0}}), true);
    tags.set_log_mode(LogMode::OnlySet);

    tags.set_tag("voltage", json!(13.2)); // changed
    tags.set_tag("current", json!(5.0)); // re-asserted, unchanged
    tags.commit_tags().await.unwrap();

    // Both the changed and the re-asserted tag are logged...
    let messages = backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, json!({"test_app": {"voltage": 13.2, "current": 5.0}}));
    // ...and the aggregate got the full current state.
    let writes = backend.aggregate_writes();
    assert_eq!(
        writes.last().unwrap().data,
        json!({"test_app": {"voltage": 13.2, "current": 5.0}})
    );
}

#[tokio::test]
async fn only_set_logs_even_with_no_change() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0}}), true);
    tags.set_log_mode(LogMode::OnlySet);

    tags.set_tag("voltage", json!(12.0)); // same value, still "set"
    tags.commit_tags().await.unwrap();

    let messages = backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, json!({"test_app": {"voltage": 12.0}}));
    // Nothing moved, so no aggregate write.
    assert!(backend.aggregate_writes().is_empty());
}

#[tokio::test]
async fn never_suppresses_log_but_updates_aggregate() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0}}), true);
    tags.set_log_mode(LogMode::Never);

    // Even an explicit log=true is suppressed by NEVER.
    set_logged(&tags, "voltage", json!(13.2));
    tags.commit_tags().await.unwrap();

    assert!(backend.messages().is_empty());
    let writes = backend.aggregate_writes();
    assert_eq!(writes.last().unwrap().data, json!({"test_app": {"voltage": 13.2}}));
}

#[tokio::test]
async fn get_tag_reads_buffered_and_seeded_values() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0}}), true);

    assert_eq!(tags.get_tag("voltage"), Some(json!(12.0)));
    assert_eq!(tags.get_tag("missing"), None);
    tags.set_tag("voltage", json!(13.0));
    assert_eq!(tags.get_tag("voltage"), Some(json!(13.0)));
}

#[tokio::test]
async fn external_app_key_widens_commit_scope() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({"test_app": {"voltage": 12.0}}), true);

    tags.set_tag("voltage", json!(13.0));
    tags.set_tag_with(
        "remote",
        json!(1),
        &SetProcessorTagOptions { app_key: Some("other_app".into()), ..Default::default() },
    );
    tags.commit_tags().await.unwrap();

    // With an external write, the full multi-app payload is published.
    let writes = backend.aggregate_writes();
    assert_eq!(
        writes.last().unwrap().data,
        json!({"test_app": {"voltage": 13.0}, "other_app": {"remote": 1}})
    );
}

#[tokio::test]
async fn commit_is_idempotent_after_flush() {
    let backend = Arc::new(MockBackend::new());
    let tags = manager(&backend, json!({}), true);

    tags.set_tag("voltage", json!(1.0));
    tags.commit_tags().await.unwrap();
    tags.commit_tags().await.unwrap(); // no pending changes → no-op

    assert_eq!(backend.aggregate_writes().len(), 1);
    assert_eq!(backend.messages().len(), 1);
}
