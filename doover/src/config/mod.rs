//! App configuration.
//!
//! Two layers, mirroring pydoover's `config` package:
//!
//! - **Dynamic** access ([`Config`]): the deployment config as raw JSON
//!   (from `CONFIG_FP` in dev, the `deployment_config` channel in prod) with
//!   `get`/`get_str`/… accessors.
//! - **Declarative** schemas: a runtime model ([`SchemaModel`] /
//!   [`ElementSchema`] in [`schema`]) that owns byte-exact JSON Schema
//!   emission matching `pydoover/config/__init__.py`, the
//!   `doover_config.json` merge-writer in [`export`], and the
//!   [`ConfigSchema`] trait implemented by `#[derive(Config)]` (from the
//!   `doover-macros` crate) for typed schema declaration + loading.

pub mod export;
pub mod schema;

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{DooverError, Result};

pub use export::{write_config_schema, write_ui_schema};
pub use schema::{ElementKind, ElementSchema, NumericBounds, SchemaModel, StringBounds};

// ---------------------------------------------------------------------------
// Dynamic config (raw JSON access)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Declarative config traits
// ---------------------------------------------------------------------------

/// A typed application config: owns the schema (for `doover_config.json`
/// export) and typed loading from a deployment config value. Implemented by
/// `#[derive(Config)]`; implement by hand for fully dynamic schemas.
pub trait ConfigSchema {
    /// The config schema, as exported to `doover_config.json`.
    fn schema() -> SchemaModel;

    /// Load a typed config from the deployment config JSON object.
    fn from_value(v: &Value) -> Result<Self>
    where
        Self: Sized;
}

/// Config-less apps.
impl ConfigSchema for () {
    fn schema() -> SchemaModel {
        SchemaModel::new()
    }

    fn from_value(_v: &Value) -> Result<Self> {
        Ok(())
    }
}

/// Builds the [`ElementSchema`] for a field of this type. Implemented for
/// the scalar types, by `#[derive(ConfigObject)]` / `#[derive(ConfigEnum)]`,
/// and by the doover marker types ([`ApplicationPosition`], …) — which may
/// override the passed title/name, exactly like their pydoover counterparts
/// hard-code `name="dv_app_position"` etc.
pub trait ConfigElementBuild {
    fn element(title: &str, name: &str) -> ElementSchema;
}

/// Parse a typed value out of one deployment-config element.
pub trait FromConfigValue: Sized {
    fn from_config_value(v: &Value) -> Result<Self>;
}

/// Convert a Rust default into the JSON value stored in the schema.
/// Int/float distinction is preserved: `0i64` emits `0`, `4.0f64` emits `4.0`.
pub trait ToConfigValue {
    fn to_config_value(&self) -> Value;
}

macro_rules! impl_scalar {
    ($($ty:ty => $ctor:ident),* $(,)?) => {$(
        impl ConfigElementBuild for $ty {
            fn element(title: &str, name: &str) -> ElementSchema {
                ElementSchema::$ctor(title, name)
            }
        }
        impl FromConfigValue for $ty {
            fn from_config_value(v: &Value) -> Result<Self> {
                serde_json::from_value(v.clone()).map_err(|e| {
                    DooverError::Other(format!(
                        "expected {}, got {v}: {e}",
                        stringify!($ty)
                    ))
                })
            }
        }
        impl ToConfigValue for $ty {
            fn to_config_value(&self) -> Value {
                Value::from(self.clone())
            }
        }
    )*};
}

impl_scalar! {
    i8 => integer, i16 => integer, i32 => integer, i64 => integer,
    u8 => integer, u16 => integer, u32 => integer,
    f32 => number, f64 => number,
    bool => boolean,
    String => string,
}

impl ToConfigValue for str {
    fn to_config_value(&self) -> Value {
        Value::String(self.to_string())
    }
}

impl<T: FromConfigValue> FromConfigValue for Option<T> {
    fn from_config_value(v: &Value) -> Result<Self> {
        if v.is_null() {
            Ok(None)
        } else {
            T::from_config_value(v).map(Some)
        }
    }
}

impl<T: FromConfigValue> FromConfigValue for Vec<T> {
    fn from_config_value(v: &Value) -> Result<Self> {
        let arr = v
            .as_array()
            .ok_or_else(|| DooverError::Other(format!("expected array, got {v}")))?;
        arr.iter().map(T::from_config_value).collect()
    }
}

impl<T: ToConfigValue> ToConfigValue for Vec<T> {
    fn to_config_value(&self) -> Value {
        Value::Array(self.iter().map(ToConfigValue::to_config_value).collect())
    }
}

// ---------------------------------------------------------------------------
// Loading helpers
// ---------------------------------------------------------------------------

