//! Argument parsers matching pydoover's CLI conventions
//! (`pydoover/cli/parsers.py`): scalar-or-list values like `3`, `[1,2]` or
//! `1,2`, Python-style booleans (`True`/`False`), and inline-JSON payloads.

use serde_json::Value;

/// A list of pins/values parsed from `3`, `[1,2,3]` or `1,2,3`
/// (pydoover `int_or_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntList(pub Vec<i32>);

/// A list of values parsed from `1.5`, `[1.5,2.0]` or `1.5,2.0`
/// (pydoover `float_or_list`).
#[derive(Debug, Clone, PartialEq)]
pub struct FloatList(pub Vec<f32>);

/// A list of values parsed from `1`/`0`/`true`/`false`/`True`/`False` or a
/// bracketed/comma list of those (pydoover `bool_or_list` / `int_or_list` —
/// pydoover uses ints for digital-output values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoolList(pub Vec<bool>);

/// An optional float, where `none`/`null` (any case) means "unset" —
/// pydoover models these as `float | None` parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct MaybeFloat(pub Option<f32>);

fn items(s: &str) -> impl Iterator<Item = &str> {
    let s = s.trim();
    let s = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(s);
    s.split(',').map(str::trim).filter(|p| !p.is_empty())
}

pub fn parse_int_list(s: &str) -> Result<IntList, String> {
    let values: Vec<i32> = items(s)
        .map(|p| p.parse().map_err(|_| format!("{p:?} is not an integer")))
        .collect::<Result<_, _>>()?;
    if values.is_empty() {
        return Err(format!("{s:?} is not an integer or a list of integers"));
    }
    Ok(IntList(values))
}

pub fn parse_float_list(s: &str) -> Result<FloatList, String> {
    let values: Vec<f32> = items(s)
        .map(|p| p.parse().map_err(|_| format!("{p:?} is not a number")))
        .collect::<Result<_, _>>()?;
    if values.is_empty() {
        return Err(format!("{s:?} is not a number or a list of numbers"));
    }
    Ok(FloatList(values))
}

fn parse_bool_item(s: &str) -> Result<bool, String> {
    match s {
        "1" | "true" | "True" => Ok(true),
        "0" | "false" | "False" => Ok(false),
        other => Err(format!("{other:?} is not a boolean (use true/false/1/0)")),
    }
}

pub fn parse_bool_list(s: &str) -> Result<BoolList, String> {
    let values: Vec<bool> = items(s).map(parse_bool_item).collect::<Result<_, _>>()?;
    if values.is_empty() {
        return Err(format!("{s:?} is not a boolean or a list of booleans"));
    }
    Ok(BoolList(values))
}

pub fn parse_maybe_float(s: &str) -> Result<MaybeFloat, String> {
    if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("null") {
        return Ok(MaybeFloat(None));
    }
    s.parse::<f32>()
        .map(|v| MaybeFloat(Some(v)))
        .map_err(|_| format!("{s:?} is not a number (or 'none' to unset)"))
}

/// Parse an inline-JSON payload argument, e.g. `'{"level": 42}'`.
pub fn parse_json(s: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| format!("not valid inline JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn int_lists() {
        assert_eq!(parse_int_list("3").unwrap(), IntList(vec![3]));
        assert_eq!(parse_int_list("[1, 2,3]").unwrap(), IntList(vec![1, 2, 3]));
        assert_eq!(parse_int_list("1,2").unwrap(), IntList(vec![1, 2]));
        assert!(parse_int_list("x").is_err());
        assert!(parse_int_list("[]").is_err());
    }

    #[test]
    fn float_lists() {
        assert_eq!(parse_float_list("1.5").unwrap(), FloatList(vec![1.5]));
        assert_eq!(parse_float_list("[1.5, 2]").unwrap(), FloatList(vec![1.5, 2.0]));
        assert!(parse_float_list("nope").is_err());
    }

    #[test]
    fn bool_lists_accept_python_and_rust_spellings() {
        assert_eq!(parse_bool_list("True").unwrap(), BoolList(vec![true]));
        assert_eq!(parse_bool_list("[true, False, 1, 0]").unwrap(),
            BoolList(vec![true, false, true, false]));
        assert!(parse_bool_list("yes").is_err());
    }

    #[test]
    fn maybe_float() {
        assert_eq!(parse_maybe_float("12.5").unwrap(), MaybeFloat(Some(12.5)));
        assert_eq!(parse_maybe_float("none").unwrap(), MaybeFloat(None));
        assert_eq!(parse_maybe_float("None").unwrap(), MaybeFloat(None));
        assert!(parse_maybe_float("abc").is_err());
    }

    #[test]
    fn json_payloads() {
        assert_eq!(parse_json(r#"{"a": 1}"#).unwrap(), json!({"a": 1}));
        assert!(parse_json("not json").is_err());
    }
}
