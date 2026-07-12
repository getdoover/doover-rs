//! Parameter inputs — pydoover `ui/parameter.py` ([`FloatInput`],
//! [`TextInput`], [`DatetimeInput`], [`TimeInput`]).
//!
//! Parameters are interactions (they extend pydoover `Interaction`), so
//! they carry the full interaction surface: derived `$cmds.app().<name>`
//! current values, defaults, confirm dialogs, etc.
//!
//! pydoover's `BooleanParameter` is deliberately not ported — its
//! constructor raises `NotImplementedError` ("boolean parameter not
//! implemented in doover site"). The deprecated `include_time=` alias of
//! `DatetimeInput` is not ported either (use [`DatetimeInput::pickers`]).

use std::time::Duration;

use serde_json::{Number, Value};

#[allow(unused_imports)]
use super::element::{impl_element_common, ElementCommon};
use super::interaction::{impl_interaction_builders, Confirm, InteractionCommon};
use super::misc::IntoNumber;
use super::value::UiValue;

/// A numeric input (pydoover `ui.FloatInput`, type `uiFloatInput`).
#[derive(Debug, Clone, PartialEq)]
pub struct FloatInput {
    pub interaction: InteractionCommon,
    /// `min` — int-vs-float flavour survives ([`serde_json::Number`]).
    pub min_val: Option<Number>,
    pub max_val: Option<Number>,
}

impl FloatInput {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name), min_val: None, max_val: None }
    }

    pub fn min_val(mut self, min: impl IntoNumber) -> Self {
        self.min_val = Some(min.into_number());
        self
    }

    pub fn max_val(mut self, max: impl IntoNumber) -> Self {
        self.max_val = Some(max.into_number());
        self
    }

    /// pydoover `FloatInput.to_dict()`: interaction keys + `min?` + `max?`.
    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiFloatInput");
        if let Some(min) = &self.min_val {
            m.insert("min".into(), Value::Number(min.clone()));
        }
        if let Some(max) = &self.max_val {
            m.insert("max".into(), Value::Number(max.clone()));
        }
        Value::Object(m)
    }
}

impl_interaction_builders!(FloatInput);

/// A text input (pydoover `ui.TextInput`, type `uiTextInput`;
/// `isTextArea` is always emitted, defaulting `false`).
#[derive(Debug, Clone, PartialEq)]
pub struct TextInput {
    pub interaction: InteractionCommon,
    pub is_text_area: bool,
}

impl TextInput {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name), is_text_area: false }
    }

    /// Render as a large text area instead of an inline field.
    pub fn is_text_area(mut self, is_text_area: bool) -> Self {
        self.is_text_area = is_text_area;
        self
    }

    /// pydoover `TextInput.to_dict()`: interaction keys + `isTextArea`.
    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiTextInput");
        m.insert("isTextArea".into(), Value::Bool(self.is_text_area));
        Value::Object(m)
    }
}

impl_interaction_builders!(TextInput);

macro_rules! datetime_input_element {
    ($(#[$doc:meta])* $name:ident, $ty:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            pub interaction: InteractionCommon,
            /// Which pickers the input offers: `"date"` and/or `"time"`.
            pub pickers: Option<Vec<String>>,
            /// Constrain values relative to now: `"past"` or `"future"`.
            pub direction: Option<String>,
            /// `maxPast`, milliseconds.
            pub max_past_ms: Option<i64>,
            /// `maxFuture`, milliseconds.
            pub max_future_ms: Option<i64>,
        }

        impl $name {
            pub fn new(display_name: &str) -> Self {
                Self {
                    interaction: InteractionCommon::new(display_name),
                    pickers: None,
                    direction: None,
                    max_past_ms: None,
                    max_future_ms: None,
                }
            }

            pub fn pickers(mut self, pickers: impl IntoIterator<Item = impl Into<String>>) -> Self {
                self.pickers = Some(pickers.into_iter().map(Into::into).collect());
                self
            }

            pub fn direction(mut self, direction: impl Into<String>) -> Self {
                self.direction = Some(direction.into());
                self
            }

            /// How far in the past values may be (pydoover `max_past` — a
            /// timedelta or seconds, emitted as whole milliseconds).
            pub fn max_past(mut self, max_past: Duration) -> Self {
                self.max_past_ms = Some(max_past.as_millis() as i64);
                self
            }

            /// How far in the future values may be (see [`max_past`](Self::max_past)).
            pub fn max_future(mut self, max_future: Duration) -> Self {
                self.max_future_ms = Some(max_future.as_millis() as i64);
                self
            }

            /// pydoover `DatetimeInput.to_dict()`: interaction keys +
            /// `pickers?`, `direction?`, `maxPast?`, `maxFuture?`.
            fn element_json(&self) -> Value {
                let mut m = self.interaction.interaction_json($ty);
                if let Some(pickers) = &self.pickers {
                    m.insert(
                        "pickers".into(),
                        Value::Array(pickers.iter().map(|p| Value::String(p.clone())).collect()),
                    );
                }
                if let Some(direction) = &self.direction {
                    m.insert("direction".into(), Value::String(direction.clone()));
                }
                if let Some(ms) = self.max_past_ms {
                    m.insert("maxPast".into(), Value::from(ms));
                }
                if let Some(ms) = self.max_future_ms {
                    m.insert("maxFuture".into(), Value::from(ms));
                }
                Value::Object(m)
            }
        }

        impl_interaction_builders!($name);
    };
}

datetime_input_element!(
    /// A date-and-time input (pydoover `ui.DatetimeInput`, type
    /// `uiDatetimeInput`). Values are integer epoch milliseconds in UTC.
    DatetimeInput,
    "uiDatetimeInput"
);

datetime_input_element!(
    /// A time-of-day input (pydoover `ui.TimeInput`, type `uiTimeInput`).
    /// Values are seconds into the local day.
    TimeInput,
    "uiTimeInput"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiElement;
    use serde_json::json;

    #[test]
    fn float_input_min_max_flavours() {
        let f = FloatInput::new("Target Level").min_val(0).max_val(1.5);
        let out = f.to_json();
        assert!(f.is_interaction());
        assert_eq!(out["type"], json!("uiFloatInput"));
        assert_eq!(out["currentValue"], json!("$cmds.app().target_level"));
        assert_eq!(serde_json::to_string(&out["min"]).unwrap(), "0");
        assert_eq!(serde_json::to_string(&out["max"]).unwrap(), "1.5");
    }

    #[test]
    fn float_input_without_bounds_omits_them() {
        let out = FloatInput::new("X").to_json();
        assert!(out.get("min").is_none() && out.get("max").is_none());
    }

    #[test]
    fn text_input_always_emits_is_text_area() {
        assert_eq!(TextInput::new("Notes").to_json()["isTextArea"], json!(false));
        assert_eq!(
            TextInput::new("Notes").is_text_area(true).to_json()["isTextArea"],
            json!(true)
        );
    }

    #[test]
    fn datetime_input_fields() {
        let d = DatetimeInput::new("Start At")
            .pickers(["date", "time"])
            .direction("future")
            .max_future(Duration::from_secs(3600));
        let out = d.to_json();
        assert_eq!(out["type"], json!("uiDatetimeInput"));
        assert_eq!(out["pickers"], json!(["date", "time"]));
        assert_eq!(out["direction"], json!("future"));
        assert_eq!(out["maxFuture"], json!(3_600_000));
        assert!(out.get("maxPast").is_none());
    }

    #[test]
    fn time_input_type() {
        assert_eq!(TimeInput::new("Run At").to_json()["type"], json!("uiTimeInput"));
    }
}
