//! WIT bindings for the OpenAPI Forge plugin worlds, plus conversions to and
//! from [`forge_ir`].
//!
//! `wasmtime::component::bindgen!` produces one set of generated types per
//! world. Both `ir-transformer` and `code-generator` import `host-api` and
//! use the same `types` interface, but the macro generates *separate* Rust
//! modules per world (the types are structurally identical but nominally
//! distinct).
//!
//! This crate exposes both as `bindings::transformer` and
//! `bindings::generator`, plus a `convert` module providing fallible
//! conversions between the bindgen types and [`forge_ir`].

#![forbid(unsafe_code)]

pub mod bindings;
pub mod convert;

use forge_ir as ir;

/// Errors returned when fallible conversions hit a boundary violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindgenError {
    /// A `TypeRef` referred to a type id that did not appear in
    /// [`forge_ir::Ir::types`].
    DanglingTypeRef(String),
    /// A discriminator mapping referenced a type id that did not appear in
    /// [`forge_ir::Ir::types`].
    DanglingDiscriminator { type_ref: String },
    /// The same type id appeared more than once in [`forge_ir::Ir::types`].
    DuplicateTypeId(String),
    /// A status range outside `1..=5` was encountered.
    BadStatusRange(u8),
    /// A `ValueRef` index was out of bounds for [`forge_ir::Ir::values`].
    DanglingValueRef(u32),
}

impl core::fmt::Display for BindgenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindgenError::DanglingTypeRef(r) => write!(f, "dangling type ref: {r}"),
            BindgenError::DanglingDiscriminator { type_ref } => {
                write!(f, "dangling discriminator type ref: {type_ref}")
            }
            BindgenError::DuplicateTypeId(id) => write!(f, "duplicate type id: {id}"),
            BindgenError::BadStatusRange(c) => write!(f, "invalid status range class: {c}"),
            BindgenError::DanglingValueRef(r) => write!(f, "dangling value ref: {r}"),
        }
    }
}

impl std::error::Error for BindgenError {}

/// Validate that every `TypeRef` in `ir` resolves to a `NamedType.id`. The
/// host runs this on every IR returned from a plugin before passing it to
/// the next stage.
pub fn validate_refs(ir: &ir::Ir) -> Result<(), BindgenError> {
    let mut ids = std::collections::HashSet::with_capacity(ir.types.len());
    for t in &ir.types {
        if !ids.insert(t.id.as_str()) {
            return Err(BindgenError::DuplicateTypeId(t.id.clone()));
        }
    }
    let check = |r: &str| -> Result<(), BindgenError> {
        if ids.contains(r) {
            Ok(())
        } else {
            Err(BindgenError::DanglingTypeRef(r.to_string()))
        }
    };

    for t in &ir.types {
        match &t.definition {
            ir::TypeDef::Array(a) => check(&a.items)?,
            ir::TypeDef::Object(o) => {
                for p in &o.properties {
                    check(&p.r#type)?;
                }
                if let ir::AdditionalProperties::Typed { r#type } = &o.additional_properties {
                    check(r#type)?;
                }
            }
            ir::TypeDef::Union(u) => {
                for v in &u.variants {
                    check(&v.r#type)?;
                }
                if let Some(d) = &u.discriminator {
                    for (_, r) in &d.mapping {
                        if !ids.contains(r.as_str()) {
                            return Err(BindgenError::DanglingDiscriminator {
                                type_ref: r.clone(),
                            });
                        }
                    }
                }
            }
            ir::TypeDef::Primitive(_)
            | ir::TypeDef::EnumString(_)
            | ir::TypeDef::EnumInt(_)
            | ir::TypeDef::EnumBool(_)
            | ir::TypeDef::EnumNumber(_)
            | ir::TypeDef::Null
            | ir::TypeDef::Any => {}
        }
    }
    for op in &ir.operations {
        for p in op
            .path_params
            .iter()
            .chain(&op.query_params)
            .chain(&op.header_params)
            .chain(&op.cookie_params)
        {
            check(&p.r#type)?;
        }
        if let Some(b) = &op.request_body {
            for c in &b.content {
                check(&c.r#type)?;
            }
        }
        for r in &op.responses {
            for c in &r.content {
                check(&c.r#type)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangling_ref_is_rejected() {
        let bad = ir::Ir {
            info: ir::ApiInfo {
                title: "x".into(),
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
            types: vec![ir::NamedType {
                id: "Wrap".into(),
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
                definition: ir::TypeDef::Array(ir::ArrayType {
                    items: "Missing".into(),
                    constraints: ir::ArrayConstraints::default(),
                }),
                extensions: vec![],
                location: None,
            }],
            security_schemes: vec![],
            servers: vec![],
            webhooks: vec![],
            external_docs: None,
            tags: vec![],
            json_schema_dialect: None,
            self_url: None,
            values: vec![],
        };
        assert_eq!(
            validate_refs(&bad).unwrap_err(),
            BindgenError::DanglingTypeRef("Missing".into())
        );
    }
}
