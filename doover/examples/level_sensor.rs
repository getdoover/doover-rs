//! The analog level sensor, rewritten in Rust — a faithful port of the Python
//! `analog-level-sensor`: read an analog-input pin (mA) from the platform
//! interface, convert to level (m) / percentage / volume using the deployment
//! config, and publish them as tags. A reading below `sensor_minimum_ma`
//! (default 4 mA) is treated as a disconnected/faulted sensor and skipped.
//!
//! Config comes from the `deployment_config` channel (keyed by `APP_KEY`) in
//! production, exactly like the Python app — no config file needed.
//!
//! Run:
//!   DDA_URI=127.0.0.1:50051 PLT_URI=127.0.0.1:50053 APP_KEY=analog_level_sensor_1 \
//!     cargo run --release --example level_sensor

use std::time::Duration;

use doover::error::Result;
use doover::{run_app, AppContext, Application, PlatformClient};
use serde_json::json;

/// Linear map, clamped to the output range (pydoover `_map_value`).
fn map_value(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    if (in_max - in_min).abs() < f64::EPSILON {
        return out_min;
    }
    let y = out_min + (x - in_min) * (out_max - out_min) / (in_max - in_min);
    let (lo, hi) = if out_min <= out_max { (out_min, out_max) } else { (out_max, out_min) };
    y.clamp(lo, hi)
}

#[derive(Default)]
struct Cfg {
    ai_pin: i32,
    sensor_min_ma: f64,
    sensor_max_ma: f64,
    sensor_min_m: f64,
    sensor_max_m: f64,
    empty_level: f64,
    full_level: f64,
    max_volume: f64,
    hide_volume: bool,
    volume_precision: i64,
    polling_hz: f64,
}

struct AnalogLevelSensor {
    plt_uri: String,
    cfg: Cfg,
    plt: Option<PlatformClient>,
    /// Test mode: synthesise a noisy in-range mA value instead of reading the
    /// (slow, ~200ms serial round-trip) platform interface, to expose the loop
    /// / live-mode ceiling. Set via the SIMULATE_AI env var.
    simulate: bool,
    rng: u64,
    t: u64,
}

impl AnalogLevelSensor {
    /// Cheap xorshift64 in [0,1) — enough entropy for visible sensor noise.
    fn next_rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A plausible 4–20 mA signal: mid-range base + slow drift + per-loop noise.
    fn simulated_ma(&mut self) -> f64 {
        self.t = self.t.wrapping_add(1);
        let drift = 6.0 * (self.t as f64 * 0.01).sin();
        let noise = (self.next_rand() - 0.5) * 1.0;
        (12.0 + drift + noise).clamp(4.0, 20.0)
    }
}

