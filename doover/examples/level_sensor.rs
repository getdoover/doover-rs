//! The analog level sensor, rewritten on the full declarative framework — a
//! faithful port of the Python `analog-level-sensor`
//! (`app_config.py` / `app_tags.py` / `app_ui.py` / `application.py`):
//! read an analog-input pin (mA) from the platform interface, convert to
//! level (m) / filled percentage / volume via the typed deployment config
//! (including the volume curve), and publish them as declared tags that the
//! declared UI references. The UI mirrors `AnalogLevelSensorUI.setup`'s
//! volume-promotion logic at runtime.
//!
//! Export its `doover_config.json` schemas without connecting to anything:
//!
//!   cargo run --example level_sensor -- export /tmp/doover_config.json --app-name analog_level_sensor
//!
//! Run against a live agent:
//!
//!   DDA_URI=127.0.0.1:50051 PLT_URI=127.0.0.1:50053 APP_KEY=analog_level_sensor_1 \
//!     cargo run --release --example level_sensor
//!
//! Set SIMULATE_AI=1 to synthesise a noisy 4–20 mA signal instead of reading
//! the platform interface.

use std::time::Duration;

use doover::config::ApplicationPosition;
use doover::error::Result;
use doover::tags::Tag;
use doover::ui::{Colour, NumericVariable, Range, UiBuild, Widget};
use doover::{AppContext, Application, Config, ConfigEnum, ConfigObject, PlatformClient, Tags, Ui};

/// Boundary between the "Low" and "Good" colour bands, as a fraction of the
/// gauge's full-scale value (`app_ui.py` `_LOW_BAND`).
const LOW_BAND: f64 = 0.15;

// ---------------------------------------------------------------------------
// app_config.py — AnalogLevelSensorConfig
// ---------------------------------------------------------------------------

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
    // Only read at deploy time (drives the app's slot in the UI); a doc
    // comment here would override ApplicationPosition's canonical description.
    #[allow(dead_code)]
    position: ApplicationPosition,
}

// ---------------------------------------------------------------------------
// app_tags.py — AnalogLevelSensorTags
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// app_ui.py — AnalogLevelSensorUI (class body)
// ---------------------------------------------------------------------------

#[derive(Ui)]
struct AnalogLevelSensorUi {
    percentage: NumericVariable,
    level_reading: NumericVariable,
    volume: NumericVariable,
}

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

// ---------------------------------------------------------------------------
// application.py — AnalogLevelSensorApplication
// ---------------------------------------------------------------------------

struct AnalogLevelSensor {
    config: AnalogLevelSensorConfig,
    tags: AnalogLevelSensorTags,
    ui: AnalogLevelSensorUi,
    plt: Option<PlatformClient>,
    /// SIMULATE_AI: synthesise a noisy in-range mA value instead of reading
    /// the platform interface.
    simulate: bool,
    rng: u64,
    t: u64,
}

impl AnalogLevelSensor {
    /// Sorted `(level, volume)` float pairs (Python `_get_volume`'s `points`).
    fn curve_points(&self) -> Vec<(f64, f64)> {
        let mut points: Vec<(f64, f64)> =
            self.config.volume_curve.iter().map(|p| (p.level, p.volume)).collect();
        points.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        points
    }

    /// Python `_map_value`: linear map, inverted for Radar sensors when
    /// `invert` is requested.
    fn map_value(&self, value: f64, low_a: f64, high_a: f64, low_b: f64, high_b: f64, invert: bool) -> f64 {
        if invert && self.config.sensor_type == SensorType::Radar {
            return (high_b - low_b) - ((value - low_a) / (high_a - low_a)) * (high_b - low_b);
        }
        ((value - low_a) / (high_a - low_a)) * (high_b - low_b) + low_b
    }

    fn sensor_percentage(&self, reading: f64) -> f64 {
        self.map_value(
            reading,
            self.config.sensor_minimum_ma,
            self.config.sensor_maximum_ma,
            0.0,
            100.0,
            true,
        )
    }

    fn level_reading(&self, reading: f64) -> f64 {
        let perc = self.sensor_percentage(reading);
        self.map_value(
            perc,
            0.0,
            100.0,
            self.config.sensor_minimum_metres,
            self.config.sensor_maximum_metres,
            false,
        )
    }

    fn filled_percentage(&self, reading: f64) -> Option<f64> {
        let lev = self.level_reading(reading);
        let points = self.curve_points();
        if points.len() < 2 {
            return Some(self.map_value(
                lev,
                self.config.empty_level,
                self.config.full_level,
                0.0,
                100.0,
                false,
            ));
        }
        let vol = interpolate_volume(lev, &points)?;
        let max_vol = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        Some((vol / max_vol * 100.0 * 1000.0).round() / 1000.0)
    }

    fn volume(&self, reading: f64) -> Option<f64> {
        let points = self.curve_points();
        if points.len() >= 2 {
            return interpolate_volume(self.level_reading(reading), &points);
        }
        let perc = self.filled_percentage(reading)?;
        Some(self.config.max_volume * (perc / 100.0))
    }

