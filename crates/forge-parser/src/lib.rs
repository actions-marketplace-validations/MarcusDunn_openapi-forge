//! OpenAPI 3.0 JSON → `forge_ir::Ir`.
//!
//! Stage 3 supports a deliberately narrow subset of OpenAPI 3.0.x. See
//! `docs/parser-coverage.md` for the precise list. Anything outside the
//! subset produces an `error`-severity diagnostic with a JSON-pointer
//! location — never silent best-effort output.
//!
//! # Public surface
//!
//! ```no_run
//! let json = std::fs::read_to_string("openapi.json").unwrap();
//! match forge_parser::parse_str(&json) {
//!     Ok(out) => {
//!         for d in &out.diagnostics { eprintln!("{}: {}", d.code, d.message); }
//!         if let Some(ir) = out.spec { println!("{} operations", ir.operations.len()); }
//!     }
//!     Err(e) => eprintln!("fatal: {e}"),
//! }
//! ```

#![forbid(unsafe_code)]

mod ctx;
mod diag;
pub mod external;
mod finalize;
mod normalize;
mod operations;
mod pointer;
mod ref_walk;
mod refs;
mod sanitize;
mod schema;
mod security;
mod value;

pub use external::{FileResolver, NoExternalResolver, Resolver, ResolverError};

use forge_ir::{
    ApiInfo, Callback, Contact, Diagnostic, Example, ExternalDocs, Ir, Link, Server,
    ServerVariable, SpecLocation, Tag, XmlObject,
};
use serde_json::Value as J;
use thiserror::Error;

use crate::ctx::Ctx;
use crate::pointer::Ptr;
use crate::schema::{parse_schema, NameHint};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("input is empty")]
    Empty,
    #[error("root document must be a JSON object")]
    NotObject,
    #[error("could not read input file `{path}`: {message}")]
    Io { path: String, message: String },
}

