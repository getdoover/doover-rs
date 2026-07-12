//! Container elements — pydoover `ui/submodule.py` ([`Container`],
//! [`Submodule`], [`TabContainer`], [`RemoteComponent`]).
//!
//! Containers hold `Vec<Box<dyn UiElement>>` children and serialize them as
//! a `children` map keyed by element name (pydoover `Container.to_dict`).
//! They participate in `#[derive(Ui)]` trees like any other element: the
//! tree's [`finalize`](super::UiTree::finalize) walks nested children
//! depth-first (children before their container — pydoover's construction
//! order), and [`interaction_names`](super::UiTree::interaction_names)
//! recurses into them (pydoover `UI.get_interactions`).
//!
//! Position quirk: pydoover's `Container.add_children` assigns positions
//! from its own 101-based counter, but only to children whose position is
//! falsy — since `Element.__init__` always stamps the global counter, that
//! path only fires for an explicit `position=None`/`position=0` and is not
//! replicated here.
//!
//! `ui.Application` is deliberately not ported: the root `uiApplication`
//! node comes from [`UiTree::to_schema`](super::UiTree::to_schema) (its
//! `variant` knob lives on the non-declarative pydoover API only).

use serde_json::{Map, Value};

use super::element::{impl_element_common, ElementCommon};
use super::value::UiValue;
use super::UiElement;

/// Serialize children exactly like pydoover `Container.to_dict`:
/// `{name: child.to_dict()}` in insertion order.
fn children_json(children: &[Box<dyn UiElement>]) -> Value {
    let map: Map<String, Value> =
        children.iter().map(|c| (c.name().to_string(), c.to_json())).collect();
    Value::Object(map)
}

/// Generate the child-management builder methods shared by every container.
macro_rules! impl_container_builders {
    ($ty:ty) => {
        impl $ty {
            /// Replace the child list (pydoover's `children=` argument).
            pub fn children(mut self, children: Vec<Box<dyn UiElement>>) -> Self {
                self.children = children;
                self
            }

            /// Append one child (builder form of pydoover `add_children`).
            pub fn child(mut self, child: impl UiElement + 'static) -> Self {
                self.children.push(Box::new(child));
                self
            }

            /// Append children at runtime (pydoover `Container.add_children`).
            pub fn add_children(
                &mut self,
                children: impl IntoIterator<Item = Box<dyn UiElement>>,
            ) {
                self.children.extend(children);
            }

            /// Remove children by name, recursing into nested containers
            /// (pydoover `Container.remove_children`).
            pub fn remove_child(&mut self, name: &str) {
                $crate::ui::UiElement::remove_nested_child(self, name);
            }

            /// Drop every child (pydoover `Container.clear_children`).
            pub fn clear_children(&mut self) {
                self.children.clear();
            }
        }
    };
}

/// A plain container for UI elements (pydoover `ui.Container`, type
/// `uiContainer`).
pub struct Container {
    pub common: ElementCommon,
    pub children: Vec<Box<dyn UiElement>>,
}

impl Container {
    pub fn new(display_name: &str) -> Self {
        Self { common: ElementCommon::new(display_name, Some(true)), children: Vec::new() }
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `Container.to_dict()`: base keys + `children`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiContainer");
        m.insert("children".into(), children_json(&self.children));
        Value::Object(m)
    }
}

impl_element_common!(Container, container);
impl_container_builders!(Container);

/// A submodule grouping logical components (pydoover `ui.Submodule`, type
/// `uiSubmodule`).
pub struct Submodule {
    pub common: ElementCommon,
    pub children: Vec<Box<dyn UiElement>>,
    /// `statusString`. `Some(Value::Null)` is pydoover's explicit
    /// `status=None` (which survives to the wire — only `NotSet` is
    /// filtered).
    pub status: Option<Value>,
    /// Only consulted when `default_open` is unset; emitted inverted as
    /// `defaultOpen`.
    pub is_collapsed: Option<bool>,
    /// `defaultOpen` — a bool or a tag reference (pydoover allows both).
    pub default_open: Option<UiValue>,
}

impl Submodule {
    pub fn new(display_name: &str) -> Self {
        Self {
            common: ElementCommon::new(display_name, Some(true)),
            children: Vec::new(),
            status: None,
            is_collapsed: None,
            default_open: None,
        }
    }

    /// The submodule's status string (`Value::Null` replicates pydoover's
    /// explicit `status=None`, which emits `"statusString": null`).
    pub fn status(mut self, status: impl Into<Value>) -> Self {
        self.status = Some(status.into());
        self
    }

    /// Whether the submodule starts collapsed — ignored when
    /// [`default_open`](Self::default_open) is set (pydoover's precedence).
    pub fn is_collapsed(mut self, is_collapsed: bool) -> Self {
        self.is_collapsed = Some(is_collapsed);
        self
    }

    /// `defaultOpen`: a bool, or a tag reference from a declared
    /// [`Tag<T>`](crate::tags::Tag) handle.
    pub fn default_open(mut self, default_open: impl Into<UiValue>) -> Self {
        self.default_open = Some(default_open.into());
        self
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `Submodule.to_dict()`: base keys + `children`, then
    /// `statusString?`, then `defaultOpen` (from `default_open`, else the
    /// negation of `is_collapsed`).
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiSubmodule");
        m.insert("children".into(), children_json(&self.children));
        if let Some(status) = &self.status {
            m.insert("statusString".into(), status.clone());
        }
        if let Some(open) = &self.default_open {
            if let Some(v) = open.to_json() {
                m.insert("defaultOpen".into(), v);
            }
        } else if let Some(collapsed) = self.is_collapsed {
            m.insert("defaultOpen".into(), Value::Bool(!collapsed));
        }
        Value::Object(m)
    }
}

