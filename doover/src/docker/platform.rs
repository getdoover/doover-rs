//! `PlatformClient` — a client for the platform-interface sidecar
//! (`platform_iface.platformIface`, default `127.0.0.1:50053`), the Rust
//! equivalent of pydoover's `PlatformInterface` (`docker/platform/platform.py`).
//!
//! Some implementations are platform-specific, and it is your responsibility
//! to ensure that all hardware your application is compatible with implements
//! the methods you are trying to fetch. Most methods return `None` (or an
//! error) if they are not supported or you pass a bad input — e.g. requesting
//! digital input #10 on a Doovit that only supports 4.
//!
//! Unary calls share one persistent channel with per-call deadlines and a
//! rebuild-and-retry-once path for `UNAVAILABLE` (see [`crate::docker::grpc`]).

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::task::JoinHandle;

use doover_proto::platform_iface as pb;
use pb::platform_iface_client::PlatformIfaceClient as GenClient;

use crate::docker::grpc::{check_response_header, SharedChannel};
use crate::error::{DooverError, Result};

/// Health-check service name (pydoover `PlatformInterface.service_name`).
const SERVICE_NAME: &str = "doover.PlatformInterface";

/// A digital-input edge selector (`"rising"`, `"falling"` or `"both"` in
/// pydoover's string-typed APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edge {
    #[default]
    Rising,
    Falling,
    Both,
}

impl Edge {
    /// The pydoover wire string (`"rising"`, `"falling"`, `"both"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Edge::Rising => "rising",
            Edge::Falling => "falling",
            Edge::Both => "both",
        }
    }

    /// The `rising`/`falling` flag pair `getDIEventsRequest` wants
    /// (pydoover `fetch_di_events`'s edge unpacking).
    fn flags(self) -> (bool, bool) {
        match self {
            Edge::Rising => (true, false),
            Edge::Falling => (false, true),
            Edge::Both => (true, true),
        }
    }
}

impl std::fmt::Display for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Edge {
    type Err = DooverError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "rising" => Ok(Edge::Rising),
            "falling" => Ok(Edge::Falling),
            "both" => Ok(Edge::Both),
            other => Err(DooverError::Other(format!("invalid edge: {other:?}"))),
        }
    }
}

/// A device location fix (pydoover `platform_types.Location`). All fields are
/// optional: hardware without a GPS/modem fix leaves them unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Location {
    /// Latitude in degrees.
    pub latitude: Option<f32>,
    /// Longitude in degrees.
    pub longitude: Option<f32>,
    /// Altitude in meters above sea level.
    pub altitude_m: Option<f32>,
    /// Accuracy of the location in meters.
    pub accuracy_m: Option<f32>,
    /// Speed in meters per second.
    pub speed_mps: Option<f32>,
    /// Heading in degrees (0-360).
    pub heading_deg: Option<f32>,
    /// Number of satellites used to determine the location.
    pub sat_count: Option<i32>,
    /// Timestamp of the fix in ISO 8601 format (e.g. `2023-10-01T12:00:00Z`).
    pub timestamp: Option<String>,
}

/// A platform event (pydoover `platform_types.Event`), e.g. a digital-input
/// edge recorded while the compute module was asleep.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformEvent {
    /// Unique identifier for the event.
    pub event_id: i32,
    /// The type of event, e.g. `DI_R` for rising edge, `DI_F` for falling.
    pub event: String,
    /// The digital input pin number the event occurred on.
    pub pin: i32,
    /// The pin value at the time of the event (e.g. `"1"` high, `"0"` low).
    pub value: String,
    /// The timestamp of the event in milliseconds since epoch.
    pub time: i64,
    /// Whether the CM4 was online at the time of the event, if applicable.
    pub cm4_online: Option<bool>,
}

impl PlatformEvent {
    fn from_proto(e: pb::EventDetail) -> Self {
        Self {
            event_id: e.event_id,
            event: e.event,
            pin: e.pin,
            value: e.value,
            time: e.time,
            cm4_online: e.cm4_online,
        }
    }
}

/// One item from the `startPulseCounter` server stream
/// (`pulseCounterResponse`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiPulse {
    pub di: Option<i32>,
    /// The digital-input value at the pulse (true = high).
    pub value: Option<bool>,
    /// Time since the previous pulse, seconds.
    pub dt_secs: Option<f32>,
}

/// What a pulse/event listener delivers to its callback — the argument tuple
/// of pydoover's `PulseCounterCallback` `(di, di_value, dt_secs, count, edge)`
/// as a struct (events-mode callbacks additionally get the event timestamp).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PulseCounterUpdate {
    /// The pin the pulse was received on.
    pub pin: i32,
    /// The digital-input value (true = high).
    pub value: bool,
    /// Time since the previous pulse, seconds.
    pub dt_secs: f64,
    /// Unix timestamp of the pulse (events mode only; live pulses are "now").
    pub timestamp: Option<f64>,
    /// Total pulses counted so far.
    pub count: u64,
    /// The edge the pulse was received on (`None` for non-DI events).
    pub edge: Option<Edge>,
}

