//! Golden-file test for the declarative config system: a Rust replica of
//! `analog-level-sensor/src/analog_level_sensor/app_config.py` must
//! reproduce the `config_schema` subtree of that app's pydoover-generated
//! `doover_config.json` byte-for-byte through the read-merge-write export
//! path.
//!
//! The fixtures under `tests/fixtures/` are byte-exact copies of
//! `analog-level-sensor/doover_config.json` and
//! `analog-level-sensor/simulators/app_config.json`; regenerating them from
//! pydoover is a deliberate, reviewed act.
#![cfg(feature = "macros")]

use doover::config::{write_config_schema, ApplicationPosition, ConfigSchema};
use doover::{Config, ConfigEnum, ConfigObject};
use serde_json::Value;

const GOLDEN: &str = include_str!("fixtures/analog_level_sensor_doover_config.json");
const APP_CONFIG: &str = include_str!("fixtures/analog_level_sensor_app_config.json");

/// pydoover `VolumeCurvePoint(config.Object)`.
#[derive(Debug, ConfigObject)]
struct VolumeCurvePoint {
    /// Level / depth in metres
    level: f64,
    /// Volume in configured volume units
    volume: f64,
}

/// pydoover `config.Enum(choices=["Submersible", "Radar", "Radar Inverted"])`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ConfigEnum)]
enum SensorType {
    Submersible,
    Radar,
    RadarInverted,
}

/// pydoover `AnalogLevelSensorConfig(config.Schema)`.
#[derive(Debug, Config)]
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

fn golden_config_schema() -> Value {
    let root: Value = serde_json::from_str(GOLDEN).unwrap();
    root["analog_level_sensor"]["config_schema"].clone()
}

/// Test 1: the emitted schema equals the golden `config_schema` subtree, both
/// as Values and as serialized strings (which also checks key order).
#[test]
fn schema_matches_golden_subtree() {
    let ours = AnalogLevelSensorConfig::schema().to_json();
    let golden = golden_config_schema();

    assert_eq!(ours, golden, "schema Value mismatch");
    assert_eq!(
        serde_json::to_string(&ours).unwrap(),
        serde_json::to_string(&golden).unwrap(),
        "schema key order mismatch"
    );
}

/// Test 2 (the real invariant): running the read-merge-write export over a
/// copy of the pydoover-generated doover_config.json must leave the file
/// bytes completely unchanged.
#[test]
fn export_round_trip_is_byte_identical() {
    let mut path = std::env::temp_dir();
    path.push(format!("doover-rs-config-golden-{}.json", std::process::id()));
    std::fs::write(&path, GOLDEN.as_bytes()).unwrap();

    write_config_schema(&path, "analog_level_sensor", AnalogLevelSensorConfig::schema().to_json())
        .unwrap();

    let after = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        after,
        GOLDEN.as_bytes(),
        "export changed doover_config.json:\n{}",
        String::from_utf8_lossy(&after)
    );
}

/// Test 3: typed loading of the simulator deployment config, including
/// defaults for absent keys and the enum/array/marker fields.
#[test]
fn from_value_loads_simulator_config() {
    let v: Value = serde_json::from_str(APP_CONFIG).unwrap();
    let config = AnalogLevelSensorConfig::from_value(&v).unwrap();

    assert_eq!(config.ai_pin, 3);
    assert_eq!(config.full_level, 0.615);
    assert_eq!(config.empty_level, 0.0); // integer 0 in the file, f64 field
    assert_eq!(config.sensor_maximum_ma, 20.0); // integer 20 in the file
    assert_eq!(config.sensor_minimum_ma, 4.0);
    assert_eq!(config.sensor_maximum_metres, 4.0816);
    assert_eq!(config.sensor_minimum_metres, 0.0);
    assert_eq!(config.sensor_type, SensorType::Submersible);
    assert_eq!(config.volume_curve.len(), 11);
    assert_eq!(config.volume_curve[1].level, 0.2);
    assert_eq!(config.volume_curve[1].volume, 150.0);

    // Absent keys fall back to schema defaults.
    assert_eq!(config.power_pin, None);
    assert_eq!(config.polling_frequency, 1.0);
    assert!(config.hide_volume);
    assert_eq!(config.max_volume, 100000.0);
    assert_eq!(config.volume_units, "L");
    assert_eq!(config.volume_decimal_precision, 0);
    assert_eq!(config.position, ApplicationPosition(100));
}

/// A required key missing from the deployment config is a clear error naming
/// the key (pydoover raises the same).
#[test]
fn from_value_missing_required_names_the_key() {
    let v: Value = serde_json::from_str(APP_CONFIG).unwrap();
    let mut v = v;
    v.as_object_mut().unwrap().remove("ai_pin");
    let err = AnalogLevelSensorConfig::from_value(&v).unwrap_err();
    assert!(
        err.to_string().contains("required config element 'ai_pin'"),
        "unexpected error: {err}"
    );
}

/// Enum round-trip helpers generated by ConfigEnum.
#[test]
fn config_enum_display_and_from_str() {
    assert_eq!(SensorType::RadarInverted.to_string(), "Radar Inverted");
    assert_eq!("Radar Inverted".parse::<SensorType>().unwrap(), SensorType::RadarInverted);
    assert!("Sonar".parse::<SensorType>().is_err());
}
