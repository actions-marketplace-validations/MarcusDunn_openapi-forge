//! Walk `paths.<path>.<method>` into `forge_ir::Operation`s.

use std::collections::HashSet;

use forge_ir::{
    Body, BodyContent, Encoding, HttpMethod, Operation, Parameter, ParameterStyle, Response,
    ResponseStatus,
};
use serde_json::Value as J;

use crate::ctx::Ctx;
use crate::diag;
use crate::pointer::Ptr;
use crate::schema::{parse_schema, NameHint};

/// Parameters bucketed by location. The `querystring` bucket is OAS 3.2
/// `in: querystring` — a parameter that maps to the entire querystring
/// (opaque pass-through), distinct from individual `in: query` params.
#[derive(Default)]
struct ParamBuckets {
    path: Vec<Parameter>,
    query: Vec<Parameter>,
    header: Vec<Parameter>,
    cookie: Vec<Parameter>,
    querystring: Vec<Parameter>,
}

const METHODS: &[(&str, HttpMethod)] = &[
    ("get", HttpMethod::Get),
    ("put", HttpMethod::Put),
    ("post", HttpMethod::Post),
    ("delete", HttpMethod::Delete),
    ("options", HttpMethod::Options),
    ("head", HttpMethod::Head),
    ("patch", HttpMethod::Patch),
    ("trace", HttpMethod::Trace),
];

/// Walk the top-level `paths` object. Operations are pushed onto
/// `ctx.operations`.
pub(crate) fn parse_paths(ctx: &mut Ctx, paths: &J, ptr: &mut Ptr) {
    let map = match paths {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "`paths` must be an object",
                ptr.loc(ctx.file),
            ));
            return;
        }
    };

    let mut seen_op_ids: HashSet<String> = HashSet::new();
    for (path, item) in map {
        ptr.with_token(path, |ptr| {
            let ops = parse_path_item_maybe_ref(ctx, path, item, ptr, &mut seen_op_ids);
            ctx.operations.extend(ops);
        });
    }
}

/// Walk a 3.1+ top-level `webhooks` map. Each entry is a path-item
/// object; the resulting operations land on `ctx.webhooks` as
/// `Webhook { name, operations }` records (issue #111 — the spec's
/// map key is the routing identifier and must survive). operationId
/// uniqueness is shared with `paths` so duplicate ids across the two
/// are still flagged.
pub(crate) fn parse_webhooks(ctx: &mut Ctx, webhooks: &J, ptr: &mut Ptr) {
    let map = match webhooks {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "`webhooks` must be an object",
                ptr.loc(ctx.file),
            ));
            return;
        }
    };
    let mut seen_op_ids: HashSet<String> = ctx.operations.iter().map(|o| o.id.clone()).collect();
    for (name, item) in map {
        ptr.with_token(name, |ptr| {
            // Webhook entries don't have a real URL path; the entry name
            // doubles as the path-template label so generators have
            // something readable to surface for the inner operations.
            let operations = parse_path_item_maybe_ref(ctx, name, item, ptr, &mut seen_op_ids);
            ctx.webhooks.push(forge_ir::Webhook {
                name: name.clone(),
                operations,
            });
        });
    }
    // Determinism: sort by name. Operations within a Webhook stay in
    // declared HTTP-method order (METHODS table).
    ctx.webhooks.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Wrapper around `parse_path_item` that resolves `{$ref: ...}` first.
/// Path-item refs cross documents (`paths/admin.json#/<encoded>`) and
/// stay within one (`#/components/pathItems/<name>`); both shapes work
/// because the underlying resolution is just a JSON-pointer walk.
fn parse_path_item_maybe_ref(
    ctx: &mut Ctx,
    path: &str,
    value: &J,
    ptr: &mut Ptr,
    seen: &mut HashSet<String>,
) -> Vec<Operation> {
    let mut out = Vec::new();
    crate::ref_walk::with_resolved_object(ctx, value, ptr, |ctx, resolved, ptr| {
        out = parse_path_item(ctx, path, resolved, ptr, seen);
        Some(())
    });
    out
}

