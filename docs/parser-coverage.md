# Parser coverage

This document tracks what `forge-parser` supports today. Anything not listed
here produces an `error`-severity diagnostic with a JSON-pointer location —
never silent best-effort output.

Updated each time a Phase 3 feature ships.

## Status: Phase 3 (parser MVP)

The parser handles **OpenAPI 3.0.x, 3.1.x, and 3.2.x JSON** for the Phase
3 MVP subset — enough to consume most real-world specs without rejection.
The CLI accepts `[input] spec = "openapi.json"`; the legacy
`[input] ir = "ir.json"` path remains a debugging escape hatch.

YAML input is **not** supported. Specs must be JSON.

### Supported

- OpenAPI 3.0.x, 3.1.x, 3.2.x, single file, no external `$ref`.
- Primitive schemas: `string`, `integer`, `number`, `boolean`, `array`, `object`.
  Per #105 the IR carries the JSON Schema `type` value only —
  `PrimitiveKind` is `String` / `Integer` / `Number` / `Bool`. Every
  OAS `format` (`int32`, `int64`, `float`, `double`, `date`,
  `date-time`, `uuid`, `byte`, `binary`, `email`, `decimal`, `iban`,
  …) is preserved verbatim on `PrimitiveConstraints.format_extension`.
  Plugins decide whether to produce a richer target-language type
  based on the format string. The parser does not curate a registry
  of "known" formats; it captures and forwards.
- Schema `description` and `title` are preserved on `NamedType.documentation`
  and `NamedType.title`. Generators surface them as doc comments and
  short-label hover text respectively.
- `properties` + `required`.
- Schema constraints: `minimum` / `maximum` / `exclusiveMinimum` /
  `exclusiveMaximum` / `multipleOf` / `minLength` / `maxLength` / `pattern` /
  `minItems` / `maxItems` / `uniqueItems` / `minProperties` / `maxProperties`.
- Path / query / header parameters with primitive types.
- **Path-Item-level shared parameters** — declared once at
  `paths.<path>.parameters` and merged into every operation under the
  path. Operation-level entries override by `(name, in)` (OAS §4.8.9).
  Inline schemas land under a path-derived owner id so two operations
  inherit the *same* type-pool entry. Path items may also carry
  `summary` / `description`; both fall through to operations that
  declare neither of their own.
- **Response headers** — `responses.<code>.headers.<Name>` walks
  inline or `$ref`'d HeaderObjects (HeaderObject = ParameterObject
  minus `name` / `in`) into `Response.headers`. Refs to
  `components.headers.<Name>` resolve through the existing ref machinery.
- **Encoding headers** — `requestBody.content.<media>.encoding.<prop>.headers`
  walks the same way; multipart per-part `Content-Disposition` overrides
  and other custom headers reach the IR.
- JSON request body (`application/json` only).
- **Multiple response codes per operation.** Any combination of explicit
  codes (`200`, `404`, …), ranges (`2XX`, `4XX`), and `default`. The TS
  generator's return type is the union of all 2xx JSON bodies; non-2xx
  response bodies surface via `ApiError.body` and are listed in method
  JSDoc.
- `$ref` into `#/components/schemas/<Name>`. Forward references resolve.
- `additionalProperties`: `false` (`Forbidden`), `true` / unset (`Any`),
  and **typed** (`Typed { type }`) — including primitives and `$ref`.
- **String enums** — `type: string` + `enum` array. A literal `null`
  member counts as nullable.
- **Integer enums** — `type: integer` + `enum` array. `format: int64`
  picks `IntKind::Int64`; otherwise `Int32`.
- **`nullable: true`** — propagated to primitives, arrays (both axes:
  the array itself, and the items via `item_nullable` mirrored from the
  resolved items type), objects, enums, and unions.
- **`allOf` flattening** (object composition) — eager merge into a single
  `ObjectType`. Property union (last-write-wins + `parser/W-ALLOF-CONFLICT`
  on type conflicts), `required` union, most-restrictive `additionalProperties`
  / `min*` / `max*` constraints. Walks component schemas in dependency
  order so allOf-by-$ref always sees a populated target. **Object only:**
  non-object parts emit `parser/E-COMPOSITION-ALLOF`.
- **`oneOf` with `discriminator`** — produces `UnionType { kind: OneOf,
  discriminator }`. Reads `discriminator.mapping`; falls back to the short
  schema name for variants without an explicit tag. Mappings whose target
  isn't one of the declared `oneOf` variants emit
  `parser/W-DISCRIMINATOR-MAPPING-DANGLING`. `x-*` extensions on the
  discriminator object are preserved on `Discriminator.extensions`
  (scalar-only at the WIT boundary; compound values drop with
  `parser/W-EXTENSION-DROPPED`).
