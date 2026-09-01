//! `proptest` strategies for generating arbitrary IR values.
//!
//! Used by `forge-ir-bindgen` for the WIT roundtrip property test, and
//! available to plugin authors for their own tests.

use proptest::prelude::*;

use crate::diagnostic::SpecLocation;
use crate::operation::HttpMethod;
use crate::types::{
    AdditionalProperties, ArrayConstraints, ArrayType, IntKind, NamedType, ObjectConstraints,
    ObjectType, PatternProperty, PrimitiveConstraints, PrimitiveKind, PrimitiveType, Property,
    TypeDef, UnionKind, UnionType, UnionVariant, NULL_ID,
};
use crate::value::{Value, ValueRef};
use crate::{ApiInfo, Ir};

/// A simple identifier suitable for type ids and names.
pub fn ident() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_]{0,15}".prop_map(|s| s.to_string())
}

/// Scalar `Value` generator (no compound arms — those reference into the
/// pool and need a pool-size context). f64 is restricted to finite values
/// to avoid NaN equality issues in roundtrip tests.
pub fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(|b| Value::Bool { value: b }),
        any::<i64>().prop_map(|v| Value::Int { value: v }),
        proptest::num::f64::NORMAL.prop_map(|v| Value::Float { value: v }),
        ".{0,20}".prop_map(|s| Value::String { value: s }),
    ]
}

/// Generates a value pool plus an optional `ValueRef` into it. Used to
/// populate `Ir.values` with a deterministic mix of scalar and compound
/// nodes for roundtrip testing.
pub fn values_pool() -> impl Strategy<Value = Vec<Value>> {
    // Bottom layer: 0..6 scalar values. Then optionally append one List
    // and one Object referencing earlier indices.
    (
        prop::collection::vec(value(), 0..6),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(scalars, with_list, with_object)| {
            let mut pool = scalars;
            if with_list && !pool.is_empty() {
                let items: Vec<ValueRef> = (0..pool.len() as ValueRef).take(3).collect();
                pool.push(Value::List { items });
            }
            if with_object && !pool.is_empty() {
                let fields: Vec<(String, ValueRef)> = (0..pool.len() as ValueRef)
                    .take(2)
                    .enumerate()
                    .map(|(i, r)| (format!("k{i}"), r))
                    .collect();
                pool.push(Value::Object { fields });
            }
            pool
        })
}

pub fn primitive_kind() -> impl Strategy<Value = PrimitiveKind> {
    prop_oneof![
        Just(PrimitiveKind::String),
        Just(PrimitiveKind::Integer),
        Just(PrimitiveKind::Number),
        Just(PrimitiveKind::Bool),
    ]
}

pub fn primitive_constraints() -> impl Strategy<Value = PrimitiveConstraints> {
    (
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        prop::option::of(".{0,8}".prop_map(|s| s.to_string())),
    )
        .prop_map(|(min_len, max_len, pat)| PrimitiveConstraints {
            // Numeric constraint slots stay empty in randomly-generated
            // IRs — they're `ValueRef`s now and would require coordinated
            // pool generation. Roundtrip coverage for them is provided
            // separately via fixture-driven tests.
            minimum: None,
            maximum: None,
            exclusive_minimum: None,
            exclusive_maximum: None,
            multiple_of: None,
            min_length: min_len,
            max_length: max_len,
            pattern: pat,
            format_extension: None,
            content_encoding: None,
            content_media_type: None,
            content_schema: None,
        })
}

pub fn primitive_type() -> impl Strategy<Value = PrimitiveType> {
    (primitive_kind(), primitive_constraints())
        .prop_map(|(kind, constraints)| PrimitiveType { kind, constraints })
}

pub fn array_type() -> impl Strategy<Value = ArrayType> {
    (
        ident(),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        any::<bool>(),
    )
        .prop_map(|(items, min, max, unique)| ArrayType {
            items,
            constraints: ArrayConstraints {
                min_items: min,
                max_items: max,
                unique_items: unique,
            },
        })
}

