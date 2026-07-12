//! Replays the machine-generated pydoover log-trigger fixtures
//! (tests/compat/fixtures/log_triggers.json, produced by
//! scripts/gen_log_trigger_fixtures.py): each case's value sequence runs
//! through `doover::tags::TriggerSet` with pydoover's exact `prev`
//! semantics (stored value, else the declared default, else "not set") and
//! must reproduce the recorded fire/no-fire decisions.

use std::path::PathBuf;

use serde_json::Value;

use doover::tags::{LogTrigger, TriggerSet};

fn fixture_cases() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/compat/fixtures/log_triggers.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"));
    serde_json::from_str(&text).expect("fixture parses")
}

/// Construct a trigger from the fixture's machine-readable spec.
fn trigger_from_spec(spec: &Value) -> LogTrigger {
    let kind = spec["kind"].as_str().unwrap();
    let thresholds = || -> Vec<f64> {
        spec["thresholds"].as_array().unwrap().iter().map(|t| t.as_f64().unwrap()).collect()
    };
    let deadband = spec.get("deadband").and_then(Value::as_f64);
    let with_deadband = |t: LogTrigger| match deadband {
        Some(d) => t.deadband(d),
        None => t,
    };
    match kind {
        "cross" => with_deadband(LogTrigger::cross(thresholds())),
        "rise" => with_deadband(LogTrigger::rise(thresholds())),
        "fall" => with_deadband(LogTrigger::fall(thresholds())),
        "delta" => match spec.get("amount").and_then(Value::as_f64) {
            Some(amount) => LogTrigger::delta_amount(amount),
            None => LogTrigger::delta_percent(
                spec["percent"].as_f64().expect("delta needs amount or percent"),
            ),
        },
        "any_change" => LogTrigger::any_change(),
        "enter" => LogTrigger::enter(spec["value"].clone()),
        "exit" => LogTrigger::exit(spec["value"].clone()),
        other => panic!("unknown trigger kind {other:?}"),
    }
}

#[test]
fn log_triggers_match_pydoover() {
    let cases = fixture_cases();
    assert!(cases.len() >= 29, "fixture corpus unexpectedly small: {}", cases.len());

    for case in &cases {
        let id = case["case"].as_str().unwrap();
        let triggers: Vec<LogTrigger> =
            case["triggers"].as_array().unwrap().iter().map(trigger_from_spec).collect();
        // "default" absent == pydoover NotSet; present (even null) is a
        // declared default.
        let default = case.get("default").cloned();
        let mut set = TriggerSet::new(triggers, default);

        let values = case["values"].as_array().unwrap();
        let expected: Vec<bool> =
            case["fired"].as_array().unwrap().iter().map(|f| f.as_bool().unwrap()).collect();
        let explicit_log: Vec<bool> = case
            .get("explicit_log")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|f| f.as_bool().unwrap()).collect())
            .unwrap_or_else(|| vec![false; values.len()]);

        // Replay pydoover `Tags._set_tag_value`: prev = stored value (the
        // TriggerSet applies the declared-default fallback), evaluate every
        // trigger, OR with the caller's explicit log flag, then store.
        let mut store: Option<Value> = None;
        for (i, value) in values.iter().enumerate() {
            let fired = set.evaluate(store.as_ref(), value);
            let logged = fired || explicit_log[i];
            assert_eq!(
                logged, expected[i],
                "case {id}: step {i} (value {value}) fired={fired} explicit={}",
                explicit_log[i]
            );
            store = Some(value.clone());
        }
    }
}
