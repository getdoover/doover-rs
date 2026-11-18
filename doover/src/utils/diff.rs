//! The doover diff/merge engine — a faithful port of `pydoover/utils/diff.py`.
//!
//! Diffs are the substrate under aggregate updates and tag flushing, so the
//! semantics here must match pydoover exactly:
//!
//! - `null` in a diff means *delete the key* when `do_delete` is true, and
//!   *set the key to null* when it is false.
//! - A non-object diff (or applying onto non-object data) replaces the data
//!   wholesale with the diff — including any `null` values inside it.
//! - Generating a diff for a nested object that comes out empty drops the key
//!   from the diff entirely (Python's truthiness check on the sub-diff).
//! - Value comparison follows Python semantics: `1 == 1.0` and `True == 1`,
//!   so integer and float (and bool) representations of the same number are
//!   *not* a change.

use serde_json::{Map, Value};

/// Apply a doover-compatible diff to a JSON object, returning a new object.
///
/// Mirrors pydoover's `apply_diff(data, diff, do_delete, clone=True)`.
pub fn apply_diff(data: &Value, diff: &Value, do_delete: bool) -> Value {
    let mut out = data.clone();
    apply_diff_in_place(&mut out, diff, do_delete);
    out
}

/// Apply a doover-compatible diff to a JSON object in place.
///
/// Equivalent to [`apply_diff`] but mutates `data` (pydoover's `clone=False`).
pub fn apply_diff_in_place(data: &mut Value, diff: &Value, do_delete: bool) {
    let Value::Object(diff_map) = diff else {
        *data = diff.clone();
        return;
    };
    if !data.is_object() {
        *data = diff.clone();
        return;
    }
    let data_map = data.as_object_mut().expect("checked is_object above");
    for (k, v) in diff_map {
        match v {
            Value::Object(_) => {
                // Python recurses on `data.get(k, {})`: a missing or non-object
                // existing value falls through apply_diff's non-object path.
                match data_map.get_mut(k) {
                    Some(existing) => apply_diff_in_place(existing, v, do_delete),
                    None => {
                        let mut fresh = Value::Object(Map::new());
                        apply_diff_in_place(&mut fresh, v, do_delete);
                        data_map.insert(k.clone(), fresh);
                    }
                }
            }
            Value::Null => {
                if do_delete {
                    data_map.remove(k);
                } else {
                    data_map.insert(k.clone(), Value::Null);
                }
            }
            other => {
                data_map.insert(k.clone(), other.clone());
            }
        }
    }
}

/// Generate a doover-compatible diff between two JSON objects.
///
/// The diff contains every key that differs between `old` and `new`. Keys
/// present in `old` but absent from `new` are set to `null` when `do_delete`
/// is true. Mirrors pydoover's `generate_diff(old, new, do_delete)`.
pub fn generate_diff(old: &Value, new: &Value, do_delete: bool) -> Value {
    let (Value::Object(old_map), Value::Object(new_map)) = (old, new) else {
        return new.clone();
    };
    let mut diff = Map::new();
    for (k, v) in new_map {
        if v.is_object() {
            let empty = Value::Object(Map::new());
            let old_v = old_map.get(k).unwrap_or(&empty);
            let d = generate_diff(old_v, v, do_delete);
            // Python drops falsy (empty) sub-diffs. `d` is always an object
            // here: either the recursive diff, or `v` itself when the old
            // value was not an object.
            if d.as_object().is_some_and(|m| !m.is_empty()) {
                diff.insert(k.clone(), d);
            }
        } else if old_map.get(k).is_none_or(|ov| !python_json_eq(ov, v)) {
            diff.insert(k.clone(), v.clone());
        }
    }
    if do_delete {
        for k in old_map.keys() {
            if !new_map.contains_key(k) {
                diff.insert(k.clone(), Value::Null);
            }
        }
    }
    Value::Object(diff)
}

