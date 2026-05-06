//! Security-scheme + security-requirement walker.
//!
//! Scope: `apiKey` (header / query / cookie), `http` (`bearer`, `basic`),
//! `mutualTLS` (no payload), `oauth2` (all four flows: `implicit`,
//! `password`, `clientCredentials`, `authorizationCode`), and
//! `openIdConnect` (carries the discovery URL — clients perform
//! discovery themselves). Per-flow URL requirements are validated at
//! parse time; missing required URLs emit `parser/E-OAUTH2-MISSING-URL`.

use forge_ir::{
    ApiKeyLocation, ApiKeyScheme, OAuth2Flow, OAuth2FlowKind, OAuth2Scheme, SecurityRequirement,
    SecurityScheme, SecuritySchemeKind,
};
use serde_json::Value as J;

use crate::ctx::Ctx;
use crate::diag;
use crate::pointer::Ptr;

/// Walk `components.securitySchemes` into `Ir.security_schemes`.
pub(crate) fn walk_components(ctx: &mut Ctx, root: &serde_json::Map<String, J>, ptr: &mut Ptr) {
    let Some(J::Object(components)) = root.get("components") else {
        return;
    };
    let Some(J::Object(schemes)) = components.get("securitySchemes") else {
        return;
    };
    ptr.with_token("components", |ptr| {
        ptr.with_token("securitySchemes", |ptr| {
            for (name, value) in schemes {
                ptr.with_token(name, |ptr| {
                    let mut scheme: Option<SecurityScheme> = None;
                    crate::ref_walk::with_resolved_object(ctx, value, ptr, |ctx, resolved, ptr| {
                        scheme = parse_scheme(ctx, name, resolved, ptr);
                        Some(())
                    });
                    if let Some(s) = scheme {
                        ctx.security_schemes.push(s);
                    }
                });
            }
        });
    });
}

fn parse_scheme(ctx: &mut Ctx, name: &str, value: &J, ptr: &mut Ptr) -> Option<SecurityScheme> {
    let map = match value {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "security scheme must be an object",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    let ty = match map.get("type").and_then(J::as_str) {
        Some(t) => t,
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "security scheme is missing required `type`",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    let documentation = map.get("description").and_then(J::as_str).map(String::from);
    // OAS 3.2 added `deprecated` on Security Scheme. Default is false.
    let deprecated = map.get("deprecated").and_then(J::as_bool).unwrap_or(false);
    let extensions = crate::operations::collect_extensions(ctx, map, ptr);

    let kind = match ty {
        "apiKey" => parse_api_key(ctx, map, ptr)?,
        "http" => parse_http(ctx, map, ptr)?,
        "mutualTLS" => SecuritySchemeKind::MutualTls,
        "oauth2" => parse_oauth2(ctx, name, map, ptr)?,
        "openIdConnect" => parse_openid_connect(ctx, name, map, ptr)?,
        other => {
            ctx.push_diag(diag::warn(
                diag::W_UNKNOWN_SECURITY_SCHEME,
                format!("unknown security scheme type `{other}`; skipping `{name}`"),
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    Some(SecurityScheme {
        id: crate::sanitize::ident(name),
        kind,
        documentation,
        deprecated,
        extensions,
    })
}

fn parse_api_key(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<SecuritySchemeKind> {
    let Some(name) = map.get("name").and_then(J::as_str) else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "apiKey scheme is missing required `name`",
            ptr.loc(ctx.file),
        ));
        return None;
    };
    let location = match map.get("in").and_then(J::as_str) {
        Some("header") => ApiKeyLocation::Header,
        Some("query") => ApiKeyLocation::Query,
        Some("cookie") => ApiKeyLocation::Cookie,
        Some(other) => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                format!("apiKey scheme has unknown `in` value `{other}`"),
                ptr.loc(ctx.file),
            ));
            return None;
        }
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                "apiKey scheme is missing required `in`",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    Some(SecuritySchemeKind::ApiKey(ApiKeyScheme {
        name: name.to_string(),
        location,
    }))
}

fn parse_http(
    ctx: &mut Ctx,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<SecuritySchemeKind> {
    let Some(scheme) = map.get("scheme").and_then(J::as_str) else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            "http security scheme is missing required `scheme`",
            ptr.loc(ctx.file),
        ));
        return None;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" => {
            let bearer_format = map
                .get("bearerFormat")
                .and_then(J::as_str)
                .map(String::from);
            Some(SecuritySchemeKind::HttpBearer { bearer_format })
        }
        "basic" => Some(SecuritySchemeKind::HttpBasic),
        other => {
            ctx.push_diag(diag::warn(
                diag::W_UNKNOWN_SECURITY_SCHEME,
                format!(
                    "http security scheme `{other}` is not yet supported; only `basic` and \
                     `bearer` are recognised. Skipping."
                ),
                ptr.loc(ctx.file),
            ));
            None
        }
    }
}

