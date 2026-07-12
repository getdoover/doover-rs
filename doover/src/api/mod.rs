//! Async client for the Doover cloud data API (`data.doover.com`) —
//! the Rust counterpart to pydoover's `pydoover.api` data client
//! (`pydoover/api/data/_async.py` + `_base.py`), scoped to the subset the
//! processor runtime uses.
//!
//! [`DataClient`] implements [`ChannelBackend`](crate::ChannelBackend), so
//! managers written against the trait (RPC / UI / processor tags) run
//! unchanged over HTTP.

pub mod auth;
pub mod data;

pub use auth::BearerAuth;
pub use data::{
    Channel, DataClient, ListMessagesQuery, PingConnectionArgs, DATA_ENDPOINT_ENV,
    DEFAULT_DATA_ENDPOINT,
};
