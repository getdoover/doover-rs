//! Declarative UI — the port of `pydoover.ui` (schema half).
//!
//! Element structs with public fields + builder methods own byte-exact JSON
//! emission matching pydoover's `Element.to_dict()` family; `#[derive(Ui)]`
//! (from `doover-macros`) supplies the reflection ([`UiTree`]) over a struct
//! of element fields; construction is explicit via [`UiBuild`]. `Option<T>`
//! fields model pydoover's `NotSet` — an unset field omits its key.
//!
//! Runtime wiring lives in [`runtime`]: [`UiRuntime`] publishes the schema to
//! `ui_state` and caches `ui_cmds` values; incoming commands are delivered to
//! [`Application::on_ui_command`](crate::Application::on_ui_command) as
//! [`UiCommand`]s.
//!
//! Ported elements: the variables ([`NumericVariable`], [`TextVariable`],
//! [`BooleanVariable`], [`DateTimeVariable`], [`Timestamp`]), the
//! interactions ([`Button`], [`Switch`], [`Slider`], [`Select`],
//! [`WarningIndicator`]), the parameter inputs ([`FloatInput`],
//! [`TextInput`], [`DatetimeInput`], [`TimeInput`]), the containers
//! ([`Container`], [`Submodule`], [`TabContainer`], [`RemoteComponent`]),
//! the cameras ([`CameraLiveView`], [`CameraHistory`]), plus [`Multiplot`] /
//! [`Series`], [`ConnectionInfo`], and the support types [`Range`],
//! [`Threshold`], [`Widget`], [`Colour`], [`RangeView`],
//! [`ApplicationVariant`], [`SelectOption`], [`ConfirmDialog`].
//!
//! Deliberately not ported: pydoover's `BooleanParameter` (its constructor
//! raises `NotImplementedError` — the site has no such element) and the
//! `ui.Application` container (the root `uiApplication` node is produced by
//! [`UiTree::to_schema`] instead).

mod camera;
mod element;
mod interaction;
mod misc;
mod parameter;
pub mod runtime;
mod submodule;
mod value;
mod variable;

use serde_json::{Map, Value};

pub use crate::config::sanitize_display_name;
pub use camera::{CameraHistory, CameraLiveView};
pub use element::{ConnectionInfo, ConnectionType, ElementCommon, Multiplot};
pub use interaction::{
    Button, Confirm, InteractionCommon, Select, Slider, Switch, WarningIndicator,
};
pub use misc::{
    ApplicationVariant, Colour, ConfirmDialog, IntoNumber, Range, RangeView, SelectOption, Series,
    Threshold, Widget,
};
pub use parameter::{DatetimeInput, FloatInput, TextInput, TimeInput};
pub use runtime::{resolve_config_refs, UiCommand, UiRuntime};
pub use submodule::{Container, RemoteComponent, Submodule, TabContainer};
pub use value::UiValue;
pub use variable::{BooleanVariable, DateTimeVariable, NumericVariable, TextVariable, Timestamp};

/// pydoover `Element.__global_position_counter` starts at 50 and is
/// pre-incremented, so the first declared element gets position 51.
const POSITION_COUNTER_START: i64 = 50;

/// One UI element — the runtime counterpart of a pydoover `ui.Element`.
pub trait UiElement: Send {
    /// The element name — the key under the uiApplication `children` map.
    fn name(&self) -> &str;

    fn position(&self) -> Option<i64>;

    /// Assign a position only when none was declared (pydoover's global
    /// position counter applies exactly this way).
    fn set_position_if_unset(&mut self, position: i64);

    /// Serialize exactly like the pydoover `to_dict()` of the counterpart
    /// element class, including key order.
    fn to_json(&self) -> Value;

    /// Whether this element accepts commands (pydoover `Interaction`
    /// subclasses). The runtime only dispatches `ui_cmds` commands whose
    /// method matches an interaction's name.
    fn is_interaction(&self) -> bool {
        false
    }

    /// The nested child elements of a container (pydoover `Container`
    /// subclasses); empty for leaf elements.
    fn nested_children(&self) -> Vec<&dyn UiElement> {
        Vec::new()
    }

    fn nested_children_mut(&mut self) -> Vec<&mut dyn UiElement> {
        Vec::new()
    }

    /// Remove a nested child by name, recursing (pydoover
    /// `Container.remove_children`); a no-op for leaf elements.
    fn remove_nested_child(&mut self, _name: &str) {}
}

/// Walk an element depth-first — nested children before their container,
/// matching pydoover's construction order, where a container's children are
/// constructed (and hit `Element.__global_position_counter`) before the
/// container itself — incrementing the counter once per element visited
/// (pydoover increments it whether or not an explicit `position=` was
/// given) and assigning it to elements without a declared position.
pub fn assign_positions_depth_first(element: &mut dyn UiElement, counter: &mut i64) {
    for child in element.nested_children_mut() {
        assign_positions_depth_first(child, counter);
    }
    *counter += 1;
    element.set_position_if_unset(*counter);
}

