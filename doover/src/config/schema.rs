//! Runtime config-schema model, mirroring `pydoover/config/__init__.py`
//! (`Schema.to_schema()` and `ConfigElement.to_dict()` and its subclasses).
//!
//! This is the byte-format source of truth: [`SchemaModel::to_json`] must
//! reproduce pydoover's JSON-Schema draft-2020-12 emission exactly, including
//! key order. The `#[derive(Config)]` macros build these values; they can also
//! be constructed by hand as a dynamic-schema escape hatch.
//!
//! Key-order rules ported from `ConfigElement.to_dict()`:
//!
//! - Base order: `title, x-name, x-hidden, [format], type, x-required,
//!   [description], [default], [x-position], [deprecated], [x-advanced]`,
//!   then subclass extras.
//! - `Enum.to_dict()` prepends `"enum"` FIRST (`{"enum": …, **super().to_dict()}`).
//! - Integer/Number append `minimum, exclusiveMinimum, maximum,
//!   exclusiveMaximum, multipleOf`; String appends `length, pattern`; Array
//!   appends `items, minItems, maxItems, uniqueItems`; Object appends
//!   `properties, additionalElements, required, x-collapsible,
//!   x-defaultCollapsed`.
//! - An element is required iff it has no default (pydoover's `NotSet`),
//!   unless explicitly overridden. Optional elements get `"type": [T, "null"]`
//!   and `x-required: false`.
//!
//! Numeric slots hold [`serde_json::Number`] so integers are never coerced to
//! floats — `"default": 0` must stay `0`, `4.0` must stay `4.0`.

use serde_json::{Map, Number, Value};

/// A comparison operator for a conditional config element
/// (pydoover `config.Comparator`). Equality is the only operator the schema
/// format defines so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparator {
    Equal,
}

/// A condition controlling whether a config element is active
/// (pydoover `config.Condition`, built there by `config.equal()`).
///
/// The controller is named by its `x-name`. pydoover also accepts the
/// `ConfigElement` object itself and resolves it back to a name by identity;
/// there is nothing to resolve here, so the name is the only form.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// `x-name` of the element this one depends on.
    pub element: String,
    pub comparator: Comparator,
    pub value: Value,
}

impl Condition {
    /// Show the element when `element` equals `value` (pydoover
    /// `config.equal()`).
    pub fn equal(element: impl Into<String>, value: Value) -> Self {
        Self { element: element.into(), comparator: Comparator::Equal, value }
    }

    /// The `if` branch's test on the controlling property
    /// (pydoover `_condition_schema`).
    fn to_test(&self) -> Value {
        match self.comparator {
            Comparator::Equal => {
                let mut m = Map::new();
                m.insert("const".into(), self.value.clone());
                Value::Object(m)
            }
        }
    }

    /// Whether this condition holds for a given actual value (pydoover
    /// `_condition_matches`). `None` is pydoover's `NotSet` — an element with no
    /// default never satisfies a condition.
    fn matches(&self, actual: Option<&Value>) -> bool {
        match (self.comparator, actual) {
            (_, None) => false,
            (Comparator::Equal, Some(actual)) => *actual == self.value,
        }
    }
}

/// Numeric constraints shared by integer and number elements
/// (pydoover `config.Integer` / `config.Number` kwargs).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NumericBounds {
    pub minimum: Option<Number>,
    pub exclusive_minimum: Option<Number>,
    pub maximum: Option<Number>,
    pub exclusive_maximum: Option<Number>,
    pub multiple_of: Option<Number>,
}

/// String constraints (pydoover `config.String` kwargs).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StringBounds {
    pub length: Option<u64>,
    pub pattern: Option<String>,
}

