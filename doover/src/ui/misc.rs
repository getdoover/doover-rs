//! UI support types — pydoover `ui/misc.py`: [`Colour`], [`Widget`],
//! [`Range`], [`Threshold`], [`Series`], [`RangeView`],
//! [`ApplicationVariant`], [`SelectOption`] (pydoover `ui.Option`) and
//! [`ConfirmDialog`].

use serde_json::{Map, Number, Value};

use super::value::UiValue;
use crate::config::sanitize_display_name;

/// Colour constants for UI elements (pydoover `ui.Colour`). The site accepts
/// any HTML colour name or hex string, so these are plain `&str`s.
pub struct Colour;

impl Colour {
    pub const BLUE: &'static str = "blue";
    pub const YELLOW: &'static str = "yellow";
    pub const RED: &'static str = "red";
    pub const GREEN: &'static str = "green";
    pub const MAGENTA: &'static str = "magenta";
    pub const LIMEGREEN: &'static str = "limegreen";
    pub const TOMATO: &'static str = "tomato";
    pub const ORANGE: &'static str = "orange";
    pub const PURPLE: &'static str = "purple";
    pub const GREY: &'static str = "grey";
}

/// Gauge widget strings (pydoover `ui.Widget`).
pub struct Widget;

impl Widget {
    pub const LINEAR: &'static str = "linearGauge";
    pub const RADIAL: &'static str = "radialGauge";
}

/// Numeric slots in UI JSON hold [`serde_json::Number`] so the int-vs-float
/// distinction survives (`0` stays `0`, `15.0` stays `15.0`, exactly like
/// Python ints and floats). This conversion keeps builder call sites tidy.
pub trait IntoNumber {
    fn into_number(self) -> Number;
}

macro_rules! impl_into_number_int {
    ($($ty:ty),*) => {$(
        impl IntoNumber for $ty {
            fn into_number(self) -> Number {
                Number::from(self as i64)
            }
        }
    )*};
}

impl_into_number_int!(i8, i16, i32, i64, u8, u16, u32);

impl IntoNumber for f64 {
    fn into_number(self) -> Number {
        Number::from_f64(self).expect("UI numbers must be finite")
    }
}

impl IntoNumber for f32 {
    fn into_number(self) -> Number {
        Number::from_f64(self as f64).expect("UI numbers must be finite")
    }
}

impl IntoNumber for Number {
    fn into_number(self) -> Number {
        self
    }
}

/// A display range on a variable/gauge (pydoover `ui.Range`).
#[derive(Debug, Clone, PartialEq)]
pub struct Range {
    /// Emitted only when non-empty (pydoover: `if self.label`).
    pub label: Option<String>,
    /// `None` emits `"min": null` (pydoover emits min/max unconditionally).
    pub min: Option<Number>,
    pub max: Option<Number>,
    pub colour: String,
    pub show_on_graph: bool,
}

impl Range {
    /// Positional form matching pydoover
    /// `Range(label, min_val, max_val, colour)`.
    pub fn new(
        label: impl Into<String>,
        min: impl IntoNumber,
        max: impl IntoNumber,
        colour: impl Into<String>,
    ) -> Self {
        Self {
            label: Some(label.into()),
            min: Some(min.into_number()),
            max: Some(max.into_number()),
            colour: colour.into(),
            show_on_graph: true,
        }
    }

    pub fn show_on_graph(mut self, show: bool) -> Self {
        self.show_on_graph = show;
        self
    }

    /// pydoover `Range.to_dict()`: `min, max, colour, show_on_graph[, label]`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("min".into(), self.min.clone().map_or(Value::Null, Value::Number));
        m.insert("max".into(), self.max.clone().map_or(Value::Null, Value::Number));
        m.insert("colour".into(), Value::String(self.colour.clone()));
        m.insert("show_on_graph".into(), Value::Bool(self.show_on_graph));
        if let Some(label) = &self.label {
            if !label.is_empty() {
                m.insert("label".into(), Value::String(label.clone()));
            }
        }
        Value::Object(m)
    }
}

/// A threshold line on a variable's plot (pydoover `ui.Threshold`).
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    pub label: String,
    pub value: Number,
    pub colour: String,
}

impl Threshold {
    pub fn new(label: impl Into<String>, value: impl IntoNumber, colour: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into_number(), colour: colour.into() }
    }

    /// pydoover `Threshold.to_dict()`: `label, value, colour`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("label".into(), Value::String(self.label.clone()));
        m.insert("value".into(), Value::Number(self.value.clone()));
        m.insert("colour".into(), Value::String(self.colour.clone()));
        Value::Object(m)
    }
}