    /// Full-scale value for the volume gauge (`app_ui.py _gauge_max_volume`):
    /// the top of the volume curve when configured, else the max volume.
    fn gauge_max_volume(&self) -> f64 {
        let points = self.curve_points();
        if points.len() >= 2 {
            points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max)
        } else {
            self.config.max_volume
        }
    }

    async fn set_power_pin(&self, high: bool) -> Result<()> {
        if let (Some(pin), Some(plt)) = (self.config.power_pin, &self.plt) {
            plt.set_do(pin as i32, high).await?;
        }
        Ok(())
    }

    /// Cheap xorshift64 in [0,1) — enough entropy for visible sensor noise.
    fn next_rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A plausible 4–20 mA signal: mid-range base + slow drift + noise.
    fn simulated_ma(&mut self) -> f64 {
        self.t = self.t.wrapping_add(1);
        let drift = 6.0 * (self.t as f64 * 0.01).sin();
        let noise = (self.next_rand() - 0.5) * 1.0;
        (12.0 + drift + noise).clamp(4.0, 20.0)
    }
}

/// Python `_get_volume`: interpolate within the curve, extrapolating off the
/// nearest end segment outside it (so misconfiguration shows up rather than
/// being hidden).
fn interpolate_volume(level: f64, points: &[(f64, f64)]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    for pair in points.windows(2) {
        let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
        if x1 <= level && level <= x2 {
            return Some(y1 + (level - x1) * (y2 - y1) / (x2 - x1));
        }
    }
    let ((x1, y1), (x2, y2)) = if level < points[0].0 {
        (points[0], points[1])
    } else {
        (points[points.len() - 2], points[points.len() - 1])
    };
    Some(y1 + (level - x1) * (y2 - y1) / (x2 - x1))
}

#[doover::async_trait]
impl Application for AnalogLevelSensor {
    type Config = AnalogLevelSensorConfig;
    type Tags = AnalogLevelSensorTags;
    type Ui = AnalogLevelSensorUi;

    fn create(
        config: AnalogLevelSensorConfig,
        tags: AnalogLevelSensorTags,
        ui: AnalogLevelSensorUi,
    ) -> Self {
        let simulate =
            std::env::var("SIMULATE_AI").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64 | 1)
            .unwrap_or(0x9E3779B97F4A7C15);
        Self { config, tags, ui, plt: None, simulate, rng: seed, t: 0 }
    }

    fn ui(&self) -> Option<&AnalogLevelSensorUi> {
        Some(&self.ui)
    }

    fn ui_mut(&mut self) -> Option<&mut AnalogLevelSensorUi> {
        Some(&mut self.ui)
    }

    fn loop_target_period(&self) -> Duration {
        // Python setup: `self.loop_target_period = 1 / freq` when freq > 0.
        let freq = self.config.polling_frequency;
        if freq > 0.0 {
            Duration::from_secs_f64(1.0 / freq)
        } else {
            Duration::from_secs(1)
        }
    }

    async fn setup(&mut self, _ctx: &AppContext) -> Result<()> {
        if !self.simulate {
            let plt_uri =
                std::env::var("PLT_URI").unwrap_or_else(|_| "127.0.0.1:50053".to_string());
            tracing::info!("connecting to platform interface at {plt_uri}");
            self.plt = Some(PlatformClient::connect(format!("http://{plt_uri}")).await?);
        } else {
            tracing::warn!("SIMULATE_AI set: synthesising AI values (platform interface NOT read)");
        }

        // Python `AnalogLevelSensorApplication.setup`.
        self.set_power_pin(true).await?;

        // Python `AnalogLevelSensorUI.setup` — runtime UI mutations.
        self.ui.volume.common.units = Some(self.config.volume_units.clone());
        if !self.config.hide_volume {
            // Volume display enabled -> promote volume to the primary radial
            // gauge, demote the percentage level to a plain reading below it.
            let max_vol = self.gauge_max_volume();
            self.ui.volume.common.hidden = Some(serde_json::Value::Bool(false));
            self.ui.volume.precision = Some(self.config.volume_decimal_precision);
            self.ui.volume.common.form = Some(Widget::RADIAL.to_string());
            self.ui.volume.ranges = Some(vec![
                Range::new("Low", 0, LOW_BAND * max_vol, Colour::BLUE),
                Range::new("Good", LOW_BAND * max_vol, max_vol, Colour::GREEN),
            ]);
            self.ui.volume.common.position = Some(10);

            self.ui.percentage.common.form = None;
            self.ui.percentage.ranges = None;
            self.ui.percentage.common.position = Some(20);

            self.ui.level_reading.common.position = Some(30);
        }
        Ok(())
    }

    async fn main_loop(&mut self, _ctx: &AppContext) -> Result<()> {
        let reading = if self.simulate {
            self.simulated_ma()
        } else {
            let pin = self.config.ai_pin as i32;
            self.plt.as_ref().expect("platform connected in setup").fetch_ai(pin).await? as f64
        };
        tracing::info!("Level sensor reading: {reading}");

        // Python: below the mA floor = no sensor / fault; skip the tags.
        if reading < self.config.sensor_minimum_ma {
            return Ok(());
        }

        self.set_power_pin(true).await?;

        if let Some(pct) = self.filled_percentage(reading) {
            self.tags.level_filled_percentage.set(pct).await?;
        }
        self.tags.level_reading.set(self.level_reading(reading)).await?;
        self.tags.raw_level_reading.set(reading).await?;
        if !self.config.hide_volume {
            if let Some(volume) = self.volume(reading) {
                self.tags.level_volume.set(volume).await?;
            }
        }
        Ok(())
    }

    async fn on_shutdown(&mut self, _ctx: &AppContext) -> Result<()> {
        // Python `on_shutdown_at`: de-power the sensor.
        self.set_power_pin(false).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    doover::run::<AnalogLevelSensor>().await
}
