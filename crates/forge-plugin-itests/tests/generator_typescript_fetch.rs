//! `generator-typescript-fetch` integration tests.
//!
//! Loads the Petstore IR from the parser's conformance fixtures, runs
//! the generator through the WIT boundary via
//! `forge_test_harness::PluginRunner`, and asserts on the emitted files.

mod common;

use common::{ir_for, petstore_ir, runner_for};
use forge_host::{GenerationOutput, OutputFile};
use forge_ir::Ir;

fn run(config: serde_json::Value) -> GenerationOutput {
    run_with(petstore_ir(), config)
}

fn run_with(ir: Ir, config: serde_json::Value) -> GenerationOutput {
    let runner = runner_for("generator-typescript-fetch");
    runner.generate(ir, config).expect("generate")
}

fn file<'a>(out: &'a GenerationOutput, path: &str) -> &'a OutputFile {
    out.files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("missing output file {path}"))
}

fn body(f: &OutputFile) -> &str {
    std::str::from_utf8(&f.content).unwrap()
}

#[test]
fn emits_expected_files() {
    let out = run(serde_json::json!({}));
    let paths: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    let expected = [
        "README.md",
        "package.json",
        "src/client.ts",
        "src/index.ts",
        "src/models.ts",
        "src/runtime.ts",
        "tsconfig.json",
    ];
    for e in expected {
        assert!(paths.contains(&e), "missing {e}; got {paths:?}");
    }
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
}

#[test]
fn models_contains_pet_interface() {
    let out = run(serde_json::json!({}));
    let f = file(&out, "src/models.ts");
    let s = body(f);
    assert!(s.contains("export interface Pet {"), "models: {s}");
    assert!(s.contains("id: number;"), "models: {s}");
    assert!(s.contains("name: string;"), "models: {s}");
    assert!(s.contains("tag?: string;"), "models: {s}");
    assert!(s.contains("export type Pets = Array<Pet>;"), "models: {s}");
    assert!(s.contains("export interface Error {"), "models: {s}");
}

#[test]
fn client_contains_three_methods() {
    let out = run(serde_json::json!({}));
    let f = file(&out, "src/client.ts");
    let s = body(f);
    assert!(s.contains("export class ApiClient {"), "client: {s}");
    assert!(s.contains("async listPets("), "client: {s}");
    assert!(s.contains("async createPet("), "client: {s}");
    assert!(s.contains("async showPetById("), "client: {s}");
    // listPets is GET → no body, optional limit query.
    assert!(s.contains("method: \"GET\""), "client: {s}");
    // createPet is POST with required body.
    assert!(s.contains("method: \"POST\""), "client: {s}");
    assert!(s.contains("body: JSON.stringify(body)"), "client: {s}");
    // showPetById has the path-template substitution.
    assert!(
        s.contains("`/pets/${encodeURIComponent(String(petId))}`"),
        "client: {s}"
    );
    assert!(s.contains(": Promise<M.Pet>"), "client: {s}");
    assert!(s.contains(": Promise<M.Pets>"), "client: {s}");
}

#[test]
fn package_json_uses_default_name() {
    let out = run(serde_json::json!({}));
    let f = file(&out, "package.json");
    let s = body(f);
    assert!(s.contains("\"name\": \"api-client\""), "package: {s}");
    assert!(s.contains("\"version\": \"1.0.0\""), "package: {s}");
}

#[test]
fn package_json_uses_configured_name() {
    let out = run(serde_json::json!({"packageName": "petstore-client"}));
    let f = file(&out, "package.json");
    let s = body(f);
    assert!(s.contains("\"name\": \"petstore-client\""), "package: {s}");
}

#[test]
fn base_url_falls_back_to_first_server() {
    let out = run(serde_json::json!({}));
    let f = file(&out, "src/client.ts");
    let s = body(f);
    assert!(
        s.contains("\"https://petstore.example.com/v1\""),
        "client: {s}"
    );
}

#[test]
fn determinism_two_runs_match() {
    let a = run(serde_json::json!({}));
    let b = run(serde_json::json!({}));
    assert_eq!(a.files.len(), b.files.len());
    for (fa, fb) in a.files.iter().zip(b.files.iter()) {
        assert_eq!(fa.path, fb.path);
        assert_eq!(fa.content, fb.content, "non-deterministic for {}", fa.path);
    }
}

