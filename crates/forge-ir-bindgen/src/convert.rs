//! Conversions between bindgen-generated WIT types and [`forge_ir`].
//!
//! Both `ir-transformer` and `code-generator` worlds share the `types`,
//! `host-api`, and `stage` interfaces, but `wasmtime::component::bindgen!`
//! emits *separate* Rust modules per world (the types are structurally
//! identical, nominally distinct). To avoid duplicating ~600 lines of
//! conversion glue, we expand the conversions once per world via a
//! `macro_rules!` macro keyed on the bindings root module.
//!
//! Public API:
//!
//! * `convert::transformer::*` — operates on
//!   `crate::bindings::transformer::forge::plugin::types::*`.
//! * `convert::generator::*` — operates on
//!   `crate::bindings::generator::forge::plugin::types::*`.
//!
//! Both submodules expose:
//!
//! * `ir_to_wit(forge_ir::Ir) -> b::types::Ir`
//! * `ir_from_wit(b::types::Ir) -> Result<forge_ir::Ir, BindgenError>`
//! * `diagnostic_from_wit(b::types::Diagnostic) -> Result<forge_ir::Diagnostic, BindgenError>`
//! * `plugin_info_from_wit(b::types::PluginInfo) -> forge_ir::PluginInfo`
//! * `stage_error_from_wit(b::stage::StageError) -> StageErrorRepr`
//!
//! `StageErrorRepr` is a world-independent representation of the WIT
//! `stage-error` variant; the host translates it to its own `StageError`
//! type to avoid a circular dependency.

use forge_ir as ir;