#[doover::async_trait]
impl Application for AnalogLevelSensor {
    fn loop_target_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.cfg.polling_hz.max(0.01))
    }

    // Tags that stream at the loop rate when a user opens them in live mode.
    // (The Python app marks level_reading + level_filled_percentage live;
    // raw_level_reading is added so live mode is observable even when the
    // sensor is disconnected and only the raw reading is published.)
    fn live_tags(&self) -> Vec<String> {
        vec![
            "raw_level_reading".into(),
            "level_reading".into(),
            "level_filled_percentage".into(),
        ]
    }

    async fn setup(&mut self, ctx: &AppContext) -> Result<()> {
        let c = ctx.config();
        // Read the deployment config (same keys as the Python app), each with
        // the Python app's default.
        self.cfg = Cfg {
            ai_pin: c.get_i64("ai_pin").unwrap_or(1) as i32,
            sensor_min_ma: c.get_f64("sensor_minimum_ma").unwrap_or(4.0),
            sensor_max_ma: c.get_f64("sensor_maximum_ma").unwrap_or(20.0),
            sensor_min_m: c.get_f64("sensor_minimum_metres").unwrap_or(0.0),
            sensor_max_m: c.get_f64("sensor_maximum_metres").unwrap_or(20.0),
            empty_level: c.get_f64("empty_level").unwrap_or(0.0),
            full_level: c.get_f64("full_level").unwrap_or(5.0),
            max_volume: c.get_f64("max_volume").unwrap_or(100_000.0),
            hide_volume: c.get_bool("hide_volume").unwrap_or(true),
            volume_precision: c.get_i64("volume_decimal_precision").unwrap_or(0),
            polling_hz: c.get_f64("polling_frequency").unwrap_or(1.0),
        };

        if self.simulate {
            tracing::warn!("SIMULATE_AI set: synthesising noisy AI values (platform interface NOT read)");
            return Ok(());
        }
        tracing::info!("connecting to platform interface at {}", self.plt_uri);
        let plt = PlatformClient::connect(format!("http://{}", self.plt_uri)).await?;
        plt.test_comms("hello from doover-rs level_sensor").await?;
        self.plt = Some(plt);
        tracing::info!(
            "level sensor ready (AI pin {}, {}..{} mA -> {}..{} m, full_level {} m, poll {} Hz)",
            self.cfg.ai_pin, self.cfg.sensor_min_ma, self.cfg.sensor_max_ma,
            self.cfg.sensor_min_m, self.cfg.sensor_max_m, self.cfg.full_level, self.cfg.polling_hz
        );
        Ok(())
    }

    async fn main_loop(&mut self, ctx: &AppContext) -> Result<()> {
        let t0 = std::time::Instant::now();
        let ma = if self.simulate {
            self.simulated_ma()
        } else {
            let pin = self.cfg.ai_pin;
            self.plt.as_ref().expect("platform connected in setup").fetch_ai(pin).await? as f64
        };
        let t_ai = t0.elapsed();
        let c = &self.cfg;

        // Always publish the raw reading so the round-trip is observable even
        // when the sensor is disconnected (~0 mA on an unwired pin).
        let t1 = std::time::Instant::now();
        ctx.set_tag("raw_level_reading", json!((ma * 1000.0).round() / 1000.0)).await?;
        tracing::info!(
            "Level sensor reading: {ma:.3} mA (AI pin {}) | fetch_ai {:.1}ms set_tag {:.1}ms",
            c.ai_pin, t_ai.as_secs_f64() * 1000.0, t1.elapsed().as_secs_f64() * 1000.0
        );

        // Faithful to the Python app: below the mA floor = no sensor / fault.
        if ma < c.sensor_min_ma {
            tracing::info!(
                "reading below {} mA floor — treating as disconnected, skipping level tags",
                c.sensor_min_ma
            );
            return Ok(());
        }

        // mA -> depth (m) over the sensor range, then depth -> % over empty..full.
        let depth_m = map_value(ma, c.sensor_min_ma, c.sensor_max_ma, c.sensor_min_m, c.sensor_max_m);
        let pct = map_value(depth_m, c.empty_level, c.full_level, 0.0, 100.0);
        let mut tags = vec![
            ("level_filled_percentage".to_string(), json!((pct * 10.0).round() / 10.0)),
            ("level_reading".to_string(), json!((depth_m * 100.0).round() / 100.0)),
        ];
        if !c.hide_volume {
            let volume = pct / 100.0 * c.max_volume;
            let f = 10f64.powi(c.volume_precision as i32);
            tags.push(("level_volume".to_string(), json!((volume * f).round() / f)));
        }
        ctx.set_tags(tags).await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let plt_uri = std::env::var("PLT_URI").unwrap_or_else(|_| "127.0.0.1:50053".to_string());
    let simulate = std::env::var("SIMULATE_AI").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 | 1)
        .unwrap_or(0x9E3779B97F4A7C15);
    run_app(AnalogLevelSensor {
        plt_uri,
        cfg: Cfg { ai_pin: 1, sensor_min_ma: 4.0, sensor_max_ma: 20.0, polling_hz: 1.0, ..Default::default() },
        plt: None,
        simulate,
        rng: seed,
        t: 0,
    })
    .await
}
