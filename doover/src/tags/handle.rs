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

use crate::config::TagRef;
use crate::error::{DooverError, Result};

use super::runtime::TagCallback;
use super::triggers::validate_log_on;
use super::{KeyPath, LogTrigger, SetTagOptions, TagsRuntime};

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
    /// Cross-app resolution: when set, this handle reads another app's
    /// namespace (`tag_values.<remote_app_key>.<name>`) instead of the
    /// runtime's own app key. Remote handles are read-only — see
    /// [`Tag::attached_remote`].
    remote_app_key: Option<Arc<str>>,
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
            remote_app_key: self.remote_app_key.clone(),
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
        Self {
            name,
            live,
            default,
            log_on: Vec::new(),
            runtime: None,
            remote_app_key: None,
            _marker: PhantomData,
        }
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

    /// Bind this declaration to a runtime but scoped to **another app's**
    /// namespace (`app_key`) — cross-app resolution. The resulting handle is
    /// read-only: [`get`](Self::get) reads `tag_values.<app_key>.<name>` from
    /// the runtime's live cache (the runtime already subscribes to the whole
    /// `tag_values` channel, so remote values stay fresh), and any
    /// [`set`](Self::set) errors rather than clobbering another app's tag.
    ///
    /// `log_on` triggers are *not* registered for remote tags — we don't own
    /// them, so we don't log their transitions.
    pub fn attached_remote(mut self, runtime: Arc<TagsRuntime>, app_key: impl Into<Arc<str>>) -> Self {
        self.remote_app_key = Some(app_key.into());
        self.runtime = Some(runtime);
        self
    }

    /// Whether this handle points at another app's namespace
    /// (cross-app / remote — read-only).
    pub fn is_remote(&self) -> bool {
        self.remote_app_key.is_some()
    }

    /// The app key this handle reads from: the explicit remote key when set,
    /// else the runtime's own app key.
    fn effective_app_key<'a>(&'a self, rt: &'a TagsRuntime) -> &'a str {
        self.remote_app_key.as_deref().unwrap_or_else(|| rt.app_key())
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
        let raw = self
            .runtime
            .as_ref()
            .and_then(|rt| rt.get_tag(self.effective_app_key(rt), self.name));
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
        if let Some(app_key) = &self.remote_app_key {
            return Err(DooverError::Other(format!(
                "tag '{}': remote (cross-app) tag on '{app_key}' is read-only",
                self.name
            )));
        }
        let rt = self
            .runtime
            .as_ref()
            .ok_or_else(|| DooverError::Other(format!("tag '{}': tags not attached", self.name)))?;
        rt.set_tag(rt.app_key(), self.name, value.to_value(), opts).await
    }
}

