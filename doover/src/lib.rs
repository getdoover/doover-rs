//! # doover
//!
//! A client library for writing Doover device applications in Rust — the Rust
//! counterpart to `pydoover`. Apps connect to the local **Doover Device
//! Agent** (DDA) over gRPC and read/write channels (aggregates, messages,
//! events); this crate wraps that contract in an ergonomic async API and a
//! declarative `Application` framework that mirrors pydoover's config / tags
//! / UI classes and `run_app` lifecycle.
//!
//! ## Quick start
//!
//! ```no_run
//! use doover::error::Result;
//! use doover::tags::Tag;
//! use doover::ui::{NumericVariable, UiBuild};
//! use doover::{AppContext, Application, Tags, Ui};
//!
//! /// Tag declarations — the field name is the tag name.
//! #[derive(Tags)]
//! struct MyTags {
//!     #[tag(live, default = None)]
//!     level: Tag<f64>,
//! }
//!
//! /// UI declarations, built from the tags.
//! #[derive(Ui)]
//! struct MyUi {
//!     level: NumericVariable,
//! }
//!
//! impl UiBuild for MyUi {
//!     type Tags = MyTags;
//!
//!     fn build(tags: &MyTags) -> Self {
//!         Self { level: NumericVariable::new("Level").units("%").value(&tags.level) }
//!     }
//! }
//!
//! struct MyApp {
//!     tags: MyTags,
//!     ui: MyUi,
//!     reading: f64,
//! }
//!
//! #[doover::async_trait]
//! impl Application for MyApp {
//!     type Config = (); // or #[derive(Config)] for a typed schema
//!     type Tags = MyTags;
//!     type Ui = MyUi;
//!
//!     fn create(_config: (), tags: MyTags, ui: MyUi) -> Self {
//!         Self { tags, ui, reading: 0.0 }
//!     }
//!
//!     fn ui(&self) -> Option<&MyUi> {
//!         Some(&self.ui)
//!     }
//!
//!     fn ui_mut(&mut self) -> Option<&mut MyUi> {
//!         Some(&mut self.ui)
//!     }
//!
//!     async fn main_loop(&mut self, _ctx: &AppContext) -> Result<()> {
//!         self.reading += 1.0;
//!         self.tags.level.set(self.reading).await
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     // Also provides the built-in `export` subcommand:
//!     //   my-app export [doover_config.json] [--app-name NAME]
//!     doover::run::<MyApp>().await
//! }
//! ```
//!
//! ## Status
//!
//! Implemented: the `DeviceAgentClient` (aggregates, messages, one-shots,
//! event subscriptions) with the lossless `data_json` encoding and typed
//! error mapping; the declarative `Config`/`ConfigObject`/`ConfigEnum`,
//! `Tags` and `Ui` derives with byte-exact `doover_config.json` export; the
//! `Application` runtime (typed config loading, tag attachment, `ui_state`
//! publishing, `ui_cmds` command dispatch, drift-corrected loop); the
//! `RpcManager` (`dv-rpc`) and `send_notification`; the `platform_iface` and
//! `modbus_iface` sidecar clients with pydoover's shared-channel retry
//! semantics; and the `doover` CLI.
//!
//! ## Processors (feature `processor`)
//!
//! Event-driven cloud apps on AWS Lambda talking HTTP to `data.doover.com`
//! instead of a local agent — see the [`processor`] module. The `cloud-api`
//! feature exposes the underlying [`api::DataClient`] (also a
//! [`ChannelBackend`]) on its own.
//!
//! Not yet ported (see the repo README roadmap): tag `log_on` triggers,
//! Submodule/Camera/Multiplot UI elements, auth profiles/OIDC, and
//! declarative processor-config authoring.

#[cfg(feature = "cloud-api")]
pub mod api;
pub mod channel_backend;
pub mod config;
pub mod docker;
pub mod error;
pub mod events;
pub mod models;
#[cfg(feature = "processor")]
pub mod processor;
pub mod rpc;
pub mod tags;
#[cfg(feature = "testing")]
pub mod testing;
pub mod ui;
pub mod utils;

#[cfg(feature = "cloud-api")]
pub use api::DataClient;
pub use channel_backend::{AggregateOptions, ChannelBackend, UpdateMessageOptions};
pub use config::{Config, ConfigSchema};
pub use docker::application::{
    channels, run, run_with, write_export, AppContext, Application, RunOptions,
};
pub use docker::device_agent::{DdaStatus, DeviceAgentClient};
pub use docker::modbus::ModbusClient;
pub use docker::platform::PlatformClient;
pub use docker::subscriptions::SubscriptionHub;
pub use error::{DooverError, Result};
pub use events::{Event, EventSubscription};
pub use models::{Notification, NotificationSeverity};
pub use rpc::{RpcContext, RpcError, RpcManager};
pub use tags::{Tag, TagsCollection};
pub use ui::{UiCommand, UiRuntime};

// The derive macros. These live in the macro namespace, so e.g.
// `doover::Config` the derive and `doover::Config` the dynamic config struct
// coexist (likewise `doover::Tags` the derive and `doover::tags` the module).
#[cfg(feature = "macros")]
pub use doover_macros::{Config, ConfigEnum, ConfigObject, Tags, Ui};

// Re-exports for macro-generated code; not public API.
#[doc(hidden)]
pub mod __private {
    pub use serde_json;
}

// Re-export so `#[doover::async_trait]` works without a direct dependency.
pub use async_trait::async_trait;

/// The generated protobuf/gRPC types, for advanced use.
pub use doover_proto as proto;
