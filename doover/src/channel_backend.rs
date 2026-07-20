//! [`ChannelBackend`] — the channel I/O surface shared by managers that must
//! run against both the docker device agent (gRPC) and, later, the processor
//! (HTTP cloud API). pydoover shares its RPC/UI/tags managers between the two
//! by duck typing; this trait makes that contract explicit so the managers
//! ([`RpcManager`](crate::rpc::RpcManager), [`UiRuntime`](crate::ui::UiRuntime))
//! are written once.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::Result;

/// A file attached to an aggregate or message (pydoover `Attachment`).
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub filename: String,
    /// Absent when the agent doesn't know the type.
    pub content_type: Option<String>,
    pub size: u64,
    pub url: String,
}

impl Attachment {
    /// The pydoover `Attachment.to_dict()` shape — note `size`, not the
    /// proto's `size_bytes`.
    pub fn to_json(&self) -> Value {
        json!({
            "filename": self.filename,
            "content_type": self.content_type,
            "size": self.size,
            "url": self.url,
        })
    }
}

/// A channel aggregate: the whole object, not just its payload. `data` is the
/// current state; `attachments` and `last_updated` are part of the aggregate
/// too, and pydoover's `Aggregate` carries all three.
///
/// Managers that only ever want the payload take the
/// [`ChannelBackend::fetch_channel_data`] path instead.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelAggregate {
    pub data: Value,
    pub attachments: Vec<Attachment>,
    /// Unix epoch milliseconds of the last write, as the agent reports it.
    pub last_updated: Option<u64>,
}

impl ChannelAggregate {
    /// The pydoover `Aggregate.to_dict()` shape, which the CLI prints verbatim.
    /// `last_updated` is a float there (a `datetime` scaled to milliseconds),
    /// so it stays a float here.
    pub fn to_json(&self) -> Value {
        json!({
            "data": self.data,
            "attachments": self.attachments.iter().map(Attachment::to_json).collect::<Vec<_>>(),
            "last_updated": self.last_updated.map(|ms| ms as f64),
        })
    }
}

/// Options for an aggregate write (`update_channel_aggregate`).
#[derive(Debug, Clone, Default)]
pub struct AggregateOptions {
    /// Coalesce with other writes to this channel and flush at most this old
    /// (seconds). 0 = publish immediately.
    pub max_age_secs: f32,
    /// Log a historical datapoint in addition to updating current state.
    pub save_log: bool,
    /// Replace the whole aggregate rather than merge-patch it.
    pub replace_data: bool,
    pub clear_attachments: bool,
    /// Dotted key paths to *replace* (rather than merge) within an otherwise
    /// merge-patch write — pydoover's `replace_keys` (`?replace=` on the HTTP
    /// aggregate endpoint), used by the processor's `publish_ui_schema` clear
    /// path (`state.children.<app_key>`).
    ///
    /// Divergence: only the cloud HTTP backend honours this. The device-agent
    /// gRPC proto (`UpdateAggregateRequest`) has no such field, so the docker
    /// backend logs a warning and merges normally.
    pub replace_keys: Vec<String>,
}

/// Options for `update_message`.
#[derive(Debug, Clone, Default)]
pub struct UpdateMessageOptions {
    /// Replace the whole payload rather than merge-patch it.
    pub replace_data: bool,
    pub clear_attachments: bool,
}

/// Async channel reads/writes, independent of the transport.
///
/// Implemented by [`DeviceAgentClient`](crate::DeviceAgentClient) (gRPC,
/// persistent connection) today; a processor-side HTTP implementation reuses
/// the same managers later.
#[async_trait]
pub trait ChannelBackend: Send + Sync {
    /// Current aggregate *data* for a channel, or `None` if it doesn't exist.
    ///
    /// Only the payload: the managers behind this trait never need the
    /// attachments or update stamp, and the subscription cache backing some
    /// implementations only holds data. For the whole aggregate, use
    /// [`DeviceAgentClient::fetch_channel_aggregate`](crate::DeviceAgentClient::fetch_channel_aggregate).
    async fn fetch_channel_data(&self, channel: &str) -> Result<Option<Value>>;

    /// Merge-write (or replace, per `opts`) a channel aggregate.
    async fn update_channel_aggregate(
        &self,
        channel: &str,
        data: &Value,
        opts: &AggregateOptions,
    ) -> Result<()>;

    /// Append a message to a channel; returns the minted message id.
    async fn create_message(&self, channel: &str, data: &Value) -> Result<u64>;

    /// Update an existing message's payload (merge unless
    /// `opts.replace_data`).
    async fn update_message(
        &self,
        channel: &str,
        message_id: u64,
        data: &Value,
        opts: &UpdateMessageOptions,
    ) -> Result<()>;

    /// Whether this backend holds a live event connection (docker: yes;
    /// processor over HTTP: no — pydoover's `is_processor` checks gate
    /// subscription-dependent behaviour on this).
    fn has_persistent_connection(&self) -> bool;
}
