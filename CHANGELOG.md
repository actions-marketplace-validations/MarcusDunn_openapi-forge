# Changelog

All notable changes to this project will be documented in this file. This
project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Pre-1.0, the IR is unstable. Every release that touches the IR carries an
`## IR` section enumerating what changed.

## [Unreleased]

## [0.1.7] - 2026-05-06

## [0.1.6] - 2026-05-06

## [0.1.5] - 2026-05-06

## [0.1.4] - 2026-05-06

## [0.1.3] - 2026-05-06

## [0.1.2] - 2026-05-06

### Changed — `PrimitiveKind` is now JSON Schema `type` values only (#105) [BREAKING]

`PrimitiveKind` shrank from 13 variants to 4, matching the JSON Schema `type` keyword exactly: `String`, `Integer`, `Number`, `Bool`. Every OAS `format` refinement (`int32`, `int64`, `float`, `double`, `date`, `date-time`, `uuid`, `byte`, `binary`, `email`, `password`, `decimal`, `iban`, …) now lands verbatim on `PrimitiveConstraints.format_extension`. Plugins decide whether to produce a richer target-language type based on the format string.

**Removed enum variants:** `Int32`, `Int64`, `Float32`, `Float64`, `Bytes`, `Date`, `DateTime`, `Uuid`, `Uri`, `Email`, `Password`. Renamed: `Int*`/`Float*` → `Integer`/`Number` (no width).

**Removed diagnostic:** `parser/W-UNKNOWN-FORMAT` — the parser no longer curates a registry of "known" formats, so there's nothing to be unknown about.

**Generator behaviour changes:**
- `generator-rust-reqwest`: integer with no format defaults to `i64` (was `i32`); `format: int32` produces `i32`. `format: byte`/`binary` produces `Vec<u8>`. Other string formats fold to `String`.
- `generator-typescript-fetch`: all integer/number kinds collapse to `number`. `format: byte`/`binary` produces `BinaryData`.
- `generator-typescript-cli`: same — `string` / `number` / `boolean`. `formatExtension` available for richer future generators.
- `generator-go-server`: integer with no format defaults to `int64` (was `string` via switch default); explicit `int32` produces `int32`. `format: byte`/`binary` produces `[]byte`.

**Migration:** spec authors don't need to change anything. Plugin authors that match on `PrimitiveKind::Date` / `Email` / `Bytes` / etc. need to switch to inspecting `format_extension`. External consumers reading the IR's JSON serialisation will see `"kind": "string"` / `"integer"` / `"number"` / `"bool"` only; format hints move under `constraints.format_extension`.

35 conformance fixtures regenerated. The `Wave 7 annotations & polish` entry below documents the complementary structural work that landed before this change.

### Added — Wave 7 annotations & polish (#143, #145, #146, #147)

Four small-to-medium gaps closed in one PR.

