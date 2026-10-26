//! A `ConfigObject` (and a list of them) can be a field's default: the derive
//! emits `ToConfigValue`, the inverse of its loader.

#![cfg(feature = "macros")]

use doover::config::{ConfigSchema, FromConfigValue, TagRef, ToConfigValue};
use doover::{Config, ConfigEnum, ConfigObject};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum Function {
    #[config(rename = "coils")]
    Coils,
    #[config(rename = "input_registers")]
    InputRegisters,
}

#[derive(Debug, Clone, PartialEq, ConfigObject)]
struct Block {
    #[config(title = "Function")]
    function: Option<Function>,
    #[config(title = "Start")]
    start: i64,
    #[config(title = "Count", default = 1)]
    count: i64,
}

#[derive(Debug, Config)]
struct AppConfig {
    #[config(title = "Blocks", item_title = "Block", default = Vec::new())]
    blocks: Vec<Block>,

    #[config(
        title = "Defaults",
        item_title = "Block",
        default = vec![Block { function: Some(Function::InputRegisters), start: 512, count: 4 }]
    )]
    defaults: Vec<Block>,
}

#[test]
fn an_empty_list_of_objects_is_an_optional_field() {
    let schema = AppConfig::schema().to_json();
    assert_eq!(schema["properties"]["blocks"]["default"], json!([]));
    assert!(!schema["required"].as_array().unwrap().contains(&json!("blocks")));

    let cfg = AppConfig::from_value(&json!({})).unwrap();
    assert!(cfg.blocks.is_empty());
}

#[test]
fn an_object_default_uses_element_names_and_choice_strings() {
    let schema = AppConfig::schema().to_json();
    assert_eq!(
        schema["properties"]["defaults"]["default"],
        json!([{"function": "input_registers", "start": 512, "count": 4}])
    );
    let cfg = AppConfig::from_value(&json!({})).unwrap();
    assert_eq!(cfg.defaults[0].start, 512);
}

#[test]
fn objects_round_trip() {
    let block = Block { function: None, start: 3, count: 2 };
    let value = block.to_config_value();
    assert_eq!(value, json!({"function": null, "start": 3, "count": 2}));
    assert_eq!(Block::from_config_value(&value).unwrap(), block);
}

#[derive(Debug, PartialEq, ConfigObject)]
struct WithMarkers {
    #[config(title = "Source")]
    source: TagRef,
    #[config(title = "Note")]
    note: Option<String>,
}

#[test]
fn marker_types_round_trip_inside_objects() {
    let original = WithMarkers {
        source: TagRef {
            reference_name: "source".into(),
            agent_id: None,
            app_name: "pump".into(),
            tag_name: "flow".into(),
        },
        note: Some("x".into()),
    };
    let value = original.to_config_value();
    assert_eq!(value["source"]["agent_id"], json!(null));
    assert_eq!(WithMarkers::from_config_value(&value).unwrap(), original);
}