/// Host-neutral representation of the WIT `stage-error` variant. Shared
/// between both worlds; the host wraps it into its own `StageError`.
#[derive(Debug, Clone, PartialEq)]
pub enum StageErrorRepr {
    Rejected {
        reason: String,
        diagnostics: Vec<ir::Diagnostic>,
    },
    PluginBug(String),
    ConfigInvalid(String),
    ResourceExceeded(ResourceKindRepr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKindRepr {
    Fuel,
    Memory,
    Time,
    OutputSize,
}

macro_rules! define_world_conversions {
    ($mod:ident, $world:ident) => {
        pub mod $mod {
            //! Conversions between `forge_ir` and the
            #![doc = concat!(" `", stringify!($world), "` world's WIT types.")]

            use super::{ResourceKindRepr, StageErrorRepr};
            use crate::bindings::$world::forge::plugin::{stage as b_stage, types as b};
            use crate::BindgenError;
            use forge_ir as ir;

            // -------------------------------------------------------------
            // PluginInfo
            // -------------------------------------------------------------

            pub fn plugin_info_from_wit(p: b::PluginInfo) -> ir::PluginInfo {
                ir::PluginInfo {
                    name: p.name,
                    version: p.version,
                }
            }

            pub fn plugin_info_to_wit(p: ir::PluginInfo) -> b::PluginInfo {
                b::PluginInfo {
                    name: p.name,
                    version: p.version,
                }
            }

            // -------------------------------------------------------------
            // SpecLocation
            // -------------------------------------------------------------

            fn loc_from(l: b::SpecLocation) -> ir::SpecLocation {
                ir::SpecLocation {
                    pointer: l.pointer,
                    file: l.file,
                }
            }

            fn loc_to(l: ir::SpecLocation) -> b::SpecLocation {
                b::SpecLocation {
                    pointer: l.pointer,
                    file: l.file,
                }
            }

            // -------------------------------------------------------------
            // Value
            // -------------------------------------------------------------

            fn value_from(v: b::Value) -> ir::Value {
                match v {
                    b::Value::Null => ir::Value::Null,
                    b::Value::Bool(value) => ir::Value::Bool { value },
                    b::Value::Int(value) => ir::Value::Int { value },
                    b::Value::Float(value) => ir::Value::Float { value },
                    b::Value::String(value) => ir::Value::String { value },
                    b::Value::List(items) => ir::Value::List { items },
                    b::Value::Object(fields) => ir::Value::Object {
                        fields: fields.into_iter().collect(),
                    },
                }
            }

            fn value_to(v: ir::Value) -> b::Value {
                match v {
                    ir::Value::Null => b::Value::Null,
                    ir::Value::Bool { value } => b::Value::Bool(value),
                    ir::Value::Int { value } => b::Value::Int(value),
                    ir::Value::Float { value } => b::Value::Float(value),
                    ir::Value::String { value } => b::Value::String(value),
                    ir::Value::List { items } => b::Value::List(items),
                    ir::Value::Object { fields } => b::Value::Object(fields),
                }
            }

            // Extensions are `Vec<(String, ValueRef)>` — identity copy
            // across the boundary since `ValueRef` is `u32` on both sides.
            // Kept as functions so call sites stay readable.
            fn extensions_from(xs: Vec<(String, ir::ValueRef)>) -> Vec<(String, ir::ValueRef)> {
                xs
            }

            fn extensions_to(xs: Vec<(String, ir::ValueRef)>) -> Vec<(String, ir::ValueRef)> {
                xs
            }

            // -------------------------------------------------------------
            // Diagnostics
            // -------------------------------------------------------------

            pub fn diagnostic_from_wit(d: b::Diagnostic) -> Result<ir::Diagnostic, BindgenError> {
                Ok(ir::Diagnostic {
                    severity: severity_from(d.severity),
                    code: d.code,
                    message: d.message,
                    location: d.location.map(loc_from),
                    related: d.related.into_iter().map(related_from).collect(),
                    suggested_fix: d.suggested_fix.map(fix_from),
                })
            }

            pub fn diagnostic_to_wit(d: ir::Diagnostic) -> b::Diagnostic {
                b::Diagnostic {
                    severity: severity_to(d.severity),
                    code: d.code,
                    message: d.message,
                    location: d.location.map(loc_to),
                    related: d.related.into_iter().map(related_to).collect(),
                    suggested_fix: d.suggested_fix.map(fix_to),
                }
            }

            fn severity_from(s: b::Severity) -> ir::Severity {
                match s {
                    b::Severity::Error => ir::Severity::Error,
                    b::Severity::Warning => ir::Severity::Warning,
                    b::Severity::Info => ir::Severity::Info,
                    b::Severity::Hint => ir::Severity::Hint,
                }
            }

            fn severity_to(s: ir::Severity) -> b::Severity {
                match s {
                    ir::Severity::Error => b::Severity::Error,
                    ir::Severity::Warning => b::Severity::Warning,
                    ir::Severity::Info => b::Severity::Info,
                    ir::Severity::Hint => b::Severity::Hint,
                }
            }

            fn related_from(r: b::RelatedInfo) -> ir::RelatedInfo {
                ir::RelatedInfo {
                    message: r.message,
                    location: r.location.map(loc_from),
                }
            }

            fn related_to(r: ir::RelatedInfo) -> b::RelatedInfo {
                b::RelatedInfo {
                    message: r.message,
                    location: r.location.map(loc_to),
                }
            }

            fn fix_from(f: b::FixSuggestion) -> ir::FixSuggestion {
                ir::FixSuggestion {
                    message: f.message,
                    edits: f
                        .edits
                        .into_iter()
                        .map(|e| ir::FixEdit {
                            location: loc_from(e.location),
                            replacement: e.replacement,
                        })
                        .collect(),
                }
            }

            fn fix_to(f: ir::FixSuggestion) -> b::FixSuggestion {
                b::FixSuggestion {
                    message: f.message,
                    edits: f
                        .edits
                        .into_iter()
                        .map(|e| b::FixEdit {
                            location: loc_to(e.location),
                            replacement: e.replacement,
                        })
                        .collect(),
                }
            }

            // -------------------------------------------------------------
            // StageError
            // -------------------------------------------------------------

            pub fn stage_error_from_wit(e: b_stage::StageError) -> StageErrorRepr {
                match e {
                    b_stage::StageError::Rejected(r) => StageErrorRepr::Rejected {
                        reason: r.reason,
                        // Diagnostics from a rejection are best-effort; if
                        // any fail to convert, surface them as a plugin-bug
                        // by collapsing to an empty list with the reason
                        // preserved.
                        diagnostics: r
                            .diagnostics
                            .into_iter()
                            .filter_map(|d| diagnostic_from_wit(d).ok())
                            .collect(),
                    },
                    b_stage::StageError::PluginBug(s) => StageErrorRepr::PluginBug(s),
                    b_stage::StageError::ConfigInvalid(s) => StageErrorRepr::ConfigInvalid(s),
                    b_stage::StageError::ResourceExceeded(k) => {
                        StageErrorRepr::ResourceExceeded(match k {
                            b_stage::ResourceKind::Fuel => ResourceKindRepr::Fuel,
                            b_stage::ResourceKind::Memory => ResourceKindRepr::Memory,
                            b_stage::ResourceKind::Time => ResourceKindRepr::Time,
                            b_stage::ResourceKind::OutputSize => ResourceKindRepr::OutputSize,
                        })
                    }
                }
            }

            // -------------------------------------------------------------
            // Top-level Ir
            // -------------------------------------------------------------

            pub fn ir_to_wit(i: ir::Ir) -> b::Ir {
                b::Ir {
                    info: api_info_to(i.info),
                    operations: i.operations.into_iter().map(operation_to).collect(),
                    types: i.types.into_iter().map(named_type_to).collect(),
                    security_schemes: i
                        .security_schemes
                        .into_iter()
                        .map(security_scheme_to)
                        .collect(),
                    servers: i.servers.into_iter().map(server_to).collect(),
                    webhooks: i.webhooks.into_iter().map(webhook_to).collect(),
                    external_docs: i.external_docs.map(external_docs_to),
                    tags: i.tags.into_iter().map(tag_to).collect(),
                    json_schema_dialect: i.json_schema_dialect,
                    self_url: i.self_url,
                    values: i.values.into_iter().map(value_to).collect(),
                }
            }

            pub fn ir_from_wit(i: b::Ir) -> Result<ir::Ir, BindgenError> {
                let out = ir::Ir {
                    info: api_info_from(i.info),
                    operations: i
                        .operations
                        .into_iter()
                        .map(operation_from)
                        .collect::<Result<_, _>>()?,
                    types: i
                        .types
                        .into_iter()
                        .map(named_type_from)
                        .collect::<Result<_, _>>()?,
                    security_schemes: i
                        .security_schemes
                        .into_iter()
                        .map(security_scheme_from)
                        .collect(),
                    servers: i.servers.into_iter().map(server_from).collect(),
                    webhooks: i
                        .webhooks
                        .into_iter()
                        .map(webhook_from)
                        .collect::<Result<_, _>>()?,
                    external_docs: i.external_docs.map(external_docs_from),
                    tags: i.tags.into_iter().map(tag_from).collect(),
                    json_schema_dialect: i.json_schema_dialect,
                    self_url: i.self_url,
                    values: i.values.into_iter().map(value_from).collect(),
                };
                Ok(out)
            }

            // -------------------------------------------------------------
            // ApiInfo / Server
            // -------------------------------------------------------------

            fn api_info_from(a: b::ApiInfo) -> ir::ApiInfo {
                ir::ApiInfo {
                    title: a.title,
                    version: a.version,
                    summary: a.summary,
                    description: a.description,
                    terms_of_service: a.terms_of_service,
                    contact: a.contact.map(contact_from),
                    license_name: a.license_name,
                    license_url: a.license_url,
                    license_identifier: a.license_identifier,
                    extensions: extensions_from(a.extensions),
                }
            }

            fn api_info_to(a: ir::ApiInfo) -> b::ApiInfo {
                b::ApiInfo {
                    title: a.title,
                    version: a.version,
                    summary: a.summary,
                    description: a.description,
                    terms_of_service: a.terms_of_service,
                    contact: a.contact.map(contact_to),
                    license_name: a.license_name,
                    license_url: a.license_url,
                    license_identifier: a.license_identifier,
                    extensions: extensions_to(a.extensions),
                }
            }

            fn contact_from(c: b::Contact) -> ir::Contact {
                ir::Contact {
                    name: c.name,
                    url: c.url,
                    email: c.email,
                }
            }

            fn contact_to(c: ir::Contact) -> b::Contact {
                b::Contact {
                    name: c.name,
                    url: c.url,
                    email: c.email,
                }
            }

            fn server_from(s: b::Server) -> ir::Server {
                ir::Server {
                    url: s.url,
                    description: s.description,
                    name: s.name,
                    variables: s
                        .variables
                        .into_iter()
                        .map(|(k, v)| (k, server_var_from(v)))
                        .collect(),
                    extensions: extensions_from(s.extensions),
                }
            }

            fn server_to(s: ir::Server) -> b::Server {
                b::Server {
                    url: s.url,
                    description: s.description,
                    name: s.name,
                    variables: s
                        .variables
                        .into_iter()
                        .map(|(k, v)| (k, server_var_to(v)))
                        .collect(),
                    extensions: extensions_to(s.extensions),
                }
            }

            fn server_var_from(v: b::ServerVariable) -> ir::ServerVariable {
                ir::ServerVariable {
                    default: v.default,
                    r#enum: v.enum_,
                    description: v.description,
                    extensions: extensions_from(v.extensions),
                }
            }

            fn server_var_to(v: ir::ServerVariable) -> b::ServerVariable {
                b::ServerVariable {
                    default: v.default,
                    enum_: v.r#enum,
                    description: v.description,
                    extensions: extensions_to(v.extensions),
                }
            }

            // -------------------------------------------------------------
            // Types
            // -------------------------------------------------------------

            fn named_type_from(n: b::NamedType) -> Result<ir::NamedType, BindgenError> {
                Ok(ir::NamedType {
                    id: n.id,
                    original_name: n.original_name,
                    title: n.title,
                    description: n.description,
                    deprecated: n.deprecated,
                    read_only: n.read_only,
                    write_only: n.write_only,
                    external_docs: n.external_docs.map(external_docs_from),
                    default: n.default,
                    examples: examples_from(n.examples),
                    xml: n.xml.map(xml_object_from),
                    definition: type_def_from(n.definition)?,
                    extensions: extensions_from(n.extensions),
                    location: n.location.map(loc_from),
                })
            }

            fn named_type_to(n: ir::NamedType) -> b::NamedType {
                b::NamedType {
                    id: n.id,
                    original_name: n.original_name,
                    title: n.title,
                    description: n.description,
                    deprecated: n.deprecated,
                    read_only: n.read_only,
                    write_only: n.write_only,
                    external_docs: n.external_docs.map(external_docs_to),
                    default: n.default,
                    examples: examples_to(n.examples),
                    xml: n.xml.map(xml_object_to),
                    definition: type_def_to(n.definition),
                    extensions: extensions_to(n.extensions),
                    location: n.location.map(loc_to),
                }
            }

            fn external_docs_from(d: b::ExternalDocs) -> ir::ExternalDocs {
                ir::ExternalDocs {
                    description: d.description,
                    url: d.url,
                }
            }

            fn external_docs_to(d: ir::ExternalDocs) -> b::ExternalDocs {
                b::ExternalDocs {
                    description: d.description,
                    url: d.url,
                }
            }

            fn example_from(e: b::Example) -> ir::Example {
                ir::Example {
                    summary: e.summary,
                    description: e.description,
                    value: e.value,
                    external_value: e.external_value,
                    data_value: e.data_value,
                    serialized_value: e.serialized_value,
                }
            }

            fn example_to(e: ir::Example) -> b::Example {
                b::Example {
                    summary: e.summary,
                    description: e.description,
                    value: e.value,
                    external_value: e.external_value,
                    data_value: e.data_value,
                    serialized_value: e.serialized_value,
                }
            }

            fn examples_from(xs: Vec<(String, b::Example)>) -> Vec<(String, ir::Example)> {
                xs.into_iter().map(|(k, v)| (k, example_from(v))).collect()
            }

            fn examples_to(xs: Vec<(String, ir::Example)>) -> Vec<(String, b::Example)> {
                xs.into_iter().map(|(k, v)| (k, example_to(v))).collect()
            }

            fn xml_object_from(x: b::XmlObject) -> ir::XmlObject {
                ir::XmlObject {
                    name: x.name,
                    namespace: x.namespace,
                    prefix: x.prefix,
                    attribute: x.attribute,
                    wrapped: x.wrapped,
                    text: x.text,
                    ordered: x.ordered,
                    extensions: x.extensions,
                }
            }

            fn xml_object_to(x: ir::XmlObject) -> b::XmlObject {
                b::XmlObject {
                    name: x.name,
                    namespace: x.namespace,
                    prefix: x.prefix,
                    attribute: x.attribute,
                    wrapped: x.wrapped,
                    text: x.text,
                    ordered: x.ordered,
                    extensions: x.extensions,
                }
            }

            fn link_from(l: b::Link) -> ir::Link {
                ir::Link {
                    operation_ref: l.operation_ref,
                    operation_id: l.operation_id,
                    parameters: l.parameters,
                    request_body: l.request_body,
                    description: l.description,
                    server: l.server.map(server_from),
                    extensions: l.extensions,
                }
            }

            fn link_to(l: ir::Link) -> b::Link {
                b::Link {
                    operation_ref: l.operation_ref,
                    operation_id: l.operation_id,
                    parameters: l.parameters,
                    request_body: l.request_body,
                    description: l.description,
                    server: l.server.map(server_to),
                    extensions: l.extensions,
                }
            }

            fn links_from(xs: Vec<(String, b::Link)>) -> Vec<(String, ir::Link)> {
                xs.into_iter().map(|(k, v)| (k, link_from(v))).collect()
            }

            fn links_to(xs: Vec<(String, ir::Link)>) -> Vec<(String, b::Link)> {
                xs.into_iter().map(|(k, v)| (k, link_to(v))).collect()
            }

            fn webhook_from(w: b::Webhook) -> Result<ir::Webhook, BindgenError> {
                Ok(ir::Webhook {
                    name: w.name,
                    summary: w.summary,
                    description: w.description,
                    operations: w
                        .operations
                        .into_iter()
                        .map(operation_from)
                        .collect::<Result<_, _>>()?,
                })
            }

            fn webhook_to(w: ir::Webhook) -> b::Webhook {
                b::Webhook {
                    name: w.name,
                    summary: w.summary,
                    description: w.description,
                    operations: w.operations.into_iter().map(operation_to).collect(),
                }
            }

            fn callback_from(c: b::Callback) -> ir::Callback {
                ir::Callback {
                    name: c.name,
                    expression: c.expression,
                    operation_ids: c.operation_ids,
                    extensions: c.extensions,
                }
            }

            fn callback_to(c: ir::Callback) -> b::Callback {
                b::Callback {
                    name: c.name,
                    expression: c.expression,
                    operation_ids: c.operation_ids,
                    extensions: c.extensions,
                }
            }

            fn tag_from(t: b::Tag) -> ir::Tag {
                ir::Tag {
                    name: t.name,
                    summary: t.summary,
                    description: t.description,
                    external_docs: t.external_docs.map(external_docs_from),
                    parent: t.parent,
                    kind: t.kind,
                    extensions: t.extensions,
                }
            }

            fn tag_to(t: ir::Tag) -> b::Tag {
                b::Tag {
                    name: t.name,
                    summary: t.summary,
                    description: t.description,
                    external_docs: t.external_docs.map(external_docs_to),
                    parent: t.parent,
                    kind: t.kind,
                    extensions: t.extensions,
                }
            }

            fn type_def_from(d: b::TypeDef) -> Result<ir::TypeDef, BindgenError> {
                Ok(match d {
                    b::TypeDef::Primitive(p) => ir::TypeDef::Primitive(prim_from(p)),
                    b::TypeDef::Object(o) => ir::TypeDef::Object(object_from(o)),
                    b::TypeDef::Array(a) => ir::TypeDef::Array(array_from(a)),
                    b::TypeDef::EnumString(e) => ir::TypeDef::EnumString(enum_str_from(e)),
                    b::TypeDef::EnumInt(e) => ir::TypeDef::EnumInt(enum_int_from(e)),
                    b::TypeDef::EnumBool(e) => ir::TypeDef::EnumBool(enum_bool_from(e)),
                    b::TypeDef::EnumNumber(e) => ir::TypeDef::EnumNumber(enum_number_from(e)),
                    b::TypeDef::Union(u) => ir::TypeDef::Union(union_from(u)),
                    b::TypeDef::Null => ir::TypeDef::Null,
                    b::TypeDef::Any => ir::TypeDef::Any,
                })
            }

            fn type_def_to(d: ir::TypeDef) -> b::TypeDef {
                match d {
                    ir::TypeDef::Primitive(p) => b::TypeDef::Primitive(prim_to(p)),
                    ir::TypeDef::Object(o) => b::TypeDef::Object(object_to(o)),
                    ir::TypeDef::Array(a) => b::TypeDef::Array(array_to(a)),
                    ir::TypeDef::EnumString(e) => b::TypeDef::EnumString(enum_str_to(e)),
                    ir::TypeDef::EnumInt(e) => b::TypeDef::EnumInt(enum_int_to(e)),
                    ir::TypeDef::EnumBool(e) => b::TypeDef::EnumBool(enum_bool_to(e)),
                    ir::TypeDef::EnumNumber(e) => b::TypeDef::EnumNumber(enum_number_to(e)),
                    ir::TypeDef::Union(u) => b::TypeDef::Union(union_to(u)),
                    ir::TypeDef::Null => b::TypeDef::Null,
                    ir::TypeDef::Any => b::TypeDef::Any,
                }
            }

            // ---- primitive ----

            fn prim_kind_from(k: b::PrimitiveKind) -> ir::PrimitiveKind {
                use b::PrimitiveKind as W;
                use ir::PrimitiveKind as I;
                match k {
                    W::PrimString => I::String,
                    W::PrimInteger => I::Integer,
                    W::PrimNumber => I::Number,
                    W::PrimBool => I::Bool,
                }
            }

            fn prim_kind_to(k: ir::PrimitiveKind) -> b::PrimitiveKind {
                use b::PrimitiveKind as W;
                use ir::PrimitiveKind as I;
                match k {
                    I::String => W::PrimString,
                    I::Integer => W::PrimInteger,
                    I::Number => W::PrimNumber,
                    I::Bool => W::PrimBool,
                }
            }

            fn prim_constraints_from(c: b::PrimitiveConstraints) -> ir::PrimitiveConstraints {
                ir::PrimitiveConstraints {
                    minimum: c.minimum,
                    maximum: c.maximum,
                    exclusive_minimum: c.exclusive_minimum,
                    exclusive_maximum: c.exclusive_maximum,
                    multiple_of: c.multiple_of,
                    min_length: c.min_length,
                    max_length: c.max_length,
                    pattern: c.pattern,
                    format_extension: c.format_extension,
                    content_encoding: c.content_encoding,
                    content_media_type: c.content_media_type,
                    content_schema: c.content_schema,
                }
            }

            fn prim_constraints_to(c: ir::PrimitiveConstraints) -> b::PrimitiveConstraints {
                b::PrimitiveConstraints {
                    minimum: c.minimum,
                    maximum: c.maximum,
                    exclusive_minimum: c.exclusive_minimum,
                    exclusive_maximum: c.exclusive_maximum,
                    multiple_of: c.multiple_of,
                    min_length: c.min_length,
                    max_length: c.max_length,
                    pattern: c.pattern,
                    format_extension: c.format_extension,
                    content_encoding: c.content_encoding,
                    content_media_type: c.content_media_type,
                    content_schema: c.content_schema,
                }
            }

            fn prim_from(p: b::PrimitiveType) -> ir::PrimitiveType {
                ir::PrimitiveType {
                    kind: prim_kind_from(p.kind),
                    constraints: prim_constraints_from(p.constraints),
                }
            }

            fn prim_to(p: ir::PrimitiveType) -> b::PrimitiveType {
                b::PrimitiveType {
                    kind: prim_kind_to(p.kind),
                    constraints: prim_constraints_to(p.constraints),
                }
            }

            // ---- array ----

            fn array_from(a: b::ArrayType) -> ir::ArrayType {
                ir::ArrayType {
                    items: a.items,
                    constraints: ir::ArrayConstraints {
                        min_items: a.constraints.min_items,
                        max_items: a.constraints.max_items,
                        unique_items: a.constraints.unique_items,
                    },
                }
            }

            fn array_to(a: ir::ArrayType) -> b::ArrayType {
                b::ArrayType {
                    items: a.items,
                    constraints: b::ArrayConstraints {
                        min_items: a.constraints.min_items,
                        max_items: a.constraints.max_items,
                        unique_items: a.constraints.unique_items,
                    },
                }
            }

            // ---- object ----

            fn object_from(o: b::ObjectType) -> ir::ObjectType {
                ir::ObjectType {
                    properties: o
                        .properties
                        .into_iter()
                        .map(|p| ir::Property {
                            name: p.name,
                            r#type: p.type_,
                            required: p.required,
                            title: p.title,
                            description: p.description,
                            deprecated: p.deprecated,
                            read_only: p.read_only,
                            write_only: p.write_only,
                            external_docs: p.external_docs.map(external_docs_from),
                            default: p.default,
                            examples: examples_from(p.examples),
                            extensions: p.extensions,
                        })
                        .collect(),
                    pattern_properties: o
                        .pattern_properties
                        .into_iter()
                        .map(|p| ir::PatternProperty {
                            pattern: p.pattern,
                            r#type: p.type_,
                        })
                        .collect(),
                    additional_properties: match o.additional_properties {
                        b::AdditionalProperties::Forbidden => ir::AdditionalProperties::Forbidden,
                        b::AdditionalProperties::Any => ir::AdditionalProperties::Any,
                        b::AdditionalProperties::Typed(t) => {
                            ir::AdditionalProperties::Typed { r#type: t }
                        }
                    },
                    property_names: o.property_names,
                    constraints: ir::ObjectConstraints {
                        min_properties: o.constraints.min_properties,
                        max_properties: o.constraints.max_properties,
                    },
                }
            }

            fn object_to(o: ir::ObjectType) -> b::ObjectType {
                b::ObjectType {
                    properties: o
                        .properties
                        .into_iter()
                        .map(|p| b::Property {
                            name: p.name,
                            type_: p.r#type,
                            required: p.required,
                            title: p.title,
                            description: p.description,
                            deprecated: p.deprecated,
                            read_only: p.read_only,
                            write_only: p.write_only,
                            external_docs: p.external_docs.map(external_docs_to),
                            default: p.default,
                            examples: examples_to(p.examples),
                            extensions: p.extensions,
                        })
                        .collect(),
                    pattern_properties: o
                        .pattern_properties
                        .into_iter()
                        .map(|p| b::PatternProperty {
                            pattern: p.pattern,
                            type_: p.r#type,
                        })
                        .collect(),
                    additional_properties: match o.additional_properties {
                        ir::AdditionalProperties::Forbidden => b::AdditionalProperties::Forbidden,
                        ir::AdditionalProperties::Any => b::AdditionalProperties::Any,
                        ir::AdditionalProperties::Typed { r#type } => {
                            b::AdditionalProperties::Typed(r#type)
                        }
                    },
                    property_names: o.property_names,
                    constraints: b::ObjectConstraints {
                        min_properties: o.constraints.min_properties,
                        max_properties: o.constraints.max_properties,
                    },
                }
            }

            // ---- map ----

            // ---- enums ----

            fn enum_str_from(e: b::EnumStringType) -> ir::EnumStringType {
                ir::EnumStringType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| ir::EnumStringValue { value: v.value })
                        .collect(),
                }
            }

            fn enum_str_to(e: ir::EnumStringType) -> b::EnumStringType {
                b::EnumStringType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| b::EnumStringValue { value: v.value })
                        .collect(),
                }
            }

            fn enum_int_from(e: b::EnumIntType) -> ir::EnumIntType {
                ir::EnumIntType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| ir::EnumIntValue { value: v.value })
                        .collect(),
                    kind: match e.kind {
                        b::IntKind::Int32 => ir::IntKind::Int32,
                        b::IntKind::Int64 => ir::IntKind::Int64,
                    },
                }
            }

