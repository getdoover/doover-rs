//! `ModbusClient` — a client for the modbus-interface sidecar
//! (`modbus_iface.modbusIface`, default `127.0.0.1:50054`), the Rust
//! equivalent of pydoover's `ModbusInterface` (`docker/modbus/modbus_iface.py`).
//!
//! Buses open on demand: each read/write carries its bus's connection
//! settings ([`BusSettings`]), so nothing needs pre-opening (the legacy
//! `openBus` flow is kept only for old sidecars). Where pydoover hands failed
//! responses back to the caller as `None`, this client surfaces them as
//! typed [`DooverError`]s.
//!
//! Unary calls share one persistent channel with per-call deadlines and a
//! rebuild-and-retry-once path for `UNAVAILABLE` (see [`crate::docker::grpc`]).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::task::{AbortHandle, JoinHandle};

use doover_proto::modbus_iface as pb;
use pb::modbus_iface_client::ModbusIfaceClient as GenClient;

use crate::docker::grpc::{check_response_header, SharedChannel};
use crate::error::{DooverError, Result};

/// Health-check service name (pydoover `ModbusInterface.service_name`).
const SERVICE_NAME: &str = "doover.ModbusInterface";

/// Serial (RTU/ASCII) bus connection settings. Defaults mirror pydoover's
/// `open_bus`/`_get_bus_request` parameter defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct SerialBusSettings {
    /// Serial device, e.g. `/dev/ttyS0`.
    pub port: String,
    pub baud: i32,
    /// `"rtu"` or `"ascii"`.
    pub method: String,
    pub data_bits: i32,
    /// `"N"`, `"E"` or `"O"`.
    pub parity: String,
    pub stop_bits: i32,
    /// Serial timeout in seconds.
    pub timeout: f32,
}

impl Default for SerialBusSettings {
    fn default() -> Self {
        Self {
            port: "/dev/ttyS0".to_string(),
            baud: 9600,
            method: "rtu".to_string(),
            data_bits: 8,
            parity: "N".to_string(),
            stop_bits: 1,
            timeout: 0.3,
        }
    }
}

/// Modbus/TCP bus connection settings. Defaults mirror pydoover's
/// `tcp_uri="127.0.0.1:5000"`, `tcp_timeout=2`.
#[derive(Debug, Clone, PartialEq)]
pub struct TcpBusSettings {
    pub ip: String,
    pub port: i32,
    /// TCP timeout in seconds.
    pub timeout: f32,
}

impl Default for TcpBusSettings {
    fn default() -> Self {
        Self { ip: "127.0.0.1".to_string(), port: 5000, timeout: 2.0 }
    }
}

/// The connection settings a request carries so its bus can be opened on
/// demand (pydoover `_resolve_bus_settings` / the proto `bus_settings`
/// oneof).
#[derive(Debug, Clone, PartialEq)]
pub enum BusSettings {
    Serial(SerialBusSettings),
    Tcp(TcpBusSettings),
}

impl BusSettings {
    fn to_serial_proto(s: &SerialBusSettings) -> pb::SerialBusSettings {
        pb::SerialBusSettings {
            port: s.port.clone(),
            baud: s.baud,
            modbus_method: Some(s.method.clone()),
            data_bits: Some(s.data_bits),
            parity: Some(s.parity.clone()),
            stop_bits: Some(s.stop_bits),
            timeout: Some(s.timeout),
        }
    }

    fn to_tcp_proto(t: &TcpBusSettings) -> pb::EthernetBusSettings {
        pb::EthernetBusSettings {
            ip: t.ip.clone(),
            port: t.port,
            modbus_method: None,
            timeout: Some(t.timeout),
        }
    }
}

/// Build the per-request-type `bus_settings` oneof from a [`BusSettings`].
/// (Each proto request has its own oneof enum, hence the macro.)
macro_rules! bus_settings_oneof {
    ($mod:ident, $bus:expr) => {
        $bus.map(|b| match b {
            BusSettings::Serial(s) => {
                pb::$mod::BusSettings::SerialSettings(BusSettings::to_serial_proto(s))
            }
            BusSettings::Tcp(t) => {
                pb::$mod::BusSettings::EthernetSettings(BusSettings::to_tcp_proto(t))
            }
        })
    };
}

