//! Argument parsers matching pydoover's CLI conventions
//! (`pydoover/cli/parsers.py`): scalar-or-list values like `3`, `[1,2]` or
//! `1,2`, Python-style booleans (`True`/`False`), and inline-JSON payloads.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use serde_json::Value;

use doover::utils::{generate_snowflake_id_at, SnowflakeType, DOOVER_EPOCH};

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
        "true" | "True" => Ok(true),
        "false" | "False" => Ok(false),
        // pydoover's `set_do` typed its value `int | list[int]` and passed it
        // straight through, so any integer was accepted — nonzero is on.
        other => other
            .parse::<i64>()
            .map(|v| v != 0)
            .map_err(|_| format!("{other:?} is not a boolean (use true/false/1/0)")),
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

/// Parse an ISO-8601 datetime the way pydoover's `SubSection._datetime` did
/// (`datetime.fromisoformat`, with `Z` accepted): an explicit offset is
/// honoured, and a naive timestamp is read in the local timezone.
fn parse_iso8601_millis(s: &str) -> Result<u64, String> {
    let err = || format!("{s:?} is not a unix-millisecond timestamp or an ISO-8601 datetime");
    let millis = if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        dt.timestamp_millis()
    } else {
        let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .or_else(|_| {
                NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| {
                    d.and_hms_opt(0, 0, 0).expect("midnight is always a valid time")
                })
            })
            .map_err(|_| err())?;
        naive.and_local_timezone(Local).single().ok_or_else(err)?.timestamp_millis()
    };
    u64::try_from(millis).map_err(|_| format!("{s:?} is before the unix epoch"))
}

/// A `--timestamp` argument: unix milliseconds, or an ISO-8601 datetime.
pub fn parse_timestamp_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    match s.parse::<u64>() {
        Ok(millis) => Ok(millis),
        Err(_) => parse_iso8601_millis(s),
    }
}

/// A `--before` / `--after` message-listing bound. A bare integer is already a
/// snowflake id; an ISO-8601 datetime is converted to the snowflake that sorts
/// at that instant (pydoover `list_messages`' `int | datetime` handling).
pub fn parse_snowflake_bound(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Ok(id) = s.parse::<u64>() {
        return Ok(id);
    }
    let millis = parse_iso8601_millis(s)?;
    if millis < DOOVER_EPOCH {
        return Err(format!("{s:?} is before the doover epoch (2025-01-01)"));
    }
    Ok(generate_snowflake_id_at(millis, SnowflakeType::Unknown, 0, 0, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use doover::utils::unix_millis_from_snowflake;
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

    #[test]
    fn timestamps_accept_millis_and_iso8601() {
        assert_eq!(parse_timestamp_ms("1750000000000").unwrap(), 1_750_000_000_000);
        // An explicit offset is honoured; `Z` is accepted (pydoover replaced it
        // with +00:00 before calling fromisoformat).
        assert_eq!(parse_timestamp_ms("2025-06-15T12:00:00Z").unwrap(), 1_749_988_800_000);
        assert_eq!(
            parse_timestamp_ms("2025-06-15T12:00:00+00:00").unwrap(),
            1_749_988_800_000
        );
        assert_eq!(
            parse_timestamp_ms("2025-06-15T22:00:00+10:00").unwrap(),
            1_749_988_800_000
        );
        assert_eq!(
            parse_timestamp_ms("2025-06-15T12:00:00.500Z").unwrap(),
            1_749_988_800_500
        );
        assert!(parse_timestamp_ms("not a date").is_err());
        assert!(parse_timestamp_ms("1969-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn naive_iso8601_is_read_in_the_local_zone() {
        // pydoover's fromisoformat produced a naive datetime whose .timestamp()
        // interprets it as local time; match that rather than assuming UTC.
        let expected = NaiveDate::from_ymd_opt(2025, 6, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap()
            .timestamp_millis() as u64;
        assert_eq!(parse_timestamp_ms("2025-06-15T12:00:00").unwrap(), expected);
        assert_eq!(parse_timestamp_ms("2025-06-15 12:00:00").unwrap(), expected);
    }

    #[test]
    fn snowflake_bounds_pass_ints_through_and_convert_datetimes() {
        // A bare integer is already a snowflake id.
        assert_eq!(parse_snowflake_bound("123456789").unwrap(), 123_456_789);

        // A datetime becomes the snowflake sorting at that instant: millis
        // since the doover epoch in the high bits, everything else zeroed.
        let id = parse_snowflake_bound("2025-06-15T12:00:00Z").unwrap();
        assert_eq!(id, (1_749_988_800_000 - DOOVER_EPOCH) << 22);
        assert_eq!(unix_millis_from_snowflake(id), 1_749_988_800_000);

        // Before the doover epoch there is no representable snowflake.
        assert!(parse_snowflake_bound("2024-06-15T12:00:00Z").is_err());
        assert!(parse_snowflake_bound("gibberish").is_err());
    }
}
