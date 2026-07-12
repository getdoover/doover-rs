//! Typed tag handles — the declarative half of `pydoover.tags`.
//!
//! A [`Tag<T>`] is the Rust counterpart of a pydoover `Tag` declaration
//! (`Tag("number", default=None, live=True)`) *and* its runtime `BoundTag`
//! proxy: detached it only knows its declaration (name / type / default /
//! live), attached to a [`TagsRuntime`] its `get`/`set` calls route through
//! the buffered tag manager scoped under the runtime's app key.
//!
//! `#[derive(Tags)]` (from `doover-macros`) turns a struct of `Tag<T>`
//! fields into a [`TagsCollection`] — the counterpart of a pydoover `Tags`
//! subclass — where the **field name is the tag name**.
//!
//! `log_on` triggers ([`LogTrigger`](super::LogTrigger)) declared on a tag
//! — via [`Tag::with_log_on`] or the `#[tag(log_on(...))]` attribute — are
//! registered with the [`TagsRuntime`] when the collection is attached and
//! evaluated on every `set`, exactly like pydoover `Tags._set_tag_value`.

use std::marker::PhantomData;
use std::sync::Arc;

use serde_json::Value;

use crate::error::{DooverError, Result};

use super::triggers::validate_log_on;
use super::{LogTrigger, SetTagOptions, TagsRuntime};

/// A Rust type that can live in a tag slot, with its pydoover tag-type
/// string (the `tag_type` argument of a pydoover `Tag` declaration).
pub trait TagValue: Sized {
    /// The declared tag type (`"number"`, `"integer"`, `"boolean"`,
    /// `"string"`, `"object"` — see pydoover `tags.Tag`).
    const TAG_TYPE: &'static str;

    fn to_value(&self) -> Value;
    fn from_value(v: &Value) -> Option<Self>;
}

impl TagValue for f64 {
    const TAG_TYPE: &'static str = "number";

    fn to_value(&self) -> Value {
        Value::from(*self)
    }

    fn from_value(v: &Value) -> Option<Self> {
        v.as_f64()
    }
}

impl TagValue for i64 {
    const TAG_TYPE: &'static str = "integer";

    fn to_value(&self) -> Value {
        Value::from(*self)
    }

    fn from_value(v: &Value) -> Option<Self> {
        // JSON doesn't distinguish int from float; accept whole floats like
        // pydoover `_coerce_tag_value` does for "integer" tags.
        v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
    }
}

impl TagValue for bool {
    const TAG_TYPE: &'static str = "boolean";

    fn to_value(&self) -> Value {
        Value::Bool(*self)
    }

    fn from_value(v: &Value) -> Option<Self> {
        v.as_bool()
    }
}

impl TagValue for String {
    const TAG_TYPE: &'static str = "string";

    fn to_value(&self) -> Value {
        Value::String(self.clone())
    }

    fn from_value(v: &Value) -> Option<Self> {
        v.as_str().map(str::to_string)
    }
}

/// Arbitrary JSON — pydoover `Tag("object")`.
impl TagValue for Value {
    const TAG_TYPE: &'static str = "object";

    fn to_value(&self) -> Value {
        self.clone()
    }

    fn from_value(v: &Value) -> Option<Self> {
        Some(v.clone())
    }
}

/// Map a declared tag type to the type name used in UI tag references —
/// pydoover `ui/declarative.py` `_TAG_TYPE_MAP` (applied by
/// `UITagBinding.__init__`, so `"integer"` tags reference as `"number"`).
pub fn ui_tag_type(tag_type: &str) -> &str {
    match tag_type {
        "integer" | "float" => "number",
        "bool" => "boolean",
        "list" => "array",
        "dict" => "object",
        other => other,
    }
}

/// Build the `$tag.app().<name>[:<type>[:<default>]]` reference string —
/// pydoover `UITagBinding.to_lookup()`. `default` of `None` is pydoover's
/// `_MISSING` (no default declared); `Some(Value::Null)` is a declared
/// `default=None` and serializes as `:null`.
pub(crate) fn tag_ref_lookup(name: &str, tag_type: Option<&str>, default: Option<&Value>) -> String {
    let mut result = format!("$tag.app().{name}");
    if let Some(ty) = tag_type {
        result.push(':');
        result.push_str(ty);
    }
    if let Some(default) = default {
        if tag_type.is_none() {
            result.push_str(":string");
        }
        // Python `json.dumps(value, separators=(',', ':'))` — serde_json's
        // compact form matches.
        result.push(':');
        result.push_str(&serde_json::to_string(default).expect("tag default serializes"));
    }
    result
}

