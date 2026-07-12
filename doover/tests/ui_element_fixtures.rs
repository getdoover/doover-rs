//! Replays the machine-generated pydoover UI-element fixtures
//! (tests/compat/fixtures/ui_elements.json, produced by
//! scripts/gen_ui_element_fixtures.py) against the Rust element builders:
//! each case constructs the equivalent element and asserts `to_json()`
//! equality with pydoover's `to_dict()` — including key order (serialized
//! strings are compared).
//!
//! Positions are assigned the same way the generator does: pydoover's
//! global position counter is reset to 50 per case, so the Rust side runs
//! [`assign_positions_depth_first`] with a fresh counter per element.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use doover::ui::{
    assign_positions_depth_first, BooleanVariable, Button, CameraHistory, CameraLiveView, Colour,
    ConnectionInfo, ConnectionType, Container, DatetimeInput, FloatInput, Multiplot,
    NumericVariable, Range, RangeView, RemoteComponent, Select, SelectOption, Series, Slider,
    Submodule, Switch, TabContainer, TextInput, TextVariable, Threshold, TimeInput, Timestamp,
    UiElement, UiValue, WarningIndicator, Widget,
};

fn fixture_cases() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/compat/fixtures/ui_elements.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"));
    serde_json::from_str(&text).expect("fixture parses")
}

/// A `$tag.app()` reference like the generator's `UITagBinding`s.
fn tag_ref(name: &str, tag_type: &str, default: Option<Value>, live: bool) -> UiValue {
    UiValue::TagRef {
        name: name.into(),
        tag_type: Some(tag_type.into()),
        default,
        live,
    }
}

/// The children shared by the container cases (`submodule_children()` in the
/// generator).
fn submodule_children() -> Vec<Box<dyn UiElement>> {
    vec![
        Box::new(
            NumericVariable::new("Speed")
                .value(tag_ref("speed", "number", Some(Value::Null), true)),
        ),
        Box::new(Button::new("Reset")),
    ]
}

