//! `doover_config.json` read-merge-write, mirroring pydoover
//! `config.Schema.export()` (and the identical `ui.UI.export()`):
//!
//! ```python
//! if fp.exists():
//!     data = json.loads(fp.read_text())
//! else:
//!     data = {}
//! try:
//!     data[app_name]["config_schema"] = cls.to_schema()
//! except KeyError:
//!     data[app_name] = {"config_schema": cls.to_schema()}
//! fp.write_text(json.dumps(data, indent=4))
//! ```
//!
//! Byte-compat notes: `json.dumps(indent=4)` uses 4-space indent, `": "` /
//! `","` separators, and writes NO trailing newline. serde_json's
//! `PrettyFormatter` with a 4-space indent produces identical bytes for
//! ASCII content (Python escapes non-ASCII by default — `ensure_ascii=True`
//! — while serde_json emits UTF-8; keep doover_config.json ASCII).

use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{DooverError, Result};

/// Read `path` (if present), set `data[app_name]["config_schema"] =
/// schema_json` and write the file back with Python `json.dumps(indent=4)`
/// formatting. Other keys (and their order) are preserved.
pub fn write_config_schema(path: impl AsRef<Path>, app_name: &str, schema_json: Value) -> Result<()> {
    write_app_entry(path.as_ref(), app_name, "config_schema", schema_json)
}

/// Like [`write_config_schema`] but for `data[app_name]["ui_schema"]` —
/// pydoover `ui.UI.export()`. Pass the value from
/// [`UiTree::to_schema`](crate::ui::UiTree::to_schema) (after
/// [`finalize`](crate::ui::UiTree::finalize)).
pub fn write_ui_schema(path: impl AsRef<Path>, app_name: &str, ui_json: Value) -> Result<()> {
    write_app_entry(path.as_ref(), app_name, "ui_schema", ui_json)
}

/// The shared read-merge-write: set `data[app_name][key] = value` preserving
/// every other key (and their order).
fn write_app_entry(path: &Path, app_name: &str, key: &str, value: Value) -> Result<()> {
    let mut data: Map<String, Value> = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DooverError::Other(format!("reading {path:?}: {e}")))?;
        serde_json::from_str(&text)?
    } else {
        Map::new()
    };

    match data.get_mut(app_name) {
        Some(Value::Object(app)) => {
            app.insert(key.into(), value);
        }
        _ => {
            let mut app = Map::new();
            app.insert(key.into(), value);
            data.insert(app_name.into(), Value::Object(app));
        }
    }

    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    Value::Object(data).serialize(&mut ser)?;
    // No trailing newline: Python's `fp.write_text(json.dumps(...))` writes
    // exactly the dump.
    std::fs::write(path, buf).map_err(|e| DooverError::Other(format!("writing {path:?}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("doover-rs-export-{}-{name}.json", std::process::id()));
        p
    }

    #[test]
    fn creates_file_when_missing() {
        let path = temp_path("create");
        let _ = std::fs::remove_file(&path);
        write_config_schema(&path, "my_app", json!({"title": "$default"})).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\n    \"my_app\": {\n        \"config_schema\": {\n            \"title\": \"$default\"\n        }\n    }\n}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preserves_other_keys_and_order() {
        let path = temp_path("merge");
        std::fs::write(&path, r#"{"other": 1, "my_app": {"id": 42, "config_schema": {"old": true}, "z": null}}"#)
            .unwrap();
        write_config_schema(&path, "my_app", json!({"new": true})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["other"], json!(1));
        assert_eq!(v["my_app"]["id"], json!(42));
        assert_eq!(v["my_app"]["config_schema"], json!({"new": true}));
        // key order preserved: "other" first, config_schema stays between id and z
        let keys: Vec<_> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["other", "my_app"]);
        let app_keys: Vec<_> = v["my_app"].as_object().unwrap().keys().collect();
        assert_eq!(app_keys, ["id", "config_schema", "z"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ui_schema_merges_beside_config_schema() {
        let path = temp_path("ui");
        std::fs::write(&path, r#"{"my_app": {"config_schema": {"a": 1}}}"#).unwrap();
        write_ui_schema(&path, "my_app", json!({"type": "uiApplication"})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["my_app"]["config_schema"], json!({"a": 1}));
        assert_eq!(v["my_app"]["ui_schema"], json!({"type": "uiApplication"}));
        let _ = std::fs::remove_file(&path);
    }
}