/// The element's JSON type plus type-specific extras, one variant per
/// pydoover `ConfigElement` subclass.
#[derive(Debug, Clone, PartialEq)]
pub enum ElementKind {
    Integer(NumericBounds),
    Number(NumericBounds),
    Boolean,
    String(StringBounds),
    /// pydoover `config.Enum`: `ty` is `"string"` if every choice is a
    /// string, `"number"` if every choice is a float, otherwise `None`
    /// (in which case no `type` key is emitted at all).
    Enum {
        choices: Vec<Value>,
        ty: Option<&'static str>,
    },
    Array {
        items: Box<ElementSchema>,
        min_items: Option<u64>,
        max_items: Option<u64>,
        unique_items: Option<bool>,
    },
    Object {
        properties: Vec<ElementSchema>,
        /// pydoover allows `bool` or a schema dict here; default `true`.
        additional_elements: Value,
        collapsible: bool,
        default_collapsed: bool,
    },
}

impl ElementKind {
    /// The JSON-Schema `type` string (pydoover `ConfigElement._type`).
    fn type_name(&self) -> Option<&'static str> {
        match self {
            ElementKind::Integer(_) => Some("integer"),
            ElementKind::Number(_) => Some("number"),
            ElementKind::Boolean => Some("boolean"),
            ElementKind::String(_) => Some("string"),
            ElementKind::Enum { ty, .. } => *ty,
            ElementKind::Array { .. } => Some("array"),
            ElementKind::Object { .. } => Some("object"),
        }
    }
}

/// One config element — the runtime counterpart of a pydoover
/// `ConfigElement` instance.
#[derive(Debug, Clone, PartialEq)]
pub struct ElementSchema {
    /// Display title (pydoover `display_name`).
    pub title: String,
    /// The `x-name` — also the JSON key in `properties` and in deployment
    /// config data.
    pub name: String,
    /// `x-hidden`.
    pub hidden: bool,
    pub format: Option<String>,
    /// Explicit required override (pydoover's `required=` kwarg). `None`
    /// derives required-ness from `default`.
    pub required: Option<bool>,
    pub description: Option<String>,
    /// `None` == pydoover `NotSet` (element is required, no `default` key
    /// emitted). `Some(Value::Null)` == a Python `default=None`, which IS
    /// emitted as `"default": null`.
    pub default: Option<Value>,
    /// `x-position`. Top-level schema elements are 0-based; Object children
    /// are 1-based; Array item elements have no position.
    pub position: Option<u32>,
    pub deprecated: Option<bool>,
    pub advanced: Option<bool>,
    /// When set, this element is *conditional*: it moves out of the object's
    /// `properties`/`required` and into an `allOf` `if`/`then` branch keyed on
    /// another element's value (pydoover's `show_if=config.equal(...)`).
    pub show_if: Option<Condition>,
    pub kind: ElementKind,
}

impl ElementSchema {
    fn new(title: &str, name: &str, kind: ElementKind) -> Self {
        Self {
            title: title.to_string(),
            name: name.to_string(),
            hidden: false,
            format: None,
            required: None,
            description: None,
            default: None,
            position: None,
            deprecated: None,
            advanced: None,
            show_if: None,
            kind,
        }
    }

    pub fn integer(title: &str, name: &str) -> Self {
        Self::new(title, name, ElementKind::Integer(NumericBounds::default()))
    }

    pub fn number(title: &str, name: &str) -> Self {
        Self::new(title, name, ElementKind::Number(NumericBounds::default()))
    }

    pub fn boolean(title: &str, name: &str) -> Self {
        Self::new(title, name, ElementKind::Boolean)
    }

    pub fn string(title: &str, name: &str) -> Self {
        Self::new(title, name, ElementKind::String(StringBounds::default()))
    }

    /// An enum element. The `type` is derived from the choices exactly like
    /// pydoover: all strings → `"string"`, all floats → `"number"`,
    /// otherwise no type key.
    pub fn enumeration(title: &str, name: &str, choices: Vec<Value>) -> Self {
        let ty = if choices.iter().all(|c| c.is_string()) {
            Some("string")
        } else if choices.iter().all(|c| matches!(c, Value::Number(n) if n.is_f64())) {
            Some("number")
        } else {
            None
        };
        Self::new(title, name, ElementKind::Enum { choices, ty })
    }