/// A typed, read-only handle on a tag published by **another** application —
/// the port of pydoover's `RemoteTag`. Its app key and tag name are resolved
/// at runtime (from a [`config::TagRef`](crate::config::TagRef) the operator
/// fills in, or supplied directly), so — unlike [`Tag<T>`] whose name is
/// fixed at compile time — the target can vary per deployment.
///
/// Reads come from the runtime's live `tag_values` cache: the runtime already
/// subscribes to the whole channel, so a `RemoteTag` on any app key stays
/// fresh without extra wiring. There is no `set` — you don't own another
/// app's tags.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use doover::tags::{RemoteTag, TagsRuntime};
/// # use doover::config::TagRef;
/// # fn demo(rt: Arc<TagsRuntime>, source: &TagRef) {
/// // `source` is a TagRef field on your #[derive(Config)] struct.
/// if let Some(level) = RemoteTag::<f64>::resolve(rt, source, None) {
///     let value = level.get(); // Option<f64> from the upstream app
///     # let _ = value;
/// }
/// # }
/// ```
pub struct RemoteTag<T> {
    app_key: String,
    tag_name: String,
    default: Option<Value>,
    runtime: Arc<TagsRuntime>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for RemoteTag<T> {
    fn clone(&self) -> Self {
        Self {
            app_key: self.app_key.clone(),
            tag_name: self.tag_name.clone(),
            default: self.default.clone(),
            runtime: self.runtime.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for RemoteTag<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTag")
            .field("app_key", &self.app_key)
            .field("tag_name", &self.tag_name)
            .field("default", &self.default)
            .finish()
    }
}

impl<T: TagValue> RemoteTag<T> {
    /// Bind to an explicit upstream `(app_key, tag_name)` — the cross-app
    /// resolution done by hand. Prefer [`resolve`](Self::resolve) when the
    /// target comes from a [`TagRef`](crate::config::TagRef) config element.
    pub fn from_parts(
        runtime: Arc<TagsRuntime>,
        app_key: impl Into<String>,
        tag_name: impl Into<String>,
        default: Option<Value>,
    ) -> Self {
        Self {
            app_key: app_key.into(),
            tag_name: tag_name.into(),
            default,
            runtime,
            _marker: PhantomData,
        }
    }

    /// Resolve a `TagRef` config binding into a live remote handle. Returns
    /// `None` when the operator hasn't pointed the reference at an app + tag
    /// yet (pydoover's `optional=True` remote tag) — callers treat that as
    /// "no upstream configured". Cross-*agent* references (a set `agent_id`)
    /// also resolve to `None`, mirroring pydoover's not-yet-implemented
    /// cross-agent path.
    pub fn resolve(runtime: Arc<TagsRuntime>, tag_ref: &TagRef, default: Option<Value>) -> Option<Self> {
        if tag_ref.agent_id.is_some() {
            tracing::warn!(
                "remote tag '{}': cross-agent references are not supported yet; ignoring",
                tag_ref.reference_name
            );
            return None;
        }
        let (app_key, tag_name) = tag_ref.target()?;
        Some(Self::from_parts(runtime, app_key, tag_name, default))
    }

    /// The resolved upstream app key.
    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    /// The resolved upstream tag name.
    pub fn tag_name(&self) -> &str {
        &self.tag_name
    }

    /// Current value from the upstream app's `tag_values` slot, falling back
    /// to the declared default (pydoover `RemoteTag.value`).
    pub fn get(&self) -> Option<T> {
        match self.runtime.get_tag(&self.app_key, &self.tag_name) {
            Some(v) if !v.is_null() => T::from_value(&v),
            _ => self.default.as_ref().filter(|v| !v.is_null()).and_then(T::from_value),
        }
    }

    /// Invoke `callback` with the decoded value whenever the upstream tag
    /// changes (built on [`TagsRuntime::subscribe_to_tag`]). A change that
    /// clears the tag delivers the declared default (or `None`).
    pub fn subscribe(&self, callback: impl Fn(Option<T>) + Send + Sync + 'static) {
        let default = self.default.clone();
        let cb: TagCallback = Arc::new(move |_path: &KeyPath, value: Option<&Value>| {
            let decoded = match value {
                Some(v) if !v.is_null() => T::from_value(v),
                _ => default.as_ref().filter(|v| !v.is_null()).and_then(T::from_value),
            };
            callback(decoded);
        });
        self.runtime.subscribe_to_tag(&self.app_key, &self.tag_name, cb);
    }

    /// Mirror the upstream tag into *this* app's namespace under `local_name`
    /// (pydoover `RemoteTag(republish_locally=True)`): seed the current value
    /// and re-publish every subsequent change, so this app's own UI/tags can
    /// consume it as a local tag. Best-effort; returns the seeded value.
    pub async fn republish_locally(&self, local_name: &'static str) -> Result<Option<T>> {
        let own_key = self.runtime.app_key().to_string();
        // Re-publish on every upstream change.
        let rt = self.runtime.clone();
        let key = own_key.clone();
        self.subscribe_raw(move |value| {
            if let Some(v) = value.cloned() {
                let rt = rt.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    let _ = rt.set_tag(&key, local_name, v, &SetTagOptions::default()).await;
                });
            }
        });
        // Seed the current value.
        let current = self.get();
        if let Some(v) = self.runtime.get_tag(&self.app_key, &self.tag_name) {
            if !v.is_null() {
                self.runtime.set_tag(&own_key, local_name, v, &SetTagOptions::default()).await?;
            }
        }
        Ok(current)
    }

    /// Subscribe with the raw JSON value (used by [`republish_locally`]).
    fn subscribe_raw(&self, callback: impl Fn(Option<&Value>) + Send + Sync + 'static) {
        let cb: TagCallback = Arc::new(move |_path: &KeyPath, value: Option<&Value>| callback(value));
        self.runtime.subscribe_to_tag(&self.app_key, &self.tag_name, cb);
    }
}

/// A declarative collection of tags — the counterpart of a pydoover `Tags`
/// subclass. Implemented by `#[derive(Tags)]` on a struct of [`Tag<T>`]
/// fields, where the field name is the tag name.
pub trait TagsCollection: Sized {
    /// Bind every declared tag to the runtime.
    fn attach(runtime: Arc<TagsRuntime>) -> Self;

    /// Bind every declared tag to the runtime but scoped to **another app's**
    /// key (`app_key`) — cross-app resolution for a whole known schema (e.g.
    /// reading another instance of the same `#[derive(Tags)]` type). The
    /// resulting handles are read-only; see [`Tag::attached_remote`].
    fn attach_remote(runtime: Arc<TagsRuntime>, app_key: &str) -> Self;

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

    fn attach_remote(_runtime: Arc<TagsRuntime>, _app_key: &str) -> Self {}

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
