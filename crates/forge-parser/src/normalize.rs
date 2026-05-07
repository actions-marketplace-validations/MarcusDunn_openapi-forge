//! Schema normalization passes.
//!
//! Currently: eager `allOf` flattening into a single `ObjectType`. Walks
//! every sub-schema, resolves `$ref`s into already-parsed component
//! objects, and merges properties / required / additional-properties /
//! constraints. Conflicts emit `parser/W-ALLOF-CONFLICT`; non-object
//! parts emit `parser/E-COMPOSITION-ALLOF` (Stage 4 keeps allOf scoped to
//! object inheritance).

use forge_ir::{
    AdditionalProperties, NamedType, ObjectConstraints, ObjectType, Property, TypeDef, TypeRef,
};
use indexmap::IndexMap;
use serde_json::Value as J;

use crate::ctx::Ctx;
use crate::diag;
use crate::pointer::Ptr;
use crate::schema::{
    alloc_id, description, maybe_wrap_nullable, original_name, parse_schema, title, NameHint,
};

#[derive(Debug)]
struct Acc {
    /// Insertion order preserved on first occurrence, even when later parts
    /// re-declare a property.
    properties: IndexMap<String, Property>,
    /// Tracks the union of `required` across parts. Stored as Vec to keep
    /// the original declaration order from sub-parts; deduped on insert.
    required: Vec<String>,
    additional: AdditionalProperties,
    min_properties: Option<u64>,
    max_properties: Option<u64>,
}

impl Acc {
    fn new() -> Self {
        Self {
            properties: IndexMap::new(),
            required: Vec::new(),
            additional: AdditionalProperties::Any,
            min_properties: None,
            max_properties: None,
        }
    }

    fn add_required(&mut self, name: &str) {
        if !self.required.iter().any(|r| r == name) {
            self.required.push(name.to_string());
        }
    }
}

/// Flatten a schema with `allOf` into a single `ObjectType`. Registers the
/// flattened type and returns its id. Sub-parts are walked through the
/// regular schema walker so any inline objects they contain still land in
/// the type pool.
pub(crate) fn parse_all_of(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let parts = match map.get("allOf") {
        Some(J::Array(items)) if !items.is_empty() => items.clone(),
        _ => {
            ptr.with_token("allOf", |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_COMPOSITION_ALLOF,
                    "`allOf` must be a non-empty array",
                    ptr.loc(ctx.file),
                ));
            });
            return None;
        }
    };

    let id = alloc_id(ctx, &hint);
    let mut acc = Acc::new();
    let mut got_anything = false;
    // Synthetic "part" container objects we lifted via parse_schema. After
    // merging their fields into the accumulator they're orphaned (no
    // other type points at them), so we drop them from the pool to keep
    // the IR — and downstream TS — clean. Their *property* types stay
    // because the flattened object still references them by id.
    let mut synthetic_parts: Vec<String> = Vec::new();

    ptr.with_token("allOf", |ptr| {
        for (i, sub) in parts.iter().enumerate() {
            ptr.with_index(i, |ptr| {
                if let Some(part_ref) = merge_part(ctx, sub, ptr, &id, i, &mut acc) {
                    got_anything = true;
                    // Only synthesised inline parts ($ref parts return the
                    // component's own id, which we must NOT drop).
                    if part_ref.starts_with(&format!("{id}_allof_part_")) {
                        synthetic_parts.push(part_ref);
                    }
                }
            });
        }
    });

    // If the parent schema also declares its own properties / required at
    // the same level (a common 3.0 idiom for "extend X with these extra
    // fields"), merge them in too.
    merge_inline_object_fields(ctx, map, ptr, &id, &mut acc);

    if !got_anything && acc.properties.is_empty() {
        return None;
    }

    // Materialise per-property `required` from the accumulator's
    // required-name set (issue #110). The same name appears in
    // multiple `allOf` parts coalesces here.
    let mut properties: Vec<Property> = acc.properties.into_values().collect();
    for p in properties.iter_mut() {
        if acc.required.contains(&p.name) {
            p.required = true;
        }
    }
    let obj = ObjectType {
        properties,
        additional_properties: acc.additional,
        constraints: ObjectConstraints {
            min_properties: acc.min_properties,
            max_properties: acc.max_properties,
        },
    };
    let (read_only, write_only) = crate::schema::read_write_only(map);
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id,
        original_name: original_name(&hint),
        documentation: description(map),
        title: title(map),
        read_only,
        write_only,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Object(obj),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    let outer_id = maybe_wrap_nullable(ctx, nt, nullable);
    for part in synthetic_parts {
        ctx.types.shift_remove(&part);
    }
    Some(outer_id)
}

