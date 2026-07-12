//! Derive macros for the `doover` crate's declarative layer: config
//! (`#[derive(Config)]`, `#[derive(ConfigObject)]`, `#[derive(ConfigEnum)]`,
//! mirroring pydoover's `config.Schema` / `config.Object` / `config.Enum`),
//! tags (`#[derive(Tags)]`, mirroring a pydoover `Tags` subclass) and UI
//! (`#[derive(Ui)]`, mirroring a pydoover `ui.UI` subclass body).
//!
//! Unlike pydoover — where the JSON key is derived from the display title via
//! `sanitize_display_name` — the Rust field name IS the key/`x-name`, and a
//! `#[config(title = "…")]` override is checked at compile time to sanitize
//! back to the field name, so a Rust app and a Python app declaring the same
//! config cannot silently diverge.
//!
//! Field grammar (all under `#[config(...)]`):
//!
//! - `title = "…"` — display title (default: Title-Case of the field name).
//! - `name = "…"` — explicit key override (skips the title check).
//! - `default = <expr>` — the field becomes optional with this default;
//!   integer literals stay JSON integers, float literals stay floats.
//! - `item_title = "…"` — the items element title for `Vec<T>` fields
//!   (pydoover: `config.Array("Volume Curve", element=Point("Volume Curve Point"))`).
//! - `hidden`, `advanced`, `deprecated` — flags.
//! - `format = "…"`, `pattern = "…"` — strings.
//! - `min = <n>`, `max = <n>`, `multiple_of = <n>` — numeric bounds.
//! - `min_items = <n>`, `max_items = <n>` — array bounds.
//!
//! `Option<T>` fields are optional with `"default": null` (pydoover
//! `default=None`); plain fields without a default are required.
//!
//! Enum variants take `#[config(rename = "…")]`; the default choice string is
//! the variant name split on camel-case boundaries (`RadarInverted` →
//! `"Radar Inverted"`).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{parse_macro_input, Data, DeriveInput, Fields, FieldsNamed, LitStr, Type};

// ---------------------------------------------------------------------------
// String helpers (compile-time copies of doover::config helpers)
// ---------------------------------------------------------------------------