/// Synchronous pulse callback (pydoover accepts sync or async callables;
/// spawn a task inside the callback for real work).
pub type PulseCallback = Arc<dyn Fn(&PulseCounterUpdate) + Send + Sync>;

/// Callback for system events seen while collecting offline DI events
/// (pydoover `PulseCounter.handle_system_event`, an overridable hook).
pub type SystemEventCallback = Arc<dyn Fn(&PlatformEvent) + Send + Sync>;

/// Patch for a digital input's stored configuration, applied by
/// [`PlatformClient::set_di_config`]. Only the fields you set are changed;
/// `None` keeps the existing stored value (pydoover `set_di_config` kwargs).
#[derive(Debug, Clone, Default)]
pub struct DiConfigUpdate {
    /// Whether the input is in PNP (sourcing) mode.
    pub pnp_mode: Option<bool>,
    /// The edge(s) that trigger an interrupt/event on this pin.
    pub irq_edge: Option<Edge>,
    /// Debounce time in milliseconds.
    pub debounce_ms: Option<i32>,
    /// Whether an event on this pin should wake the device from shutdown.
    pub wake_on_event: Option<bool>,
}

#[derive(Clone)]
pub struct PlatformClient {
    shared: Arc<SharedChannel>,
    /// Running DI pulse listener tasks (pydoover
    /// `pulse_counter_listeners`), aborted by [`close`](Self::close).
    listeners: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl PlatformClient {
    /// Connect to the platform sidecar (default `http://127.0.0.1:50053`).
    /// The channel is built lazily, as in pydoover — use
    /// [`wait_until_healthy`](Self::wait_until_healthy) to gate on the
    /// sidecar actually serving.
    pub async fn connect(uri: impl Into<String>) -> Result<Self> {
        Ok(Self {
            shared: Arc::new(SharedChannel::new(uri, SERVICE_NAME)?),
            listeners: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// One-shot `grpc.health.v1` probe (pydoover `health_check`).
    pub async fn health_check(&self) -> bool {
        self.shared.health_check().await
    }

    /// Poll until the sidecar reports healthy (pydoover
    /// `wait_until_healthy`); never gives up — bound it with
    /// `tokio::time::timeout` if you need a deadline.
    pub async fn wait_until_healthy(&self, interval: Duration) {
        self.shared.wait_until_healthy(interval).await
    }

    /// Cancel all running DI pulse listeners (pydoover `close`).
    pub fn close(&self) {
        let mut listeners = self.listeners.lock().expect("listener lock poisoned");
        for task in listeners.drain(..) {
            task.abort();
        }
    }

    /// Map a platform `ResponseHeader` to a typed error (pydoover
    /// `process_response`).
    fn check(header: Option<pb::ResponseHeader>) -> Result<()> {
        match header {
            Some(h) => check_response_header(h.success, h.response_code, h.message),
            None => Ok(()),
        }
    }

    /// Liveness echo (pydoover `test_comms`).
    pub async fn test_comms(&self, message: impl Into<String>) -> Result<String> {
        let req = pb::TestCommsRequest { message: message.into() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).test_comms(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.response)
    }

    // ------------------------------------------------------------------
    // Digital / analog IO
    // ------------------------------------------------------------------

    /// Read one digital-input pin: true = high (pydoover `fetch_di` with a
    /// single pin).
    pub async fn fetch_di(&self, pin: i32) -> Result<bool> {
        Ok(self.fetch_dis(&[pin]).await?.into_iter().next().unwrap_or(false))
    }

    /// Read several digital-input pins in one transaction (pydoover
    /// `fetch_di` with several pins).
    pub async fn fetch_dis(&self, pins: &[i32]) -> Result<Vec<bool>> {
        let req = pb::GetDiRequest { di: pins.to_vec() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_di(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.di)
    }

    /// Read one analog-input pin (mA) (pydoover `fetch_ai` with a single
    /// pin). Errors if the sidecar reports failure.
    pub async fn fetch_ai(&self, pin: i32) -> Result<f32> {
        Ok(self.fetch_ais(&[pin]).await?.into_iter().next().unwrap_or(0.0))
    }

    /// Read several analog-input pins in one transaction (pydoover
    /// `fetch_ai` with several pins).
    pub async fn fetch_ais(&self, pins: &[i32]) -> Result<Vec<f32>> {
        let req = pb::GetAiRequest { ai: pins.to_vec() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_ai(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.ai)
    }

    /// Read one digital-output pin (pydoover `fetch_do` with a single pin).
    pub async fn fetch_do(&self, pin: i32) -> Result<bool> {
        Ok(self.fetch_dos(&[pin]).await?.into_iter().next().unwrap_or(false))
    }

    /// Read several digital-output pins in one transaction (pydoover
    /// `fetch_do` with several pins).
    pub async fn fetch_dos(&self, pins: &[i32]) -> Result<Vec<bool>> {
        let req = pb::GetDoRequest { r#do: pins.to_vec() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_do(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.r#do)
    }

    /// Set one digital-output pin; returns the value that was set
    /// (pydoover `set_do` with a single pin/value).
    pub async fn set_do(&self, pin: i32, value: bool) -> Result<bool> {
        Ok(self.set_dos(&[pin], &[value]).await?.into_iter().next().unwrap_or(false))
    }

    /// Set several digital-output pins in one transaction. A single value
    /// broadcasts to every pin, otherwise the lists must be the same length
    /// (pydoover `set_do` / `_cast_pin_values`).
    pub async fn set_dos(&self, pins: &[i32], values: &[bool]) -> Result<Vec<bool>> {
        let values = broadcast_values(pins, values, "digital output")?;
        let req = pb::SetDoRequest { r#do: pins.to_vec(), value: values };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).set_do(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.r#do)
    }

    /// Schedule one digital-output pin to change in `in_secs` seconds
    /// (pydoover `schedule_do` with a single pin/value).
    pub async fn schedule_do(&self, pin: i32, value: bool, in_secs: u32) -> Result<()> {
        self.schedule_dos(&[pin], &[value], in_secs).await.map(|_| ())
    }

    /// Schedule several digital-output pins to change in `in_secs` seconds;
    /// single values broadcast as in [`set_dos`](Self::set_dos)
    /// (pydoover `schedule_do`).
    pub async fn schedule_dos(
        &self,
        pins: &[i32],
        values: &[bool],
        in_secs: u32,
    ) -> Result<Vec<bool>> {
        let values = broadcast_values(pins, values, "digital output")?;
        let req = pb::ScheduleDoRequest {
            r#do: pins.to_vec(),
            value: values,
            time_secs: Some(in_secs as f32),
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).schedule_do(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.r#do)
    }

    /// Read one analog-output pin (pydoover `fetch_ao` with a single pin).
    pub async fn fetch_ao(&self, pin: i32) -> Result<f32> {
        Ok(self.fetch_aos(&[pin]).await?.into_iter().next().unwrap_or(0.0))
    }

    /// Read several analog-output pins in one transaction (pydoover
    /// `fetch_ao` with several pins).
    pub async fn fetch_aos(&self, pins: &[i32]) -> Result<Vec<f32>> {
        let req = pb::GetAoRequest { ao: pins.to_vec() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_ao(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.ao)
    }

    /// Set one analog-output pin; returns the value that was set (pydoover
    /// `set_ao` with a single pin/value).
    pub async fn set_ao(&self, pin: i32, value: f32) -> Result<f32> {
        Ok(self.set_aos(&[pin], &[value]).await?.into_iter().next().unwrap_or(0.0))
    }

    /// Set several analog-output pins in one transaction. A single value
    /// broadcasts to every pin, otherwise the lists must be the same length
    /// (pydoover `set_ao` / `_cast_ao_pin_values`).
    pub async fn set_aos(&self, pins: &[i32], values: &[f32]) -> Result<Vec<f32>> {
        let values = broadcast_values(pins, values, "analogue output")?;
        let req = pb::SetAoRequest { ao: pins.to_vec(), value: values };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).set_ao(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.ao)
    }

    /// Schedule one analog-output pin to change in `in_secs` seconds
    /// (pydoover `schedule_ao` with a single pin/value).
    pub async fn schedule_ao(&self, pin: i32, value: f32, in_secs: u32) -> Result<()> {
        self.schedule_aos(&[pin], &[value], in_secs).await.map(|_| ())
    }

    /// Schedule several analog-output pins to change in `in_secs` seconds;
    /// single values broadcast as in [`set_aos`](Self::set_aos)
    /// (pydoover `schedule_ao`).
    pub async fn schedule_aos(
        &self,
        pins: &[i32],
        values: &[f32],
        in_secs: u32,
    ) -> Result<Vec<f32>> {
        let values = broadcast_values(pins, values, "analogue output")?;
        let req = pb::ScheduleAoRequest {
            ao: pins.to_vec(),
            value: values,
            time_secs: Some(in_secs as f32),
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).schedule_ao(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.ao)
    }

    // ------------------------------------------------------------------
    // System status
    // ------------------------------------------------------------------

    /// The system input voltage in volts, typically from a power supply or
    /// battery; `None` when unsupported (pydoover `fetch_system_voltage`).
    pub async fn fetch_system_voltage(&self) -> Result<Option<f32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_input_voltage(pb::GetInputVoltageRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.voltage)
    }

    /// The system input power in watts; `None` when unsupported (pydoover
    /// `fetch_system_power`).
    pub async fn fetch_system_power(&self) -> Result<Option<f32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_system_power(pb::GetSystemPowerRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.power_watts)
    }

    /// The system temperature in degrees Celsius — on a Doovit, the
    /// Raspberry Pi CM4's; `None` when unsupported (pydoover
    /// `fetch_system_temperature`).
    pub async fn fetch_system_temperature(&self) -> Result<Option<f32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_temperature(pb::GetTemperatureRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.temperature)
    }

    /// The device location. Doovits with 4G cards generally implement this
    /// via ModemManager (pydoover `fetch_location`).
    pub async fn fetch_location(&self) -> Result<Location> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_location(pb::GetLocationRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(Location {
            latitude: resp.latitude,
            longitude: resp.longitude,
            altitude_m: resp.altitude_m,
            accuracy_m: resp.accuracy_m,
            speed_mps: resp.speed_mps,
            heading_deg: resp.heading_deg,
            sat_count: resp.sat_count,
            timestamp: resp.timestamp,
        })
    }

    /// The IO table advertised by the platform, as JSON; `None` when
    /// unsupported (pydoover `fetch_io_table`).
    pub async fn fetch_io_table(&self) -> Result<Option<Value>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_io_table(pb::GetIoTableRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        match resp.io_table {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    /// Synchronize the real-time clock with the system (network) time. On
    /// Doovits `doovitd` does this automatically (pydoover `sync_rtc`).
    pub async fn sync_rtc(&self) -> Result<()> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).sync_rtc_time(pb::SyncRtcTimeRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)
    }

    // ------------------------------------------------------------------
    // Shutdown / wake management
    // ------------------------------------------------------------------

    /// Reboot the device (pydoover `reboot`). You should **not** call this
    /// directly — see <https://docs.doover.com/guide/app-shutdown> for how to
    /// safely initiate a shutdown from an application.
    pub async fn reboot(&self) -> Result<()> {
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).reboot(pb::RebootRequest {}).await })
            .await?;
        Self::check(resp.response_header)
    }

    /// Shut the device down (pydoover `shutdown`). You should **not** call
    /// this directly — see <https://docs.doover.com/guide/app-shutdown>.
    pub async fn shutdown(&self) -> Result<()> {
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).shutdown(pb::ShutdownRequest {}).await })
            .await?;
        Self::check(resp.response_header)
    }

