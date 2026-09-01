//! OpenAPI Forge intermediate representation.
//!
//! These types are the canonical Rust shape of the IR. They mirror the WIT
//! definitions in `wit/ir.wit` exactly. The `forge-ir-bindgen` crate handles
//! conversion to and from the WIT-generated representation that crosses the
//! component boundary.
//!
//! Pre-1.0 the IR is unstable: every change is a breaking change, and there is
//! no `api-version` field. Plugins built against a different `forge-ir`
//! version will fail at component load time with a WIT type error.
//!
//! See `docs/ir-spec.md` for the full contract.

#![forbid(unsafe_code)]

pub mod diagnostic;
pub mod operation;
#[cfg(any(test, feature = "proptest"))]
pub mod proptest_util;
pub mod security;
pub mod types;
pub mod value;

use serde::{Deserialize, Serialize};

pub use diagnostic::{Diagnostic, FixEdit, FixSuggestion, RelatedInfo, Severity, SpecLocation};
pub use operation::{
    Body, BodyContent, Encoding, Header, HttpMethod, Operation, Parameter, ParameterStyle,
    Response, ResponseStatus,
};
pub use security::{
    ApiKeyLocation, ApiKeyScheme, OAuth2Flow, OAuth2FlowKind, OAuth2Scheme, SecurityRequirement,
    SecurityScheme, SecuritySchemeKind,
};
pub use types::{
    AdditionalProperties, ArrayConstraints, ArrayType, EnumBoolType, EnumBoolValue, EnumIntType,
    EnumIntValue, EnumNumberType, EnumNumberValue, EnumStringType, EnumStringValue, IntKind,
    NamedType, NumberKind, ObjectConstraints, ObjectType, PatternProperty, PrimitiveConstraints,
    PrimitiveKind, PrimitiveType, Property, TypeDef, TypeRef, NULL_ID,
};
pub use types::{Discriminator, UnionKind, UnionType, UnionVariant};
pub use value::{Value, ValueRef};

// Documentation fields are inlined per node, matching the OAS 3.2 spec
// exactly: each node type carries only the doc surfaces the spec
// defines for it. Strict spec conformance — no uniform `Docs` slot.
// Nodes that the spec doesn't grant a `description` / `summary` / etc.
// simply don't have those fields. Reference Object `$ref` siblings
// override the target's same-keyed fields where applicable, and have
// "no effect" elsewhere because the target's parser doesn't read what
// the spec doesn't grant.

pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// Top-level IR document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ir {
    pub info: ApiInfo,
    /// Sorted by `id` for determinism.
    pub operations: Vec<Operation>,
    /// Topologically sorted; every `TypeRef` resolves to one of these by `id`.
    pub types: Vec<NamedType>,
    pub security_schemes: Vec<SecurityScheme>,
    pub servers: Vec<Server>,
    /// OpenAPI 3.1+ inbound webhooks. Each entry pairs the spec's
    /// `webhooks.<name>` map key (the routing identifier) with the
    /// path item's operations. Sorted by name for determinism.
    /// Generators that only emit outbound clients can ignore this
    /// field; webhook-handler generators dispatch on `Webhook.name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,
    /// Root-level `externalDocs`. Per-operation and per-schema slots
    /// live on `Operation.external_docs` / `NamedType.external_docs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<ExternalDocs>,
    /// Top-level `tags` array, walked into structured records. Sorted
    /// by `name` for determinism. `Operation.tags` stays a flat list
    /// of names that reference into this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,
    /// OpenAPI 3.1+ `jsonSchemaDialect` — declares which JSON Schema
    /// draft the document's schemas conform to. Carried verbatim
    /// (URL string); the parser does not validate or switch dialects
    /// based on it. Generators that care can read it; most ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema_dialect: Option<String>,
    /// OpenAPI 3.2 `$self` — the document's canonical URI for
    /// base-URI resolution per RFC 3986. The parser captures it
    /// verbatim; full base-URI semantics for external-`$ref`
    /// resolution land in #93.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_url: Option<String>,
    /// Pool of every structured `Value` referenced from elsewhere in
    /// the IR (defaults, examples, link parameters, extensions,
    /// constraint bounds). Compound `Value::List` / `Value::Object`
    /// arms hold `ValueRef` indices into this list — see ADR-0007's
    /// amendment and `crates/forge-ir/src/value.rs` for the design.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