/// Load a required field: error naming the key when absent
/// (pydoover raises "Required config element X not found in deployment config").
pub fn field_required<T: FromConfigValue>(v: &Value, key: &str) -> Result<T> {
    match v.get(key) {
        Some(val) => T::from_config_value(val)
            .map_err(|e| DooverError::Other(format!("config element '{key}': {e}"))),
        None => Err(DooverError::Other(format!(
            "required config element '{key}' not found in deployment config"
        ))),
    }
}

/// Load a field, falling back to `default` when the key is absent OR null.
pub fn field_or<T: FromConfigValue>(v: &Value, key: &str, default: T) -> Result<T> {
    match v.get(key) {
        Some(val) if !val.is_null() => T::from_config_value(val)
            .map_err(|e| DooverError::Other(format!("config element '{key}': {e}"))),
        _ => Ok(default),
    }
}

/// Load an optional field: `None` when the key is absent or null.
pub fn field_optional<T: FromConfigValue>(v: &Value, key: &str) -> Result<Option<T>> {
    match v.get(key) {
        Some(val) if !val.is_null() => T::from_config_value(val)
            .map(Some)
            .map_err(|e| DooverError::Other(format!("config element '{key}': {e}"))),
        _ => Ok(None),
    }
}

/// Load one field per its [`ElementSchema`]: required-missing is an error;
/// otherwise the schema default applies when the key is absent or null.
/// This is the workhorse behind `#[derive(Config)]`'s `from_value`.
pub fn load_element<T: FromConfigValue>(v: &Value, el: &ElementSchema) -> Result<T> {
    let key = el.name.as_str();
    match v.get(key) {
        Some(val) if !val.is_null() => T::from_config_value(val)
            .map_err(|e| DooverError::Other(format!("config element '{key}': {e}"))),
        Some(_) | None if !el.is_required() => {
            // absent or null: fall back to the declared default (a missing
            // default means the element is optional-with-null, e.g. Option<T>)
            let default = el.default.clone().unwrap_or(Value::Null);
            T::from_config_value(&default)
                .map_err(|e| DooverError::Other(format!("config element '{key}' default: {e}")))
        }
        Some(val) => {
            // required but explicitly null in the config file
            T::from_config_value(val)
                .map_err(|e| DooverError::Other(format!("config element '{key}': {e}")))
        }
        None => Err(DooverError::Other(format!(
            "required config element '{key}' not found in deployment config"
        ))),
    }
}

