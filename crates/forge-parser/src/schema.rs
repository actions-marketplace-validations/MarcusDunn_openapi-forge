//! Schema walker.
//!
//! Resolves a JSON Schema fragment into a `TypeRef` (string id into the
//! type pool). Inline schemas are lifted into the pool with synthesized
//! ids. Untagged composition (`oneOf` without `discriminator`, `anyOf`,
//! `not`) still produces error diagnostics; everything else is supported.

use forge_ir::{
    AdditionalProperties, ArrayConstraints, ArrayType, Discriminator, EnumIntType, EnumIntValue,
    EnumStringType, EnumStringValue, IntKind, NamedType, ObjectConstraints, ObjectType,
    PatternProperty, PrimitiveConstraints, PrimitiveKind, PrimitiveType, Property, TypeDef,
    TypeRef, UnionKind, UnionType, UnionVariant, NULL_ID,
};
use serde_json::Value as J;

use crate::ctx::Ctx;
use crate::diag;
use crate::pointer::Ptr;
use crate::refs::{self, RefOutcome};

/// How an inline schema should be named when lifted into the type pool.
#[derive(Debug, Clone)]
pub(crate) enum NameHint {
    /// Top-level schema in `components.schemas` — use this id verbatim.
    Named(String),
    /// Inline schema; synthesize `<owner>_<role>`.
    Inline { owner: String, role: String },
}

impl NameHint {
    pub fn inline(owner: impl Into<String>, role: impl Into<String>) -> Self {
        NameHint::Inline {
            owner: owner.into(),
            role: role.into(),
        }
    }

    fn base(&self) -> String {
        match self {
            NameHint::Named(s) => crate::sanitize::ident(s),
            NameHint::Inline { owner, role } => crate::sanitize::join(&[owner, role]),
        }
    }
}

/// Detect whether a schema is nullable. Recognises three forms:
/// - OpenAPI 3.0 `nullable: true`.
/// - OpenAPI 3.1 `type: [..., "null"]` (array with a `"null"` member).
/// - OpenAPI 3.1 standalone `type: "null"` (the schema permits *only*
///   null).
///
/// Any form flips the bit; multiple forms are equivalent.
pub(crate) fn detect_nullable(map: &serde_json::Map<String, J>) -> bool {
    if matches!(map.get("nullable"), Some(J::Bool(true))) {
        return true;
    }
    match map.get("type") {
        Some(J::Array(items)) if items.iter().any(|v| v.as_str() == Some("null")) => true,
        Some(J::String(s)) if s == "null" => true,
        _ => false,
    }
}

struct DeferredFeature {
    key: &'static str,
    code: &'static str,
    msg: &'static str,
}

/// JSON-Schema-2020-12 keywords 3.1+ inherits but the IR doesn't
/// surface yet. Each match emits a `parser/W-*-DROPPED` warning at
/// the keyword's site; the schema walker continues with the rest of
/// the schema. (#144 downgraded these from hard errors.)
fn collect_deferred_features(map: &serde_json::Map<String, J>) -> Vec<DeferredFeature> {
    const ENTRIES: &[(&str, &str, &str)] = &[
        (
            "dependentRequired",
            diag::W_DEPENDENT_REQUIRED_DROPPED,
            "`dependentRequired` (JSON Schema 2020-12) is not yet surfaced; dropping",
        ),
        (
            "dependentSchemas",
            diag::W_DEPENDENT_SCHEMAS_DROPPED,
            "`dependentSchemas` (JSON Schema 2020-12) is not yet surfaced; dropping",
        ),
        (
            "unevaluatedProperties",
            diag::W_UNEVALUATED_PROPERTIES_DROPPED,
            "`unevaluatedProperties` (JSON Schema 2020-12) is not yet surfaced; dropping",
        ),
        (
            "$dynamicRef",
            diag::W_DYNAMIC_REF_DROPPED,
            "`$dynamicRef` (JSON Schema 2020-12) is not yet surfaced; dropping",
        ),
        (
            "$dynamicAnchor",
            diag::W_DYNAMIC_ANCHOR_DROPPED,
            "`$dynamicAnchor` (JSON Schema 2020-12) is not yet surfaced; dropping",
        ),
    ];
    let mut out = Vec::new();
    for (key, code, msg) in ENTRIES {
        if map.contains_key(*key) {
            out.push(DeferredFeature { key, code, msg });
        }
    }
    out
}

