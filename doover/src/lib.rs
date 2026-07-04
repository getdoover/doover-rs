//! # doover
//!
//! A client library for writing Doover device applications in Rust — the Rust
//! counterpart to `pydoover`. Apps connect to the local **Doover Device
//! Agent** (DDA) over gRPC and read/write channels (aggregates, messages,
//! events); this crate wraps that contract in an ergonomic async API and an
//! `Application` runtime that mirrors pydoover's `run_app` lifecycle.
//!
//! ## Quick start
//!
//! ```no_run
//! use doover::{Application, AppContext, run_app};
//! use doover::error::Result;
//! use serde_json::json;
//!
//! struct MyApp { level: f64 }
//!
//! #[doover::async_trait]
//! impl Application for MyApp {
//!     async fn main_loop(&mut self, ctx: &AppContext) -> Result<()> {
//!         self.level += 1.0;
//!         ctx.set_tag("level", json!(self.level)).await?;
//!         Ok(())
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     run_app(MyApp { level: 0.0 }).await
//! }
//! ```
//!
//! ## Status (draft)
//!
//! Implemented: the `DeviceAgentClient` (aggregates, messages, one-shots,
//! event subscriptions) with the lossless `data_json` encoding and typed
//! error mapping; the `Application` trait + drift-corrected `run_app` loop;
//! `tag_values` writes (`set_tag`/`set_tags`); dynamic config from `CONFIG_FP`.
//!
//! Not yet ported (see the repo README roadmap): declarative `Schema`/`Tags`/
//! `UI` derive macros, the `deployment_config` subscribe bootstrap, the
//! `platform_iface`/`modbus_iface` sidecar clients, RPC + notifications.

pub mod application;
pub mod client;
pub mod config;
pub mod error;
pub mod events;
pub mod platform;

pub use application::{run_app, run_app_with, AppContext, Application, RunOptions};
pub use client::{AggregateOptions, DeviceAgentClient};
pub use platform::PlatformClient;
pub use config::Config;
pub use error::{DooverError, Result};
pub use events::Event;

// Re-export so `#[doover::async_trait]` works without a direct dependency.
pub use async_trait::async_trait;

/// The generated protobuf/gRPC types, for advanced use.
pub use doover_proto as proto;