/// Port of pydoover `utils.sanitize_display_name`: spaces become
/// underscores, everything outside `[0-9a-zA-Z_]` is dropped, and the result
/// is lowercased. This derives the `x-name` (and JSON key) from a display
/// title — e.g. `"Sensor Minimum mA"` → `"sensor_minimum_ma"`.
pub fn sanitize_display_name(name: &str) -> String {
    name.chars()
        .filter_map(|c| match c {
            ' ' => Some('_'),
            c if c.is_ascii_alphanumeric() || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Doover-specific marker types (pydoover config/__init__.py helper elements)
// ---------------------------------------------------------------------------

/// pydoover `config.ApplicationPosition()` — a hidden integer that always
/// exports under the key `dv_app_position` (the Rust field name does NOT
/// control the key, matching Python where any attribute name maps to
/// `name="dv_app_position"`), with default `100` and `minimum: 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationPosition(pub i64);

impl Default for ApplicationPosition {
    fn default() -> Self {
        Self(100)
    }
}

impl ConfigElementBuild for ApplicationPosition {
    fn element(_title: &str, _name: &str) -> ElementSchema {
        let mut el = ElementSchema::integer("Position", "dv_app_position");
        el.hidden = true;
        el.description = Some(
            "Position of Application in UI Structure. Smaller numbers are closer to the top."
                .into(),
        );
        el.default = Some(Value::from(100));
        el.set_minimum(serde_json::Number::from(0));
        el
    }
}

impl FromConfigValue for ApplicationPosition {
    fn from_config_value(v: &Value) -> Result<Self> {
        i64::from_config_value(v).map(Self)
    }
}

/// pydoover `config.ApplicationDefaultOpen()` — a hidden boolean under
/// `dv_app_default_open` with `default: null` (unset means "dynamic on the
/// number of apps installed").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplicationDefaultOpen(pub Option<bool>);

impl ConfigElementBuild for ApplicationDefaultOpen {
    fn element(_title: &str, _name: &str) -> ElementSchema {
        let mut el = ElementSchema::boolean("Default Open", "dv_app_default_open");
        el.hidden = true;
        el.description = Some(
            "Whether the application is default open in the UI. By default this is not set - \
             which makes it dynamic on the number of apps installed."
                .into(),
        );
        el.default = Some(Value::Null);
        el
    }
}

impl FromConfigValue for ApplicationDefaultOpen {
    fn from_config_value(v: &Value) -> Result<Self> {
        Option::<bool>::from_config_value(v).map(Self)
    }
}

/// pydoover `config.Application` — a string element rendered as an
/// application picker (`format: doover-resource-application`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationRef(pub String);

impl ConfigElementBuild for ApplicationRef {
    fn element(title: &str, name: &str) -> ElementSchema {
        let mut el = ElementSchema::string(title, name);
        el.description = Some("Application".into());
        el.format = Some("doover-resource-application".into());
        el
    }
}

impl FromConfigValue for ApplicationRef {
    fn from_config_value(v: &Value) -> Result<Self> {
        String::from_config_value(v).map(Self)
    }
}

/// pydoover `config.Device` — a string element rendered as a device picker
/// (`pattern: \d+`, `format: doover-resource-device`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceRef(pub String);

impl ConfigElementBuild for DeviceRef {
    fn element(title: &str, name: &str) -> ElementSchema {
        let mut el = ElementSchema::string(title, name);
        el.description = Some("Device ID".into());
        el.format = Some("doover-resource-device".into());
        el.set_pattern(r"\d+");
        el
    }
}

impl FromConfigValue for DeviceRef {
    fn from_config_value(v: &Value) -> Result<Self> {
        String::from_config_value(v).map(Self)
    }
}

/// pydoover `config.Group` — a string element rendered as a group picker
/// (`pattern: \d+`, `format: doover-resource-group`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupRef(pub String);

impl ConfigElementBuild for GroupRef {
    fn element(title: &str, name: &str) -> ElementSchema {
        let mut el = ElementSchema::string(title, name);
        el.description = Some("Group ID".into());
        el.format = Some("doover-resource-group".into());
        el.set_pattern(r"\d+");
        el
    }
}

impl FromConfigValue for GroupRef {
    fn from_config_value(v: &Value) -> Result<Self> {
        String::from_config_value(v).map(Self)
    }
}

// TODO: port pydoover's `config.TagRef` (an Object of reference_name /
// agent_id / app_name / tag_name with `format: doover-tag-reference`) once
// RemoteTag/cross-app tag resolution lands in the tags runtime — its schema
// shape depends on `ApplicationInstall` and only makes sense with that
// runtime support. `config.ApplicationInstall`, `config.DevicesConfig`,
// `config.GroupsConfig` and `config.LLMAPIKey` are similarly deferred.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_matches_pydoover() {
        assert_eq!(sanitize_display_name("Sensor Minimum mA"), "sensor_minimum_ma");
        assert_eq!(sanitize_display_name("AI Pin"), "ai_pin");
        assert_eq!(sanitize_display_name("Volume Curve Point"), "volume_curve_point");
        assert_eq!(sanitize_display_name("weird-Name (v2)!"), "weirdname_v2");
    }

    #[test]
    fn application_position_element_matches_pydoover() {
        let el = ApplicationPosition::element("ignored", "ignored");
        assert_eq!(
            serde_json::to_string(&el.to_json()).unwrap(),
            r#"{"title":"Position","x-name":"dv_app_position","x-hidden":true,"type":["integer","null"],"x-required":false,"description":"Position of Application in UI Structure. Smaller numbers are closer to the top.","default":100,"minimum":0}"#
        );
    }

    #[test]
    fn load_element_defaults_and_required() {
        let mut el = ElementSchema::number("Polling Frequency", "polling_frequency");
        el.default = Some(json!(1.0));
        // absent -> default
        assert_eq!(load_element::<f64>(&json!({}), &el).unwrap(), 1.0);
        // null -> default
        assert_eq!(load_element::<f64>(&json!({"polling_frequency": null}), &el).unwrap(), 1.0);
        // present -> value (integers accepted for f64)
        assert_eq!(load_element::<f64>(&json!({"polling_frequency": 2}), &el).unwrap(), 2.0);

        let req = ElementSchema::integer("AI Pin", "ai_pin");
        let err = load_element::<i64>(&json!({}), &req).unwrap_err();
        assert!(err.to_string().contains("required config element 'ai_pin'"), "{err}");
    }

    #[test]
    fn field_helpers() {
        let v = json!({"a": 3, "b": null});
        assert_eq!(field_required::<i64>(&v, "a").unwrap(), 3);
        assert!(field_required::<i64>(&v, "missing").is_err());
        assert_eq!(field_or::<i64>(&v, "b", 7).unwrap(), 7);
        assert_eq!(field_or::<i64>(&v, "missing", 7).unwrap(), 7);
        assert_eq!(field_optional::<i64>(&v, "b").unwrap(), None);
        assert_eq!(field_optional::<i64>(&v, "a").unwrap(), Some(3));
    }
}
