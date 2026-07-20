//! End-to-end tests: run the real `doover` binary against the `doover` crate's
//! fake device agent over real gRPC.
//!
//! These cover the pydoover-compatibility surface that argument-parsing tests
//! can't reach: what actually lands on the wire, and what reaches stdout.

use std::process::Command;

use serde_json::{json, Value};

// The fake agent lives with the `doover` crate's integration tests; include it
// rather than duplicating a second tonic server here.
#[path = "../../doover/tests/common/mod.rs"]
mod common;

/// Run the CLI against `uri`, returning (stdout, stderr, success).
fn run_cli(uri: &str, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_doover"))
        .arg("--dda-uri")
        .arg(uri)
        .args(args)
        .output()
        .expect("failed to run the doover binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn update_aggregate_prints_the_aggregate_and_return_flag_silences_it() {
    let (state, uri) = common::spawn_fake_agent().await;
    state.seed_aggregate("ch", json!({"level": 1, "name": "tank"}));

    // pydoover's return_aggregate defaults True, so the merged aggregate is
    // echoed to stdout — the whole envelope, as pydoover printed it.
    let (stdout, _, ok) = run_cli(&uri, &["device_agent", "update_aggregate", "ch", r#"{"level": 42}"#]);
    assert!(ok, "update should succeed");
    let printed: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(printed["data"], json!({"level": 42, "name": "tank"}));
    assert_eq!(printed["attachments"], json!([]));
    assert!(printed.get("last_updated").is_some(), "last_updated must be present");

    // ...and passing the flag turns the echo off (pydoover's store_false).
    let (stdout, _, ok) = run_cli(
        &uri,
        &["device_agent", "update_aggregate", "ch", r#"{"level": 7}"#, "--return_aggregate"],
    );
    assert!(ok);
    assert_eq!(stdout.trim(), "", "--return_aggregate should suppress the echo");

    // Both writes still landed.
    let writes = state.aggregate_writes.lock().unwrap();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[1].data, json!({"level": 7}));
}

/// pydoover printed the whole `Aggregate` — `data`, `attachments` and
/// `last_updated` — and callers on-device read those fields off it. Printing
/// the bare payload silently breaks them.
#[tokio::test(flavor = "multi_thread")]
async fn fetch_aggregate_prints_the_whole_envelope() {
    let (state, uri) = common::spawn_fake_agent().await;
    state.seed_aggregate("ui_state", json!({"state": {"children": {}}}));
    state.seed_aggregate_meta(
        "ui_state",
        vec![common::pb::Attachment {
            filename: "snap.jpg".into(),
            content_type: "image/jpeg".into(),
            size_bytes: 2048,
            url: "https://example.invalid/snap.jpg".into(),
        }],
        1784504229751,
    );

    let (stdout, _, ok) = run_cli(&uri, &["device_agent", "fetch_channel_aggregate", "ui_state"]);
    assert!(ok, "fetch should succeed");
    let printed: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");

    assert_eq!(printed["data"], json!({"state": {"children": {}}}));
    assert_eq!(printed["last_updated"], json!(1784504229751.0));
    // pydoover's Attachment.to_dict uses `size`, not the proto's `size_bytes`.
    assert_eq!(
        printed["attachments"],
        json!([{
            "filename": "snap.jpg",
            "content_type": "image/jpeg",
            "size": 2048,
            "url": "https://example.invalid/snap.jpg",
        }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_channels_prints_the_listing_and_gates_aggregates() {
    let (state, uri) = common::spawn_fake_agent().await;
    state.seed_aggregate("alpha", json!({"level": 1}));
    state.seed_aggregate("beta", json!({"level": 2}));

    let (stdout, _, ok) = run_cli(&uri, &["device_agent", "list_channels", "--json"]);
    assert!(ok, "list_channels should succeed");
    let listing: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(listing["from_cloud"], json!(true));
    assert_eq!(
        listing["channels"],
        json!([
            {"channel_name": "alpha", "aggregate": null},
            {"channel_name": "beta", "aggregate": null},
        ]),
        "aggregates are omitted unless asked for"
    );

    let (stdout, _, ok) = run_cli(
        &uri,
        &["device_agent", "list-channels", "--include_aggregate"],
    );
    assert!(ok);
    let listing: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(
        listing["channels"],
        json!([
            {"channel_name": "alpha", "aggregate": {"level": 1}},
            {"channel_name": "beta", "aggregate": {"level": 2}},
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pydoover_compat_flags_reach_the_wire_unchanged() {
    let (state, uri) = common::spawn_fake_agent().await;

    // A command line a pydoover-era script would have produced: --json and
    // --files are accepted, and the real arguments still take effect.
    let (_, stderr, ok) = run_cli(
        &uri,
        &[
            "device_agent",
            "update_aggregate",
            "ch",
            r#"{"level": 5}"#,
            "--json",
            "--enable-traceback",
            "--files",
            "[]",
            "--save_log",
            "--max_age_secs",
            "2.5",
            "--service_name",
            "doover.DeviceAgent",
            "--dda_timeout",
            "7",
        ],
    );
    assert!(ok, "pydoover-era flags should not fail: {stderr}");
    assert!(stderr.contains("--files"), "an ignored --files should warn: {stderr}");

    let writes = state.aggregate_writes.lock().unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].data, json!({"level": 5}));
    assert!(writes[0].save_log);
    assert_eq!(writes[0].max_age_secs, 2.5);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_message_accepts_iso_and_millisecond_timestamps() {
    let (state, uri) = common::spawn_fake_agent().await;

    let (stdout, _, ok) = run_cli(
        &uri,
        &["device_agent", "create_message", "ch", "{}", "--timestamp", "2025-06-15T12:00:00Z"],
    );
    assert!(ok);
    assert!(stdout.trim().parse::<u64>().is_ok(), "should print a message id");

    let (_, _, ok) = run_cli(
        &uri,
        &["device_agent", "create_message", "ch", "{}", "--timestamp", "1749988800000"],
    );
    assert!(ok);

    let messages = state.messages.lock().unwrap();
    assert_eq!(messages.len(), 2);
    // Both spellings denote the same instant, so both stamp the same millis.
    assert_eq!(messages[0].timestamp, 1_749_988_800_000);
    assert_eq!(messages[1].timestamp, 1_749_988_800_000);
}