#[derive(Debug, Default)]
pub struct ParseOutput {
    pub spec: Option<Ir>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse an OpenAPI 3.0 JSON document.
pub fn parse_str(source: &str) -> Result<ParseOutput, ParseError> {
    parse_str_with_file(source, None)
}

/// Parse with a `file` label that gets attached to every `SpecLocation`.
pub fn parse_str_with_file(source: &str, file: Option<&str>) -> Result<ParseOutput, ParseError> {
    parse_with_resolver(
        source,
        file,
        Box::new(external::NoExternalResolver),
        ctx::synthetic_main_path(),
    )
}

/// Parse a spec from a filesystem path. External `$ref`s are resolved
/// relative to the spec file's parent directory; paths that escape that
/// root are rejected with `parser/E-EXTERNAL-REF`.
pub fn parse_path(path: &std::path::Path) -> Result<ParseOutput, ParseError> {
    let canonical = path.canonicalize().map_err(|e| ParseError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let source = std::fs::read_to_string(&canonical).map_err(|e| ParseError::Io {
        path: canonical.display().to_string(),
        message: e.to_string(),
    })?;
    let resolver = external::FileResolver::new(&canonical).map_err(|e| ParseError::Io {
        path: canonical.display().to_string(),
        message: e.to_string(),
    })?;
    // The label only goes into `SpecLocation.file`. Use the filename so
    // generated diagnostics stay portable across machines (callers who
    // need the absolute path can join it with a known root themselves).
    let label = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    parse_with_resolver(&source, Some(&label), Box::new(resolver), canonical)
}

fn parse_with_resolver(
    source: &str,
    file: Option<&str>,
    resolver: Box<dyn external::Resolver>,
    main_doc: std::path::PathBuf,
) -> Result<ParseOutput, ParseError> {
    if source.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let root: J =
        serde_json::from_str(source).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    let root_map = match &root {
        J::Object(m) => m,
        _ => return Err(ParseError::NotObject),
    };

    let mut ctx = Ctx::with_resolver(file, resolver, main_doc);
    // Cache the main spec's root so structural refs (`#/components/parameters/Page`)
    // can resolve without going back through the resolver.
    ctx.doc_roots
        .insert(ctx.current_doc.clone(), std::sync::Arc::new(root.clone()));
    let mut ptr = Ptr::new();

    // 1. Version check.
    if !check_version(&mut ctx, root_map, &mut ptr) {
        // Bail early but still emit ParseOutput with diagnostic.
        return Ok(ParseOutput {
            spec: None,
            diagnostics: ctx.diagnostics,
        });
    }

    // 2. Info / servers (best-effort; missing fields produce diagnostics).
    parse_info(&mut ctx, root_map, &mut ptr);
    parse_servers(&mut ctx, root_map, &mut ptr);
    let tags = parse_tags(&mut ctx, root_map, &mut ptr);

    // 3. Security schemes (parses what it can; oauth2 / openIdConnect emit
    //    deferred-feature diagnostics).
    security::walk_components(&mut ctx, root_map, &mut ptr);

    // 4. Components: pre-register schemas for forward $ref resolution, then
    //    walk each in dependency-aware order so allOf $refs resolve to
    //    already-walked targets.
    register_component_schemas(&mut ctx, root_map);
    walk_component_schemas(&mut ctx, root_map, &mut ptr);

    // 5. Top-level `security` (default for operations that don't override).
    if let Some(top_sec) = root_map.get("security") {
        ptr.with_token("security", |ptr| {
            ctx.default_security = security::parse_requirements(&mut ctx, top_sec, ptr);
        });
    }

    // 6. Paths.
    if let Some(paths) = root_map.get("paths") {
        ptr.with_token("paths", |ptr| {
            operations::parse_paths(&mut ctx, paths, ptr);
        });
    }

    // 6b. 3.1+ webhooks (top-level inbound operations). Generators that
    //     handle webhooks read `Ir.webhooks`; client generators ignore.
    if let Some(webhooks) = root_map.get("webhooks") {
        ptr.with_token("webhooks", |ptr| {
            operations::parse_webhooks(&mut ctx, webhooks, ptr);
        });
    }

    // 7. Root-level `externalDocs`. Reads here rather than in
    //    `parse_info` because OAS puts it on the root, not nested under
    //    `info`. Carried on `Ir.external_docs`; the rest of
    //    `Ir.docs` stays empty (root-level prose lives on `info.docs`).
    let root_external_docs = parse_external_docs(&mut ctx, root_map.get("externalDocs"), &mut ptr);

    // 7b. Surface unused `components.pathItems` declarations. They
    //     don't affect the IR (operations land via $ref from paths /
    //     webhooks / callbacks); a declared-and-never-used entry is
    //     almost certainly a spec bug.
    scan_unused_component_path_items(&mut ctx, root_map, &mut ptr);
    scan_unused_component_media_types(&mut ctx, root_map, &mut ptr);

    // 7c. Root-level annotations. Carried verbatim — generators that
    //     care can read them; most ignore them.
    let json_schema_dialect = root_map
        .get("jsonSchemaDialect")
        .and_then(J::as_str)
        .map(String::from);
    let self_url = root_map.get("$self").and_then(J::as_str).map(String::from);

    // 8. Build the IR and finalize (sort + topo).
    let mut ir = Ir {
        info: ctx.info.take().unwrap_or(ApiInfo {
            title: String::new(),
            version: String::new(),
            summary: None,
            description: None,
            terms_of_service: None,
            contact: None,
            license_name: None,
            license_url: None,
            license_identifier: None,
            extensions: vec![],
        }),
        operations: std::mem::take(&mut ctx.operations),
        types: ctx.types.values().cloned().collect::<Vec<_>>(),
        security_schemes: std::mem::take(&mut ctx.security_schemes),
        servers: std::mem::take(&mut ctx.servers),
        webhooks: std::mem::take(&mut ctx.webhooks),
        external_docs: root_external_docs,
        tags,
        json_schema_dialect,
        self_url,
        values: std::mem::take(&mut ctx.values).finish(),
    };
    let mut diagnostics = std::mem::take(&mut ctx.diagnostics);
    diagnostics.extend(finalize::canonicalize(&mut ir));

    Ok(ParseOutput {
        spec: Some(ir),
        diagnostics,
    })
}

/// Walk the top-level `tags: []` array into `Ir.tags` records. The
/// 3.2 `parent` / `kind` / `summary` fields are preserved; tags whose
/// `parent` doesn't reference a declared sibling drop the parent ref
/// with `parser/W-TAG-PARENT-DANGLING` so generators that render tag
/// trees don't see broken nesting.
fn parse_tags(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) -> Vec<Tag> {
    let Some(J::Array(tags)) = root.get("tags") else {
        return Vec::new();
    };
    let mut out: Vec<Tag> = Vec::new();
    let mut declared_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    ptr.with_token("tags", |ptr| {
        // Pass 1: collect declared names so the parent-ref check below
        // can validate references regardless of declaration order.
        for tag in tags.iter() {
            if let Some(name) = tag
                .as_object()
                .and_then(|m| m.get("name"))
                .and_then(J::as_str)
            {
                declared_names.insert(name.to_string());
            }
        }
        for (i, tag) in tags.iter().enumerate() {
            ptr.with_index(i, |ptr| {
                let Some(map) = tag.as_object() else {
                    ctx.push_diag(diag::err(
                        diag::E_INVALID_TYPE,
                        "tag must be an object",
                        ptr.loc(ctx.file),
                    ));
                    return;
                };
                let Some(name) = map.get("name").and_then(J::as_str) else {
                    ctx.push_diag(diag::err(
                        diag::E_MISSING_FIELD,
                        "tag is missing required `name`",
                        ptr.loc(ctx.file),
                    ));
                    return;
                };
                // OAS §4.22: Tag carries summary (3.2), description,
                // externalDocs.
                let summary = crate::schema::summary(map);
                let description = crate::schema::description(map);
                let external_docs = parse_external_docs(ctx, map.get("externalDocs"), ptr);
                let kind = map.get("kind").and_then(J::as_str).map(String::from);
                let parent_raw = map.get("parent").and_then(J::as_str).map(String::from);
                let parent = match parent_raw {
                    Some(p) if !declared_names.contains(&p) => {
                        ctx.push_diag(diag::warn(
                            diag::W_TAG_PARENT_DANGLING,
                            format!(
                                "tag `{name}` references parent `{p}`, which is not declared in \
                                 the top-level `tags` array; dropping the parent reference."
                            ),
                            ptr.loc(ctx.file),
                        ));
                        None
                    }
                    other => other,
                };
                let extensions = operations::collect_extensions(ctx, map, ptr);
                out.push(Tag {
                    name: name.to_string(),
                    summary,
                    description,
                    external_docs,
                    parent,
                    kind,
                    extensions,
                });
            });
        }
    });
    // Determinism: sort by name. `Operation.tags` stays in declared order.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Versions the parser knows how to walk. Adding a new entry is the
/// single edit needed to opt a future version into the existing pipeline;
/// the per-feature differences are gated inside the walkers.
const ACCEPTED_VERSION_PREFIXES: &[&str] = &["3.0.", "3.1.", "3.2."];

/// Walk OAS `example` (3.0 single-literal), `examples` (3.1+ keyed
/// map of Example Objects on parameters / media types), and the
/// JSON-Schema 2020-12 `examples` array (bare literals on Schema
/// Objects) off a schema / parameter / media-type entry. Returns the
/// merged list with the 3.0 `example` stored under the synthetic key
/// `"_default"` and each array-form element under `"_examples[<i>]"`
/// so generators have one shape to read.
///
/// `$ref` into `components.examples.<Name>` resolves through the
/// existing ref machinery. Example values (scalar or compound) are
/// interned into [`Ctx::values`]; the only `W-EXAMPLE-DROPPED` warning
/// remaining covers `value` + `externalValue` co-declaration.
pub(crate) fn parse_examples(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Vec<(String, Example)> {
    let mut out = Vec::new();
    // 3.0 single-form `example: <literal>`. Stored under "_default".
    if let Some(raw) = map.get("example") {
        ptr.with_token("example", |_ptr| {
            let value = Some(ctx.values.intern_json(raw));
            out.push((
                "_default".to_string(),
                Example {
                    summary: None,
                    description: None,
                    value,
                    external_value: None,
                    data_value: None,
                    serialized_value: None,
                },
            ));
        });
    }
    // 3.1+ keyed `examples: { name: ExampleObject | $ref }`.
    if let Some(J::Object(named)) = map.get("examples") {
        ptr.with_token("examples", |ptr| {
            for (name, entry) in named {
                ptr.with_token(name, |ptr| {
                    crate::ref_walk::with_resolved_object(ctx, entry, ptr, |ctx, resolved, ptr| {
                        let Some(emap) = resolved.as_object() else {
                            ctx.push_diag(diag::err(
                                diag::E_INVALID_TYPE,
                                "example must be an object",
                                ptr.loc(ctx.file),
                            ));
                            return Some(());
                        };
                        let summary = emap.get("summary").and_then(J::as_str).map(String::from);
                        let description = emap
                            .get("description")
                            .and_then(J::as_str)
                            .map(String::from);
                        let external_value = emap
                            .get("externalValue")
                            .and_then(J::as_str)
                            .map(String::from);
                        let value = emap.get("value").map(|raw| ctx.values.intern_json(raw));
                        // OAS 3.2 added `dataValue` (parsed form) and
                        // `serializedValue` (wire form) as a refinement
                        // of `value`. Both compound and scalar shapes
                        // survive via the value pool.
                        let data_value =
                            emap.get("dataValue").map(|raw| ctx.values.intern_json(raw));
                        let serialized_value = emap
                            .get("serializedValue")
                            .and_then(J::as_str)
                            .map(String::from);
                        if value.is_some() && external_value.is_some() {
                            ctx.push_diag(diag::err(
                                diag::E_EXAMPLE_VALUE_CONFLICT,
                                format!(
                                    "example `{name}` declares both `value` and `externalValue`; \
                                     OAS §4.7.20 makes them mutually exclusive. Keeping `value`."
                                ),
                                ptr.loc(ctx.file),
                            ));
                        }
                        let kept_external = if value.is_some() {
                            None
                        } else {
                            external_value
                        };
                        out.push((
                            name.clone(),
                            Example {
                                summary,
                                description,
                                value,
                                external_value: kept_external,
                                data_value,
                                serialized_value,
                            },
                        ));
                        Some(())
                    });
                });
            }
        });
    }
    // JSON-Schema 2020-12 array form `examples: [ <literal>, … ]` on a
    // Schema Object. Each element is a bare literal (no Example Object
    // wrapper), lowered under the synthetic index-keyed name
    // `"_examples[<i>]"`, mirroring the `"_default"` key used for the
    // singular `example` keyword. Parameter / media-type `examples` is
    // always the keyed-object form handled above, so a JSON array here
    // only ever originates from a schema.
    if let Some(J::Array(items)) = map.get("examples") {
        ptr.with_token("examples", |ptr| {
            for (i, raw) in items.iter().enumerate() {
                ptr.with_index(i, |_ptr| {
                    let value = Some(ctx.values.intern_json(raw));
                    out.push((
                        format!("_examples[{i}]"),
                        Example {
                            summary: None,
                            description: None,
                            value,
                            external_value: None,
                            data_value: None,
                            serialized_value: None,
                        },
                    ));
                });
            }
        });
    }
    out
}

/// Scan `components.pathItems` and emit
/// `parser/W-COMPONENT-PATH-ITEM-UNUSED` for every entry that wasn't
/// `$ref`'d from `paths`, `webhooks`, or any callback. Tracking lives
/// in `Ctx::referenced_component_path_items`, populated by
/// `with_resolved_object` whenever it resolves a fragment of the form
/// `/components/pathItems/<name>` against the main spec.
fn scan_unused_component_path_items(
    ctx: &mut Ctx,
    root: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) {
    let Some(J::Object(components)) = root.get("components") else {
        return;
    };
    let Some(J::Object(path_items)) = components.get("pathItems") else {
        return;
    };
    ptr.with_token("components", |ptr| {
        ptr.with_token("pathItems", |ptr| {
            for name in path_items.keys() {
                if !ctx.referenced_component_path_items.contains(name) {
                    ptr.with_token(name, |ptr| {
                        ctx.push_diag(diag::warn(
                            diag::W_COMPONENT_PATH_ITEM_UNUSED,
                            format!(
                                "components.pathItems.`{name}` is declared but never \
                                 referenced from paths, webhooks, or callbacks. The \
                                 declaration is silently invisible to generators."
                            ),
                            ptr.loc(ctx.file),
                        ));
                    });
                }
            }
        });
    });
}

/// Scan 3.2 `components.mediaTypes` and emit
/// `parser/W-COMPONENT-MEDIA-TYPE-UNUSED` for every entry that wasn't
/// `$ref`'d from any request body / response content. Tracking lives
/// in `Ctx::referenced_component_media_types`, populated by
/// `with_resolved_object` whenever it resolves a fragment of the form
/// `/components/mediaTypes/<name>` against the main spec.
fn scan_unused_component_media_types(
    ctx: &mut Ctx,
    root: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) {
    let Some(J::Object(components)) = root.get("components") else {
        return;
    };
    let Some(J::Object(media_types)) = components.get("mediaTypes") else {
        return;
    };
    ptr.with_token("components", |ptr| {
        ptr.with_token("mediaTypes", |ptr| {
            for name in media_types.keys() {
                if !ctx.referenced_component_media_types.contains(name) {
                    ptr.with_token(name, |ptr| {
                        ctx.push_diag(diag::warn(
                            diag::W_COMPONENT_MEDIA_TYPE_UNUSED,
                            format!(
                                "components.mediaTypes.`{name}` is declared but never \
                                 referenced. The declaration is silently invisible to \
                                 generators."
                            ),
                            ptr.loc(ctx.file),
                        ));
                    });
                }
            }
        });
    });
}

/// Walk an `operation.callbacks` map (or a `components.callbacks`
/// entry resolved via `$ref`). The OAS shape is
/// `callbacks: { <name>: { <expression>: PathItem } }`; the IR
/// flattens this into a `Vec<Callback>` where each element pairs a
/// name with one runtime expression. Each path item is walked through
/// `parse_path_item` so the inner operations get the same treatment as
/// top-level paths (operationId dedup, params merging, etc.).
pub(crate) fn parse_callbacks(
    ctx: &mut Ctx,
    value: Option<&J>,
    ptr: &mut Ptr,
    seen_op_ids: &mut std::collections::HashSet<String>,
) -> Vec<Callback> {
    let Some(J::Object(named)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    ptr.with_token("callbacks", |ptr| {
        for (name, entry) in named {
            ptr.with_token(name, |ptr| {
                crate::ref_walk::with_resolved_object(ctx, entry, ptr, |ctx, resolved, ptr| {
                    let Some(emap) = resolved.as_object() else {
                        ctx.push_diag(diag::err(
                            diag::E_INVALID_TYPE,
                            "callback must be an object",
                            ptr.loc(ctx.file),
                        ));
                        return Some(());
                    };
                    // Top-level extensions on the callback wrapper.
                    let extensions = operations::collect_extensions(ctx, emap, ptr);
                    for (expr, path_item) in emap {
                        // x-* keys are extensions on the callback
                        // wrapper, not expression keys.
                        if expr.starts_with("x-") {
                            continue;
                        }
                        ptr.with_token(expr, |ptr| {
                            let ops =
                                operations::parse_path_item(ctx, expr, path_item, ptr, seen_op_ids);
                            // Operations live in `Ir.operations` (the
                            // global pool); the callback references
                            // them by id. Push onto the context so
                            // they show up in the final IR.
                            let operation_ids: Vec<String> =
                                ops.iter().map(|o| o.id.clone()).collect();
                            ctx.operations.extend(ops);
                            out.push(Callback {
                                name: name.clone(),
                                expression: expr.clone(),
                                operation_ids,
                                extensions: extensions.clone(),
                            });
                        });
                    }
                    Some(())
                });
            });
        }
    });
    out
}

/// Walk a `response.links` map. Returns the parsed list (named,
/// ordered). `$ref` into `components.links.<Name>` resolves through
/// the existing ref machinery. Compound runtime-expression values
/// (object / array literals) drop with the new
/// `parser/W-LINK-VALUE-DROPPED` warning.
pub(crate) fn parse_links(ctx: &mut Ctx, value: Option<&J>, ptr: &mut Ptr) -> Vec<(String, Link)> {
    let Some(J::Object(named)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    ptr.with_token("links", |ptr| {
        for (name, entry) in named {
            ptr.with_token(name, |ptr| {
                crate::ref_walk::with_resolved_object(ctx, entry, ptr, |ctx, resolved, ptr| {
                    let Some(lmap) = resolved.as_object() else {
                        ctx.push_diag(diag::err(
                            diag::E_INVALID_TYPE,
                            "link must be an object",
                            ptr.loc(ctx.file),
                        ));
                        return Some(());
                    };
                    let operation_ref = lmap
                        .get("operationRef")
                        .and_then(J::as_str)
                        .map(String::from);
                    let raw_operation_id = lmap
                        .get("operationId")
                        .and_then(J::as_str)
                        .map(String::from);
                    let operation_id = if operation_ref.is_some() && raw_operation_id.is_some() {
                        ctx.push_diag(diag::err(
                            diag::E_LINK_OP_CONFLICT,
                            format!(
                                "link `{name}` declares both `operationRef` and `operationId`; \
                                 OAS §4.7.21 makes them mutually exclusive. Keeping `operationRef`."
                            ),
                            ptr.loc(ctx.file),
                        ));
                        None
                    } else {
                        raw_operation_id
                    };
                    let parameters = lmap
                        .get("parameters")
                        .and_then(|v| v.as_object())
                        .map(|m| {
                            m.iter()
                                .map(|(k, raw)| (k.clone(), ctx.values.intern_json(raw)))
                                .collect()
                        })
                        .unwrap_or_default();
                    let request_body = lmap
                        .get("requestBody")
                        .map(|raw| ctx.values.intern_json(raw));
                    // OAS §4.20: Link Object carries only `description`.
                    let description = crate::schema::description(lmap);
                    let server = lmap.get("server").and_then(|s| {
                        s.as_object().and_then(|m| {
                            let url = m.get("url").and_then(J::as_str)?;
                            let server_desc =
                                m.get("description").and_then(J::as_str).map(String::from);
                            let server_name = m.get("name").and_then(J::as_str).map(String::from);
                            Some(Server {
                                url: url.to_string(),
                                description: server_desc,
                                name: server_name,
                                variables: Vec::new(),
                                extensions: Vec::new(),
                            })
                        })
                    });
                    let extensions = operations::collect_extensions(ctx, lmap, ptr);
                    out.push((
                        name.clone(),
                        Link {
                            operation_ref,
                            operation_id,
                            parameters,
                            request_body,
                            description,
                            server,
                            extensions,
                        },
                    ));
                    Some(())
                });
            });
        }
    });
    out
}

/// OAS Schema Object's `xml` block. Returns `None` when the spec
/// didn't declare an `xml` field. Defaults match OAS: `attribute` and
/// `wrapped` are `false`. `x-*` extensions on the xml block survive
/// via the existing `collect_extensions` path.
pub(crate) fn parse_xml(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<XmlObject> {
    let xml = map.get("xml")?;
    let xml_map = xml.as_object()?;
    let mut out = None;
    ptr.with_token("xml", |ptr| {
        let name = xml_map.get("name").and_then(J::as_str).map(String::from);
        let namespace = xml_map
            .get("namespace")
            .and_then(J::as_str)
            .map(String::from);
        let prefix = xml_map.get("prefix").and_then(J::as_str).map(String::from);
        let attribute = xml_map
            .get("attribute")
            .and_then(J::as_bool)
            .unwrap_or(false);
        let wrapped = xml_map.get("wrapped").and_then(J::as_bool).unwrap_or(false);
        let text = xml_map.get("text").and_then(J::as_bool).unwrap_or(false);
        let ordered = xml_map.get("ordered").and_then(J::as_bool).unwrap_or(false);
        let extensions = operations::collect_extensions(ctx, xml_map, ptr);
        out = Some(XmlObject {
            name,
            namespace,
            prefix,
            attribute,
            wrapped,
            text,
            ordered,
            extensions,
        });
    });
    out
}

/// JSON Schema `default` for a schema or property. Interns the value
/// (scalar or compound) into [`Ctx::values`] and returns its
/// [`forge_ir::ValueRef`]. Returns `None` only when the field is absent.
pub(crate) fn parse_default(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    _ptr: &mut Ptr,
    _site: &str,
) -> Option<forge_ir::ValueRef> {
    let raw = map.get("default")?;
    Some(ctx.values.intern_json(raw))
}

/// Walk an OAS ExternalDocumentation Object. `url` is the only
/// required field; documents that omit it surface
/// `parser/W-EXTERNAL-DOCS-NO-URL` and the block is dropped. Used
/// at root, per-operation, and per-schema sites.
pub(crate) fn parse_external_docs(
    ctx: &mut Ctx,
    value: Option<&J>,
    ptr: &mut Ptr,
) -> Option<ExternalDocs> {
    let map = value?.as_object()?;
    let mut out = None;
    ptr.with_token("externalDocs", |ptr| {
        let Some(url) = map.get("url").and_then(J::as_str) else {
            ctx.push_diag(diag::warn(
                diag::W_EXTERNAL_DOCS_NO_URL,
                "externalDocs is missing required `url`; dropping the block.",
                ptr.loc(ctx.file),
            ));
            return;
        };
        let description = map.get("description").and_then(J::as_str).map(String::from);
        out = Some(ExternalDocs {
            description,
            url: url.to_string(),
        });
    });
    out
}

fn check_version(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) -> bool {
    // OpenAPI 2.0 / Swagger uses a `swagger` field instead of `openapi`.
    // Surface it specifically so users get a clear message.
    if root.contains_key("swagger") {
        ptr.with_token("swagger", |ptr| {
            ctx.push_diag(diag::err(
                diag::E_UNSUPPORTED_VERSION,
                "OpenAPI 2.0 (Swagger) is not supported and is not on the roadmap. \
                 Convert to OpenAPI 3.0 upstream (e.g. `swagger2openapi`) before invoking forge.",
                ptr.loc(ctx.file),
            ));
        });
        return false;
    }
    let version = root.get("openapi").and_then(J::as_str);
    match version {
        Some(v) if ACCEPTED_VERSION_PREFIXES.iter().any(|p| v.starts_with(p)) => {
            // OAS 3.0 forbade `$ref` siblings; 3.1+ inherits JSON
            // Schema 2020-12's allowance. The schema walker reads this
            // bit to pick the right diagnostic.
            ctx.is_oas_3_0 = v.starts_with("3.0.");
            true
        }
        Some(other) => {
            let msg = if other.starts_with("2.") || other.starts_with("1.") {
                format!(
                    "OpenAPI {other} is not supported and is not on the roadmap. \
                     Convert to OpenAPI 3.x upstream before invoking forge."
                )
            } else {
                format!("unsupported OpenAPI version `{other}`; expected 3.0.x / 3.1.x / 3.2.x")
            };
            ptr.with_token("openapi", |ptr| {
                ctx.push_diag(diag::err(
                    diag::E_UNSUPPORTED_VERSION,
                    msg,
                    ptr.loc(ctx.file),
                ));
            });
            false
        }
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "missing required `openapi` field",
                SpecLocation::new(""),
            ));
            false
        }
    }
}

fn parse_info(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) {
    let Some(J::Object(info)) = root.get("info") else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "missing required `info` object",
            ptr.loc(ctx.file),
        ));
        return;
    };
    ptr.with_token("info", |ptr| {
        let title = info.get("title").and_then(J::as_str).unwrap_or_else(|| {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "info is missing `title`",
                ptr.loc(ctx.file),
            ));
            ""
        });
        let version = info.get("version").and_then(J::as_str).unwrap_or_else(|| {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "info is missing `version`",
                ptr.loc(ctx.file),
            ));
            ""
        });
        // OAS §4.2: Info Object carries summary (3.1+) and description.
        let summary = crate::schema::summary(info);
        let description = crate::schema::description(info);
        let terms_of_service = info
            .get("termsOfService")
            .and_then(J::as_str)
            .map(String::from);
        let contact =
            info.get("contact")
                .and_then(|v| v.as_object())
                .and_then(|m| -> Option<Contact> {
                    let name = m.get("name").and_then(J::as_str).map(String::from);
                    let url = m.get("url").and_then(J::as_str).map(String::from);
                    let email = m.get("email").and_then(J::as_str).map(String::from);
                    if name.is_none() && url.is_none() && email.is_none() {
                        None
                    } else {
                        Some(Contact { name, url, email })
                    }
                });
        let license = info.get("license").and_then(|l| l.as_object());
        let license_name = license
            .and_then(|m| m.get("name"))
            .and_then(J::as_str)
            .map(String::from);
        let license_url = license
            .and_then(|m| m.get("url"))
            .and_then(J::as_str)
            .map(String::from);
        let license_identifier = license
            .and_then(|m| m.get("identifier"))
            .and_then(J::as_str)
            .map(String::from);
        let extensions = operations::collect_extensions(ctx, info, ptr);
        ctx.info = Some(ApiInfo {
            title: title.to_string(),
            version: version.to_string(),
            summary,
            description,
            terms_of_service,
            contact,
            license_name,
            license_url,
            license_identifier,
            extensions,
        });
    });
}