/// Parameters for a register read (pydoover `read_registers` /
/// `add_read_register_subscription` keyword arguments). Defaults mirror
/// pydoover: device 1, register type 4 (holding registers), address 0,
/// one register.
#[derive(Debug, Clone)]
pub struct RegisterRange {
    /// The modbus ID of the target device.
    pub modbus_id: i32,
    /// Register type (default 4 — typically holding registers).
    pub register_type: i32,
    /// The starting register address.
    pub start_address: i32,
    /// How many registers to read.
    pub num_registers: i32,
    /// Connection settings for the bus; `None` lets the sidecar use its
    /// default. Pass this to select a bus when several are configured.
    pub bus: Option<BusSettings>,
    /// How many times the sidecar retries on failure. `Some(0)` fails fast
    /// (no retry) — useful when a failure is expected/normal. `None` applies
    /// the sidecar's default.
    pub retries: Option<i32>,
}

impl Default for RegisterRange {
    fn default() -> Self {
        Self {
            modbus_id: 1,
            register_type: 4,
            start_address: 0,
            num_registers: 1,
            bus: None,
            retries: None,
        }
    }
}

/// Synchronous read-subscription callback (pydoover
/// `ReadRegisterSubscriptionCallback`): `Some(values)` for a successful
/// poll, `None` when the sidecar reports a failed read. Must be cheap —
/// spawn a task for real work.
pub type ReadRegisterCallback = Arc<dyn Fn(Option<&[i32]>) + Send + Sync>;

/// Combine two 16-bit words into one 32-bit value: `word1 + word2 * 65536`
/// (pydoover `two_words_to_32bit_float` — the name is pydoover's; despite
/// it, the result is the raw 32-bit integer, commonly reinterpreted by the
/// caller). `swap` exchanges the words first.
pub fn two_words_to_32bit_float(word1: u16, word2: u16, swap: bool) -> u32 {
    let (word1, word2) = if swap { (word2, word1) } else { (word1, word2) };
    word1 as u32 + (word2 as u32) * 65536
}