    pub fn array(title: &str, name: &str, items: ElementSchema) -> Self {
        Self::new(
            title,
            name,
            ElementKind::Array {
                items: Box::new(items),
                min_items: None,
                max_items: None,
                unique_items: None,
            },
        )
    }

    pub fn object(title: &str, name: &str, properties: Vec<ElementSchema>) -> Self {
        Self::new(
            title,
            name,
            ElementKind::Object {
                properties,
                additional_elements: Value::Bool(true),
                collapsible: true,
                default_collapsed: false,
            },
        )
    }

    /// Required iff no default, unless explicitly overridden
    /// (pydoover `ConfigElement.required`).
    pub fn is_required(&self) -> bool {
        self.required.unwrap_or(self.default.is_none())
    }

    /// Whether this element is *active* for a given config object — i.e. its
    /// `show_if` condition holds (pydoover `_condition_matches`).
    ///
    /// An unconditional element is always active. A conditional one reads its
    /// controller's value from `data`, falling back to that controller's
    /// declared default from `siblings`; with neither (pydoover's `NotSet`) the
    /// condition does not hold, so the element stays inactive.
    pub fn condition_holds(&self, data: &Value, siblings: &[ElementSchema]) -> bool {
        let Some(condition) = &self.show_if else { return true };
        let from_data = data.get(&condition.element);
        let actual = match from_data {
            Some(v) => Some(v),
            None => siblings
                .iter()
                .find(|el| el.name == condition.element)
                .and_then(|el| el.default.as_ref()),
        };
        condition.matches(actual)
    }

    fn numeric_bounds_mut(&mut self) -> &mut NumericBounds {
        match &mut self.kind {
            ElementKind::Integer(b) | ElementKind::Number(b) => b,
            _ => panic!("numeric constraint on non-numeric config element {:?}", self.name),
        }
    }

    fn string_bounds_mut(&mut self) -> &mut StringBounds {
        match &mut self.kind {
            ElementKind::String(b) => b,
            _ => panic!("string constraint on non-string config element {:?}", self.name),
        }
    }

    pub fn set_minimum(&mut self, n: Number) {
        self.numeric_bounds_mut().minimum = Some(n);
    }

    pub fn set_exclusive_minimum(&mut self, n: Number) {
        self.numeric_bounds_mut().exclusive_minimum = Some(n);
    }

    pub fn set_maximum(&mut self, n: Number) {
        self.numeric_bounds_mut().maximum = Some(n);
    }

    pub fn set_exclusive_maximum(&mut self, n: Number) {
        self.numeric_bounds_mut().exclusive_maximum = Some(n);
    }

    pub fn set_multiple_of(&mut self, n: Number) {
        self.numeric_bounds_mut().multiple_of = Some(n);
    }

    pub fn set_length(&mut self, n: u64) {
        self.string_bounds_mut().length = Some(n);
    }

    pub fn set_pattern(&mut self, pattern: &str) {
        self.string_bounds_mut().pattern = Some(pattern.to_string());
    }

    pub fn set_min_items(&mut self, n: u64) {
        match &mut self.kind {
            ElementKind::Array { min_items, .. } => *min_items = Some(n),
            _ => panic!("min_items on non-array config element {:?}", self.name),
        }
    }

    pub fn set_max_items(&mut self, n: u64) {
        match &mut self.kind {
            ElementKind::Array { max_items, .. } => *max_items = Some(n),
            _ => panic!("max_items on non-array config element {:?}", self.name),
        }
    }

    pub fn set_unique_items(&mut self, unique: bool) {
        match &mut self.kind {
            ElementKind::Array { unique_items, .. } => *unique_items = Some(unique),
            _ => panic!("unique_items on non-array config element {:?}", self.name),
        }
    }

