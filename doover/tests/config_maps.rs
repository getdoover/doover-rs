//! Map config fields (`BTreeMap<String, T>`, `HashMap<String, T>`): an object
//! with free-form keys whose values all follow one schema.
#![cfg(feature = "macros")]

use std::collections::{BTreeMap, HashMap};

use doover::config::ConfigSchema;
use doover::{Config, ConfigEnum, ConfigObject};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum PackageState {
    Present,
    Absent,
}

#[derive(Debug, ConfigObject)]
struct Package {
    /// Exact version to install
    version: Option<String>,
    #[config(default = PackageState::Present)]
    state: PackageState,
}

#[derive(Debug, ConfigObject)]
struct Host {
    /// Labels on this host, by name
    #[config(item_title = "Label")]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Config)]
struct MapsConfig {
    /// Packages, by name
    #[config(item_title = "Package")]
    packages: BTreeMap<String, Package>,
    /// Extra environment, by variable name
    #[config(item_title = "Value", default = HashMap::<String, String>::new())]
    env: HashMap<String, String>,
    /// Optional port overrides, by service
    #[config(item_title = "Port")]
    ports: Option<BTreeMap<String, i64>>,
    host: Host,
}

fn schema() -> Value {
    MapsConfig::schema().to_json()
}

#[test]
fn map_field_is_an_object_whose_values_follow_one_schema() {
    let packages = &schema()["properties"]["packages"];
    assert_eq!(packages["type"], "object");
    assert_eq!(packages["properties"], json!({}));
    assert_eq!(packages["required"], json!([]));

    let values = &packages["additionalElements"];
    assert_eq!(values["type"], "object");
    assert_eq!(values["title"], "Package");
    assert_eq!(values["properties"]["state"]["enum"], json!(["Present", "Absent"]));
    assert_eq!(values["properties"]["version"]["type"], json!(["string", "null"]));
}

#[test]
fn map_values_schema_is_also_additional_properties() {
    // What JSON-schema form editors read to offer key/value entry.
    let s = schema();
    for field in ["packages", "env", "ports"] {
        let el = &s["properties"][field];
        assert_eq!(el["additionalProperties"], el["additionalElements"], "{field}");
    }
}

#[test]
fn plain_objects_still_emit_only_additional_elements_true() {
    let host = &schema()["properties"]["host"];
    assert_eq!(host["additionalElements"], json!(true));
    assert!(host.get("additionalProperties").is_none());
}

#[test]
fn scalar_values_and_nested_maps() {
    let s = schema();
    assert_eq!(s["properties"]["env"]["additionalElements"]["type"], "string");
    assert_eq!(s["properties"]["ports"]["additionalElements"]["type"], "integer");
    let labels = &s["properties"]["host"]["properties"]["labels"];
    assert_eq!(labels["additionalElements"]["title"], "Label");
}

#[test]
fn required_unless_defaulted_or_optional() {
    let s = schema();
    let required = s["required"].as_array().unwrap();
    assert!(required.contains(&json!("packages")));
    assert!(!required.contains(&json!("env")));
    assert!(!required.contains(&json!("ports")));
    assert_eq!(s["properties"]["env"]["default"], json!({}));
}

#[test]
fn loads_maps() {
    let config = MapsConfig::from_value(&json!({
        "packages": {
            "curl": {},
            "docker-ce": {"version": "5:19.03.15", "state": "Present"},
            "nano": {"state": "Absent"}
        },
        "env": {"RUST_LOG": "info"},
        "host": {"labels": {"site": "bench"}}
    }))
    .unwrap();

    assert_eq!(config.packages.len(), 3);
    assert_eq!(config.packages["curl"].state, PackageState::Present);
    assert_eq!(config.packages["docker-ce"].version.as_deref(), Some("5:19.03.15"));
    assert_eq!(config.packages["nano"].state, PackageState::Absent);
    assert_eq!(config.env["RUST_LOG"], "info");
    assert_eq!(config.ports, None);
    assert_eq!(config.host.labels["site"], "bench");
}

#[test]
fn defaults_apply_when_absent() {
    let config = MapsConfig::from_value(&json!({"packages": {}, "host": {"labels": {}}})).unwrap();
    assert!(config.packages.is_empty());
    assert!(config.env.is_empty());
}

#[test]
fn a_bad_value_names_its_key() {
    let err = MapsConfig::from_value(&json!({
        "packages": {"curl": {"state": "Sideways"}},
        "host": {"labels": {}}
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("packages") && err.contains("curl"), "unexpected error: {err}");
}

#[test]
fn a_non_object_is_rejected() {
    let err = MapsConfig::from_value(&json!({"packages": ["curl"], "host": {"labels": {}}}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("expected object"), "unexpected error: {err}");
}