/// Extract the non-null types from a schema's `type` field. Returns
/// `None` when `type` is missing or invalid; returns `Some(vec![...])`
/// otherwise. The 3.1 `type: ["string", "null"]` form yields
/// `Some(vec!["string"])`; multi-non-null arrays yield multiple entries
/// and are surfaced as untagged unions by `parse_schema`.
fn extract_types(map: &serde_json::Map<String, J>) -> Option<Vec<String>> {
    match map.get("type") {
        // Bare `type: "null"` is just nullable-with-no-shape; surface it
        // as "no concrete type" so the caller folds it into the freeform
        // path with `nullable: true` already set.
        Some(J::String(s)) if s == "null" => None,
        Some(J::String(s)) => Some(vec![s.clone()]),
        Some(J::Array(items)) => {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut out: Vec<String> = Vec::new();
            for v in items {
                let Some(s) = v.as_str() else { continue };
                if s == "null" {
                    continue;
                }
                if seen.insert(s.to_string()) {
                    out.push(s.to_string());
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// Parse a schema fragment, registering any inline types in the pool.
/// Returns the `TypeRef` (id) on success; on hard failure returns `None`
/// and pushes a diagnostic.
pub(crate) fn parse_schema(
    ctx: &mut Ctx,
    value: &J,
    ptr: &mut Ptr,
    hint: NameHint,
) -> Option<TypeRef> {
    let map = match value {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "schema must be an object",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    // 1. $ref short-circuits everything else. The current `hint` is
    //    threaded through so a named-component-with-ref (the
    //    `components.schemas.Pet = {$ref: ...}` pattern) lands the
    //    resolved type under the hint's id, not the external prefix.
    if let Some(r) = map.get("$ref") {
        // OAS 3.0 forbids siblings on `$ref`. JSON Schema 2020-12
        // (3.1+) allows them; merging is not yet implemented (tracked
        // as a follow-up to #74). For now, both versions ignore
        // siblings — but only 3.0 gets a warning since 3.1+ siblings
        // are legal and will eventually round-trip.
        if ctx.is_oas_3_0 {
            let has_non_ref_keys = map.keys().any(|k| k != "$ref" && !k.starts_with("x-"));
            if has_non_ref_keys {
                let dropped: Vec<&str> = map
                    .keys()
                    .filter(|k| k.as_str() != "$ref" && !k.starts_with("x-"))
                    .map(|k| k.as_str())
                    .collect();
                ctx.push_diag(diag::warn(
                    diag::W_REF_SIBLINGS_3_0,
                    format!(
                        "schema declares `$ref` together with sibling keys ({}); OAS 3.0 \
                         forbids siblings on `$ref`. Dropping siblings — promote to OpenAPI \
                         3.1+ to keep them.",
                        dropped.join(", ")
                    ),
                    ptr.loc(ctx.file),
                ));
            }
        }
        return ptr.with_token("$ref", |ptr| resolve_ref(ctx, r, ptr, &hint));
    }

    // 1b. JSON Schema 2020-12 features that 3.1 inherits but we defer.
    //     Each gets a stable reject code so users see a clear "not yet"
    //     message rather than silent best-effort output.
    // JSON Schema 2020-12 features that 3.1+ inherits but the IR
    // doesn't model yet. Previously hard errors that rejected the
    // whole spec; now warnings so the rest of the schema parses
    // (#144). Validators that need them can still inspect the source
    // spec out-of-band; codegen plugins that don't care just keep
    // walking. `dependentRequired` is a structured value the parser
    // *can* capture; the others stay warn-and-drop.
    for feature in collect_deferred_features(map) {
        ptr.with_token(feature.key, |ptr| {
            ctx.push_diag(diag::warn(feature.code, feature.msg, ptr.loc(ctx.file)));
        });
    }

    let nullable = detect_nullable(map);

    // 2. allOf eager flattening into a single ObjectType.
    if map.contains_key("allOf") {
        return crate::normalize::parse_all_of(ctx, map, ptr, hint, nullable);
    }

    // 3. oneOf with discriminator → tagged union; bare oneOf and anyOf
    //    fold into untagged unions.
    if map.contains_key("oneOf") {
        if map.contains_key("discriminator") {
            return parse_oneof_discriminated(ctx, map, ptr, hint, nullable);
        }
        return parse_untagged_union(ctx, map, ptr, hint, nullable, "oneOf", UnionKind::OneOf);
    }
    if map.contains_key("anyOf") {
        return parse_untagged_union(ctx, map, ptr, hint, nullable, "anyOf", UnionKind::AnyOf);
    }
    if map.contains_key("not") {
        ptr.with_token("not", |ptr| {
            ctx.push_diag(diag::err(
                diag::E_COMPOSITION_NOT,
                "not is not supported",
                ptr.loc(ctx.file),
            ));
        });
        return None;
    }

    // 4. `const` (3.1) — single-value enum. Single literal of any
    //    primitive type, plus the IR's existing nullable axis.
    if let Some(c) = map.get("const") {
        return parse_const(ctx, map, c, ptr, hint, nullable);
    }

    // 5. enum — branch on the declared type. Goes before the regular type
    //    dispatch so we don't double-emit a primitive plus an enum.
    let resolved_types = extract_types(map);
    if let Some(J::Array(values)) = map.get("enum") {
        let is_integer = matches!(resolved_types.as_deref(), Some([t]) if t == "integer");
        return if is_integer {
            parse_int_enum(ctx, map, values, ptr, hint, nullable)
        } else {
            parse_string_enum(ctx, map, values, ptr, hint, nullable)
        };
    }

    // 6. Resolve the declared type(s). 3.1 allows `type` to be an array
    //    (`["string", "null"]` → `detect_nullable` flips the bit;
    //    `["string", "integer"]` → multi-type → untagged union). A
    //    bare `type: "null"` or no `type` at all is a freeform schema —
    //    treat as a nullable freeform object so the surrounding
    //    structure (a `oneOf` / `anyOf` variant, a property type) gets
    //    something to point at.
    let types = match resolved_types {
        Some(ts) => ts,
        None => return parse_freeform(ctx, map, ptr, hint, nullable),
    };
    if types.len() > 1 {
        return parse_type_array_union(ctx, map, ptr, hint, nullable, &types);
    }
    let ty = types[0].as_str();

    match ty {
        "string" | "integer" | "number" | "boolean" => {
            parse_primitive(ctx, map, ptr, ty, hint, nullable)
        }
        "array" => parse_array(ctx, map, ptr, hint, nullable),
        "object" => parse_object(ctx, map, ptr, hint, nullable),
        other => {
            let msg = format!("unsupported schema type `{other}`");
            ptr.with_token("type", |ptr| {
                ctx.push_diag(diag::err(diag::E_INVALID_TYPE, msg, ptr.loc(ctx.file)));
            });
            None
        }
    }
}

/// Schemas with no `type` (freeform) or `type: "null"` (the bare
/// 3.1 null variant). Bare-null schemas resolve to a `TypeDef::Null`
/// node; freeform-with-no-type-info resolves to an empty object with
/// permissive `additionalProperties: any`. When `nullable` is true the
/// resolved shape is wrapped in a `Union(_, Null)`.
fn parse_freeform(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    // Detect "this schema is *only* null" — `type: "null"` (or `type:
    // ["null"]`) with no other schema-shape keywords. In that case emit
    // `TypeDef::Null` directly so the user-visible name aliases the
    // singleton structure rather than wrapping a freeform-any.
    if nullable && is_bare_null_schema(map) {
        match &hint {
            NameHint::Inline { .. } => return Some(ensure_null_singleton(ctx)),
            NameHint::Named(_) => {
                let extensions = crate::operations::collect_extensions(ctx, map, ptr);
                let nt = NamedType {
                    id: alloc_id(ctx, &hint),
                    original_name: original_name(&hint),
                    title: title(map),
                    description: description(map),
                    deprecated: deprecated(map),
                    read_only: read_write_only(map).0,
                    write_only: read_write_only(map).1,
                    external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
                    default: crate::parse_default(ctx, map, ptr, "schema"),
                    examples: crate::parse_examples(ctx, map, ptr),
                    xml: crate::parse_xml(ctx, map, ptr),
                    definition: TypeDef::Null,
                    extensions,
                    location: Some(ptr.loc(ctx.file)),
                };
                let id = nt.id.clone();
                ctx.push_type(nt);
                return Some(id);
            }
        }
    }
    // A schema with no `type` (and no composition keyword) is the JSON Schema
    // "any" schema — equivalent to `{}` / boolean `true` — which validates ANY
    // instance, not just objects. Lower it to `TypeDef::Any` rather than an
    // empty permissive object (`{"type":"object"}`), which would incorrectly
    // reject non-object instances (strings, numbers, arrays, …).
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: alloc_id(ctx, &hint),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Any,
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

/// True when the schema's only structural keyword is `type: "null"` (or
/// `type: ["null"]`). Metadata keywords (`description`, `title`,
/// `readOnly`, `writeOnly`, `default`, `example`, `examples`, `$comment`)
/// are tolerated — they ride along on the resulting NamedType.
fn is_bare_null_schema(map: &serde_json::Map<String, J>) -> bool {
    const METADATA: &[&str] = &[
        "type",
        "description",
        "title",
        "readOnly",
        "writeOnly",
        "default",
        "example",
        "examples",
        "$comment",
        "deprecated",
        "nullable",
    ];
    let only_metadata = map.keys().all(|k| METADATA.contains(&k.as_str()));
    if !only_metadata {
        return false;
    }
    match map.get("type") {
        Some(J::String(s)) if s == "null" => true,
        Some(J::Array(items)) => items.iter().all(|v| v.as_str() == Some("null")),
        None => false,
        _ => false,
    }
}

fn resolve_ref(ctx: &mut Ctx, r: &J, ptr: &mut Ptr, hint: &NameHint) -> Option<TypeRef> {
    let raw = match r {
        J::String(s) => s.as_str(),
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "$ref must be a string",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    match refs::resolve(ctx.refs(), raw) {
        RefOutcome::Component(id) => {
            // Inside the main spec a local component ref just returns
            // the id (`walk_component_schemas` covers the actual
            // walk). External docs aren't traversed by that walker, so
            // local refs there need to walk the pointed-at schema
            // lazily. The main spec is identified by its empty
            // doc-prefix entry (set in `Ctx::with_resolver`).
            let in_external = ctx
                .doc_prefix
                .get(&ctx.current_doc)
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            if in_external {
                let canonical = ctx.current_doc.clone();
                let (_, fragment) = crate::external::split_ref(raw);
                walk_resolved_schema_at_pointer(ctx, &canonical, fragment, raw, ptr, hint)
            } else {
                Some(id)
            }
        }
        RefOutcome::External => walk_external_ref(ctx, raw, ptr, hint),
        RefOutcome::UnsupportedLocal => {
            ctx.push_diag(diag::err(
                diag::E_EXTERNAL_REF,
                format!("$ref `{raw}` must point into #/components/schemas/"),
                ptr.loc(ctx.file),
            ));
            None
        }
        RefOutcome::Dangling(target) => {
            ctx.push_diag(diag::err(
                diag::E_DANGLING_REF,
                format!(
                    "$ref `{raw}` does not resolve to a declared schema (looked for `{target}`)"
                ),
                ptr.loc(ctx.file),
            ));
            None
        }
    }
}

/// Resolve an external `$ref` (cross-document). Loads the target via the
/// configured resolver, walks the JSON pointer to the inline value, and
/// lazy-walks it as a schema. Returns the prefixed type id.
///
/// The fragment may be any RFC 6901 JSON pointer — `/components/schemas/Pet`
/// (the standard form), `/Pet` (flat-root form), or anything else that
/// addresses a schema object inside the loaded document.
fn walk_external_ref(ctx: &mut Ctx, raw: &str, ptr: &mut Ptr, hint: &NameHint) -> Option<TypeRef> {
    let (file_part, fragment) = crate::external::split_ref(raw);
    if file_part.is_empty() {
        ctx.push_diag(diag::err(
            diag::E_EXTERNAL_REF,
            format!("external $ref `{raw}` could not be resolved"),
            ptr.loc(ctx.file),
        ));
        return None;
    }
    // Ask the resolver to load (or look up the cached) document.
    let current_doc = ctx.current_doc.clone();
    let loaded = match ctx.resolver.load(raw, &current_doc) {
        Ok(d) => d,
        Err(e) => {
            ctx.push_diag(diag::err(
                diag::E_EXTERNAL_REF,
                resolver_error_message(raw, &e),
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    let canonical = loaded.canonical_path.clone();
    ensure_doc_registered(ctx, &canonical, &loaded.root);
    walk_resolved_schema_at_pointer(ctx, &canonical, fragment, raw, ptr, hint)
}

/// Inner schema walker shared between cross-file refs (the resolver
/// found the doc on disk) and same-doc fragment refs inside an already-
/// loaded external document. The doc must already live in
/// `ctx.doc_roots` / `ctx.doc_refs` / `ctx.doc_prefix` (call
/// [`ensure_doc_registered`] first).
pub(crate) fn walk_resolved_schema_at_pointer(
    ctx: &mut Ctx,
    canonical: &std::path::Path,
    fragment: &str,
    raw_for_diag: &str,
    ptr: &mut Ptr,
    hint: &NameHint,
) -> Option<TypeRef> {
    let Some(schema_name) = crate::external::fragment_last_token(fragment) else {
        ctx.push_diag(diag::err(
            diag::E_EXTERNAL_REF,
            format!("$ref `{raw_for_diag}` has an empty fragment"),
            ptr.loc(ctx.file),
        ));
        return None;
    };
    let fragment_string = fragment.to_string();
    let canonical = canonical.to_path_buf();
    let dedup_key = (canonical.clone(), fragment_string.clone());

    // 3. Decide on the target id. The dedup map carries a pre-registered
    //    or previously-walked id for this `(canonical, fragment)`:
    //      - The spec's `components.schemas.Pet = { $ref: ext.json#/Pet }`
    //        seeds it with `Pet` *before* sibling components are walked,
    //        so a later inline ref to the same target reuses `Pet`
    //        instead of synthesising a fresh `<docprefix>Pet`.
    //      - On first walk we register the id we just minted, so a
    //        third resolution under yet another hint hits the cache.
    //    When the dedup map has nothing, fall back to the hint: `Named`
    //    uses the caller's id verbatim, `Inline` uses the doc-prefix
    //    scheme.
    let target_id = match ctx.external_ref_to_id.get(&dedup_key).cloned() {
        Some(existing) => existing,
        None => match hint {
            NameHint::Named(s) => crate::sanitize::ident(s),
            NameHint::Inline { .. } => {
                let prefix = ctx.doc_prefix.get(&canonical).cloned().unwrap_or_default();
                format!("{prefix}{}", crate::sanitize::ident(&schema_name))
            }
        },
    };

    // 4. Cycle / already-walked detection. Re-entry returns the id
    //    immediately; finalize handles the cycle as recursion (for
    //    schema graphs).
    let walking_key = (canonical.clone(), fragment_string.clone());
    if ctx.types.contains_key(&target_id) || ctx.walking.contains(&walking_key) {
        return Some(target_id);
    }

    // 5. Resolve the JSON pointer to the inline schema value, using the
    //    cached doc root.
    let Some(root) = ctx.doc_roots.get(&canonical) else {
        ctx.push_diag(diag::err(
            diag::E_EXTERNAL_REF,
            format!("internal: doc `{}` not in cache", canonical.display()),
            ptr.loc(ctx.file),
        ));
        return None;
    };
    let Some(schema_value) = crate::external::resolve_pointer(root, fragment).cloned() else {
        ctx.push_diag(diag::err(
            diag::E_DANGLING_REF,
            format!(
                "$ref `{raw_for_diag}` could not be resolved against `{}`",
                canonical.display()
            ),
            ptr.loc(ctx.file),
        ));
        return None;
    };

    let prev_doc = std::mem::replace(&mut ctx.current_doc, canonical.clone());
    ctx.walking.insert(walking_key.clone());
    let mut child_ptr = Ptr::new();
    let walked = walk_with_pointer_tokens(&mut child_ptr, fragment, |p| {
        parse_schema(ctx, &schema_value, p, NameHint::Named(target_id.clone()))
    });
    ctx.walking.remove(&walking_key);
    ctx.current_doc = prev_doc;
    // Pretty up the `original_name` so TS generators emit `interface Pet`
    // not `interface Types__Pet`. The id stays prefixed so the type pool
    // remains globally unique.
    if let Some(ref id) = walked {
        if let Some(nt) = ctx.types.get_mut(id) {
            nt.original_name = Some(schema_name);
        }
        // Cache the canonical id for this `(canonical, fragment)` pair so
        // a later ref to the same schema under a different hint reuses
        // it instead of producing a duplicate type.
        ctx.external_ref_to_id.insert(dedup_key, id.clone());
    }
    walked.or(Some(target_id))
}

/// Render a resolver error as a portable user-facing string. Hides
/// machine-local paths so the wording is reproducible across CI envs.
pub(crate) fn resolver_error_message(raw: &str, e: &crate::external::ResolverError) -> String {
    use crate::external::ResolverError as RE;
    match e {
        RE::NotConfigured { .. } => format!(
            "external $ref `{raw}` requires a file-based resolver; \
             call `parse_path` instead of `parse_str`"
        ),
        RE::UrlNotSupported { .. } => {
            format!("URL $ref `{raw}` is not yet supported (file-relative refs only)")
        }
        RE::EscapesRoot { .. } => {
            format!("external $ref `{raw}` resolves outside the input file's directory")
        }
        RE::Io { .. } => format!("external $ref `{raw}` could not be read from disk"),
        RE::InvalidJson { .. } => {
            format!("external $ref `{raw}` points at a file that is not valid JSON")
        }
    }
}

/// Make sure `canonical` has a doc-prefix and a `RefIndex`. Idempotent;
/// repeated calls for the same doc are no-ops.
///
/// Pre-registered names come from two places:
/// 1. `components.schemas.<X>` keys (the standard wrapped shape).
/// 2. Root-level object keys when the file has no `components` wrapper
///    (the flat-root shape used by split-document specs — `Pet` lives
///    directly under the file's root).
///
/// Pre-registration only enables `refs::resolve` to find the *names*;
/// the actual schema gets walked on demand by `walk_external_ref`.
pub(crate) fn ensure_doc_registered(
    ctx: &mut Ctx,
    canonical: &std::path::Path,
    root: &std::sync::Arc<serde_json::Value>,
) {
    // The root cache is keyed independently of `doc_prefix` so the main
    // spec (which has an empty prefix pre-registered by `Ctx::with_resolver`)
    // still gets its root cached on first contact via the resolver. This
    // matters when an external doc refs back into the main spec.
    ctx.doc_roots
        .entry(canonical.to_path_buf())
        .or_insert_with(|| std::sync::Arc::clone(root));
    if ctx.doc_prefix.contains_key(canonical) {
        return;
    }
    let prefix = build_doc_prefix(ctx, canonical);
    ctx.doc_prefix.insert(canonical.to_path_buf(), prefix);
    let mut idx = crate::refs::RefIndex::default();
    let mut registered_anything = false;
    if let Some(serde_json::Value::Object(schemas)) =
        root.get("components").and_then(|c| c.get("schemas"))
    {
        for name in schemas.keys() {
            idx.register(crate::sanitize::ident(name));
            registered_anything = true;
        }
    }
    if !registered_anything {
        if let serde_json::Value::Object(map) = root.as_ref() {
            for name in map.keys() {
                idx.register(crate::sanitize::ident(name));
            }
        }
    }
    ctx.doc_refs.insert(canonical.to_path_buf(), idx);
}

/// Push each token of a JSON pointer fragment onto a fresh `Ptr`, then
/// run `body`. The pointer trail makes diagnostic locations point at
/// the right place inside the resolved doc.
pub(crate) fn walk_with_pointer_tokens<F, R>(ptr: &mut Ptr, fragment: &str, body: F) -> R
where
    F: FnOnce(&mut Ptr) -> R,
{
    let trimmed = fragment.strip_prefix('/').unwrap_or(fragment);
    let tokens: Vec<String> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed
            .split('/')
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .collect()
    };
    fn step<F, R>(ptr: &mut Ptr, tokens: &[String], body: F) -> R
    where
        F: FnOnce(&mut Ptr) -> R,
    {
        match tokens.split_first() {
            None => body(ptr),
            Some((head, rest)) => ptr.with_token(head, |ptr| step(ptr, rest, body)),
        }
    }
    step(ptr, &tokens, body)
}

/// Sanitised file-stem prefix for a loaded external doc. Used to build
/// globally-unique type ids so two docs can each declare a `Pet` without
/// colliding in the type pool.
fn build_doc_prefix(ctx: &Ctx, canonical: &std::path::Path) -> String {
    let stem = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ext");
    let base = crate::sanitize::ident(stem);
    let mut candidate = format!("{base}__");
    let mut counter = 2u32;
    while ctx.doc_prefix.values().any(|p| p == &candidate) {
        candidate = format!("{base}{counter}__");
        counter += 1;
    }
    candidate
}

fn parse_primitive(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    ty: &str,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let format = map.get("format").and_then(J::as_str);
    let (kind, format_extension) = primitive_kind(ctx, ptr, ty, format)?;
    let mut constraints = primitive_constraints(ctx, map);
    constraints.format_extension = format_extension;

    // Allocate the owning id up-front so `contentSchema`'s name hint
    // can derive from it. `alloc_id` is idempotent for `Named` hints
    // and bumps a unique counter for `Inline` hints; calling it again
    // below in the NamedType ctor would double-bump for inline owners.
    let id = alloc_id(ctx, &hint);

    // OAS 3.2 / JSON Schema 2020-12 `contentSchema` — schema for the
    // decoded payload of a `contentEncoding`'d string. The shallow
    // `primitive_constraints` accessor leaves the slot None; populate
    // it here where we have access to the NameHint.
    if let Some(cs) = map.get("contentSchema") {
        let cs_ref = ptr.with_token("contentSchema", |ptr| {
            parse_schema(ctx, cs, ptr, NameHint::inline(&id, "content_schema"))
        });
        if cs_ref.is_some() {
            constraints.content_schema = cs_ref;
        }
    }

    let prim = PrimitiveType { kind, constraints };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id,
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Primitive(prim),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

/// Returns the IR `PrimitiveKind` (the JSON Schema `type` value) plus
/// the original `format` string preserved verbatim. Format
/// refinements (`int32` / `int64` / `float` / `double` / `date` /
/// `date-time` / `uuid` / `byte` / `binary` / `email` / `password` /
/// `decimal` / `iban` / etc.) all land on
/// `PrimitiveConstraints.format_extension`. Plugins decide whether to
/// produce a richer target-language type based on the format. The IR
/// itself stays uniform — the `type` keyword only.
///
/// Unknown string formats no longer warn (#105 reframed): every
/// non-`type` refinement is treated the same way, so there's nothing
/// to be "unknown" about. The parser's job is verbatim capture, not
/// curating a registry of known formats.
fn primitive_kind(
    _ctx: &mut Ctx,
    ptr: &mut Ptr,
    ty: &str,
    format: Option<&str>,
) -> Option<(PrimitiveKind, Option<String>)> {
    use PrimitiveKind as P;
    let result = match ty {
        "string" => (P::String, format.map(String::from)),
        "integer" => (P::Integer, format.map(String::from)),
        "number" => (P::Number, format.map(String::from)),
        "boolean" => (P::Bool, format.map(String::from)),
        _ => {
            _ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                format!("unsupported `type` value: `{ty}`"),
                ptr.loc(_ctx.file),
            ));
            return None;
        }
    };
    Some(result)
}

fn primitive_constraints(ctx: &mut Ctx, map: &serde_json::Map<String, J>) -> PrimitiveConstraints {
    let (minimum, exclusive_minimum) =
        normalise_exclusive_bound(ctx, map.get("minimum"), map.get("exclusiveMinimum"));
    let (maximum, exclusive_maximum) =
        normalise_exclusive_bound(ctx, map.get("maximum"), map.get("exclusiveMaximum"));
    PrimitiveConstraints {
        minimum,
        maximum,
        exclusive_minimum,
        exclusive_maximum,
        multiple_of: map.get("multipleOf").map(|v| ctx.values.intern_json(v)),
        min_length: map.get("minLength").and_then(J::as_u64),
        max_length: map.get("maxLength").and_then(J::as_u64),
        pattern: map.get("pattern").and_then(J::as_str).map(String::from),
        format_extension: None,
        content_encoding: map
            .get("contentEncoding")
            .and_then(J::as_str)
            .map(String::from),
        content_media_type: map
            .get("contentMediaType")
            .and_then(J::as_str)
            .map(String::from),
        // `content_schema` is populated separately because it requires
        // walking the sub-schema with a `NameHint` derived from the
        // owner — `primitive_constraints` is a shallow accessor.
        content_schema: None,
    }
}

/// Reconcile the OAS 3.0 form (`minimum: N` + `exclusiveMinimum: bool`)
/// and the OAS 3.1 form (`exclusiveMinimum: N`) into a single IR shape:
/// `(bound_ref, exclusive_flag_ref)` where `exclusive_flag_ref` points
/// at `Bool(true)` for an exclusive bound and `None` otherwise.
fn normalise_exclusive_bound(
    ctx: &mut Ctx,
    inclusive: Option<&J>,
    exclusive: Option<&J>,
) -> (Option<forge_ir::ValueRef>, Option<forge_ir::ValueRef>) {
    let inclusive_ref = inclusive.map(|v| ctx.values.intern_json(v));
    match exclusive {
        // 3.0 form: a boolean flag accompanying `minimum` / `maximum`.
        Some(J::Bool(true)) => {
            let flag = ctx.values.intern(forge_ir::Value::Bool { value: true });
            (inclusive_ref, Some(flag))
        }
        Some(J::Bool(false)) | None => (inclusive_ref, None),
        // 3.1 form: the bound itself. Promote it to the inclusive slot
        // and flag the constraint as exclusive.
        Some(num @ J::Number(_)) => {
            let promoted = ctx.values.intern_json(num);
            let inclusive = inclusive_ref.or(Some(promoted));
            let flag = ctx.values.intern(forge_ir::Value::Bool { value: true });
            (inclusive, Some(flag))
        }
        // Anything else (string, object, ...) ignored — the spec is malformed.
        Some(_) => (inclusive_ref, None),
    }
}

fn parse_array(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let id = alloc_id(ctx, &hint);
    let items_ref = match map.get("items") {
        Some(v) => ptr.with_token("items", |ptr| {
            parse_schema(ctx, v, ptr, NameHint::inline(&id, "items"))
        })?,
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "array schema missing `items`",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    // Per-element nullability is encoded in the items TypeRef itself: a
    // nullable element is a `Union(T, Null)` reference, and the array's
    // own outer wrap (if any) is applied via `maybe_wrap_nullable`. See
    // issue #107.
    let constraints = ArrayConstraints {
        min_items: map.get("minItems").and_then(J::as_u64),
        max_items: map.get("maxItems").and_then(J::as_u64),
        unique_items: map.get("uniqueItems").and_then(J::as_bool).unwrap_or(false),
    };
    let arr = ArrayType {
        items: items_ref,
        constraints,
    };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id,
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Array(arr),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

fn parse_object(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let id = alloc_id(ctx, &hint);
    let mut properties: Vec<Property> = Vec::new();
    if let Some(J::Object(props)) = map.get("properties") {
        ptr.with_token("properties", |ptr| {
            for (name, schema) in props {
                ptr.with_token(name, |ptr| {
                    let role = format!("property_{}", crate::sanitize::ident(name));
                    if let Some(t) = parse_schema(ctx, schema, ptr, NameHint::inline(&id, &role)) {
                        let (
                            prop_title,
                            prop_desc,
                            prop_dep,
                            prop_ro,
                            prop_wo,
                            prop_extdocs,
                            prop_default,
                            prop_examples,
                        ) = match schema {
                            J::Object(m) => (
                                title(m),
                                description(m),
                                deprecated(m),
                                m.get("readOnly").and_then(J::as_bool).unwrap_or(false),
                                m.get("writeOnly").and_then(J::as_bool).unwrap_or(false),
                                crate::parse_external_docs(ctx, m.get("externalDocs"), ptr),
                                crate::parse_default(ctx, m, ptr, "property"),
                                crate::parse_examples(ctx, m, ptr),
                            ),
                            _ => (None, None, false, false, false, None, None, vec![]),
                        };
                        let extensions = match schema {
                            J::Object(m) => crate::operations::collect_extensions(ctx, m, ptr),
                            _ => Vec::new(),
                        };
                        properties.push(Property {
                            name: name.clone(),
                            r#type: t,
                            required: false, // patched below from parent's `required` array
                            title: prop_title,
                            description: prop_desc,
                            deprecated: prop_dep,
                            read_only: prop_ro,
                            write_only: prop_wo,
                            external_docs: prop_extdocs,
                            default: prop_default,
                            examples: prop_examples,
                            extensions,
                        });
                    }
                });
            }
        });
    }
    let required_names: std::collections::HashSet<String> = match map.get("required") {
        Some(J::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => std::collections::HashSet::new(),
    };
    for p in properties.iter_mut() {
        if required_names.contains(&p.name) {
            p.required = true;
        }
    }
    let pattern_properties = parse_pattern_properties(ctx, map, ptr, &id);
    let additional = match map.get("additionalProperties") {
        Some(J::Bool(false)) => AdditionalProperties::Forbidden,
        Some(J::Bool(true)) | None => AdditionalProperties::Any,
        Some(J::Object(_)) => ptr
            .with_token("additionalProperties", |ptr| {
                parse_schema(
                    ctx,
                    map.get("additionalProperties").unwrap(),
                    ptr,
                    NameHint::inline(&id, "additional_properties"),
                )
            })
            .map(|t| AdditionalProperties::Typed { r#type: t })
            .unwrap_or(AdditionalProperties::Any),
        Some(_) => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "`additionalProperties` must be a boolean or schema object",
                ptr.loc(ctx.file),
            ));
            AdditionalProperties::Any
        }
    };
    let property_names = parse_property_names(ctx, map, ptr, &id);
    let constraints = ObjectConstraints {
        min_properties: map.get("minProperties").and_then(J::as_u64),
        max_properties: map.get("maxProperties").and_then(J::as_u64),
    };
    let obj = ObjectType {
        properties,
        pattern_properties,
        additional_properties: additional,
        property_names,
        constraints,
    };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id,
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Object(obj),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

/// Parse JSON Schema `patternProperties` (2020-12 §10.3.2.2) into IR
/// entries. The keyword's value is an object whose keys are ECMA-262
/// regexes and whose values are the schemas that property *values* under
/// a matching name must satisfy. Inline value schemas are lifted under
/// `<owner>_pattern_property_<sanitized-regex>`. A non-object keyword
/// value carries no entries — same as omitting the keyword — so we return
/// an empty vec rather than erroring.
pub(crate) fn parse_pattern_properties(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    owner_id: &str,
) -> Vec<PatternProperty> {
    let mut out = Vec::new();
    if let Some(J::Object(pats)) = map.get("patternProperties") {
        ptr.with_token("patternProperties", |ptr| {
            for (pattern, schema) in pats {
                ptr.with_token(pattern, |ptr| {
                    let role = format!("pattern_property_{}", crate::sanitize::ident(pattern));
                    if let Some(t) =
                        parse_schema(ctx, schema, ptr, NameHint::inline(owner_id, &role))
                    {
                        out.push(PatternProperty {
                            pattern: pattern.clone(),
                            r#type: t,
                        });
                    }
                });
            }
        });
    }
    out
}

/// Parse JSON Schema `propertyNames` (2020-12 §10.3.2.4) into a `TypeRef`
/// the property *names* must satisfy. Returns `None` when the keyword is
/// absent or is a boolean schema (`true` is the no-op; `false`/other
/// boolean shapes carry no string constraint the IR can surface).
///
/// Property names are always strings, and the canonical `propertyNames`
/// schema omits `type` — e.g. `{ "pattern": "^[A-Za-z_]\w*$" }`. So a
/// typeless, non-`$ref` schema is defaulted to `type: "string"` before
/// walking, ensuring `pattern` / `minLength` / `maxLength` / `format`
/// land on a string primitive instead of being dropped into a freeform
/// `any`.
pub(crate) fn parse_property_names(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    owner_id: &str,
) -> Option<TypeRef> {
    let J::Object(pn) = map.get("propertyNames")? else {
        return None;
    };
    let effective: J = if pn.contains_key("type") || pn.contains_key("$ref") {
        J::Object(pn.clone())
    } else {
        let mut m = pn.clone();
        m.insert("type".to_string(), J::String("string".to_string()));
        J::Object(m)
    };
    ptr.with_token("propertyNames", |ptr| {
        parse_schema(
            ctx,
            &effective,
            ptr,
            NameHint::inline(owner_id, "property_names"),
        )
    })
}

fn parse_string_enum(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    raw_values: &[J],
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let mut values: Vec<EnumStringValue> = Vec::new();
    let mut nullable = nullable;
    ptr.with_token("enum", |ptr| {
        for (i, v) in raw_values.iter().enumerate() {
            ptr.with_index(i, |ptr| match v {
                J::String(s) => values.push(EnumStringValue { value: s.clone() }),
                J::Null => {
                    // A literal `null` member is the OpenAPI 3.0 idiom for
                    // a nullable enum (alongside `nullable: true`). Treat
                    // both forms as the same axis.
                    nullable = true;
                }
                other => {
                    ctx.push_diag(diag::warn(
                        diag::W_ENUM_VALUE_DROPPED,
                        format!(
                            "string enum value `{}` is not a string; dropped",
                            short_json(other)
                        ),
                        ptr.loc(ctx.file),
                    ));
                }
            });
        }
    });
    if values.is_empty() {
        ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            "string enum has no usable values",
            ptr.loc(ctx.file),
        ));
        return None;
    }
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: alloc_id(ctx, &hint),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::EnumString(EnumStringType { values }),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

fn parse_int_enum(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    raw_values: &[J],
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let kind = match map.get("format").and_then(J::as_str) {
        Some("int64") => IntKind::Int64,
        _ => IntKind::Int32,
    };
    let mut values: Vec<EnumIntValue> = Vec::new();
    let mut nullable = nullable;
    ptr.with_token("enum", |ptr| {
        for (i, v) in raw_values.iter().enumerate() {
            ptr.with_index(i, |ptr| match v {
                J::Number(n) => match n.as_i64() {
                    Some(value) => values.push(EnumIntValue { value }),
                    None => ctx.push_diag(diag::warn(
                        diag::W_ENUM_VALUE_DROPPED,
                        format!("integer enum value `{n}` is not an i64; dropped"),
                        ptr.loc(ctx.file),
                    )),
                },
                J::Null => nullable = true,
                other => {
                    ctx.push_diag(diag::warn(
                        diag::W_ENUM_VALUE_DROPPED,
                        format!(
                            "integer enum value `{}` is not a number; dropped",
                            short_json(other)
                        ),
                        ptr.loc(ctx.file),
                    ));
                }
            });
        }
    });
    if values.is_empty() {
        ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            "integer enum has no usable values",
            ptr.loc(ctx.file),
        ));
        return None;
    }
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: alloc_id(ctx, &hint),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::EnumInt(EnumIntType { values, kind }),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

/// Lift each entry of a composition array (`oneOf` / `anyOf`) into the
/// type pool and return the variants alongside their `TypeRef`s. Used by
/// both the discriminated and untagged union builders.
fn lift_union_variants(
    ctx: &mut Ctx,
    parts: &[J],
    ptr: &mut Ptr,
    owner_id: &str,
    composition_key: &str,
) -> (Vec<UnionVariant>, Vec<TypeRef>) {
    let mut variants: Vec<UnionVariant> = Vec::new();
    let mut variant_refs: Vec<TypeRef> = Vec::new();
    ptr.with_token(composition_key, |ptr| {
        for (i, sub) in parts.iter().enumerate() {
            ptr.with_index(i, |ptr| {
                let role = format!("variant_{i}");
                if let Some(t) = parse_schema(ctx, sub, ptr, NameHint::inline(owner_id, &role)) {
                    variant_refs.push(t.clone());
                    variants.push(UnionVariant {
                        r#type: t,
                        tag: None,
                    });
                }
            });
        }
    });
    (variants, variant_refs)
}

/// Build an untagged union from `oneOf` or `anyOf` parts.
fn parse_untagged_union(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
    composition_key: &str,
    kind: UnionKind,
) -> Option<TypeRef> {
    let id = alloc_id(ctx, &hint);
    let parts = match map.get(composition_key) {
        Some(J::Array(items)) if !items.is_empty() => items.clone(),
        _ => {
            ptr.with_token(composition_key, |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_INVALID_TYPE,
                    format!("`{composition_key}` must be a non-empty array"),
                    ptr.loc(ctx.file),
                ));
            });
            return None;
        }
    };
    let (mut variants, _refs) = lift_union_variants(ctx, &parts, ptr, &id, composition_key);
    if variants.is_empty() {
        return None;
    }
    if nullable {
        let null_id = ensure_null_singleton(ctx);
        // Canonical position: Null is always the last variant. Issue #107.
        variants.push(UnionVariant {
            r#type: null_id,
            tag: None,
        });
    }
    let union = UnionType {
        variants,
        discriminator: None,
        kind,
    };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: id.clone(),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Union(union),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    ctx.push_type(nt);
    Some(id)
}

/// Build an untagged union from a 3.1 multi-type array. Synthesises one
/// inline schema per non-null type (`{ "type": <T> }`) and lifts them via
/// the same machinery that handles `oneOf`/`anyOf`.
fn parse_type_array_union(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
    types: &[String],
) -> Option<TypeRef> {
    let id = alloc_id(ctx, &hint);
    let parts: Vec<J> = types
        .iter()
        .map(|t| {
            let mut m = serde_json::Map::new();
            m.insert("type".to_string(), J::String(t.clone()));
            J::Object(m)
        })
        .collect();
    let (mut variants, _refs) = lift_union_variants(ctx, &parts, ptr, &id, "type");
    if variants.is_empty() {
        return None;
    }
    if nullable {
        let null_id = ensure_null_singleton(ctx);
        // Canonical position: Null is always the last variant. Issue #107.
        variants.push(UnionVariant {
            r#type: null_id,
            tag: None,
        });
    }
    let union = UnionType {
        variants,
        discriminator: None,
        kind: UnionKind::OneOf,
    };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: id.clone(),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Union(union),
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    ctx.push_type(nt);
    Some(id)
}

/// 3.1 `const` keyword. Single literal value of any scalar type. We fold
/// it into the matching single-value enum so the existing IR shapes cover
/// it without a new `ConstType`: strings → `EnumString`, integers →
/// `EnumInt`, non-integer numbers → `EnumNumber`, booleans → `EnumBool`.
/// `const: null` resolves to a `TypeDef::Null` node (named, when the user
/// gave it a name; otherwise the canonical singleton). Array- and
/// object-typed `const` are not yet supported. See issue #107.
fn parse_const(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    c: &J,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    use forge_ir::{
        EnumBoolType, EnumBoolValue, EnumIntType, EnumIntValue, EnumNumberType, EnumNumberValue,
        EnumStringType, EnumStringValue, IntKind, NumberKind,
    };
    let nt_definition = match c {
        J::String(s) => TypeDef::EnumString(EnumStringType {
            values: vec![EnumStringValue { value: s.clone() }],
        }),
        J::Bool(b) => TypeDef::EnumBool(EnumBoolType {
            values: vec![EnumBoolValue { value: *b }],
        }),
        J::Number(n) => {
            // Integers stay on `EnumInt`; any other number (a genuine
            // fraction, or a magnitude beyond i64) becomes an `EnumNumber`.
            if let Some(int) = n.as_i64() {
                let kind = match map.get("format").and_then(J::as_str) {
                    Some("int64") => IntKind::Int64,
                    _ => IntKind::Int32,
                };
                TypeDef::EnumInt(EnumIntType {
                    values: vec![EnumIntValue { value: int }],
                    kind,
                })
            } else if let Some(float) = n.as_f64() {
                let kind = match map.get("format").and_then(J::as_str) {
                    Some("float") => NumberKind::Float,
                    _ => NumberKind::Double,
                };
                TypeDef::EnumNumber(EnumNumberType {
                    values: vec![EnumNumberValue { value: float }],
                    kind,
                })
            } else {
                ctx.push_diag(diag::err(
                    diag::E_INVALID_TYPE,
                    format!("`const: {n}` is not representable as a finite number"),
                    ptr.loc(ctx.file),
                ));
                return None;
            }
        }
        J::Null => {
            // `const: null` means the value is exactly null. For inline
            // hints, alias to the canonical singleton. For named hints,
            // emit the Null node under the user's id so generators that
            // emit `type Foo = ...` keep producing `Foo`.
            return Some(match &hint {
                NameHint::Inline { .. } => ensure_null_singleton(ctx),
                NameHint::Named(_) => {
                    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
                    let nt = NamedType {
                        id: alloc_id(ctx, &hint),
                        original_name: original_name(&hint),
                        title: title(map),
                        description: description(map),
                        deprecated: deprecated(map),
                        read_only: read_write_only(map).0,
                        write_only: read_write_only(map).1,
                        external_docs: crate::parse_external_docs(
                            ctx,
                            map.get("externalDocs"),
                            ptr,
                        ),
                        default: crate::parse_default(ctx, map, ptr, "schema"),
                        examples: crate::parse_examples(ctx, map, ptr),
                        xml: crate::parse_xml(ctx, map, ptr),
                        definition: TypeDef::Null,
                        extensions,
                        location: Some(ptr.loc(ctx.file)),
                    };
                    let id = nt.id.clone();
                    ctx.push_type(nt);
                    id
                }
            });
        }
        other => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                format!(
                    "`const` value `{}` is not a scalar; string, number, boolean, and null \
                     `const` values are supported (array and object are not)",
                    serde_json::to_string(other).unwrap_or_default()
                ),
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: alloc_id(ctx, &hint),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: nt_definition,
        extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    Some(maybe_wrap_nullable(ctx, nt, nullable))
}

fn parse_oneof_discriminated(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
    hint: NameHint,
    nullable: bool,
) -> Option<TypeRef> {
    let id = alloc_id(ctx, &hint);
    let one_of = match map.get("oneOf") {
        Some(J::Array(items)) if !items.is_empty() => items.clone(),
        _ => {
            ptr.with_token("oneOf", |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_INVALID_TYPE,
                    "`oneOf` must be a non-empty array",
                    ptr.loc(ctx.file),
                ));
            });
            return None;
        }
    };

    let (mut variants, variant_refs) = lift_union_variants(ctx, &one_of, ptr, &id, "oneOf");
    if variants.is_empty() {
        return None;
    }

    let disc_obj = match map.get("discriminator") {
        Some(J::Object(o)) => o,
        _ => {
            ptr.with_token("discriminator", |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_INVALID_TYPE,
                    "discriminator must be an object",
                    ptr.loc(ctx.file),
                ));
            });
            return None;
        }
    };
    let property_name = match disc_obj.get("propertyName").and_then(J::as_str) {
        Some(p) => p.to_string(),
        None => {
            ptr.with_token("discriminator", |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_MISSING_FIELD,
                    "discriminator is missing `propertyName`",
                    ptr.loc(ctx.file),
                ));
            });
            return None;
        }
    };

    // Collect explicit mapping. Drop entries whose target isn't one of our
    // variants (warn, don't fail).
    let mut explicit: Vec<(String, TypeRef)> = Vec::new();
    if let Some(J::Object(m)) = disc_obj.get("mapping") {
        ptr.with_token("discriminator", |ptr| {
            ptr.with_token("mapping", |ptr| {
                for (tag, raw_target) in m {
                    let Some(s) = raw_target.as_str() else {
                        continue;
                    };
                    let id = ref_target_to_id(s);
                    if variant_refs.iter().any(|v| v == &id) {
                        explicit.push((tag.clone(), id));
                    } else {
                        ptr.with_token(tag, |ptr| {
                            ctx.push_diag(diag::warn(
                                diag::W_DISCRIMINATOR_MAPPING_DANGLING,
                                format!(
                                    "discriminator mapping `{tag}` -> `{s}` does not match any oneOf variant; dropping"
                                ),
                                ptr.loc(ctx.file),
                            ));
                        });
                    }
                }
            });
        });
    }

    // Build the canonical mapping: explicit entries first (declaration
    // order), then synthesize entries for variants without an explicit tag
    // using the variant's short id as the tag.
    let mut mapping: Vec<(String, TypeRef)> = explicit.clone();
    for vref in &variant_refs {
        if !mapping.iter().any(|(_, t)| t == vref) {
            mapping.push((vref.clone(), vref.clone()));
        }
    }
    // Tag each variant with the *first* mapping entry that targets it.
    for v in variants.iter_mut() {
        if let Some((tag, _)) = mapping.iter().find(|(_, t)| t == &v.r#type) {
            v.tag = Some(tag.clone());
        }
    }

    // x-* extensions declared on the discriminator object itself. Same
    // scalar-only policy as everywhere else in the IR.
    let extensions = ptr.with_token("discriminator", |ptr| {
        crate::operations::collect_extensions(ctx, disc_obj, ptr)
    });

    if nullable {
        let null_id = ensure_null_singleton(ctx);
        // Canonical position: Null is always the last variant. The
        // discriminator mapping was built from `variant_refs` above and
        // therefore does not tag the Null variant — nullability is
        // structural, not discriminated. Issue #107.
        variants.push(UnionVariant {
            r#type: null_id,
            tag: None,
        });
    }

    let union = UnionType {
        variants,
        discriminator: Some(Discriminator {
            property_name,
            mapping,
            extensions,
        }),
        kind: UnionKind::OneOf,
    };
    let outer_extensions = crate::operations::collect_extensions(ctx, map, ptr);
    let nt = NamedType {
        id: id.clone(),
        original_name: original_name(&hint),
        title: title(map),
        description: description(map),
        deprecated: deprecated(map),
        read_only: read_write_only(map).0,
        write_only: read_write_only(map).1,
        external_docs: crate::parse_external_docs(ctx, map.get("externalDocs"), ptr),
        default: crate::parse_default(ctx, map, ptr, "schema"),
        examples: crate::parse_examples(ctx, map, ptr),
        xml: crate::parse_xml(ctx, map, ptr),
        definition: TypeDef::Union(union),
        extensions: outer_extensions,
        location: Some(ptr.loc(ctx.file)),
    };
    ctx.push_type(nt);
    Some(id)
}