/// Returns the part's TypeRef on a successful merge so the caller can drop
/// it from the pool when it's a redundant inline container.
fn merge_part(
    ctx: &mut Ctx,
    sub: &J,
    ptr: &mut Ptr,
    owner_id: &str,
    index: usize,
    acc: &mut Acc,
) -> Option<TypeRef> {
    let role = format!("allof_part_{index}");
    let part_ref = parse_schema(ctx, sub, ptr, NameHint::inline(owner_id, &role))?;
    let nt = ctx.types.get(&part_ref)?.clone();
    let part_obj = match &nt.definition {
        TypeDef::Object(o) => o.clone(),
        _ => {
            ctx.push_diag(diag::err(
                diag::E_COMPOSITION_ALLOF,
                format!(
                    "allOf part {index} resolves to a non-object type (`{part_ref}`); only \
                     object composition is supported"
                ),
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    for prop in &part_obj.properties {
        if let Some(existing) = acc.properties.get(&prop.name) {
            if existing.r#type != prop.r#type {
                ctx.push_diag(diag::warn(
                    diag::W_ALLOF_CONFLICT,
                    format!(
                        "allOf merge: property `{}` declared with conflicting types `{}` and `{}`; using the latter",
                        prop.name, existing.r#type, prop.r#type
                    ),
                    ptr.loc(ctx.file),
                ));
            }
        }
        acc.properties.insert(prop.name.clone(), prop.clone());
        // Lifted: if this part's property is required, accumulate it
        // (the per-property `required` flag is materialised at the
        // end of merging).
        if prop.required {
            acc.add_required(&prop.name);
        }
    }
    acc.additional = restrict_additional(acc.additional.clone(), part_obj.additional_properties);
    if let Some(m) = part_obj.constraints.min_properties {
        acc.min_properties = Some(acc.min_properties.map(|x| x.max(m)).unwrap_or(m));
    }
    if let Some(m) = part_obj.constraints.max_properties {
        acc.max_properties = Some(acc.max_properties.map(|x| x.min(m)).unwrap_or(m));
    }
    Some(part_ref)
}

/// If the parent schema has sibling `properties`/`required`/etc at the same
/// level as `allOf`, merge them in. We do this by walking those fields
/// directly (rather than synthesizing another inline object) so we don't
/// pollute the type pool with a redundant `_self` part.
fn merge_inline_object_fields(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    owner_id: &str,
    acc: &mut Acc,
) {
    if let Some(J::Object(props)) = map.get("properties") {
        ptr.with_token("properties", |ptr| {
            for (name, schema) in props {
                ptr.with_token(name, |ptr| {
                    let role = format!("property_{}", crate::sanitize::ident(name));
                    if let Some(t) =
                        parse_schema(ctx, schema, ptr, NameHint::inline(owner_id, &role))
                    {
                        let (doc, deprecated, read_only, write_only, default) = match schema {
                            J::Object(m) => (
                                description(m),
                                m.get("deprecated").and_then(J::as_bool).unwrap_or(false),
                                m.get("readOnly").and_then(J::as_bool).unwrap_or(false),
                                m.get("writeOnly").and_then(J::as_bool).unwrap_or(false),
                                crate::parse_default(ctx, m, ptr, "property"),
                            ),
                            _ => (None, false, false, false, None),
                        };
                        if let Some(existing) = acc.properties.get(name) {
                            if existing.r#type != t {
                                ctx.push_diag(diag::warn(
                                    diag::W_ALLOF_CONFLICT,
                                    format!(
                                        "allOf merge: property `{}` declared with conflicting types `{}` and `{}`; using the latter",
                                        name, existing.r#type, t
                                    ),
                                    ptr.loc(ctx.file),
                                ));
                            }
                        }
                        let extensions = match schema {
                            J::Object(m) => crate::operations::collect_extensions(ctx, m, ptr),
                            _ => Vec::new(),
                        };
                        acc.properties.insert(
                            name.clone(),
                            Property {
                                name: name.clone(),
                                r#type: t,
                                required: false, // patched from acc.required at merge end
                                documentation: doc,
                                deprecated,
                                read_only,
                                write_only,
                                default,
                                extensions,
                            },
                        );
                    }
                });
            }
        });
    }
    if let Some(J::Array(items)) = map.get("required") {
        for v in items {
            if let Some(s) = v.as_str() {
                acc.add_required(s);
            }
        }
    }
    if let Some(m) = map.get("minProperties").and_then(J::as_u64) {
        acc.min_properties = Some(acc.min_properties.map(|x| x.max(m)).unwrap_or(m));
    }
    if let Some(m) = map.get("maxProperties").and_then(J::as_u64) {
        acc.max_properties = Some(acc.max_properties.map(|x| x.min(m)).unwrap_or(m));
    }
    match map.get("additionalProperties") {
        Some(J::Bool(false)) => {
            acc.additional =
                restrict_additional(acc.additional.clone(), AdditionalProperties::Forbidden);
        }
        Some(J::Bool(true)) | None => {}
        Some(J::Object(_)) => {
            if let Some(t) = ptr.with_token("additionalProperties", |ptr| {
                parse_schema(
                    ctx,
                    map.get("additionalProperties").unwrap(),
                    ptr,
                    NameHint::inline(owner_id, "additional_properties"),
                )
            }) {
                acc.additional = restrict_additional(
                    acc.additional.clone(),
                    AdditionalProperties::Typed { r#type: t },
                );
            }
        }
        Some(_) => {}
    }
}

/// Most-restrictive merge: `Forbidden` > `Typed` > `Any`. Two `Typed` with
/// different element types is a conflict and we keep the first (warning).
fn restrict_additional(a: AdditionalProperties, b: AdditionalProperties) -> AdditionalProperties {
    use AdditionalProperties as A;
    match (a, b) {
        (A::Forbidden, _) | (_, A::Forbidden) => A::Forbidden,
        (A::Typed { r#type: t }, A::Any) | (A::Any, A::Typed { r#type: t }) => {
            A::Typed { r#type: t }
        }
        // Two `Typed` parts: we keep the first regardless. Different
        // element types are a structural conflict, but property-level
        // merging surfaces the same condition via `W-ALLOF-CONFLICT`
        // already, so we don't double-emit at this layer.
        (A::Typed { r#type: a_t }, A::Typed { .. }) => A::Typed { r#type: a_t },
        (A::Any, A::Any) => A::Any,
    }
}
