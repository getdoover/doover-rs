//! Replays the pydoover-generated config-schema fixtures
//! (`tests/compat/fixtures/config_schemas.json`, produced by
//! `scripts/gen_config_schema_fixtures.py`) against the Rust schema emitter.
//!
//! Each case builds the same schema by hand with [`ElementSchema`] and compares
//! the *serialized string*, so key order is part of the contract — the
//! conditional-field work added an `allOf` key whose position after `required`
//! matters as much as its contents.
//!
//! The `loads` half of each fixture records what pydoover's
//! `_inject_deployment_config` did with sample configs; the assertions below
//! mirror those outcomes through `#[derive(Config)]`. Where pydoover leaves an
//! inactive conditional element `NotSet` (its `"<unset>"` marker), Rust has no
//! such state — a struct field is always populated — so the field is declared
//! `Option<T>` and reads `None`. See PARITY.md.

use std::path::PathBuf;

use serde_json::{json, Value};

use doover::config::{
    ApplicationDefaultOpen, ApplicationFullWidth, ApplicationInterpreterHidden,
    ApplicationPosition, Condition, ConfigSchema, ElementSchema, SchemaModel,
};

fn cases() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/compat/fixtures/config_schemas.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"));
    serde_json::from_str(&text).expect("fixture parses")
}

fn case(name: &str) -> Value {
    cases()
        .into_iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no fixture case named {name:?}"))
}

/// Compare serialized strings, not `Value`s: key order is the contract.
fn assert_schema_eq(built: &SchemaModel, name: &str) {
    let expected = case(name)["schema"].clone();
    let got = serde_json::to_string_pretty(&built.to_json()).unwrap();
    let want = serde_json::to_string_pretty(&expected).unwrap();
    assert_eq!(got, want, "schema mismatch for fixture case {name:?}");
}

fn mode_enum(default: &str) -> ElementSchema {
    let mut el = ElementSchema::enumeration(
        "Mode",
        "mode",
        vec![json!("radar"), json!("submersible")],
    );
    el.default = Some(json!(default));
    el
}

#[test]
fn fixture_corpus_is_present() {
    assert!(cases().len() >= 5, "fixture corpus unexpectedly small");
}

/// The controller's default satisfies the condition, so the `if` carries no
/// `required`: JSON Schema's `properties` already matches an absent property,
/// and an omitted controller does default into the branch.
#[test]
fn default_matching_the_condition_needs_no_required_in_the_if() {
    let mut schema = SchemaModel::new();
    schema.push(mode_enum("radar"));
    let mut depth = ElementSchema::number("Depth", "depth");
    depth.show_if = Some(Condition::equal("mode", json!("radar")));
    schema.push(depth);

    assert_schema_eq(&schema, "default_matches_condition");
    let emitted = schema.to_json();
    assert!(
        emitted["allOf"][0]["if"].get("required").is_none(),
        "the branch must not require a controller whose default already matches"
    );
    // The conditional element is out of the object's own properties/required.
    assert!(emitted["properties"].get("depth").is_none());
    assert_eq!(emitted["required"], json!([]));
    assert_eq!(emitted["allOf"][0]["then"]["required"], json!(["depth"]));
}

/// When the controller's default does *not* satisfy the condition, the `if`
/// gains `required: [controller]` — otherwise the branch would fire on a config
/// that omits the controller entirely.
#[test]
fn default_differing_from_the_condition_requires_the_controller() {
    let mut schema = SchemaModel::new();
    schema.push(mode_enum("submersible"));
    let mut depth = ElementSchema::number("Depth", "depth");
    depth.show_if = Some(Condition::equal("mode", json!("radar")));
    schema.push(depth);

    assert_schema_eq(&schema, "default_differs_from_condition");
    assert_eq!(schema.to_json()["allOf"][0]["if"]["required"], json!(["mode"]));
}