/// Compile-time copy of pydoover `utils.sanitize_display_name` (also ported
/// at runtime as `doover::config::sanitize_display_name`).
fn sanitize_display_name(name: &str) -> String {
    name.chars()
        .filter_map(|c| match c {
            ' ' => Some('_'),
            c if c.is_ascii_alphanumeric() || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// Default display title from a field name: underscores become spaces and
/// each word is capitalized (`sensor_type` → `"Sensor Type"`).
fn title_case(field_name: &str) -> String {
    field_name
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Default enum choice string: split the variant name on camel-case
/// boundaries (`RadarInverted` → `"Radar Inverted"`).
fn camel_to_title(variant: &str) -> String {
    let mut out = String::new();
    let mut prev_is_lower_or_digit = false;
    for c in variant.chars() {
        if c.is_ascii_uppercase() && prev_is_lower_or_digit {
            out.push(' ');
        }
        prev_is_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
        out.push(c);
    }
    out
}

/// pydoover `check_key`: keys may only contain `[ a-zA-Z0-9_-]`.
fn check_key(key: &str) -> bool {
    key.chars().all(|c| c == ' ' || c == '-' || c == '_' || c.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------------------
// Attribute parsing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FieldAttrs {
    title: Option<String>,
    name: Option<String>,
    default: Option<syn::Expr>,
    item_title: Option<String>,
    hidden: bool,
    advanced: bool,
    deprecated: bool,
    format: Option<String>,
    pattern: Option<String>,
    min: Option<syn::Lit>,
    max: Option<syn::Lit>,
    multiple_of: Option<syn::Lit>,
    min_items: Option<u64>,
    max_items: Option<u64>,
}

fn parse_field_attrs(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let ident = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected a #[config(...)] option name"))?
                .to_string();
            match ident.as_str() {
                "title" => out.title = Some(meta.value()?.parse::<LitStr>()?.value()),
                "name" => out.name = Some(meta.value()?.parse::<LitStr>()?.value()),
                "item_title" => out.item_title = Some(meta.value()?.parse::<LitStr>()?.value()),
                "format" => out.format = Some(meta.value()?.parse::<LitStr>()?.value()),
                "pattern" => out.pattern = Some(meta.value()?.parse::<LitStr>()?.value()),
                "default" => out.default = Some(meta.value()?.parse()?),
                "hidden" => out.hidden = true,
                "advanced" => out.advanced = true,
                "deprecated" => out.deprecated = true,
                "min" => out.min = Some(meta.value()?.parse()?),
                "max" => out.max = Some(meta.value()?.parse()?),
                "multiple_of" => out.multiple_of = Some(meta.value()?.parse()?),
                "min_items" => {
                    out.min_items = Some(meta.value()?.parse::<syn::LitInt>()?.base10_parse()?)
                }
                "max_items" => {
                    out.max_items = Some(meta.value()?.parse::<syn::LitInt>()?.base10_parse()?)
                }
                other => {
                    return Err(meta.error(format!("unknown #[config] option `{other}`")));
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// Extract the `///` doc comment as the element description: lines joined
/// with `\n`, single leading space and surrounding whitespace trimmed.
fn doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = &nv.value {
                let line = s.value();
                lines.push(line.strip_prefix(' ').unwrap_or(&line).to_string());
            }
        }
    }
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then_some(doc)
}

/// A numeric literal as a `serde_json::Number` expression, preserving the
/// int/float distinction.
fn number_tokens(lit: &syn::Lit) -> syn::Result<TokenStream2> {
    match lit {
        syn::Lit::Int(i) => {
            let v: i64 = i.base10_parse()?;
            Ok(quote! { ::doover::__private::serde_json::Number::from(#v) })
        }
        syn::Lit::Float(f) => {
            let v: f64 = f.base10_parse()?;
            Ok(quote! {
                ::doover::__private::serde_json::Number::from_f64(#v)
                    .expect("config bound must be finite")
            })
        }
        other => Err(syn::Error::new(other.span(), "expected an integer or float literal")),
    }
}

// ---------------------------------------------------------------------------
// Field element generation (shared by Config and ConfigObject)
// ---------------------------------------------------------------------------

/// If `ty` is `Wrapper<Inner>` for the given single-segment wrapper name,
/// return `Inner`.
fn generic_inner<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != wrapper {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else { return None };
    if args.args.len() != 1 {
        return None;
    }
    match args.args.first()? {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

/// Generate the block that builds this field's `ElementSchema` and pushes it
/// onto `__els`. `position` is pre-computed by the caller (0-based for
/// top-level schemas, 1-based for Object children).
fn field_element_block(field: &syn::Field, position: u32) -> syn::Result<TokenStream2> {
    let ident = field.ident.as_ref().expect("named field");
    let field_name = ident.to_string();
    let attrs = parse_field_attrs(&field.attrs)?;

    if let Some(name) = &attrs.name {
        if !check_key(name) {
            return Err(syn::Error::new(
                field.span(),
                format!(
                    "invalid config key {name:?}: keys must only contain alphanumeric \
                     characters, hyphens (-), underscores (_) and spaces ( )"
                ),
            ));
        }
    }
    let key = attrs.name.clone().unwrap_or_else(|| field_name.clone());
    let title = attrs.title.clone().unwrap_or_else(|| title_case(&field_name));

    // pydoover derives the key from the title; verify a custom title round-trips
    // to the field name so Rust and Python declarations of the same schema agree.
    if attrs.title.is_some() && attrs.name.is_none() {
        let sanitized = sanitize_display_name(&title);
        if sanitized != field_name {
            return Err(syn::Error::new(
                field.span(),
                format!(
                    "#[config(title = {title:?})] sanitizes to {sanitized:?} but the field is \
                     named {field_name:?}; pydoover derives the config key from the title, so \
                     rename the field to {sanitized:?} or add #[config(name = \"...\")]"
                ),
            ));
        }
    }

    let is_option = generic_inner(&field.ty, "Option").is_some();
    let inner_ty = generic_inner(&field.ty, "Option").unwrap_or(&field.ty);

    // Element constructor per shape.
    let construct = if let Some(item_ty) = generic_inner(inner_ty, "Vec") {
        let Some(item_title) = attrs.item_title.clone() else {
            return Err(syn::Error::new(
                field.span(),
                "Vec<T> config fields need #[config(item_title = \"...\")] — the display \
                 title of the array's item element",
            ));
        };
        quote! {
            let __item = <#item_ty as ::doover::config::ConfigElementBuild>::element(
                #item_title,
                &::doover::config::sanitize_display_name(#item_title),
            );
            let mut __el = ::doover::config::ElementSchema::array(#title, #key, __item);
        }
    } else {
        quote! {
            let mut __el =
                <#inner_ty as ::doover::config::ConfigElementBuild>::element(#title, #key);
        }
    };

    let mut extras: Vec<TokenStream2> = Vec::new();
    extras.push(quote! { __el.position = ::core::option::Option::Some(#position); });

    if let Some(doc) = doc_comment(&field.attrs) {
        extras.push(quote! {
            __el.description = ::core::option::Option::Some(#doc.to_string());
        });
    }
    if attrs.hidden {
        extras.push(quote! { __el.hidden = true; });
    }
    if attrs.advanced {
        extras.push(quote! { __el.advanced = ::core::option::Option::Some(true); });
    }
    if attrs.deprecated {
        extras.push(quote! { __el.deprecated = ::core::option::Option::Some(true); });
    }
    if let Some(format) = &attrs.format {
        extras.push(quote! { __el.format = ::core::option::Option::Some(#format.to_string()); });
    }
    if let Some(pattern) = &attrs.pattern {
        extras.push(quote! { __el.set_pattern(#pattern); });
    }
    if let Some(lit) = &attrs.min {
        let n = number_tokens(lit)?;
        extras.push(quote! { __el.set_minimum(#n); });
    }
    if let Some(lit) = &attrs.max {
        let n = number_tokens(lit)?;
        extras.push(quote! { __el.set_maximum(#n); });
    }
    if let Some(lit) = &attrs.multiple_of {
        let n = number_tokens(lit)?;
        extras.push(quote! { __el.set_multiple_of(#n); });
    }
    if let Some(n) = attrs.min_items {
        extras.push(quote! { __el.set_min_items(#n); });
    }
    if let Some(n) = attrs.max_items {
        extras.push(quote! { __el.set_max_items(#n); });
    }

    if let Some(default) = &attrs.default {
        // Coerce the default expression to the field's (inner) type so the
        // schema value keeps the right JSON number flavour: an integer
        // literal on an i64 field emits a JSON integer, `4.0` on an f64
        // field emits a float, `"L"` on a String field converts via Into.
        extras.push(quote! {
            let __default: #inner_ty = ::core::convert::Into::into(#default);
            __el.default = ::core::option::Option::Some(
                ::doover::config::ToConfigValue::to_config_value(&__default),
            );
        });
    } else if is_option {
        // pydoover `default=None`: emitted as `"default": null`, optional.
        extras.push(quote! {
            __el.default =
                ::core::option::Option::Some(::doover::__private::serde_json::Value::Null);
        });
    }

    Ok(quote! {
        {
            #construct
            #(#extras)*
            __els.push(__el);
        }
    })
}

fn named_fields(input: &DeriveInput, derive: &str) -> syn::Result<FieldsNamed> {
    match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => Ok(f.clone()),
            _ => Err(syn::Error::new(
                input.ident.span(),
                format!("#[derive({derive})] requires a struct with named fields"),
            )),
        },
        _ => Err(syn::Error::new(
            input.ident.span(),
            format!("#[derive({derive})] requires a struct with named fields"),
        )),
    }
}

// ---------------------------------------------------------------------------
// #[derive(Config)]
// ---------------------------------------------------------------------------

/// Derive `doover::ConfigSchema` for an application config struct
/// (pydoover `config.Schema` subclass).
#[proc_macro_derive(Config, attributes(config))]
pub fn derive_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_config(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_config(input: DeriveInput) -> syn::Result<TokenStream2> {
    let fields = named_fields(&input, "Config")?;
    let ident = &input.ident;

    // Struct-level options: `#[config(name = "...")]` sets the root schema
    // title (pydoover `class MyConfig(Schema, name="...")`, default
    // "$default"); `#[config(advanced)]` sets root `x-advanced`.
    let struct_attrs = parse_field_attrs(&input.attrs)?;
    let set_title = struct_attrs.name.as_ref().map(|name| {
        quote! { __m.title = ::core::option::Option::Some(#name.to_string()); }
    });
    let set_advanced = struct_attrs
        .advanced
        .then(|| quote! { __m.advanced = ::core::option::Option::Some(true); });

    let mut builders = Vec::new();
    let mut loads = Vec::new();
    for (idx, field) in fields.named.iter().enumerate() {
        // Top-level schema elements are 0-based (pydoover Schema.add_element).
        builders.push(field_element_block(field, idx as u32)?);
        let fident = field.ident.as_ref().unwrap();
        loads.push(quote! {
            #fident: ::doover::config::load_element(__v, &__els[#idx])?,
        });
    }

    Ok(quote! {
        const _: () = {
            fn __config_elements() -> ::std::vec::Vec<::doover::config::ElementSchema> {
                let mut __els = ::std::vec::Vec::new();
                #(#builders)*
                __els
            }

            #[automatically_derived]
            impl ::doover::config::ConfigSchema for #ident {
                fn schema() -> ::doover::config::SchemaModel {
                    let mut __m = ::doover::config::SchemaModel::new();
                    #set_title
                    #set_advanced
                    for __el in __config_elements() {
                        __m.push(__el);
                    }
                    __m
                }

                fn from_value(
                    __v: &::doover::__private::serde_json::Value,
                ) -> ::doover::error::Result<Self> {
                    let __els = __config_elements();
                    ::core::result::Result::Ok(Self {
                        #(#loads)*
                    })
                }
            }
        };
    })
}

// ---------------------------------------------------------------------------
// #[derive(ConfigObject)]
// ---------------------------------------------------------------------------

/// Derive the element-building and value-loading impls for a nested config
/// object (pydoover `config.Object` subclass), usable as a `Config` field or
/// inside `Vec<T>`.
#[proc_macro_derive(ConfigObject, attributes(config))]
pub fn derive_config_object(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_config_object(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_config_object(input: DeriveInput) -> syn::Result<TokenStream2> {
    let fields = named_fields(&input, "ConfigObject")?;
    let ident = &input.ident;

    let mut builders = Vec::new();
    let mut loads = Vec::new();
    for (idx, field) in fields.named.iter().enumerate() {
        // Object children are 1-based (pydoover Object._add_cls_element
        // assigns positions after insertion, so the first child gets 1).
        builders.push(field_element_block(field, idx as u32 + 1)?);
        let fident = field.ident.as_ref().unwrap();
        loads.push(quote! {
            #fident: ::doover::config::load_element(__v, &__els[#idx])?,
        });
    }

    Ok(quote! {
        const _: () = {
            fn __object_elements() -> ::std::vec::Vec<::doover::config::ElementSchema> {
                let mut __els = ::std::vec::Vec::new();
                #(#builders)*
                __els
            }

            #[automatically_derived]
            impl ::doover::config::ConfigElementBuild for #ident {
                fn element(title: &str, name: &str) -> ::doover::config::ElementSchema {
                    ::doover::config::ElementSchema::object(title, name, __object_elements())
                }
            }

            #[automatically_derived]
            impl ::doover::config::FromConfigValue for #ident {
                fn from_config_value(
                    __v: &::doover::__private::serde_json::Value,
                ) -> ::doover::error::Result<Self> {
                    let __els = __object_elements();
                    ::core::result::Result::Ok(Self {
                        #(#loads)*
                    })
                }
            }
        };
    })
}

// ---------------------------------------------------------------------------
// #[derive(ConfigEnum)]
// ---------------------------------------------------------------------------

/// Derive the choice-string machinery for a fieldless enum (pydoover
/// `config.Enum` with string choices): `Display`/`FromStr` plus the config
/// element and value-loading impls.
#[proc_macro_derive(ConfigEnum, attributes(config))]
pub fn derive_config_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_config_enum(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_config_enum(input: DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new(
            ident.span(),
            "#[derive(ConfigEnum)] requires a fieldless enum",
        ));
    };

    let mut variant_idents = Vec::new();
    let mut choice_strings = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "#[derive(ConfigEnum)] variants must be fieldless",
            ));
        }
        let mut rename = None;
        for attr in &variant.attrs {
            if !attr.path().is_ident("config") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    rename = Some(meta.value()?.parse::<LitStr>()?.value());
                    Ok(())
                } else {
                    Err(meta.error("only #[config(rename = \"...\")] is supported on variants"))
                }
            })?;
        }
        variant_idents.push(&variant.ident);
        choice_strings.push(rename.unwrap_or_else(|| camel_to_title(&variant.ident.to_string())));
    }

    let ident_str = ident.to_string();
    let expected = choice_strings.join("', '");

    Ok(quote! {
        #[automatically_derived]
        impl ::doover::config::ConfigElementBuild for #ident {
            fn element(title: &str, name: &str) -> ::doover::config::ElementSchema {
                ::doover::config::ElementSchema::enumeration(
                    title,
                    name,
                    ::std::vec![
                        #(::doover::__private::serde_json::Value::String(
                            #choice_strings.to_string()
                        )),*
                    ],
                )
            }
        }

        #[automatically_derived]
        impl ::core::fmt::Display for #ident {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #(Self::#variant_idents => f.write_str(#choice_strings),)*
                }
            }
        }

        #[automatically_derived]
        impl ::core::str::FromStr for #ident {
            type Err = ::doover::error::DooverError;

            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                match s {
                    #(#choice_strings => ::core::result::Result::Ok(Self::#variant_idents),)*
                    other => ::core::result::Result::Err(::doover::error::DooverError::Other(
                        ::std::format!(
                            "invalid value '{other}' for enum {}; expected one of: '{}'",
                            #ident_str,
                            #expected,
                        ),
                    )),
                }
            }
        }

        #[automatically_derived]
        impl ::doover::config::FromConfigValue for #ident {
            fn from_config_value(
                v: &::doover::__private::serde_json::Value,
            ) -> ::doover::error::Result<Self> {
                match v.as_str() {
                    ::core::option::Option::Some(s) => <Self as ::core::str::FromStr>::from_str(s),
                    ::core::option::Option::None => {
                        ::core::result::Result::Err(::doover::error::DooverError::Other(
                            ::std::format!("expected string for enum {}, got {v}", #ident_str),
                        ))
                    }
                }
            }
        }

        #[automatically_derived]
        impl ::doover::config::ToConfigValue for #ident {
            fn to_config_value(&self) -> ::doover::__private::serde_json::Value {
                ::doover::__private::serde_json::Value::String(::std::string::ToString::to_string(self))
            }
        }
    })
}

// ---------------------------------------------------------------------------
// #[derive(Tags)]
// ---------------------------------------------------------------------------

/// One field's `#[tag(...)]` options.
#[derive(Default)]
struct TagAttrs {
    live: bool,
    name: Option<String>,
    default: Option<syn::Expr>,
    log_on: Vec<TriggerSpec>,
}

/// One parsed `log_on(...)` trigger: `cross(15.0, deadband = 1.0)`,
/// `delta(percent = 10.0)`, `any_change`, `enter("fault")`, …
struct TriggerSpec {
    name: syn::Ident,
    positional: Vec<syn::Expr>,
    named: Vec<(syn::Ident, syn::Expr)>,
}

impl syn::parse::Parse for TriggerSpec {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let name: syn::Ident = input.parse()?;
        let mut positional = Vec::new();
        let mut named = Vec::new();
        if input.peek(syn::token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            while !inner.is_empty() {
                if inner.peek(syn::Ident) && inner.peek2(syn::Token![=]) {
                    let key: syn::Ident = inner.parse()?;
                    inner.parse::<syn::Token![=]>()?;
                    named.push((key, inner.parse()?));
                } else {
                    positional.push(inner.parse()?);
                }
                if !inner.is_empty() {
                    inner.parse::<syn::Token![,]>()?;
                }
            }
        }
        Ok(Self { name, positional, named })
    }
}

impl TriggerSpec {
    /// Whether this trigger applies to numeric tags (`Some(true)`), to
    /// boolean/string tags (`Some(false)`), or is unknown (`None`).
    fn numeric_kind(&self) -> Option<bool> {
        match self.name.to_string().as_str() {
            "cross" | "rise" | "fall" | "delta" => Some(true),
            "any_change" | "enter" | "exit" => Some(false),
            _ => None,
        }
    }

    /// The `doover::tags::LogTrigger` constructor expression.
    fn to_tokens(&self) -> syn::Result<TokenStream2> {
        let name = self.name.to_string();
        let err = |msg: &str| Err(syn::Error::new(self.name.span(), msg));
        match name.as_str() {
            "cross" | "rise" | "fall" => {
                if self.positional.is_empty() {
                    return err(&format!(
                        "log_on: {name}(...) requires at least one threshold"
                    ));
                }
                let mut deadband = None;
                for (key, value) in &self.named {
                    if key == "deadband" {
                        deadband = Some(value);
                    } else {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("log_on: unknown {name}() option `{key}`; expected `deadband`"),
                        ));
                    }
                }
                let ctor = syn::Ident::new(&name, self.name.span());
                let thresholds = &self.positional;
                let mut tokens = quote! {
                    ::doover::tags::LogTrigger::#ctor(
                        ::std::vec![#((#thresholds) as f64),*]
                    )
                };
                if let Some(d) = deadband {
                    tokens = quote! { #tokens.deadband((#d) as f64) };
                }
                Ok(tokens)
            }
            "delta" => {
                if !self.positional.is_empty() || self.named.len() != 1 {
                    return err(
                        "log_on: delta requires exactly one of `amount = ...` or `percent = ...`",
                    );
                }
                let (key, value) = &self.named[0];
                match key.to_string().as_str() {
                    "amount" => Ok(quote! {
                        ::doover::tags::LogTrigger::delta_amount((#value) as f64)
                    }),
                    "percent" => Ok(quote! {
                        ::doover::tags::LogTrigger::delta_percent((#value) as f64)
                    }),
                    other => Err(syn::Error::new(
                        key.span(),
                        format!("log_on: unknown delta option `{other}`; expected `amount` or `percent`"),
                    )),
                }
            }
            "any_change" => {
                if !self.positional.is_empty() || !self.named.is_empty() {
                    return err("log_on: any_change takes no arguments");
                }
                Ok(quote! { ::doover::tags::LogTrigger::any_change() })
            }
            "enter" | "exit" => {
                if self.positional.len() != 1 || !self.named.is_empty() {
                    return err(&format!("log_on: {name}(...) takes exactly one value"));
                }
                let ctor = syn::Ident::new(&name, self.name.span());
                let value = &self.positional[0];
                Ok(quote! {
                    ::doover::tags::LogTrigger::#ctor(
                        ::doover::__private::serde_json::json!(#value)
                    )
                })
            }
            other => err(&format!(
                "log_on: unknown trigger `{other}`; expected one of cross, rise, fall, \
                 delta, any_change, enter, exit"
            )),
        }
    }
}

fn parse_tag_attrs(attrs: &[syn::Attribute]) -> syn::Result<TagAttrs> {
    let mut out = TagAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("tag") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let ident = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected a #[tag(...)] option name"))?
                .to_string();
            match ident.as_str() {
                "live" => out.live = true,
                "name" => out.name = Some(meta.value()?.parse::<LitStr>()?.value()),
                "default" => out.default = Some(meta.value()?.parse()?),
                "log_on" => {
                    let inner;
                    syn::parenthesized!(inner in meta.input);
                    let specs = inner.parse_terminated(
                        <TriggerSpec as syn::parse::Parse>::parse,
                        syn::Token![,],
                    )?;
                    if specs.is_empty() {
                        return Err(meta.error("log_on(...) needs at least one trigger"));
                    }
                    out.log_on.extend(specs);
                }
                other => {
                    return Err(meta.error(format!("unknown #[tag] option `{other}`")));
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// Whether an expression is the bare path `None` — `#[tag(default = None)]`
/// is pydoover's `Tag(..., default=None)` and becomes a JSON `null` default.
fn expr_is_none(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::Path(p) if p.qself.is_none() && p.path.is_ident("None"))
}

/// Derive `doover::tags::TagsCollection` for a struct of `Tag<T>` fields —
/// the counterpart of a pydoover `Tags` subclass. The **field name is the
/// tag name** (override with `#[tag(name = "...")]`).
///
/// Field grammar (under `#[tag(...)]`): `live` — pydoover `live=True`;
/// `default = <expr>` — the declared default (`default = None` for
/// pydoover's `default=None`, i.e. a JSON `null`); `name = "..."`;
/// `log_on(<trigger>, ...)` — pydoover's `log_on=` auto-logging rules:
///
/// - `cross(15.0)` / `cross(50.0, 100.0, deadband = 4.0)` — pydoover
///   `Cross(*thresholds, deadband=…)`; same shape for `rise(...)` and
///   `fall(...)`. Numeric tags only.
/// - `delta(amount = 5.0)` / `delta(percent = 10.0)` — pydoover `Delta`.
///   Numeric tags only.
/// - `any_change` — pydoover `AnyChange()`. Boolean/string tags only.
/// - `enter(<value>)` / `exit(<value>)` — pydoover `Enter`/`Exit`; the
///   value is any JSON literal (`true`, `"fault"`, …). Boolean/string tags
///   only.
///
/// Trigger/type mismatches are compile errors (pydoover raises `TypeError`
/// at declaration time).
#[proc_macro_derive(Tags, attributes(tag))]
pub fn derive_tags(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_tags(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_tags(input: DeriveInput) -> syn::Result<TokenStream2> {
    let fields = named_fields(&input, "Tags")?;
    let ident = &input.ident;

    let mut field_idents = Vec::new();
    let mut declarations = Vec::new();
    let mut tag_names = Vec::new();
    let mut live_tag_names = Vec::new();

    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field");
        let attrs = parse_tag_attrs(&field.attrs)?;

        let Some(inner) = generic_inner(&field.ty, "Tag") else {
            return Err(syn::Error::new(
                field.span(),
                "#[derive(Tags)] fields must be of type `Tag<T>` (doover::tags::Tag)",
            ));
        };

        let tag_name = attrs.name.clone().unwrap_or_else(|| fident.to_string());
        let live = attrs.live;

        let default_tokens = match &attrs.default {
            None => quote! { ::core::option::Option::None },
            Some(expr) if expr_is_none(expr) => quote! {
                ::core::option::Option::Some(::doover::__private::serde_json::Value::Null)
            },
            Some(expr) => quote! {
                ::core::option::Option::Some({
                    // Coerce through the field's value type so the JSON
                    // number flavour matches the declaration (int stays int).
                    let __default: #inner = ::core::convert::Into::into(#expr);
                    <#inner as ::doover::tags::TagValue>::to_value(&__default)
                })
            },
        };

        // Compile-time trigger/type validation, when the value type is a
        // recognized primitive (unrecognized aliases fall through to the
        // runtime panic in `Tag::with_log_on`).
        let inner_numeric: Option<bool> = if let Type::Path(tp) = inner {
            tp.path.segments.last().map(|seg| seg.ident.to_string()).and_then(|id| {
                match id.as_str() {
                    "f64" | "i64" => Some(true),
                    "bool" | "String" => Some(false),
                    _ => None,
                }
            })
        } else {
            None
        };
        let mut trigger_tokens = Vec::new();
        for spec in &attrs.log_on {
            if let (Some(field_kind), Some(trigger_kind)) = (inner_numeric, spec.numeric_kind()) {
                if field_kind != trigger_kind {
                    return Err(syn::Error::new(
                        spec.name.span(),
                        if trigger_kind {
                            format!(
                                "log_on: `{}` only applies to numeric tags \
                                 (Tag<f64>/Tag<i64>); this field is not numeric \
                                 (pydoover: boolean/string log_on accepts AnyChange, \
                                 Enter, Exit)",
                                spec.name
                            )
                        } else {
                            format!(
                                "log_on: `{}` only applies to boolean/string tags \
                                 (Tag<bool>/Tag<String>) \
                                 (pydoover: numeric log_on accepts Cross, Rise, Fall, Delta)",
                                spec.name
                            )
                        },
                    ));
                }
            }
            trigger_tokens.push(spec.to_tokens()?);
        }
        let log_on_tokens = (!trigger_tokens.is_empty()).then(|| {
            quote! { .with_log_on(::std::vec![#(#trigger_tokens),*]) }
        });

        field_idents.push(fident);
        declarations.push(quote! {
            ::doover::tags::Tag::<#inner>::declared(#tag_name, #live, #default_tokens)
                #log_on_tokens
        });
        if live {
            live_tag_names.push(tag_name.clone());
        }
        tag_names.push(tag_name);
    }

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::doover::tags::TagsCollection for #ident {
                fn attach(__rt: ::std::sync::Arc<::doover::tags::TagsRuntime>) -> Self {
                    Self {
                        #(#field_idents: #declarations
                            .attached(::std::sync::Arc::clone(&__rt)),)*
                    }
                }

                fn detached() -> Self {
                    Self {
                        #(#field_idents: #declarations,)*
                    }
                }

                fn tag_names() -> ::std::vec::Vec<&'static str> {
                    ::std::vec![#(#tag_names),*]
                }

                fn live_tag_names() -> ::std::vec::Vec<&'static str> {
                    ::std::vec![#(#live_tag_names),*]
                }
            }
        };
    })
}

// ---------------------------------------------------------------------------
// #[derive(Ui)]
// ---------------------------------------------------------------------------

/// Derive `doover::ui::UiTree` for a struct of UI element fields — the
/// counterpart of a pydoover `ui.UI` subclass body. Supplies reflection only
/// (children in field order); positions and the root `uiApplication` node
/// come from the trait's default `finalize`/`to_schema`, and construction is
/// explicit via `doover::ui::UiBuild`.
#[proc_macro_derive(Ui)]
pub fn derive_ui(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_ui(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_ui(input: DeriveInput) -> syn::Result<TokenStream2> {
    let fields = named_fields(&input, "Ui")?;
    let ident = &input.ident;
    let field_idents: Vec<_> = fields.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::doover::ui::UiTree for #ident {
                fn children(&self) -> ::std::vec::Vec<&dyn ::doover::ui::UiElement> {
                    ::std::vec![
                        #(&self.#field_idents as &dyn ::doover::ui::UiElement),*
                    ]
                }

                fn children_mut(
                    &mut self,
                ) -> ::std::vec::Vec<&mut dyn ::doover::ui::UiElement> {
                    ::std::vec![
                        #(&mut self.#field_idents as &mut dyn ::doover::ui::UiElement),*
                    ]
                }
            }
        };
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_pydoover() {
        assert_eq!(sanitize_display_name("Sensor Minimum mA"), "sensor_minimum_ma");
        assert_eq!(sanitize_display_name("AI Pin"), "ai_pin");
    }

    #[test]
    fn title_case_from_field_name() {
        assert_eq!(title_case("sensor_type"), "Sensor Type");
        assert_eq!(title_case("volume_decimal_precision"), "Volume Decimal Precision");
    }

    #[test]
    fn camel_split() {
        assert_eq!(camel_to_title("RadarInverted"), "Radar Inverted");
        assert_eq!(camel_to_title("Submersible"), "Submersible");
    }

    fn trigger_tokens(src: &str) -> String {
        let spec: TriggerSpec = syn::parse_str(src).expect("spec parses");
        spec.to_tokens().expect("tokens generate").to_string().replace(' ', "")
    }

    #[test]
    fn log_on_grammar_generates_constructors() {
        assert_eq!(
            trigger_tokens("cross(15.0)"),
            "::doover::tags::LogTrigger::cross(::std::vec![(15.0)asf64])"
        );
        assert_eq!(
            trigger_tokens("rise(50, 100, deadband = 4.0)"),
            "::doover::tags::LogTrigger::rise(::std::vec![(50)asf64,(100)asf64]).deadband((4.0)asf64)"
        );
        assert_eq!(
            trigger_tokens("delta(percent = 10)"),
            "::doover::tags::LogTrigger::delta_percent((10)asf64)"
        );
        assert_eq!(trigger_tokens("any_change"), "::doover::tags::LogTrigger::any_change()");
        assert!(trigger_tokens("enter(\"fault\")").contains("json!(\"fault\")"));
    }

    #[test]
    fn log_on_grammar_rejects_bad_shapes() {
        for (src, msg) in [
            ("cross()", "at least one threshold"),
            ("cross(5.0, deadline = 1.0)", "unknown cross() option"),
            ("delta(amount = 1.0, percent = 2.0)", "exactly one of"),
            ("delta(5.0)", "exactly one of"),
            ("any_change(true)", "takes no arguments"),
            ("enter()", "exactly one value"),
            ("exit(1, 2)", "exactly one value"),
            ("wiggle(1)", "unknown trigger"),
        ] {
            let spec: TriggerSpec = syn::parse_str(src).expect("spec parses");
            let err = spec.to_tokens().expect_err(src).to_string();
            assert!(err.contains(msg), "{src}: {err}");
        }
    }

    #[test]
    fn trigger_kinds() {
        for (src, kind) in [
            ("cross(1.0)", Some(true)),
            ("fall(1.0)", Some(true)),
            ("delta(amount = 1.0)", Some(true)),
            ("any_change", Some(false)),
            ("enter(true)", Some(false)),
            ("wiggle", None),
        ] {
            let spec: TriggerSpec = syn::parse_str(src).unwrap();
            assert_eq!(spec.numeric_kind(), kind, "{src}");
        }
    }
}
