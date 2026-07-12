//! The Doover **processor** runtime — event-driven cloud apps on AWS Lambda
//! (pydoover `pydoover/processor/`).
//!
//! Where a docker app runs a persistent loop against the local device agent
//! over gRPC, a processor handles exactly one event per invocation over
//! HTTP against `data.doover.com`: the invocation arrives with a minimal
//! JWT, the runner upgrades it via the subscription/schedule info endpoint
//! (getting the full token, `agent_id`, `app_key` and seeded channels), the
//! [`Processor`] handler runs, buffered tags flush once, and an invocation
//! summary is fanned out to the `dv_proc_config.inv_targets` channels.
//!
//! ```no_run
//! use doover::processor::{
//!     Handled, MessageCreateEvent, Processor, ProcessorContext, run_processor,
//! };
//!
//! #[derive(Default)]
//! struct MyProcessor;
//!
//! #[doover::async_trait]
//! impl Processor for MyProcessor {
//!     async fn on_message_create(
//!         &mut self,
//!         ctx: &ProcessorContext,
//!         event: &MessageCreateEvent,
//!     ) -> doover::error::Result<Handled> {
//!         ctx.set_tag("last_channel", event.channel.name.clone().into());
//!         Ok(Handled::Done)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), lambda_runtime::Error> {
//!     run_processor::<MyProcessor>().await
//! }
//! ```

pub mod application;
pub mod config;
pub mod events;
pub mod handler;
pub mod runner;
pub mod tags;

pub use application::{
    Handled, PingOptions, Processor, ProcessorContext, SkipReason, DEFAULT_OFFLINE_AFTER_SECS,
};
pub use config::{InvocationPublishTarget, ProcConfig};
pub use events::{
    AggregateUpdateEvent, ChannelId, DeploymentEvent, EventMessage, EventPayload,
    IngestionEndpointEvent, ManualInvokeEvent, MessageCreateEvent, ScheduleEvent,
};
pub use handler::{handle_event_local, handle_event_with, run_processor, ProcessorOptions};
pub use runner::LambdaMeta;
pub use tags::{LogMode, ProcessorTags, SetProcessorTagOptions};