/// Selectable views for plotting ranges/thresholds (pydoover `ui.RangeView`).
pub struct RangeView;

impl RangeView {
    /// Show thresholds as horizontal lines.
    pub const LINE: &'static str = "line";
    /// Shade the area covered by each range.
    pub const ZONE: &'static str = "zone";
    /// Don't show ranges or thresholds.
    pub const OFF: &'static str = "off";
}

/// How applications are displayed to users (pydoover
/// `ui.ApplicationVariant`).
pub struct ApplicationVariant;

impl ApplicationVariant {
    /// Embed the application in its own submodule.
    pub const SUBMODULE: &'static str = "submodule";
    /// Stack applications without submodule partitioning.
    pub const STACKED: &'static str = "stacked";
}

/// One series of a [`Multiplot`](super::Multiplot) — pydoover `ui.Series`.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// The key in the plot's `series` map —
    /// `sanitize_display_name(display_name)` unless overridden.
    pub name: String,
    pub display_name: String,
    /// The bound tag the series data is looked up from, emitted as
    /// `lookup`. [`UiValue::Missing`] (or a `null` literal — pydoover's
    /// `value=None`) omits the key.
    pub value: UiValue,
    /// `dataType`: `"number"`, `"string"`, `"boolean"` or `"unknown"`
    /// (pydoover's default).
    pub data_type: String,
    pub active: Option<bool>,
    pub colour: Option<String>,
    pub icon: Option<String>,
    /// `sharedAxis` — a bool, or the name of the axis to share.
    pub shared_axis: Option<Value>,
    pub units: Option<String>,
    pub step_labels: Option<Vec<String>>,
    /// A `(min, max)` pair; values may be numbers or `"auto"`.
    pub range: Option<(Value, Value)>,
    /// Zones when the user picks the `"zone"` range view.
    pub ranges: Option<Vec<Range>>,
    /// Lines when the user picks the `"line"` range view.
    pub thresholds: Option<Vec<Threshold>>,
}

impl Series {
    pub fn new(display_name: impl Into<String>, value: impl Into<UiValue>) -> Self {
        let display_name = display_name.into();
        Self {
            name: sanitize_display_name(&display_name),
            display_name,
            value: value.into(),
            data_type: "unknown".to_string(),
            active: None,
            colour: None,
            icon: None,
            shared_axis: None,
            units: None,
            step_labels: None,
            range: None,
            ranges: None,
            thresholds: None,
        }
    }

    /// Override the derived series key (pydoover `name=`).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn data_type(mut self, data_type: impl Into<String>) -> Self {
        self.data_type = data_type.into();
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = Some(active);
        self
    }

    pub fn colour(mut self, colour: impl Into<String>) -> Self {
        self.colour = Some(colour.into());
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Share the y-axis (`true`), or name the axis to share.
    pub fn shared_axis(mut self, shared_axis: impl Into<Value>) -> Self {
        self.shared_axis = Some(shared_axis.into());
        self
    }

    pub fn units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    pub fn step_labels(mut self, labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.step_labels = Some(labels.into_iter().map(Into::into).collect());
        self
    }

    /// The y-axis range; values may be numbers or the string `"auto"`.
    pub fn range(mut self, min: impl Into<Value>, max: impl Into<Value>) -> Self {
        self.range = Some((min.into(), max.into()));
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

    /// pydoover `Series.to_dict()`: `name, displayString, dataType`, then
    /// `lookup?, active?, colour?, icon?, sharedAxis?, units?, stepLabels?,
    /// range?, ranges?, thresholds?, live?`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(self.name.clone()));
        m.insert("displayString".into(), Value::String(self.display_name.clone()));
        m.insert("dataType".into(), Value::String(self.data_type.clone()));
        // pydoover gates on `value is not None`.
        if !matches!(&self.value, UiValue::Missing | UiValue::Lit(Value::Null)) {
            if let Some(v) = self.value.to_json() {
                m.insert("lookup".into(), v);
            }
        }
        if let Some(active) = self.active {
            m.insert("active".into(), Value::Bool(active));
        }
        if let Some(colour) = &self.colour {
            m.insert("colour".into(), Value::String(colour.clone()));
        }
        if let Some(icon) = &self.icon {
            m.insert("icon".into(), Value::String(icon.clone()));
        }
        if let Some(shared_axis) = &self.shared_axis {
            m.insert("sharedAxis".into(), shared_axis.clone());
        }
        if let Some(units) = &self.units {
            m.insert("units".into(), Value::String(units.clone()));
        }
        if let Some(labels) = &self.step_labels {
            m.insert(
                "stepLabels".into(),
                Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
            );
        }
        if let Some((min, max)) = &self.range {
            let mut range = Map::new();
            range.insert("min".into(), min.clone());
            range.insert("max".into(), max.clone());
            m.insert("range".into(), Value::Object(range));
        }
        if let Some(ranges) = &self.ranges {
            m.insert("ranges".into(), Value::Array(ranges.iter().map(Range::to_json).collect()));
        }
        if let Some(thresholds) = &self.thresholds {
            m.insert(
                "thresholds".into(),
                Value::Array(thresholds.iter().map(Threshold::to_json).collect()),
            );
        }
        if self.value.is_live() {
            m.insert("live".into(), Value::Bool(true));
        }
        Value::Object(m)
    }
}

