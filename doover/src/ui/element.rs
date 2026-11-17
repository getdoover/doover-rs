//! [`ElementCommon`] — the shared attributes and base serialization of every
//! UI element (pydoover `ui/element.py` `Element.__init__` / `to_dict`).

use serde_json::{Map, Value};

use crate::config::sanitize_display_name;

/// pydoover `VALID_NAME_RE = ^[0-9a-zA-Z_]+$`. Panics like pydoover's
/// `ValueError` — element declaration is startup-time code.
pub(crate) fn validate_element_name(name: &str) {
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    assert!(valid, "invalid UI element name: {name:?}. Must be [a-zA-Z0-9_]");
}

/// The base attributes of a pydoover `ui.Element`. `Option<T>` fields model
/// pydoover's `NotSet` (and `None`): unset fields omit their key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ElementCommon {
    /// The element name — defaults to `sanitize_display_name(display_name)`
    /// (pydoover `Element.__init__`), e.g. `"Level"` → `"level"`.
    pub name: String,
    pub display_name: String,
    pub is_available: Option<Value>,
    pub help_str: Option<String>,
    pub verbose_str: Option<String>,
    /// `Some(true)` for plain elements/variables; interactions default this
    /// to unset (pydoover `Interaction.__init__` overrides it to NotSet).
    pub show_activity: Option<bool>,
    /// A [`Widget`](super::Widget) string like `"radialGauge"`.
    pub form: Option<String>,
    pub graphic: Option<String>,
    pub layout: Option<String>,
    pub component_url: Option<String>,
    /// Assigned by [`UiTree::finalize`](super::UiTree::finalize) when unset.
    pub position: Option<i64>,
    pub conditions: Option<Value>,
    /// Emitted as a bare bool by default (`false`); pydoover also allows tag
    /// reference strings here.
    pub hidden: Option<Value>,
    pub units: Option<String>,
    pub icon: Option<String>,
    pub colour: Option<String>,
}

impl ElementCommon {
    /// `show_activity`: `Some(true)` for elements/variables, `None` for
    /// interactions — see the field docs.
    pub(crate) fn new(display_name: &str, show_activity: Option<bool>) -> Self {
        let name = sanitize_display_name(display_name);
        validate_element_name(&name);
        Self {
            name,
            display_name: display_name.to_string(),
            show_activity,
            hidden: Some(Value::Bool(false)),
            ..Default::default()
        }
    }

