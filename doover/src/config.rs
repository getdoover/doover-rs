//! App configuration.
//!
//! In development the config is a JSON file pointed at by `CONFIG_FP`
//! (`--config-fp`); in production the agent injects it via the
//! `deployment_config` channel. This draft supports the file path and a
//! prefetched dict; the `deployment_config` subscribe bootstrap is a TODO
//! (see README). Access is dynamic (`get`/`get_str`/…) — a derive-macro
//! `Schema` mirroring pydoover's declarative config is future work.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{DooverError, Result};

#[derive(Debug, Clone, Default)]
pub struct Config {
    root: Value,
}

impl Config {
    pub fn from_value(root: Value) -> Self {
        Self { root }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| DooverError::Other(format!("reading config {:?}: {e}", path.as_ref())))?;
        Ok(Self { root: serde_json::from_str(&text)? })
    }

    pub fn root(&self) -> &Value {
        &self.root
    }

    /// Raw value at a top-level key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.root.get(key)
    }

    /// Typed value at a top-level key.
    pub fn get_as<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.root.get(key).and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.root.get(key).and_then(|v| v.as_str().map(str::to_string))
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.root.get(key).and_then(Value::as_f64)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.root.get(key).and_then(Value::as_i64)
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.root.get(key).and_then(Value::as_bool)
    }
}