/// The root-node literals of a pydoover `ui.UI` subclass — the
/// `__init_subclass__(display_name=…, hidden=…, position=…, default_open=…,
/// icon=…, colour=…)` parameters. [`Default`] reproduces pydoover's defaults
/// verbatim, `$config.app()` templates included.
#[derive(Debug, Clone, PartialEq)]
pub struct UiApplicationInfo {
    pub display_name: Value,
    pub hidden: Value,
    pub position: Value,
    pub default_open: Value,
    pub icon: Value,
    pub colour: Value,
}

impl Default for UiApplicationInfo {
    fn default() -> Self {
        Self {
            display_name: Value::String("$config.app().APP_DISPLAY_NAME".into()),
            // pydoover `UI.__init_subclass__` appends ":boolean:false" to any
            // string `hidden` — including its own default, which already ends
            // in ":boolean:false" — so the exported reference carries a
            // doubled suffix. Replicated verbatim: the golden
            // doover_config.json files contain it.
            hidden: Value::String("$config.app().hidden:boolean:false:boolean:false".into()),
            position: Value::String("$config.app().dv_app_position:number:100".into()),
            default_open: Value::String("$config.app().dv_app_default_open:boolean".into()),
            icon: Value::Null,
            colour: Value::Null,
        }
    }
}

/// Reflection over a struct of UI elements — implemented by `#[derive(Ui)]`,
/// the counterpart of a pydoover `ui.UI` subclass body.
pub trait UiTree {
    /// The elements in declaration (field) order.
    fn children(&self) -> Vec<&dyn UiElement>;

    fn children_mut(&mut self) -> Vec<&mut dyn UiElement>;

    /// Assign positions 51, 52, 53, … to elements without a declared
    /// position — reproducing pydoover's global position counter (which
    /// increments per constructed element whether or not an explicit
    /// position was given) deterministically. Container children are
    /// visited depth-first *before* their container, because pydoover
    /// constructs them first (they're constructor arguments). Idempotent.
    ///
    /// Note pydoover's `Container.add_children` has its own 101-based
    /// fallback counter, but it only applies to children whose position is
    /// *falsy* (an explicit `position=None`/`position=0`) — the global
    /// counter has already stamped everything else — so it is not
    /// replicated here.
    fn finalize(&mut self) {
        let mut counter = POSITION_COUNTER_START;
        for child in self.children_mut() {
            assign_positions_depth_first(child, &mut counter);
        }
    }

    /// Every command-accepting element name, recursing into containers —
    /// pydoover `UI.get_interactions()` (names only).
    fn interaction_names(&self) -> Vec<String> {
        fn collect(element: &dyn UiElement, out: &mut Vec<String>) {
            if element.is_interaction() {
                out.push(element.name().to_string());
            }
            for child in element.nested_children() {
                collect(child, out);
            }
        }
        let mut out = Vec::new();
        for child in self.children() {
            collect(child, &mut out);
        }
        out
    }

    /// Build the root `uiApplication` node exactly like pydoover
    /// `UI.to_schema(resolve_config=False)` (as used by `UI.export` — note
    /// pydoover's export does NOT run `setup()`). Call
    /// [`finalize`](Self::finalize) first so unset positions are assigned.
    fn to_schema(&self, app: &UiApplicationInfo) -> Value {
        let mut m = Map::new();
        m.insert("displayString".into(), app.display_name.clone());
        m.insert("hidden".into(), app.hidden.clone());
        m.insert("position".into(), app.position.clone());
        m.insert("icon".into(), app.icon.clone());
        m.insert("colour".into(), app.colour.clone());
        m.insert("defaultOpen".into(), app.default_open.clone());
        m.insert("type".into(), Value::String("uiApplication".into()));
        m.insert("name".into(), Value::String("$config.app().APP_KEY".into()));
        let children: Map<String, Value> = self
            .children()
            .into_iter()
            .map(|child| (child.name().to_string(), child.to_json()))
            .collect();
        m.insert("children".into(), Value::Object(children));
        Value::Object(m)
    }
}

/// Explicit UI construction from a tags collection — hand-written by app
/// authors (the counterpart of a pydoover `ui.UI` class body referencing
/// declared tags).
pub trait UiBuild: Sized {
    type Tags;

    fn build(tags: &Self::Tags) -> Self;
}

/// UI-less apps (`type Ui = ()`): no children, so the runtime publishes
/// nothing to `ui_state` — matching pydoover, where an app without a dynamic
/// UI skips the runtime schema publish.
impl UiTree for () {
    fn children(&self) -> Vec<&dyn UiElement> {
        Vec::new()
    }

    fn children_mut(&mut self) -> Vec<&mut dyn UiElement> {
        Vec::new()
    }
}

impl UiBuild for () {
    type Tags = ();

    fn build(_tags: &()) -> Self {}
}
