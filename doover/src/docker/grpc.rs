//! Shared gRPC plumbing for the hardware sidecar clients — the Rust port of
//! pydoover's `GRPCInterface` (`docker/grpc_interface.py`).
//!
//! Unary requests share one persistent channel rather than paying a TCP +
//! HTTP/2 handshake per call. If the channel has gone stale — most commonly
//! the sidecar restarting under us — the call fails fast with `UNAVAILABLE`
//! and is retried once on a freshly built channel. Every call carries a
//! deadline, so a half-dead connection costs at most one timeout before the
//! channel is rebuilt; a wedged channel can never permanently wedge the
//! client.
//!
//! Used by [`PlatformClient`](crate::docker::platform::PlatformClient) and
//! [`ModbusClient`](crate::docker::modbus::ModbusClient);
//! `DeviceAgentClient` keeps its own (simpler) channel handling.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

use doover_proto::health::health_client::HealthClient;
use doover_proto::health::health_check_response::ServingStatus;
use doover_proto::health::HealthCheckRequest;

use crate::error::{DooverError, Result};

/// Per-call deadline, matching pydoover `GRPCInterface(timeout=7)`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(7);

/// A lazily connected, shared tonic [`Channel`] with pydoover
/// `GRPCInterface` semantics: keepalive pings only while calls are in
/// flight, a per-call deadline, and a rebuild-and-retry-once path for
/// `UNAVAILABLE` failures ([`SharedChannel::call`]).
pub struct SharedChannel {
    endpoint: Endpoint,
    service_name: String,
    timeout: Duration,
    channel: Mutex<Option<Channel>>,
}