fn parse_servers(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) {
    let servers = parse_servers_array(ctx, root.get("servers"), ptr);
    ctx.servers.extend(servers);
}

/// Walk a `servers` array off any host (root, path-item, or operation)
/// and return the parsed list. Empty / missing input returns `[]`. Used
/// by `parse_servers` for the root list and by the operations walker
/// for path-item / per-operation overrides.
pub(crate) fn parse_servers_array(ctx: &mut Ctx, value: Option<&J>, ptr: &mut Ptr) -> Vec<Server> {
    let Some(J::Array(items)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    ptr.with_token("servers", |ptr| {
        for (i, item) in items.iter().enumerate() {
            ptr.with_index(i, |ptr| {
                let Some(map) = item.as_object() else {
                    ctx.push_diag(diag::err(
                        diag::E_INVALID_TYPE,
                        "server must be an object",
                        ptr.loc(ctx.file),
                    ));
                    return;
                };
                let Some(url) = map.get("url").and_then(J::as_str) else {
                    ctx.push_diag(diag::err(
                        diag::E_MISSING_FIELD,
                        "server is missing `url`",
                        ptr.loc(ctx.file),
                    ));
                    return;
                };
                // OAS §4.5: Server carries `description` only (+ name).
                let description = crate::schema::description(map);
                let server_name = map.get("name").and_then(J::as_str).map(String::from);
                let mut variables: Vec<(String, ServerVariable)> = Vec::new();
                if let Some(J::Object(vars)) = map.get("variables") {
                    ptr.with_token("variables", |ptr| {
                        for (name, v) in vars {
                            ptr.with_token(name, |ptr| {
                                let Some(vmap) = v.as_object() else { return };
                                let Some(default) = vmap.get("default").and_then(J::as_str) else {
                                    return;
                                };
                                let var_extensions = operations::collect_extensions(ctx, vmap, ptr);
                                // OAS §4.6: ServerVariable carries `description` only.
                                let var_description = crate::schema::description(vmap);
                                variables.push((
                                    name.clone(),
                                    ServerVariable {
                                        default: default.to_string(),
                                        r#enum: vmap.get("enum").and_then(|e| {
                                            e.as_array().map(|arr| {
                                                arr.iter()
                                                    .filter_map(|v| v.as_str().map(String::from))
                                                    .collect()
                                            })
                                        }),
                                        description: var_description,
                                        extensions: var_extensions,
                                    },
                                ));
                            });
                        }
                    });
                }
                let extensions = operations::collect_extensions(ctx, map, ptr);
                out.push(Server {
                    url: url.to_string(),
                    description,
                    name: server_name,
                    variables,
                    extensions,
                });
            });
        }
    });
    out
}

fn register_component_schemas(ctx: &mut Ctx, root: &serde_json::Map<String, J>) {
    let Some(J::Object(components)) = root.get("components") else {
        return;
    };
    let Some(J::Object(schemas)) = components.get("schemas") else {
        return;
    };
    for name in schemas.keys() {
        let id = sanitize::ident(name);
        ctx.refs_mut().register(&id);
    }
}

fn walk_component_schemas(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) {
    let Some(J::Object(components)) = root.get("components") else {
        return;
    };
    let Some(J::Object(schemas)) = components.get("schemas") else {
        return;
    };
    // Pre-register every `components.schemas.<X>: { $ref: ext.json#/Y }`
    // mapping before walking. Without this step, a sibling component
    // walked earlier whose body $ref's the same external schema would
    // trigger that schema's walk under an `Inline` hint and pollute the
    // dedup map with the prefixed id; the later `components.schemas.X`
    // walk would then dedup-hit and inherit the wrong (prefixed) id.
    pre_register_external_named_hints(ctx, schemas);
    let order = order_components_by_allof(schemas);
    ptr.with_token("components", |ptr| {
        ptr.with_token("schemas", |ptr| {
            for name in &order {
                let Some(schema) = schemas.get(name) else {
                    continue;
                };
                ptr.with_token(name, |ptr| {
                    // Push the in-progress walk into `walking` so a
                    // cross-file ref that loops back at this schema
                    // recognises the cycle and returns the id without
                    // re-walking. The lazy walker uses the same key shape.
                    let key = (
                        ctx.current_doc.clone(),
                        format!("/components/schemas/{name}"),
                    );
                    ctx.walking.insert(key.clone());
                    let _ = parse_schema(ctx, schema, ptr, NameHint::Named(name.clone()));
                    ctx.walking.remove(&key);
                });
            }
        });
    });
}

/// Walk `components.schemas` and seed the external-ref dedup map for
/// every entry whose body is a single `$ref` to a cross-document schema.
/// The seeded id is the component's *Named* id (e.g. `Pet`), so any
/// indirect resolution to the same target during sibling-schema walking
/// returns `Pet` instead of synthesising a fresh `<docprefix>Pet`.
///
/// This only seeds *direct* `$ref`s — schemas with `allOf`/`oneOf`/etc.
/// that internally use `$ref` get their dedup entry filled by the lazy
/// walker as usual.
fn pre_register_external_named_hints(ctx: &mut Ctx, schemas: &serde_json::Map<String, J>) {
    let current_doc = ctx.current_doc.clone();
    for (name, schema) in schemas {
        let Some(map) = schema.as_object() else {
            continue;
        };
        let Some(J::String(raw)) = map.get("$ref") else {
            continue;
        };
        let (file_part, fragment) = crate::external::split_ref(raw);
        if file_part.is_empty() || crate::external::is_url(file_part) {
            continue;
        }
        let Ok(loaded) = ctx.resolver.load(raw, &current_doc) else {
            continue;
        };
        let canonical = loaded.canonical_path.clone();
        crate::schema::ensure_doc_registered(ctx, &canonical, &loaded.root);
        ctx.external_ref_to_id
            .entry((canonical, fragment.to_string()))
            .or_insert_with(|| crate::sanitize::ident(name));
    }
}

/// Order component schemas so any schema that uses `allOf: [{ $ref: X }]`
/// is walked *after* `X`. Cycles fall back to alphabetical order at the
/// end (the parser still produces best-effort output, and `finalize`
/// flags the cycle separately).
fn order_components_by_allof(schemas: &serde_json::Map<String, J>) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, schema) in schemas {
        let mut targets: BTreeSet<String> = BTreeSet::new();
        collect_allof_ref_targets(schema, &mut targets);
        targets.retain(|t| schemas.contains_key(t) && t != name);
        deps.insert(name.clone(), targets);
    }

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut ordered: Vec<String> = Vec::new();
    let mut all_names: Vec<String> = schemas.keys().cloned().collect();
    all_names.sort();

    loop {
        let next = all_names.iter().find(|n| {
            !visited.contains(*n)
                && deps
                    .get(*n)
                    .map(|d| d.iter().all(|t| visited.contains(t)))
                    .unwrap_or(true)
        });
        match next {
            Some(name) => {
                let n = name.clone();
                visited.insert(n.clone());
                ordered.push(n);
            }
            None => break,
        }
    }
    // Cycle remainder: append alphabetically.
    for n in all_names {
        if !visited.contains(&n) {
            ordered.push(n);
        }
    }
    ordered
}