// -- New features ----------------------------------------------------------

#[test]
fn string_enum_renders_as_string_literal_union() {
    let out = run_with(ir_for("string-enum"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains(r#"export type Status = "available" | "pending" | "sold";"#),
        "models: {s}"
    );
}

#[test]
fn integer_enum_renders_as_numeric_literal_union() {
    let out = run_with(ir_for("integer-enum"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type Priority = 1 | 2 | 3 | 5 | 8;"),
        "models: {s}"
    );
    assert!(
        s.contains("export type BigPriority = 100 | 200 | 300;"),
        "models: {s}"
    );
}

#[test]
fn nullable_primitive_property_includes_null() {
    let out = run_with(ir_for("nullable-primitive"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("nickname: string | null;"),
        "required nullable should be `name: T | null`, not optional: {s}"
    );
    assert!(
        s.contains("bio?: string | null;"),
        "optional nullable should be `name?: T | null`: {s}"
    );
}

#[test]
fn nullable_array_renders_as_alias_union_with_null() {
    let out = run_with(ir_for("nullable-array"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    // The array's `nullable: true` flag wraps the alias as `T | null`. The
    // inner non-null array is emitted under `_nonnull` per issue #107's
    // canonical wrap-with-Null shape.
    assert!(
        s.contains("export type TagsNonnull = Array<string>;")
            && s.contains("export type Tags = TagsNonnull | null;"),
        "models: {s}"
    );
}

#[test]
fn array_of_nullable_items_renders_with_null_in_items() {
    let out = run_with(ir_for("array-of-nullable"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type OptionalScores = Array<number | null>;"),
        "models: {s}"
    );
}

#[test]
fn additional_properties_typed_renders_record_alias() {
    // Issue #109 collapsed MapType into ObjectType; pure-map shapes
    // (empty properties + typed additional) now render as a TS type
    // alias `type Foo = { [key: string]: T }` rather than an
    // interface with an index signature. Mixed shapes (properties +
    // additional) continue to emit interfaces.
    let out = run_with(ir_for("additional-properties-typed"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type Headers = { [key: string]: string };"),
        "Headers should be a Record alias: {s}"
    );
    assert!(
        s.contains("export type Counts = { [key: string]: number };"),
        "Counts should be a Record alias: {s}"
    );
}

#[test]
fn allof_flatten_merges_properties_into_single_interface() {
    let out = run_with(ir_for("allof-flatten"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export interface Cat {"), "models: {s}");
    assert!(s.contains("id: string;"), "models: {s}");
    assert!(s.contains("name?: string;"), "models: {s}");
    assert!(s.contains("whiskers: number;"), "models: {s}");
    assert!(
        !s.contains("Cat_allof_part_0"),
        "synthetic part should be pruned: {s}"
    );
}

#[test]
fn allof_with_ref_inherits_from_referenced_component() {
    let out = run_with(ir_for("allof-with-ref"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export interface Animal {"), "models: {s}");
    assert!(s.contains("export interface Dog {"), "models: {s}");
    let dog_section = s
        .split("export interface Dog {")
        .nth(1)
        .expect("Dog block")
        .split("}\n")
        .next()
        .unwrap();
    assert!(dog_section.contains("id: string;"), "dog: {dog_section}");
    assert!(dog_section.contains("name?: string;"), "dog: {dog_section}");
    assert!(dog_section.contains("breed: string;"), "dog: {dog_section}");
}

#[test]
fn oneof_discriminator_renders_as_intersection_union() {
    let out = run_with(ir_for("oneof-discriminator"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type Pet = ({ kind: \"cat\" } & Cat) | ({ kind: \"dog\" } & Dog);"),
        "models: {s}"
    );
}

#[test]
fn multi_response_returns_union_of_2xx_bodies() {
    let out = run_with(ir_for("multi-response"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("Promise<M.Widget | M.WidgetCreated>"),
        "client: {s}"
    );
    assert!(s.contains("@throws") && s.contains("4XX"), "client: {s}");
    assert!(
        s.contains("@throws") && s.contains("default"),
        "client: {s}"
    );
}

#[test]
fn security_api_key_emits_auth_config_and_header_injection() {
    let out = run_with(ir_for("security-api-key"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("export type AuthConfig"),
        "client should declare AuthConfig: {s}"
    );
    assert!(
        s.contains("kind: \"apiKey\""),
        "AuthConfig should have apiKey variant: {s}"
    );
    assert!(
        s.contains("auth?: AuthConfig"),
        "ApiClientOptions should accept auth: {s}"
    );
    assert!(
        s.contains("this._auth?.kind === \"apiKey\""),
        "should inject apiKey based on configured auth: {s}"
    );
    assert!(
        s.contains("headers[\"X-API-Key\"] = this._auth.key;"),
        "should write header from configured key: {s}"
    );
}

#[test]
fn security_http_bearer_injects_authorization_header() {
    let out = run_with(ir_for("security-http-bearer"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("kind: \"bearer\""),
        "AuthConfig should have bearer variant: {s}"
    );
    assert!(
        s.contains("headers[\"Authorization\"] = `Bearer ${this._auth.token}`;"),
        "should inject bearer token: {s}"
    );
}

#[test]
fn security_http_basic_uses_btoa_for_authorization() {
    let out = run_with(ir_for("security-http-basic"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("kind: \"basic\""),
        "AuthConfig should have basic variant: {s}"
    );
    assert!(
        s.contains("`Basic ${btoa("),
        "should base64-encode user:pass for Basic: {s}"
    );
}

#[test]
fn security_operation_override_uses_per_operation_security() {
    let out = run_with(ir_for("security-operation-override"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    let health_block = method_body(s, "async health(");
    assert!(
        !health_block.contains("this._auth?.kind"),
        "health should have no auth injection: {health_block}"
    );
    let admin_block = method_body(s, "async adminAccess(");
    assert!(
        admin_block.contains("this._auth?.kind === \"apiKey\""),
        "adminAccess should inject apiKey: {admin_block}"
    );
    let me_block = method_body(s, "async getMe(");
    assert!(
        me_block.contains("this._auth?.kind === \"bearer\""),
        "getMe should inject bearer (inherited default): {me_block}"
    );
}

fn method_body<'a>(source: &'a str, marker: &str) -> &'a str {
    let start = source
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker}"));
    let rest = &source[start..];
    let end = rest[1..]
        .find("\n  async ")
        .or_else(|| rest[1..].find("\n}\n"))
        .map(|n| n + 1)
        .unwrap_or(rest.len());
    &rest[..end]
}

// -- Operation surface coverage --------------------------------------------

#[test]
fn formats_extended_compile_to_expected_ts() {
    let out = run_with(ir_for("formats-extended"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    // String-formatted primitives keep their `string` mapping; binary/byte
    // collapse onto BinaryData.
    assert!(s.contains("contact?: string;"), "models: {s}");
    assert!(s.contains("homepage?: string;"), "models: {s}");
    assert!(s.contains("secret?: string;"), "models: {s}");
    assert!(s.contains("born?: string;"), "models: {s}");
    assert!(s.contains("raw?: BinaryData;"), "models: {s}");
    assert!(s.contains("encoded?: BinaryData;"), "models: {s}");
    let client = body(file(&out, "src/client.ts"));
    assert!(
        client.contains("import type { BinaryData } from \"./runtime.js\";"),
        "client should import BinaryData: {client}"
    );
}

#[test]
fn array_query_form_uses_explode_helper() {
    let out = run_with(ir_for("param-array-query-form"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("appendQueryForm(url, \"ids\", ids, true);"),
        "client: {s}"
    );
}

#[test]
fn array_query_pipe_uses_delimited_helper() {
    let out = run_with(ir_for("param-array-query-pipe"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("appendQueryDelimited(url, \"ids\""),
        "client: {s}"
    );
    assert!(s.contains("\"|\""), "delimiter should be pipe: {s}");
}

#[test]
fn array_query_space_uses_delimited_helper() {
    let out = run_with(ir_for("param-array-query-space"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("appendQueryDelimited(url, \"ids\""),
        "client: {s}"
    );
    assert!(s.contains("\" \""), "delimiter should be space: {s}");
}

#[test]
fn deep_object_query_uses_dedicated_helper() {
    let out = run_with(ir_for("param-deep-object"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("appendQueryDeepObject(url, \"filter\""),
        "client: {s}"
    );
}

#[test]
fn array_path_param_comma_joins_in_template() {
    let out = run_with(ir_for("param-array-path-simple"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    // The path template should switch from String(petId) to a join.
    assert!(
        s.contains(".map((v) => encodeURIComponent(String(v))).join(\",\")"),
        "client: {s}"
    );
    assert!(s.contains("`/users/${"), "still a template literal: {s}");
}

#[test]
fn cookie_param_assembles_cookie_header() {
    let out = run_with(ir_for("param-cookie"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(s.contains("const _cookies: string[] = [];"), "client: {s}");
    assert!(
        s.contains("_cookies.push(`${\"session\"}=${encodeURIComponent(String(session))}`);"),
        "required cookie always pushed: {s}"
    );
    assert!(
        s.contains("if (trace !== undefined && trace !== null)"),
        "optional cookie guarded: {s}"
    );
    assert!(
        s.contains("headers[\"Cookie\"] = _cookies.join(\"; \");"),
        "cookie header assembled: {s}"
    );
}

#[test]
fn form_urlencoded_body_uses_url_search_params() {
    let out = run_with(ir_for("body-form-urlencoded"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("const _form = new URLSearchParams();"),
        "client: {s}"
    );
    assert!(
        s.contains("_form.append(\"username\", String(body.username));"),
        "client: {s}"
    );
    assert!(
        s.contains("headers[\"Content-Type\"] = \"application/x-www-form-urlencoded\";"),
        "client: {s}"
    );
    assert!(
        s.contains("body: _form,"),
        "init.body should be the URLSearchParams: {s}"
    );
    // No JSON_HEADERS spread in the headers block.
    assert!(
        !s.contains("...JSON_HEADERS"),
        "form body should NOT spread JSON_HEADERS: {s}"
    );
}

#[test]
fn multipart_body_uses_form_data_and_no_explicit_content_type() {
    let out = run_with(ir_for("body-multipart"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(s.contains("const _form = new FormData();"), "client: {s}");
    assert!(
        s.contains("_form.append(\"file\", body.file as Blob);"),
        "binary field should be passed as Blob: {s}"
    );
    assert!(
        s.contains("_form.append(\"metadata\", String(body.metadata));"),
        "non-binary field should be String()-coerced: {s}"
    );
    let upload_method = method_body(s, "async uploadFile(");
    // Multipart must NOT set Content-Type (fetch sets the boundary).
    assert!(
        !upload_method.contains("headers[\"Content-Type\"] ="),
        "multipart Content-Type must stay implicit: {upload_method}"
    );
    assert!(
        upload_method.contains("body: _form,"),
        "client: {upload_method}"
    );
}

#[test]
fn octet_stream_body_passes_through_as_binary_data() {
    let out = run_with(ir_for("body-octet-stream"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(s.contains("body: BinaryData"), "signature: {s}");
    assert!(
        s.contains("headers[\"Content-Type\"] = \"application/octet-stream\";"),
        "client: {s}"
    );
    assert!(
        s.contains("body: body as BodyInit,"),
        "init.body passes through: {s}"
    );
}

#[test]
fn text_plain_body_signature_is_string() {
    let out = run_with(ir_for("body-text-plain"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(s.contains("async writeNote(body: string"), "signature: {s}");
    assert!(
        s.contains("headers[\"Content-Type\"] = \"text/plain\";"),
        "client: {s}"
    );
}

#[test]
fn event_stream_response_returns_raw_response() {
    let out = run_with(ir_for("response-event-stream"), serde_json::json!({}));
    let s = body(file(&out, "src/client.ts"));
    assert!(
        s.contains("Promise<Response>"),
        "non-JSON 2xx returns Response: {s}"
    );
    assert!(
        s.contains("return res;"),
        "method should not call res.json(): {s}"
    );
    assert!(
        s.contains("text/event-stream"),
        "JSDoc should name the media type: {s}"
    );
}

// -- 3.1 prep ---------------------------------------------------------------

#[test]
fn untagged_oneof_renders_as_plain_union() {
    let out = run_with(ir_for("oneof-untagged"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type StringOrInt = string | number;"),
        "models: {s}"
    );
}

#[test]
fn anyof_renders_as_plain_union_with_named_variants() {
    let out = run_with(ir_for("anyof"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export interface Cat {"), "models: {s}");
    assert!(s.contains("export interface Dog {"), "models: {s}");
    assert!(s.contains("export type Pet = Cat | Dog;"), "models: {s}");
}

#[test]
fn recursive_self_emits_self_referential_interface() {
    let out = run_with(ir_for("recursive-self"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export interface TreeNode {"), "models: {s}");
    // The synthetic array type for `children` is `Array<TreeNode>`.
    assert!(s.contains("Array<TreeNode>"), "models: {s}");
}

#[test]
fn recursive_mutual_emits_both_interfaces() {
    let out = run_with(ir_for("recursive-mutual"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export interface Folder {"), "models: {s}");
    assert!(s.contains("export interface File {"), "models: {s}");
    // Entry is the discriminated union; both Folder and File reference it.
    assert!(s.contains("export type Entry"), "models: {s}");
}

#[test]
fn external_ref_pulls_in_pet_from_other_doc() {
    let out = run_with(ir_for("external-ref-file"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    // External-doc Pet is rendered with its short name; the prefix lives
    // only in the type-pool id.
    assert!(s.contains("export interface Pet {"), "models: {s}");
    let client = body(file(&out, "src/client.ts"));
    assert!(client.contains(": Promise<M.Pet>"), "client: {client}");
}

// -- 3.1 / 3.2 --------------------------------------------------------------

#[test]
fn v3_1_nullable_type_array_renders_with_null() {
    let out = run_with(ir_for("v3_1-nullable-type-array"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    // The schema is `type: ["string", "null"]` — a nullable string alias.
    // Because the IR collapses this to a single primitive with `nullable:
    // true`, the rendered alias references the primitive and the wrapping
    // happens at use sites. The MaybeName entry in models is a primitive
    // alias — render_named_type returns None for primitives, so we just
    // confirm the file does not contain a TODO/unknown placeholder.
    assert!(!s.contains("MaybeName = unknown"), "models: {s}");
    assert!(!s.contains("TODO"), "models: {s}");
}

#[test]
fn nullable_three_way_union_renders_with_null_arm() {
    // `Either: { oneOf: [string, integer], nullable: true }` — the IR has
    // a 3-variant Union (the third is the canonical Null). Generator
    // emits each arm including the `null` literal. Issue #107.
    let out = run_with(ir_for("nullable-union-three-way"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export type Either"), "models: {s}");
    assert!(s.contains("string"), "string arm: {s}");
    assert!(s.contains("number"), "integer arm: {s}");
    assert!(s.contains("null"), "null arm: {s}");
}

#[test]
fn v3_1_multi_type_renders_as_plain_union() {
    let out = run_with(ir_for("v3_1-multi-type"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(
        s.contains("export type StringOrInt = string | number;"),
        "models: {s}"
    );
    assert!(s.contains("export type MaybeStringOrInt"), "models: {s}");
    assert!(
        s.contains(" | null"),
        "nullable union should include null: {s}"
    );
}

#[test]
fn v3_1_const_renders_as_single_value_literal() {
    let out = run_with(ir_for("v3_1-const-string"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains(r#"export type Kind = "pet";"#), "models: {s}");
}

#[test]
fn v3_1_const_boolean_renders_as_literal_type() {
    let out = run_with(ir_for("v3_1-const-boolean"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export type Enabled = true;"), "models: {s}");
}

#[test]
fn v3_1_const_number_renders_as_literal_type() {
    let out = run_with(ir_for("v3_1-const-number"), serde_json::json!({}));
    let s = body(file(&out, "src/models.ts"));
    assert!(s.contains("export type Ratio = 1.5;"), "models: {s}");
}

#[test]
fn v3_1_webhook_does_not_render_as_client_method() {
    let out = run_with(ir_for("v3_1-webhooks"), serde_json::json!({}));
    let client = body(file(&out, "src/client.ts"));
    // newPetWebhook lives on `Ir.webhooks`, not `Ir.operations`. The
    // client class should not contain it as a method.
    assert!(
        !client.contains("async newPetWebhook("),
        "webhook leaked into client: {client}"
    );
}