#[derive(Clone)]
pub struct ModbusClient {
    shared: Arc<SharedChannel>,
    /// Running read-register subscription tasks (pydoover
    /// `subscription_tasks`), aborted by [`close`](Self::close).
    subscriptions: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ModbusClient {
    /// Connect to the modbus sidecar (default `http://127.0.0.1:50054`).
    /// The channel is built lazily, as in pydoover — use
    /// [`wait_until_healthy`](Self::wait_until_healthy) to gate on the
    /// sidecar actually serving.
    pub async fn connect(uri: impl Into<String>) -> Result<Self> {
        Ok(Self {
            shared: Arc::new(SharedChannel::new(uri, SERVICE_NAME)?),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
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

    /// Cancel all running read-register subscriptions (pydoover `close`).
    pub fn close(&self) {
        let mut tasks = self.subscriptions.lock().expect("subscription lock poisoned");
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    /// Map a modbus `responseHeader` to a typed error. (pydoover's
    /// `ModbusInterface.process_response` instead hands failed responses
    /// back as `None`; here failure is a typed `Err`.)
    fn check(header: Option<pb::ResponseHeader>) -> Result<()> {
        match header {
            Some(h) => check_response_header(h.success, h.response_code, h.response_message),
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
    // Bus management
    // ------------------------------------------------------------------

    /// Open a modbus bus; returns whether the sidecar reports success
    /// (pydoover `open_bus`).
    #[deprecated(
        note = "buses open on demand: pass BusSettings to read_registers/write_registers instead"
    )]
    pub async fn open_bus(&self, bus_id: &str, settings: &BusSettings) -> Result<bool> {
        let req = pb::OpenBusRequest {
            bus_id: bus_id.to_string(),
            bus_settings: bus_settings_oneof!(open_bus_request, Some(settings)),
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).open_bus(req).await }
            })
            .await?;
        Ok(resp.response_header.map(|h| h.success).unwrap_or(false))
    }

    /// Close a modbus bus; returns whether the sidecar reports success
    /// (pydoover `close_bus`).
    #[deprecated(note = "buses are pooled by the modbus sidecar and need no explicit closing")]
    pub async fn close_bus(&self, bus_id: &str) -> Result<bool> {
        let req = pb::CloseBusRequest { bus_id: bus_id.to_string() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).close_bus(req).await }
            })
            .await?;
        Ok(resp.response_header.map(|h| h.success).unwrap_or(false))
    }

    /// Whether a modbus bus is open (pydoover `fetch_bus_status`).
    pub async fn fetch_bus_status(&self, bus_id: &str) -> Result<bool> {
        let req = pb::BusStatusRequest { bus_id: bus_id.to_string() };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).bus_status(req).await }
            })
            .await?;
        // pydoover: response_header.success and bus_status.open — a failed
        // header is a `False` status, not an error.
        let ok = resp.response_header.map(|h| h.success).unwrap_or(false);
        Ok(ok && resp.bus_status.map(|b| b.open).unwrap_or(false))
    }

    /// List the sidecar's buses, as the raw proto statuses (`doover::proto`).
    /// (`listBus` RPC; pydoover has no wrapper for it.)
    pub async fn list_buses(&self) -> Result<Vec<pb::BusStatus>> {
        let resp = self
            .shared
            .call(|ch| async move { GenClient::new(ch).list_bus(pb::ListBusRequest {}).await })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.bus_status)
    }

    // ------------------------------------------------------------------
    // Register IO
    // ------------------------------------------------------------------

    /// Read a range of registers (pydoover `read_registers`). Where pydoover
    /// returns `None` on a failed read, this returns a typed error.
    pub async fn read_registers(&self, range: &RegisterRange) -> Result<Vec<i32>> {
        let req = pb::ReadRegisterRequest {
            bus_id: String::new(),
            modbus_id: range.modbus_id,
            register_type: range.register_type,
            address: range.start_address,
            count: range.num_registers,
            bus_settings: bus_settings_oneof!(read_register_request, range.bus.as_ref()),
            retries: range.retries,
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).read_registers(req).await }
            })
            .await?;
        Self::check(resp.response_header)?;
        Ok(resp.values)
    }

    /// Read a single register (pydoover `read_registers` with
    /// `num_registers=1`, which returns the scalar).
    pub async fn read_register(&self, range: &RegisterRange) -> Result<i32> {
        let range = RegisterRange { num_registers: 1, ..range.clone() };
        self.read_registers(&range).await?.into_iter().next().ok_or_else(|| {
            DooverError::Other("read_register: sidecar returned no values".to_string())
        })
    }

    /// Write `values` to registers starting at `range.start_address`
    /// (pydoover `write_registers`; `range.num_registers` is ignored — the
    /// value count determines how many registers are written). Where
    /// pydoover returns `False` on failure, this returns a typed error.
    pub async fn write_registers(&self, range: &RegisterRange, values: &[i32]) -> Result<()> {
        let req = pb::WriteRegisterRequest {
            bus_id: String::new(),
            modbus_id: range.modbus_id,
            register_type: range.register_type,
            address: range.start_address,
            values: values.to_vec(),
            bus_settings: bus_settings_oneof!(write_register_request, range.bus.as_ref()),
            retries: range.retries,
        };
        let resp = self
            .shared
            .call(|ch| {
                let req = req.clone();
                async move { GenClient::new(ch).write_registers(req).await }
            })
            .await?;
        Self::check(resp.response_header)
    }

    // ------------------------------------------------------------------
    // Read subscriptions
    // ------------------------------------------------------------------

    /// Spawn a background task that has the sidecar poll a register range
    /// every `poll_secs` seconds and invokes `callback` with each result
    /// (pydoover `add_read_register_subscription` /
    /// `run_read_register_subscription_task`): `Some(values)` on success,
    /// `None` for a failed read.
    ///
    /// The subscription re-registers with the sidecar whenever the stream
    /// ends or errors (sidecar restart, connection drop), so callbacks keep
    /// flowing after a reconnect. The task is cancelled by
    /// [`close`](Self::close); the returned handle can abort it
    /// individually.
    pub fn add_read_register_subscription(
        &self,
        range: &RegisterRange,
        poll_secs: i32,
        callback: ReadRegisterCallback,
    ) -> AbortHandle {
        let req = pb::ReadRegisterSubscriptionRequest {
            bus_id: String::new(),
            modbus_id: range.modbus_id,
            register_type: range.register_type,
            address: range.start_address,
            count: range.num_registers,
            poll_secs,
            bus_settings: bus_settings_oneof!(
                read_register_subscription_request,
                range.bus.as_ref()
            ),
        };
        let shared = Arc::clone(&self.shared);
        let modbus_id = range.modbus_id;
        let task = tokio::spawn(async move {
            loop {
                // A stream-lifetime channel of its own, mirroring pydoover's
                // per-subscription grpc.aio.insecure_channel.
                let mut client = GenClient::new(shared.fresh_channel());
                match client.read_register_subscription(req.clone()).await {
                    Ok(resp) => {
                        let mut stream = resp.into_inner();
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(r) => {
                                    let success = r
                                        .response_header
                                        .as_ref()
                                        .map(|h| h.success)
                                        .unwrap_or(false);
                                    tracing::debug!(
                                        "received modbus subscription result for \
                                         modbus_id {modbus_id}, result={success}"
                                    );
                                    if success {
                                        callback(Some(&r.values));
                                    } else {
                                        callback(None);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "error in read register subscription task: {e}"
                                    );
                                    break;
                                }
                            }
                        }
                        tracing::info!(
                            "read register subscription for modbus_id {modbus_id} ended; \
                             re-registering"
                        );
                    }
                    Err(e) => {
                        tracing::error!("error registering read register subscription: {e}");
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let abort = task.abort_handle();
        self.subscriptions.lock().expect("subscription lock poisoned").push(task);
        abort
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_words_combine_low_high() {
        // word1 is the low word, word2 the high word.
        assert_eq!(two_words_to_32bit_float(0x1234, 0x5678, false), 0x5678_1234);
        assert_eq!(two_words_to_32bit_float(0x1234, 0x5678, true), 0x1234_5678);
        assert_eq!(two_words_to_32bit_float(0, 0, false), 0);
        // Matches pydoover: word1 + word2 * 65536.
        assert_eq!(two_words_to_32bit_float(1, 1, false), 65537);
        assert_eq!(two_words_to_32bit_float(0xFFFF, 0xFFFF, false), u32::MAX);
    }

    #[test]
    fn bus_settings_defaults_match_pydoover() {
        let s = SerialBusSettings::default();
        assert_eq!(
            (s.port.as_str(), s.baud, s.method.as_str(), s.data_bits),
            ("/dev/ttyS0", 9600, "rtu", 8)
        );
        assert_eq!((s.parity.as_str(), s.stop_bits, s.timeout), ("N", 1, 0.3));

        let t = TcpBusSettings::default();
        assert_eq!((t.ip.as_str(), t.port, t.timeout), ("127.0.0.1", 5000, 2.0));
    }

    #[test]
    fn register_range_defaults_match_pydoover() {
        let r = RegisterRange::default();
        assert_eq!(
            (r.modbus_id, r.register_type, r.start_address, r.num_registers),
            (1, 4, 0, 1)
        );
        assert!(r.bus.is_none() && r.retries.is_none());
    }

    #[test]
    fn bus_settings_map_to_proto_oneof() {
        let serial = BusSettings::Serial(SerialBusSettings::default());
        match bus_settings_oneof!(read_register_request, Some(&serial)) {
            Some(pb::read_register_request::BusSettings::SerialSettings(s)) => {
                assert_eq!(s.port, "/dev/ttyS0");
                assert_eq!(s.modbus_method.as_deref(), Some("rtu"));
            }
            other => panic!("expected serial settings, got {other:?}"),
        }

        let tcp = BusSettings::Tcp(TcpBusSettings { ip: "10.0.0.5".into(), ..Default::default() });
        match bus_settings_oneof!(open_bus_request, Some(&tcp)) {
            Some(pb::open_bus_request::BusSettings::EthernetSettings(e)) => {
                assert_eq!(e.ip, "10.0.0.5");
                assert_eq!(e.port, 5000);
            }
            other => panic!("expected ethernet settings, got {other:?}"),
        }

        assert!(bus_settings_oneof!(write_register_request, None::<&BusSettings>).is_none());
    }
}
