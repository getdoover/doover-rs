//! `doover modbus <cmd>` — mirrors the `@cli_command`-decorated surface of
//! pydoover's `ModbusInterface` (`docker/modbus/modbus_iface.py`).

use clap::Subcommand;
use serde_json::{json, Value};

use doover::docker::modbus::{
    BusSettings, ModbusClient, RegisterRange, SerialBusSettings, TcpBusSettings,
};
use doover::proto::modbus_iface as pb;

use crate::parse::{parse_int_list, IntList};
use crate::{normalize_uri, print_json, CliResult};

#[derive(Subcommand, Debug)]
pub enum ModbusCmd {
    /// Liveness echo against the modbus interface.
    #[command(name = "test_comms", alias = "test-comms")]
    TestComms {
        /// Message for the sidecar to echo back.
        #[arg(long, default_value = "Comms Check Message")]
        message: String,
    },

    /// Open a modbus bus (legacy: buses now open on demand per read/write).
    #[command(name = "open_bus", alias = "open-bus")]
    OpenBus {
        /// Bus type: 'serial' or 'tcp'.
        #[arg(long = "bus_type", alias = "bus-type", default_value = "serial")]
        bus_type: String,
        /// Bus id to open the bus as.
        #[arg(long, default_value = "default")]
        name: String,
        /// Serial device path.
        #[arg(long = "serial_port", alias = "serial-port", default_value = "/dev/ttyS0")]
        serial_port: String,
        /// Serial baud rate.
        #[arg(long = "serial_baud", alias = "serial-baud", default_value_t = 9600)]
        serial_baud: i32,
        /// Modbus framing: 'rtu' or 'ascii'.
        #[arg(long = "serial_method", alias = "serial-method", default_value = "rtu")]
        serial_method: String,
        /// Serial data bits.
        #[arg(long = "serial_bits", alias = "serial-bits", default_value_t = 8)]
        serial_bits: i32,
        /// Serial parity: 'N', 'E' or 'O'.
        #[arg(long = "serial_parity", alias = "serial-parity", default_value = "N")]
        serial_parity: String,
        /// Serial stop bits.
        #[arg(long = "serial_stop", alias = "serial-stop", default_value_t = 1)]
        serial_stop: i32,
        /// Serial timeout in seconds.
        #[arg(long = "serial_timeout", alias = "serial-timeout", default_value_t = 0.3)]
        serial_timeout: f32,
        /// Modbus/TCP target as 'ip:port'.
        #[arg(long = "tcp_uri", alias = "tcp-uri", default_value = "127.0.0.1:5000")]
        tcp_uri: String,
        /// TCP timeout in seconds.
        #[arg(long = "tcp_timeout", alias = "tcp-timeout", default_value_t = 2.0)]
        tcp_timeout: f32,
    },

    /// Close a modbus bus (legacy: buses are pooled by the sidecar).
    #[command(name = "close_bus", alias = "close-bus")]
    CloseBus {
        /// Bus id to close.
        #[arg(long = "bus_id", alias = "bus-id", default_value = "default")]
        bus_id: String,
    },

    /// Whether a modbus bus is open.
    #[command(name = "fetch_bus_status", alias = "fetch-bus-status")]
    FetchBusStatus {
        /// Bus id to query.
        #[arg(long = "bus_id", alias = "bus-id", default_value = "default")]
        bus_id: String,
    },

    /// List the sidecar's buses and their settings.
    #[command(name = "list_buses", aliases = ["list-buses", "list_bus", "list-bus"])]
    ListBuses,

    /// Read a range of registers from a modbus device.
    #[command(name = "read_registers", alias = "read-registers")]
    ReadRegisters {
        /// The modbus id of the target device.
        #[arg(long = "modbus_id", alias = "modbus-id", default_value_t = 1)]
        modbus_id: i32,
        /// The starting register address.
        #[arg(long = "start_address", alias = "start-address", default_value_t = 0)]
        start_address: i32,
        /// How many registers to read.
        #[arg(long = "num_registers", alias = "num-registers", default_value_t = 1)]
        num_registers: i32,
        /// Register type (default 4: holding registers).
        #[arg(long = "register_type", alias = "register-type", default_value_t = 4)]
        register_type: i32,
        /// Sidecar retries on failure (0 fails fast; default: sidecar's).
        #[arg(long)]
        retries: Option<i32>,
    },