/// Elements sharing a controller *and* a condition share one branch, in
/// declaration order; a different condition on the same controller is its own
/// branch. `then.required` is omitted entirely when nothing in the branch is
/// required.
#[test]
fn shared_conditions_collapse_into_one_branch() {
    let mut schema = SchemaModel::new();
    schema.push(mode_enum("radar"));
    let mut depth = ElementSchema::number("Depth", "depth");
    depth.show_if = Some(Condition::equal("mode", json!("radar")));
    schema.push(depth);
    let mut offset = ElementSchema::number("Offset", "offset");
    offset.default = Some(json!(0.0));
    offset.show_if = Some(Condition::equal("mode", json!("radar")));
    schema.push(offset);
    let mut probe = ElementSchema::string("Probe", "probe");
    probe.show_if = Some(Condition::equal("mode", json!("submersible")));
    schema.push(probe);
    let mut always = ElementSchema::boolean("Always", "always");
    always.default = Some(json!(true));
    schema.push(always);

    assert_schema_eq(&schema, "shared_and_distinct_branches");
    let emitted = schema.to_json();
    let branches = emitted["allOf"].as_array().unwrap();
    assert_eq!(branches.len(), 2, "one branch per distinct (controller, condition)");
    let radar_props = branches[0]["then"]["properties"].as_object().unwrap();
    assert_eq!(
        radar_props.keys().collect::<Vec<_>>(),
        vec!["depth", "offset"],
        "branch members keep declaration order"
    );
    // `probe` has no default, so its branch requires it; `offset` does, so the
    // radar branch requires only `depth`.
    assert_eq!(branches[0]["then"]["required"], json!(["depth"]));
    assert_eq!(branches[1]["then"]["required"], json!(["probe"]));
}

/// Conditions work the same one level down: a nested `Object` runs the same
/// builder, so its own `allOf` sits after its `required` and before
/// `x-collapsible`.
#[test]
fn nested_objects_get_their_own_conditional_branches() {
    let mut use_filter = ElementSchema::boolean("Use Filter", "use_filter");
    use_filter.default = Some(json!(false));
    use_filter.position = Some(1);
    let mut alpha = ElementSchema::number("Filter Alpha", "filter_alpha");
    alpha.default = Some(json!(0.5));
    alpha.position = Some(2);
    alpha.show_if = Some(Condition::equal("use_filter", json!(true)));
    let mut window = ElementSchema::integer("Filter Window", "filter_window");
    window.position = Some(3);
    window.show_if = Some(Condition::equal("use_filter", json!(true)));

    let mut tuning =
        ElementSchema::object("Tuning", "tuning", vec![use_filter, alpha, window]);
    tuning.default = Some(json!({}));

    let mut advanced = ElementSchema::boolean("Advanced Mode", "advanced_mode");
    advanced.default = Some(json!(false));
    let mut threshold = ElementSchema::number("Threshold", "threshold");
    threshold.show_if = Some(Condition::equal("advanced_mode", json!(true)));

    // pydoover's declaration order here is advanced_mode, threshold, tuning:
    // the Object is attached to the class after the plain elements.
    let mut schema = SchemaModel::new();
    schema.push(advanced);
    schema.push(threshold);
    schema.push(tuning);

    assert_schema_eq(&schema, "nested_object_conditions");
    let emitted = schema.to_json();
    let nested = &emitted["properties"]["tuning"];
    assert_eq!(
        nested.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec![
            "title",
            "x-name",
            "x-hidden",
            "type",
            "x-required",
            "default",
            "x-position",
            "properties",
            "additionalElements",
            "required",
            "allOf",
            "x-collapsible",
            "x-defaultCollapsed",
        ],
        "allOf sits between required and x-collapsible"
    );
    assert_eq!(nested["allOf"][0]["if"]["required"], json!(["use_filter"]));
    assert_eq!(nested["allOf"][0]["then"]["required"], json!(["filter_window"]));
}

/// The application marker elements, including `interpreter_full_width`
/// (pydoover 6394c84).
#[derive(doover::Config)]
struct AppElements {
    interpreter_hidden: ApplicationInterpreterHidden,
    interpreter_full_width: ApplicationFullWidth,
    dv_app_position: ApplicationPosition,
    dv_app_default_open: ApplicationDefaultOpen,
}