/// Parse a path-item object into a list of operations. Caller decides
/// whether they go on `ctx.operations` (paths) or `ctx.webhooks` (3.1+
/// webhooks).
pub(crate) fn parse_path_item(
    ctx: &mut Ctx,
    path: &str,
    item: &J,
    ptr: &mut Ptr,
    seen: &mut HashSet<String>,
) -> Vec<Operation> {
    let mut out = Vec::new();
    let map = match item {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "path item must be an object",
                ptr.loc(ctx.file),
            ));
            return out;
        }
    };

    // OAS path items declare shared `parameters` once and `summary` /
    // `description` once for every operation under the path. Walk them
    // up-front and thread into each operation. The owner id used for
    // synthesised inline-schema names is derived from the path so two
    // operations under the same path resolve to the *same* type-pool
    // entry instead of duplicating per operation.
    let path_owner = format!("path_{}", crate::sanitize::ident(path));
    let path_item_params = parse_parameters(ctx, &path_owner, map, ptr);
    let path_item_summary = map.get("summary").and_then(J::as_str).map(String::from);
    let path_item_description = map.get("description").and_then(J::as_str).map(String::from);
    // OAS §4.8.10 servers inheritance: operation > path-item > root. We
    // resolve at parse time so generators read a single effective list
    // off `Operation.servers`.
    let path_item_servers = crate::parse_servers_array(ctx, map.get("servers"), ptr);

    for (m_str, method) in METHODS {
        let Some(op_value) = map.get(*m_str) else {
            continue;
        };
        ptr.with_token(m_str, |ptr| {
            if let Some(op) = parse_operation(
                ctx,
                path,
                method.clone(),
                op_value,
                ptr,
                &path_item_params,
                path_item_summary.as_deref(),
                path_item_description.as_deref(),
                &path_item_servers,
                seen,
            ) {
                if !seen.insert(op.id.clone()) {
                    ctx.push_diag(diag::err(
                        diag::E_DUPLICATE_OPERATION_ID,
                        format!("duplicate operationId `{}`", op.id),
                        ptr.loc(ctx.file),
                    ));
                } else {
                    out.push(op);
                }
            }
        });
    }
    // OpenAPI 3.2 `additionalOperations`: arbitrary HTTP-method names
    // (RFC 9205 `QUERY`, etc.). The IR's `HttpMethod::Other(String)`
    // carries the upper-cased verb verbatim. Generators that only know
    // the eight standard verbs match-and-reject (`StageError::Rejected`
    // with a structured diagnostic).
    if let Some(J::Object(extra)) = map.get("additionalOperations") {
        ptr.with_token("additionalOperations", |ptr| {
            for (extra_method, op_value) in extra {
                ptr.with_token(extra_method, |ptr| {
                    let method = HttpMethod::Other(extra_method.to_uppercase());
                    if let Some(op) = parse_operation(
                        ctx,
                        path,
                        method,
                        op_value,
                        ptr,
                        &path_item_params,
                        path_item_summary.as_deref(),
                        path_item_description.as_deref(),
                        &path_item_servers,
                        seen,
                    ) {
                        if !seen.insert(op.id.clone()) {
                            ctx.push_diag(diag::err(
                                diag::E_DUPLICATE_OPERATION_ID,
                                format!("duplicate operationId `{}`", op.id),
                                ptr.loc(ctx.file),
                            ));
                        } else {
                            out.push(op);
                        }
                    }
                });
            }
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn parse_operation(
    ctx: &mut Ctx,
    path: &str,
    method: HttpMethod,
    value: &J,
    ptr: &mut Ptr,
    path_item_params: &ParamBuckets,
    path_item_summary: Option<&str>,
    path_item_description: Option<&str>,
    path_item_servers: &[forge_ir::Server],
    seen_op_ids: &mut HashSet<String>,
) -> Option<Operation> {
    let map = match value {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "operation must be an object",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    let original_id = map.get("operationId").and_then(J::as_str).map(String::from);
    let op_id = match &original_id {
        Some(s) => crate::sanitize::ident(s),
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "operation is missing required `operationId`",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    let op_params = parse_parameters(ctx, &op_id, map, ptr);
    // Shared path-item parameters merge in *first*; operation-level
    // entries override by `(name, in)` per OAS §4.8.9.
    let path_params = merge_path_item_params(&path_item_params.path, op_params.path);
    let query_params = merge_path_item_params(&path_item_params.query, op_params.query);
    let header_params = merge_path_item_params(&path_item_params.header, op_params.header);
    let cookie_params = merge_path_item_params(&path_item_params.cookie, op_params.cookie);
    let querystring_params =
        merge_path_item_params(&path_item_params.querystring, op_params.querystring);

    let request_body = map.get("requestBody").and_then(|rb| {
        ptr.with_token("requestBody", |ptr| {
            parse_request_body(ctx, &op_id, rb, ptr)
        })
    });

    let responses = map
        .get("responses")
        .map(|r| ptr.with_token("responses", |ptr| parse_responses(ctx, &op_id, r, ptr)))
        .unwrap_or_default();

    // Operation-level `security` overrides the spec default; absence
    // inherits the top-level list. An explicit empty array is the OpenAPI
    // idiom for "this operation is unsecured" — preserve it.
    let security = match map.get("security") {
        Some(value) => ptr.with_token("security", |ptr| {
            crate::security::parse_requirements(ctx, value, ptr)
        }),
        None => ctx.default_security.clone(),
    };

    let tags: Vec<String> = match map.get("tags") {
        Some(J::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };

    let documentation = map
        .get("description")
        .and_then(J::as_str)
        .map(String::from)
        .or_else(|| map.get("summary").and_then(J::as_str).map(String::from))
        .or_else(|| path_item_description.map(String::from))
        .or_else(|| path_item_summary.map(String::from));

    let deprecated = map.get("deprecated").and_then(J::as_bool).unwrap_or(false);

    let extensions = collect_extensions(ctx, map, ptr);

    let external_docs = crate::parse_external_docs(ctx, map.get("externalDocs"), ptr);

    // OAS §4.8.10: operation `servers` overrides path-item `servers`,
    // which overrides root `servers`. Materialise the effective list
    // here so generators don't have to re-walk inheritance.
    let op_servers = crate::parse_servers_array(ctx, map.get("servers"), ptr);
    let servers = if !op_servers.is_empty() {
        op_servers
    } else if !path_item_servers.is_empty() {
        path_item_servers.to_vec()
    } else {
        ctx.servers.clone()
    };

    let callbacks = crate::parse_callbacks(ctx, map.get("callbacks"), ptr, seen_op_ids);

    Some(Operation {
        id: op_id,
        original_id,
        method,
        path_template: path.to_string(),
        path_params,
        query_params,
        header_params,
        cookie_params,
        querystring_params,
        request_body,
        responses,
        security,
        tags,
        documentation,
        deprecated,
        extensions,
        external_docs,
        servers,
        callbacks,
        location: Some(ptr.loc(ctx.file)),
    })
}

/// Merge a single location's worth of shared path-item parameters with
/// the operation-level entries. Operation entries that share the same
/// `name` win; otherwise the path-item entry survives. Declared order
/// is preserved (path-item first, then op-specific).
fn merge_path_item_params(shared: &[Parameter], op: Vec<Parameter>) -> Vec<Parameter> {
    let op_names: std::collections::HashSet<&str> = op.iter().map(|p| p.name.as_str()).collect();
    let mut out: Vec<Parameter> = Vec::with_capacity(shared.len() + op.len());
    for p in shared {
        if !op_names.contains(p.name.as_str()) {
            out.push(p.clone());
        }
    }
    out.extend(op);
    out
}

/// Parse the `parameters` array, splitting by `in:` location. Reads
/// `style` / `explode` per the OAS default table when not declared.
fn parse_parameters(
    ctx: &mut Ctx,
    op_id: &str,
    op_map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> ParamBuckets {
    let mut buckets = ParamBuckets::default();
    let Some(J::Array(items)) = op_map.get("parameters") else {
        return buckets;
    };
    ptr.with_token("parameters", |ptr| {
        for (i, raw) in items.iter().enumerate() {
            ptr.with_index(i, |ptr| {
                crate::ref_walk::with_resolved_object(ctx, raw, ptr, |ctx, resolved, ptr| {
                    parse_inline_parameter(ctx, op_id, resolved, ptr, &mut buckets);
                    Some(())
                });
            });
        }
    });
    buckets
}

fn parse_inline_parameter(
    ctx: &mut Ctx,
    op_id: &str,
    raw: &J,
    ptr: &mut Ptr,
    buckets: &mut ParamBuckets,
) {
    let Some(param_map) = raw.as_object() else {
        ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            "parameter must be an object",
            ptr.loc(ctx.file),
        ));
        return;
    };
    let Some(name) = param_map.get("name").and_then(J::as_str) else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "parameter is missing `name`",
            ptr.loc(ctx.file),
        ));
        return;
    };
    let Some(loc) = param_map.get("in").and_then(J::as_str) else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "parameter is missing `in`",
            ptr.loc(ctx.file),
        ));
        return;
    };
    let Some(p) = build_parameter(ctx, op_id, name, loc, param_map, ptr) else {
        return;
    };
    match loc {
        "path" => buckets.path.push(p),
        "query" => buckets.query.push(p),
        "header" => buckets.header.push(p),
        "cookie" => buckets.cookie.push(p),
        "querystring" => buckets.querystring.push(p),
        other => ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            format!("unknown parameter location `{other}`"),
            ptr.loc(ctx.file),
        )),
    }
}

