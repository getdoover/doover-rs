//! Golden-file test for the built-in `export` subcommand path
//! (`doover::write_export::<A>`): an `Application` whose Config/Tags/Ui
//! replicate `analog-level-sensor` must write both the `config_schema` and
//! `ui_schema` subtrees of the pydoover-generated `doover_config.json`
//! byte-for-byte through the read-merge-write export.
//!
//! (The per-schema goldens live in `config_golden.rs` / `ui_golden.rs`; this
//! exercises the combined application-level export the `export` subcommand
//! runs.)
#![cfg(feature = "macros")]

use doover::config::ApplicationPosition;
use doover::tags::Tag;
use doover::ui::{Colour, NumericVariable, Range, UiBuild, Widget};
use doover::{AppContext, Application, Config, ConfigEnum, ConfigObject, Tags, Ui};

const GOLDEN: &str = include_str!("fixtures/analog_level_sensor_doover_config.json");

#[derive(Debug, ConfigObject)]
struct VolumeCurvePoint {
    /// Level / depth in metres
    #[allow(dead_code)]
    level: f64,
    /// Volume in configured volume units
    #[allow(dead_code)]
    volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum SensorType {
    Submersible,
    Radar,
    RadarInverted,
}

#[derive(Debug, Config)]
#[allow(dead_code)]
struct AnalogLevelSensorConfig {
    /// Analog input pin number
    #[config(title = "AI Pin")]
    ai_pin: i64,
    /// Maximum sensor depth (m)
    sensor_maximum_metres: f64,
    /// Level reading when full (m)
    full_level: f64,
    /// Minimum sensor output (mA)
    #[config(title = "Sensor Minimum mA", default = 4.0)]
    sensor_minimum_ma: f64,
    /// Maximum sensor output (mA)
    #[config(title = "Sensor Maximum mA", default = 20.0)]
    sensor_maximum_ma: f64,
    /// Minimum sensor depth (m)
    #[config(default = 0.0)]
    sensor_minimum_metres: f64,
    /// Level reading when empty (m)
    #[config(default = 0.0)]
    empty_level: f64,
    /// Digital output pin to power the sensor
    power_pin: Option<i64>,
    /// How often to poll the sensor (Hz)
    #[config(default = 1.0)]
    polling_frequency: f64,
    /// Type of sensor. Radar inverted reads like a submersible sensor.
    #[config(default = SensorType::Submersible)]
    sensor_type: SensorType,
    #[config(item_title = "Volume Curve Point")]
    volume_curve: Vec<VolumeCurvePoint>,
    /// Whether to hide the tank volume in the UI
    #[config(default = true)]
    hide_volume: bool,
    /// Maximum tank volume in the configured volume units, used when no volume curve is configured
    #[config(default = 100000.0)]
    max_volume: f64,
    /// Units to display the volume reading in (e.g. L, kL, gal)
    #[config(default = "L")]
    volume_units: String,
    /// Number of decimal places to show for the volume reading
    #[config(default = 0)]
    volume_decimal_precision: i64,
    position: ApplicationPosition,
}

#[derive(Tags)]
struct AnalogLevelSensorTags {
    #[tag(live, default = None)]
    level_filled_percentage: Tag<f64>,
    #[tag(live, default = None)]
    level_reading: Tag<f64>,
    #[tag(default = None)]
    #[allow(dead_code)]
    raw_level_reading: Tag<f64>,
    #[tag(default = None)]
    level_volume: Tag<f64>,
}

#[derive(Ui)]
struct AnalogLevelSensorUi {
    percentage: NumericVariable,
    level_reading: NumericVariable,
    volume: NumericVariable,
}

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

struct AnalogLevelSensorApp {
    #[allow(dead_code)]
    config: AnalogLevelSensorConfig,
    #[allow(dead_code)]
    tags: AnalogLevelSensorTags,
    ui: AnalogLevelSensorUi,
}

#[doover::async_trait]
impl Application for AnalogLevelSensorApp {
    type Config = AnalogLevelSensorConfig;
    type Tags = AnalogLevelSensorTags;
    type Ui = AnalogLevelSensorUi;

    fn create(
        config: AnalogLevelSensorConfig,
        tags: AnalogLevelSensorTags,
        ui: AnalogLevelSensorUi,
    ) -> Self {
        Self { config, tags, ui }
    }

    fn ui(&self) -> Option<&AnalogLevelSensorUi> {
        Some(&self.ui)
    }

    fn ui_mut(&mut self) -> Option<&mut AnalogLevelSensorUi> {
        Some(&mut self.ui)
    }

    async fn main_loop(&mut self, _ctx: &AppContext) -> doover::Result<()> {
        Ok(())
    }
}

/// Running the application-level export over a copy of the
/// pydoover-generated doover_config.json must leave the file bytes
/// completely unchanged (both schemas byte-identical, other keys preserved).
#[test]
fn write_export_round_trip_is_byte_identical() {
    let mut path = std::env::temp_dir();
    path.push(format!("doover-rs-export-golden-{}.json", std::process::id()));
    std::fs::write(&path, GOLDEN.as_bytes()).unwrap();

    let wrote_ui =
        doover::write_export::<AnalogLevelSensorApp>(&path, "analog_level_sensor").unwrap();
    assert!(wrote_ui, "the app has UI elements, so a ui_schema must be written");

    let after = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        after,
        GOLDEN.as_bytes(),
        "export changed doover_config.json:\n{}",
        String::from_utf8_lossy(&after)
    );
}

/// An app with no UI elements must not write a ui_schema (pydoover skips the
/// runtime schema for UI-less apps too).
#[test]
fn write_export_skips_ui_schema_without_elements() {
    struct NoUiApp;

    #[doover::async_trait]
    impl Application for NoUiApp {
        type Config = ();
        type Tags = ();
        type Ui = ();

        fn create(_: (), _: (), _: ()) -> Self {
            Self
        }

        async fn main_loop(&mut self, _ctx: &AppContext) -> doover::Result<()> {
            Ok(())
        }
    }

    let mut path = std::env::temp_dir();
    path.push(format!("doover-rs-export-noui-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let wrote_ui = doover::write_export::<NoUiApp>(&path, "bare_app").unwrap();
    assert!(!wrote_ui);

    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v["bare_app"]["config_schema"].is_object());
    assert!(v["bare_app"].get("ui_schema").is_none());
}
