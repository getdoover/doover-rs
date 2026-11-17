//! Container healthcheck endpoint — pydoover's aiohttp server on
//! `127.0.0.1:HEALTHCHECK_PORT` (default 49200), reduced to a hand-rolled
//! HTTP/1.1 responder over a raw TCP listener so the scratch image carries no
//! HTTP-framework dependency.
//!
//! Semantics match pydoover `_handle_healthcheck`: any request gets
//! `200 OK` / body `OK` while the app is healthy, `503` / body `ERROR`
//! otherwise. The flag starts false and is set true after each successful
//! `main_loop` iteration.
//!
//! [`probe`] is the other end, for the image's `HEALTHCHECK`: a scratch image
//! has no curl, so `run` answers `<binary> healthcheck` with it (pydoover apps
//! use `curl -f 127.0.0.1:$HEALTHCHECK_PORT`). Doover shows Docker's verdict
//! as the app's health, which reads "unknown" without one.

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

/// The port the server listens on: `HEALTHCHECK_PORT`, default 49200.
pub fn port_from_env() -> u16 {
    std::env::var("HEALTHCHECK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(49200)
}

/// Ask the healthcheck server on `port` once: `Ok` on `200`, otherwise why
/// not. Blocking, with short timeouts, as it runs as a one-shot process.
pub fn probe(port: u16) -> std::result::Result<(), String> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let timeout = Duration::from_secs(3);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("{addr}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).map_err(|e| format!("{addr}: {e}"))?;
    match &head {
        h if h.starts_with(b"HTTP/1.") && &h[9..12] == b"200" => Ok(()),
        h => Err(format!("{addr} answered {:?}", String::from_utf8_lossy(h))),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn the_probe_follows_the_flag() {
        let port = free_port();
        let state = HealthState::default();
        spawn_healthcheck_server(port, state.clone()).await;
        let probe_now = move || tokio::task::spawn_blocking(move || probe(port));
        assert!(probe_now().await.unwrap().unwrap_err().contains("503"), "starts unhealthy");
        state.set_healthy(true);
        assert_eq!(probe_now().await.unwrap(), Ok(()));
    }

    #[test]
    fn nothing_listening_is_unhealthy() {
        assert!(probe(free_port()).is_err());
    }
}