    /// The base key order of pydoover `Element.to_dict()`, with its
    /// None/NotSet filter applied. Subclass extras are appended by the
    /// caller.
    pub(crate) fn base_json(&self, ty: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(self.name.clone()));
        m.insert("type".into(), Value::String(ty.to_string()));
        m.insert("displayString".into(), Value::String(self.display_name.clone()));
        if let Some(v) = &self.is_available {
            m.insert("isAvailable".into(), v.clone());
        }
        if let Some(v) = &self.help_str {
            m.insert("helpString".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.verbose_str {
            m.insert("verboseString".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.show_activity {
            m.insert("showActivity".into(), Value::Bool(v));
        }
        if let Some(v) = &self.form {
            m.insert("form".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.graphic {
            m.insert("graphic".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.layout {
            m.insert("layout".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.component_url {
            m.insert("componentUrl".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.position {
            m.insert("position".into(), Value::from(v));
        }
        if let Some(v) = &self.conditions {
            m.insert("conditions".into(), v.clone());
        }
        if let Some(v) = &self.hidden {
            m.insert("hidden".into(), v.clone());
        }
        if let Some(v) = &self.units {
            m.insert("units".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.icon {
            m.insert("icon".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.colour {
            m.insert("colour".into(), Value::String(v.clone()));
        }
        m
    }
}

/// Generate the shared builder methods + the [`UiElement`](super::UiElement)
/// impl for an element struct. The struct must provide
/// `fn common(&self) -> &ElementCommon`, `fn common_mut(&mut self) -> &mut
/// ElementCommon` and `fn element_json(&self) -> serde_json::Value`.
macro_rules! impl_element_common {
    // `interaction` marks the element as command-accepting
    // (`UiElement::is_interaction`).
    ($ty:ty, interaction) => {
        $crate::ui::element::impl_element_common!(@builders $ty);
        $crate::ui::element::impl_element_common!(@uielement $ty, true, {});
    };
    // `container` gives the `UiElement` impl nested-children access; the
    // struct must expose a `children: Vec<Box<dyn UiElement>>` field.
    ($ty:ty, container) => {
        $crate::ui::element::impl_element_common!(@builders $ty);
        $crate::ui::element::impl_element_common!(@uielement $ty, false, {
            fn nested_children(&self) -> ::std::vec::Vec<&dyn $crate::ui::UiElement> {
                self.children
                    .iter()
                    .map(|c| c.as_ref() as &dyn $crate::ui::UiElement)
                    .collect()
            }

            fn nested_children_mut(
                &mut self,
            ) -> ::std::vec::Vec<&mut dyn $crate::ui::UiElement> {
                self.children
                    .iter_mut()
                    .map(|c| c.as_mut() as &mut dyn $crate::ui::UiElement)
                    .collect()
            }

            fn remove_nested_child(&mut self, name: &str) {
                self.children.retain(|c| c.name() != name);
                for child in &mut self.children {
                    child.remove_nested_child(name);
                }
            }
        });
    };
    ($ty:ty) => {
        $crate::ui::element::impl_element_common!(@builders $ty);
        $crate::ui::element::impl_element_common!(@uielement $ty, false, {});
    };
    (@builders $ty:ty) => {
        impl $ty {
            /// Override the derived element name (pydoover `name=` kwarg).
            /// Note: shadows `UiElement::name` for dot-calls on the concrete
            /// type; use `UiElement::name(&e)` to read.
            pub fn name(mut self, name: impl Into<::std::string::String>) -> Self {
                let name = name.into();
                $crate::ui::element::validate_element_name(&name);
                self.common_mut().name = name;
                self
            }

            /// Declare an explicit position (skips the auto counter).
            pub fn position(mut self, position: i64) -> Self {
                self.common_mut().position = ::core::option::Option::Some(position);
                self
            }

            pub fn hidden(mut self, hidden: bool) -> Self {
                self.common_mut().hidden =
                    ::core::option::Option::Some(::serde_json::Value::Bool(hidden));
                self
            }

            pub fn units(mut self, units: impl Into<::std::string::String>) -> Self {
                self.common_mut().units = ::core::option::Option::Some(units.into());
                self
            }

            pub fn help_str(mut self, help_str: impl Into<::std::string::String>) -> Self {
                self.common_mut().help_str = ::core::option::Option::Some(help_str.into());
                self
            }

            pub fn verbose_str(mut self, verbose_str: impl Into<::std::string::String>) -> Self {
                self.common_mut().verbose_str = ::core::option::Option::Some(verbose_str.into());
                self
            }

            pub fn show_activity(mut self, show_activity: bool) -> Self {
                self.common_mut().show_activity = ::core::option::Option::Some(show_activity);
                self
            }

            /// A `Widget` string like `"radialGauge"`.
            pub fn form(mut self, form: impl Into<::std::string::String>) -> Self {
                self.common_mut().form = ::core::option::Option::Some(form.into());
                self
            }

            pub fn graphic(mut self, graphic: impl Into<::std::string::String>) -> Self {
                self.common_mut().graphic = ::core::option::Option::Some(graphic.into());
                self
            }

            pub fn layout(mut self, layout: impl Into<::std::string::String>) -> Self {
                self.common_mut().layout = ::core::option::Option::Some(layout.into());
                self
            }

            pub fn component_url(mut self, url: impl Into<::std::string::String>) -> Self {
                self.common_mut().component_url = ::core::option::Option::Some(url.into());
                self
            }

            pub fn conditions(mut self, conditions: ::serde_json::Value) -> Self {
                self.common_mut().conditions = ::core::option::Option::Some(conditions);
                self
            }

            pub fn is_available(mut self, is_available: bool) -> Self {
                self.common_mut().is_available =
                    ::core::option::Option::Some(::serde_json::Value::Bool(is_available));
                self
            }

            pub fn icon(mut self, icon: impl Into<::std::string::String>) -> Self {
                self.common_mut().icon = ::core::option::Option::Some(icon.into());
                self
            }

            pub fn colour(mut self, colour: impl Into<::std::string::String>) -> Self {
                self.common_mut().colour = ::core::option::Option::Some(colour.into());
                self
            }
        }
    };
    (@uielement $ty:ty, $is_interaction:literal, {$($extra:tt)*}) => {
        impl $crate::ui::UiElement for $ty {
            fn name(&self) -> &str {
                &self.common().name
            }

            fn position(&self) -> ::core::option::Option<i64> {
                self.common().position
            }

            fn set_position_if_unset(&mut self, position: i64) {
                let common = self.common_mut();
                if common.position.is_none() {
                    common.position = ::core::option::Option::Some(position);
                }
            }

            fn to_json(&self) -> ::serde_json::Value {
                self.element_json()
            }

            fn is_interaction(&self) -> bool {
                $is_interaction
            }

            $($extra)*
        }
    };
}

pub(crate) use impl_element_common;

/// How a device connects (pydoover `ui.ConnectionType`). Serialized as its
/// lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionType {
    #[default]
    Constant,
    Periodic,
    Other,
}

impl ConnectionType {
    fn as_str(self) -> &'static str {
        match self {
            ConnectionType::Constant => "constant",
            ConnectionType::Periodic => "periodic",
            ConnectionType::Other => "other",
        }
    }
}

/// Connection info element (pydoover `ui.ConnectionInfo`, type
/// `uiConnectionInfo`).
///
/// Unlike other elements, its `to_dict` is fully custom: only `name`, `type`,
/// `connectionType` and the periodic extras are emitted — no display string,
/// position or other base attributes (though the element still consumes a
/// slot of the position counter, exactly like pydoover, where
/// `Element.__init__` runs).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionInfo {
    /// pydoover defaults the name to `"connectionInfo"`.
    pub name: String,
    pub connection_type: ConnectionType,
    /// Expected seconds between connection events (periodic only).
    pub connection_period: Option<i64>,
    /// Expected seconds until the next connection (periodic only).
    pub next_connection: Option<i64>,
    /// Show as offline if disconnected for more than this many seconds.
    pub offline_after: Option<i64>,
    /// Allowed consecutive missed connections (periodic only).
    pub allowed_misses: Option<i64>,
    /// Consumed from the position counter but never serialized.
    position: Option<i64>,
}

impl ConnectionInfo {
    pub fn new(connection_type: ConnectionType) -> Self {
        Self {
            name: "connectionInfo".to_string(),
            connection_type,
            connection_period: None,
            next_connection: None,
            offline_after: None,
            allowed_misses: None,
            position: None,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Panics unless the connection type is periodic (pydoover raises
    /// `RuntimeError` from `__init__` for the same combination).
    pub fn connection_period(mut self, secs: i64) -> Self {
        self.assert_periodic("connection_period");
        self.connection_period = Some(secs);
        self
    }

    /// Panics unless the connection type is periodic (see
    /// [`connection_period`](Self::connection_period)).
    pub fn next_connection(mut self, secs: i64) -> Self {
        self.assert_periodic("next_connection");
        self.next_connection = Some(secs);
        self
    }

    /// Allowed for every connection type (pydoover's validation exempts it).
    pub fn offline_after(mut self, secs: i64) -> Self {
        self.offline_after = Some(secs);
        self
    }

    /// Panics unless the connection type is periodic (see
    /// [`connection_period`](Self::connection_period)).
    pub fn allowed_misses(mut self, misses: i64) -> Self {
        self.assert_periodic("allowed_misses");
        self.allowed_misses = Some(misses);
        self
    }

    fn assert_periodic(&self, what: &str) {
        assert!(
            self.connection_type == ConnectionType::Periodic,
            "connection_type must be periodic to set {what}"
        );
    }
}

impl crate::ui::UiElement for ConnectionInfo {
    fn name(&self) -> &str {
        &self.name
    }

    fn position(&self) -> Option<i64> {
        self.position
    }

    fn set_position_if_unset(&mut self, position: i64) {
        if self.position.is_none() {
            self.position = Some(position);
        }
    }

    /// pydoover `ConnectionInfo.to_dict()`: `name, type, connectionType`,
    /// then `connectionPeriod`/`nextConnection`/`offlineAfter`/
    /// `allowedMisses` when set.
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(self.name.clone()));
        m.insert("type".into(), Value::String("uiConnectionInfo".into()));
        m.insert(
            "connectionType".into(),
            Value::String(self.connection_type.as_str().into()),
        );
        if let Some(v) = self.connection_period {
            m.insert("connectionPeriod".into(), Value::from(v));
        }
        if let Some(v) = self.next_connection {
            m.insert("nextConnection".into(), Value::from(v));
        }
        if let Some(v) = self.offline_after {
            m.insert("offlineAfter".into(), Value::from(v));
        }
        if let Some(v) = self.allowed_misses {
            m.insert("allowedMisses".into(), Value::from(v));
        }
        Value::Object(m)
    }
}

/// A multi-series plot (pydoover `ui.Multiplot`, type `uiMultiPlot`).
///
/// The constructor's `title` doubles as the display name (pydoover passes it
/// through to `Element.__init__`), and — matching pydoover, where
/// `self.title` is never `None` — the `title` key is always emitted.
#[derive(Debug, Clone, PartialEq)]
pub struct Multiplot {
    pub common: ElementCommon,
    pub series: Vec<super::misc::Series>,
    pub title: String,
    /// `earliestDataDate`, epoch seconds.
    pub earliest_data_time: Option<i64>,
    pub default_zoom: Option<String>,
    /// `"line"`, `"zone"` or `"off"` (pydoover `ui.RangeView`).
    pub default_range_view: Option<String>,
}

impl Multiplot {
    pub fn new(title: &str) -> Self {
        Self {
            common: ElementCommon::new(title, Some(true)),
            series: Vec::new(),
            title: title.to_string(),
            earliest_data_time: None,
            default_zoom: None,
            default_range_view: None,
        }
    }

    pub fn series(mut self, series: Vec<super::misc::Series>) -> Self {
        self.series = series;
        self
    }

    pub fn push_series(mut self, series: super::misc::Series) -> Self {
        self.series.push(series);
        self
    }

    /// Earliest time data is available (pydoover `earliest_data_time`,
    /// emitted as whole epoch seconds).
    pub fn earliest_data_time_epoch(mut self, epoch_secs: i64) -> Self {
        self.earliest_data_time = Some(epoch_secs);
        self
    }

    pub fn default_zoom(mut self, zoom: impl Into<String>) -> Self {
        self.default_zoom = Some(zoom.into());
        self
    }

    pub fn default_range_view(mut self, view: impl Into<String>) -> Self {
        self.default_range_view = Some(view.into());
        self
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `Multiplot.to_dict()`: base keys, `series` (a map keyed by
    /// series name), `defaultZoom?`, `defaultRangeView?`, `title` (always),
    /// `earliestDataDate?`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiMultiPlot");
        let series: Map<String, Value> =
            self.series.iter().map(|s| (s.name.clone(), s.to_json())).collect();
        m.insert("series".into(), Value::Object(series));
        if let Some(z) = &self.default_zoom {
            m.insert("defaultZoom".into(), Value::String(z.clone()));
        }
        if let Some(v) = &self.default_range_view {
            m.insert("defaultRangeView".into(), Value::String(v.clone()));
        }
        m.insert("title".into(), Value::String(self.title.clone()));
        if let Some(t) = self.earliest_data_time {
            m.insert("earliestDataDate".into(), Value::from(t));
        }
        Value::Object(m)
    }
}

impl_element_common!(Multiplot);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_derived_from_display_name() {
        let c = ElementCommon::new("Level Reading", Some(true));
        assert_eq!(c.name, "level_reading");
        assert_eq!(c.display_name, "Level Reading");
        assert_eq!(c.hidden, Some(Value::Bool(false)));
    }

    #[test]
    #[should_panic(expected = "invalid UI element name")]
    fn empty_sanitized_name_panics() {
        ElementCommon::new("!!!", Some(true));
    }

    #[test]
    fn base_json_key_order_and_filter() {
        let c = ElementCommon::new("Level", Some(true));
        let m = c.base_json("uiVariable");
        let keys: Vec<_> = m.keys().map(String::as_str).collect();
        // unset optionals are filtered; hidden=false survives (it's a value,
        // not NotSet) — pydoover filters only None/NotSet.
        assert_eq!(keys, ["name", "type", "displayString", "showActivity", "hidden"]);
    }
}
