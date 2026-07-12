//! Replays machine-generated pydoover fixtures (tests/compat/fixtures/ at
//! the repo root, produced by scripts/gen_compat_fixtures.py) against the
//! Rust ports. This is the no-IDL compatibility contract: if pydoover's
//! behavior changes intentionally, regenerate the fixtures and review the
//! diff — never hand-edit them.

use std::path::PathBuf;

use serde_json::Value;

use doover::docker::validate_payload;
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