fn collect_allof_ref_targets(value: &J, out: &mut std::collections::BTreeSet<String>) {
    let Some(map) = value.as_object() else {
        return;
    };
    if let Some(J::Array(parts)) = map.get("allOf") {
        for part in parts {
            if let Some(rs) = part
                .as_object()
                .and_then(|m| m.get("$ref"))
                .and_then(|r| r.as_str())
            {
                if let Some(name) = rs.strip_prefix("#/components/schemas/") {
                    out.insert(name.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_errors() {
        let err = parse_str("").unwrap_err();
        matches!(err, ParseError::Empty);
    }

    #[test]
    fn invalid_json_errors() {
        let err = parse_str("{not json").unwrap_err();
        matches!(err, ParseError::InvalidJson(_));
    }

    #[test]
    fn root_array_errors() {
        let err = parse_str("[]").unwrap_err();
        matches!(err, ParseError::NotObject);
    }

    #[test]
    fn unsupported_version_diagnostic() {
        // 4.0 is not on the roadmap; it should fail-fast.
        let src = r#"{"openapi":"4.0.0","info":{"title":"x","version":"1"},"paths":{}}"#;
        let out = parse_str(src).unwrap();
        assert!(out.spec.is_none());
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, diag::E_UNSUPPORTED_VERSION);
    }

    #[test]
    fn minimal_spec_round_trips() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{}
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        assert_eq!(ir.info.title, "t");
        assert!(ir.operations.is_empty());
        assert!(ir.types.is_empty());
    }

    #[test]
    fn info_full_block_populates_every_field() {
        let src = r#"{
            "openapi":"3.1.0",
            "info":{
                "title":"t",
                "version":"1",
                "summary":"s",
                "description":"d",
                "termsOfService":"https://tos.example",
                "contact":{
                    "name":"API Team",
                    "url":"https://example.com",
                    "email":"team@example.com"
                },
                "license":{
                    "name":"Apache 2.0",
                    "url":"https://www.apache.org/licenses/LICENSE-2.0",
                    "identifier":"Apache-2.0"
                }
            },
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.info.summary.as_deref(), Some("s"));
        assert_eq!(ir.info.description.as_deref(), Some("d"));
        assert_eq!(
            ir.info.terms_of_service.as_deref(),
            Some("https://tos.example")
        );
        let contact = ir.info.contact.expect("contact populated");
        assert_eq!(contact.name.as_deref(), Some("API Team"));
        assert_eq!(contact.url.as_deref(), Some("https://example.com"));
        assert_eq!(contact.email.as_deref(), Some("team@example.com"));
        assert_eq!(ir.info.license_name.as_deref(), Some("Apache 2.0"));
        assert_eq!(
            ir.info.license_url.as_deref(),
            Some("https://www.apache.org/licenses/LICENSE-2.0")
        );
        assert_eq!(ir.info.license_identifier.as_deref(), Some("Apache-2.0"));
    }

    #[test]
    fn info_contact_object_with_no_known_keys_is_none() {
        // OAS allows `x-*` extensions on Contact; with no recognised
        // fields, the IR should leave `contact` as None rather than
        // emitting an empty Contact record.
        let src = r#"{
            "openapi":"3.0.0",
            "info":{
                "title":"t",
                "version":"1",
                "contact":{ "x-vendor": "acme" }
            },
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert!(ir.info.contact.is_none());
    }

    #[test]
    fn external_docs_populated_at_root_operation_and_schema() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "externalDocs":{"description":"top","url":"https://example.com"},
            "paths":{
                "/x":{
                    "get":{
                        "operationId":"getX",
                        "externalDocs":{"url":"https://example.com/op"},
                        "responses":{"200":{"description":"ok"}}
                    }
                }
            },
            "components":{
                "schemas":{
                    "Foo":{
                        "type":"object",
                        "externalDocs":{"description":"d","url":"https://example.com/foo"}
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let root = ir.external_docs.expect("root externalDocs");
        assert_eq!(root.url, "https://example.com");
        assert_eq!(root.description.as_deref(), Some("top"));

        let op_docs = ir.operations[0]
            .external_docs
            .as_ref()
            .expect("op externalDocs");
        assert_eq!(op_docs.url, "https://example.com/op");
        assert!(op_docs.description.is_none());

        let foo = ir.types.iter().find(|t| t.id == "Foo").expect("Foo type");
        let schema_docs = foo.external_docs.as_ref().expect("schema externalDocs");
        assert_eq!(schema_docs.url, "https://example.com/foo");
        assert_eq!(schema_docs.description.as_deref(), Some("d"));
    }

    #[test]
    fn external_docs_missing_url_warns_and_drops() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "externalDocs":{"description":"oops"},
            "paths":{}
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        assert!(ir.external_docs.is_none());
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_EXTERNAL_DOCS_NO_URL));
    }

    #[test]
    fn webhooks_carry_routing_name_and_multiple_methods() {
        let src = r#"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "webhooks":{
                "newPet":{
                    "post":{
                        "operationId":"newPetCreated",
                        "responses":{"200":{"description":"ok"}}
                    },
                    "delete":{
                        "operationId":"newPetDeleted",
                        "responses":{"200":{"description":"ok"}}
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.webhooks.len(), 1);
        let w = &ir.webhooks[0];
        assert_eq!(w.name, "newPet");
        // Path item has both `post` and `delete`; both surface as
        // operations on the same Webhook.
        assert_eq!(w.operations.len(), 2);
        assert!(w.operations.iter().any(|o| o.id == "newPetCreated"));
        assert!(w.operations.iter().any(|o| o.id == "newPetDeleted"));
    }

    #[test]
    fn webhooks_sort_by_name() {
        let src = r#"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "webhooks":{
                "zebra":{"post":{"operationId":"z","responses":{"200":{"description":"ok"}}}},
                "alpha":{"post":{"operationId":"a","responses":{"200":{"description":"ok"}}}}
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.webhooks[0].name, "alpha");
        assert_eq!(ir.webhooks[1].name, "zebra");
    }

    #[test]
    fn response_headers_use_dedicated_header_struct() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x":{
                    "get":{
                        "operationId":"x",
                        "responses":{
                            "200":{
                                "description":"ok",
                                "headers":{
                                    "X-Trace":{
                                        "description":"trace id",
                                        "required":true,
                                        "schema":{"type":"string"}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let resp = &ir.operations[0].responses[0];
        assert_eq!(resp.headers.len(), 1);
        let (name, header) = &resp.headers[0];
        assert_eq!(name, "X-Trace");
        assert!(header.required);
        assert_eq!(header.description.as_deref(), Some("trace id"));
        // No `name`, `style`, `explode`, etc. — Header struct doesn't
        // carry those (they don't apply to OAS headers).
    }

    #[test]
    fn openid_connect_security_scheme_round_trips() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "securitySchemes":{
                    "oidc":{
                        "type":"openIdConnect",
                        "openIdConnectUrl":"https://example.com/.well-known/openid-configuration"
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let scheme = ir
            .security_schemes
            .iter()
            .find(|s| s.id == "oidc")
            .expect("oidc scheme present");
        match &scheme.kind {
            forge_ir::SecuritySchemeKind::OpenIdConnect { url } => {
                assert_eq!(url, "https://example.com/.well-known/openid-configuration");
            }
            other => panic!("expected OpenIdConnect, got {other:?}"),
        }
    }

    #[test]
    fn openid_connect_missing_url_errors() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "securitySchemes":{
                    "oidc":{"type":"openIdConnect"}
                }
            }
        }"#;
        let out = parse_str(src).unwrap();
        // Scheme is dropped (None return) — security_schemes empty.
        assert!(out.spec.unwrap().security_schemes.is_empty());
        // Missing-field error fires.
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::E_MISSING_FIELD));
    }

    #[test]
    fn ref_siblings_warn_on_oas_3_0() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "A":{"type":"string"},
                    "B":{"$ref":"#/components/schemas/A","description":"sibling"}
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let diags = out.diagnostics;
        let warning = diags
            .iter()
            .find(|d| d.code == diag::W_REF_SIBLINGS_3_0)
            .expect("warning emitted");
        assert!(warning.message.contains("description"));
    }

    #[test]
    fn ref_siblings_dont_warn_on_oas_3_1() {
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "A":{"type":"string"},
                    "B":{"$ref":"#/components/schemas/A","description":"sibling"}
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        assert!(!out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_REF_SIBLINGS_3_0));
    }

    #[test]
    fn ref_with_only_x_extensions_does_not_warn() {
        // x-* extensions are always allowed alongside $ref (per OAS).
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "A":{"type":"string"},
                    "B":{"$ref":"#/components/schemas/A","x-vendor":"acme"}
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        assert!(!out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_REF_SIBLINGS_3_0));
    }

    #[test]
    fn referenced_component_path_item_lands_in_operations() {
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/items":{"$ref":"#/components/pathItems/ItemsPath"}
            },
            "components":{
                "pathItems":{
                    "ItemsPath":{
                        "get":{"operationId":"list","responses":{"200":{"description":"ok"}}}
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        // Operation came in via $ref.
        assert!(ir.operations.iter().any(|o| o.id == "list"));
        // No unused warning since the only declaration is referenced.
        assert!(!out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_COMPONENT_PATH_ITEM_UNUSED));
    }

    #[test]
    fn unused_component_path_item_warns() {
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "pathItems":{
                    "Orphan":{
                        "get":{"operationId":"orphan","responses":{"200":{"description":"ok"}}}
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        // The orphan operation never lands in IR — only the warning.
        assert!(ir.operations.is_empty());
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_COMPONENT_PATH_ITEM_UNUSED));
    }

    #[test]
    fn webhook_ref_into_component_path_item_counts_as_use() {
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "webhooks":{
                "ev":{"$ref":"#/components/pathItems/EventPath"}
            },
            "components":{
                "pathItems":{
                    "EventPath":{
                        "post":{"operationId":"ev","responses":{"200":{"description":"ok"}}}
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        // Webhook reference counts — no unused warning.
        assert!(!out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_COMPONENT_PATH_ITEM_UNUSED));
    }

    #[test]
    fn callbacks_walk_inline_and_via_ref() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/sub":{
                    "post":{
                        "operationId":"sub",
                        "responses":{"200":{"description":"ok"}},
                        "callbacks":{
                            "evt":{
                                "{$request.body#/url}":{
                                    "post":{
                                        "operationId":"evtCb",
                                        "responses":{"200":{"description":"ok"}}
                                    }
                                }
                            },
                            "shared":{"$ref":"#/components/callbacks/Shared"}
                        }
                    }
                }
            },
            "components":{
                "callbacks":{
                    "Shared":{
                        "{$request.body#/sharedUrl}":{
                            "post":{
                                "operationId":"sharedCb",
                                "responses":{"200":{"description":"ok"}}
                            }
                        }
                    }
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let sub = ir.operations.iter().find(|o| o.id == "sub").unwrap();
        assert_eq!(sub.callbacks.len(), 2);
        let evt = sub.callbacks.iter().find(|c| c.name == "evt").unwrap();
        assert_eq!(evt.expression, "{$request.body#/url}");
        assert_eq!(evt.operation_ids, vec!["evtCb".to_string()]);
        let shared = sub.callbacks.iter().find(|c| c.name == "shared").unwrap();
        assert_eq!(shared.expression, "{$request.body#/sharedUrl}");
        assert_eq!(shared.operation_ids, vec!["sharedCb".to_string()]);
        // Callback operations live in Ir.operations alongside the
        // top-level operation.
        assert!(ir.operations.iter().any(|o| o.id == "evtCb"));
        assert!(ir.operations.iter().any(|o| o.id == "sharedCb"));
    }

    #[test]
    fn callback_op_id_collides_with_top_level_emits_dup_error() {
        // Callback operationIds share the global namespace with
        // top-level operations per OAS.
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/a":{
                    "post":{
                        "operationId":"foo",
                        "responses":{"200":{"description":"ok"}},
                        "callbacks":{
                            "x":{
                                "{$req}":{
                                    "post":{
                                        "operationId":"foo",
                                        "responses":{"200":{"description":"ok"}}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::E_DUPLICATE_OPERATION_ID));
    }

    #[test]
    fn response_links_populate_inline_and_via_ref() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/u":{
                    "get":{
                        "operationId":"getU",
                        "responses":{
                            "200":{
                                "description":"ok",
                                "links":{
                                    "addr":{
                                        "operationId":"getA",
                                        "parameters":{"id":"$response.body#/id"},
                                        "description":"docs"
                                    },
                                    "shared":{"$ref":"#/components/links/Shared"}
                                }
                            }
                        }
                    }
                },
                "/a":{
                    "get":{
                        "operationId":"getA",
                        "responses":{"200":{"description":"ok"}}
                    }
                }
            },
            "components":{
                "links":{
                    "Shared":{"operationId":"getA","description":"shared"}
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let op = ir.operations.iter().find(|o| o.id == "getU").unwrap();
        let links = &op.responses[0].links;
        assert_eq!(links.len(), 2);
        let addr = &links.iter().find(|(k, _)| k == "addr").unwrap().1;
        assert_eq!(addr.operation_id.as_deref(), Some("getA"));
        assert_eq!(addr.parameters.len(), 1);
        assert_eq!(addr.parameters[0].0, "id");
        assert_eq!(addr.description.as_deref(), Some("docs"));
        let shared = &links.iter().find(|(k, _)| k == "shared").unwrap().1;
        assert_eq!(shared.description.as_deref(), Some("shared"));
        assert_eq!(shared.operation_id.as_deref(), Some("getA"));
    }

    #[test]
    fn link_with_both_operation_ref_and_id_keeps_ref() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/u":{
                    "get":{
                        "operationId":"getU",
                        "responses":{
                            "200":{
                                "description":"ok",
                                "links":{
                                    "x":{
                                        "operationRef":"#/paths/~1a/get",
                                        "operationId":"getA"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        let link = &ir.operations[0].responses[0].links[0].1;
        assert!(link.operation_ref.is_some());
        assert!(link.operation_id.is_none());
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::E_LINK_OP_CONFLICT));
    }

    #[test]
    fn link_compound_parameter_survives_via_value_pool() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/u":{
                    "get":{
                        "operationId":"getU",
                        "responses":{
                            "200":{
                                "description":"ok",
                                "links":{
                                    "x":{
                                        "operationId":"foo",
                                        "parameters":{"complex":["a","b"]}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        let link = &ir.operations[0].responses[0].links[0].1;
        // Compound parameters survive: the entry resolves to a List in the
        // value pool. No W-*-DROPPED warnings any more.
        assert_eq!(link.parameters.len(), 1);
        let r = link.parameters[0].1 as usize;
        assert!(matches!(ir.values[r], forge_ir::Value::List { .. }));
    }

    #[test]
    fn xml_block_populates_with_all_fields() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Pet":{
                        "type":"object",
                        "xml":{
                            "name":"Pet",
                            "namespace":"http://example.com/pet",
                            "prefix":"pt",
                            "attribute":false,
                            "wrapped":true,
                            "x-vendor":"acme"
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let pet = ir.types.iter().find(|t| t.id == "Pet").unwrap();
        let xml = pet.xml.as_ref().expect("xml populated");
        assert_eq!(xml.name.as_deref(), Some("Pet"));
        assert_eq!(xml.namespace.as_deref(), Some("http://example.com/pet"));
        assert_eq!(xml.prefix.as_deref(), Some("pt"));
        assert!(!xml.attribute);
        assert!(xml.wrapped);
        assert_eq!(xml.extensions.len(), 1);
        assert_eq!(xml.extensions[0].0, "x-vendor");
    }

    #[test]
    fn xml_attribute_defaults_to_false() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Foo":{"type":"string","xml":{"name":"Foo"}}
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        let xml = foo.xml.as_ref().unwrap();
        assert!(!xml.attribute);
        assert!(!xml.wrapped);
    }

    #[test]
    fn xml_absent_leaves_field_none() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{"Foo":{"type":"string"}}}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        assert!(foo.xml.is_none());
    }

    #[test]
    fn examples_populate_at_parameter_and_schema_sites() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x/{id}":{
                    "get":{
                        "operationId":"getX",
                        "parameters":[{
                            "name":"id","in":"path","required":true,
                            "schema":{"type":"string"},
                            "examples":{
                                "short":{"summary":"S","value":"42"},
                                "uuid":{"$ref":"#/components/examples/UuidExample"}
                            }
                        }],
                        "responses":{"204":{"description":"ok"}}
                    }
                }
            },
            "components":{
                "examples":{
                    "UuidExample":{"summary":"UUID","value":"abc"}
                },
                "schemas":{
                    "Foo":{"type":"string","example":"hello"}
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        // Parameter examples (inline + ref'd).
        let param = &ir.operations[0].path_params[0];
        assert_eq!(param.examples.len(), 2);
        assert_eq!(param.examples[0].0, "short");
        let r0 = param.examples[0].1.value.unwrap() as usize;
        assert_eq!(ir.values[r0], forge_ir::Value::s("42"));
        assert_eq!(param.examples[1].0, "uuid");
        let r1 = param.examples[1].1.value.unwrap() as usize;
        assert_eq!(ir.values[r1], forge_ir::Value::s("abc"));
        // Schema-level 3.0 single-example lands under "_default".
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        assert_eq!(foo.examples.len(), 1);
        assert_eq!(foo.examples[0].0, "_default");
        let r2 = foo.examples[0].1.value.unwrap() as usize;
        assert_eq!(ir.values[r2], forge_ir::Value::s("hello"));
    }

    #[test]
    fn compound_example_survives_via_value_pool() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Foo":{"type":"object","example":{"k":"v"}}
                }
            }
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").cloned().unwrap();
        // Compound example survives in the pool.
        assert_eq!(foo.examples.len(), 1);
        assert_eq!(foo.examples[0].0, "_default");
        let r = foo.examples[0].1.value.unwrap() as usize;
        let resolved = &ir.values[r];
        let forge_ir::Value::Object { fields } = resolved else {
            panic!("expected object example, got {resolved:?}");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "k");
        assert_eq!(ir.values[fields[0].1 as usize], forge_ir::Value::s("v"));
    }

    #[test]
    fn example_with_value_and_external_value_keeps_value() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "examples":{
                    "Conflict":{
                        "value":"inline",
                        "externalValue":"https://example.com/blob"
                    }
                },
                "schemas":{
                    "Foo":{
                        "type":"string",
                        "examples":{"a":{"$ref":"#/components/examples/Conflict"}}
                    }
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.as_ref().unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        let ex = &foo.examples[0].1;
        let r = ex.value.unwrap() as usize;
        assert_eq!(ir.values[r], forge_ir::Value::s("inline"));
        assert!(ex.external_value.is_none());
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::E_EXAMPLE_VALUE_CONFLICT));
    }

    #[test]
    fn json_schema_examples_array_lowers_at_schema_and_property_sites() {
        // JSON-Schema 2020-12 array-form `examples` on a Schema Object
        // (and on an object property's schema) must reach the IR under
        // index-keyed synthetic names, alongside the singular `example`.
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "ArrayBadSchema":{
                        "type":"integer","format":"int32",
                        "examples":["not-an-int", 7]
                    },
                    "ObjArrayBadProp":{
                        "type":"object",
                        "properties":{
                            "n":{"type":"integer","format":"int32","examples":["also-not-an-int"]}
                        },
                        "required":["n"],
                        "additionalProperties":false
                    }
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        // Schema-level array form: one entry per element, index-keyed.
        let arr = ir.types.iter().find(|t| t.id == "ArrayBadSchema").unwrap();
        assert_eq!(arr.examples.len(), 2);
        assert_eq!(arr.examples[0].0, "_examples[0]");
        let r0 = arr.examples[0].1.value.unwrap() as usize;
        assert_eq!(ir.values[r0], forge_ir::Value::s("not-an-int"));
        assert_eq!(arr.examples[1].0, "_examples[1]");
        let r1 = arr.examples[1].1.value.unwrap() as usize;
        assert_eq!(ir.values[r1], forge_ir::Value::Int { value: 7 });
        // Property-level array form reaches `Property.examples`.
        let obj_ty = ir.types.iter().find(|t| t.id == "ObjArrayBadProp").unwrap();
        let forge_ir::TypeDef::Object(obj) = &obj_ty.definition else {
            panic!("expected object");
        };
        let n = obj.properties.iter().find(|p| p.name == "n").unwrap();
        assert_eq!(n.examples.len(), 1);
        assert_eq!(n.examples[0].0, "_examples[0]");
        let rp = n.examples[0].1.value.unwrap() as usize;
        assert_eq!(ir.values[rp], forge_ir::Value::s("also-not-an-int"));
    }

    #[test]
    fn singular_example_and_examples_array_coexist() {
        // A schema may carry both keywords; they land under distinct
        // synthetic keys without colliding.
        let src = r##"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Both":{"type":"string","example":"a","examples":["b","c"]}
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let both = ir.types.iter().find(|t| t.id == "Both").unwrap();
        let keys: Vec<&str> = both.examples.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["_default", "_examples[0]", "_examples[1]"]);
    }

    #[test]
    fn item_schema_populates_item_schema_and_type() {
        // OAS 3.2: itemSchema-only entry. type is set to the item-type
        // ref so non-streaming generators see a usable type;
        // item_schema is populated for streaming-aware generators.
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/events":{
                    "get":{
                        "operationId":"stream",
                        "responses":{
                            "200":{
                                "description":"jsonl",
                                "content":{
                                    "application/jsonl":{
                                        "itemSchema":{"$ref":"#/components/schemas/Event"}
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "components":{
                "schemas":{
                    "Event":{"type":"object","properties":{"id":{"type":"string"}}}
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let op = &ir.operations[0];
        let content = &op.responses[0].content[0];
        assert_eq!(content.media_type, "application/jsonl");
        assert_eq!(content.r#type, "Event");
        assert_eq!(content.item_schema.as_deref(), Some("Event"));
    }

    #[test]
    fn schema_only_leaves_item_schema_none() {
        // Plain schema should not populate item_schema.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x":{
                    "get":{
                        "operationId":"x",
                        "responses":{
                            "200":{"description":"ok","content":{
                                "application/json":{"schema":{"type":"string"}}
                            }}
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let content = &ir.operations[0].responses[0].content[0];
        assert!(content.item_schema.is_none());
    }

    #[test]
    fn schema_and_item_schema_together_emit_conflict_error() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x":{
                    "get":{
                        "operationId":"x",
                        "responses":{
                            "200":{"description":"ok","content":{
                                "application/json":{
                                    "schema":{"type":"string"},
                                    "itemSchema":{"type":"string"}
                                }
                            }}
                        }
                    }
                }
            }
        }"#;
        let out = parse_str(src).unwrap();
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::E_CONTENT_SCHEMA_CONFLICT));
    }

    #[test]
    fn additional_operations_walk_into_other_method() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/items":{
                    "get":{"operationId":"listItems","responses":{"204":{"description":"ok"}}},
                    "additionalOperations":{
                        "QUERY":{
                            "operationId":"queryItems",
                            "responses":{"204":{"description":"ok"}}
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let query_op = ir
            .operations
            .iter()
            .find(|o| o.id == "queryItems")
            .expect("queryItems present");
        assert_eq!(query_op.method, forge_ir::HttpMethod::Other("QUERY".into()));
        // Standard method untouched.
        let list_op = ir.operations.iter().find(|o| o.id == "listItems").unwrap();
        assert_eq!(list_op.method, forge_ir::HttpMethod::Get);
    }

    #[test]
    fn additional_operations_method_normalised_to_uppercase() {
        // RFC 7230 §3.1.1 method names are case-sensitive but
        // conventionally uppercase. The parser uppercases so generators
        // emitting `Method::from_bytes(b"...")` see a single canonical
        // form.
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x":{
                    "additionalOperations":{
                        "Query":{
                            "operationId":"qx",
                            "responses":{"204":{"description":"ok"}}
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(
            ir.operations[0].method,
            forge_ir::HttpMethod::Other("QUERY".into())
        );
    }

    #[test]
    fn http_method_as_str_returns_wire_form() {
        use forge_ir::HttpMethod as M;
        assert_eq!(M::Get.as_str(), "GET");
        assert_eq!(M::Patch.as_str(), "PATCH");
        assert_eq!(M::Other("QUERY".into()).as_str(), "QUERY");
    }

    #[test]
    fn schema_defaults_populate_named_type_and_property() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "PageSize":{"type":"integer","default":25},
                    "Pet":{
                        "type":"object",
                        "properties":{
                            "name":{"type":"string","default":"Rex"}
                        }
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let page_size = ir.types.iter().find(|t| t.id == "PageSize").unwrap();
        let r = page_size.default.unwrap() as usize;
        assert_eq!(ir.values[r], forge_ir::Value::Int { value: 25 });
        let pet = ir.types.iter().find(|t| t.id == "Pet").unwrap();
        let forge_ir::TypeDef::Object(pet_obj) = &pet.definition else {
            panic!("Pet should be object");
        };
        let name_prop = pet_obj
            .properties
            .iter()
            .find(|p| p.name == "name")
            .unwrap();
        let r = name_prop.default.unwrap() as usize;
        assert_eq!(ir.values[r], forge_ir::Value::s("Rex"));
    }

    #[test]
    fn schema_default_null_round_trips() {
        // JSON `null` is a scalar; round-trips as Value::Null in the pool.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Empty":{"type":"string","default":null}
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let empty = ir.types.iter().find(|t| t.id == "Empty").unwrap();
        let r = empty.default.unwrap() as usize;
        assert_eq!(ir.values[r], forge_ir::Value::Null);
    }

    #[test]
    fn empty_and_freeform_schemas_lower_to_any_not_object() {
        // A schema with no `type` — `{}` or an annotation-only schema — is the
        // JSON Schema "any" schema (equivalent to boolean `true`, JSON Schema
        // 2020-12 §4.3.2): it validates ANY instance, not just objects. It must
        // lower to `TypeDef::Any`, NOT to `{"type":"object"}` (an `Object` with
        // permissive additionalProperties), which would reject non-object
        // instances like strings or numbers.
        let src = r#"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "AnyVal":{},
                    "OpaqueDoc":{"description":"any JSON value"},
                    "RealObject":{"type":"object"}
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let def = |id: &str| {
            &ir.types
                .iter()
                .find(|t| t.id == id)
                .unwrap_or_else(|| panic!("missing type {id}"))
                .definition
        };
        assert!(
            matches!(def("AnyVal"), forge_ir::TypeDef::Any),
            "`{{}}` must lower to Any, got {:?}",
            def("AnyVal")
        );
        assert!(
            matches!(def("OpaqueDoc"), forge_ir::TypeDef::Any),
            "an annotation-only schema must lower to Any, got {:?}",
            def("OpaqueDoc")
        );
        // A schema that *does* declare `type: object` stays an object.
        assert!(
            matches!(def("RealObject"), forge_ir::TypeDef::Object(_)),
            "`{{\"type\":\"object\"}}` must stay Object, got {:?}",
            def("RealObject")
        );
    }

    #[test]
    fn schema_compound_default_survives_via_value_pool() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Cfg":{"type":"object","default":{"k":"v"}}
                }
            }
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        let cfg = ir.types.iter().find(|t| t.id == "Cfg").unwrap();
        let r = cfg.default.unwrap() as usize;
        let forge_ir::Value::Object { fields } = &ir.values[r] else {
            panic!("expected object default");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "k");
        assert_eq!(ir.values[fields[0].1 as usize], forge_ir::Value::s("v"));
    }

    #[test]
    fn tags_walk_into_structured_records() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "tags":[
                {
                    "name":"pets",
                    "summary":"S",
                    "description":"D",
                    "kind":"audience",
                    "externalDocs":{"url":"https://example.com"}
                },
                {"name":"cats","parent":"pets"}
            ],
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        // Sorted by name for determinism — cats before pets.
        assert_eq!(ir.tags[0].name, "cats");
        assert_eq!(ir.tags[0].parent.as_deref(), Some("pets"));
        assert_eq!(ir.tags[1].name, "pets");
        assert_eq!(ir.tags[1].summary.as_deref(), Some("S"));
        assert_eq!(ir.tags[1].description.as_deref(), Some("D"));
        assert_eq!(ir.tags[1].kind.as_deref(), Some("audience"));
        assert_eq!(
            ir.tags[1].external_docs.as_ref().unwrap().url,
            "https://example.com"
        );
    }

    #[test]
    fn tag_parent_dangling_drops_ref_keeps_tag() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "tags":[
                {"name":"cats","parent":"no-such-tag"}
            ],
            "paths":{}
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        assert_eq!(ir.tags.len(), 1);
        assert_eq!(ir.tags[0].name, "cats");
        // Parent reference dropped; the tag itself survives.
        assert!(ir.tags[0].parent.is_none());
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.code == diag::W_TAG_PARENT_DANGLING));
    }

    #[test]
    fn tags_extensions_round_trip() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "tags":[
                {"name":"pets","x-priority":5}
            ],
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let ext = &ir.tags[0].extensions;
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].0, "x-priority");
    }

    #[test]
    fn operation_servers_resolution_picks_most_specific() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "servers":[{"url":"https://root"}],
            "paths":{
                "/a":{
                    "get":{"operationId":"opA","responses":{"204":{"description":"ok"}}}
                },
                "/b":{
                    "servers":[{"url":"https://path-b"}],
                    "get":{"operationId":"opB","responses":{"204":{"description":"ok"}}},
                    "post":{
                        "operationId":"opC",
                        "servers":[{"url":"https://op-c"}],
                        "responses":{"204":{"description":"ok"}}
                    }
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let by_id = |id: &str| {
            ir.operations
                .iter()
                .find(|o| o.id == id)
                .unwrap_or_else(|| panic!("operation {id} not found"))
        };
        assert_eq!(by_id("opA").servers[0].url, "https://root");
        assert_eq!(by_id("opB").servers[0].url, "https://path-b");
        assert_eq!(by_id("opC").servers[0].url, "https://op-c");
    }

    #[test]
    fn operation_servers_empty_when_no_root_or_overrides() {
        // No `servers` anywhere — operation list stays empty rather than
        // synthesising a default URL.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/x":{"get":{"operationId":"x","responses":{"204":{"description":"ok"}}}}
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert!(ir.operations[0].servers.is_empty());
        assert!(ir.servers.is_empty());
    }

    #[test]
    fn operation_servers_explicit_empty_array_falls_through_to_root() {
        // OAS doesn't define semantics for an empty `servers: []` on an
        // operation. We treat it as "no override" and inherit the root —
        // matches the empty-vs-absent distinction we already do for
        // `security`. Document the choice in the test.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "servers":[{"url":"https://root"}],
            "paths":{
                "/x":{"get":{
                    "operationId":"x",
                    "servers":[],
                    "responses":{"204":{"description":"ok"}}
                }}
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.operations[0].servers[0].url, "https://root");
    }

    #[test]
    fn external_docs_absent_leaves_field_none() {
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert!(ir.external_docs.is_none());
    }

    #[test]
    fn info_license_name_only_round_trips() {
        // 3.0 specs commonly carry `license.name` with no `identifier`;
        // it must not be lost.
        let src = r#"{
            "openapi":"3.0.0",
            "info":{
                "title":"t",
                "version":"1",
                "license":{"name":"MIT"}
            },
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.info.license_name.as_deref(), Some("MIT"));
        assert!(ir.info.license_url.is_none());
        assert!(ir.info.license_identifier.is_none());
    }

    #[test]
    fn extensions_populate_on_every_specification_object() {
        use forge_ir::{SecuritySchemeKind, TypeDef};
        // Issue #112. One spec exercising every site that gained an
        // `extensions` field: info, server, server-variable, schema /
        // property, parameter, request body / body content / encoding,
        // response, security scheme, oauth2 flow.
        let src = r##"{
            "openapi":"3.0.3",
            "info":{
                "title":"t",
                "version":"1",
                "x-info":"info-ext"
            },
            "servers":[{
                "url":"https://api.example.com/{tier}",
                "x-server":"server-ext",
                "variables":{
                    "tier":{
                        "default":"v1",
                        "x-var":"var-ext"
                    }
                }
            }],
            "paths":{
                "/things":{
                    "post":{
                        "operationId":"create",
                        "parameters":[{
                            "name":"q",
                            "in":"query",
                            "schema":{"type":"string"},
                            "x-param":"param-ext"
                        }],
                        "requestBody":{
                            "x-body":"body-ext",
                            "content":{
                                "multipart/form-data":{
                                    "x-content":"content-ext",
                                    "schema":{"$ref":"#/components/schemas/Thing"},
                                    "encoding":{
                                        "name":{
                                            "contentType":"text/plain",
                                            "x-encoding":"encoding-ext"
                                        }
                                    }
                                }
                            }
                        },
                        "responses":{
                            "200":{
                                "description":"ok",
                                "x-response":"response-ext"
                            }
                        }
                    }
                }
            },
            "components":{
                "schemas":{
                    "Thing":{
                        "type":"object",
                        "x-schema":"schema-ext",
                        "properties":{
                            "name":{
                                "type":"string",
                                "x-prop":"prop-ext"
                            }
                        }
                    }
                },
                "securitySchemes":{
                    "OAuth":{
                        "type":"oauth2",
                        "x-scheme":"scheme-ext",
                        "flows":{
                            "authorizationCode":{
                                "authorizationUrl":"https://a",
                                "tokenUrl":"https://t",
                                "scopes":{},
                                "x-flow":"flow-ext"
                            }
                        }
                    }
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();

        // info
        let exts = &ir.info.extensions;
        assert!(
            exts.iter().any(|(k, _)| k == "x-info"),
            "info.extensions missing x-info: {exts:?}"
        );

        // server + server-variable
        let server = &ir.servers[0];
        assert!(server.extensions.iter().any(|(k, _)| k == "x-server"));
        let (_var_name, var) = &server.variables[0];
        assert!(var.extensions.iter().any(|(k, _)| k == "x-var"));

        // schema (NamedType) + property
        let thing = ir.types.iter().find(|t| t.id == "Thing").unwrap();
        assert!(thing.extensions.iter().any(|(k, _)| k == "x-schema"));
        let TypeDef::Object(obj) = &thing.definition else {
            panic!("expected object")
        };
        let name_prop = obj.properties.iter().find(|p| p.name == "name").unwrap();
        assert!(name_prop.extensions.iter().any(|(k, _)| k == "x-prop"));

        // operation pieces
        let op = &ir.operations[0];
        let p = &op.query_params[0];
        assert!(p.extensions.iter().any(|(k, _)| k == "x-param"));
        let body = op.request_body.as_ref().unwrap();
        assert!(body.extensions.iter().any(|(k, _)| k == "x-body"));
        let content = &body.content[0];
        assert!(content.extensions.iter().any(|(k, _)| k == "x-content"));
        let (_enc_name, enc) = &content.encoding[0];
        assert!(enc.extensions.iter().any(|(k, _)| k == "x-encoding"));
        let resp = &op.responses[0];
        assert!(resp.extensions.iter().any(|(k, _)| k == "x-response"));

        // security scheme + oauth2 flow
        let scheme = ir
            .security_schemes
            .iter()
            .find(|s| s.id == "OAuth")
            .unwrap();
        assert!(scheme.extensions.iter().any(|(k, _)| k == "x-scheme"));
        let SecuritySchemeKind::Oauth2(o) = &scheme.kind else {
            panic!("expected oauth2");
        };
        let flow = &o.flows[0];
        assert!(flow.extensions.iter().any(|(k, _)| k == "x-flow"));
    }

    #[test]
    fn compound_extensions_survive_via_value_pool() {
        // List / object `x-*` values now survive the WIT boundary via the
        // value pool (ADR-0007 amendment). The parser interns the array
        // into the pool and `info.extensions` references it by `ValueRef`.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{
                "title":"t",
                "version":"1",
                "x-array":[1,2,3]
            },
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let entry = ir
            .info
            .extensions
            .iter()
            .find(|(k, _)| k == "x-array")
            .expect("x-array extension survives");
        let r = entry.1 as usize;
        let forge_ir::Value::List { items } = &ir.values[r] else {
            panic!("expected list, got {:?}", ir.values[r]);
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn server_name_3_2_round_trips() {
        // OAS 3.2 added `Server.name` as a short label distinct from
        // `description`. Capture it verbatim.
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "servers":[
                {"url":"https://api.example.com","name":"production"},
                {"url":"https://staging.example.com"}
            ],
            "paths":{}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(ir.servers[0].name.as_deref(), Some("production"));
        assert!(ir.servers[1].name.is_none());
    }

    #[test]
    fn parameter_querystring_3_2_routes_to_new_bucket() {
        // OAS 3.2 added `in: querystring`. Should land on the new
        // `Operation.querystring_params` slot, not the regular
        // `query_params`.
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/search":{"get":{
                    "operationId":"search",
                    "parameters":[
                        {"name":"raw","in":"querystring","schema":{"type":"string"}}
                    ],
                    "responses":{"200":{"description":"ok"}}
                }}
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let op = &ir.operations[0];
        assert!(op.query_params.is_empty(), "must not land in query_params");
        assert_eq!(op.querystring_params.len(), 1);
        assert_eq!(op.querystring_params[0].name, "raw");
    }

    #[test]
    fn example_data_value_serialized_value_3_2() {
        // OAS 3.2 split `value` into `dataValue` (parsed) and
        // `serializedValue` (wire form). Both must round-trip.
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Thing":{
                        "type":"string",
                        "examples":[
                            {"summary":"alice","dataValue":"alice","serializedValue":"\"alice\""}
                        ]
                    }
                }
            }
        }"##;
        // Use parameter-level examples since schema-level `examples` array (plural)
        // is in a different code path; here exercise the parameter / media-type path.
        let src2 = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/thing":{"post":{
                    "operationId":"create",
                    "requestBody":{"content":{"application/json":{
                        "schema":{"type":"string"},
                        "examples":{
                            "alice":{"dataValue":"alice","serializedValue":"\"alice\""}
                        }
                    }}},
                    "responses":{"200":{"description":"ok"}}
                }}
            }
        }"##;
        let _ = src; // schema-level examples (plural) handled separately; smoke-load
        let ir = parse_str(src2).unwrap().spec.unwrap();
        let body = ir.operations[0].request_body.as_ref().unwrap();
        let example = &body.content[0].examples[0].1;
        let r = example.data_value.unwrap() as usize;
        assert_eq!(ir.values[r], forge_ir::Value::s("alice"));
        assert_eq!(example.serialized_value.as_deref(), Some("\"alice\""));
    }

    #[test]
    fn xml_text_ordered_3_2() {
        // OAS 3.2 added `text` and `ordered` flags to the XML Object.
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "schemas":{
                    "Title":{"type":"string","xml":{"text":true}},
                    "Steps":{"type":"array","items":{"type":"string"},"xml":{"wrapped":true,"ordered":true}}
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let title = ir.types.iter().find(|t| t.id == "Title").unwrap();
        let title_xml = title.xml.as_ref().unwrap();
        assert!(title_xml.text);
        assert!(!title_xml.ordered);

        let steps = ir.types.iter().find(|t| t.id == "Steps").unwrap();
        let steps_xml = steps.xml.as_ref().unwrap();
        assert!(steps_xml.ordered);
        assert!(!steps_xml.text);
    }

    #[test]
    fn mutual_tls_security_scheme_round_trips() {
        use forge_ir::SecuritySchemeKind;
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"securitySchemes":{
                "mtls":{"type":"mutualTLS","description":"client-cert auth"}
            }}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let scheme = &ir.security_schemes[0];
        assert_eq!(scheme.id, "mtls");
        assert!(matches!(scheme.kind, SecuritySchemeKind::MutualTls));
        assert_eq!(scheme.description.as_deref(), Some("client-cert auth"));
    }

    #[test]
    fn oauth2_all_four_flows_succeed() {
        use forge_ir::{OAuth2FlowKind, SecuritySchemeKind};
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"securitySchemes":{
                "auth":{"type":"oauth2","flows":{
                    "implicit":{
                        "authorizationUrl":"https://a/auth",
                        "scopes":{"read":"r"}
                    },
                    "password":{
                        "tokenUrl":"https://a/token",
                        "scopes":{"read":"r"}
                    },
                    "clientCredentials":{
                        "tokenUrl":"https://a/token",
                        "scopes":{"read":"r"}
                    },
                    "authorizationCode":{
                        "authorizationUrl":"https://a/auth",
                        "tokenUrl":"https://a/token",
                        "scopes":{"read":"r"}
                    }
                }}
            }}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let scheme = &ir.security_schemes[0];
        let SecuritySchemeKind::Oauth2(o) = &scheme.kind else {
            panic!("expected oauth2 kind");
        };
        assert_eq!(o.flows.len(), 4, "all four flows surface");
        let kinds: Vec<OAuth2FlowKind> = o.flows.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&OAuth2FlowKind::Implicit));
        assert!(kinds.contains(&OAuth2FlowKind::Password));
        assert!(kinds.contains(&OAuth2FlowKind::ClientCredentials));
        assert!(kinds.contains(&OAuth2FlowKind::AuthorizationCode));
    }

    #[test]
    fn oauth2_missing_required_url_errors() {
        // `password` flow requires `tokenUrl`; absence emits
        // `parser/E-OAUTH2-MISSING-URL`.
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"securitySchemes":{
                "auth":{"type":"oauth2","flows":{
                    "password":{"scopes":{"read":"r"}}
                }}
            }}
        }"#;
        let out = parse_str(src).unwrap();
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diag::E_OAUTH2_MISSING_URL),
            "expected E-OAUTH2-MISSING-URL"
        );
    }

    #[test]
    fn content_encoding_keywords_round_trip() {
        // JSON Schema 2020-12 / OAS 3.2 contentEncoding +
        // contentMediaType + contentSchema land on
        // PrimitiveConstraints. contentSchema's body lifts into the
        // type pool under a `<owner>_content_schema` id.
        use forge_ir::TypeDef;
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "Avatar":{
                    "type":"string",
                    "contentEncoding":"base64",
                    "contentMediaType":"image/png"
                },
                "Embedded":{
                    "type":"string",
                    "contentMediaType":"application/json",
                    "contentSchema":{
                        "type":"object",
                        "properties":{"id":{"type":"string"}}
                    }
                }
            }}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();

        let avatar = ir.types.iter().find(|t| t.id == "Avatar").unwrap();
        let TypeDef::Primitive(p) = &avatar.definition else {
            panic!("expected primitive");
        };
        assert_eq!(p.constraints.content_encoding.as_deref(), Some("base64"));
        assert_eq!(
            p.constraints.content_media_type.as_deref(),
            Some("image/png")
        );
        assert!(p.constraints.content_schema.is_none());

        let embedded = ir.types.iter().find(|t| t.id == "Embedded").unwrap();
        let TypeDef::Primitive(p) = &embedded.definition else {
            panic!("expected primitive");
        };
        assert_eq!(
            p.constraints.content_media_type.as_deref(),
            Some("application/json")
        );
        let cs_ref = p
            .constraints
            .content_schema
            .as_deref()
            .expect("content_schema set");
        // The decoded payload's type must be reachable in the type pool.
        assert!(ir.types.iter().any(|t| t.id == cs_ref));
    }

    #[test]
    fn components_media_types_pool_resolves_refs() {
        // OAS 3.2 `components.mediaTypes` — `$ref` from `requestBody.content.<media>`
        // and `response.content.<media>` resolves through the pool.
        // Unused entries warn with `parser/W-COMPONENT-MEDIA-TYPE-UNUSED`.
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{
                "/things":{"post":{
                    "operationId":"create",
                    "requestBody":{"content":{
                        "application/json":{"$ref":"#/components/mediaTypes/ThingJson"}
                    }},
                    "responses":{"204":{"description":"ok"}}
                }}
            },
            "components":{
                "schemas":{
                    "Thing":{"type":"object","properties":{"id":{"type":"string"}}}
                },
                "mediaTypes":{
                    "ThingJson":{"schema":{"$ref":"#/components/schemas/Thing"}},
                    "Unused":{"schema":{"type":"string"}}
                }
            }
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.unwrap();
        let body = ir.operations[0].request_body.as_ref().unwrap();
        // The ref should resolve and the type should point to Thing.
        assert_eq!(body.content[0].r#type, "Thing");
        // Unused entry warns.
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diag::W_COMPONENT_MEDIA_TYPE_UNUSED
                    && d.message.contains("Unused")),
            "expected W-COMPONENT-MEDIA-TYPE-UNUSED for `Unused`"
        );
        // ThingJson was referenced — must NOT warn.
        assert!(
            !out.diagnostics
                .iter()
                .any(|d| d.code == diag::W_COMPONENT_MEDIA_TYPE_UNUSED
                    && d.message.contains("ThingJson")),
            "ThingJson is referenced; should not warn"
        );
    }

    #[test]
    fn json_schema_deferred_keywords_warn_not_error() {
        // #144: deferred 2020-12 keywords (dependentRequired,
        // dependentSchemas, unevaluatedProperties, $dynamicRef,
        // $dynamicAnchor) used to error and reject the whole spec.
        // Now they warn and the rest of the schema parses.
        let src = r#"{
            "openapi":"3.1.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "Bad":{
                    "type":"object",
                    "dependentRequired":{"a":["b"]},
                    "unevaluatedProperties":false,
                    "properties":{"id":{"type":"string"}}
                }
            }}
        }"#;
        let out = parse_str(src).unwrap();
        let ir = out.spec.expect("spec parses despite deferred keywords");
        // The schema landed in the type pool — `Bad` exists with its
        // declared `id` property.
        assert!(ir.types.iter().any(|t| t.id == "Bad"));
        // Both deferred keywords surfaced as warnings, not errors.
        let warns: Vec<&str> = out
            .diagnostics
            .iter()
            .filter(|d| d.severity == forge_ir::Severity::Warning)
            .map(|d| d.code.as_str())
            .collect();
        assert!(
            warns.contains(&diag::W_DEPENDENT_REQUIRED_DROPPED),
            "expected W-DEPENDENT-REQUIRED-DROPPED, got {warns:?}"
        );
        assert!(
            warns.contains(&diag::W_UNEVALUATED_PROPERTIES_DROPPED),
            "expected W-UNEVALUATED-PROPERTIES-DROPPED, got {warns:?}"
        );
        // No errors — these used to reject the spec entirely.
        let errs: Vec<&str> = out
            .diagnostics
            .iter()
            .filter(|d| d.severity == forge_ir::Severity::Error)
            .map(|d| d.code.as_str())
            .collect();
        assert!(errs.is_empty(), "no errors expected, got {errs:?}");
    }

    #[test]
    fn root_json_schema_dialect_and_self_round_trip() {
        // #143, #147. Capture jsonSchemaDialect and $self verbatim.
        let src = r##"{
            "openapi":"3.2.0",
            "$self":"https://example.com/api.json",
            "jsonSchemaDialect":"https://json-schema.org/draft/2020-12/schema",
            "info":{"title":"t","version":"1"},
            "paths":{}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        assert_eq!(
            ir.json_schema_dialect.as_deref(),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(ir.self_url.as_deref(), Some("https://example.com/api.json"));
    }

    #[test]
    fn header_style_explode_round_trip() {
        // #145. Header object's serialization fields populate the IR
        // even though the spec fixes style to `simple`.
        use forge_ir::ParameterStyle;
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "description":"ok",
                    "headers":{
                        "X-Rate":{
                            "schema":{"type":"integer"},
                            "style":"simple",
                            "explode":true,
                            "allowReserved":false,
                            "allowEmptyValue":false
                        }
                    }
                }}
            }}}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let resp = &ir.operations[0].responses[0];
        let (_name, h) = &resp.headers[0];
        assert_eq!(h.style, Some(ParameterStyle::Simple));
        assert!(h.explode);
        assert!(!h.allow_reserved);
        assert!(!h.allow_empty_value);
    }

    #[test]
    fn ref_siblings_3_1_plus_merge_onto_target() {
        // #146 (covers #139 too): in 3.1+, sibling keywords on a `$ref`
        // override the resolved target's same-keyed fields. OAS 3.2
        // codifies `summary` / `description` on Reference Object.
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "description":"per-call override"
                }}
            }}},
            "components":{"responses":{
                "Shared":{
                    "description":"shared default",
                    "content":{"application/json":{"schema":{"type":"string"}}}
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let resp = &ir.operations[0].responses[0];
        assert_eq!(
            resp.description.as_deref(),
            Some("per-call override"),
            "the sibling `description` wins over the shared default"
        );
    }

    /// OAS 3.2 §4.23: Response now carries `summary` as well as
    /// `description`. A Reference Object's `summary` override should
    /// land on `Response.summary`.
    #[test]
    fn ref_summary_override_on_response_3_2() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "summary":"per-call summary",
                    "description":"per-call desc"
                }}
            }}},
            "components":{"responses":{
                "Shared":{
                    "summary":"shared summary",
                    "description":"shared desc"
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let resp = &ir.operations[0].responses[0];
        assert_eq!(resp.summary.as_deref(), Some("per-call summary"));
        assert_eq!(resp.description.as_deref(), Some("per-call desc"));
    }

    /// Parameter Object (§4.12) only carries `description`. The
    /// Reference Object's `summary` should silently have no effect —
    /// the target type doesn't read it. `description` overrides.
    #[test]
    fn ref_override_on_parameter_summary_has_no_effect() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x/{id}":{
                "parameters":[{
                    "$ref":"#/components/parameters/Id",
                    "summary":"override summary",
                    "description":"override desc"
                }],
                "get":{"operationId":"x","responses":{"200":{"description":"ok"}}}
            }},
            "components":{"parameters":{
                "Id":{"name":"id","in":"path","required":true,"schema":{"type":"string"},
                      "description":"shared desc"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let param = &ir.operations[0].path_params[0];
        // description on Parameter: ref-site value wins.
        assert_eq!(param.description.as_deref(), Some("override desc"));
        // summary on Parameter: OAS §4.12 doesn't define summary on
        // Parameter, so the IR has no slot for it — type-level
        // enforcement of "this field has no effect" per §4.23. The
        // override value lands nowhere visible.
    }

    /// Header Object (§4.21) only carries `description`. Same
    /// no-effect rule for `summary` as Parameter.
    #[test]
    fn ref_description_override_on_header() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "description":"ok",
                    "headers":{
                        "X-Trace":{
                            "$ref":"#/components/headers/Trace",
                            "description":"override"
                        }
                    }
                }}
            }}},
            "components":{"headers":{
                "Trace":{"schema":{"type":"string"},"description":"shared"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let (_n, h) = &ir.operations[0].responses[0].headers[0];
        assert_eq!(h.description.as_deref(), Some("override"));
    }

    /// Example Object (§4.19) carries `summary` and `description`.
    /// Both must override the target's values.
    #[test]
    fn ref_override_on_example_summary_and_description() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{
                "examples":{
                    "Shared":{"summary":"shared sum","description":"shared d","value":"v"}
                },
                "schemas":{
                    "Foo":{
                        "type":"string",
                        "examples":{
                            "ref":{
                                "$ref":"#/components/examples/Shared",
                                "summary":"call sum",
                                "description":"call d"
                            }
                        }
                    }
                }
            }
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        let ex = &foo.examples[0].1;
        assert_eq!(ex.summary.as_deref(), Some("call sum"));
        assert_eq!(ex.description.as_deref(), Some("call d"));
    }

    /// Link Object (§4.20) only carries `description`. The current
    /// Reference Object override path supplies it. Covered alongside
    /// the broader sibling-merge test, but called out explicitly.
    #[test]
    fn ref_description_override_on_link() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "description":"ok",
                    "links":{"next":{
                        "$ref":"#/components/links/Shared",
                        "description":"per-call link doc"
                    }}
                }}
            }}},
            "components":{"links":{
                "Shared":{"operationId":"x","description":"shared link doc"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let link = &ir.operations[0].responses[0].links[0].1;
        assert_eq!(link.description.as_deref(), Some("per-call link doc"));
    }

    /// RequestBody (§4.13) only carries `description`. Override
    /// applies via the Reference Object path.
    #[test]
    fn ref_description_override_on_request_body() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"post":{
                "operationId":"x",
                "requestBody":{
                    "$ref":"#/components/requestBodies/Shared",
                    "description":"per-call"
                },
                "responses":{"200":{"description":"ok"}}
            }}},
            "components":{"requestBodies":{
                "Shared":{
                    "description":"shared",
                    "content":{"application/json":{"schema":{"type":"string"}}}
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let body = ir.operations[0].request_body.as_ref().unwrap();
        assert_eq!(body.description.as_deref(), Some("per-call"));
    }

    /// SecurityScheme (§4.27) carries `description`. Reference Object
    /// override applies. Tests the `securitySchemes` ref path.
    #[test]
    fn ref_description_override_on_security_scheme() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"securitySchemes":{
                "Wrap":{
                    "$ref":"#/components/securitySchemes/Shared",
                    "description":"per-call"
                },
                "Shared":{"type":"http","scheme":"bearer","description":"shared"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let scheme = ir
            .security_schemes
            .iter()
            .find(|s| s.id == "Wrap")
            .expect("Wrap scheme present");
        assert_eq!(scheme.description.as_deref(), Some("per-call"));
    }

    /// OAS 3.2 §4.23: Reference Object "cannot be extended with
    /// additional properties, and any properties added SHALL be
    /// ignored." Verify the parser drops the extras and emits
    /// `W-REF-SIBLINGS-INVALID`.
    #[test]
    fn ref_with_invalid_siblings_warns_and_drops() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "parameters":[{
                    "$ref":"#/components/parameters/Id",
                    "description":"ok",
                    "required":false,
                    "deprecated":true
                }],
                "responses":{"200":{"description":"ok"}}
            }}},
            "components":{"parameters":{
                "Id":{"name":"id","in":"path","required":true,"schema":{"type":"string"}}
            }}
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.as_ref().unwrap();
        let param = &ir.operations[0].path_params[0];
        // `required` and `deprecated` on the ref site SHALL be
        // ignored; the target's values survive.
        assert!(param.required);
        assert!(!param.deprecated);
        // `description` on the ref site applies.
        assert_eq!(param.description.as_deref(), Some("ok"));
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diag::W_REF_SIBLINGS_INVALID),
            "expected W-REF-SIBLINGS-INVALID; got: {:?}",
            out.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// `x-*` extensions on a Reference Object are also invalid per
    /// §4.23 ("cannot be extended with additional properties"). Verify
    /// the warning fires and the extension does not propagate.
    #[test]
    fn ref_with_x_extension_sibling_warns() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "x-vendor":"yes"
                }}
            }}},
            "components":{"responses":{
                "Shared":{"description":"shared"}
            }}
        }"##;
        let out = parse_str(src).unwrap();
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diag::W_REF_SIBLINGS_INVALID),
            "expected W-REF-SIBLINGS-INVALID; got: {:?}",
            out.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// MediaType (§4.14) has no `description` / `summary` fields.
    /// A Reference Object pointing at a MediaType with override docs
    /// applies neither — the target type doesn't read those fields.
    /// (Tested via the `mediaTypes` components map, OAS 3.2.)
    #[test]
    fn ref_override_on_media_type_has_no_effect() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "description":"ok",
                    "content":{"application/json":{
                        "$ref":"#/components/mediaTypes/Shared",
                        "description":"ignored"
                    }}
                }}
            }}},
            "components":{"mediaTypes":{
                "Shared":{"schema":{"type":"string"}}
            }}
        }"##;
        let out = parse_str(src).unwrap();
        let ir = out.spec.as_ref().unwrap();
        let content = &ir.operations[0].responses[0].content[0];
        // MediaType (§4.14) has no `description` field per spec — the
        // IR's BodyContent has no description slot. The override
        // overlays the JSON but lands nowhere in the IR, matching
        // §4.23's "this field has no effect" clause at the type level.
        assert_eq!(content.media_type, "application/json");
        assert!(content.examples.is_empty());
    }

    /// Reference Object on the target side of a multi-hop chain
    /// (`A → B → final`) still gets its `summary`/`description`
    /// applied at the outermost site. Only the outermost siblings
    /// participate — intermediates are pure pass-through.
    #[test]
    fn ref_chain_only_outermost_siblings_apply() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Outer",
                    "description":"call-site"
                }}
            }}},
            "components":{"responses":{
                "Outer":{"$ref":"#/components/responses/Inner"},
                "Inner":{"description":"deepest"}
            }}
        }"##;
        let out = parse_str(src).unwrap();
        let resp = &out.spec.as_ref().unwrap().operations[0].responses[0];
        assert_eq!(resp.description.as_deref(), Some("call-site"));
    }

    /// OAS 3.0 dropped siblings silently from non-schema refs (the
    /// schema-side path emits W-REF-SIBLINGS-3-0 separately). Verify
    /// the override is not applied for 3.0 docs.
    #[test]
    fn ref_override_skipped_for_oas_3_0() {
        let src = r##"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "description":"would-be override"
                }}
            }}},
            "components":{"responses":{
                "Shared":{"description":"shared"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let resp = &ir.operations[0].responses[0];
        // 3.0: siblings are dropped, target's description survives.
        assert_eq!(resp.description.as_deref(), Some("shared"));
    }

    /// $ref with no siblings — basic happy path. Target's docs survive
    /// unchanged; no warnings emitted.
    #[test]
    fn ref_with_only_dollar_ref_no_overrides() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{"$ref":"#/components/responses/Shared"}}
            }}},
            "components":{"responses":{
                "Shared":{"description":"shared","summary":"shared sum"}
            }}
        }"##;
        let out = parse_str(src).unwrap();
        let resp = &out.spec.as_ref().unwrap().operations[0].responses[0];
        assert_eq!(resp.summary.as_deref(), Some("shared sum"));
        assert_eq!(resp.description.as_deref(), Some("shared"));
        assert!(
            !out.diagnostics
                .iter()
                .any(|d| d.code == diag::W_REF_SIBLINGS_INVALID),
            "no W-REF-SIBLINGS-INVALID for a bare $ref"
        );
    }

    /// $ref override of `summary` only — `description` from target
    /// survives unchanged. The two fields are independent.
    #[test]
    fn ref_summary_override_only_leaves_description_intact() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "summary":"call-site sum"
                }}
            }}},
            "components":{"responses":{
                "Shared":{"summary":"shared sum","description":"shared d"}
            }}
        }"##;
        let resp = &parse_str(src).unwrap().spec.unwrap().operations[0].responses[0];
        assert_eq!(resp.summary.as_deref(), Some("call-site sum"));
        assert_eq!(resp.description.as_deref(), Some("shared d"));
    }

    /// $ref override of `description` only — `summary` from target
    /// survives unchanged.
    #[test]
    fn ref_description_override_only_leaves_summary_intact() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "$ref":"#/components/responses/Shared",
                    "description":"call-site d"
                }}
            }}},
            "components":{"responses":{
                "Shared":{"summary":"shared sum","description":"shared d"}
            }}
        }"##;
        let resp = &parse_str(src).unwrap().spec.unwrap().operations[0].responses[0];
        assert_eq!(resp.summary.as_deref(), Some("shared sum"));
        assert_eq!(resp.description.as_deref(), Some("call-site d"));
    }

    // ---- Per-node strict spec doc-field population ----

    /// Operation `summary` populates independently from `description`.
    /// OAS 3.1 introduced summary as a distinct field; this verifies the
    /// two slots don't collapse.
    #[test]
    fn operation_summary_independent_of_description() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "summary":"short",
                "description":"long form",
                "responses":{"200":{"description":"ok"}}
            }}}
        }"#;
        let op = &parse_str(src).unwrap().spec.unwrap().operations[0];
        assert_eq!(op.summary.as_deref(), Some("short"));
        assert_eq!(op.description.as_deref(), Some("long form"));
    }

    /// PathItem-level `summary` falls back into `Operation.summary` when
    /// the operation has none. OAS §4.9.
    #[test]
    fn operation_summary_falls_back_from_path_item() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{
                "summary":"path-item sum",
                "description":"path-item desc",
                "get":{"operationId":"x","responses":{"200":{"description":"ok"}}}
            }}
        }"#;
        let op = &parse_str(src).unwrap().spec.unwrap().operations[0];
        assert_eq!(op.summary.as_deref(), Some("path-item sum"));
        assert_eq!(op.description.as_deref(), Some("path-item desc"));
    }

    /// PathItem-level fallback is per-field independent — operation
    /// `summary` overrides PathItem.summary, but PathItem.description
    /// still flows through when the operation has none.
    #[test]
    fn path_item_fallback_per_field_independent() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{
                "summary":"path-item sum",
                "description":"path-item desc",
                "get":{
                    "operationId":"x",
                    "summary":"op sum",
                    "responses":{"200":{"description":"ok"}}
                }
            }}
        }"#;
        let op = &parse_str(src).unwrap().spec.unwrap().operations[0];
        assert_eq!(op.summary.as_deref(), Some("op sum"));
        assert_eq!(op.description.as_deref(), Some("path-item desc"));
    }

    /// `Operation.deprecated` populates from the spec.
    #[test]
    fn operation_deprecated_populates() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "deprecated":true,
                "responses":{"200":{"description":"ok"}}
            }}}
        }"#;
        let op = &parse_str(src).unwrap().spec.unwrap().operations[0];
        assert!(op.deprecated);
    }

    /// Webhooks carry PathItem-level `summary` / `description` per OAS
    /// §4.9. Both fields populate independently.
    #[test]
    fn webhook_path_item_summary_and_description_populate() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "webhooks":{
                "newPet":{
                    "summary":"hook sum",
                    "description":"hook desc",
                    "post":{"operationId":"onNewPet","responses":{"200":{"description":"ok"}}}
                }
            }
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let webhook = ir.webhooks.iter().find(|w| w.name == "newPet").unwrap();
        assert_eq!(webhook.summary.as_deref(), Some("hook sum"));
        assert_eq!(webhook.description.as_deref(), Some("hook desc"));
    }

    /// OAS 3.2 added `summary` to Response Object. Verify it populates
    /// independently from `description` (which was already there).
    #[test]
    fn response_summary_3_2_populates() {
        let src = r#"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{"/x":{"get":{
                "operationId":"x",
                "responses":{"200":{
                    "summary":"short label",
                    "description":"long form"
                }}
            }}}
        }"#;
        let resp = &parse_str(src).unwrap().spec.unwrap().operations[0].responses[0];
        assert_eq!(resp.summary.as_deref(), Some("short label"));
        assert_eq!(resp.description.as_deref(), Some("long form"));
    }

    /// `Property.title` populates from each schema property's JSON
    /// Schema `title` field.
    #[test]
    fn property_title_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "User":{
                    "type":"object",
                    "properties":{
                        "id":{"type":"string","title":"User ID"}
                    }
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let user = ir.types.iter().find(|t| t.id == "User").unwrap();
        let forge_ir::TypeDef::Object(obj) = &user.definition else {
            panic!("expected object");
        };
        let id_prop = obj.properties.iter().find(|p| p.name == "id").unwrap();
        assert_eq!(id_prop.title.as_deref(), Some("User ID"));
    }

    /// `Property.external_docs` populates from the property schema's
    /// `externalDocs` block.
    #[test]
    fn property_external_docs_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "User":{
                    "type":"object",
                    "properties":{
                        "id":{
                            "type":"string",
                            "externalDocs":{"url":"https://example.com","description":"d"}
                        }
                    }
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let user = ir.types.iter().find(|t| t.id == "User").unwrap();
        let forge_ir::TypeDef::Object(obj) = &user.definition else {
            panic!("expected object");
        };
        let id_prop = obj.properties.iter().find(|p| p.name == "id").unwrap();
        let ed = id_prop.external_docs.as_ref().unwrap();
        assert_eq!(ed.url, "https://example.com");
        assert_eq!(ed.description.as_deref(), Some("d"));
    }

    /// `Property.examples` populates from per-property schema-level
    /// `example` (3.0 form, stored under `_default`).
    #[test]
    fn property_examples_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "User":{
                    "type":"object",
                    "properties":{
                        "id":{"type":"string","example":"42"}
                    }
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let user = ir.types.iter().find(|t| t.id == "User").unwrap();
        let forge_ir::TypeDef::Object(obj) = &user.definition else {
            panic!("expected object");
        };
        let id_prop = obj.properties.iter().find(|p| p.name == "id").unwrap();
        assert_eq!(id_prop.examples.len(), 1);
        assert_eq!(id_prop.examples[0].0, "_default");
    }

    /// `Property.deprecated` populates from JSON Schema 2020-12
    /// `deprecated`.
    #[test]
    fn property_deprecated_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "User":{
                    "type":"object",
                    "properties":{
                        "id":{"type":"string","deprecated":true}
                    }
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let user = ir.types.iter().find(|t| t.id == "User").unwrap();
        let forge_ir::TypeDef::Object(obj) = &user.definition else {
            panic!("expected object");
        };
        let id_prop = obj.properties.iter().find(|p| p.name == "id").unwrap();
        assert!(id_prop.deprecated);
    }

    /// `NamedType.deprecated` populates from JSON Schema 2020-12
    /// `deprecated` at the schema level.
    #[test]
    fn named_type_deprecated_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "Legacy":{"type":"string","deprecated":true}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let legacy = ir.types.iter().find(|t| t.id == "Legacy").unwrap();
        assert!(legacy.deprecated);
    }

    /// OAS 3.2 §4.27: SecurityScheme adds `deprecated`. Verify it
    /// populates.
    #[test]
    fn security_scheme_deprecated_3_2_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"securitySchemes":{
                "Old":{
                    "type":"http",
                    "scheme":"basic",
                    "description":"do not use",
                    "deprecated":true
                }
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let scheme = ir.security_schemes.iter().find(|s| s.id == "Old").unwrap();
        assert!(scheme.deprecated);
        assert_eq!(scheme.description.as_deref(), Some("do not use"));
    }

    /// `NamedType.title` populates separately from `description`.
    /// JSON Schema treats title as a short human label.
    #[test]
    fn named_type_title_populates() {
        let src = r##"{
            "openapi":"3.2.0",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "Foo":{"type":"string","title":"Foo Type","description":"long"}
            }}
        }"##;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let foo = ir.types.iter().find(|t| t.id == "Foo").unwrap();
        assert_eq!(foo.title.as_deref(), Some("Foo Type"));
        assert_eq!(foo.description.as_deref(), Some("long"));
    }

    #[test]
    fn primitive_kind_carries_only_jsonschema_type_values() {
        // #105: PrimitiveKind is exactly the JSON Schema `type`
        // keyword's leaf values. All format refinements land in
        // `format_extension` verbatim — including ones the IR used
        // to fold into rich kinds (`int32`, `date`, `email`, `byte`).
        use forge_ir::{PrimitiveKind, TypeDef};
        let src = r#"{
            "openapi":"3.0.3",
            "info":{"title":"t","version":"1"},
            "paths":{},
            "components":{"schemas":{
                "Plain":      {"type":"string"},
                "Stamp":      {"type":"string","format":"date-time"},
                "Mail":       {"type":"string","format":"email"},
                "Avatar":     {"type":"string","format":"byte"},
                "Tally":      {"type":"integer","format":"int32"},
                "Big":        {"type":"integer","format":"int64"},
                "Money":      {"type":"string","format":"decimal"},
                "Flag":       {"type":"boolean"}
            }}
        }"#;
        let ir = parse_str(src).unwrap().spec.unwrap();
        let prim = |id: &str| -> (PrimitiveKind, Option<String>) {
            let nt = ir.types.iter().find(|t| t.id == id).unwrap();
            let TypeDef::Primitive(p) = &nt.definition else {
                panic!("{id} not primitive");
            };
            (p.kind, p.constraints.format_extension.clone())
        };
        assert_eq!(prim("Plain"), (PrimitiveKind::String, None));
        assert_eq!(
            prim("Stamp"),
            (PrimitiveKind::String, Some("date-time".into()))
        );
        assert_eq!(prim("Mail"), (PrimitiveKind::String, Some("email".into())));
        assert_eq!(prim("Avatar"), (PrimitiveKind::String, Some("byte".into())));
        assert_eq!(
            prim("Tally"),
            (PrimitiveKind::Integer, Some("int32".into()))
        );
        assert_eq!(prim("Big"), (PrimitiveKind::Integer, Some("int64".into())));
        assert_eq!(
            prim("Money"),
            (PrimitiveKind::String, Some("decimal".into()))
        );
        assert_eq!(prim("Flag"), (PrimitiveKind::Bool, None));
    }
}
