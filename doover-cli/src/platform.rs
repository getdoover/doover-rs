//! `doover platform <cmd>` — mirrors the `@cli_command`-decorated surface of
//! pydoover's `PlatformInterface` (`docker/platform/platform.py`).

use clap::Subcommand;
use serde_json::{json, Value};

use doover::docker::platform::{DiConfigUpdate, Edge, PlatformClient, PlatformEvent};
use doover::proto::platform_iface as pb;

use crate::parse::{
    parse_bool_list, parse_float_list, parse_int_list, parse_maybe_float, BoolList, FloatList,
    IntList, MaybeFloat,
};
use crate::{normalize_uri, print_json, CliResult};

fn parse_edge(s: &str) -> Result<Edge, String> {
    s.parse().map_err(|e| format!("{e}"))
}

#[derive(Subcommand, Debug)]
pub enum PlatformCmd {
    /// Liveness echo against the platform interface.
    #[command(name = "test_comms", alias = "test-comms")]
    TestComms {
        /// Message for the sidecar to echo back.
        #[arg(long, default_value = "Comms Check Message")]
        message: String,
    },

    /// Read one or more digital-input pins (true = high).
    #[command(name = "fetch_di", alias = "fetch-di")]
    FetchDi {
        /// Pin number(s) to read.
        #[arg(required = true)]
        di: Vec<i32>,
    },

    /// Read one or more analog-input pins (mA).
    #[command(name = "fetch_ai", alias = "fetch-ai")]
    FetchAi {
        /// Pin number(s) to read.
        #[arg(required = true)]
        ai: Vec<i32>,
    },

    /// Read one or more digital-output pins.
    #[command(name = "fetch_do", alias = "fetch-do")]
    FetchDo {
        /// Pin number(s) to read.
        #[arg(required = true)]
        r#do: Vec<i32>,
    },

    /// Read one or more analog-output pins.
    #[command(name = "fetch_ao", alias = "fetch-ao")]
    FetchAo {
        /// Pin number(s) to read.
        #[arg(required = true)]
        ao: Vec<i32>,
    },

    /// Set digital-output pin(s); a single value broadcasts to every pin.
    #[command(name = "set_do", alias = "set-do")]
    SetDo {
        /// Pin(s) to set: '3' or '[1,2]'.
        #[arg(value_parser = parse_int_list)]
        r#do: IntList,
        /// Value(s): '1', '0', 'true', 'false' or a list like '[1,0]'.
        #[arg(value_parser = parse_bool_list)]
        value: BoolList,
    },

    /// Schedule digital-output pin(s) to change in `in_secs` seconds.
    #[command(name = "schedule_do", alias = "schedule-do")]
    ScheduleDo {
        /// Pin(s) to set: '3' or '[1,2]'.
        #[arg(value_parser = parse_int_list)]
        r#do: IntList,
        /// Value(s): '1', '0', 'true', 'false' or a list like '[1,0]'.
        #[arg(value_parser = parse_bool_list)]
        value: BoolList,
        /// Seconds from now to apply the change.
        in_secs: u32,
    },

    /// Set analog-output pin(s); a single value broadcasts to every pin.
    #[command(name = "set_ao", alias = "set-ao")]
    SetAo {
        /// Pin(s) to set: '0' or '[0,1]'.
        #[arg(value_parser = parse_int_list)]
        ao: IntList,
        /// Value(s): '4.5' or a list like '[4.5,12.0]'.
        #[arg(value_parser = parse_float_list)]
        value: FloatList,
    },

    /// Schedule analog-output pin(s) to change in `in_secs` seconds.
    #[command(name = "schedule_ao", alias = "schedule-ao")]
    ScheduleAo {
        /// Pin(s) to set: '0' or '[0,1]'.
        #[arg(value_parser = parse_int_list)]
        ao: IntList,
        /// Value(s): '4.5' or a list like '[4.5,12.0]'.
        #[arg(value_parser = parse_float_list)]
        value: FloatList,
        /// Seconds from now to apply the change.
        in_secs: u32,
    },

