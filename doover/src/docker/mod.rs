//! The device-application runtime — the Rust port of `pydoover.docker`.
//!
//! Apps in a container talk to local sidecars over gRPC: the **device agent**
//! (DDA, port 50051) which syncs channels with the cloud, the **platform
//! interface** (port 50053) for hardware I/O, and the **modbus interface**
//! (port 50054).

pub mod application;
pub mod device_agent;
pub mod grpc;
pub mod healthcheck;
pub mod modbus;
pub mod platform;
pub mod subscriptions;

pub use application::{
    channels, run, run_with, wire_rpc, write_export, AppContext, Application, RunOptions,
};
pub use device_agent::{validate_payload, AggregateOptions, DdaStatus, DeviceAgentClient};
pub use healthcheck::HealthState;
pub use modbus::{BusSettings, ModbusClient, RegisterRange, SerialBusSettings, TcpBusSettings};
pub use platform::{
    DiConfigUpdate, DiPulse, Edge, Location, PlatformClient, PlatformEvent, PulseCounter,
    PulseCounterUpdate,
};
pub use subscriptions::SubscriptionHub;