/// A typed tag handle — a pydoover `Tag` declaration plus its runtime
/// `BoundTag` proxy (see the module-level docs above for the mapping).
pub struct Tag<T> {
    name: &'static str,
    live: bool,
    /// `None` == pydoover `NotSet` (no declared default); `Some(Value::Null)`
    /// == a declared `default=None`.
    default: Option<Value>,
    /// Auto-logging rules (pydoover `log_on=`), registered with the runtime
    /// on attach.
    log_on: Vec<LogTrigger>,
    runtime: Option<Arc<TagsRuntime>>,
    // `fn() -> T` keeps Tag<T> Send + Sync without requiring it of T.
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Tag<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            live: self.live,
            default: self.default.clone(),
            log_on: self.log_on.clone(),
            runtime: self.runtime.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Tag<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tag")
            .field("name", &self.name)
            .field("live", &self.live)
            .field("default", &self.default)
            .field("log_on", &self.log_on)
            .field("attached", &self.runtime.is_some())
            .finish()
    }
}

impl<T: TagValue> Tag<T> {
    /// Declare a detached tag (pydoover `Tag(tag_type, default=…, live=…)`;
    /// the type comes from `T`). Used by `#[derive(Tags)]`.
    pub fn declared(name: &'static str, live: bool, default: Option<Value>) -> Self {
        Self { name, live, default, log_on: Vec::new(), runtime: None, _marker: PhantomData }
    }

    /// Declare auto-logging rules (pydoover `log_on=`): when any trigger
    /// fires on a `set`, the update is promoted to an immediate logged data
    /// point. Panics on triggers invalid for this tag's type, exactly like
    /// pydoover's declaration-time `TypeError` (numeric tags accept
    /// Cross/Rise/Fall/Delta; boolean/string tags accept
    /// AnyChange/Enter/Exit).
    pub fn with_log_on(mut self, triggers: Vec<LogTrigger>) -> Self {
        validate_log_on(T::TAG_TYPE, &triggers);
        self.log_on = triggers;
        self
    }

    /// The declared `log_on` triggers.
    pub fn log_on(&self) -> &[LogTrigger] {
        &self.log_on
    }

    /// Bind this declaration to a runtime (pydoover binding a `Tags`
    /// instance to its manager), registering any `log_on` triggers for
    /// evaluation inside the runtime's set path.
    pub fn attached(mut self, runtime: Arc<TagsRuntime>) -> Self {
        if !self.log_on.is_empty() {
            runtime.register_log_triggers(
                runtime.app_key(),
                self.name,
                self.log_on.clone(),
                self.default.clone(),
            );
        }
        self.runtime = Some(runtime);
        self
    }

    /// The tag name — the key inside this app's `tag_values` namespace.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Whether this tag was declared `live=True` (republished as a one-shot
    /// message each loop iteration while a user watches it).
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The declared tag type (pydoover `Tag.tag_type`).
    pub fn tag_type(&self) -> &'static str {
        T::TAG_TYPE
    }

    /// The declared default, if any (`Some(Value::Null)` == `default=None`).
    pub fn default(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    /// The `$tag.app().<name>:<type>[:<default>]` string this tag serializes
    /// to when referenced from a UI element (pydoover
    /// `UITagBinding.to_lookup()` via `_binding_from_tag`).
    pub fn ui_reference(&self) -> String {
        tag_ref_lookup(self.name, Some(ui_tag_type(T::TAG_TYPE)), self.default.as_ref())
    }

    /// Current value from the runtime's cached+pending state, falling back
    /// to the declared default (pydoover `BoundTag.get`). Detached handles
    /// only see the default.
    pub fn get(&self) -> Option<T> {
        let raw = self.runtime.as_ref().and_then(|rt| rt.get_tag(rt.app_key(), self.name));
        match raw {
            Some(v) if !v.is_null() => T::from_value(&v),
            _ => self.default.as_ref().filter(|v| !v.is_null()).and_then(T::from_value),
        }
    }

    /// Buffer a tag write, flushed at the end of the loop iteration
    /// (pydoover `BoundTag.set(value)`).
    pub async fn set(&self, value: T) -> Result<()> {
        self.set_with(value, &SetTagOptions::default()).await
    }

    /// Like [`set`](Self::set), additionally recording the update as a
    /// logged data point at the end of this loop rather than waiting for the
    /// periodic log flush (pydoover `BoundTag.set(value, log=True)`).
    pub async fn set_logged(&self, value: T) -> Result<()> {
        self.set_with(value, &SetTagOptions { log: true, ..Default::default() }).await
    }

    async fn set_with(&self, value: T, opts: &SetTagOptions) -> Result<()> {
        let rt = self
            .runtime
            .as_ref()
            .ok_or_else(|| DooverError::Other(format!("tag '{}': tags not attached", self.name)))?;
        rt.set_tag(rt.app_key(), self.name, value.to_value(), opts).await
    }
}