            fn enum_int_to(e: ir::EnumIntType) -> b::EnumIntType {
                b::EnumIntType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| b::EnumIntValue { value: v.value })
                        .collect(),
                    kind: match e.kind {
                        ir::IntKind::Int32 => b::IntKind::Int32,
                        ir::IntKind::Int64 => b::IntKind::Int64,
                    },
                }
            }

            fn enum_bool_from(e: b::EnumBoolType) -> ir::EnumBoolType {
                ir::EnumBoolType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| ir::EnumBoolValue { value: v.value })
                        .collect(),
                }
            }

            fn enum_bool_to(e: ir::EnumBoolType) -> b::EnumBoolType {
                b::EnumBoolType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| b::EnumBoolValue { value: v.value })
                        .collect(),
                }
            }

            fn enum_number_from(e: b::EnumNumberType) -> ir::EnumNumberType {
                ir::EnumNumberType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| ir::EnumNumberValue { value: v.value })
                        .collect(),
                    kind: match e.kind {
                        b::NumberKind::Float => ir::NumberKind::Float,
                        b::NumberKind::Double => ir::NumberKind::Double,
                    },
                }
            }

            fn enum_number_to(e: ir::EnumNumberType) -> b::EnumNumberType {
                b::EnumNumberType {
                    values: e
                        .values
                        .into_iter()
                        .map(|v| b::EnumNumberValue { value: v.value })
                        .collect(),
                    kind: match e.kind {
                        ir::NumberKind::Float => b::NumberKind::Float,
                        ir::NumberKind::Double => b::NumberKind::Double,
                    },
                }
            }

            // ---- union ----

            fn union_from(u: b::UnionType) -> ir::UnionType {
                ir::UnionType {
                    variants: u
                        .variants
                        .into_iter()
                        .map(|v| ir::UnionVariant {
                            r#type: v.type_,
                            tag: v.tag,
                        })
                        .collect(),
                    discriminator: u.discriminator.map(|d| ir::Discriminator {
                        property_name: d.property_name,
                        mapping: d.mapping.into_iter().collect(),
                        extensions: d.extensions,
                    }),
                    kind: match u.kind {
                        b::UnionKind::OneOf => ir::UnionKind::OneOf,
                        b::UnionKind::AnyOf => ir::UnionKind::AnyOf,
                    },
                }
            }

            fn union_to(u: ir::UnionType) -> b::UnionType {
                b::UnionType {
                    variants: u
                        .variants
                        .into_iter()
                        .map(|v| b::UnionVariant {
                            type_: v.r#type,
                            tag: v.tag,
                        })
                        .collect(),
                    discriminator: u.discriminator.map(|d| b::Discriminator {
                        property_name: d.property_name,
                        mapping: d.mapping,
                        extensions: d.extensions,
                    }),
                    kind: match u.kind {
                        ir::UnionKind::OneOf => b::UnionKind::OneOf,
                        ir::UnionKind::AnyOf => b::UnionKind::AnyOf,
                    },
                }
            }

            // -------------------------------------------------------------
            // Operations
            // -------------------------------------------------------------

            fn http_method_from(m: b::HttpMethod) -> ir::HttpMethod {
                use b::HttpMethod as W;
                use ir::HttpMethod as I;
                match m {
                    W::Get => I::Get,
                    W::Put => I::Put,
                    W::Post => I::Post,
                    W::Delete => I::Delete,
                    W::Options => I::Options,
                    W::Head => I::Head,
                    W::Patch => I::Patch,
                    W::Trace => I::Trace,
                    W::Other(s) => I::Other(s),
                }
            }

            fn http_method_to(m: ir::HttpMethod) -> b::HttpMethod {
                use b::HttpMethod as W;
                use ir::HttpMethod as I;
                match m {
                    I::Get => W::Get,
                    I::Put => W::Put,
                    I::Post => W::Post,
                    I::Delete => W::Delete,
                    I::Options => W::Options,
                    I::Head => W::Head,
                    I::Patch => W::Patch,
                    I::Trace => W::Trace,
                    I::Other(s) => W::Other(s),
                }
            }

            fn param_style_from(s: b::ParameterStyle) -> ir::ParameterStyle {
                use b::ParameterStyle as W;
                use ir::ParameterStyle as I;
                match s {
                    W::ParamForm => I::Form,
                    W::ParamSimple => I::Simple,
                    W::ParamLabel => I::Label,
                    W::ParamMatrix => I::Matrix,
                    W::ParamSpaceDelimited => I::SpaceDelimited,
                    W::ParamPipeDelimited => I::PipeDelimited,
                    W::ParamDeepObject => I::DeepObject,
                }
            }

            fn param_style_to(s: ir::ParameterStyle) -> b::ParameterStyle {
                use b::ParameterStyle as W;
                use ir::ParameterStyle as I;
                match s {
                    I::Form => W::ParamForm,
                    I::Simple => W::ParamSimple,
                    I::Label => W::ParamLabel,
                    I::Matrix => W::ParamMatrix,
                    I::SpaceDelimited => W::ParamSpaceDelimited,
                    I::PipeDelimited => W::ParamPipeDelimited,
                    I::DeepObject => W::ParamDeepObject,
                }
            }

            fn header_from(h: b::Header) -> ir::Header {
                ir::Header {
                    r#type: h.type_,
                    required: h.required,
                    description: h.description,
                    deprecated: h.deprecated,
                    examples: examples_from(h.examples),
                    style: h.style.map(param_style_from),
                    explode: h.explode,
                    allow_reserved: h.allow_reserved,
                    allow_empty_value: h.allow_empty_value,
                    location: h.location.map(loc_from),
                }
            }

            fn header_to(h: ir::Header) -> b::Header {
                b::Header {
                    type_: h.r#type,
                    required: h.required,
                    description: h.description,
                    deprecated: h.deprecated,
                    examples: examples_to(h.examples),
                    style: h.style.map(param_style_to),
                    explode: h.explode,
                    allow_reserved: h.allow_reserved,
                    allow_empty_value: h.allow_empty_value,
                    location: h.location.map(loc_to),
                }
            }

            fn parameter_from(p: b::Parameter) -> ir::Parameter {
                ir::Parameter {
                    name: p.name,
                    r#type: p.type_,
                    required: p.required,
                    description: p.description,
                    deprecated: p.deprecated,
                    examples: examples_from(p.examples),
                    style: p.style.map(param_style_from),
                    explode: p.explode,
                    allow_empty_value: p.allow_empty_value,
                    allow_reserved: p.allow_reserved,
                    extensions: extensions_from(p.extensions),
                    location: p.location.map(loc_from),
                }
            }

            fn parameter_to(p: ir::Parameter) -> b::Parameter {
                b::Parameter {
                    name: p.name,
                    type_: p.r#type,
                    required: p.required,
                    description: p.description,
                    deprecated: p.deprecated,
                    examples: examples_to(p.examples),
                    style: p.style.map(param_style_to),
                    explode: p.explode,
                    allow_empty_value: p.allow_empty_value,
                    allow_reserved: p.allow_reserved,
                    extensions: extensions_to(p.extensions),
                    location: p.location.map(loc_to),
                }
            }

            fn body_from(b_: b::Body) -> ir::Body {
                ir::Body {
                    content: b_.content.into_iter().map(body_content_from).collect(),
                    required: b_.required,
                    description: b_.description,
                    extensions: extensions_from(b_.extensions),
                }
            }

            fn body_to(b_: ir::Body) -> b::Body {
                b::Body {
                    content: b_.content.into_iter().map(body_content_to).collect(),
                    required: b_.required,
                    description: b_.description,
                    extensions: extensions_to(b_.extensions),
                }
            }

            fn body_content_from(c: b::BodyContent) -> ir::BodyContent {
                ir::BodyContent {
                    media_type: c.media_type,
                    r#type: c.type_,
                    encoding: c
                        .encoding
                        .into_iter()
                        .map(|(k, v)| (k, encoding_from(v)))
                        .collect(),
                    item_schema: c.item_schema,
                    examples: examples_from(c.examples),
                    extensions: extensions_from(c.extensions),
                }
            }

            fn body_content_to(c: ir::BodyContent) -> b::BodyContent {
                b::BodyContent {
                    media_type: c.media_type,
                    type_: c.r#type,
                    encoding: c
                        .encoding
                        .into_iter()
                        .map(|(k, v)| (k, encoding_to(v)))
                        .collect(),
                    item_schema: c.item_schema,
                    examples: examples_to(c.examples),
                    extensions: extensions_to(c.extensions),
                }
            }

            fn encoding_from(e: b::Encoding) -> ir::Encoding {
                ir::Encoding {
                    content_type: e.content_type,
                    style: e.style.map(param_style_from),
                    explode: e.explode,
                    allow_reserved: e.allow_reserved,
                    headers: e
                        .headers
                        .into_iter()
                        .map(|(k, v)| (k, header_from(v)))
                        .collect(),
                    extensions: extensions_from(e.extensions),
                }
            }

            fn encoding_to(e: ir::Encoding) -> b::Encoding {
                b::Encoding {
                    content_type: e.content_type,
                    style: e.style.map(param_style_to),
                    explode: e.explode,
                    allow_reserved: e.allow_reserved,
                    headers: e
                        .headers
                        .into_iter()
                        .map(|(k, v)| (k, header_to(v)))
                        .collect(),
                    extensions: extensions_to(e.extensions),
                }
            }

            fn response_from(r: b::Response) -> ir::Response {
                ir::Response {
                    status: match r.status {
                        b::ResponseStatus::Explicit(code) => ir::ResponseStatus::Explicit { code },
                        b::ResponseStatus::Default => ir::ResponseStatus::Default,
                        b::ResponseStatus::Range(class) => ir::ResponseStatus::Range { class },
                    },
                    content: r.content.into_iter().map(body_content_from).collect(),
                    headers: r
                        .headers
                        .into_iter()
                        .map(|(k, v)| (k, header_from(v)))
                        .collect(),
                    summary: r.summary,
                    description: r.description,
                    links: links_from(r.links),
                    extensions: extensions_from(r.extensions),
                }
            }

            fn response_to(r: ir::Response) -> b::Response {
                b::Response {
                    status: match r.status {
                        ir::ResponseStatus::Explicit { code } => b::ResponseStatus::Explicit(code),
                        ir::ResponseStatus::Default => b::ResponseStatus::Default,
                        ir::ResponseStatus::Range { class } => b::ResponseStatus::Range(class),
                    },
                    content: r.content.into_iter().map(body_content_to).collect(),
                    headers: r
                        .headers
                        .into_iter()
                        .map(|(k, v)| (k, header_to(v)))
                        .collect(),
                    summary: r.summary,
                    description: r.description,
                    links: links_to(r.links),
                    extensions: extensions_to(r.extensions),
                }
            }

            fn operation_from(op: b::Operation) -> Result<ir::Operation, BindgenError> {
                Ok(ir::Operation {
                    id: op.id,
                    original_id: op.original_id,
                    method: http_method_from(op.method),
                    path_template: op.path_template,
                    path_params: op.path_params.into_iter().map(parameter_from).collect(),
                    query_params: op.query_params.into_iter().map(parameter_from).collect(),
                    header_params: op.header_params.into_iter().map(parameter_from).collect(),
                    cookie_params: op.cookie_params.into_iter().map(parameter_from).collect(),
                    querystring_params: op
                        .querystring_params
                        .into_iter()
                        .map(parameter_from)
                        .collect(),
                    request_body: op.request_body.map(body_from),
                    responses: op.responses.into_iter().map(response_from).collect(),
                    security: op
                        .security
                        .into_iter()
                        .map(|s| ir::SecurityRequirement {
                            scheme_id: s.scheme_id,
                            scopes: s.scopes,
                        })
                        .collect(),
                    tags: op.tags,
                    summary: op.summary,
                    description: op.description,
                    deprecated: op.deprecated,
                    external_docs: op.external_docs.map(external_docs_from),
                    extensions: op.extensions,
                    servers: op.servers.into_iter().map(server_from).collect(),
                    callbacks: op.callbacks.into_iter().map(callback_from).collect(),
                    location: op.location.map(loc_from),
                })
            }

            fn operation_to(op: ir::Operation) -> b::Operation {
                b::Operation {
                    id: op.id,
                    original_id: op.original_id,
                    method: http_method_to(op.method),
                    path_template: op.path_template,
                    path_params: op.path_params.into_iter().map(parameter_to).collect(),
                    query_params: op.query_params.into_iter().map(parameter_to).collect(),
                    header_params: op.header_params.into_iter().map(parameter_to).collect(),
                    cookie_params: op.cookie_params.into_iter().map(parameter_to).collect(),
                    querystring_params: op
                        .querystring_params
                        .into_iter()
                        .map(parameter_to)
                        .collect(),
                    request_body: op.request_body.map(body_to),
                    responses: op.responses.into_iter().map(response_to).collect(),
                    security: op
                        .security
                        .into_iter()
                        .map(|s| b::SecurityRequirement {
                            scheme_id: s.scheme_id,
                            scopes: s.scopes,
                        })
                        .collect(),
                    tags: op.tags,
                    summary: op.summary,
                    description: op.description,
                    deprecated: op.deprecated,
                    external_docs: op.external_docs.map(external_docs_to),
                    extensions: op.extensions,
                    servers: op.servers.into_iter().map(server_to).collect(),
                    callbacks: op.callbacks.into_iter().map(callback_to).collect(),
                    location: op.location.map(loc_to),
                }
            }

            // -------------------------------------------------------------
            // Security
            // -------------------------------------------------------------

            fn security_scheme_from(s: b::SecurityScheme) -> ir::SecurityScheme {
                ir::SecurityScheme {
                    id: s.id,
                    kind: match s.kind {
                        b::SecuritySchemeKind::ApiKey(k) => {
                            ir::SecuritySchemeKind::ApiKey(ir::ApiKeyScheme {
                                name: k.name,
                                location: match k.location {
                                    b::ApiKeyLocation::Header => ir::ApiKeyLocation::Header,
                                    b::ApiKeyLocation::Query => ir::ApiKeyLocation::Query,
                                    b::ApiKeyLocation::Cookie => ir::ApiKeyLocation::Cookie,
                                },
                            })
                        }
                        b::SecuritySchemeKind::HttpBasic => ir::SecuritySchemeKind::HttpBasic,
                        b::SecuritySchemeKind::HttpBearer(f) => {
                            ir::SecuritySchemeKind::HttpBearer { bearer_format: f }
                        }
                        b::SecuritySchemeKind::MutualTls => ir::SecuritySchemeKind::MutualTls,
                        b::SecuritySchemeKind::Oauth2(o) => {
                            ir::SecuritySchemeKind::Oauth2(ir::OAuth2Scheme {
                                flows: o.flows.into_iter().map(oauth2_flow_from).collect(),
                            })
                        }
                        b::SecuritySchemeKind::OpenIdConnect(u) => {
                            ir::SecuritySchemeKind::OpenIdConnect { url: u }
                        }
                    },
                    description: s.description,
                    deprecated: s.deprecated,
                    extensions: extensions_from(s.extensions),
                }
            }

            fn security_scheme_to(s: ir::SecurityScheme) -> b::SecurityScheme {
                b::SecurityScheme {
                    id: s.id,
                    kind: match s.kind {
                        ir::SecuritySchemeKind::ApiKey(k) => {
                            b::SecuritySchemeKind::ApiKey(b::ApiKeyScheme {
                                name: k.name,
                                location: match k.location {
                                    ir::ApiKeyLocation::Header => b::ApiKeyLocation::Header,
                                    ir::ApiKeyLocation::Query => b::ApiKeyLocation::Query,
                                    ir::ApiKeyLocation::Cookie => b::ApiKeyLocation::Cookie,
                                },
                            })
                        }
                        ir::SecuritySchemeKind::HttpBasic => b::SecuritySchemeKind::HttpBasic,
                        ir::SecuritySchemeKind::HttpBearer { bearer_format } => {
                            b::SecuritySchemeKind::HttpBearer(bearer_format)
                        }
                        ir::SecuritySchemeKind::MutualTls => b::SecuritySchemeKind::MutualTls,
                        ir::SecuritySchemeKind::Oauth2(o) => {
                            b::SecuritySchemeKind::Oauth2(b::Oauth2Scheme {
                                flows: o.flows.into_iter().map(oauth2_flow_to).collect(),
                            })
                        }
                        ir::SecuritySchemeKind::OpenIdConnect { url } => {
                            b::SecuritySchemeKind::OpenIdConnect(url)
                        }
                    },
                    description: s.description,
                    deprecated: s.deprecated,
                    extensions: extensions_to(s.extensions),
                }
            }

            fn oauth2_flow_from(f: b::Oauth2Flow) -> ir::OAuth2Flow {
                ir::OAuth2Flow {
                    kind: match f.kind {
                        b::Oauth2FlowKind::Implicit => ir::OAuth2FlowKind::Implicit,
                        b::Oauth2FlowKind::Password => ir::OAuth2FlowKind::Password,
                        b::Oauth2FlowKind::ClientCredentials => {
                            ir::OAuth2FlowKind::ClientCredentials
                        }
                        b::Oauth2FlowKind::AuthorizationCode => {
                            ir::OAuth2FlowKind::AuthorizationCode
                        }
                    },
                    authorization_url: f.authorization_url,
                    token_url: f.token_url,
                    refresh_url: f.refresh_url,
                    scopes: f.scopes,
                    extensions: extensions_from(f.extensions),
                }
            }

            fn oauth2_flow_to(f: ir::OAuth2Flow) -> b::Oauth2Flow {
                b::Oauth2Flow {
                    kind: match f.kind {
                        ir::OAuth2FlowKind::Implicit => b::Oauth2FlowKind::Implicit,
                        ir::OAuth2FlowKind::Password => b::Oauth2FlowKind::Password,
                        ir::OAuth2FlowKind::ClientCredentials => {
                            b::Oauth2FlowKind::ClientCredentials
                        }
                        ir::OAuth2FlowKind::AuthorizationCode => {
                            b::Oauth2FlowKind::AuthorizationCode
                        }
                    },
                    authorization_url: f.authorization_url,
                    token_url: f.token_url,
                    refresh_url: f.refresh_url,
                    scopes: f.scopes,
                    extensions: extensions_to(f.extensions),
                }
            }
        }
    };
}

define_world_conversions!(transformer, transformer);
define_world_conversions!(generator, generator);

#[cfg(test)]
mod tests {
    use super::*;
    use forge_ir::proptest_util::small_ir;
    use proptest::prelude::*;

    proptest! {
        /// Roundtrip: forge_ir → wit (transformer) → forge_ir is identity.
        #[test]
        fn roundtrip_transformer(ir in small_ir()) {
            let wit = transformer::ir_to_wit(ir.clone());
            let back = transformer::ir_from_wit(wit).unwrap();
            prop_assert_eq!(ir, back);
        }

        /// Roundtrip: forge_ir → wit (generator) → forge_ir is identity.
        #[test]
        fn roundtrip_generator(ir in small_ir()) {
            let wit = generator::ir_to_wit(ir.clone());
            let back = generator::ir_from_wit(wit).unwrap();
            prop_assert_eq!(ir, back);
        }
    }
}