/// `#/components/schemas/Foo` or bare `Foo` → sanitized id.
fn ref_target_to_id(raw: &str) -> String {
    let name = raw
        .strip_prefix("#/components/schemas/")
        .unwrap_or(raw)
        .trim_start_matches('/');
    crate::sanitize::ident(name)
}

pub(crate) fn alloc_id(ctx: &mut Ctx, hint: &NameHint) -> String {
    match hint {
        NameHint::Named(s) => {
            let id = crate::sanitize::ident(s);
            // Reserve the literal "null" id for the canonical Null singleton.
            // A user-declared component named `null` is renamed by collision-
            // bumping; preserves IR uniqueness without losing the schema.
            if id == NULL_ID {
                ensure_null_singleton(ctx);
                let bumped = ctx.unique_id(&id);
                ctx.push_diag(diag::warn(
                    diag::W_RESERVED_NAME,
                    format!("schema id `{id}` is reserved for the Null type singleton; renamed to `{bumped}`"),
                    forge_ir::SpecLocation::new(""),
                ));
                return bumped;
            }
            id
        }
        NameHint::Inline { .. } => {
            let base = hint.base();
            ctx.unique_id(&base)
        }
    }
}

/// Ensure the canonical [`TypeDef::Null`] singleton lives in the type pool
/// under [`NULL_ID`]. Idempotent. Returns the canonical TypeRef.
pub(crate) fn ensure_null_singleton(ctx: &mut Ctx) -> TypeRef {
    if !ctx.types.contains_key(NULL_ID) {
        ctx.push_type(NamedType {
            id: NULL_ID.to_string(),
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
            definition: TypeDef::Null,
            extensions: vec![],
            location: None,
        });
    }
    NULL_ID.to_string()
}

