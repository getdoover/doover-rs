//! Error taxonomy, ported from pydoover's `GRPCInterface.process_response`
//! (docker) and `pydoover/models/data/exceptions.py` (cloud HTTP): a
//! `ResponseHeader` with `success == false` or an HTTP 4xx/5xx maps to
//! `NotFound` (code 404) or `Http { code, message }`; transport/decoding
//! failures are their own arms.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DooverError {
    // The tonic variants are boxed to keep `Result<T>` small — `tonic::Status`
    // alone is ~176 bytes inline (clippy::result_large_err).
    #[error("gRPC transport error: {0}")]
    Transport(#[from] Box<tonic::transport::Error>),

    #[error("gRPC status: {0}")]
    Status(#[from] Box<tonic::Status>),

    /// The agent (or cloud API) returned a 404 — the channel / message /
    /// aggregate / subscription does not exist (pydoover `NotFoundError`).
    #[error("not found: {0}")]
    NotFound(String),

    /// Any other `success:false` response header or non-2xx HTTP status
    /// (pydoover `HTTPError` / `BadRequestError` / `UnauthorizedError` /
    /// `ForbiddenError` — the status code disambiguates).
    #[error("agent error {code}: {message}")]
    Http { code: i32, message: String },

    /// HTTP transport failure talking to the cloud data API (DNS, TLS,
    /// connect, decode) — the analogue of `aiohttp.ClientError`.
    #[cfg(feature = "cloud-api")]
    #[error("HTTP request error: {0}")]
    Request(#[from] Box<reqwest::Error>),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    #[error("{0}")]
    Other(String),
}

impl From<tonic::transport::Error> for DooverError {
    fn from(e: tonic::transport::Error) -> Self {
        Self::Transport(Box::new(e))
    }
}

impl From<tonic::Status> for DooverError {
    fn from(e: tonic::Status) -> Self {
        Self::Status(Box::new(e))
    }
}

#[cfg(feature = "cloud-api")]
impl From<reqwest::Error> for DooverError {
    fn from(e: reqwest::Error) -> Self {
        Self::Request(Box::new(e))
    }
}

impl DooverError {
    /// A short pydoover-exception-style name for the invocation summary's
    /// `error.type` field (`{"type": ..., "message": ...}`). Python publishes
    /// `type(e).__name__`; these are the closest stable analogues.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Transport(_) => "TransportError",
            Self::Status(_) => "StatusError",
            Self::NotFound(_) => "NotFoundError",
            Self::Http { .. } => "HTTPError",
            #[cfg(feature = "cloud-api")]
            Self::Request(_) => "RequestError",
            Self::Json(_) => "JSONError",
            Self::InvalidPayload(_) => "InvalidPayload",
            Self::Other(_) => "RuntimeError",
        }
    }
}

pub type Result<T> = std::result::Result<T, DooverError>;