    /// Seconds for which the device ignores shutdown requests (pydoover
    /// `fetch_immunity_seconds`).
    pub async fn fetch_immunity_seconds(&self) -> Result<Option<i32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch)
                    .get_shutdown_immunity(pb::GetShutdownImmunityRequest {})
                    .await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.immunity_secs)
    }

    /// Set the number of seconds the device ignores shutdown requests for
    /// (pydoover `set_immunity_seconds`).
    pub async fn set_immunity_seconds(&self, immunity_secs: i32) -> Result<Option<i32>> {
        let req = pb::SetShutdownImmunityRequest { immunity_secs };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).set_shutdown_immunity(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.immunity_secs)
    }

    /// The input-voltage threshold at which the device wakes from shutdown
    /// (pydoover `fetch_wake_on_voltage`).
    pub async fn fetch_wake_on_voltage(&self) -> Result<Option<f32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_wake_on_voltage(pb::GetWakeOnVoltageRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.voltage)
    }

    /// Set the input-voltage threshold at which the device wakes from
    /// shutdown; `None` disables it (pydoover `set_wake_on_voltage`).
    pub async fn set_wake_on_voltage(&self, voltage: Option<f32>) -> Result<Option<f32>> {
        let req = pb::SetWakeOnVoltageRequest { voltage };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).set_wake_on_voltage(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.voltage)
    }

    /// Why the device was most recently woken from shutdown: one of `rpc`,
    /// `button`, `voltage`, `di_<pin>_event`, `scheduled`, `max_off`,
    /// `external` or `reboot`; `None` if never woken (pydoover
    /// `fetch_wake_reason`).
    pub async fn fetch_wake_reason(&self) -> Result<Option<String>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_wake_reason(pb::GetWakeReasonRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.wake_reason)
    }

    /// Schedule the device to start up in `time_secs` seconds; returns the
    /// scheduled delay (pydoover `schedule_startup`).
    pub async fn schedule_startup(&self, time_secs: u32) -> Result<Option<f32>> {
        let req = pb::ScheduleStartupRequest { time_secs: Some(time_secs as f32) };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).schedule_startup(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.time_secs)
    }

    /// Schedule the device to shut down in `time_secs` seconds; returns the
    /// scheduled delay (pydoover `schedule_shutdown`).
    pub async fn schedule_shutdown(&self, time_secs: u32) -> Result<Option<f32>> {
        let req = pb::ScheduleShutdownRequest { time_secs: Some(time_secs as f32) };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).schedule_shutdown(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.time_secs)
    }

    // ------------------------------------------------------------------
    // Sleep log
    // ------------------------------------------------------------------

    /// System-status snapshots captured while the device was asleep, oldest
    /// first (pydoover `fetch_sleep_log`). `since` is epoch milliseconds; 0
    /// returns all stored snapshots (capped at 100, oldest dropped first).
    /// Returns the raw proto entries (`doover::proto`), like pydoover.
    pub async fn fetch_sleep_log(&self, since: i64) -> Result<Vec<pb::SleepLogEntry>> {
        let req = pb::GetSleepLogRequest { since: Some(since) };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).get_sleep_log(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.entries)
    }

    /// The interval between sleep-log snapshots in seconds; 0 means sleep
    /// logging is disabled (pydoover `fetch_sleep_log_interval`).
    pub async fn fetch_sleep_log_interval(&self) -> Result<Option<i32>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch)
                    .get_sleep_log_interval(pb::GetSleepLogIntervalRequest {})
                    .await
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.interval_secs)
    }

    /// Set the interval between sleep-log snapshots; 0 disables sleep
    /// logging. Returns the interval that was set (pydoover
    /// `set_sleep_log_interval`).
    pub async fn set_sleep_log_interval(&self, interval_secs: i32) -> Result<Option<i32>> {
        let req = pb::SetSleepLogIntervalRequest { interval_secs: Some(interval_secs) };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).set_sleep_log_interval(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.interval_secs)
    }

    // ------------------------------------------------------------------
    // Events + DI config
    // ------------------------------------------------------------------

    /// All platform events from `events_from` (event id or epoch
    /// milliseconds; 0 = all available) (pydoover `fetch_events`).
    pub async fn fetch_events(&self, events_from: i64) -> Result<Vec<PlatformEvent>> {
        let req = pb::GetEventsRequest { events_from: Some(events_from) };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).get_events(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.events.into_iter().map(PlatformEvent::from_proto).collect())
    }

    /// Digital-input events for a pin: whether the event log is synced, and
    /// the events (pydoover `fetch_di_events`). `include_system_events` adds
    /// events like the CM4 or IO board power-cycling; `events_from` is a
    /// starting event id or epoch-milliseconds timestamp (0 = all).
    pub async fn fetch_di_events(
        &self,
        pin: i32,
        edge: Edge,
        include_system_events: bool,
        events_from: i64,
    ) -> Result<(bool, Vec<PlatformEvent>)> {
        let (rising, falling) = edge.flags();
        let req = pb::GetDiEventsRequest {
            pin,
            rising,
            falling,
            include_system_events,
            events_from: Some(events_from),
        };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).get_di_events(req).await })
            .await?;
        Self::check(resp.response_header)?;
        let events = resp.events.into_iter().map(PlatformEvent::from_proto).collect();
        Ok((resp.events_synced.unwrap_or(false), events))
    }

    /// The stored configuration for a digital-input pin, as the raw proto
    /// message (`doover::proto`), like pydoover (pydoover `fetch_di_config`).
    pub async fn fetch_di_config(&self, pin: i32) -> Result<Option<pb::DiConfig>> {
        let req = pb::GetDiConfigRequest { pin };
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).get_di_config(req).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.config)
    }

    /// Patch the configuration for a digital-input pin; unset fields keep
    /// their stored values. Returns the resulting configuration (pydoover
    /// `set_di_config`).
    pub async fn set_di_config(
        &self,
        pin: i32,
        update: &DiConfigUpdate,
    ) -> Result<Option<pb::DiConfig>> {
        let req = pb::SetDiConfigRequest {
            pin,
            pnp_mode: update.pnp_mode,
            irq_edge: update.irq_edge.map(|e| e.as_str().to_string()),
            debounce_ms: update.debounce_ms,
            wake_on_event: update.wake_on_event,
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).set_di_config(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.config)
    }

    // ------------------------------------------------------------------
    // DI pulse streaming
    // ------------------------------------------------------------------

    /// Subscribe to live pulses on a digital-input pin — the raw
    /// `startPulseCounter` server stream on a channel of its own, like
    /// pydoover's per-stream `grpc.aio.insecure_channel`. The stream ends
    /// when the sidecar closes it or the connection drops; callers should
    /// reconnect (or use
    /// [`start_di_pulse_listener`](Self::start_di_pulse_listener), which
    /// does).
    pub async fn subscribe_di_pulses(
        &self,
        di: i32,
        edge: Edge,
    ) -> Result<impl Stream<Item = Result<DiPulse>>> {
        let req = pb::PulseCounterRequest { di, edge: edge.as_str().to_string() };
        let mut client = GenClient::new(self.shared.fresh_channel());
        let stream = client.start_pulse_counter(req).await?.into_inner();
        Ok(stream.map(|item| {
            let resp = item?;
            Self::check(resp.response_header)?;
            Ok(DiPulse { di: resp.di, value: resp.value, dt_secs: resp.dt_secs })
        }))
    }

    /// Spawn a background task that listens for pulses on a digital-input
    /// pin forever, reconnecting on stream failure, counting pulses from
    /// `start_count`, and invoking `callback` for each pulse with a positive
    /// `dt_secs` (pydoover `start_di_pulse_listener` / `recv_di_pulses`).
    ///
    /// The task is cancelled by [`close`](Self::close); the returned handle
    /// can abort it individually.
    pub fn start_di_pulse_listener(
        &self,
        di: i32,
        edge: Edge,
        start_count: u64,
        callback: PulseCallback,
    ) -> tokio::task::AbortHandle {
        let client = self.clone();
        let task = tokio::spawn(async move {
            let mut counter = start_count;
            loop {
                match client.subscribe_di_pulses(di, edge).await {
                    Ok(mut stream) => {
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(pulse) => {
                                    let dt_secs = pulse.dt_secs.unwrap_or(0.0);
                                    // pydoover only counts pulses with dt > 0.
                                    if dt_secs > 0.0 {
                                        counter += 1;
                                        callback(&PulseCounterUpdate {
                                            pin: di,
                                            value: pulse.value.unwrap_or(false),
                                            dt_secs: dt_secs as f64,
                                            timestamp: None,
                                            count: counter,
                                            edge: Some(edge),
                                        });
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("error receiving pulse for di={di}: {e}");
                                    break;
                                }
                            }
                        }
                        tracing::info!("pulseCounter for di={di} ended.");
                    }
                    Err(e) => tracing::error!("error subscribing to pulses for di={di}: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let abort = task.abort_handle();
        // Track for close() (pydoover `pulse_counter_listeners`).
        self.listeners.lock().expect("listener lock poisoned").push(task);
        abort
    }

    /// Create a [`PulseCounter`] counting live pulses on a digital-input pin
    /// (pydoover `get_new_pulse_counter`). With `auto_start` the listener
    /// starts immediately; otherwise call
    /// [`PulseCounter::start_listener_pulses`].
    pub fn get_new_pulse_counter(
        &self,
        di: i32,
        edge: Edge,
        callback: Option<PulseCallback>,
        rate_window_secs: f64,
        auto_start: bool,
    ) -> PulseCounter {
        let counter = PulseCounter::new(self.clone(), di, edge, callback, rate_window_secs);
        if auto_start {
            counter.start_listener_pulses();
        }
        counter
    }

    /// Create a [`PulseCounter`] counting *offline* DI events (pydoover
    /// `get_new_event_counter`). With `auto_collect` the stored events are
    /// fetched and processed immediately.
    pub async fn get_new_event_counter(
        &self,
        di: i32,
        edge: Edge,
        callback: Option<PulseCallback>,
        rate_window_secs: f64,
        auto_collect: bool,
    ) -> Result<PulseCounter> {
        let counter = PulseCounter::new(self.clone(), di, edge, callback, rate_window_secs);
        if auto_collect {
            counter.update_events().await?;
        }
        Ok(counter)
    }
}

/// Broadcast a single value across all pins, or require matching lengths
/// (pydoover `_cast_pin_values` / `_cast_ao_pin_values`).
fn broadcast_values<T: Copy>(pins: &[i32], values: &[T], kind: &str) -> Result<Vec<T>> {
    if values.len() == pins.len() {
        Ok(values.to_vec())
    } else if values.len() == 1 {
        Ok(vec![values[0]; pins.len()])
    } else {
        Err(DooverError::Other(format!(
            "{kind} and value lists are not the same length"
        )))
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Pulses arriving within this many seconds of the listener starting are
/// ignored (pydoover `PulseCounter.pulse_grace_period`).
const PULSE_GRACE_PERIOD: f64 = 0.2;

#[derive(Debug)]
struct PulseCounterState {
    count: u64,
    pulse_timestamps: Vec<f64>,
    rate_window_secs: f64,
    receiving_pulses: bool,
    receiving_events: bool,
    start_time: f64,
}

/// Counts pulses on a digital-input pin (pydoover
/// `docker.platform.PulseCounter`).
///
/// Create one through [`PlatformClient::get_new_pulse_counter`] (live pulses
/// via the `startPulseCounter` stream) or
/// [`PlatformClient::get_new_event_counter`] (offline events via
/// `getDIEvents`). As in pydoover, one counter must not mix the two modes.
pub struct PulseCounter {
    client: PlatformClient,
    pin: i32,
    edge: Edge,
    callback: Option<PulseCallback>,
    system_event_callback: Option<SystemEventCallback>,
    state: Arc<Mutex<PulseCounterState>>,
}

impl PulseCounter {
    fn new(
        client: PlatformClient,
        pin: i32,
        edge: Edge,
        callback: Option<PulseCallback>,
        rate_window_secs: f64,
    ) -> Self {
        Self {
            client,
            pin,
            edge,
            callback,
            system_event_callback: None,
            state: Arc::new(Mutex::new(PulseCounterState {
                count: 0,
                pulse_timestamps: Vec::new(),
                rate_window_secs,
                receiving_pulses: false,
                receiving_events: false,
                start_time: unix_now(),
            })),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PulseCounterState> {
        self.state.lock().expect("pulse counter lock poisoned")
    }

    /// Set the hook called for system events seen while collecting offline
    /// events (pydoover `PulseCounter.handle_system_event`, an overridable
    /// method there).
    pub fn set_system_event_callback(&mut self, callback: SystemEventCallback) {
        self.system_event_callback = Some(callback);
    }

    /// Start listening for live pulses on the pin (pydoover
    /// `start_listener_pulses` + `receive_pulse`). Not compatible with a
    /// counter already receiving offline events.
    pub fn start_listener_pulses(&self) {
        let start_count = {
            let mut st = self.state();
            if st.receiving_events {
                tracing::error!("using a pulse counter for both pulses and offline events");
                return;
            }
            st.receiving_pulses = true;
            st.start_time = unix_now();
            st.count
        };

        let state = Arc::clone(&self.state);
        let user_callback = self.callback.clone();
        let pin = self.pin;
        let edge = self.edge;
        // The port of pydoover `receive_pulse`: grace period, count, record
        // timestamp, then invoke the user callback with *this* counter's
        // count (the listener's own counter is ignored, as in pydoover).
        let on_pulse: PulseCallback = Arc::new(move |update: &PulseCounterUpdate| {
            let update = {
                let mut st = state.lock().expect("pulse counter lock poisoned");
                let now = unix_now();
                if now - st.start_time < PULSE_GRACE_PERIOD {
                    tracing::info!("ignoring pulse on di={pin} with dt={}s", update.dt_secs);
                    return;
                }
                tracing::debug!("received pulse on di={pin} with dt={}s", update.dt_secs);
                st.count += 1;
                st.pulse_timestamps.push(now);
                PulseCounterUpdate { count: st.count, timestamp: None, ..*update }
            };
            if let Some(cb) = &user_callback {
                cb(&update);
            }
        });
        self.client.start_di_pulse_listener(pin, edge, start_count, on_pulse);
    }

    /// Fetch and process stored offline events for the pin (pydoover
    /// `update_events`). Not compatible with a counter already receiving
    /// live pulses.
    pub async fn update_events(&self) -> Result<()> {
        {
            let mut st = self.state();
            if st.receiving_pulses {
                tracing::error!("using a pulse counter for both pulses and offline events");
                return Ok(());
            }
            st.receiving_events = true;
        }
        let (_synced, events) =
            self.client.fetch_di_events(self.pin, self.edge, true, 0).await?;
        self.receive_events(&events);
        Ok(())
    }

    /// Seed the counter from already-known pulse timestamps (pydoover
    /// `add_existing_events`).
    pub fn add_existing_events(&self, timestamps: &[f64]) {
        let mut st = self.state();
        if st.receiving_pulses {
            tracing::error!("using a pulse counter for both pulses and offline events");
            return;
        }
        st.receiving_events = true;
        st.pulse_timestamps.extend_from_slice(timestamps);
        st.count = st.pulse_timestamps.len() as u64;
    }

    /// Process a batch of platform events into pulses (pydoover
    /// `receive_events`): `DI_R`/`DI_F` count as rising/falling pulses,
    /// out-of-order events (within 10 ms of the newest) are dropped, and
    /// anything else goes to the system-event callback.
    pub fn receive_events(&self, events: &[PlatformEvent]) {
        {
            let mut st = self.state();
            if st.receiving_pulses {
                tracing::error!("using a pulse counter for both pulses and offline events");
                return;
            }
            st.receiving_events = true;
        }

        for event in events {
            let (value, edge) = match event.event.as_str() {
                "DI_R" => (true, Some(Edge::Rising)),
                "DI_F" => (false, Some(Edge::Falling)),
                // Voltage events count as pulses with no DI edge (pydoover's
                // handling of these is a latent TypeError; we count them).
                "VI" => (false, None),
                _ => {
                    // Could be a system event.
                    if let Some(cb) = &self.system_event_callback {
                        cb(event);
                    }
                    continue;
                }
            };

            let update = {
                let mut st = self.state();
                let mut timestamp = event.time as f64 / 1000.0;
                if timestamp == 0.0 {
                    timestamp = unix_now();
                }
                let mut dt_secs = 0.0;
                if let Some(&last) = st.pulse_timestamps.last() {
                    if timestamp <= last + 0.01 {
                        tracing::warn!(
                            "ignoring old event on di={} t={timestamp} latest event: {last}",
                            event.pin
                        );
                        continue;
                    }
                    dt_secs = timestamp - last;
                }
                tracing::info!("received event on di={} with t={dt_secs}s", event.pin);
                st.count += 1;
                st.pulse_timestamps.push(timestamp);
                PulseCounterUpdate {
                    pin: self.pin,
                    value,
                    dt_secs,
                    timestamp: Some(timestamp),
                    count: st.count,
                    edge,
                }
            };
            if let Some(cb) = &self.callback {
                cb(&update);
            }
        }
    }

    /// The pulse timestamps within the rate window, measured back from the
    /// newest pulse (pydoover `get_pulses_in_window`).
    pub fn get_pulses_in_window(&self) -> Vec<f64> {
        let st = self.state();
        let Some(&newest) = st.pulse_timestamps.last() else {
            return Vec::new();
        };
        let cutoff = newest - st.rate_window_secs;
        st.pulse_timestamps.iter().copied().filter(|&t| t > cutoff).collect()
    }

    /// Pulses per minute over the rate window (pydoover
    /// `get_pulses_per_minute`).
    pub fn get_pulses_per_minute(&self) -> f64 {
        let window = self.get_pulses_in_window().len() as f64;
        window * 60.0 / self.state().rate_window_secs
    }

    /// Set the rate-window size in seconds (pydoover `set_rate_window`).
    pub fn set_rate_window(&self, rate_window_secs: f64) {
        self.state().rate_window_secs = rate_window_secs;
    }

    /// The rate-window size in seconds (pydoover `get_rate_window`).
    pub fn get_rate_window(&self) -> f64 {
        self.state().rate_window_secs
    }

    /// Overwrite the pulse count (pydoover `set_counter`).
    pub fn set_counter(&self, counter: u64) {
        self.state().count = counter;
    }

    /// The total pulses counted (pydoover `get_counter`).
    pub fn get_counter(&self) -> u64 {
        self.state().count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_roundtrip() {
        for edge in [Edge::Rising, Edge::Falling, Edge::Both] {
            assert_eq!(edge.as_str().parse::<Edge>().unwrap(), edge);
        }
        assert!("sideways".parse::<Edge>().is_err());
        assert_eq!(Edge::default(), Edge::Rising);
    }

    #[test]
    fn edge_flags_match_pydoover_fetch_di_events() {
        assert_eq!(Edge::Rising.flags(), (true, false));
        assert_eq!(Edge::Falling.flags(), (false, true));
        assert_eq!(Edge::Both.flags(), (true, true));
    }

    #[test]
    fn broadcast_single_value_to_all_pins() {
        assert_eq!(broadcast_values(&[1, 4, 2], &[true], "do").unwrap(), vec![true; 3]);
        assert_eq!(
            broadcast_values(&[1, 2], &[false, true], "do").unwrap(),
            vec![false, true]
        );
        assert!(broadcast_values(&[1, 2, 3], &[true, false], "do").is_err());
    }

    fn test_counter(callback: Option<PulseCallback>) -> PulseCounter {
        let client = PlatformClient {
            shared: Arc::new(SharedChannel::new("http://127.0.0.1:1", SERVICE_NAME).unwrap()),
            listeners: Arc::new(Mutex::new(Vec::new())),
        };
        PulseCounter::new(client, 0, Edge::Rising, callback, 60.0)
    }

    fn di_event(event: &str, time_ms: i64) -> PlatformEvent {
        PlatformEvent {
            event_id: 1,
            event: event.to_string(),
            pin: 0,
            value: "1".to_string(),
            time: time_ms,
            cm4_online: None,
        }
    }

    #[test]
    fn receive_events_counts_and_dedups() {
        let seen: Arc<Mutex<Vec<PulseCounterUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let counter =
            test_counter(Some(Arc::new(move |u: &PulseCounterUpdate| {
                sink.lock().unwrap().push(*u);
            })));

        counter.receive_events(&[
            di_event("DI_R", 10_000),
            di_event("DI_F", 12_000),
            // Within 10ms of the newest pulse: dropped as an old event.
            di_event("DI_R", 12_005),
            // System events don't count.
            di_event("CM4_ON", 13_000),
            di_event("DI_R", 14_000),
        ]);

        assert_eq!(counter.get_counter(), 3);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        // First pulse has dt 0 (no prior pulse).
        assert_eq!(seen[0].dt_secs, 0.0);
        assert_eq!(seen[0].edge, Some(Edge::Rising));
        assert!(seen[0].value);
        // Second pulse: 2s after the first.
        assert_eq!(seen[1].dt_secs, 2.0);
        assert_eq!(seen[1].edge, Some(Edge::Falling));
        assert!(!seen[1].value);
        assert_eq!(seen[2].count, 3);
    }

    #[test]
    fn pulses_per_minute_over_window() {
        let counter = test_counter(None);
        // 4 pulses, 10s apart; 60s window from the newest (t=130) covers
        // t>70, i.e. all 4.
        counter.add_existing_events(&[100.0, 110.0, 120.0, 130.0]);
        assert_eq!(counter.get_counter(), 4);
        assert_eq!(counter.get_pulses_in_window().len(), 4);
        assert_eq!(counter.get_pulses_per_minute(), 4.0);

        // Shrink the window: only pulses newer than 130-15=115 remain.
        counter.set_rate_window(15.0);
        assert_eq!(counter.get_pulses_in_window(), vec![120.0, 130.0]);
        assert_eq!(counter.get_pulses_per_minute(), 8.0);
    }

    #[test]
    fn counter_modes_are_exclusive() {
        let counter = test_counter(None);
        counter.add_existing_events(&[1.0]);
        // Now in events mode; live-pulse mode must refuse to start.
        counter.start_listener_pulses();
        assert!(!counter.state().receiving_pulses);
    }
}