    /// Write values to registers starting at an address.
    #[command(name = "write_registers", alias = "write-registers")]
    WriteRegisters {
        /// Value(s) to write: '5' or '[1,2,3]'.
        #[arg(value_parser = parse_int_list)]
        values: IntList,
        /// The modbus id of the target device.
        #[arg(long = "modbus_id", alias = "modbus-id", default_value_t = 1)]
        modbus_id: i32,
        /// The starting register address.
        #[arg(long = "start_address", alias = "start-address", default_value_t = 0)]
        start_address: i32,
        /// Register type (default 4: holding registers).
        #[arg(long = "register_type", alias = "register-type", default_value_t = 4)]
        register_type: i32,
        /// Sidecar retries on failure (0 fails fast; default: sidecar's).
        #[arg(long)]
        retries: Option<i32>,
    },
}

fn serial_json(s: &pb::SerialBusSettings) -> Value {
    json!({
        "port": s.port,
        "baud": s.baud,
        "modbus_method": s.modbus_method,
        "data_bits": s.data_bits,
        "parity": s.parity,
        "stop_bits": s.stop_bits,
        "timeout": s.timeout,
    })
}

fn ethernet_json(e: &pb::EthernetBusSettings) -> Value {
    json!({
        "ip": e.ip,
        "port": e.port,
        "timeout": e.timeout,
    })
}

fn bus_status_json(b: &pb::BusStatus) -> Value {
    json!({
        "bus_id": b.bus_id,
        "open": b.open,
        "serial_settings": b.serial_settings.as_ref().map(serial_json),
        "ethernet_settings": b.ethernet_settings.as_ref().map(ethernet_json),
    })
}

pub async fn run(uri: &str, cmd: ModbusCmd) -> CliResult {
    let client = ModbusClient::connect(normalize_uri(uri)).await?;

    match cmd {
        ModbusCmd::TestComms { message } => {
            let resp = client.test_comms(message).await?;
            print_json(&json!(resp));
        }
        ModbusCmd::OpenBus {
            bus_type,
            name,
            serial_port,
            serial_baud,
            serial_method,
            serial_bits,
            serial_parity,
            serial_stop,
            serial_timeout,
            tcp_uri,
            tcp_timeout,
        } => {
            let settings = match bus_type.as_str() {
                "serial" => BusSettings::Serial(SerialBusSettings {
                    port: serial_port,
                    baud: serial_baud,
                    method: serial_method,
                    data_bits: serial_bits,
                    parity: serial_parity,
                    stop_bits: serial_stop,
                    timeout: serial_timeout,
                }),
                "tcp" => {
                    let (ip, port) = tcp_uri
                        .rsplit_once(':')
                        .ok_or_else(|| format!("tcp_uri {tcp_uri:?} is not 'ip:port'"))?;
                    BusSettings::Tcp(TcpBusSettings {
                        ip: ip.to_string(),
                        port: port
                            .parse()
                            .map_err(|_| format!("tcp_uri port {port:?} is not a number"))?,
                        timeout: tcp_timeout,
                    })
                }
                other => return Err(format!("unknown bus_type {other:?} (serial or tcp)").into()),
            };
            #[allow(deprecated)]
            let ok = client.open_bus(&name, &settings).await?;
            print_json(&json!(ok));
        }
        ModbusCmd::CloseBus { bus_id } => {
            #[allow(deprecated)]
            let ok = client.close_bus(&bus_id).await?;
            print_json(&json!(ok));
        }
        ModbusCmd::FetchBusStatus { bus_id } => {
            print_json(&json!(client.fetch_bus_status(&bus_id).await?));
        }
        ModbusCmd::ListBuses => {
            let buses = client.list_buses().await?;
            print_json(&Value::Array(buses.iter().map(bus_status_json).collect()));
        }
        ModbusCmd::ReadRegisters {
            modbus_id,
            start_address,
            num_registers,
            register_type,
            retries,
        } => {
            let range = RegisterRange {
                modbus_id,
                register_type,
                start_address,
                num_registers,
                bus: None,
                retries,
            };
            let values = client.read_registers(&range).await?;
            // pydoover returns the scalar when a single register was read.
            if num_registers == 1 && values.len() == 1 {
                print_json(&json!(values[0]));
            } else {
                print_json(&json!(values));
            }
        }
        ModbusCmd::WriteRegisters {
            values,
            modbus_id,
            start_address,
            register_type,
            retries,
        } => {
            let range = RegisterRange {
                modbus_id,
                register_type,
                start_address,
                num_registers: values.0.len() as i32,
                bus: None,
                retries,
            };
            client.write_registers(&range, &values.0).await?;
            print_json(&json!(true));
        }
    }
    Ok(())
}