    /// Emit this element exactly like pydoover `ConfigElement.to_dict()`
    /// (key order relies on serde_json's `preserve_order` feature).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();

        // Enum.to_dict() is `{"enum": choices, **super().to_dict()}` — the
        // enum key lands first.
        if let ElementKind::Enum { choices, .. } = &self.kind {
            m.insert("enum".into(), Value::Array(choices.clone()));
        }

        m.insert("title".into(), Value::String(self.title.clone()));
        m.insert("x-name".into(), Value::String(self.name.clone()));
        m.insert("x-hidden".into(), Value::Bool(self.hidden));

        if let Some(format) = &self.format {
            m.insert("format".into(), Value::String(format.clone()));
        }

        let required = self.is_required();
        if let Some(ty) = self.kind.type_name() {
            if required {
                m.insert("type".into(), Value::String(ty.into()));
            } else {
                // Note: the JSON-Schema nullable type is the STRING "null",
                // not a JSON null.
                m.insert(
                    "type".into(),
                    Value::Array(vec![Value::String(ty.into()), Value::String("null".into())]),
                );
            }
        }
        m.insert("x-required".into(), Value::Bool(required));

        if let Some(description) = &self.description {
            m.insert("description".into(), Value::String(description.clone()));
        }
        if let Some(default) = &self.default {
            m.insert("default".into(), default.clone());
        }
        if let Some(position) = self.position {
            m.insert("x-position".into(), Value::from(position));
        }
        if let Some(deprecated) = self.deprecated {
            m.insert("deprecated".into(), Value::Bool(deprecated));
        }
        if let Some(advanced) = self.advanced {
            m.insert("x-advanced".into(), Value::Bool(advanced));
        }

        match &self.kind {
            ElementKind::Integer(b) | ElementKind::Number(b) => {
                if let Some(n) = &b.minimum {
                    m.insert("minimum".into(), Value::Number(n.clone()));
                }
                if let Some(n) = &b.exclusive_minimum {
                    m.insert("exclusiveMinimum".into(), Value::Number(n.clone()));
                }
                if let Some(n) = &b.maximum {
                    m.insert("maximum".into(), Value::Number(n.clone()));
                }
                if let Some(n) = &b.exclusive_maximum {
                    m.insert("exclusiveMaximum".into(), Value::Number(n.clone()));
                }
                if let Some(n) = &b.multiple_of {
                    m.insert("multipleOf".into(), Value::Number(n.clone()));
                }
            }
            ElementKind::String(b) => {
                if let Some(n) = b.length {
                    m.insert("length".into(), Value::from(n));
                }
                if let Some(p) = &b.pattern {
                    m.insert("pattern".into(), Value::String(p.clone()));
                }
            }
            ElementKind::Array { items, min_items, max_items, unique_items } => {
                m.insert("items".into(), items.to_json());
                if let Some(n) = min_items {
                    m.insert("minItems".into(), Value::from(*n));
                }
                if let Some(n) = max_items {
                    m.insert("maxItems".into(), Value::from(*n));
                }
                if let Some(u) = unique_items {
                    m.insert("uniqueItems".into(), Value::Bool(*u));
                }
            }
            ElementKind::Object { properties, additional_elements, collapsible, default_collapsed } => {
                let object = build_object_schema(properties);
                m.insert("properties".into(), Value::Object(object.properties));
                m.insert("additionalElements".into(), additional_elements.clone());
                m.insert("required".into(), Value::Array(object.required));
                if !object.all_of.is_empty() {
                    m.insert("allOf".into(), Value::Array(object.all_of));
                }
                m.insert("x-collapsible".into(), Value::Bool(*collapsible));
                m.insert("x-defaultCollapsed".into(), Value::Bool(*default_collapsed));
            }
            _ => {}
        }

        Value::Object(m)
    }
}

