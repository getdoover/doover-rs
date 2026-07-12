//! UI runtime wiring — the port of pydoover's `UICommandsManager`
//! (`pydoover/ui/manager.py`) plus the `ui_state` publishing half of
//! `Application._setup`.
//!
//! Protocol (verified against pydoover):
//!
//! - **Commands are RPC messages on `ui_cmds`**: `UICommandsManager` extends
//!   `RPCManager`, so an incoming command is a `MessageCreate` /
//!   `OneShotMessage` whose payload is
//!   `{"type": "rpc", "method": <interaction name>, "request": <value>, …}`.
//!   The interaction's default handler writes the value back into the
//!   `ui_cmds` aggregate (`{app_key: {name: value}}`), records a
//!   `{"type": "log", "app_key": …, "key": …, "value": …}` message, and the
//!   manager then updates the request message with a `success` status.
//! - **`AggregateUpdate` / `ChannelSync` events on `ui_cmds`** only refresh
//!   the cached current values (`aggregate.data[app_key]`) — they do NOT
//!   dispatch commands.
//! - **`ui_state` publish**: after setup, pydoover clears then sets its
//!   subtree (`{"state": {"children": {app_key: null}}}` followed by the
//!   schema) with `max_age_secs=-1`, with `$config.app()` references resolved
//!   against the deployment config. pydoover does this only for non-static
//!   UIs and never re-publishes; the Rust runtime additionally re-publishes
//!   whenever the serialized schema changes after a `main_loop` (a superset —
//!   identical wire behaviour when the app never mutates its UI).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use crate::channel_backend::{AggregateOptions, ChannelBackend, UpdateMessageOptions};
use crate::error::Result;
use crate::events::Event;

use super::UiElement;

/// The channel UI commands arrive on / values are written back to.
pub const UI_CMDS_CHANNEL: &str = "ui_cmds";
/// The channel the UI schema is published to.
pub const UI_STATE_CHANNEL: &str = "ui_state";

/// A user command for one of this app's interactions, delivered to
/// [`Application::on_ui_command`](crate::Application::on_ui_command).
#[derive(Debug, Clone)]
pub struct UiCommand {
    /// The interaction's element name (the RPC `method`).
    pub name: String,
    /// The commanded value (the RPC `request` payload).
    pub value: Value,
    /// The request message id (`None` for one-shot commands, which cannot be
    /// responded to).
    pub(crate) message_id: Option<u64>,
}

impl UiCommand {
    /// Whether this command targets the given element
    /// (`cmd.is(&self.ui.my_button)`).
    pub fn is(&self, element: &impl UiElement) -> bool {
        self.name == UiElement::name(element)
    }

    /// The commanded value deserialized as `T`, if it fits.
    pub fn value_as<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.value.clone()).ok()
    }
}

/// Runtime state for the declarative UI: the cached `ui_cmds` values, the
/// interaction names commands may target, and the last-published `ui_state`
/// schema (for publish-on-change).
pub struct UiRuntime {
    backend: Arc<dyn ChannelBackend>,
    app_key: String,
    /// This app's subtree of the `ui_cmds` aggregate (pydoover
    /// `UICommandsManager.values`).
    values: Mutex<Value>,
    interactions: Mutex<HashSet<String>>,
    last_published: Mutex<Option<String>>,
}

