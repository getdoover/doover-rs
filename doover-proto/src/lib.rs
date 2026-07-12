//! Generated gRPC/protobuf types for the Doover sidecar services (device
//! agent, platform interface, modbus interface) plus the standard
//! `grpc.health.v1` contract the sidecars expose.
//!
//! The protos are vendored verbatim from pydoover (`protos/*.proto`);
//! re-vendor when they change. Client stubs are what apps use; server stubs
//! are generated too so tests can run in-process fake sidecars.

pub mod device_agent {
    tonic::include_proto!("device_agent");
}

// The sidecar protos name streaming RPCs in lowerCamelCase, which the server
// codegen turns into non-camel-case associated types (e.g.
// `startPulseCounterStream`) — silence the lint rather than diverge from the
// vendored protos.
#[allow(non_camel_case_types)]
pub mod platform_iface {
    tonic::include_proto!("platform_iface");
}

#[allow(non_camel_case_types)]
pub mod modbus_iface {
    tonic::include_proto!("modbus_iface");
}

/// The standard gRPC health-checking protocol (`grpc.health.v1`), used by
/// pydoover's `GRPCInterface.health_check` / `wait_until_healthy`.
pub mod health {
    tonic::include_proto!("grpc.health.v1");
}

// Re-export so downstream crates share one prost-types version (Struct etc.).
pub use prost_types;