/// Top-level `tags[]` entry. Generators surface `description` and
/// `summary` as group-level docs and use `parent` (3.2) to render
/// nested operation menus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    /// OAS 3.2 `summary` — short single-line label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// OAS `description` — CommonMark prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<ExternalDocs>,
    /// OAS 3.2 `parent` — name of another tag this one nests under.
    /// The parser warns and drops the parent reference (rather than the
    /// entire tag) if it doesn't match a declared tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// OAS 3.2 `kind` — free-form classifier (e.g. `"audience"`,
    /// `"channel"`). Generators that don't model it can ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

/// OAS ExternalDocumentation Object. `url` is required; `description`
/// is CommonMark-flavoured prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalDocs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
}

/// OpenAPI 3.1+ inbound webhook entry. The spec keys webhooks under a
/// map name (`newPet`, `deletedPet`); that name is the routing
/// identifier a webhook-handler generator dispatches on. A single
/// path item can hold multiple HTTP-method operations, all sharing
/// the same name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Webhook {
    pub name: String,
    /// PathItem-level `summary` (OAS §4.9). Applies to all operations
    /// the path item declares unless an operation overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// PathItem-level `description` (OAS §4.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Operations declared on the path item. Walked through the same
    /// `parse_path_item` machinery used for top-level `paths`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<Operation>,
}

/// OAS Callback Object — describes out-of-band requests the API makes
/// back to the caller. Used heavily by event-driven and webhook APIs.
///
/// The OAS shape is `callbacks: { <name>: { <expression>: PathItem } }`
/// (a name maps to a map of runtime expressions, each pointing to a
/// path item). The IR flattens this: each `Callback` carries one
/// (name, expression) pair plus the ids of the operations the path
/// item declared. A callback name with multiple expressions becomes
/// multiple `Callback` entries with the same name.
///
/// `operation_ids` reference into [`Ir::operations`] — callback path-
/// item operations live in the same flat list as top-level paths so
/// the WIT shape stays non-recursive. OAS operationId uniqueness is
/// API-wide, so this is consistent with the spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Callback {
    pub name: String,
    /// Runtime expression keyed by the path-item entry, e.g.
    /// `{$request.body#/callbackUrl}`. Verbatim from the spec.
    pub expression: String,
    /// Ids referencing into [`Ir::operations`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

/// OAS Link Object — HATEOAS-style "given this response, here's how to
/// call the next operation". Carried in a `Vec<(String, Link)>` on
/// `Response.links` (named, ordered).
///
/// Per OAS, `operation_ref` and `operation_id` are mutually exclusive.
/// The parser keeps the first one declared if both appear.
///
/// `parameters` and `request_body` carry OAS *runtime expressions*
/// (e.g. `$response.body#/id`). The IR stores them as `ValueRef`s
/// indexing into [`Ir::values`]; compound expressions are now
/// preserved via the value pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Map of parameter name → runtime expression / scalar literal.
    /// Order is preserved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<(String, ValueRef)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<ValueRef>,
    /// OAS §4.20: Link Object's `description` (CommonMark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-link `server` override (rare).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