impl_element_common!(Submodule, container);
impl_container_builders!(Submodule);

/// A container rendering its children as tabs (pydoover `ui.TabContainer`,
/// type `uiTabs`).
pub struct TabContainer {
    pub common: ElementCommon,
    pub children: Vec<Box<dyn UiElement>>,
    /// 0-based index of the tab open by default (`defaultPage`).
    pub default_page: Option<i64>,
}

impl TabContainer {
    pub fn new(display_name: &str) -> Self {
        Self {
            common: ElementCommon::new(display_name, Some(true)),
            children: Vec::new(),
            default_page: None,
        }
    }

    pub fn default_page(mut self, page: i64) -> Self {
        self.default_page = Some(page);
        self
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `TabContainer.to_dict()`: base keys + `children` +
    /// `defaultPage?`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiTabs");
        m.insert("children".into(), children_json(&self.children));
        if let Some(page) = self.default_page {
            m.insert("defaultPage".into(), Value::from(page));
        }
        Value::Object(m)
    }
}

impl_element_common!(TabContainer, container);
impl_container_builders!(TabContainer);

/// A remotely-loaded component (pydoover `ui.RemoteComponent`, type
/// `uiRemoteComponent`). The component URL lands in the base
/// `componentUrl` slot; arbitrary extra keys (pydoover's captured
/// `**kwargs`) are appended after `children` via [`extra`](Self::extra).
pub struct RemoteComponent {
    pub common: ElementCommon,
    pub children: Vec<Box<dyn UiElement>>,
    /// Extra keys appended by pydoover's `res.update(self.kwargs)`.
    pub extras: Map<String, Value>,
}

impl RemoteComponent {
    pub fn new(display_name: &str, component_url: &str) -> Self {
        let mut common = ElementCommon::new(display_name, Some(true));
        common.component_url = Some(component_url.to_string());
        Self { common, children: Vec::new(), extras: Map::new() }
    }

    /// An extra key/value forwarded to the component (pydoover `**kwargs`).
    /// Re-using a key already emitted replaces its value in place, exactly
    /// like Python's `dict.update`.
    pub fn extra(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extras.insert(key.into(), value.into());
        self
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `RemoteComponent.to_dict()`: base keys (with
    /// `componentUrl`) + `children`, then `res.update(kwargs)`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiRemoteComponent");
        m.insert("children".into(), children_json(&self.children));
        for (k, v) in &self.extras {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

impl_element_common!(RemoteComponent, container);
impl_container_builders!(RemoteComponent);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{Button, NumericVariable, TextVariable};
    use serde_json::json;

    #[test]
    fn container_serializes_children_in_order() {
        let c = Container::new("Group")
            .child(NumericVariable::new("Speed"))
            .child(TextVariable::new("Status"));
        let out = c.to_json();
        assert_eq!(out["type"], json!("uiContainer"));
        let keys: Vec<_> = out["children"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["speed", "status"]);
    }

    #[test]
    fn submodule_status_and_default_open() {
        let s = Submodule::new("Pump").status("OK").default_open(false);
        let out = s.to_json();
        assert_eq!(out["statusString"], json!("OK"));
        assert_eq!(out["defaultOpen"], json!(false));

        // explicit status=None survives (pydoover filters only NotSet)
        let s = Submodule::new("Pump").status(Value::Null);
        assert_eq!(s.to_json()["statusString"], Value::Null);

        // is_collapsed only applies when default_open is unset, inverted
        let s = Submodule::new("Pump").is_collapsed(true);
        assert_eq!(s.to_json()["defaultOpen"], json!(false));
        let s = Submodule::new("Pump").is_collapsed(true).default_open(true);
        assert_eq!(s.to_json()["defaultOpen"], json!(true));
    }

    #[test]
    fn tab_container_default_page() {
        let t = TabContainer::new("Tabs").default_page(1);
        let out = t.to_json();
        assert_eq!(out["type"], json!("uiTabs"));
        assert_eq!(out["defaultPage"], json!(1));
        assert!(TabContainer::new("Tabs").to_json().get("defaultPage").is_none());
    }

    #[test]
    fn remote_component_url_and_extras() {
        let r = RemoteComponent::new("Widget", "https://example.com/c.js").extra("foo", "bar");
        let out = r.to_json();
        assert_eq!(out["componentUrl"], json!("https://example.com/c.js"));
        assert_eq!(out["foo"], json!("bar"));
        // componentUrl sits in the base slot, before children
        let keys: Vec<_> = out.as_object().unwrap().keys().map(String::as_str).collect();
        let url_idx = keys.iter().position(|k| *k == "componentUrl").unwrap();
        let children_idx = keys.iter().position(|k| *k == "children").unwrap();
        let foo_idx = keys.iter().position(|k| *k == "foo").unwrap();
        assert!(url_idx < children_idx && children_idx < foo_idx, "{keys:?}");
    }

    #[test]
    fn remove_child_recurses() {
        let mut outer = Container::new("Outer").child(
            Container::new("Inner").child(Button::new("Deep")).child(Button::new("Keep")),
        );
        outer.remove_child("deep");
        let out = outer.to_json();
        assert!(out["children"]["inner"]["children"].get("deep").is_none());
        assert!(out["children"]["inner"]["children"].get("keep").is_some());
    }

    #[test]
    fn nested_children_expose_interactions() {
        let c = Container::new("Outer").child(Button::new("Go"));
        let nested = UiElement::nested_children(&c);
        assert_eq!(nested.len(), 1);
        assert!(nested[0].is_interaction());
    }
}