- **`info.jsonSchemaDialect`** (#143): captured verbatim on `Ir.json_schema_dialect`. The parser does not switch dialects based on it; declarations round-trip for plugins that care.
- **`Ir.$self`** (#147): root document's canonical URI (3.2). Captured verbatim on `Ir.self_url`. Full base-URI semantics for external-`$ref` resolution land separately under #93.
- **Header serialization fields** (#145): `Header` now carries `style`, `explode`, `allow_reserved`, `allow_empty_value` (previously dropped). The OAS spec fixes `style` to `simple`, but spec-strict consumers (validators, doc generators) need to see what was declared.
- **`$ref` sibling merging in 3.1+** (#146, also closes #139): non-schema `$ref` sites in 3.1+ merge sibling keywords onto the resolved target — so `{"$ref": "...", "description": "override"}` overrides the shared default. Covers OAS 3.2's Reference Object `summary` / `description` fields automatically. 3.0 specs continue to emit `parser/W-REF-SIBLINGS-3-0` for schema sibling drops; non-schema sibling-merging in 3.0 stays silent (3.0 forbade it). Schema-side sibling merging in 3.1+ is a separate follow-up.

WIT, bindgen, plugin-sdk, typescript-cli view-types updated. Three new conformance fixtures and four unit tests.

### Changed — JSON Schema 2020-12 deferred keywords downgraded from errors to warnings (#144) (BREAKING)

`dependentRequired`, `dependentSchemas`, `unevaluatedProperties`, `$dynamicRef`, and `$dynamicAnchor` previously emitted `parser/E-*` errors and rejected the whole spec. Now they warn and the rest of the schema parses through.

- New warning codes:
  - `parser/W-DEPENDENT-REQUIRED-DROPPED`
  - `parser/W-DEPENDENT-SCHEMAS-DROPPED`
  - `parser/W-UNEVALUATED-PROPERTIES-DROPPED`
  - `parser/W-DYNAMIC-REF-DROPPED`
  - `parser/W-DYNAMIC-ANCHOR-DROPPED`
- Removed error codes: `parser/E-DEPENDENT-REQUIRED`, `parser/E-DEPENDENT-SCHEMAS`, `parser/E-UNEVALUATED-PROPERTIES`, `parser/E-DYNAMIC-REF`, `parser/E-DYNAMIC-ANCHOR`. **Breaking** for callers that pattern-match on the old codes.
- The keywords are still dropped from the IR — strict validators that need them must consult the source spec separately. Unit test + renamed conformance fixtures (`v3_1-warn-*` instead of `v3_1-error-*`).

### Added — 3.2 `components.mediaTypes` reusable pool (#140)

A new component pool, parallel to `components.pathItems`. Specs can now `$ref` to `#/components/mediaTypes/<Name>` from request-body and response `content` entries, and the parser resolves through the same machinery as `pathItems`. Declared-but-unreferenced entries surface with `parser/W-COMPONENT-MEDIA-TYPE-UNUSED`.

The body-content walkers (request body and response) now wrap each media-type entry in `with_resolved_object`, so refs everywhere a media-type entry is expected get followed transparently. Inline entries are unaffected.

Conformance fixture `components-mediatypes-3-2/` and one unit test.

### Added — JSON Schema `contentEncoding` / `contentMediaType` / `contentSchema` (#141)

Three string-only schema keywords that describe encoded payloads (e.g. base64-encoded images, embedded-JSON strings). Previously dropped silently.

- `PrimitiveConstraints.content_encoding: Option<String>` — e.g. `"base64"`.
- `PrimitiveConstraints.content_media_type: Option<String>` — media type of the decoded payload (e.g. `"image/png"`).
- `PrimitiveConstraints.content_schema: Option<TypeRef>` (3.2) — schema for the decoded payload. The body lifts into the type pool under a `<owner>_content_schema` id.

WIT, bindgen, plugin-sdk, typescript-cli view-types updated. Conformance fixture `content-encoding-keywords/` and one unit test.

### Added — `mutualTLS` + all four OAuth2 flows

Closes #137, #138.

- `SecuritySchemeKind::MutualTls` (no payload) — OAS 3.0+ mTLS. Cert provisioning is left to the consumer's transport configuration; the IR carries the declaration so generators can surface it.
- OAuth2 flows beyond `authorizationCode` now parse: `implicit`, `password`, `clientCredentials`. Per-flow URL requirements validated at parse time (missing required URLs emit `parser/E-OAUTH2-MISSING-URL`). The `parser/W-OAUTH2-FLOW-SKIPPED` and `parser/W-OAUTH2-NO-SUPPORTED-FLOWS` warnings are removed — the parser no longer drops flows. typescript-fetch and rust-reqwest generators add `MutualTls` arms (no-op; mTLS is a transport concern).
- New conformance fixtures `security-mutual-tls/`, `security-oauth2-implicit/`, `security-oauth2-password/`, `security-oauth2-client-credentials/`. Removed the now-obsolete `security-oauth2-skipped-flows/` and `security-oauth2-mixed-flows/` fixtures (their behaviour was "drop with warning"; warnings are gone). Three new unit tests.

### Added — OAS 3.2 additive fields: `Server.name`, `in: querystring`, `Example.dataValue/serializedValue`, `XmlObject.text/ordered`

Closes #133, #134, #135, #136. Four 3.2-new fields the parser previously dropped silently.

- `Server.name` (#133) — short label distinct from `description`.
- Parameter `in: querystring` (#134) — bind to entire query string (opaque). Lands on new `Operation.querystring_params` slot, separate from `query_params`.
- `Example.dataValue` / `serializedValue` (#135) — split the 3.0/3.1 `value` into parsed-form and wire-form. Same scalar-only WIT policy as `value`.
- `XmlObject.text` / `ordered` (#136) — render value as element text content; declare array-element-order significance.

WIT, bindgen, plugin-sdk, typescript-cli view-types updated in lockstep. Conformance fixtures `server-name-3-2/`, `parameter-querystring-3-2/`, `example-data-serialized-3-2/`, `xml-text-ordered-3-2/` plus four unit tests.

### Added — Universal `x-*` extensions on every Specification Object

Closes #112. Until now only `Operation`, `Tag`, `Link`, `Callback`,
`XmlObject`, and `Discriminator` carried `x-*` extensions; every other
Specification Object dropped them silently. The IR now surfaces them
on every site the OpenAPI spec defines.

- WIT (`wit/ir.wit`) and `forge-ir`: new `extensions: list<tuple<string,
  value>>` field on `api-info`, `server`, `server-variable`, `named-type`
  (schema), `property`, `parameter`, `body`, `body-content`, `encoding`,
  `response`, `security-scheme`, and `oauth2-flow`. Same scalar-only
  policy as elsewhere — compound (list / object) extensions drop with
  `parser/W-EXTENSION-DROPPED` (ADR-0007).
- `forge-parser`: each parse site calls the existing
  `operations::collect_extensions` helper to populate the new fields.
  No behavioural change beyond extra field population.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new lists.
- `generator-typescript-cli` view-types: matching `extensions` arrays
  on every interface.
- Conformance fixture `extensions-universal/` exercises every site.
  Unit test `extensions_populate_on_every_specification_object`
  asserts each field is populated; `compound_extensions_drop_with_warning`
  covers the drop path.

### Changed — Webhook name surfaced as a routing key (BREAKING)

Closes #111. `Ir.webhooks: Vec<Operation>` dropped the spec's
`webhooks.<name>` map key — the routing identifier a webhook-handler
generator dispatches on. The IR now carries the name first-class.

- WIT (`wit/ir.wit`): `ir.webhooks: list<webhook>` (was
  `list<operation>`). New `webhook` record (`name`, `operations`).
  A single path item can hold multiple HTTP-method operations under
  one name, so the wrapper struct is more faithful than a flat tuple
  list.
- `forge-ir`: new `Webhook { name, operations }` struct.
  `Ir.webhooks: Vec<Webhook>` (was `Vec<Operation>`). **Breaking**
  for plugins that walk `ir.webhooks`.
- `forge-parser`: `parse_webhooks` now pushes one `Webhook` per
  spec-map key, with all of the path item's operations grouped
  underneath. operationId uniqueness is still global across paths /
  webhooks. The webhook list is sorted by name for determinism.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  record. Bindgen drops the operationId-uniqueness check on the
  webhook side (operations live inside the wrapper, but
  `validate_refs` walks them transitively).
- `generator-rust-reqwest`: `spec_uses_multipart` now flat-maps
  through `webhooks.iter().flat_map(|w| w.operations.iter())`.
  `generator-typescript-cli` view-types: new `Webhook` interface;
  `Ir.webhooks: Webhook[]`.
- Two unit tests cover (a) routing-name + multiple methods on one
  path item, and (b) name-sorted determinism.

### Changed — `MapType` collapsed into `ObjectType` (BREAKING)

Closes #109. `MapType` was a strict subset of `ObjectType` with
`additional_properties: Typed { … }` and empty `properties`. The IR
now uses one canonical spelling: an object schema with no declared
properties and a typed `additionalProperties` is the map shape.

- WIT (`wit/ir.wit`): `map-type` record removed; `type-def` loses
  the `%map(map-type)` arm.
- `forge-ir`: `MapType` and `TypeDef::Map` are deleted. **Breaking**
  for plugins that match exhaustively on `TypeDef`.
- `forge-parser`: nothing to change — pure-map schemas already
  produced `Object` with empty `properties` and `Typed` additional.
  As a side benefit, `minProperties` / `maxProperties` on map
  schemas now round-trip (the old `MapType` couldn't carry them).
- `forge-ir-bindgen` and `forge-plugin-sdk`: drop the `map_to` /
  `map_from` and the `Map` arms of `type_def_to` / `type_def_from`.
- Generators: new local `as_map(&ObjectType)` helper detects the
  canonical map shape (empty properties + Typed additional) and
  emits a type alias (`Foo = HashMap<String, T>` in Rust;
  `Foo = { [key: string]: T }` in TS) rather than a struct.
  Mixed shapes (declared properties + typed additional) keep the
  old struct rendering. `generator-typescript-cli` view-types
  drop the `MapType` interface.
- Two existing tests renamed and updated:
  `additional_properties_typed_renders_flatten_extras` (rust-reqwest)
  and `additional_properties_typed_renders_index_signature`
  (TS-fetch) now check for the type alias.

### Changed — `required` moved onto `Property` (BREAKING)

Closes #110. `ObjectType.required: Vec<String>` and
`Parameter.required: bool` represented the same logical bit two
different ways. The IR now uses the inline-flag spelling
everywhere — `Property.required: bool` mirrors `Parameter.required`.

- WIT (`wit/ir.wit`): `object-type.required` removed; `property`
  gains `required: bool`.
- `forge-ir`: `Property` gains `required: bool`. `ObjectType` loses
  `required`. **Breaking** for plugins that read object schemas.
- `forge-parser`: `parse_schema` walks the spec's `required` array
  once, then patches the corresponding `Property.required` flags
  before assembling the `ObjectType`. The `allOf` flattener
  (`normalize.rs`) lifts `required` out of merged parts the same
  way.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  shape; existing `object_to`/`object_from` lose the `required`
  field, `property_to`/`from` gain it.
- Generators: `generator-typescript-fetch` and
  `generator-rust-reqwest` switch from `o.required.contains(&prop.name)`
  to `prop.required`. `generator-typescript-cli` view-types
  follow.
- Existing fixtures regenerated — no semantic change beyond the
  field move.

### Changed — `Header` IR struct replaces `Parameter` for response/encoding headers (BREAKING)

Closes #108. `Response.headers` and `Encoding.headers` used to reuse
`Parameter`, which carried dead fields (`style` / `explode` are
fixed for headers per OAS §4.8.13), an overloaded `required` (the
response-header semantics differ from request-parameter), and a
duplicated name (the tuple key already carries it).

- WIT (`wit/ir.wit`): new `header` record
  (`type`, `required`, `deprecated`, `documentation`, `examples`,
  `location`). `encoding.headers` and `response.headers` now use
  `list<tuple<string, header>>`.
- `forge-ir`: new `Header` struct exported from the crate root.
  `Encoding.headers: Vec<(String, Header)>` and
  `Response.headers: Vec<(String, Header)>`. **Breaking** for
  out-of-tree plugins that read header lists.
- `forge-parser`: new `build_header` helper replaces the old
  reused-parameter path. `parse_header_map` returns the new shape;
  the inline-schema role uses `header_<name>` rather than
  `param_header_<name>` so type-pool ids don't collide with actual
  parameters of the same name.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  record at every layer.
- `generator-typescript-cli` view-types: new `Header` interface;
  `Response.headers` and `Encoding.headers` typed accordingly.
- Existing fixtures regenerated to use the new shape (no
  semantic change beyond the field rename / removal).
- One unit test pins the new shape.

### Added — `openIdConnect` security scheme parser

Closes #27 (parser side). The parser used to reject
`openIdConnect` security schemes with `parser/E-SECURITY-SCHEME-OIDC`
because the issue called for discovery via
`.well-known/openid-configuration`. In practice clients perform
discovery themselves; the IR just needs the URL.

- `forge-parser`: new `parse_openid_connect` walker. Reads the
  required `openIdConnectUrl` and produces
  `SecuritySchemeKind::OpenIdConnect { url }` (the IR shape was
  already present). Missing `openIdConnectUrl` falls through to
  `parser/E-MISSING-FIELD`. The retired diagnostic
  `parser/E-SECURITY-SCHEME-OIDC` is removed.
- New conformance fixture `security-openid-connect/`.
- Two unit tests cover the populated case and missing-`openIdConnectUrl`.

The TS-fetch generator branch listed in #27 is still pending; this
PR only ships the parser side.

### Added — `$ref` sibling-keys warning for OAS 3.0

Closes #74 (partial). OAS 3.0 forbade siblings on `$ref`; JSON Schema
2020-12 (which 3.1+ inherits) allows them. The parser used to silently
drop siblings on every spec.

- `forge-parser`: tracks `Ctx::is_oas_3_0` from `check_version` so the
  schema walker can pick the right diagnostic. Schemas declaring
  `$ref` plus non-`x-*` siblings on a 3.0 spec now emit
  `parser/W-REF-SIBLINGS-3-0` listing the dropped keys. `x-*`
  extensions (always legal alongside `$ref`) don't trigger the
  warning.
- 3.1+ specs do **not** emit the warning — siblings are legal there.
  Sibling-merging on the resolved type (so `description`,
  `nullable`, `readOnly`, etc. apply on top of the referenced
  schema) is not yet implemented; tracked as a follow-up to #74.
- New conformance fixtures `ref-siblings-3-0-warning/` (warns) and
  `ref-siblings-3-1-allowed/` (clean diagnostics).
- Three unit tests cover the 3.0 warning, the 3.1 silence, and the
  `x-*`-only case.

### Added — Unused `components.pathItems` warning

Closes #94. OAS 3.1+ added `components.pathItems`, a registry for
shared path items reusable from `paths.<path>` and (3.1+)
`webhooks.<name>`. The existing `$ref` machinery already resolves into
them lazily, but a path item declared and never referenced was
silently invisible.

- `forge-parser`: new `parser/W-COMPONENT-PATH-ITEM-UNUSED` warning
  fires for each `components.pathItems.<Name>` entry that wasn't
  `$ref`'d from `paths`, `webhooks`, or any callback. Implementation:
  `Ctx::referenced_component_path_items` records names whenever
  `with_resolved_object` resolves a fragment of the form
  `/components/pathItems/<name>` against the main spec; a final
  scan over the components map emits the warning for missing names.
- The unused entry's operations don't land in `Ir.operations` (since
  no `$ref` triggered the walk), so generators see exactly the
  surface the spec author meant to expose. The warning gives the
  spec author a useful pointer instead of leaving the declaration
  silently dead.
- New conformance fixture `components-pathitems-shared/` declares
  one referenced and one unreferenced path item.
- Three unit tests cover the referenced-path-yields-operation case,
  the orphan-warning case, and the webhook-reference-counts case.

Eager structural validation (walking each declaration up-front to
surface dangling refs / missing `operationId` even when never `$ref`'d)
is deferred — the issue calls for it but lazy walk-on-reference is
already correct for the IR. The unused warning addresses the silent-
data-loss concern from the issue.

### Added — `callbacks` on operations and components

Closes #88. OAS Callback Objects describe out-of-band requests the API
makes back to the caller (event-driven / webhook APIs). They live on
`operation.callbacks` and `components.callbacks`. The IR dropped them.

- WIT (`wit/ir.wit`): new `callback` record (`name`, `expression`,
  `operation-ids`, `extensions`). `operation` gains
  `callbacks: list<callback>`. WIT can't express direct
  operation→callback→operation recursion, so callbacks reference
  operations by id rather than embedding them.
- `forge-ir`: new `Callback` struct; `callbacks: Vec<Callback>` on
  `Operation`. Each entry pairs one `(name, expression)` with the
  ids of the operations the embedded path item declared. The
  operations themselves live in `Ir.operations` alongside the
  top-level paths — OAS operationId uniqueness is API-wide, so this
  is consistent with the spec.
- `forge-parser`: new `parse_callbacks` helper. Walks
  `operation.callbacks` and resolves `$ref` into
  `components.callbacks.<Name>`. Reuses `parse_path_item` for the
  embedded path item so callback operations get the same treatment
  as top-level paths (operationId dedup, params merging, body /
  response walking). `parse_operation` now threads the global
  `seen_op_ids` set through so callback operations share the dedup
  scope with top-level operations.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  record at every layer.
- `generator-typescript-cli` view-types: new `Callback` interface;
  `Operation` gains `callbacks: Callback[]`.
- New conformance fixture `callbacks/` exercises an inline callback
  (with embedded request body) and a `$ref`'d shared callback from
  `components.callbacks`.
- Two unit tests cover the inline + ref'd case and operationId
  dedup across the top-level / callback boundary.

### Added — Response `links` (HATEOAS-style follow-ups)

Closes #89. OAS Link Objects describe how to use response data to call
another operation. They live on `response.links` and
`components.links`. The IR dropped them.

- WIT (`wit/ir.wit`): new `link` record (`operation-ref`,
  `operation-id`, `parameters`, `request-body`, `description`,
  `server`, `extensions`). `response` gains
  `links: list<tuple<string, link>>`.
- `forge-ir`: new `Link` struct; `links: Vec<(String, Link)>` on
  `Response`. Serde defaults so existing IR JSON keeps deserializing.
- `forge-parser`: new `parse_links` helper called from
  `parse_inline_response`. `$ref` into `components.links.<Name>`
  resolves through the existing ref machinery. Compound runtime-
  expression values (`parameters.<name>` / `requestBody`) drop with
  the new `parser/W-LINK-VALUE-DROPPED` warning since
  `forge_ir::Value` is scalar-only at the WIT boundary. Links
  declaring both `operationRef` and `operationId` keep
  `operationRef` (per OAS §4.7.21 they're mutually exclusive) and
  emit the same warning.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  record at every layer.
- `generator-typescript-cli` view-types: new `Link` interface;
  `Response` gains `links: [string, Link][]`.
- New conformance fixture `response-links/` exercises an inline link
  (with parameters + description) and a `$ref`'d shared link from
  `components.links`.
- Three unit tests cover the inline + ref'd case, the
  `operationRef`-vs-`operationId` mutual exclusion, and compound-
  value drops.

### Added — Schema `xml` block (XML namespacing)

Closes #90. OAS Schema objects can carry an `xml` block governing how
the schema serializes to XML — element name override, namespace,
prefix, attribute-vs-element placement, array wrapping. Common in
finance and government APIs. The IR had no slot.

- WIT (`wit/ir.wit`): new `xml-object` record (`name`, `namespace`,
  `prefix`, `attribute`, `wrapped`, `extensions`). `named-type` gains
  `xml: option<xml-object>`.
- `forge-ir`: new `XmlObject` struct; `xml: Option<XmlObject>` on
  `NamedType`. Serde defaults so existing IR JSON keeps deserializing.
- `forge-parser`: new `parse_xml` helper called at every
  `NamedType` construction site (every `parse_*` schema variant in
  `schema.rs` and the `allOf`-flatten merger in `normalize.rs`).
  `attribute` and `wrapped` default to `false` per OAS. `x-*`
  extensions on the `xml` block survive via `collect_extensions`.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  record at every layer.
- `generator-typescript-cli` view-types: new `XmlObject` interface;
  `NamedType` gains the `xml` field.
- New conformance fixture `xml-namespacing/` exercises element rename,
  `namespace` + `prefix`, `attribute: true`, and `wrapped: true` on
  an array.
- Three unit tests cover the populated case (every field), `attribute`
  defaulting, and the absent-block case.

No in-tree generator currently emits XML clients; the IR carries the
data so a future XML-capable generator can consume it.

### Added — `example` / `examples` on parameters, media-types, and schemas

Closes #85. OAS examples are normative for SDK fixture generation,
doc generators, and contract testing. The IR dropped every example;
the parser never read the field.

- WIT (`wit/ir.wit`): new `example` record (`summary`, `description`,
  `value`, `external-value`). `parameter`, `body-content`, and
  `named-type` each gain `examples: list<tuple<string, example>>`.
- `forge-ir`: new `Example` struct; matching
  `examples: Vec<(String, Example)>` slots on `Parameter`,
  `BodyContent`, `NamedType`. Serde defaults so existing IR JSON
  keeps deserializing.
- `forge-parser`: new `parse_examples` helper walks both the 3.0
  single-form `example: <literal>` (stored under synthetic key
  `"_default"`) and the 3.1+ keyed `examples: { name: ExampleObject }`.
  `$ref` into `components.examples.<Name>` resolves through the
  existing ref machinery. Compound (object/array) `value` literals
  drop with the new `parser/W-EXAMPLE-DROPPED` warning since
  `forge_ir::Value` is scalar-only at the WIT boundary. Examples
  declaring both `value` and `externalValue` keep `value` (per OAS
  §4.7.20 they're mutually exclusive) and emit the same warning.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  records at every layer.
- `generator-typescript-cli` view-types: new `Example` interface;
  `Parameter`, `BodyContent`, `NamedType` gain `examples: [...]`.
- New conformance fixture `examples/` exercises all three placement
  sites plus a `$ref`'d shared example.
- Three unit tests cover the populated case across both sites,
  compound-value drops, and the `value` / `externalValue` mutual
  exclusion.

### Added — OAS 3.2 `mediaType.itemSchema` (sequence-of-items responses)

Closes #92. OAS 3.2 added `itemSchema` to Media Type Objects: it
describes the shape of *one item* in a sequence-of-items response
(JSON Lines, SSE event-stream, multipart/mixed) — distinct from
`schema`, which describes the whole stream. The IR had no slot, so
streaming generators had nothing to bind against.

- WIT (`wit/ir.wit`): `body-content` gains
  `item-schema: option<type-ref>`.
- `forge-ir::BodyContent`: new `item_schema: Option<TypeRef>`. Serde
  defaults so existing IR JSON keeps deserializing.
- `forge-parser`: new `parse_content_schema` helper handles all four
  cases (only `schema`, only `itemSchema`, both, neither). When only
  `itemSchema` is present, `BodyContent.type` is set to the item
  type so non-streaming generators see a usable type;
  `item_schema` is populated for streaming-aware generators. Both
  declared together emits new `parser/E-CONTENT-SCHEMA-CONFLICT`
  (per OAS 3.2 §4.7.5 they are mutually exclusive).
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  field at every layer.
- New conformance fixture `v3_2-item-schema-jsonl/` exercises an
  `application/jsonl` response with `itemSchema: { $ref: '#/components/schemas/Event' }`.
- Three unit tests in `forge-parser` cover the populated case, the
  schema-only case (item_schema stays None), and the mutual-exclusion
  conflict.

### Added — `HttpMethod::Other(String)` for OAS 3.2 `additionalOperations` (BREAKING)

Closes #91. OAS 3.2 lifts the closed HTTP-method set: path items can
declare operations under any verb (e.g. RFC 9205 `QUERY`) via
`paths.<path>.additionalOperations.<METHOD>`. The parser previously
walked these and dropped them with
`parser/W-ADDITIONAL-OPERATIONS-DROPPED` because the IR's `HttpMethod`
enum only knew the eight standard verbs.

- WIT (`wit/ir.wit`): `enum http-method` becomes `variant
  http-method` with a new `other(string)` arm.
- `forge-ir::HttpMethod`: gains `Other(String)`. **Breaking** for
  plugins that match exhaustively on `HttpMethod` — add an
  `Other(_) => …` arm. New `HttpMethod::as_str()` helper returns the
  uppercased wire form (`"GET"`, `"QUERY"`).
- `forge-parser`: walks `additionalOperations` the same way as the
  standard methods. The verb is upper-cased
  (`HttpMethod::Other("QUERY")`) so generators emitting
  `Method::from_bytes(b"...")` see a single canonical form. The old
  `parser/W-ADDITIONAL-OPERATIONS-DROPPED` is retired.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  variant.
- Generators: `generator-typescript-fetch` and
  `generator-rust-reqwest` now emit the verb verbatim
  (`request("QUERY")` and
  `Method::from_bytes(b"QUERY").unwrap()` respectively).
  `generator-debug-dump` and the strict / warn fixtures call
  `HttpMethod::as_str()` directly.
- Conformance: replaced the obsolete
  `v3_2-additional-operations-dropped/` (negative) with
  `v3_2-query-method/` (positive — `QUERY` produces a real
  `Operation` with `method: { other: "QUERY" }` in JSON).
- Three unit tests in `forge-parser` cover the populated case,
  case-normalisation, and the new `HttpMethod::as_str` helper.

### Added — Schema `default` values

Closes #77. JSON Schema's `default` is part of OAS 3.x and signals
the value a server should assume when a property is absent. The IR
dropped it entirely; SDK builders that auto-populate request bodies
had no way to ask "what's the spec's default?".

- WIT (`wit/ir.wit`): `named-type` and `property` each gain
  `default: option<value>`.
- `forge-ir`: matching `default: Option<Value>` slots on `NamedType`
  and `Property`. Serde defaults so existing IR JSON keeps
  deserializing without them.
- `forge-parser`: new `pub(crate) parse_default` helper used at every
  schema and property site. Compound defaults (objects/arrays) are
  dropped at the WIT boundary with the new
  `parser/W-DEFAULT-DROPPED` warning — `forge_ir::Value` is
  scalar-only per ADR-0007. Scalar defaults (`null`, `bool`, `int`,
  `float`, `string`) round-trip directly.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  fields at every layer.
- `generator-typescript-cli` view-types: `NamedType` and `Property`
  gain `default?: Value`.
- New conformance fixtures `schema-defaults/` (every scalar kind on
  both sites) and `default-compound-dropped/` (warns and drops). Three
  unit tests in `forge-parser` cover scalar populated, `null`-as-
  scalar, and compound-drops-with-warning.

### Added — Structured top-level `tags` list (incl. OAS 3.2 `parent` / `kind` / `summary`)

Closes #84. The OAS top-level `tags` array carries `description`,
`externalDocs`, and (in 3.2) `parent`, `kind`, `summary`. The parser
previously dropped the lot, emitting a `parser/W-TAG-METADATA-DROPPED`
warning for `parent` / `kind`. Now everything survives.

- WIT (`wit/ir.wit`): new `tag` record (`name`, `summary`,
  `description`, `external-docs`, `parent`, `kind`, `extensions`).
  `ir` gains `tags: list<tag>`.
- `forge-ir`: new `Tag` struct; `Ir.tags: Vec<Tag>` (sorted by name
  for determinism). `Operation.tags: Vec<String>` is unchanged — it
  references into `Ir.tags` by name.
- `forge-parser`: `scan_tag_metadata` replaced with `parse_tags`. New
  `parser/W-TAG-PARENT-DANGLING` warning fires when a tag's `parent`
  references a name not declared in the array; the parent ref is
  dropped (the tag itself stays). `x-*` extensions on tags survive
  via the existing `collect_extensions` path. The old
  `parser/W-TAG-METADATA-DROPPED` diagnostic is retired.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  records.
- `generator-typescript-cli` view-types: new `Tag` interface; `Ir`
  gains `tags: Tag[]`.
- Conformance: removed the obsolete `v3_2-tag-metadata-dropped/`
  negative fixture; replaced with `v3_2-tags-structured/` exercising
  every Tag field (incl. extensions and `parent` resolution) and
  `tag-parent-dangling/` pinning the warn-and-drop behaviour.
- Three unit tests in `forge-parser` cover the populated case
  (sorted output, all fields), the dangling-parent warning, and
  extension round-trip.

### Added — Per-operation and per-path-item `servers` overrides

Closes #87. OAS §4.8.10 lets path items and operations override the
root `servers` list. Common in real specs that mix CDN endpoints
with control-plane endpoints, or APIs with regional sub-paths. The
parser only read root-level `servers`; the rest dropped silently.

- WIT (`wit/ir.wit`): `operation` gains `servers: list<server>`.
- `forge-ir::Operation`: new `servers: Vec<Server>` field; serde
  defaults to empty so existing IR JSON keeps deserializing.
- `forge-parser`: new `pub(crate)` `parse_servers_array` helper used
  for root, path-item, and operation sites. The operations walker
  resolves OAS §4.8.10 inheritance — operation > path-item > root —
  and materialises the **effective** list onto `Operation.servers`
  so generators don't have to re-walk inheritance. An explicit empty
  `servers: []` array on a path item or operation is treated as "no
  override" and falls through to the next-level list (the OAS spec
  is silent on this; we match the inherit-on-absent behaviour we
  already use elsewhere).
- `forge-ir-bindgen` and `forge-plugin-sdk` round-trip the new
  field at every layer.
- `generator-typescript-cli` view-types: `Operation` gains
  `servers: Server[]`.
- New conformance fixture `servers-override/` exercises the three
  inheritance cases (root, path-item override, operation override)
  in one spec. Three unit tests in `forge-parser` cover the
  most-specific-wins rule, the empty-everywhere case, and the
  explicit-empty-array fallthrough.

### Added — `externalDocs` at root, operation, and schema sites

Closes #83. OAS lets every named entity carry an
`externalDocs: { description, url }` block. The IR had no slot
anywhere; the parser dropped them all.

- WIT (`wit/ir.wit`): new `external-docs` record. `ir`,
  `operation`, and `named-type` each gain an `external-docs:
  option<external-docs>` field.
- `forge-ir`: new `ExternalDocs` struct;
  `external_docs: Option<ExternalDocs>` slots on `Ir`, `Operation`,
  and `NamedType`. Serde defaults so existing IR JSON keeps
  deserializing without them.
- `forge-parser`: new `pub(crate)` `parse_external_docs` helper used
  at root, per-operation, and per-schema sites (incl. inline schemas
  and the `allOf`-flatten merger in `normalize.rs`). Missing-`url`
  blocks are dropped with the new `parser/W-EXTERNAL-DOCS-NO-URL`
  warning so generators don't render bogus links.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  field at every layer.
- `generator-typescript-cli` view-types: new `ExternalDocs`
  interface; `Ir`, `Operation`, and `NamedType` gain the field.
- New conformance fixtures `external-docs/` (root + operation +
  schema, all populated) and `external-docs-no-url/` (warns and
  drops). Three unit tests in `forge-parser` cover the populated
  case, the warn-and-drop case, and the absent case.

### Added — `info.contact`, `info.license` (name + url), `info.termsOfService`

Closes #82. The OAS Info Object's `contact { name, url, email }`,
`license { name, url }`, and `termsOfService` were all dropped by the
parser. SDK metadata (`package.json` author / homepage,
`Cargo.toml` license / repository) needs them, so generators were
silently emitting incomplete crate-level data.

- WIT (`wit/ir.wit`): `api-info` gains `terms-of-service`,
  `contact: option<contact>`, `license-name`, `license-url`. New
  `contact` record carries `name` / `url` / `email` (all optional).
- `forge-ir::ApiInfo`: matching fields; serde defaults so existing IR
  JSON keeps deserializing without them. New `forge_ir::Contact`.
- `forge-parser::parse_info` populates all of them. A `contact` block
  with only `x-*` extensions (or otherwise empty) returns `None`
  rather than emitting a record where every field is `None`.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new
  fields and the new `Contact` record.
- `generator-typescript-cli` `ApiInfo` view-type matches; new
  `Contact` interface mirrors the IR shape.
- New conformance fixture `info-full/` exercises every Info field.
  Existing `v3_1-info-summary-license` regenerated to surface
  `license_name` (which was already in the spec but dropped by the
  parser).

### Added — `Parameter.allow_empty_value` / `allow_reserved`

Closes #76. OAS query parameters carry `allowEmptyValue` (permits
`?foo=` with no value; query-only) and `allowReserved` (permits raw
RFC 3986 reserved chars). Both were dropped by the parser.

- WIT (`wit/ir.wit`): `parameter` gains `allow-empty-value: bool` and
  `allow-reserved: bool`.
- `forge-ir::Parameter`: matching fields; serde `default` for
  back-compat with serialized IRs.
- `forge-parser::operations::parse_inline_parameter` reads both. New
  `parser/W-PARAM-ALLOW-EMPTY-MISPLACED` warning fires when
  `allowEmptyValue: true` appears on a non-`query` parameter; the flag
  is dropped.
- `forge-ir-bindgen` + `forge-plugin-sdk` round-trip both fields.
- `generator-typescript-cli` `Parameter` view-type matches.
- New conformance fixtures
  `param-query-allow-empty-value/` (covers both the legitimate query
  case and the misplaced-on-header warning) and
  `param-query-allow-reserved/`.

### Added — `Encoding.allow_reserved`

Closes #75. OAS Encoding Object's `allowReserved: bool` (default
`false`) governs whether RFC 3986 reserved chars are percent-encoded
for `application/x-www-form-urlencoded` parts. The IR had no slot;
silent data loss.

- WIT (`wit/ir.wit`): `encoding` gains `allow-reserved: bool`.
- `forge-ir::Encoding`: new `allow_reserved: bool`; serde `default`
  for back-compat with serialized IRs.
- `forge-parser::operations::parse_encoding_map` reads `allowReserved`
  (default `false`).
- `forge-ir-bindgen` + `forge-plugin-sdk` round-trip the field.
- `generator-typescript-cli` `Encoding` view-type matches.
- Conformance fixture `body-form-urlencoded` extended with a
  `redirect_uri` property and `encoding.redirect_uri.allowReserved:
  true`; expected IR carries `allow_reserved: true`.

### Added — `readOnly` / `writeOnly` on top-level schemas

Closes #86. JSON Schema's `readOnly` / `writeOnly` flags propagate
per-schema. The IR honoured them only on `Property`; a top-level
component schema with `readOnly: true` (legal, and meaningful for
response-only types or oneOf variants) lost the flag. Now lifted to
`NamedType` so generators can opt into stripping read-only types from
their request surface and write-only types from their response surface.

- WIT (`wit/ir.wit`): `named-type` gains `read-only: bool` and
  `write-only: bool`.
- `forge-ir::NamedType`: new `read_only: bool` / `write_only: bool`
  fields; serde defaults to `false` with `skip_serializing_if` so
  existing IR JSON keeps deserializing.
- `forge-parser`: new `schema::read_write_only(map)` helper; every
  `NamedType` constructor reads the pair from the schema map.
- `forge-ir-bindgen` + `forge-plugin-sdk` round-trip both fields.
- `generator-typescript-cli` view-type matches.
- New conformance fixture `read-write-only-on-types/` declares a
  `ServerAssignedId` (`readOnly: true`) and a `WriteSecret`
  (`writeOnly: true`) component schema; expected IR carries both flags.

### Added — `x-*` extensions on Discriminator

Closes #81. OAS allows `x-*` extensions on every Specification Object,
including Discriminator. The IR now carries them.

- WIT (`wit/ir.wit`): `discriminator` gains
  `extensions: list<tuple<string, value>>`.
- `forge-ir::Discriminator`: new `extensions: Vec<(String, Value)>`
  field; serde defaults to empty so existing IR JSON keeps deserializing.
- `forge-parser`: `collect_extensions` is now `pub(crate)`; the
  discriminator walker calls it scoped to the `discriminator` ptr token,
  so compound extensions surface as `parser/W-EXTENSION-DROPPED`
  diagnostics with a precise pointer.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new field.
- `generator-typescript-cli`'s `Discriminator` view-type matches.
- Conformance fixture `oneof-discriminator` declares
  `x-codegen-naming-strategy` and `x-vendor-priority`; the regenerated
  expected IR asserts both survive.

### Added — Schema `title` preserved on `NamedType`

Closes #78. JSON Schema `title` is the short human label that doc
generators and IDE hover surface. The parser now reads it and sets
`NamedType.title`; previously it was silently dropped.

- WIT (`wit/ir.wit`): `named-type` gains `title: option<string>` after
  `documentation`.
- `forge-ir::NamedType`: new `title: Option<String>` field.
- `forge-parser`: new `schema::title(map)` helper; every `NamedType`
  constructor in `schema.rs` / `normalize.rs` populates it from
  `map.get("title")`.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new field.
- Conformance fixture `petstore-minimal` now declares a `title` on the
  `Pet` schema and on the `Pet.name` property.

### Added — `securityScheme.deprecated` (OpenAPI 3.2)

Closes #80. OpenAPI 3.2 added a `deprecated: bool` flag on Security
Scheme objects to signal that a scheme is being phased out. The parser
now surfaces it on `SecurityScheme.deprecated`.

- WIT (`wit/ir.wit`): `security-scheme` gains `deprecated: bool`.
- `forge-ir::SecurityScheme`: new `deprecated: bool` field
  (`#[serde(default, skip_serializing_if)]` so existing IR JSON keeps
  deserializing without it).
- `forge-parser::security::parse_scheme` reads `deprecated`, defaults
  to `false`.
- `forge-ir-bindgen` and `forge-plugin-sdk`: round-trip the new field.
- `generator-typescript-cli` `SecurityScheme` view-type gains the
  field to match the WIT shape.
- New conformance fixture
  `fixtures/conformance/v3_2-security-scheme-deprecated/` with a
  deprecated `apiKey`.

### Removed — Static feature-compatibility system (BREAKING)

The `requires` / `forbids` / `provides` lists in `plugin-info` and the
`ir-feature` enum are gone. Plugins now declare only `name` and
`version`. The host no longer pre-checks pipelines against a feature
manifest; plugins inspect the IR they receive and either reject
(`StageError::Rejected` with a structured diagnostic) or warn
(`Severity::Warning`). See ADR-0009 and the new "Handling specs you
can't generate code for" section in `docs/plugin-authoring.md`.

- WIT (`wit/ir.wit`): deleted `ir-feature` enum; shrunk `plugin-info`
  to `{ name, version }`.
- `forge-ir`: removed `IrFeature`; updated `PluginInfo`.
- `forge-pipeline`: deleted `compatibility.rs`, removed
  `check_compatibility` and `CompatError` from the public surface,
  removed the static-check call from `driver::run`.
- `forge-ir-bindgen`, `forge-plugin-sdk`: dropped the corresponding
  `IrFeature` and `PluginInfo` field conversions.
- All in-tree plugins (Rust + Go) updated to the smaller `PluginInfo`.
- New test fixtures `plugins/test-fixtures/generator-strict` (hard
  reject) and `plugins/test-fixtures/generator-warn` (soft warn).
- New integration test `crates/forge-plugin-itests/tests/rejection.rs`
  exercises both fixtures end-to-end across the WIT boundary.

Out-of-tree plugins must rebuild against the new contract. The
migration is mechanical: drop the three list fields from `PluginInfo`
and add `StageError::Rejected` returns where you previously relied on
the static gate.

### Added — Split-document spec support

Closes #51. The parser now follows `$ref` everywhere OAS permits one,
not just inside schemas. Real-world specs commonly split the document
across many JSON files (e.g. `openapi.json` with `$ref` stubs into
`paths/<feature>.json`, `components/schemas.json`, etc.); this PR makes
those parse cleanly.

- Parser
  - New `crates/forge-parser/src/ref_walk.rs` with
    `with_resolved_object` — generic dispatcher that resolves `{$ref:
    …}` (chains too), switches `Ctx::current_doc` to the target doc,
    runs a body callback with the inline value, restores context. Used
    by the path-item / parameter / response / request-body /
    security-scheme walkers.
  - `crates/forge-parser/src/external.rs::resolve_pointer` — RFC 6901
    JSON-pointer walk over a loaded `Value`. Replaces the previous
    "fragment must be `/components/schemas/<X>`" check.
  - `crates/forge-parser/src/refs.rs::resolve` — accepts both
    `#/components/schemas/<Name>` (wrapped) and `#/<Name>` (flat-root)
    fragments. Flat-root works because `ensure_doc_registered` now
    pre-registers root-level keys when the loaded doc has no
    `components.schemas` wrapper.
  - `Ctx` gains `doc_roots: HashMap<PathBuf, Value>` so fragment-only
    refs inside an external doc resolve from the cached root rather
    than re-loading. The main spec's root is cached too.
  - `walk_component_schemas` pushes
    `(canonical, "/components/schemas/<Name>")` onto `Ctx::walking`
    while walking each component, so cross-file refs that loop back
    into a main-spec component recognise the cycle and return the id
    without re-walking.
  - `parse_inline_parameter` accepts the OAS `content.<media>.schema`
    form when `schema` is absent (used for complex parameter values).
  - Schemas with no `type` (or bare `type: "null"`) fold into a
    permissive freeform object (`additional_properties: any`) instead
    of failing. Lets specs that use opaque "any JSON" placeholders
    (`{"description": "Any JSON Schema"}`) parse.
  - `detect_nullable` recognises standalone `type: "null"` (3.1) in
    addition to the existing array form and `nullable: true`.
  - New `parser/E-CYCLIC-REF` (re-introduced) for non-schema cycles
    (path-items pointing at each other across files). Schema cycles
    keep their recursion-allowed treatment via finalize's Tarjan pass.

- Probed against a real-world 3700-line OpenAPI 3.2.0 split-document
  spec: **0 errors → 436 operations, 6738 types, 7 security schemes**.
  Pre-PR baseline: 838 errors and no IR. The remaining 996 warnings
  are all benign (`W-DISCRIMINATOR-MAPPING-DANGLING`,
  `W-EXTENSION-DROPPED`, `W-ALLOF-CONFLICT`, `W-UNKNOWN-FORMAT`).

- Conformance fixtures (added)
  - `split-doc-flat-schemas/`, `split-doc-component-schema-ref/`,
    `split-doc-paths/`, `split-doc-parameters/`, `split-doc-responses/`,
    `split-doc-request-body/`, `split-doc-security-schemes/`,
    `split-doc-cycle/` — one per ref location, including a deliberate
    cross-file cycle.

- Real-world fixture (added)
  - `fixtures/real-world/multi-tenant-shape/` — synthesised mini-spec
    exercising a split-document layout (1 root + 2 paths files + 5
    components files). Wired into
    `crates/forge-plugin-itests/tests/real_world.rs` so CI compiles
    the generated TypeScript and Rust against it.

### Plugins — generator-rust-reqwest: param styles + non-JSON bodies

Brings rust-gen to TS-fetch parity for the request-side surface. Closes
#42 and #43.

- **Cookie params** assemble into a `Cookie` header per request via a
  new `_cookies` accumulator; required cookies pushed unconditionally,
  optional ones gated behind `if let Some(v) = …`. Values
  percent-encoded.
- **Query parameter style dispatch**: `form+explode` repeats the key
  (`?ids=1&ids=2`); `form` collapsed comma-joins; `pipeDelimited` /
  `spaceDelimited` use `|` / ` `; `deepObject` emits `key[subprop]=…`
  per object property.
- **Path-template percent-encoding** for scalar params via a tiny inline
  `_pe` helper (RFC 3986 unreserved set). Path-array params with the
  default `simple` style comma-join their percent-encoded elements.
- **Header arrays** comma-join their values.
- **Non-JSON request bodies**: `application/x-www-form-urlencoded` →
  `req.form(body)` (reqwest serialises via `serde_urlencoded`);
  `multipart/form-data` → `reqwest::multipart::Form` built per
  property, with `Part::bytes` for `PrimitiveKind::Bytes` fields and
  `Part::text` for everything else, plus per-part `mime_str` from
  `BodyContent.encoding[name].content_type`; `application/octet-stream`
  → `req.body(body: Vec<u8>)` with explicit `Content-Type`; `text/*` →
  `req.body(body: String)` with the spec's media-type as
  `Content-Type`.
- **Spec-aware `Cargo.toml`**: the `multipart` reqwest feature is added
  only when the spec uses multipart; `percent-encoding = "2"` is always
  present (used by the inline URL helpers).
- Module split: `operations.rs` shrinks to orchestration + auth;
  `params.rs` owns path / query / header / cookie assembly + the
  inline `_pe` / `_append_query*` helpers; `bodies.rs` owns the
  `BodyPick` enum + body assembly; `util.rs` holds shared string-escape
  helpers. No behaviour change in moved code.
- 11 new tests in `crates/forge-plugin-itests/tests/generator_rust_reqwest.rs`
  exercise the conformance fixtures (`param-cookie`, `param-array-query-{form,pipe,space}`,
  `param-array-path-simple`, `param-deep-object`,
  `body-{form-urlencoded,multipart,octet-stream,text-plain}`) plus a
  conditional-multipart-feature check.

### Plugins — generator-rust-reqwest hardening

- `Cargo.toml`'s `version` field now goes through a hand-rolled SemVer 2.0
  validator. Specs that ship date-formatted versions (Stripe's
  `2026-04-22.dahlia`) or otherwise non-SemVer strings (GitHub's
  `2026.04.0` — leading-zero middle segment) get coerced to `0.0.0` with
  a `# original API version: <raw>` comment preserving the upstream
  value. Closes #47.
- Generated `EnumString` and `EnumInt` types now `impl std::fmt::Display`,
  writing the wire value (the same string the `#[serde(rename = "...")]`
  attribute uses for strings; the integer literal for ints). Required so
  enums can be used as query / header / path param values via the
  generator's `.to_string()`-based assembly. Closes #48.
- Object property names that sanitise to the same Rust ident now get
  `_2`, `_3`, … suffixes per struct (e.g. GitHub `Reactions::+1` and
  `-1` both → `_1`/`_1_2`). Same disambiguation applied to enum
  variants and discriminated-union variants. The serde rename on each
  field/variant preserves the wire name. Surfacing a
  `W-FIELD-COLLISION` diagnostic when this fires is tracked in #52.
  Closes #49.
- `fixtures/real-world/{stripe-customers,github-issues}/spec.json`
  restored to their realistic shapes — the workarounds documented at
  #17 (date version, enum query params, `Reactions` plus/minus fields)
  are gone. The MVP-gate test passes against the restored fixtures.
- TS generator audit for the same `info.version` issue tracked in #53.

### MVP gate — real-world smoke tests

- New `fixtures/real-world/` with two trimmed-but-realistic OpenAPI 3.0
  specs: `stripe-customers/` (15 KB, customer + charge endpoints, bearer
  auth, `allOf` update params, paginated list envelopes) and
  `github-issues/` (19 KB, issues CRUD + comments, deeply-nested
  nullable user/milestone refs, `date-time` formats). See
  `fixtures/real-world/README.md` for the curation policy.
- New `crates/forge-plugin-itests/tests/real_world.rs`, gated behind
  `--features real-world`. For each fixture × generator pair: parse the
  spec (no error-severity diagnostics allowed), run the generator, write
  output to a tempdir, and assert the language toolchain accepts it
  (`tsc --noEmit` for TypeScript, `cargo check` for Rust). Skips
  gracefully when `tsc`/`cargo` aren't on PATH.
- New CI job `real-world` (Linux only) installs Node + `typescript@5`,
  pre-builds plugins, and runs the gated test suite. Closes #17.
- Three follow-ups filed for generator gaps the gate surfaced:
  `info.version` SemVer coercion (#47), `Display` impl for query/header
  enum params (#48), and field-name collision disambiguation (#49). The
  github-issues fixture has the affected pieces trimmed with a comment
  pointing at each follow-up; restore once they land.

### Plugins — generator-rust-reqwest

- New `plugins/generator-rust-reqwest/` plugin emits an async-`reqwest`
  Rust client crate. v1 covers structs / enums / type aliases for every
  IR `TypeDef` variant (objects with `additionalProperties` flatten
  extras, string + integer enums via `#[serde(rename)]` / `#[repr(...)]`,
  discriminated unions via `#[serde(tag = "...")]`); async fns per
  operation with path / query / header param substitution; JSON request
  / response bodies; and three security schemes (`apiKey`-in-header,
  `http bearer`, `http basic`) injected via `AuthConfig`. Output crate
  uses rustls by default — no OpenSSL — and never pulls a runtime, so
  the consumer chooses tokio / async-std / etc. Closes #16.
- 17-test integration suite at
  `crates/forge-plugin-itests/tests/generator_rust_reqwest.rs`,
  including `generated_petstore_crate_cargo_checks` which writes the
  petstore output to a tempdir and shells out to `cargo check`.
- v1 deferrals filed as follow-ups: cookie params + parameter style
  coverage (#42); multipart / urlencoded request bodies (#43); typed
  multi-response return + per-status `ApiError` bodies (#44).

### Toolchain

- `rust-toolchain.toml` and `workspace.package.rust-version` bumped from
  `1.85` to `1.87`. Required by reqwest's transitive deps (`idna_adapter`
  → `icu_normalizer` → MSRV 1.86 + 1.87 cascade) so the new
  `cargo check`-on-output test compiles.

### Added — OpenAPI 3.1 + 3.2 support

Closes #30 (3.1) and #31 (3.2). After this PR, `oauth2`/`openIdConnect`
generator support (#27) is the only remaining open parser issue.

- Parser
  - `ACCEPTED_VERSION_PREFIXES` includes `"3.1."` and `"3.2."`. The
    version-check special-case for those versions is gone.
  - `detect_nullable` recognises both `nullable: true` (3.0) and
    `type: [..., "null"]` (3.1). Either form flips the bit.
  - `parse_schema` handles `type` arrays. A multi-non-null array
    (`type: ["string", "integer"]`) desugars to an untagged
    `UnionType { kind: OneOf }` over synthetic primitive variants;
    reuses `lift_union_variants` from #41.
  - `exclusiveMinimum` / `exclusiveMaximum` numeric form (3.1) is
    normalised on the way into the IR: a numeric value with no
    companion `minimum` / `maximum` rewrites to the 3.0 `(bound, true)`
    shape. Generators consume one IR shape regardless of source version.
  - `const` keyword (3.1) folds into a single-value `EnumString`
    (string `const`), `EnumInt` (integer `const`), or a nullable
    placeholder (`const: null`).
  - Top-level `webhooks` (3.1+) walks each PathItem entry through the
    refactored `parse_path_item` helper and lands the resulting
    operations on a new `Ir.webhooks: Vec<Operation>`. Operation-id
    uniqueness is shared with `paths`.
  - 3.1 `info.summary` and `info.license.identifier` populate two new
    `ApiInfo` fields. Both `Option<String>`, additive.
  - 3.2 `tags` enriched fields (`parent`, `kind`) emit
    `parser/W-TAG-METADATA-DROPPED`; `Operation.tags` stays a flat
    string list.
  - 3.2 `additionalOperations` on path items emit
    `parser/W-ADDITIONAL-OPERATIONS-DROPPED` per non-standard method
    and skip those operations until the IR's `HttpMethod` enum widens.
  - JSON Schema 2020-12 features deferred behind named rejects:
    `parser/E-DEPENDENT-REQUIRED`, `E-DEPENDENT-SCHEMAS`,
    `E-UNEVALUATED-PROPERTIES`, `E-DYNAMIC-REF`, `E-DYNAMIC-ANCHOR`.

- IR / WIT
  - `ApiInfo` gains `summary: Option<String>` and
    `license_identifier: Option<String>`. `Ir` gains
    `webhooks: list<operation>`. Both additive; existing fixtures
    serialize identically.
  - Mirror updates in `forge-ir-bindgen` / `forge-plugin-sdk`
    conversion macros and `forge-ir`'s `proptest_util`.

- Conformance fixtures (added)
  - `v3_1-nullable-type-array/`, `v3_1-multi-type/`, `v3_1-webhooks/`,
    `v3_1-const-string/`, `v3_1-const-integer/`,
    `v3_1-numeric-exclusive/`, `v3_1-info-summary-license/`,
    `v3_1-error-dependent-required/`,
    `v3_1-error-unevaluated-properties/`, `v3_1-error-dynamic-ref/`,
    `v3_2-minimal/`, `v3_2-tag-metadata-dropped/`,
    `v3_2-additional-operations-dropped/`.

- `forge-plugin-itests/tests/generator_typescript_fetch.rs`: 4 new
  tests covering nullable type-array rendering, multi-type unions,
  `const` literals, and webhook operations not leaking into the client.

### Test discipline

- New `crates/forge-plugin-itests/` host-workspace member crate hosts
  every plugin integration test. Each `tests/<plugin>.rs` routes through
  `forge_test_harness::PluginRunner::build_and_load` — the canonical
  harness API plugin authors are pointed at, instead of the raw
  `Plugin::load_*` path the host's own tests previously used.
- The 26 tests previously living in `crates/forge-host/tests/{plugins,
  typescript_fetch, validator_required_op_id}.rs` move into the new
  crate one file per plugin; original files deleted.
- `plugins/validator-required-operation-id/src/lib.rs` loses its dead
  `#[cfg(all(test, target_arch = "wasm32"))]` block (added in #20). The
  new harness-based test under `forge-plugin-itests` covers the same
  behaviour.
- ADR-0004 rewritten: the "pure-module unit tests" mitigation is gone
  (it was always unfollowable — `cargo test` in a plugin crate hits the
  same `compile_error!` that blocks host builds), replaced with the
  stricter "all plugin tests cross the WIT boundary."
  `docs/plugin-authoring.md` updated to match.
- New `cargo xtask plugin-test-discipline` scan walks
  `plugins/*/src/**/*.rs` and fails on `#[test]`, `#[cfg(test)]`,
  `#[cfg(all(test, ...))]`, or `#[cfg(any(test, ...))]`. Wired into
  `cargo xtask ci` and `.github/workflows/ci.yml` so reintroduction is
  caught before the slow plugin-build / test cycle.
- Closes #38.

### Added — 3.1 prep

Closes #26, #28, #29 — three structural rejects that were blocking the
OpenAPI 3.1 PR. The IR shapes already covered every feature; this PR
fills in the parser walkers and updates the SCC pass.

- Parser
  - **Untagged `oneOf` / `anyOf`** (#26): replace the `E-COMPOSITION-ONEOF`
    (untagged) and `E-COMPOSITION-ANYOF` rejects with a new
    `parse_untagged_union` helper that produces
    `UnionType { discriminator: None, kind }`. The discriminated `oneOf`
    path keeps tag synthesis on top of a shared `lift_union_variants`.
  - **Recursive types** (#29): `finalize.rs` swaps Kahn's algorithm for
    Tarjan's SCC pass, then topo-sorts the SCC DAG with the same
    alphabetical tiebreak so cycle-free graphs sort identically. Multi-node
    SCCs and self-loops emit grouped at the end of their topo position.
    Drops the hard `E-CYCLIC-REF` diagnostic; recursive groups surface as
    info-severity `parser/W-RECURSIVE-TYPE`.
  - **External `$ref`** (#28): new `external` module with a `Resolver`
    trait + a `FileResolver` impl that loads adjacent JSON docs (cached by
    canonical path) and refuses paths escaping the input file's parent.
    URL refs (`http://`/`https://`) are still rejected behind a clearly
    distinct message. New `parse_path(&Path)` entry point uses the file
    resolver; `parse_str` keeps current single-file semantics via
    `NoExternalResolver`.
  - `Ctx` gains per-document `RefIndex` map, `current_doc`, `walking`
    cycle-detection set, and a `doc_prefix` map. External component ids
    get a sanitised file-stem prefix (`types__Pet`) so the global type
    pool stays unique even when two docs share a schema name. The
    `original_name` field is left unprefixed so generated TypeScript
    emits `interface Pet`, not `interface Types__Pet`.

- `generator-typescript-fetch`
  - Declares `provides: [UntaggedUnions, RecursiveTypes, ExternalRefs]`
    so the pipeline compatibility check accepts specs that exercise these
    features. No render-side changes — the existing `render_plain_union_body`
    handles untagged unions, recursive types compile natively in TS, and
    external refs surface as ordinary types.

- Conformance test harness
  - Switched from `forge_parser::parse_str` to `parse_path` so multi-file
    fixtures resolve `$ref` against adjacent JSON files automatically.
    Single-file fixtures keep working unchanged. `SpecLocation.file` now
    carries the input file's name (e.g. `spec.json`) instead of being
    `None`, so locations stay portable across machines.

- Conformance fixtures
  - Added: `oneof-untagged/`, `anyof/` (promoted from rejects),
    `recursive-self/`, `recursive-mutual/`, `external-ref-file/`,
    `external-ref-cycle/`, `external-ref-escape/`, `external-ref-url/`.
  - Removed: `error-oneof-untagged/`, `error-anyof/`,
    `error-external-ref/` (the latter superseded by the new `escape` and
    `url` reject fixtures).

- `forge-plugin-itests/tests/generator_typescript_fetch.rs`: 5 new tests
  asserting plain union output, self- and mutual-recursion rendering,
  and external-ref type pull-through.

### Plugins

- New `validator-required-operation-id` plugin under `plugins/`. First
  reference validator: walks every `Operation`, emits
  `validator-required-operation-id/E-MISSING-ID` for any op with
  `original_id == None`, returns the IR unchanged. The parser already
  rejects missing `operationId` (`parser/E-MISSING-FIELD`), so this
  catches nothing in production today — it exists to demonstrate the
  validator pattern end-to-end. Closes #20.

### Added — Operation surface coverage

Closes the next batch of `parser`-tagged issues (#32, #33, #34, #35, #36).
The IR shapes already existed; this PR fills in the parser dispatchers and
TypeScript generator code that drives them.

- Parser
  - String formats expanded: `byte` and `binary` → `PrimitiveKind::Bytes`;
    `email` → `Email`; `uri` / `uri-reference` → `Uri`; `password` →
    `Password`; `date` → `Date`. Removed from the `W-UNKNOWN-FORMAT`
    fallthrough.
  - Request bodies of any media type accepted. `parse_request_body`
    becomes a permissive walker — it lifts the schema and records the
    declared media type plus optional per-property `encoding` (style /
    explode / contentType). `multipart/*`, `application/x-www-form-urlencoded`,
    and any non-JSON body now parse instead of rejecting.
  - Response bodies of any media type accepted. `parse_responses` drops
    its JSON-only gate.
  - Parameter `style` and `explode` are read from the spec (with the OAS
    default per location: `form`/`true` for query+cookie, `simple`/`false`
    for path+header). Object and array parameter types are no longer
    rejected — they pass through to the IR.
  - Cookie parameters route into `Operation.cookie_params`.
  - New diag code `W-PARAM-STYLE-UNSUPPORTED` for unrecognised style
    strings. Removed: `E-MULTIPART`, `E-FORM-ENCODING`, `E-BODY-NOT-JSON`,
    `E-RESPONSE-NOT-JSON`, `E-COOKIE-PARAMETER`, `E-NON-PRIMITIVE-PARAM`.

- `generator-typescript-fetch`
  - Added `BinaryData = string | Blob | ArrayBuffer | Uint8Array` to
    `runtime.ts`. `PrimitiveKind::Bytes` now renders as `BinaryData`
    instead of `string`.
  - New runtime helpers: `appendQueryForm`, `appendQueryDelimited`,
    `appendQueryDeepObject`. `appendQuery` is preserved as a
    `appendQueryForm`-on-each-key shim for backwards compatibility.
  - Per-operation query assembly picks the right helper based on
    `Parameter.style`. Pipe / space delimited and deepObject paths are
    fully wired.
  - Path templates with array path params using `simple` style emit
    a comma-joined, per-item-encoded substitution.
  - Header injection comma-joins array values.
  - Cookie params assemble into a single `Cookie:` header per request,
    skipping `undefined` values.
  - Request-body dispatcher branches on the picked content media type:
    JSON keeps `JSON.stringify(body)`; urlencoded builds a
    `URLSearchParams`; multipart builds a `FormData` (binary props pass
    through as `Blob`, others are `String()`-coerced; **no explicit
    `Content-Type`** so fetch sets the boundary); octet-stream / text /
    other media types pass the body through with an explicit
    `Content-Type`.
  - Response handling: methods with a non-JSON 2xx return `Response`
    directly (and JSDoc describes the media type); JSON 2xx still folds
    into a typed union as before.

- Conformance fixtures
  - Added: `formats-extended/`, `param-array-query-form/`,
    `param-array-query-pipe/`, `param-array-query-space/`,
    `param-array-path-simple/`, `param-deep-object/`, `param-cookie/`,
    `body-form-urlencoded/`, `body-multipart/`, `body-octet-stream/`,
    `body-text-plain/`, `response-event-stream/`.
  - Removed: `error-multipart/` (multipart now supported).

- `forge-host/tests/typescript_fetch.rs` — 12 new integration tests, one
  per new fixture, asserting the generated client uses the right helper /
  Content-Type / body construction.

### Publish-readiness

- `forge-plugin-sdk` and `forge-test-harness` now carry the metadata
  needed for `cargo publish`: per-crate README, `keywords`, `categories`,
  `readme` field. `LICENSE-MIT` and `LICENSE-APACHE` are checked in at
  the repo root. `forge-test-harness` gains a working doctest on
  `HarnessError`; `forge-plugin-sdk`'s remaining `ignore`d doctests are
  documented as a wasm-only constraint (see ADR-0004). Closes #18. The
  transitive publish chain (`forge-ir`, `forge-ir-bindgen`, `forge-host`)
  is tracked in #24.

### Foundation hardening

- `forge-cli` validates each plugin's TOML config against its
  `config_schema()` before invoking the stage. Mismatches fail fast with
  exit code 2 and a diagnostic naming the offending key. Closes #4.
- CI gains a `determinism` job that runs the petstore pipeline twice and
  diffs the output byte-for-byte. Closes #3.
- ADR-0002 (two-pass normalization), ADR-0003 (no wall-clock in plugins),
  and ADR-0005 (separate plugin workspace) are written. Closes #5, #6, #7.

### Added — Phase 3 parser MVP

Closes the eight `parser` + `mvp` issues (#8, #9, #10, #11, #12, #13, #14,
#15) in a single PR. The IR types and WIT bindings already covered every
feature; this work fills in the parser and TypeScript generator paths so
real-world specs flow through the pipeline without being rejected.

- Parser
  - String enums (#8) — `EnumString` with `nullable` propagation. Bare
    `null` literals in the `enum` array fold into the nullable axis.
  - Integer enums (#9) — `EnumInt` with `IntKind::Int32` / `Int64` chosen
    by `format`.
  - `nullable: true` propagation (#10) — applied to primitives, arrays,
    objects, enums, and unions. `ArrayType::item_nullable` is mirrored
    from the resolved items type so both axes stay in sync.
  - `allOf` eager flattening (#11) — new `normalize` module merges parts
    into a single `ObjectType`. Properties union (last-write-wins on
    conflict + `parser/W-ALLOF-CONFLICT`), `required` union, most-restrictive
    `additionalProperties` and object constraints. Component schemas walk
    in dependency order so `allOf` $refs resolve to already-walked targets.
    Synthetic part containers are pruned from the final IR after merging.
  - Typed `additionalProperties` (#12) — recurses through `parse_schema`
    and stores the lifted type as `AdditionalProperties::Typed`.
  - `oneOf` + `discriminator` (#13) — produces `UnionType { kind: OneOf,
    discriminator: Some(_) }`. Reads `discriminator.mapping`, falls back
    to short schema names for variants without an explicit tag, and warns
    (`parser/W-DISCRIMINATOR-MAPPING-DANGLING`) on mappings that don't
    match a declared variant. Untagged `oneOf` and `anyOf` still reject.
  - Multiple response codes per operation (#14) — the parser already
    supported them; this PR drops the "single 2xx + default" docs limit.
  - Security schemes (#15) — new `security` module walks
    `components.securitySchemes` for `apiKey` (header / query / cookie),
    `http bearer` (with optional `bearerFormat`), and `http basic`.
    `oauth2` and `openIdConnect` are still deferred and emit
    `parser/E-SECURITY-SCHEME-OAUTH2`. Operation-level `security` overrides
    a top-level default; an explicit empty array opts out.
  - Version-check refactor: replaced `starts_with("3.0.")` with an
    `ACCEPTED_VERSION_PREFIXES` allow-list table. Adding 3.1 / 3.2 in a
    later PR is a one-line change.
  - New diagnostic codes: `W-ALLOF-CONFLICT`,
    `W-DISCRIMINATOR-MAPPING-DANGLING`, `W-ENUM-VALUE-DROPPED`,
    `W-UNKNOWN-SECURITY-SCHEME`, `E-SECURITY-SCHEME-OAUTH2`. Removed:
    `E-NULLABLE`, `E-STRING-ENUM`, `E-INTEGER-ENUM`,
    `E-ADDITIONAL-PROPERTIES-TYPED`, `E-SECURITY-SCHEME` (all promoted to
    full support).

- `generator-typescript-fetch`
  - Renders `EnumString`, `EnumInt`, and discriminated `Union` types as
    TypeScript literal unions / intersections; the previous
    `unknown /* TODO */` placeholder is gone.
  - Nullable types render as `T | null` at every use site. Required-but-
    nullable object properties stay required (`name: T | null`) — they
    are not collapsed into optional.
  - `response_ts` builds a union of every 2xx JSON response body. Error
    responses (`default`, `4XX`, `5XX`) are listed in method JSDoc as
    `@throws {ApiError}` lines; the runtime continues to attach the
    parsed body to `ApiError.body`.
  - When a spec declares any security scheme, the client emits an
    `AuthConfig` discriminated union (`apiKey | bearer | basic`),
    `ApiClientOptions.auth?: AuthConfig`, and per-operation auth
    injection — `if (this._auth?.kind === "apiKey") { headers[...] = ...; }
    else if (...) { ... }`. apiKey-in-header / query / cookie, bearer
    `Authorization`, and basic via `btoa(user:pass)` are all wired.

- IR / WIT
  - `IrFeature::StringEnums` added in both `forge-ir` and `wit/ir.wit`,
    plus the matching arms in `forge-ir-bindgen` and `forge-plugin-sdk`
    conversion macros. No other IR / WIT shape changes — every other
    feature uses an existing variant.

- Conformance fixtures
  - Added: `string-enum/`, `integer-enum/`, `nullable-primitive/`,
    `nullable-array/`, `array-of-nullable/`,
    `additional-properties-typed/`, `allof-flatten/`, `allof-with-ref/`,
    `allof-conflict/`, `oneof-discriminator/`, `multi-response/`,
    `security-api-key/`, `security-http-bearer/`, `security-http-basic/`,
    `security-operation-override/`.
  - Removed: `error-string-enum/`, `error-nullable/`,
    `error-additional-properties-typed/`, `error-allof/`,
    `error-security-scheme/` (now supported).
  - Renamed `error-oneof/` → `error-oneof-untagged/` (still rejected;
    discriminated `oneOf` lives in the new positive fixture).

- `forge-host/tests/typescript_fetch.rs` — 14 new tests covering enum
  rendering, nullable propagation, typed `additionalProperties`,
  `allOf` flatten / inheritance, discriminated unions, multi-response
  union return type and JSDoc-error listing, apiKey / bearer / basic
  header injection, and operation-level security overrides.

- `forge-cli/tests/e2e.rs` — `unsupported_spec_feature_halts_with_diagnostic`
  switched from `allOf` (now supported) to untagged `oneOf` to keep the
  parser→pipeline halt path covered.

### Added — Stage 3 (Phase 2 Vertical Slice)

The parser is real, the first real codegen ships, and the CLI now drives a
spec → IR → TS pipeline end to end.

- `forge-parser` — hand-written walker over `serde_json::Value` producing a
  fully populated `forge_ir::Ir` for the Stage 3 OpenAPI 3.0 subset
  documented in `docs/parser-coverage.md`. Notable pieces:
  - `pointer` — RFC 6901 builder wrapping `jsonptr::PointerBuf` with
    panic-safe scope helpers, threaded through every diagnostic location.
  - `schema` — recursive type walker that lifts inline schemas into the
    type pool with synthesized ids (`<owner>_<role>` per ADR-0006), inlines
    `$ref` into `#/components/schemas/`, and rejects every out-of-scope
    feature with a stable diagnostic code.
  - `operations` — paths/methods walker honoring the "single 2xx + single
    default" Stage 3 limit, JSON-only bodies and responses, primitive-only
    parameters, and operationId uniqueness.
  - `finalize` — Kahn topological sort over `Ir.types` with alphabetical
    tiebreak, plus determinism-invariant validation.
  - 27 unit tests + a directory-driven conformance suite in
    `fixtures/conformance/` (12 fixtures: 6 positive, 6 negative). Set
    `FORGE_REGEN=1` to regenerate expected files.
- `generator-typescript-fetch` — first real codegen plugin. Emits a tiny
  `fetch`-based TypeScript client with no runtime dependencies:
  `package.json`, `tsconfig.json` (strict), `src/runtime.ts` (≈30 lines,
  shared `ApiError` / URL / query-string helpers), `src/models.ts`
  (interface per IR `Object`, `Array`/`Map` aliases), `src/client.ts`
  (`ApiClient` class with one async method per operation), `src/index.ts`,
  and `README.md`. Honors `packageName` and `baseUrl` config; falls back
  to the first server URL in the spec.
- `forge-plugin-sdk` — full bidirectional `*_to_wit` / `*_from_wit`
  conversions between WIT-generated types and `forge_ir::*`, including
  the entire IR tree, diagnostics, plugin info, and operations. Plugin
  authors write pure logic against canonical types and use
  `convert::{generator,transformer}::ir_from_wit` /
  `generation_output_to_wit` / `transform_output_to_wit` at the `Guest`
  boundary. New SDK helper types: `forge_plugin_sdk::output::{FileMode,
  OutputFile, GenerationOutput, TransformOutput}` (also re-exported at the
  crate root). New `stage_error` builders: `convert::*::config_invalid`,
  `plugin_bug`, `rejected`. All three first-party plugins were refactored
  to this pattern.
- Parser: OpenAPI 2.0 (Swagger) and earlier are now permanently rejected
  with a clear `parser/E-UNSUPPORTED-VERSION` diagnostic that points at
  the legacy `swagger` field. Only OpenAPI 3.0.x is accepted today; 3.1
  remains on the Phase 3 roadmap. The `error-openapi-2/` conformance
  fixture pins this.
- `forge-cli`
  - `[input]` accepts `spec = "openapi.json"` (parsed) or `ir = "ir.json"`
    (legacy, unchanged) via an untagged enum. Parser diagnostics are
    grouped by severity and written to stderr; any error halts before the
    pipeline runs.
  - New CLI errors: `Parse(forge_parser::ParseError)`,
    `ParseDiagnostics { count }`.
- `forge-host` integration tests — `tests/typescript_fetch.rs` covers
  emitted file set, model shape, every parameter location, base-url
  fallback, configured package name, and byte-for-byte determinism across
  consecutive invocations.
- `forge-cli` E2E tests — `generate_from_petstore_spec` runs the full spec
  → TS pipeline against `fixtures/e2e/petstore/`, plus a negative
  `unsupported_spec_feature_halts_with_diagnostic` covering the
  parser→pipeline halt path.

Out of scope (deferred to Phase 3 or later): YAML input, allOf/oneOf/anyOf,
nullable, string enums, typed `additionalProperties`, security schemes,
multipart, form-encoded bodies, external `$ref`, multiple response codes
per operation, Petstore reference server.

### IR

No IR changes in Stage 3. The shape committed at the close of Stage 2 was
sufficient to express the Stage 3 subset.

### Added — Stage 2 (Phase 1 Steps 1.4–1.8)

The plugin runtime is real. Plugins compile to `wasm32-wasip2`, are loaded
by `wasmtime` under fuel / memory / epoch limits, and exchange IR with the
host across the WIT boundary in both directions.

- `forge-host`
  - `Engine` wrapping `wasmtime::Engine`, with a background epoch-tick
    thread so per-store wall-clock deadlines fire.
  - `HostState` with `ResourceLimiter` for memory, fuel via
    `Store::set_fuel`, and a deny-all `wasmtime-wasi` context that
    satisfies the WASI imports the rust libstd `wasm32-wasip2` target
    inserts unconditionally — without giving plugins any actual
    capability.
  - `host-api` impls (`log`, `case-convert`) per world, dedup'd via a
    `macro_rules!`.
  - `Plugin::load_transformer` / `load_generator` / `transform` /
    `generate` — full roundtrip through bindgen-generated types.
  - `filesystem::validate_output` — output guard with traversal,
    absolute-path, duplicate, empty, per-file, total, and count checks.
  - Integration test loading the real `transformer-noop` and
    `generator-debug-dump` `.wasm` artifacts.
- `forge-ir-bindgen`
  - `wasmtime::component::bindgen!` invocations for both worlds.
  - `convert::{transformer,generator}` modules with full IR ↔
    wit-bindgen conversion (≈700 lines, expanded once per world via a
    `macro_rules!`).
  - `StageErrorRepr` / `ResourceKindRepr` — world-neutral error reps so
    `forge-host` can translate without a circular dep.
  - Proptest roundtrips for both worlds.
- `forge-pipeline`
  - `check_compatibility` — static check over `requires` / `forbids` /
    `provides` across a transformer-chain + generator pipeline.
  - `run` — pipeline driver with diagnostic aggregation and configurable
    halt-on-error policy.
- `forge-plugin-sdk`
  - `wit_bindgen::generate!` per world behind `transformer` /
    `generator` Cargo features.
  - `convert::{transformer,generator}` mirroring the host's
    one-direction conversion (`*_to_wit`).
  - Diagnostic builders, config helper.
- `forge-cli`
  - `forge generate <project>` — load `forge.toml`, run pipeline, write
    outputs.
  - End-to-end test exercising the binary against real plugins.
- `forge-test-harness`
  - Real `PluginRunner` that drives `cargo build --target
    wasm32-wasip2` and loads the resulting `.wasm` through the same
    `forge-host` runtime production uses.
- Plugins
  - `transformer-noop` — first plugin; smoke-tests the WIT boundary.
  - `generator-debug-dump` — emits a one-file textual IR summary
    (`ir.txt`). Becomes full JSON in Stage 3 once SDK gains
    `_from_wit` conversions.
- Tooling
  - `.envrc` (`use flake`) — auto-load the toolchain via `direnv` +
    `nix-direnv`.
  - `xtask plugins` — build the plugin workspace for `wasm32-wasip2`;
    `xtask ci` now invokes it before running tests.

### IR — Stage 2

- WIT identifier fix: `prim-int-32` → `prim-int32`, `prim-float-32` →
  `prim-float32`, `int-kind { int-32, int-64 }` → `int-kind { int32,
  int64 }`. WIT identifier segments must start with a letter; the old
  shape failed to parse with newer `wit-parser`.

### Deferred to Stage 3

- Real OpenAPI parser (`forge-parser` is still a stub).
- TypeScript and Rust generators.
- Petstore reference server + integration test.
- Plugin SDK `_from_wit` conversions, so generators can serialize the
  IR to JSON.
- ABI conformance test suite (deliberately-broken plugins).

### Added — Stage 1 (Phase 0 + Phase 1 Steps 1.1–1.3)

- Workspace skeleton with separate host and plugin workspaces.
- WIT package `forge:plugin@0.1.0` with `ir`, `host-api`, `stage`,
  `transformer-api`, and `generator-api` interfaces, and `ir-transformer` /
  `code-generator` worlds.
- `forge-ir` crate carrying the canonical Rust IR types.
- `forge-ir-bindgen` skeleton with `validate_refs` and a proptest roundtrip
  test against a placeholder mirror; replaced by real `wit-bindgen`
  conversions in Step 1.4.
- Stub crates for `forge-parser`, `forge-host`, `forge-pipeline`, `forge-cli`,
  `forge-plugin-sdk`, `forge-test-harness`.
- `xtask` runner with `fmt`, `clippy`, `test`, `doc`, `ci` subcommands.
- GitHub Actions CI for Linux + macOS (no Windows; see plan §2).
- ADRs 0001, 0004, 0006, 0007, 0008.
- `docs/ir-spec.md`, `docs/parser-coverage.md`, `docs/architecture.md`,
  `docs/plugin-authoring.md`.
- `flake.nix` dev shell.

### IR

- Initial IR shape: see `docs/ir-spec.md`. Will break frequently before 1.0.
