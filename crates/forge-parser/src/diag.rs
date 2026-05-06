//! Diagnostic codes and builders used by the parser.
//!
//! All codes are namespaced under `parser/`. Stable identifiers; do not
//! rename without a CHANGELOG entry.

use forge_ir::{Diagnostic, Severity, SpecLocation};

// Errors --------------------------------------------------------------------

pub const E_UNSUPPORTED_VERSION: &str = "parser/E-UNSUPPORTED-VERSION";
pub const E_INVALID_TYPE: &str = "parser/E-INVALID-TYPE";
pub const E_MISSING_FIELD: &str = "parser/E-MISSING-FIELD";
pub const E_DUPLICATE_OPERATION_ID: &str = "parser/E-DUPLICATE-OPERATION-ID";
pub const E_EXTERNAL_REF: &str = "parser/E-EXTERNAL-REF";
pub const E_DANGLING_REF: &str = "parser/E-DANGLING-REF";
/// Hard error: a non-schema `$ref` (path-item, parameter, response,
/// etc.) forms a cycle. Schema cycles are legal recursion; structural
/// cycles aren't expressible as IR.
pub const E_CYCLIC_REF: &str = "parser/E-CYCLIC-REF";
pub const E_COMPOSITION_ALLOF: &str = "parser/E-COMPOSITION-ALLOF";
pub const E_COMPOSITION_NOT: &str = "parser/E-COMPOSITION-NOT";
// Note: `parser/E-SECURITY-SCHEME-OAUTH2` was retired when the parser
// learned to handle `oauth2 authorizationCode` (#27). Remaining flow
// rejections use the more specific codes below.
/// `oauth2` scheme is missing a required `authorizationUrl` / `tokenUrl`
/// on its `authorizationCode` flow. Per the OpenAPI 3.x spec both are
/// required for that flow.
pub const E_OAUTH2_MISSING_URL: &str = "parser/E-OAUTH2-MISSING-URL";
/// A `content.<media>` entry declares both `schema` and (3.2)
/// `itemSchema`. OAS 3.2 §4.7.5 specifies these are mutually
/// exclusive — the parser keeps `schema` and emits this error so the
/// spec author can pick one.
pub const E_CONTENT_SCHEMA_CONFLICT: &str = "parser/E-CONTENT-SCHEMA-CONFLICT";
/// An OAS Example Object declares both `value` and `externalValue`.
/// Per OAS §4.7.20 they are mutually exclusive. The parser keeps
/// `value` and emits this error so the spec author picks one.
pub const E_EXAMPLE_VALUE_CONFLICT: &str = "parser/E-EXAMPLE-VALUE-CONFLICT";
/// An OAS Link Object declares both `operationRef` and `operationId`.
/// Per OAS §4.7.21 they are mutually exclusive. The parser keeps
/// `operationRef` and emits this error so the spec author picks one.
pub const E_LINK_OP_CONFLICT: &str = "parser/E-LINK-OP-CONFLICT";
// 3.1+ / JSON Schema 2020-12 keywords the IR doesn't yet model.
// Previously hard errors; (#144) downgraded to warnings so the rest
// of the schema parses. The keyword is dropped from the IR — strict
// validators that need them must consult the source spec separately.
pub const W_DEPENDENT_REQUIRED_DROPPED: &str = "parser/W-DEPENDENT-REQUIRED-DROPPED";
pub const W_DEPENDENT_SCHEMAS_DROPPED: &str = "parser/W-DEPENDENT-SCHEMAS-DROPPED";
pub const W_UNEVALUATED_PROPERTIES_DROPPED: &str = "parser/W-UNEVALUATED-PROPERTIES-DROPPED";
pub const W_DYNAMIC_REF_DROPPED: &str = "parser/W-DYNAMIC-REF-DROPPED";
pub const W_DYNAMIC_ANCHOR_DROPPED: &str = "parser/W-DYNAMIC-ANCHOR-DROPPED";

// Warnings ------------------------------------------------------------------

pub const W_ALLOF_CONFLICT: &str = "parser/W-ALLOF-CONFLICT";
pub const W_DISCRIMINATOR_MAPPING_DANGLING: &str = "parser/W-DISCRIMINATOR-MAPPING-DANGLING";
pub const W_ENUM_VALUE_DROPPED: &str = "parser/W-ENUM-VALUE-DROPPED";
pub const W_UNKNOWN_SECURITY_SCHEME: &str = "parser/W-UNKNOWN-SECURITY-SCHEME";
pub const W_PARAM_STYLE_UNSUPPORTED: &str = "parser/W-PARAM-STYLE-UNSUPPORTED";
/// `allowEmptyValue: true` set on a non-`query` parameter. OAS only
/// permits the flag on query parameters; we drop it for other
/// locations and surface this warning so the spec author can fix it.
pub const W_PARAM_ALLOW_EMPTY_MISPLACED: &str = "parser/W-PARAM-ALLOW-EMPTY-MISPLACED";
/// OAS `externalDocs` block missing the required `url` field. The
/// block is dropped (rather than emitted with a stub URL) so generators
/// don't render bogus links.
pub const W_EXTERNAL_DOCS_NO_URL: &str = "parser/W-EXTERNAL-DOCS-NO-URL";
pub const W_RECURSIVE_TYPE: &str = "parser/W-RECURSIVE-TYPE";
/// User-declared schema id collides with a name reserved by the IR (e.g.
/// `null`, which is reserved for the [`forge_ir::TypeDef::Null`] singleton).
/// The user's schema is renamed by appending a numeric suffix.
pub const W_RESERVED_NAME: &str = "parser/W-RESERVED-NAME";
/// A tag's `parent` field references a name not declared in the
/// top-level `tags` array. The parser drops the parent ref (the tag
/// itself stays) so generators that render tag trees never see broken
/// nesting.
pub const W_TAG_PARENT_DANGLING: &str = "parser/W-TAG-PARENT-DANGLING";
/// A `components.pathItems.<Name>` entry was declared but never
/// `$ref`'d from `paths`, `webhooks`, or any callback. The
/// declaration is silently invisible to generators today; this
/// warning surfaces it so the spec author can either reference it
/// or remove it.
pub const W_COMPONENT_PATH_ITEM_UNUSED: &str = "parser/W-COMPONENT-PATH-ITEM-UNUSED";
/// A 3.2 `components.mediaTypes.<Name>` entry was declared but
/// never `$ref`'d from any request body / response content. Same
/// pattern as `W_COMPONENT_PATH_ITEM_UNUSED`.
pub const W_COMPONENT_MEDIA_TYPE_UNUSED: &str = "parser/W-COMPONENT-MEDIA-TYPE-UNUSED";
/// A schema declared `$ref` together with sibling keywords, but the
/// document is OAS 3.0 — which forbade siblings on `$ref`. The
/// parser drops the siblings (matches the resolved type's metadata)
/// and emits this warning so the spec author can clean up. OAS 3.1+
/// inherits JSON Schema 2020-12's allowance; 3.1+ specs do not get
/// this warning, though sibling-merging is not yet implemented
/// (tracked at #74 follow-up).
pub const W_REF_SIBLINGS_3_0: &str = "parser/W-REF-SIBLINGS-3-0";

// Builders ------------------------------------------------------------------

pub fn err(code: &str, message: impl Into<String>, location: SpecLocation) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: code.into(),
        message: message.into(),
        location: Some(location),
        related: vec![],
        suggested_fix: None,
    }
}

pub fn warn(code: &str, message: impl Into<String>, location: SpecLocation) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: code.into(),
        message: message.into(),
        location: Some(location),
        related: vec![],
        suggested_fix: None,
    }
}
