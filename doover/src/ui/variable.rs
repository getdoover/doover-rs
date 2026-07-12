//! Read-only UI variables — pydoover `ui/variable.py` (`Variable` and its
//! `NumericVariable` / `TextVariable` / `BooleanVariable` /
//! `DateTimeVariable` / `Timestamp` subclasses).

use std::time::Duration;

use serde_json::Value;

use super::element::{impl_element_common, ElementCommon};
use super::misc::{Range, Threshold};
use super::value::UiValue;

macro_rules! variable_element {
    ($(#[$doc:meta])* $name:ident, $var_type:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            pub common: ElementCommon,
            /// The `currentValue` slot. Defaults to `Lit(Null)` — pydoover's
            /// `value=None` emits `"currentValue": null`.
            pub value: UiValue,
            /// `decPrecision`.
            pub precision: Option<i64>,
            pub ranges: Option<Vec<Range>>,
            pub thresholds: Option<Vec<Threshold>>,
            /// `"line"`, `"zone"` or `"off"` (pydoover `ui.RangeView`).
            pub default_range_view: Option<String>,
            /// `earliestDataDate`, epoch seconds.
            pub earliest_data_date: Option<i64>,
            /// `defaultRangeSince`, milliseconds.
            pub default_range_since_ms: Option<i64>,
            pub default_zoom: Option<String>,
            /// Emitted inverted as `notGraphable`.
            pub graphable: Option<bool>,
        }

        impl $name {
            pub fn new(display_name: &str) -> Self {
                Self {
                    common: ElementCommon::new(display_name, Some(true)),
                    value: UiValue::Lit(Value::Null),
                    precision: None,
                    ranges: None,
                    thresholds: None,
                    default_range_view: None,
                    earliest_data_date: None,
                    default_range_since_ms: None,
                    default_zoom: None,
                    graphable: None,
                }
            }

            /// The variable's value: a literal, or a tag reference captured
            /// from a declared [`Tag<T>`](crate::tags::Tag) handle
            /// (`.value(&tags.level_reading)`), which also carries the tag's
            /// `live` flag onto the emitted element.
            pub fn value(mut self, value: impl Into<UiValue>) -> Self {
                self.value = value.into();
                self
            }

            /// Decimal places for display (`decPrecision`).
            pub fn precision(mut self, precision: i64) -> Self {
                self.precision = Some(precision);
                self
            }

            pub fn ranges(mut self, ranges: Vec<Range>) -> Self {
                self.ranges = Some(ranges);
                self
            }

            pub fn thresholds(mut self, thresholds: Vec<Threshold>) -> Self {
                self.thresholds = Some(thresholds);
                self
            }

            pub fn default_range_view(mut self, view: impl Into<String>) -> Self {
                self.default_range_view = Some(view.into());
                self
            }

            /// Earliest time data is available for this variable.
            pub fn earliest_data_date_epoch(mut self, epoch_secs: i64) -> Self {
                self.earliest_data_date = Some(epoch_secs);
                self
            }

            /// How much history the plot shows on load (pydoover
            /// `default_range_since` — whole seconds, emitted as ms).
            pub fn default_range_since(mut self, since: Duration) -> Self {
                self.default_range_since_ms = Some(since.as_secs() as i64 * 1000);
                self
            }

            pub fn default_zoom(mut self, zoom: impl Into<String>) -> Self {
                self.default_zoom = Some(zoom.into());
                self
            }

            pub fn graphable(mut self, graphable: bool) -> Self {
                self.graphable = Some(graphable);
                self
            }

            fn common(&self) -> &ElementCommon {
                &self.common
            }

            fn common_mut(&mut self) -> &mut ElementCommon {
                &mut self.common
            }

            /// pydoover `Variable.to_dict()` (the subclasses add no extra
            /// keys — `form` sits in the base slot).
            fn element_json(&self) -> Value {
                let mut m = self.common.base_json("uiVariable");
                m.insert("varType".into(), Value::String($var_type.into()));
                if let Some(v) = self.value.to_json() {
                    m.insert("currentValue".into(), v);
                }
                if let Some(p) = self.precision {
                    m.insert("decPrecision".into(), Value::from(p));
                }
                if let Some(t) = self.earliest_data_date {
                    m.insert("earliestDataDate".into(), Value::from(t));
                }
                if let Some(ms) = self.default_range_since_ms {
                    m.insert("defaultRangeSince".into(), Value::from(ms));
                }
                if let Some(z) = &self.default_zoom {
                    m.insert("defaultZoom".into(), Value::String(z.clone()));
                }
                if let Some(ranges) = &self.ranges {
                    m.insert(
                        "ranges".into(),
                        Value::Array(ranges.iter().map(Range::to_json).collect()),
                    );
                }
                if let Some(thresholds) = &self.thresholds {
                    m.insert(
                        "thresholds".into(),
                        Value::Array(thresholds.iter().map(Threshold::to_json).collect()),
                    );
                }
                if let Some(v) = &self.default_range_view {
                    m.insert("defaultRangeView".into(), Value::String(v.clone()));
                }
                if let Some(g) = self.graphable {
                    m.insert("notGraphable".into(), Value::Bool(!g));
                }
                if self.value.is_live() {
                    m.insert("live".into(), Value::Bool(true));
                }
                Value::Object(m)
            }
        }

        impl_element_common!($name);
    };
}

