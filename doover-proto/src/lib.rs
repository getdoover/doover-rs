//! Generated gRPC/protobuf types for the Doover device agent.
//!
//! The proto is vendored verbatim from pydoover (`protos/device_agent.proto`);
//! re-vendor when it changes. This crate builds the *client* stubs only.

pub mod device_agent {
    tonic::include_proto!("device_agent");
}

pub mod platform_iface {
    tonic::include_proto!("platform_iface");
}

// Re-export so downstream crates share one prost-types version (Struct etc.).
pub use prost_types;
