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

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use doover_proto::platform_iface as pb;
use pb::platform_iface_client::PlatformIfaceClient as GenClient;

use crate::docker::grpc::{check_response_header, SharedChannel};
use crate::error::{DooverError, Result};

/// Health-check service name: the proto's own service name, which is what
/// the platform interface registers its gRPC health under.
const SERVICE_NAME: &str = "platform_iface.platformIface";

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

/// A device location fix (pydoover `platform_types.Location`).
///
/// Latitude and longitude are always present — a response missing either means
/// the device has no fix, which
/// [`fetch_location`](PlatformClient::fetch_location) reports as `None` rather
/// than a `Location` sitting at the proto default `(0, 0)`. The rest of the
/// fields are genuinely optional: not every modem reports them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Location {
    /// Latitude in degrees.
    pub latitude: f32,
    /// Longitude in degrees.
    pub longitude: f32,
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

/// One digital input's level, plus its hardware pulse counters if it has any
/// (pydoover `platform_types.DIReading`).
///
/// `pulse_count` is the device's own totaliser, not a tally kept here: it
/// survives an app restart, and on some platforms it wraps rather than
/// resetting. `None` means the pin has no hardware counter; `Some(0)` means it
/// has one and has seen nothing — so test for `None`, not zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiReading {
    pub pin: i32,
    /// Pin level: true is high (1), false is low (0).
    pub value: bool,
    pub pulse_count: Option<u64>,
    /// `None` means the platform does not measure rate on this pin — a driver
    /// with only a totaliser leaves it unset rather than differencing counts,
    /// because the caller knows its own sampling interval and the driver does
    /// not. Derive the rate from successive `pulse_count` readings instead.
    pub pulse_rate_hz: Option<f32>,
}

/// One IO channel in the flat namespace shared with
/// [`fetch_di`](PlatformClient::fetch_di) / [`set_do`](PlatformClient::set_do)
/// and friends (pydoover `platform_types.IoChannel`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoChannel {
    /// Global flat channel number — the number you pass to `fetch_di`,
    /// `set_do`, etc.
    pub channel: i32,
    /// Channel number on the owning device (e.g. DI 0 of slave 2).
    pub device_channel: i32,
    /// One of `"DI"`, `"DO"`, `"AI"`, `"AO"`.
    pub io_type: String,
    /// Analog channels only: e.g. `"voltage"`, `"current"`, `"temperature"`.
    pub kind: Option<String>,
    /// Display units, e.g. `"V"`, `"mA"`, `"degC"`.
    pub units: Option<String>,
    /// Whether DI edge events ([`fetch_di_events`](PlatformClient::fetch_di_events))
    /// work on this channel.
    pub supports_events: bool,
    /// Whether pulse counters work on this channel.
    pub supports_pulse_counter: bool,
    /// Whether DI config (PNP/NPN, debounce, wake-on-event) works on this
    /// channel.
    pub supports_di_config: bool,
    /// Whether this channel has a hardware pulse totaliser, read through
    /// [`fetch_di_readings`](PlatformClient::fetch_di_readings). Distinct from
    /// `supports_pulse_counter`: that promises per-pulse *timing*, which needs
    /// an edge source, while this is a number that can simply be read. A
    /// platform can have either, both or neither — an ELPRO Quantum counts on
    /// DIO1-4 and can time nothing.
    pub supports_pulse_count: bool,
    /// Whether the hardware measures pulse frequency on this channel.
    pub supports_pulse_rate: bool,
}

impl IoChannel {
    fn from_proto(c: pb::IoChannelDetail) -> Self {
        Self {
            channel: c.channel,
            device_channel: c.device_channel,
            io_type: c.io_type,
            kind: c.kind,
            units: c.units,
            supports_events: c.supports_events,
            supports_pulse_counter: c.supports_pulse_counter,
            supports_di_config: c.supports_di_config,
            supports_pulse_count: c.supports_pulse_count,
            supports_pulse_rate: c.supports_pulse_rate,
        }
    }
}

