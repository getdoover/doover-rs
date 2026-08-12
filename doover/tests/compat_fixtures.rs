//! Replays machine-generated pydoover fixtures (tests/compat/fixtures/ at
//! the repo root, produced by scripts/gen_compat_fixtures.py) against the
//! Rust ports. This is the no-IDL compatibility contract: if pydoover's
//! behavior changes intentionally, regenerate the fixtures and review the
//! diff — never hand-edit them.

use std::path::PathBuf;

use serde_json::Value;

use doover::docker::{validate_payload, IoDetails};
use doover::utils::{apply_diff, generate_diff, generate_snowflake_id_at, SnowflakeType};

fn fixture(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/compat/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"));
    serde_json::from_str(&text).expect("fixture parses")
}

#[test]
fn diff_engine_matches_pydoover() {
    let cases = fixture("diffs.json");
    assert!(cases.len() > 500, "fixture corpus unexpectedly small");
    for (i, case) in cases.iter().enumerate() {
        let do_delete = case["do_delete"].as_bool().unwrap();
        if case.get("op").and_then(Value::as_str) == Some("apply") {
            let result = apply_diff(&case["data"], &case["diff"], do_delete);
            assert_eq!(
                result, case["result"],
                "apply case {i}: data={} diff={} do_delete={do_delete}",
                case["data"], case["diff"]
            );
        } else {
            let diff = generate_diff(&case["old"], &case["new"], do_delete);
            assert_eq!(
                diff, case["diff"],
                "generate case {i}: old={} new={} do_delete={do_delete}",
                case["old"], case["new"]
            );
            let applied = apply_diff(&case["old"], &diff, do_delete);
            assert_eq!(
                applied, case["applied"],
                "roundtrip case {i}: old={} new={} do_delete={do_delete}",
                case["old"], case["new"]
            );
        }
    }
}

#[test]
fn payload_validation_matches_pydoover() {
    for (i, case) in fixture("payload_validation.json").iter().enumerate() {
        let expected = case["valid"].as_bool().unwrap();
        let actual = validate_payload(&case["payload"]).is_ok();
        assert_eq!(actual, expected, "payload case {i}: {}", case["payload"]);
    }
}

#[test]
fn snowflake_layout_matches_pydoover() {
    for (i, case) in fixture("snowflakes.json").iter().enumerate() {
        let type_id = match case["type_id"].as_u64().unwrap() {
            0 => SnowflakeType::Unknown,
            2 => SnowflakeType::Message,
            3 => SnowflakeType::Channel,
            11 => SnowflakeType::OneShotMessage,
            other => panic!("unmapped type id {other} in fixture"),
        };
        let id = generate_snowflake_id_at(
            case["unix_millis"].as_u64().unwrap(),
            type_id,
            case["region_id"].as_u64().unwrap() as u8,
            case["instance_id"].as_u64().unwrap() as u16,
            false,
        );
        assert_eq!(id, case["snowflake"].as_u64().unwrap(), "snowflake case {i}");
    }
}

/// `IoDetails::from_io_table` — the fallback a platform interface predating
/// `getIoDetails` takes (pydoover 51d436f). The synthesized device is one
/// anonymous master with no per-channel metadata, and `channels()` sorts by flat
/// channel number even though the table's own order is arbitrary.
#[test]
fn io_details_from_io_table_matches_pydoover() {
    let cases = fixture("io_details.json");
    assert!(cases.len() >= 5, "fixture corpus unexpectedly small");
    for (i, case) in cases.iter().enumerate() {
        let details = IoDetails::from_io_table(&case["io_table"]);
        let expected = &case["details"]["devices"];

        assert_eq!(
            details.devices.len(),
            expected.as_array().unwrap().len(),
            "case {i}: device count for {}",
            case["io_table"]
        );
        for (device, want) in details.devices.iter().zip(expected.as_array().unwrap()) {
            assert_eq!(device.name, want["name"].as_str().unwrap(), "case {i}: name");
            // pydoover's dataclass field is `type`; Rust can't use that name.
            assert_eq!(device.type_name, want["type"].as_str().unwrap(), "case {i}: type");
            assert_eq!(device.index, want["index"].as_i64().unwrap() as i32, "case {i}: index");
            assert_eq!(device.is_master, want["is_master"].as_bool().unwrap(), "case {i}");
            assert_eq!(device.online, want["online"].as_bool().unwrap(), "case {i}");

            let want_channels = want["channels"].as_array().unwrap();
            assert_eq!(
                device.channels.len(),
                want_channels.len(),
                "case {i}: channel count for {}",
                case["io_table"]
            );
            for (channel, want) in device.channels.iter().zip(want_channels) {
                // Channel order within a device follows the table's own key
                // order, not a sort — only `channels()` sorts.
                assert_eq!(channel.channel, want["channel"].as_i64().unwrap() as i32);
                assert_eq!(
                    channel.device_channel,
                    want["device_channel"].as_i64().unwrap() as i32
                );
                assert_eq!(channel.io_type, want["io_type"].as_str().unwrap());
                assert!(channel.kind.is_none() && want["kind"].is_null());
                assert!(channel.units.is_none() && want["units"].is_null());
                assert!(!channel.supports_events && !want["supports_events"].as_bool().unwrap());
                assert!(!channel.supports_pulse_counter);
                assert!(!channel.supports_di_config);
            }
        }

        // `master()` finds the synthesized master in every case.
        assert_eq!(
            details.master().is_some(),
            !case["master"].is_null(),
            "case {i}: master presence"
        );

        // `channels(io_type)` is sorted by flat channel number.
        for (io_type, want) in case["channels_by_type"].as_object().unwrap() {
            let got: Vec<i64> =
                details.channels(io_type).iter().map(|c| c.channel as i64).collect();
            let want: Vec<i64> = want.as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
            assert_eq!(got, want, "case {i}: channels({io_type}) for {}", case["io_table"]);
        }
    }
}

/// `channels_of` is the per-device filter, unsorted (pydoover
/// `IoDevice.channels_of`).
#[test]
fn channels_of_filters_one_devices_channels() {
    let details = IoDetails::from_io_table(&serde_json::json!({"AI": [3, 1], "DI": [0]}));
    let master = details.master().expect("synthesized master");
    assert_eq!(
        master.channels_of("AI").iter().map(|c| c.channel).collect::<Vec<_>>(),
        vec![3, 1],
        "channels_of keeps the device's own order"
    );
    assert_eq!(master.channels_of("DI").len(), 1);
    assert!(master.channels_of("AO").is_empty());
}