/// Lift `nt` into the type pool, optionally wrapping it as a `Union(_, Null)`
/// when `nullable` is true. Preserves the user-visible id on the outer Union;
/// the inner non-null shape lives under `<id>_nonnull` (collision-bumped).
/// Variant order is canonicalised: non-null first, Null last. See issue #107.
pub(crate) fn maybe_wrap_nullable(ctx: &mut Ctx, nt: NamedType, nullable: bool) -> TypeRef {
    if !nullable {
        let id = nt.id.clone();
        ctx.push_type(nt);
        return id;
    }
    let outer_id = nt.id.clone();
    let inner_id = ctx.unique_id(&format!("{outer_id}_nonnull"));
    let inner = NamedType {
        id: inner_id.clone(),
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
        definition: nt.definition,
        extensions: vec![],
        location: nt.location.clone(),
    };
    ctx.push_type(inner);
    let null_id = ensure_null_singleton(ctx);
    let union = TypeDef::Union(UnionType {
        variants: vec![
            UnionVariant {
                r#type: inner_id,
                tag: None,
            },
            UnionVariant {
                r#type: null_id,
                tag: None,
            },
        ],
        discriminator: None,
        kind: UnionKind::OneOf,
    });
    let outer = NamedType {
        id: outer_id.clone(),
        original_name: nt.original_name,
        title: nt.title,
        description: nt.description,
        deprecated: nt.deprecated,
        read_only: nt.read_only,
        write_only: nt.write_only,
        external_docs: nt.external_docs,
        default: nt.default,
        examples: nt.examples,
        xml: nt.xml,
        definition: union,
        extensions: nt.extensions,
        location: nt.location,
    };
    ctx.push_type(outer);
    outer_id
}