/// Construct one [`Parameter`] from an already-resolved param-shaped
/// object. `name` and `loc` are passed in because Header Objects (used
/// in `Response.headers` and `Encoding.headers`) carry neither inside
/// the value — the map key supplies the name, `loc` is implicitly
/// `"header"`. Operation parameters read both off the object itself.
fn build_parameter(
    ctx: &mut Ctx,
    owner_id: &str,
    name: &str,
    loc: &str,
    param_map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<Parameter> {
    let required = param_map
        .get("required")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let documentation = param_map
        .get("description")
        .and_then(J::as_str)
        .map(String::from);
    // OAS allows `schema` *or* `content.<media>.schema` (mutually
    // exclusive). The latter is for complex / non-trivial parameter
    // values. Try the inline form first, then fall back to content.
    let schema = match param_map.get("schema") {
        Some(s) => s,
        None => match param_map
            .get("content")
            .and_then(|c| c.as_object())
            .and_then(|m| m.values().next())
            .and_then(|first| first.get("schema"))
        {
            Some(s) => s,
            None => {
                ctx.push_diag(diag::err(
                    diag::E_MISSING_FIELD,
                    "parameter is missing `schema` or `content.<media>.schema`",
                    ptr.loc(ctx.file),
                ));
                return None;
            }
        },
    };
    let role = format!("param_{loc}_{name}");
    let role_san = crate::sanitize::ident(&role);
    let type_ref = ptr.with_token("schema", |ptr| {
        parse_schema(ctx, schema, ptr, NameHint::inline(owner_id, &role_san))
    })?;
    let (style, explode) = resolve_param_style(ctx, param_map, loc, ptr);
    let raw_allow_empty_value = param_map
        .get("allowEmptyValue")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let allow_empty_value = if raw_allow_empty_value && loc != "query" {
        // OAS restricts `allowEmptyValue` to query parameters. Drop the
        // flag for other locations and surface a warning so the spec
        // author can clean it up.
        ctx.push_diag(diag::warn(
            diag::W_PARAM_ALLOW_EMPTY_MISPLACED,
            format!(
                "parameter `{name}` declares `allowEmptyValue: true` in `{loc}`; \
                 OAS only permits this on query parameters. Dropping the flag."
            ),
            ptr.loc(ctx.file),
        ));
        false
    } else {
        raw_allow_empty_value
    };
    let allow_reserved = param_map
        .get("allowReserved")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let examples = crate::parse_examples(ctx, param_map, ptr);
    let extensions = collect_extensions(ctx, param_map, ptr);
    Some(Parameter {
        name: name.to_string(),
        r#type: type_ref,
        required,
        documentation,
        deprecated: param_map
            .get("deprecated")
            .and_then(J::as_bool)
            .unwrap_or(false),
        style: Some(style),
        explode,
        allow_empty_value,
        allow_reserved,
        examples,
        extensions,
        location: Some(ptr.loc(ctx.file)),
    })
}

/// Walk a `headers: { <name>: HeaderObject | $ref }` map. Used for
/// `Response.headers` and `Encoding.headers`. The OAS HeaderObject is
/// shaped like a Parameter minus `name` / `in` / `style` / `explode`
/// — the map key supplies the header name and OAS fixes serialization
/// at `simple`. We model it with the dedicated `Header` IR struct
/// rather than reusing `Parameter` (issue #108).
fn parse_header_map(
    ctx: &mut Ctx,
    owner_id: &str,
    value: Option<&J>,
    ptr: &mut Ptr,
) -> Vec<(String, forge_ir::Header)> {
    let Some(J::Object(entries)) = value else {
        return Vec::new();
    };
    let mut out: Vec<(String, forge_ir::Header)> = Vec::new();
    ptr.with_token("headers", |ptr| {
        for (header_name, raw) in entries {
            ptr.with_token(header_name, |ptr| {
                crate::ref_walk::with_resolved_object(ctx, raw, ptr, |ctx, resolved, ptr| {
                    let Some(map) = resolved.as_object() else {
                        ctx.push_diag(diag::err(
                            diag::E_INVALID_TYPE,
                            "header object must be an object",
                            ptr.loc(ctx.file),
                        ));
                        return None;
                    };
                    if let Some(h) = build_header(ctx, owner_id, header_name, map, ptr) {
                        out.push((header_name.clone(), h));
                    }
                    Some(())
                });
            });
        }
    });
    out
}

fn build_header(
    ctx: &mut Ctx,
    owner_id: &str,
    name: &str,
    header_map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<forge_ir::Header> {
    let required = header_map
        .get("required")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let deprecated = header_map
        .get("deprecated")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let documentation = header_map
        .get("description")
        .and_then(J::as_str)
        .map(String::from);
    let schema = header_map.get("schema").or_else(|| {
        header_map
            .get("content")
            .and_then(|c| c.as_object())
            .and_then(|m| m.values().next())
            .and_then(|first| first.get("schema"))
    })?;
    let role = format!("header_{name}");
    let role_san = crate::sanitize::ident(&role);
    let type_ref = ptr.with_token("schema", |ptr| {
        parse_schema(ctx, schema, ptr, NameHint::inline(owner_id, &role_san))
    })?;
    let examples = crate::parse_examples(ctx, header_map, ptr);
    // OAS Header inherits Parameter's serialization fields. The spec
    // fixes `style` to `simple`, but the IR captures whatever the
    // declaration says so spec-strict consumers can see it.
    let style = header_map
        .get("style")
        .and_then(J::as_str)
        .and_then(parameter_style_from_str);
    let explode = header_map
        .get("explode")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let allow_reserved = header_map
        .get("allowReserved")
        .and_then(J::as_bool)
        .unwrap_or(false);
    let allow_empty_value = header_map
        .get("allowEmptyValue")
        .and_then(J::as_bool)
        .unwrap_or(false);
    Some(forge_ir::Header {
        r#type: type_ref,
        required,
        deprecated,
        documentation,
        examples,
        style,
        explode,
        allow_reserved,
        allow_empty_value,
        location: Some(ptr.loc(ctx.file)),
    })
}

/// Resolve the `(style, explode)` pair for a parameter. Falls back to the
/// OAS default per location, and warns when an explicit style is paired
/// with a location that doesn't permit it.
fn resolve_param_style(
    ctx: &mut Ctx,
    param_map: &serde_json::Map<String, J>,
    loc: &str,
    ptr: &mut Ptr,
) -> (ParameterStyle, bool) {
    let declared = param_map.get("style").and_then(J::as_str);
    let style = match declared {
        Some(s) => match parameter_style_from_str(s) {
            Some(p) => p,
            None => {
                ctx.push_diag(diag::warn(
                    diag::W_PARAM_STYLE_UNSUPPORTED,
                    format!(
                        "unrecognised parameter style `{s}` on `{loc}` parameter; using \
                         the OAS default"
                    ),
                    ptr.loc(ctx.file),
                ));
                default_style(loc)
            }
        },
        None => default_style(loc),
    };
    // Default `explode` per OAS: true when style is `form`, false otherwise.
    let explode = param_map
        .get("explode")
        .and_then(J::as_bool)
        .unwrap_or(matches!(style, ParameterStyle::Form));
    (style, explode)
}

fn default_style(loc: &str) -> ParameterStyle {
    match loc {
        // `form` for query / cookie; `simple` for path / header (OAS 3.0 §4.7.5).
        "query" | "cookie" => ParameterStyle::Form,
        _ => ParameterStyle::Simple,
    }
}

fn parse_request_body(ctx: &mut Ctx, op_id: &str, value: &J, ptr: &mut Ptr) -> Option<Body> {
    let mut out: Option<Body> = None;
    crate::ref_walk::with_resolved_object(ctx, value, ptr, |ctx, resolved, ptr| {
        out = parse_inline_request_body(ctx, op_id, resolved, ptr);
        Some(())
    });
    out
}

fn parse_inline_request_body(ctx: &mut Ctx, op_id: &str, value: &J, ptr: &mut Ptr) -> Option<Body> {
    let map = value.as_object()?;
    let required = map.get("required").and_then(J::as_bool).unwrap_or(false);
    let documentation = map.get("description").and_then(J::as_str).map(String::from);

    let Some(J::Object(content)) = map.get("content") else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "requestBody is missing `content`",
            ptr.loc(ctx.file),
        ));
        return None;
    };
    if content.is_empty() {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "requestBody.content is empty",
            ptr.loc(ctx.file),
        ));
        return None;
    }

    let mut body_content = Vec::new();
    ptr.with_token("content", |ptr| {
        for (media_type, entry) in content {
            ptr.with_token(media_type, |ptr| {
                // Per OAS 3.2 the entry MAY be `$ref` into
                // `components.mediaTypes.<Name>`; resolve through the
                // shared walker. Inline entries pass through as-is.
                crate::ref_walk::with_resolved_object(ctx, entry, ptr, |ctx, resolved, ptr| {
                    let Some(entry_map) = resolved.as_object() else {
                        ctx.push_diag(diag::err(
                            diag::E_INVALID_TYPE,
                            "content entry must be an object",
                            ptr.loc(ctx.file),
                        ));
                        return Some(());
                    };
                    let Some((t, item_schema)) =
                        parse_content_schema(ctx, entry_map, op_id, "request_body", ptr)
                    else {
                        return Some(());
                    };
                    let encoding = parse_encoding_map(ctx, op_id, entry_map.get("encoding"), ptr);
                    let examples = crate::parse_examples(ctx, entry_map, ptr);
                    let extensions = collect_extensions(ctx, entry_map, ptr);
                    body_content.push(BodyContent {
                        media_type: media_type.clone(),
                        r#type: t,
                        encoding,
                        item_schema,
                        examples,
                        extensions,
                    });
                    Some(())
                });
            });
        }
    });

    if body_content.is_empty() {
        return None;
    }
    let extensions = collect_extensions(ctx, map, ptr);
    Some(Body {
        content: body_content,
        required,
        documentation,
        extensions,
    })
}