- **Security schemes**:
  - `apiKey` in `header`, `query`, or `cookie` — produces
    `SecuritySchemeKind::ApiKey`.
  - `http` `scheme: bearer` (with optional `bearerFormat`) — produces
    `SecuritySchemeKind::HttpBearer`.
  - `http` `scheme: basic` — produces `SecuritySchemeKind::HttpBasic`.
  - `oauth2` with any of the four flow kinds (`implicit`, `password`,
    `clientCredentials`, `authorizationCode`) — produces
    `SecuritySchemeKind::Oauth2` carrying `OAuth2Flow` records.
    Per-flow URL requirements are validated: `implicit` needs
    `authorizationUrl`; `password` and `clientCredentials` need
    `tokenUrl`; `authorizationCode` needs both. Missing required URLs
    emit `parser/E-OAUTH2-MISSING-URL`. `openIdConnect` carries the
    discovery URL on `SecuritySchemeKind::OpenIdConnect.url`; clients
    perform `.well-known/openid-configuration` discovery themselves.
    Acted on by `generator-typescript-cli`'s login flow when plugin
    config supplies a `clientId`; other generators leave auth
    injection to the consumer.
  - `mutualTLS` (3.0+) — produces `SecuritySchemeKind::MutualTls` (no
    payload). The IR carries the declaration; client-cert provisioning
    is left to the consumer's transport configuration (e.g.
    `reqwest::Identity` for rust-reqwest, `tls.connect` for Node).
  - Top-level `security` is the per-operation default; operation-level
    `security` overrides; an explicit empty `[]` opts the operation out.
  - **OAS 3.2 `deprecated`** on a Security Scheme is preserved on
    `SecurityScheme.deprecated`; defaults to `false`.
- **JSON Schema 2020-12 / OAS 3.2 content keywords** — `contentEncoding`,
  `contentMediaType`, `contentSchema` on `PrimitiveConstraints`. Together
  they replace the 3.0-era `format: byte` / `format: binary` shortcuts
  for describing encoded payloads. `contentSchema`'s body lifts into the
  type pool under `<owner>_content_schema` so it's a regular `TypeRef`
  to a fully-walked schema.
- **String formats**: forwarded verbatim on
  `PrimitiveConstraints.format_extension` (#105). The parser doesn't
  recognize a closed list — `date`, `date-time`, `uuid`, `byte`,
  `binary`, `email`, `uri`, `uri-reference`, `password`, `decimal`,
  `iban`, etc. all reach plugins as plain strings. Generators are
  free to produce richer target types when they recognize the format.
- **`readOnly` / `writeOnly`** preserved at both the property level
  (`Property.read_only` / `Property.write_only`) and the schema level
  (`NamedType.read_only` / `NamedType.write_only`) so generators that
  distinguish request from response shapes can strip read-only types
  from request surfaces and write-only types from response surfaces.
- **Request bodies**: any media type. `application/json` →
  `JSON.stringify`; `application/x-www-form-urlencoded` →
  `URLSearchParams`; `multipart/form-data` → `FormData` (binary properties
  pass through as `Blob | ArrayBuffer | Uint8Array`); `application/octet-stream`
  → raw `BinaryData`; `text/*` → `string`; anything else → `BinaryData`
  with the user-declared `Content-Type`.
- **Response bodies**: any media type. JSON 2xx responses fold into the
  method return type as before; methods with any non-JSON 2xx response
  return the raw `Response` so the caller drives streaming / decoding.
- **Parameters with `style` + `explode`**: `form` (default for query / cookie),
  `simple` (default for path / header), `pipeDelimited`, `spaceDelimited`,
  `deepObject`. Array and object parameters lift to the right TS type
  (`T[]`, `Record<...>`, or a referenced interface). Path templates
  comma-join array path params.
- **Cookie parameters**: routed to `Operation.cookie_params` and assembled
  into a single `Cookie:` header at the request site. (Note: browsers
  ignore explicit `Cookie` headers — this is for Node and other non-browser
  callers.)
- **Untagged `oneOf` / `anyOf`**: produce `UnionType { discriminator: None }`.
  The TS generator renders them as plain unions (`A | B`).
- **Recursive types** (self- and mutually-recursive component schemas):
  emitted intact. The finalize pass groups SCC members together at their
  topo-sort position and surfaces them via `parser/W-RECURSIVE-TYPE` info
  diagnostics. Generators that can't represent recursion return
  `StageError::Rejected` with a diagnostic naming the offending type.
- **External `$ref` (file-relative)**: the `parse_path` entry point
  enables `./other.json#/components/schemas/Foo`-style refs. Loaded docs
  are cached by canonical path; resolved paths must stay under the input
  file's parent directory (symlink-escape attempts reject with
  `parser/E-EXTERNAL-REF`). External component ids are prefixed with the
  file stem (`types__Pet`) to keep the global type pool unique; the
  `original_name` field carries the unprefixed name so generators emit
  natural-looking declarations.
