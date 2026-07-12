//! UI interactions — pydoover `ui/interaction.py` (`Interaction`, `Button`,
//! `Switch`, `Slider`, `Select`, `WarningIndicator`).
//!
//! This is the schema layer only: command dispatch (`ui_cmds` subscription,
//! `on_ui_command`) is a later milestone phase.
// TODO(M3 phase B+): port the parameter inputs (FloatInput / TextInput /
// DatetimeInput / TimeInput — pydoover ui/parameter.py).

use std::time::Duration;

use serde_json::{Map, Number, Value};

use super::element::{impl_element_common, ElementCommon};
use super::misc::{ConfirmDialog, IntoNumber, SelectOption};
use super::value::{python_str, UiValue};

/// The `requires_confirm` slot: a bare flag or a customized dialog
/// (pydoover accepts `bool | ConfirmDialog`).
#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    Flag(bool),
    Dialog(ConfirmDialog),
}

impl From<bool> for Confirm {
    fn from(v: bool) -> Self {
        Confirm::Flag(v)
    }
}

impl From<ConfirmDialog> for Confirm {
    fn from(v: ConfirmDialog) -> Self {
        Confirm::Dialog(v)
    }
}

/// The shared attributes of a pydoover `ui.Interaction`.
#[derive(Debug, Clone, PartialEq)]
pub struct InteractionCommon {
    pub element: ElementCommon,
    /// The `currentValue` slot. [`UiValue::Missing`] (the default) derives
    /// `$cmds.app().<name>` at serialization time, exactly like pydoover's
    /// `Interaction.__init__`.
    pub value: UiValue,
    pub default: Option<Value>,
    pub requires_confirm: Option<Confirm>,
    /// Emitted as `global`.
    pub global_interaction: Option<bool>,
    /// Emitted as `commandTimeout` (pydoover `duration_ms`).
    pub command_timeout_ms: Option<i64>,
    pub direct: Option<bool>,
}

impl InteractionCommon {
    pub(crate) fn new(display_name: &str) -> Self {
        Self {
            // Interactions default show_activity to unset (pydoover
            // Interaction.__init__ overrides Element's True with NotSet).
            element: ElementCommon::new(display_name, None),
            value: UiValue::Missing,
            default: None,
            requires_confirm: None,
            global_interaction: None,
            command_timeout_ms: None,
            direct: None,
        }
    }

    /// pydoover `Interaction.to_dict()`: base keys, then `currentValue`,
    /// `requiresConfirm`, `global`, `commandTimeout`, `direct`, `default`
    /// (`showActivity` lands in the base slot when set).
    pub(crate) fn interaction_json(&self, ty: &str) -> Map<String, Value> {
        let mut m = self.element.base_json(ty);
        let current = match self.value.to_json() {
            Some(v) => v,
            None => {
                let mut s = format!("$cmds.app().{}", self.element.name);
                if let Some(default) = &self.default {
                    s.push_str("::");
                    s.push_str(&python_str(default));
                }
                Value::String(s)
            }
        };
        m.insert("currentValue".into(), current);
        if let Some(confirm) = &self.requires_confirm {
            let v = match confirm {
                Confirm::Flag(b) => Value::Bool(*b),
                Confirm::Dialog(d) => d.to_json(),
            };
            m.insert("requiresConfirm".into(), v);
        }
        if let Some(g) = self.global_interaction {
            m.insert("global".into(), Value::Bool(g));
        }
        if let Some(t) = self.command_timeout_ms {
            m.insert("commandTimeout".into(), Value::from(t));
        }
        if let Some(d) = self.direct {
            m.insert("direct".into(), Value::Bool(d));
        }
        if let Some(default) = &self.default {
            m.insert("default".into(), default.clone());
        }
        m
    }
}

