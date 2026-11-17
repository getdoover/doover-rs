//! [`UiValue`] — a UI value slot: literal JSON, a `$tag.app()` reference, a
//! `$cmds.app()` reference, or missing (pydoover `NotSet`). Covers what
//! pydoover's `normalize_ui_value` produces for element `value` attributes.

use serde_json::Value;

use crate::tags::{tag_ref_lookup, ui_tag_type, Tag, TagValue};

/// The value slot of a UI element (`currentValue` on the wire).
#[derive(Debug, Clone, PartialEq)]
pub enum UiValue {
    /// pydoover `NotSet` — the key is omitted entirely.
    Missing,
    /// A literal JSON value (`Lit(Value::Null)` emits `"currentValue": null`,
    /// matching pydoover's `value=None`).
    Lit(Value),
    /// A tag reference, serialized as `$tag.app().<name>:<type>[:<default>]`
    /// (pydoover `UITagBinding`). `default` of `None` is pydoover's
    /// `_MISSING`; `Some(Value::Null)` is a declared `default=None` → `:null`.
    TagRef {
        name: String,
        tag_type: Option<String>,
        default: Option<Value>,
        /// Mirror of the underlying tag's `live=True` flag — variables emit
        /// `"live": true` when their value is a live tag reference.
        live: bool,
    },
    /// A `ui_cmds` reference (`$cmds.app().<name>`), the default
    /// `currentValue` of interactions.
    CmdsRef(String),
}

impl UiValue {
    pub fn is_missing(&self) -> bool {
        matches!(self, UiValue::Missing)
    }

    /// Whether this is a tag reference to a `live=True` tag
    /// (pydoover `_value_is_live`).
    pub fn is_live(&self) -> bool {
        matches!(self, UiValue::TagRef { live: true, .. })
    }

    /// The serialized JSON value, or `None` when [`Missing`](UiValue::Missing)
    /// (the caller omits the key — pydoover's NotSet filter).
    pub fn to_json(&self) -> Option<Value> {
        match self {
            UiValue::Missing => None,
            UiValue::Lit(v) => Some(v.clone()),
            UiValue::TagRef { name, tag_type, default, .. } => Some(Value::String(
                tag_ref_lookup(name, tag_type.as_deref(), default.as_ref()),
            )),
            UiValue::CmdsRef(s) => Some(Value::String(s.clone())),
        }
    }
}

/// Capture a tag reference from a declared handle — pydoover
/// `_binding_from_tag`: name, (UI-mapped) type, default and live flag.
impl<T: TagValue> From<&Tag<T>> for UiValue {
    fn from(tag: &Tag<T>) -> Self {
        UiValue::TagRef {
            name: tag.name().to_string(),
            tag_type: Some(ui_tag_type(T::TAG_TYPE).to_string()),
            default: tag.default().cloned(),
            live: tag.is_live(),
        }
    }
}

impl From<Value> for UiValue {
    fn from(v: Value) -> Self {
        UiValue::Lit(v)
    }
}

impl From<f64> for UiValue {
    fn from(v: f64) -> Self {
        UiValue::Lit(Value::from(v))
    }
}

impl From<i64> for UiValue {
    fn from(v: i64) -> Self {
        UiValue::Lit(Value::from(v))
    }
}

impl From<bool> for UiValue {
    fn from(v: bool) -> Self {
        UiValue::Lit(Value::Bool(v))
    }
}

impl From<&str> for UiValue {
    fn from(v: &str) -> Self {
        UiValue::Lit(Value::String(v.to_string()))
    }
}

impl From<String> for UiValue {
    fn from(v: String) -> Self {
        UiValue::Lit(Value::String(v))
    }
}

/// Python `str()` of a JSON value — used for the `::<default>` suffix of an
/// interaction's `$cmds.app()` reference, which pydoover builds with an
/// f-string (NOT `json.dumps`): `True`/`False`/`None`, bare strings, and
/// number reprs.
pub(crate) fn python_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        // serde_json's Display keeps ints bare and floats with a decimal
        // point — matching Python repr for typical values.
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Containers as interaction defaults are not meaningfully portable
        // (Python str() of dict/list uses single quotes); compact JSON is
        // the closest stable form.
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tag_ref_serializes_like_pydoover() {
        let v = UiValue::TagRef {
            name: "level_filled_percentage".into(),
            tag_type: Some("number".into()),
            default: Some(Value::Null),
            live: true,
        };
        assert!(v.is_live());
        assert_eq!(v.to_json(), Some(json!("$tag.app().level_filled_percentage:number:null")));
    }

    #[test]
    fn missing_and_literals() {
        assert_eq!(UiValue::Missing.to_json(), None);
        assert_eq!(UiValue::from(1.5).to_json(), Some(json!(1.5)));
        assert_eq!(UiValue::Lit(Value::Null).to_json(), Some(Value::Null));
        assert!(!UiValue::from(true).is_live());
    }

    #[test]
    fn python_str_forms() {
        assert_eq!(python_str(&json!(true)), "True");
        assert_eq!(python_str(&json!(false)), "False");
        assert_eq!(python_str(&Value::Null), "None");
        assert_eq!(python_str(&json!("on")), "on");
        assert_eq!(python_str(&json!(5)), "5");
        assert_eq!(python_str(&json!(2.5)), "2.5");
    }
}