pub(crate) fn original_name(hint: &NameHint) -> Option<String> {
    match hint {
        NameHint::Named(s) => Some(s.clone()),
        NameHint::Inline { .. } => None,
    }
}

pub(crate) fn description(map: &serde_json::Map<String, J>) -> Option<String> {
    map.get("description").and_then(J::as_str).map(String::from)
}

pub(crate) fn summary(map: &serde_json::Map<String, J>) -> Option<String> {
    map.get("summary").and_then(J::as_str).map(String::from)
}

pub(crate) fn deprecated(map: &serde_json::Map<String, J>) -> bool {
    map.get("deprecated").and_then(J::as_bool).unwrap_or(false)
}

/// JSON Schema `title` — short human label. Surfaced as
/// `NamedType.title` for doc generators / IDE hover.
pub(crate) fn title(map: &serde_json::Map<String, J>) -> Option<String> {
    map.get("title").and_then(J::as_str).map(String::from)
}

/// JSON Schema `readOnly` / `writeOnly` at the schema level. Defaults to
/// `false`. Mirrors the per-property fields on `Property`; the schema-level
/// pair propagates through `oneOf` variants and to top-level component
/// schemas that don't appear as properties anywhere.
pub(crate) fn read_write_only(map: &serde_json::Map<String, J>) -> (bool, bool) {
    let read_only = map.get("readOnly").and_then(J::as_bool).unwrap_or(false);
    let write_only = map.get("writeOnly").and_then(J::as_bool).unwrap_or(false);
    (read_only, write_only)
}

fn short_json(v: &J) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > 40 {
        // Find the largest valid UTF-8 boundary at or before position 40
        let mut end = 40;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    } else {
        s
    }
}