/// The master or one slave, with the channels it contributes to the flat
/// namespace (pydoover `platform_types.IoDevice`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoDevice {
    /// `"master"`, or the slave's configured name.
    pub name: String,
    /// Driver type, e.g. `"doovit"`, `"moxa1242"`, `"point_io"`. Empty when
    /// synthesized from a platform interface that only supports
    /// [`fetch_io_table`](PlatformClient::fetch_io_table).
    pub type_name: String,
    /// Slave index; 0 for the master — check [`is_master`](Self::is_master),
    /// not the index.
    pub index: i32,
    pub is_master: bool,
    /// Best-effort connectivity to the device.
    pub online: bool,
    /// The channels this device contributes, in flat-channel order.
    pub channels: Vec<IoChannel>,
}

impl IoDevice {
    /// This device's channels of one IO type (`"DI"`, `"DO"`, `"AI"`, `"AO"`) —
    /// pydoover `IoDevice.channels_of`.
    pub fn channels_of(&self, io_type: &str) -> Vec<&IoChannel> {
        self.channels.iter().filter(|c| c.io_type == io_type).collect()
    }

    fn from_proto(d: pb::IoDeviceDetail) -> Self {
        Self {
            name: d.name,
            type_name: d.r#type,
            index: d.index,
            is_master: d.is_master,
            online: d.online,
            channels: d.channels.into_iter().map(IoChannel::from_proto).collect(),
        }
    }
}

/// The full IO layout of a device: master plus any configured slaves
/// (pydoover `platform_types.IoDetails`).
///
/// This is the discovery result for apps that adapt to whatever IO the device
/// actually has, rather than hardcoding channel counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoDetails {
    /// Master first, then slaves in index order — matching the flat channel
    /// numbering the platform interface assigns.
    pub devices: Vec<IoDevice>,
}

impl IoDetails {
    /// The master device, or `None` if the layout marks no master.
    pub fn master(&self) -> Option<&IoDevice> {
        self.devices.iter().find(|d| d.is_master)
    }

    /// All channels of one IO type across every device, in flat-channel order
    /// (pydoover `IoDetails.channels`).
    pub fn channels(&self, io_type: &str) -> Vec<&IoChannel> {
        let mut found: Vec<&IoChannel> = self
            .devices
            .iter()
            .flat_map(|d| d.channels.iter())
            .filter(|c| c.io_type == io_type)
            .collect();
        found.sort_by_key(|c| c.channel);
        found
    }

    fn from_proto(devices: Vec<pb::IoDeviceDetail>) -> Self {
        Self { devices: devices.into_iter().map(IoDevice::from_proto).collect() }
    }