/// The `properties`, `required` and `allOf` of one object level.
struct ObjectSchema {
    properties: Map<String, Value>,
    required: Vec<Value>,
    all_of: Vec<Value>,
}

/// Split a list of elements into plain properties and conditional `allOf`
/// branches — pydoover `_build_object_schema`, used by both the root schema and
/// every nested `Object`.
///
/// A conditional element does *not* appear in the object's own
/// `properties`/`required`; it lives in the `then` of an `if`/`then` pair keyed
/// on its controller. Elements sharing a controller *and* a condition share one
/// branch, in declaration order, so `sensor_type == "Radar"` produces a single
/// branch holding every radar-only field.
///
/// Misuse — an unknown controller, a self-dependency, or depending on an element
/// that is itself conditional — panics, as pydoover raises `ValueError`. These
/// are declaration errors the derive macro can't catch, not runtime conditions.
fn build_object_schema(elements: &[ElementSchema]) -> ObjectSchema {
    let mut properties = Map::new();
    let mut required = Vec::new();
    // (controller name, condition, elements) — a Vec, not a map, to keep
    // pydoover's declaration ordering of the branches.
    let mut groups: Vec<(&str, &Condition, Vec<&ElementSchema>)> = Vec::new();

    for element in elements {
        let Some(condition) = &element.show_if else {
            properties.insert(element.name.clone(), element.to_json());
            if element.is_required() {
                required.push(Value::String(element.name.clone()));
            }
            continue;
        };

        let controller = elements
            .iter()
            .find(|el| el.name == condition.element)
            .unwrap_or_else(|| {
                panic!(
                    "conditional config element {:?} references unknown element {:?}",
                    element.name, condition.element
                )
            });
        if controller.name == element.name {
            panic!("config element {:?} cannot depend on itself", element.name);
        }
        if controller.show_if.is_some() {
            panic!(
                "config element {:?} cannot depend on conditional element {:?}",
                element.name, controller.name
            );
        }

        match groups
            .iter_mut()
            .find(|(name, cond, _)| *name == controller.name.as_str() && *cond == condition)
        {
            Some((_, _, members)) => members.push(element),
            None => groups.push((&controller.name, condition, vec![element])),
        }
    }

    let all_of = groups
        .into_iter()
        .map(|(controller_name, condition, members)| {
            let then_properties: Map<String, Value> =
                members.iter().map(|el| (el.name.clone(), el.to_json())).collect();
            let mut then = Map::new();
            then.insert("properties".into(), Value::Object(then_properties));
            let then_required: Vec<Value> = members
                .iter()
                .filter(|el| el.is_required())
                .map(|el| Value::String(el.name.clone()))
                .collect();
            if !then_required.is_empty() {
                then.insert("required".into(), Value::Array(then_required));
            }

            let mut if_properties = Map::new();
            if_properties.insert(controller_name.to_string(), condition.to_test());
            let mut if_schema = Map::new();
            if_schema.insert("properties".into(), Value::Object(if_properties));
            // JSON Schema's `properties` matches when the property is absent.
            // That is correct only when the controller's effective default also
            // satisfies the condition; otherwise require the controller to
            // exist, or the branch would fire on a config that omits it.
            let controller = elements
                .iter()
                .find(|el| el.name == controller_name)
                .expect("controller resolved above");
            if !condition.matches(controller.default.as_ref()) {
                if_schema
                    .insert("required".into(), Value::Array(vec![Value::String(
                        controller_name.to_string(),
                    )]));
            }

            let mut branch = Map::new();
            branch.insert("if".into(), Value::Object(if_schema));
            branch.insert("then".into(), Value::Object(then));
            Value::Object(branch)
        })
        .collect();

    ObjectSchema { properties, required, all_of }
}

/// The whole application config schema — pydoover `config.Schema`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaModel {
    /// Root `title`; pydoover defaults this to `"$default"`.
    pub title: Option<String>,
    /// Root `x-advanced` (pydoover `Schema` subclass `advanced=` kwarg);
    /// omitted when `None`.
    pub advanced: Option<bool>,
    pub elements: Vec<ElementSchema>,
}