variable_element!(
    /// A numeric read-only variable (pydoover `ui.NumericVariable`,
    /// `varType: "float"`).
    NumericVariable,
    "float"
);

variable_element!(
    /// A text read-only variable (pydoover `ui.TextVariable`,
    /// `varType: "string"`).
    TextVariable,
    "string"
);

variable_element!(
    /// A boolean read-only variable (pydoover `ui.BooleanVariable`,
    /// `varType: "bool"`).
    BooleanVariable,
    "bool"
);

variable_element!(
    /// A date/time read-only variable (pydoover `ui.DateTimeVariable`,
    /// `varType: "time"`).
    DateTimeVariable,
    "time"
);

/// A relative/absolute timestamp display (pydoover `ui.Timestamp`, type
/// `uiTimestamp`, `varType: "timestamp"`). Values are epoch **milliseconds**
/// (pydoover converts `datetime` values to `int(ts * 1000)`).
///
/// Quirk replicated verbatim: pydoover `Timestamp.__init__` overwrites the
/// base `Variable.precision` slot with its own *string* precision
/// (`"second"`/`"minute"`), so `Variable.to_dict` emits it as
/// `decPrecision` **and** `Timestamp.to_dict` appends it again as
/// `precision` — a set precision appears under both keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Timestamp {
    pub common: ElementCommon,
    /// The `currentValue` slot. Defaults to [`UiValue::Missing`] (pydoover's
    /// `value=NotSet` — unlike the other variables, whose default is
    /// `None`/`null`), so an unset value omits the key.
    pub value: UiValue,
    /// How often the frontend refreshes the relative label: `"second"` or
    /// `"minute"`.
    pub precision: Option<String>,
    /// `absoluteFormat`.
    pub absolute_format: Option<String>,
}

impl Timestamp {
    pub fn new(display_name: &str) -> Self {
        Self {
            common: ElementCommon::new(display_name, Some(true)),
            value: UiValue::Missing,
            precision: None,
            absolute_format: None,
        }
    }

    /// The timestamp value: epoch milliseconds, or a tag reference from a
    /// declared [`Tag<T>`](crate::tags::Tag) handle.
    pub fn value(mut self, value: impl Into<UiValue>) -> Self {
        self.value = value.into();
        self
    }

    /// `"second"` or `"minute"` (see the struct docs for the doubled
    /// `decPrecision`/`precision` emission quirk).
    pub fn precision(mut self, precision: impl Into<String>) -> Self {
        self.precision = Some(precision.into());
        self
    }