- **Split-document specs**. `$ref` is followed *everywhere* OAS permits
  it: path-item entries (`paths.<path> = {$ref: …}`), parameter list
  entries, response entries, request bodies, security scheme entries,
  and individual `components.schemas.<Name>` entries that are themselves
  refs. Fragments may be any JSON pointer — `/components/schemas/Foo`
  (the standard wrapped form) or `/Foo` (flat-root form, where the file
  is itself the schema map). Refs chain across files; cycles in
  non-schema objects produce `parser/E-CYCLIC-REF`.
- **Freeform / no-`type` schemas** map to a permissive object
  (`additional_properties: any`); standalone `type: "null"` collapses
  into the same shape with `nullable: true`. Generators surface either
  as the language's "any" (`unknown` in TS, `serde_json::Value` in
  Rust).
- **Parameter `content.<media>.schema`** — the OAS form for complex
  parameter values whose schema is media-type-scoped. Falls back from
  the inline `schema` field automatically.
- **OpenAPI 3.1**: `type` arrays for nullability (`["string", "null"]`)
  and multi-type unions (`["string", "integer"]` desugars to an untagged
  union over primitives). Numeric `exclusiveMinimum` / `exclusiveMaximum`
  normalize to the 3.0 shape so the IR stays uniform. Top-level
  `webhooks` walk through `parse_path_item` and land on `Ir.webhooks`.
  `info.summary` / `info.license.identifier` / `const` keyword.
- **`info.contact`, `info.license` (name + url), `info.termsOfService`**:
  preserved on `ApiInfo`. `contact.{name, url, email}` populates a
  `Contact` record; license `name` and `url` populate
  `license_name` / `license_url` independent of `license_identifier`
  so 3.0 and 3.1 specs round-trip the same way.
- **`externalDocs`** at root (`Ir.external_docs`), per-operation
  (`Operation.external_docs`), and per-schema (`NamedType.external_docs`).
  Blocks missing the required `url` drop with
  `parser/W-EXTERNAL-DOCS-NO-URL` instead of emitting a stub link.
  Tag-level externalDocs lands once #84 (structured tags) is in.
- **`$ref` siblings**: OAS 3.0 forbade siblings on `$ref`; OAS 3.1+
  inherits JSON Schema 2020-12's allowance. The parser emits
  `parser/W-REF-SIBLINGS-3-0` when a 3.0 spec declares non-`x-*`
  siblings on `$ref`. 3.1+ specs accept the siblings, but
  sibling-merging on the resolved type is not yet implemented (the
  `$ref` resolves; siblings are dropped without warning).
- **`components.pathItems`** — `$ref` into `#/components/pathItems/<Name>`
  resolves through the existing ref machinery (paths, webhooks, and
  callbacks all share it). Declared-but-unreferenced entries surface
  with `parser/W-COMPONENT-PATH-ITEM-UNUSED`.
- **`info.jsonSchemaDialect`** (3.1+) on `Ir.json_schema_dialect`, and
  **`$self`** (3.2) on `Ir.self_url`. Both captured verbatim;
  generators that care can read them. Full base-URI semantics for
  external-`$ref` resolution land separately under #93.
- **Header serialization fields** — `Header.style`, `explode`,
  `allow_reserved`, `allow_empty_value`. OAS Header inherits Parameter's
  fields; the spec fixes `style` to `simple`, but the IR captures
  whatever was declared so spec-strict consumers can see it.
- **`$ref` sibling merging (3.1+)** — non-schema `$ref` sites accept
  sibling keywords and overlay them onto the resolved target.
  OAS 3.2's Reference Object `summary` / `description` are the
  canonical case. 3.0 specs continue to drop siblings (with
  `parser/W-REF-SIBLINGS-3-0` on the schema side; silently elsewhere).
  Schema-side sibling merging in 3.1+ remains a follow-up.
