//! Enum choice titles, a `null` default on an optional enum, and `show_if_in`.
#![cfg(feature = "macros")]

use doover::config::ConfigSchema;
use doover::{Config, ConfigEnum};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum Kind {
    #[config(rename = "wago_750", title = "WAGO 750")]
    Wago,
    #[config(rename = "point_io", title = "POINT I/O")]
    PointIo,
    #[config(rename = "modbus_tcp")]
    ModbusTcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum WordOrder {
    #[config(rename = "ABCD")]
    Abcd,
    #[config(rename = "CDAB")]
    Cdab,
}

#[derive(Debug, Config)]
struct ChoicesConfig {
    kind: Kind,
    word_order: Option<WordOrder>,
    /// Shown for both rack types
    #[config(item_title = "Module", show_if_in(kind, [Kind::Wago, Kind::PointIo]))]
    modules: Option<Vec<String>>,
}

fn schema() -> Value {
    ChoicesConfig::schema().to_json()
}

#[test]
fn titled_choices_are_one_of_const_title_pairs() {
    let kind = &schema()["properties"]["kind"];
    assert!(kind.get("enum").is_none());
    assert_eq!(
        kind["oneOf"],
        json!([
            {"const": "wago_750", "title": "WAGO 750"},
            {"const": "point_io", "title": "POINT I/O"},
            // an untitled variant is labelled with its choice string
            {"const": "modbus_tcp", "title": "modbus_tcp"},
        ])
    );
    let out = serde_json::to_string(kind).unwrap();
    assert!(out.starts_with(r#"{"oneOf":"#), "{out}");
}

#[test]
fn a_null_default_is_one_of_the_choices() {
    let word_order = &schema()["properties"]["word_order"];
    assert_eq!(word_order["enum"], json!(["ABCD", "CDAB", null]));
    assert_eq!(word_order["default"], Value::Null);
}

#[test]
fn show_if_in_is_one_branch_testing_the_controller_against_a_list() {
    let s = schema();
    assert!(s["properties"].get("modules").is_none());
    assert_eq!(s["allOf"].as_array().unwrap().len(), 1);
    let branch = &s["allOf"][0];
    assert_eq!(
        branch["if"]["properties"]["kind"],
        json!({"enum": ["wago_750", "point_io"]})
    );
    assert_eq!(branch["if"]["required"], json!(["kind"]));
    assert!(branch["then"]["properties"].get("modules").is_some());
}

#[test]
fn loads_by_choice_string_and_skips_an_inactive_field() {
    let config = ChoicesConfig::from_value(&json!({
        "kind": "point_io", "word_order": null, "modules": ["1734-IB4"],
    }))
    .unwrap();
    assert_eq!(config.kind, Kind::PointIo);
    assert_eq!(config.word_order, None);
    assert_eq!(config.modules, Some(vec!["1734-IB4".to_string()]));

    let config = ChoicesConfig::from_value(&json!({"kind": "modbus_tcp"})).unwrap();
    assert_eq!(config.modules, None);
}
