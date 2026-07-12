//! Golden-file test for the declarative tags + UI system: a Rust replica of
//! `analog-level-sensor/src/analog_level_sensor/app_tags.py` + `app_ui.py`
//! must reproduce the `ui_schema` subtree of that app's pydoover-generated
//! `doover_config.json` byte-for-byte through the read-merge-write export
//! path.
//!
//! Only the class-body *declaration* is replicated — pydoover's
//! `UI.export()` does not run `setup()`, so neither does this test.
//!
//! The fixture is a byte-exact copy of `analog-level-sensor/doover_config.json`;
//! regenerating it from pydoover is a deliberate, reviewed act.
#![cfg(feature = "macros")]

use doover::tags::{Tag, TagsCollection};
use doover::ui::{
    Colour, NumericVariable, Range, UiApplicationInfo, UiBuild, UiElement, UiTree, Widget,
};
use doover::config::write_ui_schema;
use doover::{Tags, Ui};
use serde_json::Value;

const GOLDEN: &str = include_str!("fixtures/analog_level_sensor_doover_config.json");

/// pydoover `AnalogLevelSensorTags(Tags)` — `app_tags.py`.
#[derive(Tags)]
struct AnalogLevelSensorTags {
    #[tag(live, default = None)]
    level_filled_percentage: Tag<f64>,
    #[tag(live, default = None)]
    level_reading: Tag<f64>,
    #[tag(default = None)]
    raw_level_reading: Tag<f64>,
    #[tag(default = None)]
    level_volume: Tag<f64>,
}

/// pydoover `AnalogLevelSensorUI(ui.UI)` — `app_ui.py` (class body only).
#[derive(Ui)]
struct AnalogLevelSensorUi {
    percentage: NumericVariable,
    level_reading: NumericVariable,
    volume: NumericVariable,
}

/// Boundary between the "Low" and "Good" colour bands, as a fraction of the
/// gauge's full-scale value (`app_ui.py` `_LOW_BAND`).
const LOW_BAND: f64 = 0.15;

impl UiBuild for AnalogLevelSensorUi {
    type Tags = AnalogLevelSensorTags;

    fn build(tags: &AnalogLevelSensorTags) -> Self {
        Self {
            percentage: NumericVariable::new("Level")
                .units("%")
                .value(&tags.level_filled_percentage)
                .precision(1)
                .form(Widget::RADIAL)
                .ranges(vec![
                    Range::new("Low", 0, LOW_BAND * 100.0, Colour::BLUE),
                    Range::new("Good", LOW_BAND * 100.0, 100, Colour::GREEN),
                ]),
            level_reading: NumericVariable::new("Level Reading")
                .units("m")
                .value(&tags.level_reading)
                .precision(2),
            volume: NumericVariable::new("Volume")
                .units("L")
                .value(&tags.level_volume)
                .precision(0)
                .hidden(true),
        }
    }
}

fn golden_ui_schema() -> Value {
    let root: Value = serde_json::from_str(GOLDEN).unwrap();
    root["analog_level_sensor"]["ui_schema"].clone()
}

fn build_schema() -> Value {
    let tags = AnalogLevelSensorTags::detached();
    let mut ui = AnalogLevelSensorUi::build(&tags);
    ui.finalize();
    ui.to_schema(&UiApplicationInfo::default())
}

/// Test 1: the emitted schema equals the golden `ui_schema` subtree, both as
/// Values and as serialized strings (which also checks key order).
#[test]
fn schema_matches_golden_subtree() {
    let ours = build_schema();
    let golden = golden_ui_schema();

    assert_eq!(ours, golden, "ui schema Value mismatch");
    assert_eq!(
        serde_json::to_string(&ours).unwrap(),
        serde_json::to_string(&golden).unwrap(),
        "ui schema key order mismatch"
    );
}

/// Test 2 (the real invariant): running the read-merge-write export over a
/// copy of the pydoover-generated doover_config.json must leave the file
/// bytes completely unchanged.
#[test]
fn export_round_trip_is_byte_identical() {
    let mut path = std::env::temp_dir();
    path.push(format!("doover-rs-ui-golden-{}.json", std::process::id()));
    std::fs::write(&path, GOLDEN.as_bytes()).unwrap();

    write_ui_schema(&path, "analog_level_sensor", build_schema()).unwrap();

    let after = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        after,
        GOLDEN.as_bytes(),
        "export changed doover_config.json:\n{}",
        String::from_utf8_lossy(&after)
    );
}

/// Test 3: the Tags derive — names, live flags and the `$tag.app()`
/// reference strings match the golden `currentValue`s.
#[test]
fn tags_derive_declarations() {
    assert_eq!(
        AnalogLevelSensorTags::tag_names(),
        ["level_filled_percentage", "level_reading", "raw_level_reading", "level_volume"]
    );
    assert_eq!(
        AnalogLevelSensorTags::live_tag_names(),
        ["level_filled_percentage", "level_reading"]
    );

    let tags = AnalogLevelSensorTags::detached();
    assert!(tags.level_filled_percentage.is_live());
    assert!(tags.level_reading.is_live());
    assert!(!tags.raw_level_reading.is_live());
    assert!(!tags.level_volume.is_live());

    // The exact currentValue strings in the golden ui_schema.
    assert_eq!(
        tags.level_filled_percentage.ui_reference(),
        "$tag.app().level_filled_percentage:number:null"
    );
    assert_eq!(tags.level_reading.ui_reference(), "$tag.app().level_reading:number:null");
    assert_eq!(tags.level_volume.ui_reference(), "$tag.app().level_volume:number:null");

    // default=None reads back as no value on a detached handle.
    assert_eq!(tags.level_filled_percentage.get(), None);
}

/// A detached tag write is a clear error, not a silent no-op.
#[tokio::test]
async fn detached_tags_error_on_set() {
    let tags = AnalogLevelSensorTags::detached();
    let err = tags.level_reading.set(1.0).await.unwrap_err();
    assert!(err.to_string().contains("tags not attached"), "{err}");
}

/// The derive's UiTree reflection: field order, names derived from display
/// strings (NOT the Rust field name — `percentage` renders as "level"), and
/// the 51/52/53 position counter.
#[test]
fn ui_tree_reflection_and_positions() {
    let tags = AnalogLevelSensorTags::detached();
    let mut ui = AnalogLevelSensorUi::build(&tags);

    let names: Vec<_> = ui.children().iter().map(|c| c.name().to_string()).collect();
    assert_eq!(names, ["level", "level_reading", "volume"]);

    ui.finalize();
    let positions: Vec<_> = ui.children().iter().map(|c| c.position()).collect();
    assert_eq!(positions, [Some(51), Some(52), Some(53)]);

    // finalize is idempotent and respects explicit positions.
    ui.volume.common.position = Some(10);
    ui.finalize();
    assert_eq!(UiElement::position(&ui.volume), Some(10));
    assert_eq!(UiElement::position(&ui.percentage), Some(51));
}