    /// The system input voltage in volts.
    #[command(name = "fetch_system_voltage", alias = "fetch-system-voltage")]
    FetchSystemVoltage,

    /// The system input power in watts.
    #[command(name = "fetch_system_power", alias = "fetch-system-power")]
    FetchSystemPower,

    /// The system temperature in degrees Celsius.
    #[command(name = "fetch_system_temperature", alias = "fetch-system-temperature")]
    FetchSystemTemperature,

    /// The device location (GPS/modem fix).
    #[command(name = "fetch_location", alias = "fetch-location")]
    FetchLocation,

    /// Reboot the device.
    Reboot,

    /// Shut the device down.
    Shutdown,

    /// Seconds for which the device ignores shutdown requests.
    #[command(name = "fetch_immunity_seconds", alias = "fetch-immunity-seconds")]
    FetchImmunitySeconds,

    /// Set the number of seconds the device ignores shutdown requests for.
    #[command(name = "set_immunity_seconds", alias = "set-immunity-seconds")]
    SetImmunitySeconds {
        /// Immunity window in seconds.
        immunity_secs: i32,
    },

    /// The input-voltage threshold at which the device wakes from shutdown.
    #[command(name = "fetch_wake_on_voltage", alias = "fetch-wake-on-voltage")]
    FetchWakeOnVoltage,

    /// Set the wake-on-voltage threshold ('none' disables it).
    #[command(name = "set_wake_on_voltage", alias = "set-wake-on-voltage")]
    SetWakeOnVoltage {
        /// Threshold in volts, or 'none' to disable.
        #[arg(value_parser = parse_maybe_float)]
        voltage: MaybeFloat,
    },

    /// Why the device was most recently woken from shutdown.
    #[command(name = "fetch_wake_reason", alias = "fetch-wake-reason")]
    FetchWakeReason,

    /// System-status snapshots captured while the device was asleep.
    #[command(name = "fetch_sleep_log", alias = "fetch-sleep-log")]
    FetchSleepLog {
        /// Only snapshots after this epoch-milliseconds timestamp (0 = all).
        #[arg(long, default_value_t = 0)]
        since: i64,
    },

    /// The interval between sleep-log snapshots in seconds (0 = disabled).
    #[command(name = "fetch_sleep_log_interval", alias = "fetch-sleep-log-interval")]
    FetchSleepLogInterval,

    /// Set the interval between sleep-log snapshots (0 disables logging).
    #[command(name = "set_sleep_log_interval", alias = "set-sleep-log-interval")]
    SetSleepLogInterval {
        /// Interval in seconds.
        interval_secs: i32,
    },

    /// Schedule the device to shut down in `time_secs` seconds.
    #[command(name = "schedule_shutdown", alias = "schedule-shutdown")]
    ScheduleShutdown {
        /// Seconds from now.
        time_secs: u32,
    },

    /// Schedule the device to start up in `time_secs` seconds.
    #[command(name = "schedule_startup", alias = "schedule-startup")]
    ScheduleStartup {
        /// Seconds from now.
        time_secs: u32,
    },

    /// The IO table advertised by the platform.
    #[command(name = "fetch_io_table", alias = "fetch-io-table")]
    FetchIoTable,

    /// Synchronize the real-time clock with the system time.
    #[command(name = "sync_rtc", alias = "sync-rtc")]
    SyncRtc,

    /// Digital-input events recorded for a pin (e.g. while asleep).
    #[command(name = "fetch_di_events", alias = "fetch-di-events")]
    FetchDiEvents {
        /// The digital-input pin number.
        di_pin: i32,
        /// Which edge(s) to include: rising, falling or both.
        #[arg(value_parser = parse_edge)]
        edge: Edge,
        /// Include system events (e.g. CM4/IO board power-cycles).
        #[arg(long = "include_system_events", alias = "include-system-events")]
        include_system_events: bool,
        /// Starting event id or epoch-milliseconds timestamp (0 = all).
        #[arg(long = "events_from", alias = "events-from", default_value_t = 0)]
        events_from: i64,
    },