impl SchemaModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an element. If the element has no position yet, it is assigned
    /// the next 0-based slot (pydoover `Schema.add_element`).
    pub fn push(&mut self, mut element: ElementSchema) {
        if element.position.is_none() {
            element.position = Some(self.elements.len() as u32);
        }
        self.elements.push(element);
    }

    /// Emit the root JSON Schema exactly like pydoover `Schema.to_schema()`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert(
            "$schema".into(),
            Value::String("https://json-schema.org/draft/2020-12/schema".into()),
        );
        m.insert("$id".into(), Value::String(String::new()));
        m.insert(
            "title".into(),
            Value::String(self.title.clone().unwrap_or_else(|| "$default".into())),
        );
        m.insert("type".into(), Value::String("object".into()));

        let object = build_object_schema(&self.elements);
        m.insert("properties".into(), Value::Object(object.properties));
        m.insert("additionalElements".into(), Value::Bool(true));
        m.insert("required".into(), Value::Array(object.required));
        if !object.all_of.is_empty() {
            m.insert("allOf".into(), Value::Array(object.all_of));
        }

        if let Some(advanced) = self.advanced {
            m.insert("x-advanced".into(), Value::Bool(advanced));
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_element_key_order() {
        let mut el = ElementSchema::integer("AI Pin", "ai_pin");
        el.description = Some("Analog input pin number".into());
        el.position = Some(0);
        let out = serde_json::to_string(&el.to_json()).unwrap();
        assert_eq!(
            out,
            r#"{"title":"AI Pin","x-name":"ai_pin","x-hidden":false,"type":"integer","x-required":true,"description":"Analog input pin number","x-position":0}"#
        );
    }

    #[test]
    fn optional_element_nullable_type_and_default() {
        let mut el = ElementSchema::number("Sensor Minimum mA", "sensor_minimum_ma");
        el.default = Some(json!(4.0));
        let v = el.to_json();
        assert_eq!(v["type"], json!(["number", "null"]));
        assert_eq!(v["x-required"], json!(false));
        // int/float distinction survives serialization
        assert!(serde_json::to_string(&v).unwrap().contains("\"default\":4.0"));
    }

    #[test]
    fn enum_key_comes_first() {
        let mut el = ElementSchema::enumeration(
            "Sensor Type",
            "sensor_type",
            vec![json!("Submersible"), json!("Radar")],
        );
        el.default = Some(json!("Submersible"));
        let out = serde_json::to_string(&el.to_json()).unwrap();
        assert!(out.starts_with(r#"{"enum":["Submersible","Radar"],"title":"#), "{out}");
        assert!(out.contains(r#""type":["string","null"]"#));
    }

    #[test]
    fn integer_default_stays_integer() {
        let mut el = ElementSchema::integer("Volume Decimal Precision", "volume_decimal_precision");
        el.default = Some(json!(0));
        let out = serde_json::to_string(&el.to_json()).unwrap();
        assert!(out.ends_with(r#""default":0}"#), "{out}");
    }

    #[test]
    fn root_schema_shape() {
        let mut schema = SchemaModel::new();
        schema.push(ElementSchema::integer("AI Pin", "ai_pin"));
        let mut opt = ElementSchema::number("Full Level", "full_level");
        opt.default = Some(json!(1.0));
        schema.push(opt);

        let v = schema.to_json();
        assert_eq!(v["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
        assert_eq!(v["$id"], json!(""));
        assert_eq!(v["title"], json!("$default"));
        assert_eq!(v["required"], json!(["ai_pin"]));
        assert_eq!(v["properties"]["ai_pin"]["x-position"], json!(0));
        assert_eq!(v["properties"]["full_level"]["x-position"], json!(1));
    }
}