#[test]
fn application_marker_elements_match_pydoover() {
    assert_schema_eq(&AppElements::schema(), "application_elements");

    // And they load their declared defaults from an empty config, as the
    // fixture's `loads` records.
    let loaded = AppElements::from_value(&json!({})).unwrap();
    assert_eq!(loaded.interpreter_hidden, ApplicationInterpreterHidden(false));
    assert_eq!(loaded.interpreter_full_width, ApplicationFullWidth(false));
    assert_eq!(loaded.dv_app_position, ApplicationPosition(100));
    assert_eq!(loaded.dv_app_default_open, ApplicationDefaultOpen(None));
}

// ── The loading half, through #[derive(Config)] ────────────────────────────

/// `depth` is `Option<f64>` because pydoover leaves an inactive conditional
/// element `NotSet`; a Rust struct field has to hold something, and `None` is
/// the honest reading of "the deployment was never asked for this".
#[derive(Debug, doover::Config)]
struct RadarConfig {
    #[config(default = "radar")]
    mode: String,
    // `required` keeps the field demanded while the condition holds; `Option`
    // is what "pydoover left it NotSet" reads as when it doesn't.
    #[config(required, show_if_eq(mode, "radar"))]
    depth: Option<f64>,
}

#[test]
fn an_inactive_conditional_field_is_not_required() {
    // Condition holds (mode's default is "radar") → the field is required, and
    // pydoover raises "Required config element depth not found".
    let err = RadarConfig::from_value(&json!({})).unwrap_err();
    assert!(
        err.to_string().contains("depth"),
        "an active conditional field is still required: {err}"
    );

    // Condition does not hold → the field is skipped, not demanded.
    let cfg = RadarConfig::from_value(&json!({"mode": "submersible"})).unwrap();
    assert_eq!(cfg.mode, "submersible");
    assert_eq!(cfg.depth, None);

    let cfg = RadarConfig::from_value(&json!({"mode": "radar", "depth": 1.5})).unwrap();
    assert_eq!(cfg.depth, Some(1.5));
}

/// Mirrors the `default_differs_from_condition` fixture: the controller's
/// default keeps the field inactive, so an empty config loads, but naming the
/// controller activates it.
#[derive(Debug, doover::Config)]
#[allow(dead_code)] // the controller exists to drive the condition, not be read
struct SubmersibleDefault {
    #[config(default = "submersible")]
    mode: String,
    #[config(required, show_if_eq(mode, "radar"))]
    depth: Option<f64>,
}

#[test]
fn the_controllers_default_decides_whether_the_field_is_demanded() {
    let cfg = SubmersibleDefault::from_value(&json!({})).unwrap();
    assert_eq!(cfg.depth, None, "inactive under the controller's own default");

    let err = SubmersibleDefault::from_value(&json!({"mode": "radar"})).unwrap_err();
    assert!(err.to_string().contains("depth"), "activated by the config: {err}");
}

/// The derive emits the condition against the controller's *config key* and
/// its type's JSON flavour, so a renamed field still resolves.
#[derive(doover::Config)]
#[allow(dead_code)] // the controller exists to drive the condition, not be read
struct RenamedController {
    #[config(name = "sensor_kind", default = 1)]
    kind: i64,
    #[config(show_if_eq(kind, 2))]
    extra: Option<String>,
}

#[test]
fn a_renamed_controller_is_referenced_by_its_config_key() {
    let schema = RenamedController::schema().to_json();
    assert_eq!(
        schema["allOf"][0]["if"]["properties"]["sensor_kind"]["const"],
        json!(2),
        "the condition names the config key, not the Rust field"
    );
    assert_eq!(schema["allOf"][0]["if"]["required"], json!(["sensor_kind"]));

    assert_eq!(RenamedController::from_value(&json!({})).unwrap().extra, None);
    let cfg = RenamedController::from_value(&json!({"sensor_kind": 2, "extra": "x"})).unwrap();
    assert_eq!(cfg.extra.as_deref(), Some("x"));
}
