//! Bearer-token plumbing for the cloud data API.
//!
//! pydoover has a whole auth-client hierarchy (`pydoover/api/auth/`):
//! `~/.doover` profiles, `Doover2AuthClient` refresh-token flows against
//! auth.doover.com, GitHub-OIDC exchange, and expiry-aware `ensure_token`.
//! Processors never need any of that — they are handed a short-lived JWT in
//! the invocation event and upgrade it via the subscription/schedule info
//! endpoint — so this module is deliberately minimal.
//!
//! TODO(auth): port `AuthProfile` loading from `~/.doover` (profile name →
//! token/base URLs) for CLI/interactive use.
//! TODO(auth): port the refresh-token / OIDC `ensure_token` flow
//! (`Doover2AuthClient`) when a long-running Rust cloud consumer appears.

use std::sync::Mutex;

/// A manually-managed bearer token (pydoover `StaticTokenAuth` semantics:
/// `set_token` + `Authorization: Bearer …`; `ensure_token` is a no-op).
#[derive(Debug, Default)]
pub struct BearerAuth {
    token: Mutex<Option<String>>,
}

impl BearerAuth {
    pub fn new(token: Option<String>) -> Self {
        Self { token: Mutex::new(token) }
    }

    pub fn set_token(&self, token: impl Into<String>) {
        *self.token.lock().unwrap() = Some(token.into());
    }

    pub fn token(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }

    /// The `Authorization` header value, if a token is set.
    pub fn authorization(&self) -> Option<String> {
        self.token().map(|t| format!("Bearer {t}"))
    }
}