    pub fn absolute_format(mut self, format: impl Into<String>) -> Self {
        self.absolute_format = Some(format.into());
        self
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `Timestamp.to_dict()` over `Variable.to_dict()`: base keys,
    /// `varType`, `currentValue?`, `decPrecision?` (the quirk), `live?`,
    /// then `precision?` and `absoluteFormat?`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiTimestamp");
        m.insert("varType".into(), Value::String("timestamp".into()));
        if let Some(v) = self.value.to_json() {
            m.insert("currentValue".into(), v);
        }
        if let Some(p) = &self.precision {
            m.insert("decPrecision".into(), Value::String(p.clone()));
        }
        if self.value.is_live() {
            m.insert("live".into(), Value::Bool(true));
        }
        if let Some(p) = &self.precision {
            m.insert("precision".into(), Value::String(p.clone()));
        }
        if let Some(f) = &self.absolute_format {
            m.insert("absoluteFormat".into(), Value::String(f.clone()));
        }
        Value::Object(m)
    }
}

impl_element_common!(Timestamp);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{Colour, UiElement, Widget};
    use serde_json::json;

    #[test]
    fn plain_numeric_variable_emits_null_current_value() {
        let mut v = NumericVariable::new("Speed");
        v.set_position_if_unset(51);
        assert_eq!(
            serde_json::to_string(&v.to_json()).unwrap(),
            r#"{"name":"speed","type":"uiVariable","displayString":"Speed","showActivity":true,"position":51,"hidden":false,"varType":"float","currentValue":null}"#
        );
    }

    #[test]
    fn full_numeric_variable_key_order() {
        let mut v = NumericVariable::new("Level")
            .units("%")
            .value(UiValue::TagRef {
                name: "level".into(),
                tag_type: Some("number".into()),
                default: Some(Value::Null),
                live: true,
            })
            .precision(1)
            .form(Widget::RADIAL)
            .ranges(vec![Range::new("Low", 0, 15.0, Colour::BLUE)]);
        v.set_position_if_unset(51);
        let out = v.to_json();
        let keys: Vec<_> = out.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "name",
                "type",
                "displayString",
                "showActivity",
                "form",
                "position",
                "hidden",
                "units",
                "varType",
                "currentValue",
                "decPrecision",
                "ranges",
                "live"
            ]
        );
        assert_eq!(out["currentValue"], json!("$tag.app().level:number:null"));
        assert_eq!(out["live"], json!(true));
    }

    #[test]
    fn explicit_position_is_not_overwritten() {
        let mut v = TextVariable::new("Status").position(10);
        v.set_position_if_unset(51);
        assert_eq!(UiElement::position(&v), Some(10));
    }

    #[test]
    fn var_types() {
        assert_eq!(BooleanVariable::new("On").to_json()["varType"], json!("bool"));
        assert_eq!(DateTimeVariable::new("At").to_json()["varType"], json!("time"));
        assert_eq!(TextVariable::new("Name").to_json()["varType"], json!("string"));
    }

    #[test]
    fn graphable_inverts_to_not_graphable() {
        let v = NumericVariable::new("Speed").graphable(false);
        assert_eq!(v.to_json()["notGraphable"], json!(true));
    }

    #[test]
    fn timestamp_unset_value_omits_current_value() {
        let t = Timestamp::new("Last Seen");
        let out = t.to_json();
        assert_eq!(out["type"], json!("uiTimestamp"));
        assert_eq!(out["varType"], json!("timestamp"));
        assert!(out.get("currentValue").is_none());
    }

    #[test]
    fn timestamp_precision_quirk_emits_both_keys() {
        let t = Timestamp::new("Next Run").value(1_700_000_000_000_i64).precision("second");
        let out = t.to_json();
        assert_eq!(out["currentValue"], json!(1_700_000_000_000_i64));
        assert_eq!(out["decPrecision"], json!("second"));
        assert_eq!(out["precision"], json!("second"));
        let keys: Vec<_> = out.as_object().unwrap().keys().map(String::as_str).collect();
        let dec = keys.iter().position(|k| *k == "decPrecision").unwrap();
        let plain = keys.iter().position(|k| *k == "precision").unwrap();
        assert!(dec < plain, "{keys:?}");
    }
}