/// Generate the interaction-level builder methods (on top of the element
/// commons). The struct must expose `interaction: InteractionCommon`.
macro_rules! impl_interaction_builders {
    ($ty:ty) => {
        impl $ty {
            /// Override the `currentValue` slot (pydoover `value=`, e.g. a
            /// tag reference). Unset derives `$cmds.app().<name>`.
            pub fn value(mut self, value: impl Into<UiValue>) -> Self {
                self.interaction.value = value.into();
                self
            }

            /// The default command value; also appended to the derived
            /// `$cmds.app().<name>::<default>` reference.
            pub fn default(mut self, default: impl Into<Value>) -> Self {
                self.interaction.default = Some(default.into());
                self
            }

            /// Require a confirmation dialog: `true` for the default dialog
            /// or a [`ConfirmDialog`] to customize it.
            pub fn requires_confirm(mut self, confirm: impl Into<Confirm>) -> Self {
                self.interaction.requires_confirm = Some(confirm.into());
                self
            }

            pub fn global_interaction(mut self, global: bool) -> Self {
                self.interaction.global_interaction = Some(global);
                self
            }

            /// How long the site waits for the device to acknowledge a
            /// command before marking it failed (emitted in ms).
            pub fn command_timeout(mut self, timeout: Duration) -> Self {
                self.interaction.command_timeout_ms = Some(timeout.as_millis() as i64);
                self
            }

            /// Write commands straight into the `ui_cmds` aggregate instead
            /// of the RPC transaction flow.
            pub fn direct(mut self, direct: bool) -> Self {
                self.interaction.direct = Some(direct);
                self
            }

            fn common(&self) -> &ElementCommon {
                &self.interaction.element
            }

            fn common_mut(&mut self) -> &mut ElementCommon {
                &mut self.interaction.element
            }
        }

        impl_element_common!($ty, interaction);
    };
}

pub(crate) use impl_interaction_builders;

/// A push button (pydoover `ui.Button`, type `uiButton`).
#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub interaction: InteractionCommon,
    pub disabled: Option<bool>,
    /// Emitted as `labelString`.
    pub label_string: Option<String>,
}

impl Button {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name), disabled: None, label_string: None }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = Some(disabled);
        self
    }

    pub fn label_string(mut self, label: impl Into<String>) -> Self {
        self.label_string = Some(label.into());
        self
    }

    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiButton");
        if let Some(d) = self.disabled {
            m.insert("disabled".into(), Value::Bool(d));
        }
        if let Some(l) = &self.label_string {
            m.insert("labelString".into(), Value::String(l.clone()));
        }
        Value::Object(m)
    }
}

impl_interaction_builders!(Button);

/// An on/off switch (pydoover `ui.Switch`, type `uiSwitch` — no extra keys).
#[derive(Debug, Clone, PartialEq)]
pub struct Switch {
    pub interaction: InteractionCommon,
}

impl Switch {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name) }
    }

    fn element_json(&self) -> Value {
        Value::Object(self.interaction.interaction_json("uiSwitch"))
    }
}

impl_interaction_builders!(Switch);

/// A slider (pydoover `ui.Slider`, type `uiSlider`). `min`/`max`/`stepSize`/
/// `dualSlider`/`isInverted` are always emitted, with pydoover's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Slider {
    pub interaction: InteractionCommon,
    pub min_val: Number,
    pub max_val: Number,
    pub step_size: Number,
    pub dual_slider: bool,
    /// Emitted as `isInverted`.
    pub inverted: bool,
    pub colours: Option<String>,
}

impl Slider {
    pub fn new(display_name: &str) -> Self {
        Self {
            interaction: InteractionCommon::new(display_name),
            // pydoover defaults: min_val=0, max_val=100 (ints), step_size=0.1.
            min_val: Number::from(0),
            max_val: Number::from(100),
            step_size: Number::from_f64(0.1).unwrap(),
            dual_slider: true,
            inverted: true,
            colours: None,
        }
    }

    pub fn min_val(mut self, min: impl IntoNumber) -> Self {
        self.min_val = min.into_number();
        self
    }

    pub fn max_val(mut self, max: impl IntoNumber) -> Self {
        self.max_val = max.into_number();
        self
    }

    pub fn step_size(mut self, step: impl IntoNumber) -> Self {
        self.step_size = step.into_number();
        self
    }

    pub fn dual_slider(mut self, dual: bool) -> Self {
        self.dual_slider = dual;
        self
    }

    pub fn inverted(mut self, inverted: bool) -> Self {
        self.inverted = inverted;
        self
    }

    /// e.g. `"red,green,blue"`.
    pub fn colours(mut self, colours: impl Into<String>) -> Self {
        self.colours = Some(colours.into());
        self
    }

    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiSlider");
        m.insert("min".into(), Value::Number(self.min_val.clone()));
        m.insert("max".into(), Value::Number(self.max_val.clone()));
        m.insert("stepSize".into(), Value::Number(self.step_size.clone()));
        m.insert("dualSlider".into(), Value::Bool(self.dual_slider));
        m.insert("isInverted".into(), Value::Bool(self.inverted));
        if let Some(c) = &self.colours {
            m.insert("colours".into(), Value::String(c.clone()));
        }
        Value::Object(m)
    }
}

impl_interaction_builders!(Slider);

/// A dropdown selection (pydoover `ui.Select`, type `uiSelect`).
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub interaction: InteractionCommon,
    pub options: Vec<SelectOption>,
}