    /// The stored configuration for a digital-input pin.
    #[command(name = "fetch_di_config", alias = "fetch-di-config")]
    FetchDiConfig {
        /// The digital-input pin number.
        pin: i32,
    },

    /// Patch the configuration for a digital-input pin; unset fields keep
    /// their stored values.
    #[command(name = "set_di_config", alias = "set-di-config")]
    SetDiConfig {
        /// The digital-input pin number.
        pin: i32,
        /// Whether the input is in PNP (sourcing) mode: true or false.
        #[arg(long = "pnp_mode", alias = "pnp-mode")]
        pnp_mode: Option<bool>,
        /// The edge(s) that trigger an interrupt: rising, falling or both.
        #[arg(long = "irq_edge", alias = "irq-edge", value_parser = parse_edge)]
        irq_edge: Option<Edge>,
        /// Debounce time in milliseconds.
        #[arg(long = "debounce_ms", alias = "debounce-ms")]
        debounce_ms: Option<i32>,
        /// Whether an event on this pin wakes the device: true or false.
        #[arg(long = "wake_on_event", alias = "wake-on-event")]
        wake_on_event: Option<bool>,
    },
}

/// Print a scalar when one pin was requested, a list otherwise — matching
/// pydoover's `fetch_di(*di)` return shape.
fn print_pin_values<T: Into<Value>>(pins: &[i32], values: Vec<T>) {
    let mut values: Vec<Value> = values.into_iter().map(Into::into).collect();
    if pins.len() == 1 && values.len() == 1 {
        print_json(&values.remove(0));
    } else {
        print_json(&Value::Array(values));
    }
}

fn location_json(l: &doover::docker::platform::Location) -> Value {
    json!({
        "latitude": l.latitude,
        "longitude": l.longitude,
        "altitude_m": l.altitude_m,
        "accuracy_m": l.accuracy_m,
        "speed_mps": l.speed_mps,
        "heading_deg": l.heading_deg,
        "sat_count": l.sat_count,
        "timestamp": l.timestamp,
    })
}

fn sleep_log_json(e: &pb::SleepLogEntry) -> Value {
    json!({
        "timestamp": e.timestamp,
        "input_voltage": e.input_voltage,
        "system_current": e.system_current,
        "system_power": e.system_power,
        "di": e.di,
        "do": e.r#do,
        "ai": e.ai,
        "ao": e.ao,
    })
}

fn event_json(e: &PlatformEvent) -> Value {
    json!({
        "event_id": e.event_id,
        "event": e.event,
        "pin": e.pin,
        "value": e.value,
        "time": e.time,
        "cm4_online": e.cm4_online,
    })
}

fn di_config_json(c: &pb::DiConfig) -> Value {
    json!({
        "pin": c.pin,
        "pnp_mode": c.pnp_mode,
        "irq_edge": c.irq_edge,
        "debounce_ms": c.debounce_ms,
        "wake_on_event": c.wake_on_event,
    })
}