/// Whether `generate_diff(&apply_diff(base, pending, false), new, false)` would
/// come out empty, without building the merged tree: only the paths in `new`
/// are looked at. `base` is a cache and `pending` the writes buffered on top of
/// it; `new` is the object being set. Setting a tag asks this on every call, so
/// cloning the whole cache for it costs far more than the answer.
pub fn merged_diff_is_empty(base: &Value, pending: &Value, new: &Value) -> bool {
    let Value::Object(new_map) = new else {
        // generate_diff hands back `new` itself, which is no empty object.
        return false;
    };
    // apply_diff onto non-object data, or with a non-object diff, gives the
    // diff itself: the merged view is `pending` overlaid on `base` only when
    // `pending` is an object.
    let Value::Object(pending_map) = pending else {
        return generate_diff(pending, new, false).as_object().is_some_and(|m| m.is_empty());
    };
    merged_maps_unchanged(base.as_object(), Some(pending_map), new_map)
}

/// The merged view at one level is `base` overlaid with `pending`.
fn merged_maps_unchanged(
    base: Option<&Map<String, Value>>,
    pending: Option<&Map<String, Value>>,
    new: &Map<String, Value>,
) -> bool {
    for (k, v) in new {
        let p = pending.and_then(|m| m.get(k));
        let b = base.and_then(|m| m.get(k));
        if let Value::Object(new_sub) = v {
            if new_sub.is_empty() {
                continue; // an empty sub-diff is dropped
            }
            // Where the merged value at `k` is an object, and from what.
            let (sub_base, sub_pending) = match p {
                Some(Value::Object(pm)) => (b.and_then(Value::as_object), Some(pm)),
                Some(_) => return false, // merged value is not an object
                None => match b {
                    Some(Value::Object(bm)) => (Some(bm), None),
                    // Missing: generate_diff compares against an empty object.
                    None => (None, None),
                    Some(_) => return false, // not an object
                },
            };
            if !merged_maps_unchanged(sub_base, sub_pending, new_sub) {
                return false;
            }
        } else {
            let current = match p {
                Some(Value::Object(_)) => return false, // an object is never equal to a leaf
                Some(pv) => Some(pv),
                None => b,
            };
            if current.is_none_or(|cv| !python_json_eq(cv, v)) {
                return false;
            }
        }
    }
    true
}

/// JSON value equality with Python semantics: numbers compare by value across
/// int/float representations, and bools compare equal to 0/1 (`True == 1`).
fn python_json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(xa, ya)| python_json_eq(xa, ya))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, xv)| y.get(k).is_some_and(|yv| python_json_eq(xv, yv)))
        }
        _ => match (numeric_value(a), numeric_value(b)) {
            (Some(x), Some(y)) => numbers_eq(x, y),
            _ => false,
        },
    }
}

/// A number viewed the way Python would: bools are 0/1, integers exact,
/// floats as f64.
enum PyNum {
    Int(i128),
    Float(f64),
}

fn numeric_value(v: &Value) -> Option<PyNum> {
    match v {
        Value::Bool(b) => Some(PyNum::Int(*b as i128)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(PyNum::Int(i as i128))
            } else if let Some(u) = n.as_u64() {
                Some(PyNum::Int(u as i128))
            } else {
                n.as_f64().map(PyNum::Float)
            }
        }
        _ => None,
    }
}