- **3.2 `components.mediaTypes`** — `$ref` into
  `#/components/mediaTypes/<Name>` from `requestBody.content.<media>`
  or `response.content.<media>` resolves through the same shared
  walker. Declared-but-unreferenced entries surface with
  `parser/W-COMPONENT-MEDIA-TYPE-UNUSED`.
- **`callbacks`** on `Operation.callbacks` — out-of-band requests the
  API makes back to the caller (event-driven / webhook APIs). Each
  `Callback` pairs a name with one runtime expression and a list of
  operation ids referencing into `Ir.operations`. `$ref` into
  `components.callbacks.<Name>` resolves through the existing ref
  machinery; operationIds share the global namespace per OAS.
- **Response `links`** on `Response.links` — HATEOAS-style follow-ups
  with `operation_ref` / `operation_id`, named `parameters` (runtime
  expressions or scalars), `request_body`, `description`, per-link
  `server` override. `$ref` into `components.links.<Name>` resolves
  through the existing ref machinery. Compound runtime expressions
  drop with `parser/W-LINK-VALUE-DROPPED`.
- **Schema `xml` block** on `NamedType.xml` (`name`, `namespace`,
  `prefix`, `attribute`, `wrapped`, `x-*` extensions). No in-tree
  generator emits XML clients yet; the IR carries the data so a
  future XML-capable generator can consume it.
- **Universal `x-*` extensions** on every Specification Object:
  `ApiInfo`, `Server`, `ServerVariable`, `NamedType` (schema),
  `Property`, `Parameter`, `Body`, `BodyContent`, `Encoding`,
  `Response`, `SecurityScheme`, `OAuth2Flow`, plus the previously-
  covered `Operation`, `Tag`, `Link`, `Callback`, `XmlObject`, and
  `Discriminator`. Same scalar-only WIT policy as elsewhere; compound
  values drop with `parser/W-EXTENSION-DROPPED`.
- **`example` / `examples`** on `Parameter`, `BodyContent`, and
  `NamedType`. Named-ordered list. 3.0 `example: <literal>` lands
  under the synthetic key `"_default"`; 3.1+ keyed `examples` resolve
  `$ref` into `components.examples.<Name>`. Compound values drop with
  `parser/W-EXAMPLE-DROPPED`.
- **Schema `default` values** at both `NamedType.default` and
  `Property.default`. Scalar-only at the WIT boundary (per ADR-0007);
  compound defaults (`object` / `array`) are dropped with
  `parser/W-DEFAULT-DROPPED`.
- **Top-level `tags`** as structured records on `Ir.tags`. Each tag
  carries `summary` / `description` / `externalDocs` / `parent` /
  `kind` (3.2) / `x-*` extensions. Tags with a `parent` that doesn't
  match a declared sibling drop the parent ref with
  `parser/W-TAG-PARENT-DANGLING` (the tag itself stays).
- **Per-operation and per-path-item `servers` overrides** (OAS §4.8.10).
  Each `Operation.servers` carries the *effective* list with inheritance
  applied (operation > path-item > root); generators read one slot per
  operation and never have to re-walk the spec. An explicit empty
  `servers: []` on a path item or operation is treated as "no override"
  and inherits the next-level list.
- **3.2 `mediaType.itemSchema`** (sequence-of-items responses) — JSON
  Lines, SSE, multipart/mixed. `BodyContent.item_schema` carries the
  per-item shape; `type` is also populated with the same ref so
  generators that don't model streaming see a usable type. Mutually
  exclusive with `schema` (`parser/E-CONTENT-SCHEMA-CONFLICT` if both
  are present).
- **OAS 3.2 additive fields** — `Server.name` (short label), parameter
  `in: querystring` (whole-querystring opaque parameter on
  `Operation.querystring_params`), `Example.dataValue` / `serializedValue`
  (split parsed-vs-wire form, scalar-only at WIT), `XmlObject.text` /
  `ordered` (text-content placement and array-order significance).
- **OpenAPI 3.2**: version accepted. Tag metadata (`parent`, `kind`,
  `summary`, `externalDocs`) is preserved on `Ir.tags`. Path-item
  `additionalOperations` walks each entry into a real `Operation`
  carrying `HttpMethod::Other(<verb>)` (e.g. `QUERY`); the verb is
  upper-cased so generators see a single canonical form.

### Explicitly unsupported (error diagnostics)

