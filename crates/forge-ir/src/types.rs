//! Type-system pieces of the IR.
//!
//! Every named type lives in [`crate::Ir::types`] keyed by its sanitized [`NamedType::id`].
//! [`TypeRef`] is just that string. This makes recursion trivial across the WIT
//! boundary (no recursive records) and keeps the structure flat.

use serde::{Deserialize, Serialize};

use crate::diagnostic::SpecLocation;
use crate::value::ValueRef;

/// String id into [`crate::Ir::types`].
pub type TypeRef = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedType {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    /// JSON Schema `title` — short human label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// JSON Schema / OAS `description` (CommonMark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema 2020-12 `deprecated`.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub deprecated: bool,
    /// JSON Schema `readOnly` at the schema level. Generators that
    /// distinguish request from response shapes can opt to drop
    /// `read_only` types from their request surface.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub read_only: bool,
    /// JSON Schema `writeOnly` at the schema level. Generators that
    /// distinguish request from response shapes can opt to drop
    /// `write_only` types from their response surface.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub write_only: bool,
    /// Per-schema `externalDocs` (OAS Schema Object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<crate::ExternalDocs>,
    /// JSON Schema `default` at the schema level. `ValueRef` indexes
    /// into [`crate::Ir::values`]; compound defaults are pooled there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ValueRef>,
    /// OAS `example` / `examples` on the schema. Named entries; 3.0
    /// `example: <literal>` lands under the synthetic key `"_default"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<(String, crate::Example)>,
    /// OAS `xml` block — name override, namespace, prefix, attribute
    /// placement, array wrapping. None unless the spec declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xml: Option<crate::XmlObject>,
    pub definition: TypeDef,
    /// `x-*` extensions declared on the schema. Each entry pairs a key
    /// with a [`ValueRef`] into [`crate::Ir::values`]; compound values
    /// are pooled there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SpecLocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "def", rename_all = "kebab-case")]
pub enum TypeDef {
    Primitive(PrimitiveType),
    Object(ObjectType),
    Array(ArrayType),
    EnumString(EnumStringType),
    EnumInt(EnumIntType),
    /// A boolean-typed single-value (`const`) or closed set (`enum`). JSON
    /// Schema `const: true` lowers here with one value; a future
    /// `enum: [true, false]` would carry both. See issue #107.
    EnumBool(EnumBoolType),
    /// A number-typed (non-integer) single-value (`const`) or closed set.
    /// JSON Schema `const: 1.5` lowers here. Integer consts/enums stay on
    /// [`TypeDef::EnumInt`]; this variant covers `number`-typed literals.
    EnumNumber(EnumNumberType),
    Union(UnionType),
    /// JSON's `null` as a unit type. The canonical singleton lives in
    /// [`crate::Ir::types`] under id [`NULL_ID`]; `T | null` is expressed
    /// as a [`UnionType`] whose variants list contains a `Null` reference
    /// (canonicalized to last). See issue #107.
    Null,
    /// The JSON Schema "any" schema — an empty/freeform schema (`{}`) or the
    /// boolean schema `true`. It validates *any* instance (object, array,
    /// string, number, boolean, or null). Per JSON Schema 2020-12 §4.3.2, `{}`
    /// and `true` are equivalent. Distinct from an [`ObjectType`] with
    /// permissive `additionalProperties` (`{"type":"object"}`), which validates
    /// objects only — collapsing the two would reject otherwise-valid
    /// non-object instances.
    Any,
}

/// Canonical pool id for the [`TypeDef::Null`] singleton. See issue #107.
pub const NULL_ID: &str = "null";

// ---- primitives ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrimitiveType {
    pub kind: PrimitiveKind,
    pub constraints: PrimitiveConstraints,
}

/// JSON Schema's `type` keyword values, minus the variants that have
/// their own IR shapes (`object` / `array` / `null`). The `format`
/// keyword and any width/semantic refinement (`int32` / `int64` /
/// `float` / `double` / `date` / `uuid` / `byte` / `decimal` / etc.)
/// land on [`PrimitiveConstraints::format_extension`] verbatim.
/// Plugins decide whether to produce a richer target-language type
/// based on the format string. This keeps the IR uniform and
/// orthogonal — adding new formats never requires an IR/WIT/bindgen
/// roundtrip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrimitiveKind {
    /// JSON Schema `type: "string"`.
    String,
    /// JSON Schema `type: "integer"`. Width refinements
    /// (`int32` / `int64`) live in `format_extension`.
    Integer,
    /// JSON Schema `type: "number"`. Width refinements
    /// (`float` / `double`) and `decimal` live in `format_extension`.
    Number,
    /// JSON Schema `type: "boolean"`.
    Bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PrimitiveConstraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_minimum: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_maximum: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple_of: Option<ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_extension: Option<String>,
    /// JSON Schema 2020-12 `contentEncoding` (e.g. `base64`,
    /// `base32`). Describes how the string value is encoded; the
    /// decoded payload may have its own media type and schema.
    /// String-only — populated for `PrimitiveKind::String` schemas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    /// JSON Schema 2020-12 `contentMediaType` (e.g. `image/png`,
    /// `application/json`). The media type of the decoded payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_media_type: Option<String>,
    /// OAS 3.2 / JSON Schema 2020-12 `contentSchema` — schema for the
    /// decoded payload (after applying `contentEncoding`). Carries a
    /// `TypeRef` into [`crate::Ir::types`] so generators that decode
    /// `contentEncoding` content can validate / shape it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_schema: Option<TypeRef>,
}