/// One choice of a [`Select`](super::Select) — pydoover `ui.Option` (renamed:
/// `Option` is taken in Rust).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectOption {
    /// `sanitize_display_name(display_name)`, the key in the `options` map.
    pub name: String,
    pub display_name: String,
}

impl SelectOption {
    pub fn new(display_name: impl Into<String>) -> Self {
        let display_name = display_name.into();
        Self { name: sanitize_display_name(&display_name), display_name }
    }

    /// pydoover `Option.to_dict()`: `name, displayString, type`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(self.name.clone()));
        m.insert("displayString".into(), Value::String(self.display_name.clone()));
        m.insert("type".into(), Value::String("uiElement".into()));
        Value::Object(m)
    }
}

/// Configuration for an interaction's confirmation dialog (pydoover
/// `ui.ConfirmDialog`) — pass to
/// [`requires_confirm`](super::Button::requires_confirm).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfirmDialog {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub warning_reason: Option<String>,
    pub colour: Option<String>,
    pub help_text: Option<String>,
    pub icon: Option<String>,
}

impl ConfirmDialog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn warning_reason(mut self, warning_reason: impl Into<String>) -> Self {
        self.warning_reason = Some(warning_reason.into());
        self
    }

    pub fn colour(mut self, colour: impl Into<String>) -> Self {
        self.colour = Some(colour.into());
        self
    }

    pub fn help_text(mut self, help_text: impl Into<String>) -> Self {
        self.help_text = Some(help_text.into());
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// pydoover `ConfirmDialog.to_dict()`: `title, subtitle, warningReason,
    /// colour, helpText, icon` — set keys only.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(v) = &self.title {
            m.insert("title".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.subtitle {
            m.insert("subtitle".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.warning_reason {
            m.insert("warningReason".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.colour {
            m.insert("colour".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.help_text {
            m.insert("helpText".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.icon {
            m.insert("icon".into(), Value::String(v.clone()));
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_json_matches_pydoover() {
        let r = Range::new("Low", 0, 15.0, Colour::BLUE);
        assert_eq!(
            serde_json::to_string(&r.to_json()).unwrap(),
            r#"{"min":0,"max":15.0,"colour":"blue","show_on_graph":true,"label":"Low"}"#
        );
    }

    #[test]
    fn range_without_label_omits_it() {
        let mut r = Range::new("", 0, 1, Colour::RED);
        r.label = None;
        let out = serde_json::to_string(&r.to_json()).unwrap();
        assert!(!out.contains("label"), "{out}");
        // empty label is falsy in Python too
        let r2 = Range::new("", 0, 1, Colour::RED);
        assert!(!serde_json::to_string(&r2.to_json()).unwrap().contains("label"));
    }

    #[test]
    fn threshold_and_option_json() {
        let t = Threshold::new("High", 80, Colour::RED);
        assert_eq!(
            serde_json::to_string(&t.to_json()).unwrap(),
            r#"{"label":"High","value":80,"colour":"red"}"#
        );
        let o = SelectOption::new("Fast Mode");
        assert_eq!(
            serde_json::to_string(&o.to_json()).unwrap(),
            r#"{"name":"fast_mode","displayString":"Fast Mode","type":"uiElement"}"#
        );
    }

    #[test]
    fn confirm_dialog_key_order() {
        let d = ConfirmDialog::new().icon("warning").title("Confirm").subtitle("sub");
        assert_eq!(
            serde_json::to_string(&d.to_json()).unwrap(),
            r#"{"title":"Confirm","subtitle":"sub","icon":"warning"}"#
        );
    }
}