impl SharedChannel {
    /// Build the endpoint (pydoover `GRPCInterface.__init__` +
    /// `_CHANNEL_OPTIONS`): keepalive time 10s / timeout 5s, pings only while
    /// calls are in flight (`keepalive_permit_without_calls` stays 0 — idle
    /// channel health is handled by the retry-on-fresh-channel path).
    ///
    /// `service_name` is the health-check service name, e.g.
    /// `doover.PlatformInterface`.
    pub fn new(uri: impl Into<String>, service_name: impl Into<String>) -> Result<Self> {
        let uri = uri.into();
        let endpoint = Endpoint::from_shared(uri.clone())
            .map_err(|e| DooverError::Other(format!("bad grpc uri {uri:?}: {e}")))?
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5));
        Ok(Self {
            endpoint,
            service_name: service_name.into(),
            timeout: DEFAULT_TIMEOUT,
            channel: Mutex::new(None),
        })
    }

    /// Override the per-call deadline (pydoover `timeout` parameter).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The shared channel, built lazily on first use (pydoover `_get_stub`).
    /// tonic channels connect on demand, so this never blocks.
    pub fn channel(&self) -> Channel {
        let mut guard = self.channel.lock().expect("channel lock poisoned");
        guard.get_or_insert_with(|| self.endpoint.connect_lazy()).clone()
    }

    /// A channel of its own for long-lived streams, mirroring pydoover's
    /// fresh `grpc.aio.insecure_channel` per streaming call — a stream on the
    /// shared channel would otherwise keep it looking healthy while unary
    /// calls wedge, and vice versa.
    pub fn fresh_channel(&self) -> Channel {
        self.endpoint.connect_lazy()
    }

    /// Drop the shared channel so the next call builds a fresh one
    /// (pydoover `_discard_channel`). tonic channels close when the last
    /// clone is dropped, so in-flight calls on the old channel finish
    /// normally.
    pub fn discard(&self) {
        *self.channel.lock().expect("channel lock poisoned") = None;
    }

    /// Make a unary request with pydoover `make_request` semantics: apply the
    /// per-call deadline; on `UNAVAILABLE` rebuild the channel and retry the
    /// call once; on a deadline rebuild the channel but propagate the error
    /// (retrying would double the worst-case latency of a genuinely slow
    /// server).
    ///
    /// `f` is invoked with the channel to use and may run at most twice, so
    /// it must be a `Fn` (clone the request inside it).
    pub async fn call<R, F, Fut>(&self, f: F) -> Result<R>
    where
        F: Fn(Channel) -> Fut,
        Fut: Future<Output = std::result::Result<tonic::Response<R>, Status>>,
    {
        match self.call_once(&f).await {
            Err(DooverError::Status(status)) if status.code() == Code::Unavailable => {
                // The channel itself is suspect (sidecar restarted under us) —
                // not an application error. Rebuild and retry once.
                self.discard();
                self.call_once(&f).await
            }
            other => other,
        }
    }

    async fn call_once<R, F, Fut>(&self, f: &F) -> Result<R>
    where
        F: Fn(Channel) -> Fut,
        Fut: Future<Output = std::result::Result<tonic::Response<R>, Status>>,
    {
        let channel = self.channel();
        match tokio::time::timeout(self.timeout, f(channel)).await {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => {
                if status.code() == Code::DeadlineExceeded {
                    // Half-dead connection: rebuild so the next call starts
                    // clean, but don't retry (see `call`).
                    self.discard();
                }
                Err(status.into())
            }
            Err(_elapsed) => {
                self.discard();
                Err(Status::deadline_exceeded(format!(
                    "request to {} did not complete within {:?}",
                    self.service_name, self.timeout
                ))
                .into())
            }
        }
    }

    /// One-shot `grpc.health.v1` probe (pydoover `health_check`): `true` iff
    /// the service reports `SERVING`. Any transport error is `false`.
    pub async fn health_check(&self) -> bool {
        // Fresh channel, as in pydoover — a broken shared channel must not
        // make a healthy server look unhealthy.
        let mut client = HealthClient::new(self.fresh_channel());
        let req = HealthCheckRequest { service: self.service_name.clone() };
        match tokio::time::timeout(self.timeout, client.check(req)).await {
            Ok(Ok(resp)) => resp.into_inner().status() == ServingStatus::Serving,
            Ok(Err(status)) => {
                tracing::debug!("health check for {} failed: {status}", self.service_name);
                false
            }
            Err(_) => {
                tracing::debug!("health check for {} timed out", self.service_name);
                false
            }
        }
    }

    /// Poll [`health_check`](Self::health_check) until it succeeds
    /// (pydoover `wait_until_healthy`). As in pydoover, this never gives up —
    /// bound it with `tokio::time::timeout` if you need a deadline.
    pub async fn wait_until_healthy(&self, interval: Duration) {
        loop {
            if self.health_check().await {
                return;
            }
            tokio::time::sleep(interval).await;
        }
    }
}

/// Map a decomposed response header to a typed error (pydoover
/// `GRPCInterface.process_response`): `success == false` becomes
/// [`DooverError::NotFound`] for code 404, [`DooverError::Http`] otherwise.
/// Each sidecar has its own header message type, so callers pass the fields.
pub(crate) fn check_response_header(
    success: bool,
    code: Option<i32>,
    message: Option<String>,
) -> Result<()> {
    if success {
        return Ok(());
    }
    let code = code.unwrap_or(500);
    let message = message.unwrap_or_else(|| "Unknown error".to_string());
    if code == 404 {
        Err(DooverError::NotFound(message))
    } else {
        Err(DooverError::Http { code, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_maps_to_typed_errors() {
        assert!(check_response_header(true, None, None).is_ok());
        assert!(matches!(
            check_response_header(false, Some(404), Some("missing".into())),
            Err(DooverError::NotFound(m)) if m == "missing"
        ));
        assert!(matches!(
            check_response_header(false, Some(503), None),
            Err(DooverError::Http { code: 503, .. })
        ));
        // pydoover defaults a missing code to 500.
        assert!(matches!(
            check_response_header(false, None, None),
            Err(DooverError::Http { code: 500, .. })
        ));
    }

    #[test]
    fn bad_uri_is_rejected() {
        assert!(SharedChannel::new("not a uri", "svc").is_err());
    }
}