pub async fn run(uri: &str, cmd: PlatformCmd) -> CliResult {
    let client = PlatformClient::connect(normalize_uri(uri)).await?;

    match cmd {
        PlatformCmd::TestComms { message } => {
            let resp = client.test_comms(message).await?;
            print_json(&json!(resp));
        }
        PlatformCmd::FetchDi { di } => {
            let values = client.fetch_dis(&di).await?;
            print_pin_values(&di, values);
        }
        PlatformCmd::FetchAi { ai } => {
            let values = client.fetch_ais(&ai).await?;
            print_pin_values(&ai, values);
        }
        PlatformCmd::FetchDo { r#do } => {
            let values = client.fetch_dos(&r#do).await?;
            print_pin_values(&r#do, values);
        }
        PlatformCmd::FetchAo { ao } => {
            let values = client.fetch_aos(&ao).await?;
            print_pin_values(&ao, values);
        }
        PlatformCmd::SetDo { r#do, value } => {
            let result = client.set_dos(&r#do.0, &value.0).await?;
            print_json(&json!(result));
        }
        PlatformCmd::ScheduleDo { r#do, value, in_secs } => {
            let result = client.schedule_dos(&r#do.0, &value.0, in_secs).await?;
            print_json(&json!(result));
        }
        PlatformCmd::SetAo { ao, value } => {
            let result = client.set_aos(&ao.0, &value.0).await?;
            print_json(&json!(result));
        }
        PlatformCmd::ScheduleAo { ao, value, in_secs } => {
            let result = client.schedule_aos(&ao.0, &value.0, in_secs).await?;
            print_json(&json!(result));
        }
        PlatformCmd::FetchSystemVoltage => {
            print_json(&json!(client.fetch_system_voltage().await?));
        }
        PlatformCmd::FetchSystemPower => {
            print_json(&json!(client.fetch_system_power().await?));
        }
        PlatformCmd::FetchSystemTemperature => {
            print_json(&json!(client.fetch_system_temperature().await?));
        }
        PlatformCmd::FetchLocation => {
            let location = client.fetch_location().await?;
            print_json(&location_json(&location));
        }
        PlatformCmd::Reboot => client.reboot().await?,
        PlatformCmd::Shutdown => client.shutdown().await?,
        PlatformCmd::FetchImmunitySeconds => {
            print_json(&json!(client.fetch_immunity_seconds().await?));
        }
        PlatformCmd::SetImmunitySeconds { immunity_secs } => {
            print_json(&json!(client.set_immunity_seconds(immunity_secs).await?));
        }
        PlatformCmd::FetchWakeOnVoltage => {
            print_json(&json!(client.fetch_wake_on_voltage().await?));
        }
        PlatformCmd::SetWakeOnVoltage { voltage } => {
            print_json(&json!(client.set_wake_on_voltage(voltage.0).await?));
        }
        PlatformCmd::FetchWakeReason => {
            print_json(&json!(client.fetch_wake_reason().await?));
        }
        PlatformCmd::FetchSleepLog { since } => {
            let entries = client.fetch_sleep_log(since).await?;
            print_json(&Value::Array(entries.iter().map(sleep_log_json).collect()));
        }
        PlatformCmd::FetchSleepLogInterval => {
            print_json(&json!(client.fetch_sleep_log_interval().await?));
        }
        PlatformCmd::SetSleepLogInterval { interval_secs } => {
            print_json(&json!(client.set_sleep_log_interval(interval_secs).await?));
        }
        PlatformCmd::ScheduleShutdown { time_secs } => {
            print_json(&json!(client.schedule_shutdown(time_secs).await?));
        }
        PlatformCmd::ScheduleStartup { time_secs } => {
            print_json(&json!(client.schedule_startup(time_secs).await?));
        }
        PlatformCmd::FetchIoTable => {
            print_json(&json!(client.fetch_io_table().await?));
        }
        PlatformCmd::SyncRtc => client.sync_rtc().await?,
        PlatformCmd::FetchDiEvents { di_pin, edge, include_system_events, events_from } => {
            let (events_synced, events) = client
                .fetch_di_events(di_pin, edge, include_system_events, events_from)
                .await?;
            print_json(&json!({
                "events_synced": events_synced,
                "events": events.iter().map(event_json).collect::<Vec<_>>(),
            }));
        }
        PlatformCmd::FetchDiConfig { pin } => {
            let config = client.fetch_di_config(pin).await?;
            print_json(&config.as_ref().map(di_config_json).unwrap_or(Value::Null));
        }
        PlatformCmd::SetDiConfig { pin, pnp_mode, irq_edge, debounce_ms, wake_on_event } => {
            let update = DiConfigUpdate { pnp_mode, irq_edge, debounce_ms, wake_on_event };
            let config = client.set_di_config(pin, &update).await?;
            print_json(&config.as_ref().map(di_config_json).unwrap_or(Value::Null));
        }
    }
    Ok(())
}