/// Build the Rust equivalent of one generator case.
fn build(case: &str) -> Option<Box<dyn UiElement>> {
    let element: Box<dyn UiElement> = match case {
        // ---- containers ----
        "container_plain" => Box::new(Container::new("Group").children(submodule_children())),
        "container_explicit_position_child" => Box::new(
            Container::new("Group")
                .child(TextVariable::new("Status").position(7))
                .child(Switch::new("Pump"))
                .position(99),
        ),
        "container_nested" => Box::new(
            Container::new("Outer")
                .child(NumericVariable::new("Top"))
                .child(Container::new("Inner").child(Button::new("Deep")))
                .child(BooleanVariable::new("Flag")),
        ),
        "submodule_plain" => {
            Box::new(Submodule::new("Pump Details").children(submodule_children()))
        }
        "submodule_status_and_collapsed" => {
            Box::new(Submodule::new("Pump Details").status("OK").is_collapsed(true))
        }
        "submodule_status_none" => Box::new(Submodule::new("Pump Details").status(Value::Null)),
        "submodule_default_open_beats_is_collapsed" => {
            Box::new(Submodule::new("Pump Details").is_collapsed(true).default_open(true))
        }
        "submodule_default_open_tag" => Box::new(
            Submodule::new("Pump Details")
                .default_open(tag_ref("pump_open", "boolean", Some(json!(false)), false)),
        ),
        "tabs_plain" => Box::new(TabContainer::new("Views").children(submodule_children())),
        "tabs_default_page" => Box::new(
            TabContainer::new("Views").child(TextVariable::new("A")).default_page(1),
        ),
        "remote_component_plain" => {
            Box::new(RemoteComponent::new("Widget", "https://example.com/c.js"))
        }
        "remote_component_extras" => Box::new(
            RemoteComponent::new("Widget", "https://example.com/c.js")
                .child(Button::new("Go"))
                .extra("foo", "bar")
                .extra("answer", 42),
        ),
        // ---- cameras ----
        "camera_live_view" => Box::new(CameraLiveView::new("front", "hls", true)),
        "camera_live_view_named" => {
            Box::new(CameraLiveView::named("Front Camera", "front", "rtsp", false))
        }
        "camera_history" => Box::new(CameraHistory::new("front")),
        "camera_history_named" => Box::new(CameraHistory::named("Back History", "back")),
        // ---- connection info ----
        "connection_info_default" => Box::new(ConnectionInfo::new(ConnectionType::Constant)),
        "connection_info_periodic_full" => Box::new(
            ConnectionInfo::new(ConnectionType::Periodic)
                .connection_period(600)
                .next_connection(300)
                .offline_after(3600)
                .allowed_misses(3),
        ),
        "connection_info_offline_after_only" => {
            Box::new(ConnectionInfo::new(ConnectionType::Constant).offline_after(7200))
        }
        // ---- multiplot ----
        "multiplot_minimal" => Box::new(Multiplot::new("Trends").push_series(Series::new(
            "Level",
            tag_ref("level", "number", Some(Value::Null), false),
        ))),
        "multiplot_full" => Box::new(
            Multiplot::new("Trends")
                .push_series(
                    Series::new("Level", tag_ref("level", "number", Some(Value::Null), true))
                        .data_type("number")
                        .active(true)
                        .colour(Colour::BLUE)
                        .icon("droplet")
                        .shared_axis(true)
                        .units("%")
                        .range(0, "auto")
                        .ranges(vec![Range::new("Low", 0, 15.0, Colour::BLUE)])
                        .thresholds(vec![Threshold::new("High", 80, Colour::RED)]),
                )
                .push_series(
                    Series::new("Pump State", tag_ref("pump", "boolean", None, false))
                        .name("pump_series")
                        .data_type("boolean")
                        .shared_axis("left")
                        .step_labels(["Off", "On"]),
                )
                .push_series(Series::new("No Lookup", UiValue::Missing))
                .earliest_data_time_epoch(1_767_323_045)
                .default_zoom("7d")
                .default_range_view(RangeView::ZONE),
        ),
        // ---- timestamp ----
        "timestamp_unset" => Box::new(Timestamp::new("Last Seen")),
        "timestamp_int_ms" => Box::new(Timestamp::new("Last Seen").value(1_700_000_000_000_i64)),
        // pydoover converts the datetime to epoch milliseconds.
        "timestamp_datetime" => {
            Box::new(Timestamp::new("Last Seen").value(1_767_323_045_000_i64))
        }
        "timestamp_precision_quirk" => Box::new(
            Timestamp::new("Next Run")
                .value(1_700_000_000_000_i64)
                .precision("second")
                .absolute_format("%Y-%m-%d %H:%M"),
        ),
        "timestamp_tag_live" => Box::new(
            Timestamp::new("Last Report")
                .value(tag_ref("last_report", "number", Some(Value::Null), true)),
        ),
        // ---- parameter inputs ----
        "float_input_plain" => Box::new(FloatInput::new("Target Level")),
        "float_input_bounds_flavours" => {
            Box::new(FloatInput::new("Target Level").min_val(0).max_val(1.5))
        }
        "float_input_full_interaction" => Box::new(
            FloatInput::new("Target Level")
                .min_val(-10.5)
                .max_val(100)
                .default(42)
                .requires_confirm(true)
                .show_activity(false)
                .units("%"),
        ),
        "text_input_plain" => Box::new(TextInput::new("Notes")),
        "text_input_area" => Box::new(TextInput::new("Notes").is_text_area(true)),
        "datetime_input_plain" => Box::new(DatetimeInput::new("Start At")),
        "datetime_input_full" => Box::new(
            DatetimeInput::new("Start At")
                .pickers(["date"])
                .direction("past")
                .max_past(Duration::from_secs(7 * 24 * 3600))
                .max_future(Duration::from_secs(3600)),
        ),
        "time_input_plain" => Box::new(TimeInput::new("Run At")),
        // ---- regression coverage for previously ported elements ----
        "numeric_variable_full" => Box::new(
            NumericVariable::new("Level")
                .value(tag_ref("level", "number", Some(Value::Null), true))
                .precision(1)
                .form(Widget::RADIAL)
                .ranges(vec![Range::new("Low", 0, 15.0, Colour::BLUE)])
                .units("%"),
        ),
        "button_with_default" => Box::new(Button::new("Pump").default(true).disabled(false)),
        "slider_defaults" => Box::new(Slider::new("Speed Limit")),
        "select_options" => Box::new(
            Select::new("Mode")
                .option(SelectOption::new("Fast Mode"))
                .option(SelectOption::new("Slow Mode")),
        ),
        "warning_indicator" => Box::new(WarningIndicator::new("Low Level")),
        _ => return None,
    };
    Some(element)
}

#[test]
fn ui_elements_match_pydoover() {
    let cases = fixture_cases();
    assert!(cases.len() >= 39, "fixture corpus unexpectedly small: {}", cases.len());

    for case in &cases {
        let id = case["case"].as_str().unwrap();
        let mut element = build(id)
            .unwrap_or_else(|| panic!("fixture case {id:?} has no Rust construction"));

        // The generator resets pydoover's global position counter to 50
        // before each case; replicate with a fresh depth-first walk.
        let mut counter = 50;
        assign_positions_depth_first(element.as_mut(), &mut counter);

        let ours = serde_json::to_string(&element.to_json()).unwrap();
        let expected = serde_json::to_string(&case["expected"]).unwrap();
        assert_eq!(ours, expected, "case {id}: serialized JSON (incl. key order) differs");
    }
}