| Feature                                  | Diagnostic code                            |
| ---------------------------------------- | ------------------------------------------ |
| `not`                                    | `parser/E-COMPOSITION-NOT`                 |
| `allOf` of non-object parts              | `parser/E-COMPOSITION-ALLOF`               |
| `securityScheme` `oauth2` flow with no `authorizationCode` and missing `authorizationUrl` / `tokenUrl` | `parser/E-OAUTH2-MISSING-URL` |
| URL `$ref` (`http://`, `https://`)       | `parser/E-EXTERNAL-REF`                    |
| External `$ref` outside spec dir         | `parser/E-EXTERNAL-REF`                    |
| Unresolved `$ref` (`#/components/…`)     | `parser/E-DANGLING-REF`                    |
| OpenAPI version other than 3.0.x / 3.1.x / 3.2.x | `parser/E-UNSUPPORTED-VERSION`     |
| Duplicate `operationId`                  | `parser/E-DUPLICATE-OPERATION-ID`          |
| Missing required field                   | `parser/E-MISSING-FIELD`                   |
| Invalid type / format                    | `parser/E-INVALID-TYPE`                    |

Warnings:

| Feature                                 | Diagnostic code                                |
| --------------------------------------- | ---------------------------------------------- |
| Compound `x-*` extension dropped at WIT | `parser/W-EXTENSION-DROPPED`                   |
| Conflicting `allOf` property type       | `parser/W-ALLOF-CONFLICT`                      |
| Discriminator mapping target missing    | `parser/W-DISCRIMINATOR-MAPPING-DANGLING`      |
| Enum value of the wrong shape           | `parser/W-ENUM-VALUE-DROPPED`                  |
| Unknown security scheme type / http scheme | `parser/W-UNKNOWN-SECURITY-SCHEME`          |
| Unrecognized parameter `style`          | `parser/W-PARAM-STYLE-UNSUPPORTED`             |
| Recursive type group emitted (info)     | `parser/W-RECURSIVE-TYPE`                      |
| 3.1+ `dependentRequired` (dropped)      | `parser/W-DEPENDENT-REQUIRED-DROPPED`          |
| 3.1+ `dependentSchemas` (dropped)       | `parser/W-DEPENDENT-SCHEMAS-DROPPED`           |
| 3.1+ `unevaluatedProperties` (dropped)  | `parser/W-UNEVALUATED-PROPERTIES-DROPPED`      |
| 3.1+ `$dynamicRef` (dropped)            | `parser/W-DYNAMIC-REF-DROPPED`                 |
| 3.1+ `$dynamicAnchor` (dropped)         | `parser/W-DYNAMIC-ANCHOR-DROPPED`              |
| Unused `components.mediaTypes.<Name>`   | `parser/W-COMPONENT-MEDIA-TYPE-UNUSED`         |

### Phase 3 verification

`cargo test --workspace` covers:

- Parser unit tests in `crates/forge-parser/src/`.
- A directory-driven conformance suite at `fixtures/conformance/` with both
  positive and negative fixtures. To regenerate the expected files after an
  intentional change:
  ```
  FORGE_REGEN=1 cargo test -p forge-parser --test conformance
  ```
- `generator-typescript-fetch` integration tests at
  `crates/forge-host/tests/typescript_fetch.rs` — file structure, model
  shape, enum / nullable / union / discriminator rendering, multi-response
  return type, AuthConfig + per-scheme header injection, deterministic
  output.
- A spec-to-TS CLI E2E test at `crates/forge-cli/tests/e2e.rs`.

There is no Petstore reference server in Phase 3. Validating that the
generated client compiles under `tsc --strict` is left as a manual step
(`npx tsc -p .` from the output directory) — Node.js is not part of the
project's dev shell.

## OpenAPI version policy

`forge-parser` accepts **OpenAPI 3.0.x, 3.1.x, and 3.2.x**. The
version-check function reads from `ACCEPTED_VERSION_PREFIXES` in
`crates/forge-parser/src/lib.rs`. **OpenAPI 2.0 (Swagger) and any
earlier version will never be supported** —
`parser/E-UNSUPPORTED-VERSION` is permanent for those inputs. Convert
legacy specs upstream (e.g. with `swagger2openapi`) before feeding them
to `forge`.

## Phase 3 expansion order — remaining

1. `openIdConnect` discovery (`.well-known/openid-configuration` fetch)
   plus other `oauth2` flow kinds (`clientCredentials`, `password`,
   `implicit`) — issue #27 follow-ups. The `authorizationCode` flow now
   parses; `generator-typescript-cli` consumes it.
2. URL-based `$ref` (HTTP fetch + caching policy) — follow-up to #28.
3. 3.1 `pathItems` in `components` — follow-up.
4. 3.2 `additionalOperations` real support — needs WIT
   `http-method` enum widened; deferred until adoption.
5. 3.1 enriched tags (`parent` / `kind`) as structured IR — follow-up.
