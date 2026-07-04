//! Error taxonomy, ported from pydoover's `GRPCInterface.process_response`:
//! a `ResponseHeader` with `success == false` maps to `NotFound` (code 404) or
//! `Http { code, message }`; transport/decoding failures are their own arms.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DooverError {
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC status: {0}")]
    Status(#[from] tonic::Status),

    /// The agent returned a `ResponseHeader{success:false, code:404}` — the
    /// channel / message / aggregate does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Any other `success:false` response header.
    #[error("agent error {code}: {message}")]
    Http { code: i32, message: String },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DooverError>;