impl UiRuntime {
    pub fn new(backend: Arc<dyn ChannelBackend>, app_key: impl Into<String>) -> Self {
        Self {
            backend,
            app_key: app_key.into(),
            values: Mutex::new(Value::Object(Map::new())),
            interactions: Mutex::new(HashSet::new()),
            last_published: Mutex::new(None),
        }
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    /// Register the interaction names commands may target (pydoover
    /// `_set_interactions`, names only).
    pub fn set_interactions(&self, names: impl IntoIterator<Item = String>) {
        *self.interactions.lock().unwrap() = names.into_iter().collect();
    }

    /// Process a `ui_cmds` event: aggregate updates refresh the cached
    /// values; message events that carry an RPC command for one of this app's
    /// interactions become a [`UiCommand`] for the runner to deliver.
    pub fn handle_event(&self, event: &Event) -> Option<UiCommand> {
        if event.is_aggregate_update() || event.is_channel_sync() {
            // pydoover `_on_aggregate_update`: values = aggregate.data[app_key]
            let values = event
                .aggregate_data()
                .and_then(|d| d.get(&self.app_key))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            *self.values.lock().unwrap() = values;
            return None;
        }
        if !(event.is_message_create() || event.is_one_shot()) {
            return None;
        }

        let data = event.message_data()?;
        if data.get("type").and_then(Value::as_str) != Some("rpc") {
            return None;
        }
        let method = data.get("method").and_then(Value::as_str)?;
        if let Some(target) = data.get("app_key").and_then(Value::as_str) {
            if target != self.app_key {
                return None;
            }
        }
        if !self.interactions.lock().unwrap().contains(method) {
            // No matching interaction — not our command (pydoover's
            // `_get_handler` KeyError path silently ignores it).
            return None;
        }
        let value = data.get("request")?.clone();
        let message_id = if event.is_one_shot() { None } else { event.message_id() };
        Some(UiCommand { name: method.to_string(), value, message_id })
    }

    /// The cached current value of an interaction from the `ui_cmds`
    /// aggregate (pydoover `UICommandsManager.get_value`, without the
    /// element-default fallback).
    pub fn get_value(&self, name: &str) -> Option<Value> {
        self.values.lock().unwrap().get(name).cloned()
    }

    /// Write an interaction value back into the `ui_cmds` aggregate, and
    /// (when `log_update`) record the pydoover
    /// `{"type": "log", "app_key", "key", "value"}` message
    /// (pydoover `UICommandsManager.set_value`).
    pub async fn set_value(&self, name: &str, value: Value, log_update: bool) -> Result<()> {
        let mut inner = Map::new();
        inner.insert(name.to_string(), value.clone());
        let mut outer = Map::new();
        outer.insert(self.app_key.clone(), Value::Object(inner));
        self.backend
            .update_channel_aggregate(
                UI_CMDS_CHANNEL,
                &Value::Object(outer),
                &AggregateOptions::default(),
            )
            .await?;
        if log_update {
            let log = json!({
                "type": "log",
                "app_key": self.app_key,
                "key": name,
                "value": value,
            });
            self.backend.create_message(UI_CMDS_CHANNEL, &log).await?;
        }
        Ok(())
    }

    /// Update the command's request message with pydoover's `success` status
    /// payload. One-shot commands have no message to respond to (no-op).
    pub(crate) async fn respond_success(&self, cmd: &UiCommand) -> Result<()> {
        let Some(id) = cmd.message_id else { return Ok(()) };
        let data = json!({
            "status": {"code": "success", "message": null},
            "response": {},
        });
        self.backend
            .update_message(UI_CMDS_CHANNEL, id, &data, &UpdateMessageOptions::default())
            .await
    }

    /// Update the command's request message with pydoover's `error` status
    /// payload.
    pub(crate) async fn respond_error(
        &self,
        cmd: &UiCommand,
        code: &str,
        message: &str,
    ) -> Result<()> {
        let Some(id) = cmd.message_id else { return Ok(()) };
        let data = json!({
            "status": {
                "code": "error",
                "message": {"code": code, "message": message},
            },
            "response": {},
        });
        self.backend
            .update_message(UI_CMDS_CHANNEL, id, &data, &UpdateMessageOptions::default())
            .await
    }

    /// Publish the (config-resolved) schema to `ui_state` if it changed since
    /// the last publish. Every publish is pydoover's double write — clear the
    /// app's subtree (`null`), then set it — so removed elements don't
    /// survive the aggregate merge. `max_age_secs=-1`, exactly as pydoover's
    /// `_setup`. Returns whether a publish happened.
    pub async fn publish_schema(&self, schema: &Value) -> Result<bool> {
        let serialized = serde_json::to_string(schema)?;
        if self.last_published.lock().unwrap().as_deref() == Some(&serialized) {
            return Ok(false);
        }

        let wrap = |inner: Value| {
            let mut children = Map::new();
            children.insert(self.app_key.clone(), inner);
            json!({"state": {"children": children}})
        };
        let opts = AggregateOptions { max_age_secs: -1.0, ..Default::default() };
        self.backend
            .update_channel_aggregate(UI_STATE_CHANNEL, &wrap(Value::Null), &opts)
            .await?;
        self.backend
            .update_channel_aggregate(UI_STATE_CHANNEL, &wrap(schema.clone()), &opts)
            .await?;

        *self.last_published.lock().unwrap() = Some(serialized);
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// $config.app() reference resolution (pydoover ui/declarative.py
// `_resolve_config_refs`)
// ---------------------------------------------------------------------------

/// Recursively resolve `$config.app().KEY[:type[:default]]` references in a
/// UI schema against the deployment config object — the runtime half of
/// pydoover `UI.to_schema(resolve_config=True)`.
pub fn resolve_config_refs(value: &Value, config: &Value) -> Value {
    match value {
        Value::Object(m) => Value::Object(
            m.iter().map(|(k, v)| (k.clone(), resolve_config_refs(v, config))).collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve_config_refs(v, config)).collect())
        }
        Value::String(s) if s.contains("$config.app().") => resolve_single_ref(s, config),
        other => other.clone(),
    }
}

/// Hand-rolled equivalent of pydoover's
/// `re.fullmatch(r"\$config\.app\(\)\.(\w+)(?::(\w+))?(?::(.+))?", value)`:
/// a non-matching string is returned unchanged.
fn resolve_single_ref(s: &str, config: &Value) -> Value {
    let unchanged = || Value::String(s.to_string());
    let Some(rest) = s.strip_prefix("$config.app().") else { return unchanged() };

    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let key_len = rest.chars().take_while(|c| is_word(*c)).count();
    if key_len == 0 {
        return unchanged();
    }
    let (key, tail) = rest.split_at(key_len);

    let (type_hint, default): (Option<&str>, Option<&str>) = if tail.is_empty() {
        (None, None)
    } else if let Some(t) = tail.strip_prefix(':') {
        let th_len = t.chars().take_while(|c| is_word(*c)).count();
        if th_len == 0 {
            return unchanged();
        }
        let (th, rest2) = t.split_at(th_len);
        if rest2.is_empty() {
            (Some(th), None)
        } else if let Some(d) = rest2.strip_prefix(':') {
            if d.is_empty() {
                return unchanged(); // regex `(.+)` needs at least one char
            }
            // `.+` is greedy: the whole remainder — including any further
            // colons (the doubled ":boolean:false:boolean:false" quirk) —
            // is the default.
            (Some(th), Some(d))
        } else {
            return unchanged();
        }
    } else {
        return unchanged();
    };

    // raw = config value, else the (string) default from the reference.
    let raw: Option<Value> = match config.get(key) {
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => default.map(|d| Value::String(d.to_string())),
    };
    let Some(raw) = raw else { return Value::Null };

    match type_hint {
        Some("boolean") => match &raw {
            Value::Bool(b) => Value::Bool(*b),
            other => {
                let s = python_string_of(other).to_lowercase();
                Value::Bool(matches!(s.as_str(), "true" | "1" | "yes"))
            }
        },
        Some("number") => match &raw {
            Value::Number(n) => Value::Number(n.clone()),
            other => {
                // pydoover: float(raw) if "." in str(raw) else int(raw),
                // falling back to raw on failure.
                let s = python_string_of(other);
                if s.contains('.') {
                    s.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map_or(raw.clone(), Value::Number)
                } else {
                    s.parse::<i64>().map_or(raw.clone(), Value::from)
                }
            }
        },
        Some("string") => Value::String(python_string_of(&raw)),
        _ => raw,
    }
}

/// Python `str()` of a JSON scalar, for the coercion paths above.
fn python_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_and_typed_refs() {
        let config = json!({
            "APP_DISPLAY_NAME": "Level Sensor",
            "APP_KEY": "my_app",
            "dv_app_position": 42,
        });
        assert_eq!(
            resolve_single_ref("$config.app().APP_DISPLAY_NAME", &config),
            json!("Level Sensor")
        );
        assert_eq!(resolve_single_ref("$config.app().APP_KEY", &config), json!("my_app"));
        assert_eq!(
            resolve_single_ref("$config.app().dv_app_position:number:100", &config),
            json!(42)
        );
        // absent key -> reference default, coerced
        assert_eq!(
            resolve_single_ref("$config.app().other_position:number:100", &config),
            json!(100)
        );
        assert_eq!(
            resolve_single_ref("$config.app().scale:number:1.5", &json!({})),
            json!(1.5)
        );
    }