/// OAS Schema Object's `xml` block: governs how the schema serializes
/// to XML — element name override, namespace, prefix, attribute-vs-
/// element placement, array wrapping. No in-tree generator currently
/// emits XML clients; the IR carries the data so a future XML-capable
/// generator can consume it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct XmlObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// `true` ⇒ render as XML attribute on the parent element;
    /// `false` ⇒ render as child element. Defaults to `false`.
    #[serde(default)]
    pub attribute: bool,
    /// Array-only: `true` ⇒ wrap the array in a parent element
    /// (`<wrapper><item/><item/></wrapper>`). Defaults to `false`.
    #[serde(default)]
    pub wrapped: bool,
    /// OAS 3.2 `text` — `true` ⇒ render the value as element text
    /// content rather than a child element or attribute. Defaults to
    /// `false`.
    #[serde(default)]
    pub text: bool,
    /// OAS 3.2 `ordered` — array-only: `true` ⇒ element order is
    /// significant (consumers must preserve it). Defaults to `false`.
    #[serde(default)]
    pub ordered: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

/// OAS Example Object. Carried in a `Vec<(String, Example)>` on
/// `Parameter` / `BodyContent` / `NamedType` (named, ordered).
/// 3.0 specs that declare a single bare `example` (no name) are
/// stored under the synthetic key `"_default"` so generators have
/// one shape to read.
///
/// `value` is the inline literal — a `ValueRef` indexing into the IR's
/// value pool. `external_value` is the spec's URL escape hatch and is
/// mutually exclusive with `value`; the parser warns and keeps `value`
/// when both are declared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Example {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_value: Option<String>,
    /// OAS 3.2 `dataValue` — the parsed/decoded form of the example.
    /// Spec splits the 3.0/3.1 `value` into `dataValue` (parsed) and
    /// `serializedValue` (wire form) so generators can pick the
    /// representation that matches their language. `ValueRef` indexes
    /// into [`Ir::values`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_value: Option<ValueRef>,
    /// OAS 3.2 `serializedValue` — the wire form as a string (e.g. the
    /// JSON text, urlencoded body). Mutually exclusive with
    /// `external_value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serialized_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiInfo {
    pub title: String,
    pub version: String,
    /// OAS 3.1+ `summary` — single-line API synopsis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// OAS `description` — long-form prose (CommonMark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URL pointing to the API's terms of service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_of_service: Option<String>,
    /// `info.contact` block (any of `name` / `url` / `email`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
    /// `info.license.name` — required by OAS when `license` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_name: Option<String>,
    /// `info.license.url` — mutually exclusive with `license.identifier`
    /// in OAS 3.1+, but kept independent here so 3.0 specs round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    /// SPDX license identifier (3.1 `info.license.identifier`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_identifier: Option<String>,
    /// `x-*` extensions declared on the info object. Compound
    /// extensions drop with `parser/W-EXTENSION-DROPPED`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// OAS 3.2 `name` — short label distinct from `description`,
    /// surfaced by tooling that displays multiple servers in a picker
    /// UI. Carried verbatim; absent on 3.0 / 3.1 specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tuples preserve declared order across the WIT boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<(String, ServerVariable)>,
    /// `x-*` extensions declared on the server object. Compound
    /// extensions drop with `parser/W-EXTENSION-DROPPED`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerVariable {
    pub default: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#enum: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `x-*` extensions declared on the server-variable object.
    /// Compound extensions drop with `parser/W-EXTENSION-DROPPED`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_ir() -> Ir {
        Ir {
            info: ApiInfo {
                title: "test".into(),
                version: "0".into(),
                summary: None,
                description: None,
                terms_of_service: None,
                contact: None,
                license_name: None,
                license_url: None,
                license_identifier: None,
                extensions: vec![],
            },
            operations: vec![],
            types: vec![],
            security_schemes: vec![],
            servers: vec![],
            webhooks: vec![],
            external_docs: None,
            tags: vec![],
            json_schema_dialect: None,
            self_url: None,
            values: vec![],
        }
    }

    #[test]
    fn json_roundtrip_minimal() {
        let ir = minimal_ir();
        let json = serde_json::to_string(&ir).unwrap();
        let back: Ir = serde_json::from_str(&json).unwrap();
        pretty_assertions::assert_eq!(ir, back);
    }
}