/// A declarative collection of tags — the counterpart of a pydoover `Tags`
/// subclass. Implemented by `#[derive(Tags)]` on a struct of [`Tag<T>`]
/// fields, where the field name is the tag name.
pub trait TagsCollection: Sized {
    /// Bind every declared tag to the runtime.
    fn attach(runtime: Arc<TagsRuntime>) -> Self;

    /// Declaration-only instance (for UI schema export — `set` errors).
    fn detached() -> Self;

    /// Every declared tag name, in declaration order.
    fn tag_names() -> Vec<&'static str>;

    /// The names of `live=True` tags, in declaration order (pydoover
    /// `Tags.get_live_tag_keys`, names only — the runtime scopes them
    /// under its own app key).
    fn live_tag_names() -> Vec<&'static str>;
}

/// Tag-less apps (`type Tags = ()`).
impl TagsCollection for () {
    fn attach(_runtime: Arc<TagsRuntime>) -> Self {}

    fn detached() -> Self {}

    fn tag_names() -> Vec<&'static str> {
        Vec::new()
    }

    fn live_tag_names() -> Vec<&'static str> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lookup_string_matches_pydoover() {
        // Tag("number", default=None) → "$tag.app().x:number:null"
        assert_eq!(
            tag_ref_lookup("x", Some("number"), Some(&Value::Null)),
            "$tag.app().x:number:null"
        );
        // no default → no trailing segment
        assert_eq!(tag_ref_lookup("x", Some("number"), None), "$tag.app().x:number");
        // no type but a default → ":string" injected first
        assert_eq!(tag_ref_lookup("x", None, Some(&json!(5))), "$tag.app().x:string:5");
        // compact JSON default (Python separators=(',', ':'))
        assert_eq!(
            tag_ref_lookup("x", Some("object"), Some(&json!({"a": 1, "b": [1, 2]}))),
            r#"$tag.app().x:object:{"a":1,"b":[1,2]}"#
        );
    }

    #[test]
    fn ui_tag_type_map() {
        assert_eq!(ui_tag_type("integer"), "number");
        assert_eq!(ui_tag_type("float"), "number");
        assert_eq!(ui_tag_type("number"), "number");
        assert_eq!(ui_tag_type("boolean"), "boolean");
        assert_eq!(ui_tag_type("string"), "string");
    }

    #[test]
    fn detached_tag_declaration() {
        let tag = Tag::<f64>::declared("level", true, Some(Value::Null));
        assert_eq!(tag.name(), "level");
        assert!(tag.is_live());
        assert_eq!(tag.tag_type(), "number");
        assert_eq!(tag.ui_reference(), "$tag.app().level:number:null");
        // default=None reads as no value
        assert_eq!(tag.get(), None);

        let with_default = Tag::<f64>::declared("speed", false, Some(json!(2.5)));
        assert_eq!(with_default.get(), Some(2.5));
        assert_eq!(with_default.ui_reference(), "$tag.app().speed:number:2.5");
    }

    #[tokio::test]
    async fn detached_set_errors() {
        let tag = Tag::<f64>::declared("level", false, None);
        let err = tag.set(1.0).await.unwrap_err();
        assert!(err.to_string().contains("tags not attached"), "{err}");
    }

    #[test]
    fn integer_tags_reference_as_number() {
        let tag = Tag::<i64>::declared("count", false, Some(json!(0)));
        assert_eq!(tag.tag_type(), "integer");
        // pydoover _TAG_TYPE_MAP maps integer → number in UI references
        assert_eq!(tag.ui_reference(), "$tag.app().count:number:0");
    }
}