    #[test]
    fn resolves_doubled_boolean_quirk() {
        // The exported default carries pydoover's doubled suffix; the greedy
        // `.+` default group swallows it and the boolean coercion yields false.
        assert_eq!(
            resolve_single_ref(
                "$config.app().hidden:boolean:false:boolean:false",
                &json!({})
            ),
            json!(false)
        );
        assert_eq!(
            resolve_single_ref(
                "$config.app().hidden:boolean:false:boolean:false",
                &json!({"hidden": true})
            ),
            json!(true)
        );
    }

    #[test]
    fn missing_without_default_is_null() {
        assert_eq!(
            resolve_single_ref("$config.app().dv_app_default_open:boolean", &json!({})),
            Value::Null
        );
    }

    #[test]
    fn non_matching_strings_unchanged() {
        for s in [
            "$tag.app().level:number:null",
            "$cmds.app().pump",
            "prefix $config.app().x",
            "$config.app().",
        ] {
            assert_eq!(resolve_single_ref(s, &json!({})), json!(s), "{s}");
            assert_eq!(resolve_config_refs(&json!(s), &json!({})), json!(s), "{s}");
        }
    }

    #[test]
    fn resolves_recursively() {
        let schema = json!({
            "displayString": "$config.app().APP_DISPLAY_NAME",
            "children": {"a": {"hidden": "$config.app().hidden:boolean:false:boolean:false"}},
        });
        let resolved = resolve_config_refs(&schema, &json!({"APP_DISPLAY_NAME": "X"}));
        assert_eq!(resolved["displayString"], json!("X"));
        assert_eq!(resolved["children"]["a"]["hidden"], json!(false));
    }

    #[test]
    fn ui_command_matching() {
        let cmd = UiCommand { name: "pump".into(), value: json!(true), message_id: Some(1) };
        assert_eq!(cmd.value_as::<bool>(), Some(true));
        assert_eq!(cmd.value_as::<f64>(), None);
    }
}
