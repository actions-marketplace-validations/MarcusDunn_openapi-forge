//! `generator-rust-reqwest` integration tests.
//!
//! Loads the Petstore IR from the parser's conformance fixtures, runs
//! the generator through the WIT boundary via
//! `forge_test_harness::PluginRunner`, and asserts on the emitted Rust
//! crate. The acceptance test (`generated_petstore_crate_cargo_checks`)
//! writes the output to a `tempdir` and shells out to `cargo check`.

mod common;

use common::{ir_for, petstore_ir, runner_for};
use forge_host::{GenerationOutput, OutputFile};
use forge_ir::Ir;

fn run(config: serde_json::Value) -> GenerationOutput {
    run_with(petstore_ir(), config)
}

fn run_with(ir: Ir, config: serde_json::Value) -> GenerationOutput {
    let runner = runner_for("generator-rust-reqwest");
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
        "Cargo.toml",
        "README.md",
        "src/lib.rs",
        "src/models.rs",
        "src/client.rs",
        "src/error.rs",
    ];
    for e in expected {
        assert!(paths.contains(&e), "missing {e}; got {paths:?}");
    }
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
}

#[test]
fn cargo_toml_has_reqwest_and_serde() {
    let out = run(serde_json::json!({}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(s.contains("name = \"api-client\""), "{s}");
    assert!(s.contains("reqwest"), "{s}");
    assert!(
        s.contains("rustls-tls"),
        "default reqwest features must be off: {s}"
    );
    assert!(s.contains("serde = "), "{s}");
    assert!(s.contains("serde_json = "), "{s}");
    assert!(s.contains("thiserror = "), "{s}");
}

#[test]
fn crate_name_uses_configured_value() {
    let out = run(serde_json::json!({"crateName": "petstore-client"}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(s.contains("name = \"petstore-client\""), "{s}");
}

#[test]
fn models_contains_pet_struct() {
    let out = run(serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub struct Pet {"), "{s}");
    assert!(s.contains("pub id: i64,"), "{s}");
    assert!(s.contains("pub name: String,"), "{s}");
    assert!(s.contains("pub tag: Option<String>,"), "{s}");
    assert!(s.contains("pub struct Error {"), "{s}");
    assert!(s.contains("pub type Pets = Vec<Pet>;"), "{s}");
}

#[test]
fn client_contains_three_methods() {
    let out = run(serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("pub struct ApiClient {"), "{s}");
    assert!(s.contains("pub async fn list_pets("), "{s}");
    assert!(s.contains("pub async fn create_pet("), "{s}");
    assert!(s.contains("pub async fn show_pet_by_id("), "{s}");
    assert!(s.contains("reqwest::Method::GET"), "{s}");
    assert!(s.contains("reqwest::Method::POST"), "{s}");
    // path-template substitution
    assert!(
        s.contains("\"{base}/pets/{pet_id}\""),
        "should substitute path params via format!: {s}"
    );
    // body for createPet
    assert!(s.contains("req = req.json(body);"), "{s}");
    // success response on 200 → typed; 201 (no body) → ()
    assert!(
        s.contains("-> Result<crate::models::Pet, ApiError>")
            || s.contains("-> Result<Pet, ApiError>")
            || s.contains("-> Result<models::Pet, ApiError>"),
        "{s}"
    );
}

#[test]
fn base_url_falls_back_to_first_server() {
    let out = run(serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
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

// -- Feature coverage on existing conformance fixtures ---------------------

#[test]
fn string_enum_renders_as_rust_enum_with_serde_rename() {
    let out = run_with(ir_for("string-enum"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Status {"), "models: {s}");
    assert!(s.contains("#[serde(rename = \"available\")]"), "{s}");
    assert!(s.contains("Available,"), "{s}");
    // #48: Display so the enum is usable as a query/header/path param.
    assert!(s.contains("impl std::fmt::Display for Status {"), "{s}");
    assert!(
        s.contains("Self::Available => f.write_str(\"available\")"),
        "{s}"
    );
}

#[test]
fn integer_enum_renders_as_repr_int_enum() {
    let out = run_with(ir_for("integer-enum"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Priority {"), "{s}");
    assert!(s.contains("#[repr(i32)]"), "{s}");
    assert!(s.contains("V1 = 1,"), "{s}");
    // #48: Display dispatches through the underlying integer.
    assert!(s.contains("impl std::fmt::Display for Priority {"), "{s}");
    assert!(s.contains("(*self as i32).fmt(f)"), "{s}");
}

#[test]
fn boolean_const_renders_as_rust_enum() {
    let out = run_with(ir_for("v3_1-const-boolean"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Enabled {"), "{s}");
    assert!(s.contains("True,"), "{s}");
    // #48: Display writes the boolean wire form.
    assert!(s.contains("impl std::fmt::Display for Enabled {"), "{s}");
    assert!(s.contains("Self::True => f.write_str(\"true\")"), "{s}");
}

#[test]
fn number_const_renders_as_rust_enum() {
    let out = run_with(ir_for("v3_1-const-number"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Ratio {"), "{s}");
    assert!(s.contains("V1_5,"), "{s}");
    // #48: Display writes the numeric literal.
    assert!(s.contains("impl std::fmt::Display for Ratio {"), "{s}");
    assert!(s.contains("Self::V1_5 => f.write_str(\"1.5\")"), "{s}");
}

#[test]
fn nullable_primitive_property_is_option() {
    let out = run_with(ir_for("nullable-primitive"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    // Required nullable → Option<T>; optional+nullable also Option<T>.
    assert!(s.contains("pub nickname: Option<String>"), "{s}");
}

#[test]
fn nullable_three_way_union_wraps_outer_in_option() {
    // `Either: { oneOf: [string, integer], nullable: true }` — the IR has
    // a 3-variant Union (the third is the canonical Null). The Rust enum
    // body filters Null out; consumers reference the named type wrapped
    // in `Option<>`. Issue #107.
    let out = run_with(ir_for("nullable-union-three-way"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Either"), "models: {s}");
    // The Null arm should NOT appear in the enum body — it's expressed
    // via the surrounding Option<> at use sites.
    assert!(!s.contains("Null,"), "Null arm leaked into enum body: {s}");
}

#[test]
fn additional_properties_typed_renders_map_alias() {
    // Issue #109 collapsed MapType into ObjectType; pure-map shapes
    // (empty properties + typed additional) now render as `type Foo =
    // HashMap<String, T>` rather than a struct with a flatten field.
    // Generators that need the struct shape can still produce it for
    // the *mixed* case (properties + additionalProperties together).
    let out = run_with(ir_for("additional-properties-typed"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(
        s.contains("pub type Counts = std::collections::HashMap<String, i64>"),
        "{s}"
    );
    assert!(
        s.contains("pub type Headers = std::collections::HashMap<String, String>"),
        "{s}"
    );
}

#[test]
fn allof_flatten_merges_into_single_struct() {
    let out = run_with(ir_for("allof-flatten"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub struct Cat {"), "{s}");
    assert!(s.contains("pub id: String,"), "{s}");
    // Per #105, `type: integer` with no format defaults to i64 (safe).
    // i32 requires explicit `format: int32`.
    assert!(s.contains("pub whiskers: i64,"), "{s}");
}

#[test]
fn oneof_discriminator_renders_as_serde_tag_enum() {
    let out = run_with(ir_for("oneof-discriminator"), serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    assert!(s.contains("pub enum Pet {"), "{s}");
    assert!(s.contains("#[serde(tag = \"kind\")]"), "{s}");
    assert!(s.contains("Cat(Cat)"), "{s}");
    assert!(s.contains("Dog(Dog)"), "{s}");
}

#[test]
fn security_api_key_emits_auth_config_apikey_variant() {
    let out = run_with(ir_for("security-api-key"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("pub enum AuthConfig"), "{s}");
    assert!(s.contains("ApiKey { key: String }"), "{s}");
    assert!(s.contains("AuthConfig::ApiKey { key }"), "{s}");
    assert!(s.contains("req.header"), "{s}");
}

#[test]
fn security_http_bearer_uses_bearer_auth() {
    let out = run_with(ir_for("security-http-bearer"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("Bearer { token: String }"), "{s}");
    assert!(s.contains("req.bearer_auth(token)"), "{s}");
}

#[test]
fn security_http_basic_uses_basic_auth() {
    let out = run_with(ir_for("security-http-basic"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(
        s.contains("Basic { username: String, password: String }"),
        "{s}"
    );
    assert!(s.contains("req.basic_auth(username"), "{s}");
}

// -- #47 SemVer coercion ---------------------------------------------------

/// Build an inline IR by parsing a JSON snippet. Only used by the
/// targeted #47 / #49 tests below — the rest of the file uses the
/// upstream conformance fixtures via `ir_for`.
fn ir_from_json(s: &str) -> Ir {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("parse inline IR: {e}\n{s}"))
}

const MINIMAL_IR: &str = r#"{
  "info": { "title": "T", "version": "REPLACE_ME" },
  "operations": [],
  "types": [],
  "security_schemes": [],
  "servers": []
}"#;

#[test]
fn cargo_toml_keeps_valid_semver_verbatim() {
    let ir = ir_from_json(&MINIMAL_IR.replace("REPLACE_ME", "1.2.3"));
    let out = run_with(ir, serde_json::json!({}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(s.contains("version = \"1.2.3\""), "Cargo.toml:\n{s}");
    assert!(
        !s.contains("# original API version"),
        "no comment expected for valid SemVer:\n{s}"
    );
}

#[test]
fn cargo_toml_coerces_date_version_to_zero_with_comment() {
    let ir = ir_from_json(&MINIMAL_IR.replace("REPLACE_ME", "2026-04-22"));
    let out = run_with(ir, serde_json::json!({}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(s.contains("version = \"0.0.0\""), "Cargo.toml:\n{s}");
    assert!(
        s.contains("# original API version: 2026-04-22"),
        "expected preservation comment:\n{s}"
    );
}

#[test]
fn cargo_toml_coerces_dotted_leading_zero_to_zero_with_comment() {
    // `2026.04.0` — middle segment has a leading zero, so semver rejects.
    let ir = ir_from_json(&MINIMAL_IR.replace("REPLACE_ME", "2026.04.0"));
    let out = run_with(ir, serde_json::json!({}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(s.contains("version = \"0.0.0\""), "Cargo.toml:\n{s}");
    assert!(
        s.contains("# original API version: 2026.04.0"),
        "expected preservation comment:\n{s}"
    );
}

// -- #49 field-name collision disambiguation ------------------------------

const COLLIDING_OBJECT_IR: &str = r#"{
  "info": { "title": "T", "version": "1.0.0" },
  "operations": [],
  "security_schemes": [],
  "servers": [],
  "types": [
    {
      "id": "Plus",
      "definition": {
        "def": "primitive", "kind": "integer", "constraints": {"format_extension": "int32"}
      }
    },
    {
      "id": "Minus",
      "definition": {
        "def": "primitive", "kind": "integer", "constraints": {"format_extension": "int32"}
      }
    },
    {
      "id": "Reactions",
      "original_name": "Reactions",
      "definition": {
        "def": "object",
        "properties": [
          { "name": "+1", "type": "Plus", "deprecated": false, "read_only": false, "write_only": false },
          { "name": "-1", "type": "Minus", "deprecated": false, "read_only": false, "write_only": false }
        ],
        "additional_properties": { "kind": "forbidden" },
        "required": [],
        "constraints": {}
      }
    }
  ]
}"#;

#[test]
fn field_name_collision_disambiguates_with_serde_rename() {
    let ir = ir_from_json(COLLIDING_OBJECT_IR);
    let out = run_with(ir, serde_json::json!({}));
    let s = body(file(&out, "src/models.rs"));
    // Both fields land — the serde renames preserve the wire names. The
    // two raw names both sanitise to `_1`; the second gets a `_2` suffix.
    assert!(s.contains("#[serde(rename = \"+1\")]"), "models:\n{s}");
    assert!(s.contains("#[serde(rename = \"-1\")]"), "models:\n{s}");
    // First field keeps the base sanitised name; second is disambiguated.
    assert!(s.contains("pub _1: "), "first collider: {s}");
    assert!(s.contains("pub _1_2: "), "disambiguated collider:\n{s}");
}

// -- #42 param styles + cookies -------------------------------------------

#[test]
fn cookie_params_assemble_into_cookie_header() {
    let out = run_with(ir_for("param-cookie"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    // Required cookie pushed unconditionally; optional gated.
    assert!(
        s.contains("let mut _cookies: Vec<String> = Vec::new();"),
        "{s}"
    );
    assert!(
        s.contains("_cookies.push(format!(\"{}={}\", \"session\", _pe(&session)));"),
        "{s}"
    );
    assert!(
        s.contains("if let Some(v) = &trace {"),
        "optional cookie should be Option-gated: {s}"
    );
    assert!(
        s.contains("req.header(reqwest::header::COOKIE, _cookies.join(\"; \"));"),
        "{s}"
    );
}

#[test]
fn array_query_form_explode_repeats_key() {
    let out = run_with(ir_for("param-array-query-form"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(
        s.contains("_append_query_form_explode(&mut url, \"ids\", &"),
        "{s}"
    );
}

#[test]
fn array_query_pipe_delimited() {
    let out = run_with(ir_for("param-array-query-pipe"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(
        s.contains("_append_query_delimited(&mut url, \"ids\", &"),
        "{s}"
    );
    assert!(s.contains("'|'"), "should pass pipe delim: {s}");
}

#[test]
fn array_query_space_delimited() {
    let out = run_with(ir_for("param-array-query-space"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(
        s.contains("_append_query_delimited(&mut url, \"ids\", &"),
        "{s}"
    );
    assert!(s.contains("' '"), "should pass space delim: {s}");
}

#[test]
fn array_path_simple_comma_joined() {
    let out = run_with(ir_for("param-array-path-simple"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    // `let ids = ids.iter().map(|v| _pe(v)).collect::<Vec<_>>().join(",");`
    assert!(
        s.contains(".iter().map(|v| _pe(v)).collect::<Vec<_>>().join(\",\")"),
        "should comma-join encoded path-array elements: {s}"
    );
}

#[test]
fn deep_object_query_brackets() {
    let out = run_with(ir_for("param-deep-object"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    // bracket-style key per object property
    assert!(s.contains("\"filter[name]\""), "{s}");
    assert!(s.contains("\"filter[age]\""), "{s}");
    assert!(s.contains("_append_query(&mut url,"), "{s}");
}

// -- #43 non-JSON request bodies -----------------------------------------

#[test]
fn body_form_urlencoded_uses_req_form() {
    let out = run_with(ir_for("body-form-urlencoded"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("req = req.form(body);"), "{s}");
    // No JSON path should fire.
    assert!(
        !s.contains("req.json(body)") || s.contains("// json"),
        "shouldn't use req.json for urlencoded: {s}"
    );
}

#[test]
fn body_multipart_builds_form_with_binary_part() {
    let out = run_with(ir_for("body-multipart"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(
        s.contains("let mut _form = reqwest::multipart::Form::new();"),
        "{s}"
    );
    // Binary `file` field uses Part::bytes; spec sets contentType: image/png
    // → mime_str("image/png"). Pass the value as-is — `.into()` would
    // ambiguate against the many `From<Vec<u8>>` impls in scope.
    assert!(s.contains("Part::bytes(value)"), "{s}");
    assert!(s.contains(".mime_str(\"image/png\")"), "{s}");
    // Optional `metadata` (string) uses Part::text via Display.
    assert!(s.contains("Part::text(value.to_string())"), "{s}");
    assert!(s.contains("if let Some(value) = body.metadata"), "{s}");
    assert!(s.contains("req = req.multipart(_form);"), "{s}");
}

#[test]
fn body_octet_stream_passes_raw_bytes() {
    let out = run_with(ir_for("body-octet-stream"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("body: Vec<u8>"), "signature: {s}");
    assert!(
        s.contains(
            "req.header(reqwest::header::CONTENT_TYPE, \"application/octet-stream\").body(body)"
        ),
        "{s}"
    );
}

#[test]
fn body_text_plain_passes_string() {
    let out = run_with(ir_for("body-text-plain"), serde_json::json!({}));
    let s = body(file(&out, "src/client.rs"));
    assert!(s.contains("body: String"), "signature: {s}");
    assert!(
        s.contains("req.header(reqwest::header::CONTENT_TYPE, \"text/plain\").body(body)"),
        "{s}"
    );
}

#[test]
fn cargo_toml_includes_multipart_feature_only_when_spec_uses_it() {
    // Petstore has no multipart → no multipart feature.
    let out = run(serde_json::json!({}));
    let s = body(file(&out, "Cargo.toml"));
    assert!(
        !s.contains("\"multipart\""),
        "petstore Cargo.toml shouldn't have multipart: {s}"
    );
    // body-multipart fixture does → feature present.
    let out2 = run_with(ir_for("body-multipart"), serde_json::json!({}));
    let s2 = body(file(&out2, "Cargo.toml"));
    assert!(
        s2.contains("\"multipart\""),
        "multipart spec should add the feature: {s2}"
    );
}

// -- Acceptance: generated petstore crate compiles -------------------------

#[test]
fn generated_petstore_crate_cargo_checks() {
    if which("cargo").is_none() {
        eprintln!("skipping: `cargo` not on PATH");
        return;
    }
    let out = run(serde_json::json!({}));
    let dir = tempfile::tempdir().expect("tempdir");
    for f in &out.files {
        let target = dir.path().join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&target, &f.content).unwrap();
    }
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("check")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"));
    // CI-only: redirect target/ to a stable, cacheable path so reqwest +
    // tokio + serde build incrementally across runs instead of from scratch
    // inside the throwaway tempdir. `.github/workflows/ci.yml` sets the env
    // and caches the path; locally it stays unset and behaviour is unchanged.
    if let Some(td) = std::env::var_os("FORGE_ITEST_CARGO_TARGET_DIR") {
        cmd.arg("--target-dir").arg(td);
    }
    let status = cmd.status().expect("spawn cargo");
    assert!(
        status.success(),
        "generated crate failed `cargo check` (status {status:?})"
    );
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(&exe))
            .find(|p| p.is_file())
    })
}