fn parse_openid_connect(
    ctx: &mut Ctx,
    name: &str,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<SecuritySchemeKind> {
    // Per OAS, `openIdConnectUrl` is required. The IR carries the URL
    // verbatim — clients perform discovery against
    // `<url>/.well-known/openid-configuration` themselves; we do not
    // pre-fetch it.
    let url = match map.get("openIdConnectUrl").and_then(J::as_str) {
        Some(u) => u.to_string(),
        None => {
            ctx.push_diag(diag::err(
                diag::E_MISSING_FIELD,
                format!(
                    "security scheme `{name}` of type `openIdConnect` is missing required \
                     `openIdConnectUrl`"
                ),
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };
    Some(SecuritySchemeKind::OpenIdConnect { url })
}

fn parse_oauth2(
    ctx: &mut Ctx,
    name: &str,
    map: &serde_json::Map<String, J>,
    ptr: &mut Ptr,
) -> Option<SecuritySchemeKind> {
    let Some(J::Object(flows)) = map.get("flows") else {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            format!("oauth2 scheme `{name}` is missing required `flows`"),
            ptr.loc(ctx.file),
        ));
        return None;
    };

    let mut parsed_flows: Vec<OAuth2Flow> = Vec::new();
    ptr.with_token("flows", |ptr| {
        for (flow_name, flow_value) in flows {
            ptr.with_token(flow_name, |ptr| {
                let kind = match flow_name.as_str() {
                    "implicit" => OAuth2FlowKind::Implicit,
                    "password" => OAuth2FlowKind::Password,
                    "clientCredentials" => OAuth2FlowKind::ClientCredentials,
                    "authorizationCode" => OAuth2FlowKind::AuthorizationCode,
                    other => {
                        ctx.push_diag(diag::err(
                            diag::E_INVALID_TYPE,
                            format!("oauth2 scheme `{name}` declares unknown flow kind `{other}`"),
                            ptr.loc(ctx.file),
                        ));
                        return;
                    }
                };
                if let Some(flow) = parse_oauth2_flow(ctx, name, kind, flow_value, ptr) {
                    parsed_flows.push(flow);
                }
            });
        }
    });

    if parsed_flows.is_empty() {
        ctx.push_diag(diag::err(
            diag::E_MISSING_FIELD,
            format!("oauth2 scheme `{name}` declared no usable flow"),
            ptr.loc(ctx.file),
        ));
        return None;
    }

    Some(SecuritySchemeKind::Oauth2(OAuth2Scheme {
        flows: parsed_flows,
    }))
}

/// Parse one OAuth2 flow object. Validates per-flow URL requirements:
///   - `implicit`: `authorizationUrl` required.
///   - `password`: `tokenUrl` required.
///   - `clientCredentials`: `tokenUrl` required.
///   - `authorizationCode`: `authorizationUrl` + `tokenUrl` required.
///
/// Missing required URLs emit `parser/E-OAUTH2-MISSING-URL`.
fn parse_oauth2_flow(
    ctx: &mut Ctx,
    scheme_name: &str,
    kind: OAuth2FlowKind,
    value: &J,
    ptr: &mut Ptr,
) -> Option<OAuth2Flow> {
    let map = match value {
        J::Object(m) => m,
        _ => {
            ctx.push_diag(diag::err(
                diag::E_INVALID_TYPE,
                "oauth2 flow definition must be an object",
                ptr.loc(ctx.file),
            ));
            return None;
        }
    };

    let authorization_url = map
        .get("authorizationUrl")
        .and_then(J::as_str)
        .map(String::from);
    let token_url = map.get("tokenUrl").and_then(J::as_str).map(String::from);
    let refresh_url = map.get("refreshUrl").and_then(J::as_str).map(String::from);

    let (need_auth_url, need_token_url) = match kind {
        OAuth2FlowKind::Implicit => (true, false),
        OAuth2FlowKind::Password | OAuth2FlowKind::ClientCredentials => (false, true),
        OAuth2FlowKind::AuthorizationCode => (true, true),
    };
    let auth_missing = need_auth_url && authorization_url.is_none();
    let token_missing = need_token_url && token_url.is_none();
    if auth_missing || token_missing {
        let flow_name = match kind {
            OAuth2FlowKind::Implicit => "implicit",
            OAuth2FlowKind::Password => "password",
            OAuth2FlowKind::ClientCredentials => "clientCredentials",
            OAuth2FlowKind::AuthorizationCode => "authorizationCode",
        };
        let mut missing: Vec<&str> = Vec::new();
        if auth_missing {
            missing.push("authorizationUrl");
        }
        if token_missing {
            missing.push("tokenUrl");
        }
        ctx.push_diag(diag::err(
            diag::E_OAUTH2_MISSING_URL,
            format!(
                "oauth2 scheme `{scheme_name}` `{flow_name}` flow is missing required {}",
                missing.join(", ")
            ),
            ptr.loc(ctx.file),
        ));
        return None;
    }

    // `scopes` is required-but-may-be-empty per the spec. Iterate the
    // map deterministically by sorting keys; insertion order is
    // serde_json::Map's default but we sort for stable IR output.
    let mut scopes: Vec<(String, String)> = Vec::new();
    if let Some(J::Object(scope_map)) = map.get("scopes") {
        for (k, v) in scope_map {
            let desc = v.as_str().unwrap_or("").to_string();
            scopes.push((k.clone(), desc));
        }
        scopes.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let extensions = crate::operations::collect_extensions(ctx, map, ptr);

    Some(OAuth2Flow {
        kind,
        authorization_url,
        token_url,
        refresh_url,
        scopes,
        extensions,
    })
}

/// Parse a `security` requirement array. Each top-level entry is an
/// "any-of" alternative, an object whose keys are scheme ids. We collapse
/// each entry to a single `SecurityRequirement` (the common case); if an
/// entry declares multiple schemes (the "all-of within a single
/// alternative" form), we emit a `SecurityRequirement` for each scheme
/// and let the generator decide how to compose them.
pub(crate) fn parse_requirements(
    ctx: &mut Ctx,
    value: &J,
    ptr: &mut Ptr,
) -> Vec<SecurityRequirement> {
    let mut out = Vec::new();
    let Some(items) = value.as_array() else {
        ctx.push_diag(diag::err(
            diag::E_INVALID_TYPE,
            "`security` must be an array",
            ptr.loc(ctx.file),
        ));
        return out;
    };
    for (i, entry) in items.iter().enumerate() {
        ptr.with_index(i, |ptr| {
            let Some(map) = entry.as_object() else {
                ctx.push_diag(diag::err(
                    diag::E_INVALID_TYPE,
                    "security requirement entry must be an object",
                    ptr.loc(ctx.file),
                ));
                return;
            };
            for (scheme_id, scopes_value) in map {
                let scopes: Vec<String> = scopes_value
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(SecurityRequirement {
                    scheme_id: crate::sanitize::ident(scheme_id),
                    scopes,
                });
            }
        });
    }
    out
}