impl Select {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name), options: Vec::new() }
    }

    pub fn options(mut self, options: Vec<SelectOption>) -> Self {
        self.options = options;
        self
    }

    pub fn option(mut self, option: SelectOption) -> Self {
        self.options.push(option);
        self
    }

    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiSelect");
        let options: Map<String, Value> =
            self.options.iter().map(|o| (o.name.clone(), o.to_json())).collect();
        m.insert("options".into(), Value::Object(options));
        Value::Object(m)
    }
}

impl_interaction_builders!(Select);

/// A cancellable warning banner (pydoover `ui.WarningIndicator`, type
/// `uiWarningIndicator`; note the snake_case `can_cancel` wire key).
#[derive(Debug, Clone, PartialEq)]
pub struct WarningIndicator {
    pub interaction: InteractionCommon,
    pub can_cancel: bool,
}

impl WarningIndicator {
    pub fn new(display_name: &str) -> Self {
        Self { interaction: InteractionCommon::new(display_name), can_cancel: true }
    }

    pub fn can_cancel(mut self, can_cancel: bool) -> Self {
        self.can_cancel = can_cancel;
        self
    }

    fn element_json(&self) -> Value {
        let mut m = self.interaction.interaction_json("uiWarningIndicator");
        m.insert("can_cancel".into(), Value::Bool(self.can_cancel));
        Value::Object(m)
    }
}

impl_interaction_builders!(WarningIndicator);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiElement;
    use serde_json::json;

    #[test]
    fn button_derives_cmds_reference() {
        let mut b = Button::new("Test Button");
        b.set_position_if_unset(51);
        assert_eq!(
            serde_json::to_string(&b.to_json()).unwrap(),
            r#"{"name":"test_button","type":"uiButton","displayString":"Test Button","position":51,"hidden":false,"currentValue":"$cmds.app().test_button"}"#
        );
    }

    #[test]
    fn interaction_default_suffix_uses_python_str() {
        let s = Switch::new("Pump").default(true);
        let out = s.to_json();
        // pydoover: f"$cmds.app().{name}::{default}" — Python str(True)
        assert_eq!(out["currentValue"], json!("$cmds.app().pump::True"));
        assert_eq!(out["default"], json!(true));
    }

    #[test]
    fn interaction_key_order_with_extras() {
        let b = Button::new("Danger")
            .requires_confirm(true)
            .command_timeout(Duration::from_secs(10))
            .direct(true)
            .show_activity(false)
            .disabled(false)
            .label_string("Go");
        let out = b.to_json();
        let keys: Vec<_> = out.as_object().unwrap().keys().map(String::as_str).collect();
        // showActivity lands in the base Element slot (pydoover re-assigns an
        // existing key, which does not move it).
        assert_eq!(
            keys,
            [
                "name",
                "type",
                "displayString",
                "showActivity",
                "hidden",
                "currentValue",
                "requiresConfirm",
                "commandTimeout",
                "direct",
                "disabled",
                "labelString"
            ]
        );
        assert_eq!(out["commandTimeout"], json!(10000));
    }

    #[test]
    fn slider_defaults_match_pydoover() {
        let s = Slider::new("Speed Limit");
        let out = s.to_json();
        assert_eq!(out["min"], json!(0));
        assert_eq!(out["max"], json!(100));
        assert_eq!(out["stepSize"], json!(0.1));
        assert_eq!(out["dualSlider"], json!(true));
        assert_eq!(out["isInverted"], json!(true));
        assert!(out.get("colours").is_none());
    }

    #[test]
    fn select_options_map() {
        let s = Select::new("Mode")
            .option(SelectOption::new("Fast Mode"))
            .option(SelectOption::new("Slow Mode"));
        let out = s.to_json();
        assert_eq!(out["options"]["fast_mode"]["displayString"], json!("Fast Mode"));
        assert_eq!(out["options"]["slow_mode"]["type"], json!("uiElement"));
    }

    #[test]
    fn warning_indicator_always_emits_can_cancel() {
        let w = WarningIndicator::new("Low Level");
        assert_eq!(w.to_json()["can_cancel"], json!(true));
        let w = w.can_cancel(false);
        assert_eq!(w.to_json()["can_cancel"], json!(false));
    }

    #[test]
    fn confirm_dialog_serializes_in_requires_confirm() {
        let b = Button::new("Reset").requires_confirm(ConfirmDialog::new().title("Sure?"));
        assert_eq!(b.to_json()["requiresConfirm"], json!({"title": "Sure?"}));
    }
}
