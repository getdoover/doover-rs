//! Deserialization of `deployment_config["dv_proc_config"]` — the
//! framework-level runtime config pydoover models declaratively in
//! `pydoover/processor/config.py::ProcessorConfig`.
//!
//! Only the *consumption* side is ported here (serde structs with pydoover's
//! defaults); declarative authoring of processor config schemas
//! (`SubscriptionConfig` / `ScheduleConfig` / `IngestionEndpointConfig` /
//! `ExtendedPermissionsConfig` …) belongs to the config module and is not in
//! scope yet.

use serde::Deserialize;
use serde_json::{Map, Value};

/// One destination for the per-invocation summary message
/// (`InvocationPublishTarget`).
#[derive(Debug, Clone, Deserialize)]
pub struct InvocationPublishTarget {
    /// Agent to post on behalf of; `None` = this agent. Arrives as a string
    /// or number.
    #[serde(default, deserialize_with = "de_opt_id")]
    pub agent_id: Option<u64>,
    /// Channel name on the target agent; `$app_id` is substituted with the
    /// app's ID.
    pub channel: String,
}

/// `dv_proc_config` — pydoover `ProcessorConfig` with its element defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProcConfig {
    /// Root log level after setup (default `INFO`). Divergence: doover-rs
    /// does not yet re-apply this to the `tracing` subscriber (TODO).
    pub log_level: String,
    /// Per-logger level overrides. Deserialize-only, unapplied (TODO).
    pub log_overrides: Map<String, Value>,
    /// Agents/channels to fan the invocation summary out to. Empty list =
    /// summaries disabled; missing key = the pydoover default single target
    /// `{"agent_id": null, "channel": "dv-proc-inv-$app_id"}`.
    pub inv_targets: Vec<InvocationPublishTarget>,
    /// Stream logs to a channel during the invocation (unimplemented; the
    /// flag is parsed so configs round-trip).
    pub live_logs: bool,
}

impl Default for ProcConfig {
    fn default() -> Self {
        Self {
            log_level: "INFO".to_string(),
            log_overrides: Map::new(),
            inv_targets: vec![InvocationPublishTarget {
                agent_id: None,
                channel: "dv-proc-inv-$app_id".to_string(),
            }],
            live_logs: false,
        }
    }
}

impl ProcConfig {
    /// Parse `deployment_config["dv_proc_config"]`; `None`/null/invalid
    /// values fall back to the defaults (pydoover `load_data`).
    pub fn from_value(value: Option<&Value>) -> Self {
        match value {
            Some(v) if v.is_object() => serde_json::from_value(v.clone()).unwrap_or_else(|e| {
                tracing::warn!("invalid dv_proc_config, using defaults: {e}");
                Self::default()
            }),
            _ => Self::default(),
        }
    }
}

fn de_opt_id<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(deserializer)?;
    Ok(v.as_ref().and_then(crate::models::value_as_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_pydoover() {
        let cfg = ProcConfig::from_value(None);
        assert_eq!(cfg.log_level, "INFO");
        assert_eq!(cfg.inv_targets.len(), 1);
        assert_eq!(cfg.inv_targets[0].agent_id, None);
        assert_eq!(cfg.inv_targets[0].channel, "dv-proc-inv-$app_id");
        assert!(!cfg.live_logs);
    }

    #[test]
    fn empty_targets_disable_summaries() {
        let cfg = ProcConfig::from_value(Some(&json!({"inv_targets": []})));
        assert!(cfg.inv_targets.is_empty());
    }

    #[test]
    fn explicit_target_with_string_agent_id() {
        let cfg = ProcConfig::from_value(Some(&json!({
            "inv_targets": [{"agent_id": "12345678901234567", "channel": "audit"}],
            "log_level": "DEBUG",
        })));
        assert_eq!(cfg.inv_targets[0].agent_id, Some(12345678901234567));
        assert_eq!(cfg.inv_targets[0].channel, "audit");
        assert_eq!(cfg.log_level, "DEBUG");
    }
}