fn parse_responses(ctx: &mut Ctx, op_id: &str, value: &J, ptr: &mut Ptr) -> Vec<Response> {
    let Some(map) = value.as_object() else {
        ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            "`responses` must be an object",
            ptr.loc(ctx.file),
        ));
        return vec![];
    };
    let mut out = Vec::new();
    for (status_key, entry) in map {
        ptr.with_token(status_key, |ptr| {
            let Some(status) = parse_status(ctx, status_key, ptr) else {
                return;
            };
            crate::ref_walk::with_resolved_object(ctx, entry, ptr, |ctx, resolved, ptr| {
                if let Some(response) =
                    parse_inline_response(ctx, op_id, status_key, status, resolved, ptr)
                {
                    out.push(response);
                }
                Some(())
            });
        });
    }
    out
}

fn parse_inline_response(
    ctx: &mut Ctx,
    op_id: &str,
    status_key: &str,
    status: ResponseStatus,
    entry: &J,
    ptr: &mut Ptr,
) -> Option<Response> {
    let entry_map = match entry.as_object() {
        Some(m) => m,
        None => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "response must be an object",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    let documentation = entry_map
        .get("description")
        .and_then(J::as_str)
        .map(String::from);
    let mut content_vec = Vec::new();
    if let Some(J::Object(content)) = entry_map.get("content") {
        ptr.with_token("content", |ptr| {
            for (media_type, c_entry) in content {
                ptr.with_token(media_type, |ptr| {
                    crate::ref_walk::with_resolved_object(
                        ctx,
                        c_entry,
                        ptr,
                        |ctx, resolved, ptr| {
                            let Some(c_map) = resolved.as_object() else {
                                return Some(());
                            };
                            let role = format!("response_{}", sanitize_status_key(status_key));
                            let Some((t, item_schema)) =
                                parse_content_schema(ctx, c_map, op_id, &role, ptr)
                            else {
                                return Some(());
                            };
                            let examples = crate::parse_examples(ctx, c_map, ptr);
                            let extensions = collect_extensions(ctx, c_map, ptr);
                            content_vec.push(BodyContent {
                                media_type: media_type.clone(),
                                r#type: t,
                                encoding: vec![],
                                item_schema,
                                examples,
                                extensions,
                            });
                            Some(())
                        },
                    );
                });
            }
        });
    }
    let headers = parse_header_map(ctx, op_id, entry_map.get("headers"), ptr);
    let links = crate::parse_links(ctx, entry_map.get("links"), ptr);
    let extensions = collect_extensions(ctx, entry_map, ptr);
    Some(Response {
        status,
        content: content_vec,
        headers,
        documentation,
        links,
        extensions,
    })
}

fn sanitize_status_key(s: &str) -> String {
    crate::sanitize::ident(s)
}

/// Parse `schema` and (3.2) `itemSchema` off a `content.<media>` entry.
/// Returns `(type, item_schema)`:
/// - Both fields absent → diagnostic + None.
/// - `schema` only → `(type, None)`.
/// - `itemSchema` only → `(itemSchema, Some(itemSchema))` so generators
///   that don't model streaming see a usable type.
/// - Both present → `parser/E-CONTENT-SCHEMA-CONFLICT` (per OAS 3.2 the
///   two are mutually exclusive). The `schema` value wins so the rest
///   of the parse continues with a defined type.
fn parse_content_schema(
    ctx: &mut Ctx,
    entry_map: &serde_json::Map<String, J>,
    op_id: &str,
    role: &str,
    ptr: &mut Ptr,
) -> Option<(forge_ir::TypeRef, Option<forge_ir::TypeRef>)> {
    let schema = entry_map.get("schema");
    let item_schema_raw = entry_map.get("itemSchema");
    if schema.is_some() && item_schema_raw.is_some() {
        ctx.push_diag(diag::err(
            diag::E_CONTENT_SCHEMA_CONFLICT,
            "content entry declares both `schema` and `itemSchema`; OAS 3.2 §4.7.5 \
             requires they are mutually exclusive. Using `schema`.",
            ptr.loc(ctx.file),
        ));
    }
    if let Some(s) = schema {
        let t = ptr.with_token("schema", |ptr| {
            parse_schema(ctx, s, ptr, NameHint::inline(op_id, role))
        })?;
        return Some((t, None));
    }
    if let Some(is) = item_schema_raw {
        let item_role = format!("{role}_item");
        let t = ptr.with_token("itemSchema", |ptr| {
            parse_schema(ctx, is, ptr, NameHint::inline(op_id, &item_role))
        })?;
        return Some((t.clone(), Some(t)));
    }
    ctx.push_diag(diag::err(
        diag::E_MISSING_FIELD,
        "content entry is missing `schema` (or 3.2 `itemSchema`)",
        ptr.loc(ctx.file),
    ));
    None
}

fn parse_status(ctx: &mut Ctx, key: &str, ptr: &mut Ptr) -> Option<ResponseStatus> {
    if key.eq_ignore_ascii_case("default") {
        return Some(ResponseStatus::Default);
    }
    if let Ok(code) = key.parse::<u16>() {
        return Some(ResponseStatus::Explicit { code });
    }
    if key.len() == 3 && key.ends_with("XX") {
        if let Some(c) = key.chars().next().and_then(|d| d.to_digit(10)) {
            if (1..=5).contains(&c) {
                return Some(ResponseStatus::Range { class: c as u8 });
            }
        }
    }
    ctx.push_diag(diag::err(
        diag::E_INVALID_TYPE,
        format!("unrecognized response status `{key}`"),
        ptr.loc(ctx.file),
    ));
    None
}

/// Parse the per-property `encoding` map that may sit alongside a request
/// body schema. Used by `multipart/*` and `application/x-www-form-urlencoded`
/// to override the per-property content type or serialization style.
/// Properties not mentioned use the OAS default (form + explode for
/// urlencoded, mime-detection for multipart).
fn parse_encoding_map(
    ctx: &mut Ctx,
    op_id: &str,
    value: Option<&J>,
    ptr: &mut Ptr,
) -> Vec<(String, Encoding)> {
    let Some(J::Object(entries)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    ptr.with_token("encoding", |ptr| {
        for (prop, entry) in entries {
            ptr.with_token(prop, |ptr| {
                let Some(map) = entry.as_object() else {
                    ctx.push_diag(diag::err(
                        diag::E_INVALID_TYPE,
                        "encoding entry must be an object",
                        ptr.loc(ctx.file),
                    ));
                    return;
                };
                let content_type = map.get("contentType").and_then(J::as_str).map(String::from);
                let style = map
                    .get("style")
                    .and_then(J::as_str)
                    .and_then(parameter_style_from_str);
                let explode = map.get("explode").and_then(J::as_bool).unwrap_or(false);
                let allow_reserved = map
                    .get("allowReserved")
                    .and_then(J::as_bool)
                    .unwrap_or(false);
                let headers = parse_header_map(ctx, op_id, map.get("headers"), ptr);
                let extensions = collect_extensions(ctx, map, ptr);
                out.push((
                    prop.clone(),
                    Encoding {
                        content_type,
                        style,
                        explode,
                        allow_reserved,
                        headers,
                        extensions,
                    },
                ));
            });
        }
    });
    out
}

fn parameter_style_from_str(s: &str) -> Option<ParameterStyle> {
    Some(match s {
        "form" => ParameterStyle::Form,
        "simple" => ParameterStyle::Simple,
        "label" => ParameterStyle::Label,
        "matrix" => ParameterStyle::Matrix,
        "spaceDelimited" => ParameterStyle::SpaceDelimited,
        "pipeDelimited" => ParameterStyle::PipeDelimited,
        "deepObject" => ParameterStyle::DeepObject,
        _ => return None,
    })
}

pub(crate) fn collect_extensions(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    _ptr: &mut Ptr,
) -> Vec<(String, forge_ir::ValueRef)> {
    let mut out = Vec::new();
    for (k, v) in map {
        if !k.starts_with("x-") {
            continue;
        }
        let r = ctx.values.intern_json(v);
        out.push((k.clone(), r));
    }
    out
}
