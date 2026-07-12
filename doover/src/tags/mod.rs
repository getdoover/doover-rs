//! Tag values — the port of `pydoover.tags` (runtime half).
//!
//! Tags live in the `tag_values` channel aggregate, namespaced per app:
//! `{<app_key>: {<tag>: value}}`. Writes are buffered per loop iteration and
//! flushed once by the runner (`commit_tags`), with the publish max-age
//! driven by whether anyone actually has the app open. `live=true` tags are
//! additionally streamed as one-shot messages while a user watches them.
//!
//! The declarative typed layer: [`Tag<T>`](Tag) handles and the
//! [`TagsCollection`] trait implemented by `#[derive(Tags)]`;
//! [`TagsRuntime`] is the manager underneath it.

mod handle;
mod runtime;
mod triggers;

pub(crate) use handle::tag_ref_lookup;
pub use handle::{ui_tag_type, RemoteTag, Tag, TagValue, TagsCollection};
pub use runtime::{SetTagOptions, TagsRuntime};
pub use triggers::{IntoThresholds, LogTrigger, TriggerSet, TriggerState};

use serde_json::{Map, Value};

/// The channel persisted tag values live on.
pub const TAG_CHANNEL_NAME: &str = "tag_values";
/// The channel `live=true` tags are streamed to as one-shots.
pub const LIVE_TAG_CHANNEL_NAME: &str = "tag_values";
/// Per-device observation channel (see `dv-ui-sub` bucket layout in
/// pydoover `tags/manager.py`).
pub const UI_SUB_CHANNEL_NAME: &str = "dv-ui-sub";
/// Claims older than this are treated as dropped (the customer-site
/// re-stamps every 120 s while a tab is visible).
pub const UI_SUB_FRESH_MS: f64 = 120_000.0;
/// Default aggregate max-age when nobody has the app open (15 min).
pub const TAG_CLOUD_MAX_AGE: f32 = 60.0 * 15.0;
/// Aggregate max-age while a user has the app open (pydoover
/// `TAG_OBSERVED_MAX_AGE`).
pub const TAG_OBSERVED_MAX_AGE: f32 = 3.0;

/// A tag key as a normalized nested path into the `tag_values` aggregate
/// (pydoover `KeyPath`). The first segment is usually an app key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyPath {
    path: Vec<String>,
}

impl KeyPath {
    /// A path from raw segments (already fully qualified).
    pub fn new(parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let path: Vec<String> = parts.into_iter().map(Into::into).collect();
        debug_assert!(!path.is_empty(), "KeyPath requires at least one segment");
        Self { path }
    }

    /// A path scoped under an app key (pydoover `KeyPath(key, app_key=...)`).
    /// An empty app key produces an unscoped (global) path.
    pub fn scoped(app_key: &str, parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut path = Vec::new();
        if !app_key.is_empty() {
            path.push(app_key.to_string());
        }
        path.extend(parts.into_iter().map(Into::into));
        Self { path }
    }

    pub fn segments(&self) -> &[String] {
        &self.path
    }

    /// `<app_key>.<tag>` — how the customer-site qualifies tags on the wire.
    pub fn qualified_name(&self) -> String {
        self.path.join(".")
    }

    /// Wrap a leaf value into a nested object along this path.
    pub fn construct(&self, value: Value) -> Value {
        let mut result = value;
        for part in self.path.iter().rev() {
            let mut map = Map::new();
            map.insert(part.clone(), result);
            result = Value::Object(map);
        }
        result
    }

    /// Resolve this path against a nested value. A present-but-null leaf
    /// returns `Some(Null)`, distinguishing it from a missing key.
    pub fn lookup<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        let mut current = root;
        for part in &self.path {
            current = current.as_object()?.get(part)?;
        }
        Some(current)
    }

    /// Whether this path exists in a nested value.
    pub fn in_value(&self, root: &Value) -> bool {
        self.lookup(root).is_some()
    }
}

/// Recursively remove leaf keys present in `paths` from `target` — dedupes
/// the periodic-log buffer when the same keys were promoted to the
/// immediate-log buffer (pydoover `_strip_paths`).
pub(crate) fn strip_paths(target: &mut Value, paths: &Value) {
    let (Some(target_map), Some(path_map)) = (target.as_object_mut(), paths.as_object()) else {
        return;
    };
    for (k, v) in path_map {
        let Some(existing) = target_map.get_mut(k) else {
            continue;
        };
        if v.is_object() && existing.is_object() {
            strip_paths(existing, v);
            if existing.as_object().is_some_and(|m| m.is_empty()) {
                target_map.remove(k);
            }
        } else {
            target_map.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keypath_construct_and_lookup() {
        let kp = KeyPath::scoped("my_app", ["level"]);
        assert_eq!(kp.construct(json!(5)), json!({"my_app": {"level": 5}}));
        let root = json!({"my_app": {"level": 7, "other": null}});
        assert_eq!(kp.lookup(&root), Some(&json!(7)));
        assert_eq!(kp.qualified_name(), "my_app.level");

        // present-null is distinguishable from missing
        let null_kp = KeyPath::scoped("my_app", ["other"]);
        assert_eq!(null_kp.lookup(&root), Some(&Value::Null));
        assert!(null_kp.in_value(&root));
        let missing = KeyPath::scoped("my_app", ["nope"]);
        assert_eq!(missing.lookup(&root), None);
        assert!(!missing.in_value(&root));
    }

    #[test]
    fn keypath_global_scope() {
        let kp = KeyPath::scoped("", ["shutdown_at"]);
        assert_eq!(kp.construct(json!(1)), json!({"shutdown_at": 1}));
        assert_eq!(kp.qualified_name(), "shutdown_at");
    }

    #[test]
    fn strip_paths_removes_promoted_leaves() {
        let mut target = json!({"app": {"a": 1, "b": 2}, "other": {"c": 3}});
        strip_paths(&mut target, &json!({"app": {"a": 9}}));
        assert_eq!(target, json!({"app": {"b": 2}, "other": {"c": 3}}));

        // removing the last leaf drops the emptied parent
        let mut target = json!({"app": {"a": 1}});
        strip_paths(&mut target, &json!({"app": {"a": 1}}));
        assert_eq!(target, json!({}));
    }
}