// ---- arrays ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayType {
    pub items: TypeRef,
    pub constraints: ArrayConstraints,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ArrayConstraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u64>,
    #[serde(default)]
    pub unique_items: bool,
}

// ---- objects ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectType {
    pub properties: Vec<Property>,
    /// JSON Schema `patternProperties` — each entry pairs an ECMA-262
    /// regex against the schema that property *values* whose name
    /// matches the regex must satisfy. Orthogonal to `properties`
    /// (which match by exact name) and to `additional_properties`
    /// (the fallback for names that match neither). Empty when the
    /// keyword is absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pattern_properties: Vec<PatternProperty>,
    pub additional_properties: AdditionalProperties,
    /// JSON Schema `propertyNames` — a schema every property *name* in
    /// the object must validate against. Names are always strings, so
    /// this points at a string-shaped type (typically carrying a
    /// `pattern` / `min_length` / `max_length` constraint). `None`
    /// when the keyword is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_names: Option<TypeRef>,
    pub constraints: ObjectConstraints,
}

/// One entry of JSON Schema `patternProperties`: a property-name regex
/// paired with the schema that matching property values must satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternProperty {
    /// The raw ECMA-262 regular expression key. Generators targeting a
    /// non-ECMA regex engine translate it, the same way they do for
    /// [`PrimitiveConstraints::pattern`].
    pub pattern: String,
    /// Schema that values under a matching property name must satisfy.
    #[serde(rename = "type")]
    pub r#type: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Property {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: TypeRef,
    /// `true` when the spec lists this property name in the parent
    /// schema's `required` array. Moved here from
    /// `ObjectType.required` for a uniform spelling with
    /// `Parameter.required`.
    #[serde(default)]
    pub required: bool,
    /// JSON Schema `title` — short human label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// JSON Schema / OAS `description` (CommonMark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema 2020-12 `deprecated`.
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub write_only: bool,
    /// Per-schema `externalDocs` (OAS Schema Object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<crate::ExternalDocs>,
    /// JSON Schema `default` for the property. `ValueRef` indexes
    /// into [`crate::Ir::values`]; compound defaults are pooled there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ValueRef>,
    /// OAS `example` / `examples` for this property. Named entries;
    /// 3.0 `example: <literal>` lands under the synthetic key
    /// `"_default"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<(String, crate::Example)>,
    /// `x-*` extensions declared on the property's schema. Each entry
    /// pairs a key with a [`ValueRef`] into [`crate::Ir::values`];
    /// compound values are pooled there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AdditionalProperties {
    Forbidden,
    Any,
    Typed { r#type: TypeRef },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ObjectConstraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_properties: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_properties: Option<u64>,
}

// ---- enums ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumStringType {
    pub values: Vec<EnumStringValue>,
}

/// One value in a string-typed enum. OAS / JSON Schema does not define
/// per-value documentation, so this is a bare value. Per-value docs
/// would have to come from a vendor extension and are out of scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumStringValue {
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumIntType {
    pub values: Vec<EnumIntValue>,
    pub kind: IntKind,
}

/// One value in an integer-typed enum. OAS / JSON Schema does not
/// define per-value documentation; see [`EnumStringValue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumIntValue {
    pub value: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntKind {
    Int32,
    Int64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumBoolType {
    pub values: Vec<EnumBoolValue>,
}

/// One value in a boolean-typed enum. OAS / JSON Schema does not define
/// per-value documentation; see [`EnumStringValue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumBoolValue {
    pub value: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumNumberType {
    pub values: Vec<EnumNumberValue>,
    pub kind: NumberKind,
}

/// One value in a number-typed enum. OAS / JSON Schema does not define
/// per-value documentation; see [`EnumStringValue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumNumberValue {
    pub value: f64,
}

/// Width refinement for a number-typed literal, mirroring [`IntKind`].
/// `format: float` selects [`NumberKind::Float`]; everything else
/// (including `format: double` and an absent format) is [`NumberKind::Double`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NumberKind {
    Float,
    Double,
}

// ---- unions ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnionType {
    pub variants: Vec<UnionVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<Discriminator>,
    pub kind: UnionKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnionVariant {
    #[serde(rename = "type")]
    pub r#type: TypeRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnionKind {
    OneOf,
    AnyOf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discriminator {
    pub property_name: String,
    pub mapping: Vec<(String, TypeRef)>,
    /// `x-*` extensions declared on the discriminator object. Each
    /// entry pairs a key with a [`ValueRef`] into [`crate::Ir::values`];
    /// compound extension values are pooled there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<(String, ValueRef)>,
}
