//! Container healthcheck endpoint — pydoover's aiohttp server on
//! `127.0.0.1:HEALTHCHECK_PORT` (default 49200), reduced to a hand-rolled
//! HTTP/1.1 responder over a raw TCP listener so the scratch image carries no
//! HTTP-framework dependency.
//!
//! Semantics match pydoover `_handle_healthcheck`: any request gets
//! `200 OK` / body `OK` while the app is healthy, `503` / body `ERROR`
//! otherwise. The flag starts false and is set true after each successful
//! `main_loop` iteration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Shared healthy/unhealthy flag between the run loop and the server.
#[derive(Clone, Default)]
pub struct HealthState(Arc<AtomicBool>);

impl HealthState {
    pub fn set_healthy(&self, healthy: bool) {
        self.0.store(healthy, Ordering::Relaxed);
    }

    pub fn is_healthy(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

const RESPONSE_OK: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nOK";
const RESPONSE_ERROR: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/plain\r\ncontent-length: 5\r\nconnection: close\r\n\r\nERROR";

/// Start the healthcheck server in the background. A bind failure is logged
/// but not fatal, matching pydoover.
pub async fn spawn_healthcheck_server(port: u16, state: HealthState) {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => {
            tracing::info!("healthcheck server listening on http://127.0.0.1:{port}");
            l
        }
        Err(e) => {
            tracing::error!("error starting healthcheck server on port {port}: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                // Read (and discard) the request line/headers; any bytes make
                // this a request worth answering.
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = if state.is_healthy() { RESPONSE_OK } else { RESPONSE_ERROR };
                let _ = socket.write_all(response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
}