    /// Synthesize an `IoDetails` from a [`fetch_io_table`](PlatformClient::fetch_io_table)
    /// mapping (pydoover `IoDetails.from_io_table`).
    ///
    /// The fallback for platform interfaces that predate `getIoDetails`: the
    /// table only lists flat channel numbers per IO type, so everything lands on
    /// one anonymous master device with no per-channel metadata or capability
    /// flags. Non-numeric entries are skipped rather than guessed at.
    pub fn from_io_table(io_table: &Value) -> Self {
        let mut channels = Vec::new();
        if let Some(table) = io_table.as_object() {
            for (io_type, entries) in table {
                let Some(entries) = entries.as_array() else { continue };
                for entry in entries {
                    // pydoover's `int(ch)` accepts an int or a numeric string.
                    let Some(number) = entry
                        .as_i64()
                        .or_else(|| entry.as_str().and_then(|s| s.parse::<i64>().ok()))
                    else {
                        continue;
                    };
                    channels.push(IoChannel {
                        channel: number as i32,
                        device_channel: number as i32,
                        io_type: io_type.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        Self {
            devices: vec![IoDevice {
                name: "master".to_string(),
                type_name: String::new(),
                index: 0,
                is_master: true,
                online: true,
                channels,
            }],
        }
    }
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
        let req = pb::GetDiRequest {
            di: pins.to_vec(),
            include_pulses: None,
        };
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

    /// Read digital-input levels together with their hardware pulse counters
    /// (pydoover `fetch_di_readings`).
    ///
    /// Unlike [`fetch_dis`](Self::fetch_dis) this returns the pin level *and*,
    /// on platforms that have them, the hardware pulse totaliser and rate.
    /// Level and counts come from the same read, so they describe the same
    /// instant.
    ///
    /// Support is per-pin, not per-device, so `pulse_count`/`pulse_rate_hz`
    /// are `None` on a pin without the hardware and `Some(0)` on one that has
    /// it and has seen nothing.
    pub async fn fetch_di_readings(&self, pins: &[i32]) -> Result<Vec<DiReading>> {
        let req = pb::GetDiRequest {
            di: pins.to_vec(),
            include_pulses: Some(true),
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_di(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;

        if resp.readings.is_empty() {
            // Either the platform interface predates `DIReading`, or its
            // counter read failed and it fell back to levels. Both mean the
            // same thing to a caller — levels are known, counts are not —
            // which is exactly what `pulse_count: None` says.
            return Ok(pins
                .iter()
                .zip(resp.di)
                .map(|(&pin, value)| DiReading {
                    pin,
                    value,
                    pulse_count: None,
                    pulse_rate_hz: None,
                })
                .collect());
        }

        Ok(resp
            .readings
            .into_iter()
            .map(|r| DiReading {
                pin: r.pin,
                value: r.value,
                // Proto presence, not zero-testing: a pin that has counted 0
                // pulses is not the same as a pin that cannot count.
                pulse_count: r.pulse_count,
                pulse_rate_hz: r.pulse_rate_hz,
            })
            .collect())
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

    /// Read the load current (amps) drawn through one digital output
    /// (pydoover `fetch_do_current` with a single pin).
    pub async fn fetch_do_current(&self, pin: i32) -> Result<f32> {
        Ok(self.fetch_do_currents(&[pin]).await?.into_iter().next().unwrap_or(0.0))
    }

    /// Read the load current (amps) drawn through several digital outputs
    /// (pydoover `fetch_do_current` with several pins).
    ///
    /// This measures what the output is switching, not the system supply
    /// current — for that see [`fetch_system_power`](Self::fetch_system_power).
    /// The sidecar briefly enables each output driver's diagnostic mode to take
    /// the reading and reads the pins one at a time, so this is slower than
    /// [`fetch_dos`](Self::fetch_dos) and is not meant to be polled tightly.
    /// Platforms without per-output current sensing answer with a failure
    /// header, which surfaces here as an error rather than an empty vec.
    pub async fn fetch_do_currents(&self, pins: &[i32]) -> Result<Vec<f32>> {
        let req = pb::GetDoCurrentRequest { r#do: pins.to_vec() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).get_do_current(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.current)
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

    /// The device location, or `None` when the device has no fix. Doovits with
    /// 4G cards generally implement this via ModemManager (pydoover
    /// `fetch_location`).
    ///
    /// `getLocationResponse` carries the fix as flat optional fields, so a
    /// missing latitude or longitude is "no fix" — never a `Location` at the
    /// proto default `(0, 0)`, which reads as a real position off West Africa.
    pub async fn fetch_location(&self) -> Result<Option<Location>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_location(pb::GetLocationRequest {}).await
            })
            .await?;
        Self::check(resp.response_header)?;
        let (Some(latitude), Some(longitude)) = (resp.latitude, resp.longitude) else {
            return Ok(None);
        };
        Ok(Some(Location {
            latitude,
            longitude,
            altitude_m: resp.altitude_m,
            accuracy_m: resp.accuracy_m,
            speed_mps: resp.speed_mps,
            heading_deg: resp.heading_deg,
            sat_count: resp.sat_count,
            timestamp: resp.timestamp,
        }))
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

    /// The full IO layout — master plus any configured slaves — for apps that
    /// adapt to whatever IO the device actually has (pydoover
    /// `fetch_io_details`).
    ///
    /// Prefer this over [`fetch_io_table`](Self::fetch_io_table): it also reports
    /// which device owns each channel, plus per-channel metadata and capability
    /// flags.
    ///
    /// Against a platform interface that predates `getIoDetails` this falls back
    /// to `fetch_io_table` and synthesizes a single anonymous master device with
    /// no per-channel metadata. Returns `None` only when that fallback also came
    /// back empty.
    pub async fn fetch_io_details(&self) -> Result<Option<IoDetails>> {
        let resp = self
            .shared
            .call(|ch| async move {
                GenClient::new(ch).get_io_details(pb::GetIoDetailsRequest {}).await
            })
            .await;
        let resp = match resp {
            Ok(resp) => resp,
            // An older sidecar doesn't have the method at all. Everything else
            // — including a failure header on a sidecar that does — propagates.
            Err(DooverError::Status(status)) if status.code() == tonic::Code::Unimplemented => {
                return Ok(self.fetch_io_table().await?.as_ref().map(IoDetails::from_io_table));
            }
            Err(e) => return Err(e),
        };
        Self::check(resp.response_header)?;
        Ok(Some(IoDetails::from_proto(resp.devices)))
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
        // Stream keepalive settings, without which a half-open connection
        // surfaces nothing to the reader and the reconnect loop in
        // `start_di_pulse_listener` can never fire (pydoover
        // `_STREAM_CHANNEL_OPTIONS`).
        let mut client = GenClient::new(self.shared.stream_channel());
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
                                    // Presence, not `> 0`. The stream opens
                                    // with a handshake carrying only `di` and
                                    // no `dt_secs`, which is what this guard is
                                    // for — but the firmware measures dt as the
                                    // gap since the previous edge, so it has
                                    // nothing to measure on the FIRST edge (or
                                    // after a dropped transition) and sends
                                    // 0.0. A `> 0` test swallows the first
                                    // pulse of a burst, and where the pulse
                                    // COUNT is the payload — a vending terminal
                                    // pulsing a product number — that is not a
                                    // lost tick, it is the wrong product sold.
                                    if let Some(dt_secs) = pulse.dt_secs {
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

// ------------------------------------------------------------------
// Process image
// ------------------------------------------------------------------
//
// The whole I/O image in one message: every channel of every device (the
// master, then each slave) in the flat numbering `getIoTable` describes,
// each with its quality. This is the PLC-style way to read I/O: a scan takes
// one consistent image at its start instead of a call per channel, and
// writes its outputs back in one call. `watch_image` keeps the latest image
// from the platform's `subscribeImage` stream, so reading it at the top of a
// scan costs no call and never waits.

/// Why a channel's value isn't plainly good (the platform's
/// `ChannelQuality`). A value this build doesn't know is `Other` and counts as
/// not good, as the proto asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelQuality {
    Good,
    /// The device's data is older than its staleness limit.
    Stale,
    /// The device hasn't answered yet, or has dropped off.
    Offline,
    /// The device answered but gave no value for this channel.
    NoValue,
    /// The hardware reports a fault on this channel.
    Fault,
    /// An analog input below its valid range (e.g. a 4-20 mA wire break).
    UnderRange,
    /// An analog input above its valid range.
    OverRange,
    /// An input or output tripped on overcurrent.
    Overcurrent,
    Other(i32),
}

impl ChannelQuality {
    fn from_i32(v: i32) -> Self {
        match pb::ChannelQuality::try_from(v) {
            Ok(pb::ChannelQuality::Good) => Self::Good,
            Ok(pb::ChannelQuality::Stale) => Self::Stale,
            Ok(pb::ChannelQuality::Offline) => Self::Offline,
            Ok(pb::ChannelQuality::NoValue) => Self::NoValue,
            Ok(pb::ChannelQuality::Fault) => Self::Fault,
            Ok(pb::ChannelQuality::UnderRange) => Self::UnderRange,
            Ok(pb::ChannelQuality::OverRange) => Self::OverRange,
            Ok(pb::ChannelQuality::Overcurrent) => Self::Overcurrent,
            Err(_) => Self::Other(v),
        }
    }

    pub fn is_good(self) -> bool {
        self == Self::Good
    }
}

/// A channel whose quality isn't `Good`, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStatus {
    pub quality: ChannelQuality,
    pub detail: Option<String>,
}

/// One device's part of the image: the master (`index` -1), then each slave.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageDevice {
    pub index: i32,
    pub name: String,
    /// True while the device's data is fresh enough to serve.
    pub online: bool,
    /// Age of the device's latest successful poll when the image was built;
    /// `None` before the first poll.
    pub age_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Wall-clock time of that poll (Unix ms). `age_ms` is the reliable
    /// measure on a device whose clock hasn't synced.
    pub timestamp_ms: Option<u64>,
}

/// The process image (`getImage` / `subscribeImage`). Vectors are indexed by
/// flat channel number and never shift: a channel without a good value still
/// has an entry (false / 0 when there's no value at all) and is listed in the
/// matching status map, which holds only channels that aren't `Good`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoImage {
    pub di: Vec<bool>,
    pub r#do: Vec<bool>,
    pub ai: Vec<f64>,
    pub ao: Vec<f64>,
    pub di_status: HashMap<i32, ChannelStatus>,
    pub do_status: HashMap<i32, ChannelStatus>,
    pub ai_status: HashMap<i32, ChannelStatus>,
    pub ao_status: HashMap<i32, ChannelStatus>,
    /// Pulse totals and rates, for the DIs that have them.
    pub di_count: HashMap<i32, u64>,
    pub di_rate_hz: HashMap<i32, f64>,
    pub devices: Vec<ImageDevice>,
    /// Increases when any value or channel status changes, and only then.
    pub sequence: u64,
}

fn statuses(map: HashMap<i32, pb::ChannelStatus>) -> HashMap<i32, ChannelStatus> {
    map.into_iter()
        .map(|(ch, s)| (ch, ChannelStatus { quality: ChannelQuality::from_i32(s.quality), detail: s.detail }))
        .collect()
}

/// A channel's quality: the status map's, `Good` when it isn't listed, and
/// `NoValue` for a channel the image doesn't have.
fn quality_of(len: usize, status: &HashMap<i32, ChannelStatus>, ch: i32) -> ChannelQuality {
    if ch < 0 || ch as usize >= len {
        return ChannelQuality::NoValue;
    }
    status.get(&ch).map_or(ChannelQuality::Good, |s| s.quality)
}

impl IoImage {
    fn from_pb(resp: pb::GetImageResponse) -> Self {
        Self {
            di: resp.di,
            r#do: resp.r#do,
            ai: resp.ai,
            ao: resp.ao,
            di_status: statuses(resp.di_status),
            do_status: statuses(resp.do_status),
            ai_status: statuses(resp.ai_status),
            ao_status: statuses(resp.ao_status),
            di_count: resp.di_count,
            di_rate_hz: resp.di_rate_hz,
            devices: resp
                .devices
                .into_iter()
                .map(|d| ImageDevice {
                    index: d.index,
                    name: d.name,
                    online: d.online,
                    age_ms: d.age_ms,
                    last_error: d.last_error,
                    timestamp_ms: d.timestamp_ms,
                })
                .collect(),
            sequence: resp.sequence,
        }
    }

    pub fn ai_quality(&self, ch: i32) -> ChannelQuality {
        quality_of(self.ai.len(), &self.ai_status, ch)
    }

    pub fn di_quality(&self, ch: i32) -> ChannelQuality {
        quality_of(self.di.len(), &self.di_status, ch)
    }

    pub fn do_quality(&self, ch: i32) -> ChannelQuality {
        quality_of(self.r#do.len(), &self.do_status, ch)
    }

    pub fn ao_quality(&self, ch: i32) -> ChannelQuality {
        quality_of(self.ao.len(), &self.ao_status, ch)
    }

    /// An analog input's value, whatever its quality (`None` only for a
    /// channel the image doesn't have). Check [`ai_quality`](Self::ai_quality),
    /// or use [`good_ai`](Self::good_ai).
    pub fn ai(&self, ch: i32) -> Option<f64> {
        usize::try_from(ch).ok().and_then(|i| self.ai.get(i).copied())
    }

    /// An analog input's value only when its quality is `Good`.
    pub fn good_ai(&self, ch: i32) -> Option<f64> {
        self.ai_quality(ch).is_good().then(|| self.ai(ch)).flatten()
    }

    pub fn di(&self, ch: i32) -> Option<bool> {
        usize::try_from(ch).ok().and_then(|i| self.di.get(i).copied())
    }

    /// A digital input's level only when its quality is `Good`.
    pub fn good_di(&self, ch: i32) -> Option<bool> {
        self.di_quality(ch).is_good().then(|| self.di(ch)).flatten()
    }

    pub fn r#do(&self, ch: i32) -> Option<bool> {
        usize::try_from(ch).ok().and_then(|i| self.r#do.get(i).copied())
    }

    pub fn ao(&self, ch: i32) -> Option<f64> {
        usize::try_from(ch).ok().and_then(|i| self.ao.get(i).copied())
    }
}

/// The outputs [`set_image`](PlatformClient::set_image) couldn't write; the
/// others were written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageWriteResult {
    pub failed_do: Vec<i32>,
    pub failed_ao: Vec<i32>,
}

impl ImageWriteResult {
    pub fn all_written(&self) -> bool {
        self.failed_do.is_empty() && self.failed_ao.is_empty()
    }
}

/// The latest image from a [`watch_image`](PlatformClient::watch_image), and
/// when it arrived here.
#[derive(Debug, Clone)]
pub struct LatestImage {
    pub image: Arc<IoImage>,
    pub received: Instant,
}

impl LatestImage {
    /// How long ago this image arrived.
    pub fn age(&self) -> Duration {
        self.received.elapsed()
    }
}

/// The latest process image, kept by a background task that follows the
/// platform's `subscribeImage` stream and reconnects when it drops. Reading
/// it never makes a call or waits: a scan takes [`latest`](Self::latest) at
/// its start and checks [`LatestImage::age`] against its own staleness
/// limit. Dropping the watch (or [`PlatformClient::close`]) stops the task.
pub struct ImageWatch {
    rx: watch::Receiver<Option<LatestImage>>,
    task: tokio::task::AbortHandle,
}

impl ImageWatch {
    /// The latest image, or `None` before the first one arrives.
    pub fn latest(&self) -> Option<LatestImage> {
        self.rx.borrow().clone()
    }

    /// Wait for the next image (for event-driven code; a scan doesn't need
    /// it). Errors only when the watch has stopped.
    pub async fn changed(&mut self) -> Result<LatestImage> {
        loop {
            self.rx
                .changed()
                .await
                .map_err(|_| DooverError::Other("image watch stopped".into()))?;
            if let Some(latest) = self.rx.borrow_and_update().clone() {
                return Ok(latest);
            }
        }
    }

    /// Stop following the stream; [`latest`](Self::latest) keeps the last image.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for ImageWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How long the image watch waits before reconnecting a dropped stream.
const IMAGE_WATCH_RETRY: Duration = Duration::from_secs(1);

/// The image watch's loop, apart from the gRPC client so it can be tested
/// with any stream: connect, publish every image, reconnect after
/// `retry` ([`IMAGE_WATCH_RETRY`]) when the stream fails or ends.
async fn run_image_watch<C, Fut, S>(
    mut connect: C,
    tx: watch::Sender<Option<LatestImage>>,
    retry: Duration,
)
where
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<S>>,
    S: Stream<Item = Result<IoImage>> + Unpin,
{
    loop {
        match connect().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(image) => {
                            tx.send_replace(Some(LatestImage {
                                image: Arc::new(image),
                                received: Instant::now(),
                            }));
                        }
                        Err(e) => {
                            tracing::warn!("platform image stream failed: {e}; reconnecting");
                            break;
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("platform image stream unavailable: {e}; retrying"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(retry).await;
    }
}

impl PlatformClient {
    /// The whole process image in one call (`getImage`), served from the
    /// platform's last poll of each device: it never waits on hardware.
    pub async fn fetch_image(&self) -> Result<IoImage> {
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).get_image(pb::GetImageRequest {}).await })
            .await?;
        Self::check(resp.response_header.clone())?;
        Ok(IoImage::from_pb(resp))
    }

    /// Write any number of DOs and AOs in one call (`setImage`). The result
    /// lists the channels whose write failed; the others were written.
    pub async fn set_image(&self, dos: &[(i32, bool)], aos: &[(i32, f64)]) -> Result<ImageWriteResult> {
        let req = pb::SetImageRequest {
            r#do: dos.iter().map(|(ch, _)| *ch).collect(),
            do_value: dos.iter().map(|(_, v)| *v).collect(),
            ao: aos.iter().map(|(ch, _)| *ch).collect(),
            ao_value: aos.iter().map(|(_, v)| *v).collect(),
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).set_image(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(ImageWriteResult { failed_do: resp.failed_do, failed_ao: resp.failed_ao })
    }

    /// The image whenever it changes (`subscribeImage`), at most `max_rate_hz`
    /// times a second (`None`: every change), on a stream channel of its own
    /// with keepalives. The stream ends when the platform closes it or the
    /// connection drops; [`watch_image`](Self::watch_image) reconnects.
    pub async fn subscribe_image(
        &self,
        max_rate_hz: Option<f32>,
    ) -> Result<impl Stream<Item = Result<IoImage>> + Unpin> {
        let req = pb::SubscribeImageRequest { max_rate_hz };
        let mut client = GenClient::new(self.shared.stream_channel());
        let stream = client.subscribe_image(req).await?.into_inner();
        Ok(stream.map(|item| {
            let resp = item?;
            Self::check(resp.response_header.clone())?;
            Ok(IoImage::from_pb(resp))
        }))
    }

    /// Follow the image in the background (see [`ImageWatch`]). The task is
    /// also stopped by [`close`](Self::close).
    pub fn watch_image(&self, max_rate_hz: Option<f32>) -> ImageWatch {
        let (tx, rx) = watch::channel(None);
        let client = self.clone();
        let task = tokio::spawn(run_image_watch(
            move || {
                let client = client.clone();
                async move { client.subscribe_image(max_rate_hz).await }
            },
            tx,
            IMAGE_WATCH_RETRY,
        ));
        let abort = task.abort_handle();
        self.listeners.lock().expect("listener lock poisoned").push(task);
        ImageWatch { rx, task: abort }
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

#[cfg(test)]
mod image_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response() -> pb::GetImageResponse {
        let mut ai_status = HashMap::new();
        ai_status.insert(1, pb::ChannelStatus { quality: pb::ChannelQuality::UnderRange as i32, detail: Some("1.2 mA".into()) });
        ai_status.insert(2, pb::ChannelStatus { quality: 99, detail: None });
        pb::GetImageResponse {
            response_header: None,
            di: vec![true, false],
            r#do: vec![false; 10],
            ai: vec![15.5, 1.2, 0.0],
            ao: vec![0.0, 0.0],
            ai_status,
            di_count: HashMap::from([(0, 42)]),
            devices: vec![pb::DeviceImageStatus {
                index: -1,
                name: "doovit2".into(),
                online: true,
                age_ms: Some(12),
                last_error: None,
                timestamp_ms: None,
            }],
            sequence: 7,
            ..Default::default()
        }
    }

    #[test]
    fn image_from_response() {
        let img = IoImage::from_pb(response());
        assert_eq!(img.sequence, 7);
        assert_eq!(img.r#do.len(), 10);
        assert_eq!(img.di_count.get(&0), Some(&42));
        assert_eq!(img.devices[0].index, -1);
        assert_eq!(img.devices[0].age_ms, Some(12));
        assert_eq!(img.ai_status[&1].detail.as_deref(), Some("1.2 mA"));
    }

    #[test]
    fn qualities_and_values() {
        let img = IoImage::from_pb(response());
        assert_eq!(img.ai_quality(0), ChannelQuality::Good);
        assert_eq!(img.good_ai(0), Some(15.5));
        // Not good: the value is still there, good_ai withholds it.
        assert_eq!(img.ai_quality(1), ChannelQuality::UnderRange);
        assert_eq!(img.ai(1), Some(1.2));
        assert_eq!(img.good_ai(1), None);
        // A quality this build doesn't know counts as not good.
        assert_eq!(img.ai_quality(2), ChannelQuality::Other(99));
        assert_eq!(img.good_ai(2), None);
        // Channels the image doesn't have.
        assert_eq!(img.ai(3), None);
        assert_eq!(img.ai(-1), None);
        assert_eq!(img.ai_quality(3), ChannelQuality::NoValue);
        assert_eq!(img.good_di(0), Some(true));
        assert_eq!(img.do_quality(9), ChannelQuality::Good);
        assert_eq!(img.do_quality(10), ChannelQuality::NoValue);
    }

    #[test]
    fn write_result() {
        assert!(ImageWriteResult::default().all_written());
        assert!(!ImageWriteResult { failed_do: vec![3], failed_ao: vec![] }.all_written());
    }

    const RETRY: Duration = Duration::from_millis(10);

    fn image(sequence: u64) -> Result<IoImage> {
        Ok(IoImage { sequence, ..Default::default() })
    }

    #[tokio::test]
    async fn watch_publishes_and_reconnects() {
        let connects = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = watch::channel(None);
        let c = Arc::clone(&connects);
        let task = tokio::spawn(run_image_watch(
            move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move {
                    // First stream: two images then an error; second: one image, then ends.
                    let items = if n == 0 {
                        vec![image(1), image(2), Err(DooverError::Other("dropped".into()))]
                    } else {
                        vec![image(3)]
                    };
                    Ok(futures_util::stream::iter(items))
                }
            },
            tx,
            RETRY,
        ));
        let mut seen = Vec::new();
        while seen.last() != Some(&3) {
            rx.changed().await.unwrap();
            if let Some(latest) = rx.borrow_and_update().clone() {
                seen.push(latest.image.sequence);
            }
        }
        // Latest wins: a watch may skip images a slow reader missed, but
        // never goes backwards, and the image after the reconnect arrives.
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "out of order: {seen:?}");
        assert!(connects.load(Ordering::SeqCst) >= 2);
        // Dropping the receiver lets the loop finish at its next retry.
        drop(rx);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn watch_retries_when_unavailable() {
        let connects = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = watch::channel(None);
        let c = Arc::clone(&connects);
        let _task = tokio::spawn(run_image_watch(
            move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(DooverError::Other("connection refused".into()))
                    } else {
                        Ok(futures_util::stream::iter(vec![image(5)]))
                    }
                }
            },
            tx,
            RETRY,
        ));
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().as_ref().map(|l| l.image.sequence), Some(5));
        assert_eq!(connects.load(Ordering::SeqCst), 3);
    }
}
