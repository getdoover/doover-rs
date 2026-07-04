//! `PlatformClient` — a client for the platform-interface sidecar
//! (`platform_iface.platformIface`, default `127.0.0.1:50053`), the Rust
//! equivalent of pydoover's `PlatformInterface`. Draft: the analog-input read
//! (`fetch_ai`) an app like the analog level sensor needs; the rest of the
//! surface (DI/DO/AO, power, location, …) is future work.

use std::time::Duration;

use tonic::transport::{Channel, Endpoint};

use doover_proto::platform_iface as pb;
use pb::platform_iface_client::PlatformIfaceClient as GenClient;

use crate::error::{DooverError, Result};

#[derive(Clone)]
pub struct PlatformClient {
    inner: GenClient<Channel>,
}

impl PlatformClient {
    /// Connect to the platform sidecar (default `http://127.0.0.1:50053`).
    pub async fn connect(uri: impl Into<String>) -> Result<Self> {
        let endpoint = Endpoint::from_shared(uri.into())
            .map_err(|e| DooverError::Other(format!("bad plt uri: {e}")))?
            .keep_alive_while_idle(true)
            .connect_timeout(Duration::from_secs(10));
        Ok(Self { inner: GenClient::new(endpoint.connect().await?) })
    }

    /// Read one analog-input pin (mA). Errors if the sidecar reports failure or
    /// returns no value.
    pub async fn fetch_ai(&self, pin: i32) -> Result<f32> {
        Ok(self.fetch_ais(&[pin]).await?.into_iter().next().unwrap_or(0.0))
    }

    /// Read several analog-input pins in one transaction.
    pub async fn fetch_ais(&self, pins: &[i32]) -> Result<Vec<f32>> {
        let resp = self
            .inner
            .clone()
            .get_ai(pb::GetAiRequest { ai: pins.to_vec() })
            .await?
            .into_inner();
        if let Some(h) = &resp.response_header {
            if !h.success {
                return Err(DooverError::Http {
                    code: h.response_code.unwrap_or(500),
                    message: h.message.clone().unwrap_or_default(),
                });
            }
        }
        Ok(resp.ai)
    }

    /// Liveness echo.
    pub async fn test_comms(&self, message: impl Into<String>) -> Result<String> {
        let resp = self
            .inner
            .clone()
            .test_comms(pb::TestCommsRequest { message: message.into() })
            .await?
            .into_inner();
        Ok(resp.response)
    }
}