pub fn additional_properties() -> impl Strategy<Value = AdditionalProperties> {
    prop_oneof![
        Just(AdditionalProperties::Forbidden),
        Just(AdditionalProperties::Any),
        ident().prop_map(|t| AdditionalProperties::Typed { r#type: t }),
    ]
}

pub fn property() -> impl Strategy<Value = Property> {
    (
        ident(),
        ident(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(name, ty, required, deprecated, ro, wo)| Property {
            name,
            r#type: ty,
            required,
            title: None,
            description: None,
            deprecated,
            read_only: ro,
            write_only: wo,
            external_docs: None,
            default: None,
            examples: vec![],
            extensions: vec![],
        })
}

pub fn pattern_property() -> impl Strategy<Value = PatternProperty> {
    (ident(), ident()).prop_map(|(pattern, ty)| PatternProperty {
        pattern,
        r#type: ty,
    })
}

pub fn object_type() -> impl Strategy<Value = ObjectType> {
    (
        prop::collection::vec(property(), 0..4),
        prop::collection::vec(pattern_property(), 0..3),
        additional_properties(),
        prop::option::of(ident()),
    )
        .prop_map(|(props, pats, ap, pn)| ObjectType {
            properties: props,
            pattern_properties: pats,
            additional_properties: ap,
            property_names: pn,
            constraints: ObjectConstraints::default(),
        })
}

pub fn union_type() -> impl Strategy<Value = UnionType> {
    (
        prop::collection::vec(
            (ident(), prop::option::of(ident()))
                .prop_map(|(t, tag)| UnionVariant { r#type: t, tag }),
            1..4,
        ),
        prop_oneof![Just(UnionKind::OneOf), Just(UnionKind::AnyOf)],
    )
        .prop_map(|(mut variants, kind)| {
            // Canonicalisation invariant (issue #107): any variant
            // referencing the canonical Null id sorts to the end. Stable
            // partition preserves the relative order of non-null variants.
            variants.sort_by_key(|v| (v.r#type == NULL_ID) as u8);
            UnionType {
                variants,
                discriminator: None,
                kind,
            }
        })
}

pub fn type_def() -> impl Strategy<Value = TypeDef> {
    prop_oneof![
        primitive_type().prop_map(TypeDef::Primitive),
        array_type().prop_map(TypeDef::Array),
        object_type().prop_map(TypeDef::Object),
        union_type().prop_map(TypeDef::Union),
        Just(TypeDef::Null),
        Just(TypeDef::Any),
    ]
}

pub fn named_type() -> impl Strategy<Value = NamedType> {
    (ident(), type_def()).prop_map(|(id, definition)| NamedType {
        id,
        original_name: None,
        title: None,
        description: None,
        deprecated: false,
        read_only: false,
        write_only: false,
        external_docs: None,
        default: None,
        examples: vec![],
        xml: None,
        definition,
        extensions: vec![],
        location: None,
    })
}

pub fn http_method() -> impl Strategy<Value = HttpMethod> {
    prop_oneof![
        Just(HttpMethod::Get),
        Just(HttpMethod::Put),
        Just(HttpMethod::Post),
        Just(HttpMethod::Delete),
        Just(HttpMethod::Options),
        Just(HttpMethod::Head),
        Just(HttpMethod::Patch),
        Just(HttpMethod::Trace),
    ]
}

pub fn spec_location() -> impl Strategy<Value = SpecLocation> {
    (".{0,20}".prop_map(|s| s.to_string()),).prop_map(|(p,)| SpecLocation::new(p))
}

pub fn int_kind() -> impl Strategy<Value = IntKind> {
    prop_oneof![Just(IntKind::Int32), Just(IntKind::Int64)]
}

/// A trivial top-level IR with no operations, a few types, and a small
/// value pool. Larger IRs are composed by callers as needed.
pub fn small_ir() -> impl Strategy<Value = Ir> {
    (
        ident(),
        ident(),
        prop::collection::vec(named_type(), 0..6),
        values_pool(),
    )
        .prop_map(|(title, version, types, values)| Ir {
            info: ApiInfo {
                title,
                version,
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
            types,
            security_schemes: vec![],
            servers: vec![],
            webhooks: vec![],
            external_docs: None,
            tags: vec![],
            json_schema_dialect: None,
            self_url: None,
            values,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #[test]
        fn small_ir_serializes(ir in small_ir()) {
            let s = serde_json::to_string(&ir).unwrap();
            let _back: Ir = serde_json::from_str(&s).unwrap();
        }
    }
}