fn numbers_eq(a: PyNum, b: PyNum) -> bool {
    match (a, b) {
        (PyNum::Int(x), PyNum::Int(y)) => x == y,
        (PyNum::Float(x), PyNum::Float(y)) => x == y,
        // Python compares int == float exactly; matching via f64 conversion is
        // exact for every integer magnitude that fits a JSON payload in practice.
        (PyNum::Int(x), PyNum::Float(y)) | (PyNum::Float(y), PyNum::Int(x)) => x as f64 == y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_diff_is_empty_agrees_with_merging_and_diffing() {
        let bases = [
            json!({}),
            json!({"app": {"a": 1, "b": "x", "n": null, "o": {"p": 1.0}}, "other": {"z": [1, 2]}}),
            json!({"app": 5}),
            json!(null),
        ];
        let pendings = [
            json!({}),
            json!({"app": {"a": 2}}),
            json!({"app": {"a": null, "o": {"q": true}}}),
            json!({"app": "flat"}),
            json!({"app": {"o": 3}}),
            json!(null),
        ];
        let news = [
            json!({}),
            json!({"app": {"a": 1}}),
            json!({"app": {"a": 1.0}}),
            json!({"app": {"a": 2}}),
            json!({"app": {"a": null}}),
            json!({"app": {"n": null}}),
            json!({"app": {"b": "x", "a": true}}),
            json!({"app": {"o": {"p": 1}}}),
            json!({"app": {"o": {"q": 1}}}),
            json!({"app": {"o": {}}}),
            json!({"app": {"missing": 0}}),
            json!({"app": {}}),
            json!({"other": {"z": [1.0, 2]}}),
            json!({"other": {"z": [1, 3]}}),
            json!({"app": {"o": 3}}),
            json!({"new_app": {"t": 1}}),
        ];
        for base in &bases {
            for pending in &pendings {
                for new in &news {
                    let reference = generate_diff(&apply_diff(base, pending, false), new, false)
                        .as_object()
                        .is_some_and(|m| m.is_empty());
                    assert_eq!(
                        merged_diff_is_empty(base, pending, new),
                        reference,
                        "base {base} pending {pending} new {new}"
                    );
                }
            }
        }
    }


    // -- apply_diff: ported from pydoover/tests/test_diffs.py::TestApplyDiff --

    #[test]
    fn apply_diff_basic() {
        let data = json!({"a": 1, "b": 2, "c": 3});
        let diff = json!({"c": 4});
        assert_eq!(apply_diff(&data, &diff, true), json!({"a": 1, "b": 2, "c": 4}));
    }

    #[test]
    fn apply_diff_remove() {
        let data = json!({"a": 1, "b": 2, "c": 3});
        let diff = json!({"c": null});
        assert_eq!(apply_diff(&data, &diff, true), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn apply_diff_nested() {
        let data = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let diff = json!({"c": {"d": 4}});
        assert_eq!(
            apply_diff(&data, &diff, true),
            json!({"a": 1, "b": 2, "c": {"d": 4}})
        );
    }

    #[test]
    fn apply_diff_nested_remove() {
        let data = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let diff = json!({"c": {"d": null}});
        assert_eq!(apply_diff(&data, &diff, true), json!({"a": 1, "b": 2, "c": {}}));

        let data = json!({"a": 1, "b": 2, "c": {"d": 3, "e": 4}});
        let diff = json!({"b": 3, "c": {"d": null}});
        assert_eq!(
            apply_diff(&data, &diff, true),
            json!({"a": 1, "b": 3, "c": {"e": 4}})
        );
    }

    #[test]
    fn apply_new_dict_old_string() {
        let data = json!("a");
        let diff = json!({"a": 1});
        assert_eq!(apply_diff(&data, &diff, true), json!({"a": 1}));
    }

    #[test]
    fn apply_new_string_old_dict() {
        let data = json!({"a": 1});
        let diff = json!("a");
        assert_eq!(apply_diff(&data, &diff, true), json!("a"));
    }

    #[test]
    fn apply_diff_nested_remove_no_delete() {
        let data = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let diff = json!({"c": {"d": null}});
        assert_eq!(
            apply_diff(&data, &diff, false),
            json!({"a": 1, "b": 2, "c": {"d": null}})
        );

        let data = json!({"a": 1, "b": 2, "c": {"d": 3, "e": 4}});
        let diff = json!({"b": 3, "c": {"d": null}});
        assert_eq!(
            apply_diff(&data, &diff, false),
            json!({"a": 1, "b": 3, "c": {"d": null, "e": 4}})
        );
    }

    // -- generate_diff: ported from pydoover/tests/test_diffs.py::TestGenerateDiff --

    #[test]
    fn generate_basic() {
        let old = json!({"a": 1, "b": 2, "c": 3});
        let new = json!({"a": 1, "b": 2, "c": 4});
        assert_eq!(generate_diff(&old, &new, true), json!({"c": 4}));
    }

    #[test]
    fn generate_remove() {
        let old = json!({"a": 1, "b": 2, "c": 3});
        let new = json!({"a": 1, "b": 2});
        assert_eq!(generate_diff(&old, &new, true), json!({"c": null}));
    }

    #[test]
    fn generate_nested() {
        let old = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let new = json!({"a": 1, "b": 2, "c": {"d": 4}});
        assert_eq!(generate_diff(&old, &new, true), json!({"c": {"d": 4}}));
    }

    #[test]
    fn generate_nested_remove() {
        let old = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let new = json!({"a": 1, "b": 2, "c": {}});
        assert_eq!(generate_diff(&old, &new, true), json!({"c": {"d": null}}));

        let old = json!({"a": 1, "b": 2, "c": {"d": 3, "e": 4}});
        let new = json!({"a": 1, "b": 2, "c": {"e": 4}});
        assert_eq!(generate_diff(&old, &new, true), json!({"c": {"d": null}}));
    }

    #[test]
    fn generate_nested_same() {
        let old = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let new = json!({"a": 1, "b": 2, "c": {"d": 3}});
        assert_eq!(generate_diff(&old, &new, true), json!({}));
    }

    #[test]
    fn generate_old_string_new_dict() {
        let old = json!("a");
        let new = json!({"a": 1});
        assert_eq!(generate_diff(&old, &new, true), json!({"a": 1}));
    }

    #[test]
    fn generate_new_string_old_dict() {
        let old = json!({"a": 1});
        let new = json!("a");
        assert_eq!(generate_diff(&old, &new, true), json!("a"));
    }

    #[test]
    fn generate_diff_no_delete() {
        let old = json!({"a": 1, "b": 2, "c": 3});

        let new = json!({"a": 1, "b": 2, "c": 4});
        assert_eq!(generate_diff(&old, &new, false), json!({"c": 4}));

        let new = json!({"c": 4});
        assert_eq!(generate_diff(&old, &new, false), json!({"c": 4}));

        let new = json!({"b": 2});
        assert_eq!(generate_diff(&old, &new, false), json!({}));

        assert_eq!(generate_diff(&old, &old, false), json!({}));
    }

    // -- extra cases covering pydoover quirks the tests above don't reach --

    #[test]
    fn apply_object_diff_onto_scalar_keeps_diff_verbatim() {
        // Python: apply_diff(scalar, dict) returns the diff object itself,
        // so inner nulls survive even with do_delete=true.
        let data = json!({"a": 5});
        let diff = json!({"a": {"x": null, "y": 1}});
        assert_eq!(
            apply_diff(&data, &diff, true),
            json!({"a": {"x": null, "y": 1}})
        );
    }

    #[test]
    fn apply_object_diff_onto_missing_key_drops_nulls() {
        // Missing key recurses from {}, so nulls are deletions (no-ops).
        let data = json!({});
        let diff = json!({"a": {"x": null, "y": 1}});
        assert_eq!(apply_diff(&data, &diff, true), json!({"a": {"y": 1}}));
    }

    #[test]
    fn generate_drops_empty_object_additions() {
        // Python's truthiness check drops empty sub-diffs, so a newly added
        // empty object never appears in the diff.
        let old = json!({});
        let new = json!({"a": {}});
        assert_eq!(generate_diff(&old, &new, true), json!({}));
    }

    #[test]
    fn generate_replaces_scalar_with_object_wholesale() {
        let old = json!({"a": 5});
        let new = json!({"a": {"x": 1}});
        assert_eq!(generate_diff(&old, &new, true), json!({"a": {"x": 1}}));
    }

    #[test]
    fn generate_python_numeric_equality() {
        // 1 == 1.0 and True == 1 in Python, so these are not changes.
        let old = json!({"a": 1, "b": true, "c": 2.5});
        let new = json!({"a": 1.0, "b": 1, "c": 2.5});
        assert_eq!(generate_diff(&old, &new, true), json!({}));

        // ...but a genuinely different value still is.
        let new = json!({"a": 1.5, "b": 1, "c": 2.5});
        assert_eq!(generate_diff(&old, &new, true), json!({"a": 1.5}));
    }

    #[test]
    fn generate_compares_arrays_and_nested_objects_by_value() {
        let old = json!({"a": [1, {"x": 2}], "b": [1, 2]});
        let new = json!({"a": [1.0, {"x": 2.0}], "b": [1, 3]});
        assert_eq!(generate_diff(&old, &new, true), json!({"b": [1, 3]}));
    }

    #[test]
    fn apply_in_place_matches_cloning_variant() {
        let mut data = json!({"a": 1, "c": {"d": 3, "e": 4}});
        let diff = json!({"b": 2, "c": {"d": null}});
        let expected = apply_diff(&data, &diff, true);
        apply_diff_in_place(&mut data, &diff, true);
        assert_eq!(data, expected);
    }
}
