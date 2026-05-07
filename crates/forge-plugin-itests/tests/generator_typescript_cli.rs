//! Reference cross-language plugin: TypeScript-source CLI generator.
//!
//! Sibling of `generator_go_server.rs` (issue #58). Builds the plugin via
//! `plugins/generator-typescript-cli/build.sh` (jco + esbuild +
//! typescript), loads the resulting `.wasm` through the same `forge-host`
//! runtime production uses, and asserts:
//!
//! 1. Plugin metadata round-trips through the WIT boundary.
//! 2. Petstore-minimal IR produces the expected file set with kebab-case
//!    subcommands.
//! 3. Generated TS compiles via `tsc --noEmit --strict`.
//! 4. Generated CLI's `--help` exits 0 and lists every operation.
//! 5. Same checks against richer real-world fixtures (github-issues,
//!    stripe-customers).
//! 6. Config-invalid path returns `StageError::ConfigInvalid` (jco throw).
//!
//! Gated behind `--features typescript-cli` so the default `cargo test`
//! skips the Node toolchain. CI runs them through `plugin-typescript-cli`.

#![cfg(feature = "typescript-cli")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use common::{petstore_ir, repo_root};
use forge_test_harness::PluginRunner;

fn ensure_built() -> PathBuf {
    static ONCE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    let cell = ONCE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if let Some(p) = guard.as_ref() {
        return p.clone();
    }
    let plugin_dir = repo_root().join("plugins/generator-typescript-cli");
    let status = Command::new("bash")
        .arg(plugin_dir.join("build.sh"))
        .status()
        .unwrap_or_else(|e| panic!("spawn build.sh: {e}"));
    assert!(status.success(), "build.sh failed (status {status:?})");
    let wasm = plugin_dir.join("plugin.wasm");
    *guard = Some(wasm.clone());
    wasm
}

fn build_and_load() -> PluginRunner {
    let wasm = ensure_built();
    PluginRunner::load(&wasm).unwrap_or_else(|e| panic!("load {}: {e}", wasm.display()))
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(bin))
            .find(|p| p.is_file())
    })
}

fn write_files(out: &forge_host::GenerationOutput, dir: &Path) {
    for f in &out.files {
        let target = dir.join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&target, &f.content).unwrap();
    }
}

fn npm_install(dir: &Path) {
    let status = Command::new("npm")
        .args(["install", "--silent", "--no-audit", "--no-fund"])
        .current_dir(dir)
        .status()
        .expect("spawn npm install");
    assert!(status.success(), "npm install failed (status {status:?})");
}

fn tsc_check(dir: &Path) {
    let status = Command::new("npx")
        .args(["tsc", "--noEmit"])
        .current_dir(dir)
        .status()
        .expect("spawn npx tsc");
    assert!(status.success(), "tsc failed (status {status:?})");
}

fn tsc_build(dir: &Path) {
    let status = Command::new("npx")
        .args(["tsc"])
        .current_dir(dir)
        .status()
        .expect("spawn npx tsc build");
    assert!(status.success(), "tsc build failed (status {status:?})");
}

fn cli_help(dir: &Path, bin_name: &str) -> String {
    let bin = dir.join(format!("bin/{bin_name}.js"));
    let output = Command::new("node")
        .arg(&bin)
        .arg("--help")
        .current_dir(dir)
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "{bin_name} --help failed (status {:?}, stderr: {})",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("--help stdout is utf-8")
}

// -- petstore-minimal -------------------------------------------------------

#[test]
fn info_round_trip() {
    let runner = build_and_load();
    let info = runner.info();
    assert_eq!(info.name, "generator-typescript-cli");
    assert_eq!(info.version, "0.1.0");
}

#[test]
fn generates_petstore_files() {
    let runner = build_and_load();
    let out = runner
        .generate(petstore_ir(), serde_json::json!({"name": "petstore-cli"}))
        .expect("generate");

    let paths: Vec<_> = out.files.iter().map(|f| f.path.clone()).collect();
    for expected in [
        "package.json",
        "tsconfig.json",
        "README.md",
        "bin/petstore.js",
        "src/runtime.ts",
        "src/format.ts",
        "src/models.ts",
        "src/auth.ts",
        "src/client.ts",
        "src/cli.ts",
        "src/index.ts",
    ] {
        assert!(
            paths.iter().any(|p| p == expected),
            "missing {expected} in {paths:?}"
        );
    }

    let cli = out
        .files
        .iter()
        .find(|f| f.path == "src/cli.ts")
        .expect("cli.ts present");
    let body = std::str::from_utf8(&cli.content).expect("utf-8");

    // Kebab-case subcommands from operationIds.
    assert!(
        body.contains("\"create-pet\""),
        "missing create-pet subcommand"
    );
    assert!(
        body.contains("\"list-pets\""),
        "missing list-pets subcommand"
    );
    assert!(
        body.contains("\"show-pet-by-id\""),
        "missing show-pet-by-id subcommand"
    );

    assert!(
        out.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics
    );
}

#[test]
fn generated_petstore_compiles_and_help_works() {
    if which("node").is_none() || which("npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }

    let runner = build_and_load();
    let out = runner
        .generate(petstore_ir(), serde_json::json!({"name": "petstore-cli"}))
        .expect("generate");

    let dir = tempfile::tempdir().expect("tempdir");
    write_files(&out, dir.path());

    npm_install(dir.path());
    // `tsc` (no flags) already type-checks before emitting; calling
    // `tsc --noEmit` first is redundant and roughly doubles the tsc cost.
    tsc_build(dir.path());

    let help = cli_help(dir.path(), "petstore");
    assert!(
        help.contains("create-pet"),
        "create-pet missing from --help"
    );
    assert!(help.contains("list-pets"), "list-pets missing from --help");
    assert!(
        help.contains("show-pet-by-id"),
        "show-pet-by-id missing from --help"
    );
    // Top-level options should be in the help too.
    assert!(help.contains("--base-url"), "missing --base-url");
    assert!(help.contains("--token"), "missing --token");
}

// -- github-issues (richer real-world fixture) ------------------------------

#[test]
fn generated_github_issues_compiles_with_enum_choices() {
    if which("node").is_none() || which("npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }

    let spec_path = repo_root().join("fixtures/real-world/github-issues/spec.json");
    let parse_out = forge_parser::parse_path(&spec_path).unwrap_or_else(|e| panic!("parse: {e}"));
    let errors: Vec<_> = parse_out
        .diagnostics
        .iter()
        .filter(|d| d.severity == forge_ir::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "parser produced errors: {errors:?}");
    let ir = parse_out.spec.expect("ir");

    let runner = build_and_load();
    let out = runner
        .generate(ir, serde_json::json!({"name": "gh-issues"}))
        .expect("generate");

    let dir = tempfile::tempdir().expect("tempdir");
    write_files(&out, dir.path());
    npm_install(dir.path());
    // `tsc` already type-checks before emitting; the prior `tsc_check`
    // was redundant.
    tsc_build(dir.path());

    let help = cli_help(dir.path(), "gh-issues");
    // Spec-derived subcommand sample (operationId: listIssuesForRepo).
    assert!(
        help.contains("list-issues-for-repo"),
        "list-issues-for-repo missing: {help}"
    );

    // Per-subcommand --help should expose enum choices for state.
    let bin = dir.path().join("bin/gh-issues.js");
    let sub = Command::new("node")
        .args([bin.to_str().unwrap(), "list-issues-for-repo", "--help"])
        .output()
        .expect("spawn");
    assert!(sub.status.success(), "subcommand --help failed");
    let sub_help = String::from_utf8(sub.stdout).expect("utf-8");
    // The state enum has values open / closed / all per the spec.
    assert!(
        sub_help.contains("open") && sub_help.contains("closed"),
        "expected state enum choices in --help: {sub_help}"
    );
}

// -- stripe-customers (third real-world fixture) ---------------------------

#[test]
fn generated_stripe_customers_compiles() {
    if which("node").is_none() || which("npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }

    let spec_path = repo_root().join("fixtures/real-world/stripe-customers/spec.json");
    let parse_out = forge_parser::parse_path(&spec_path).unwrap_or_else(|e| panic!("parse: {e}"));
    let errors: Vec<_> = parse_out
        .diagnostics
        .iter()
        .filter(|d| d.severity == forge_ir::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "parser errors: {errors:?}");
    let ir = parse_out.spec.expect("ir");

    let runner = build_and_load();
    let out = runner
        .generate(ir, serde_json::json!({"name": "stripe"}))
        .expect("generate");

    let dir = tempfile::tempdir().expect("tempdir");
    write_files(&out, dir.path());
    npm_install(dir.path());
    tsc_check(dir.path());
}

// -- multi-tenant-shape (regression coverage) ------------------------------
//
// `fixtures/real-world/multi-tenant-shape/` is a hand-curated split-document
// spec exercising a multi-tenant API. Three regressions surfaced when
// openapi-forge was first pointed at a real-world spec of this shape — this
// test bundles them so each is locked in from the same generation run:
//
// 1. **`BodyInit` not in scope.** The generator emitted `body as BodyInit`
//    for non-JSON / non-text / non-octet bodies, which only exists with
//    `lib: ["DOM"]`. The fixture now has a multipart-body op
//    (`uploadDocumentAttachment`); the regression bites if the generated
//    `client.ts` won't compile under our `lib: ["ES2022"]` tsconfig.
//
// 2. **Stray path-template placeholders.** Real specs sometimes leave a
//    `{viewId}` in the path-template undeclared in `parameters` (the
//    `updateNoteSavedView` op here). Generators that blindly
//    substitute every `{...}` produce code referencing undeclared
//    variables, breaking `tsc`. The fix: only substitute placeholders
//    that match a declared path-param; leave others literal.
//
// 3. **Required keyword args + structural typing.** Operations with a
//    *required* query param (here `listUsers` with `--limit`) make the
//    generated client method's `opts` parameter type strictly typed.
//    The CLI collected from commander into `Record<string, unknown>`
//    and TS strict mode rejected the structural mismatch. The fix:
//    cast through `any` at the call site (commander's `requiredOption`
//    enforces presence at runtime).

#[test]
fn generated_multi_tenant_shape_compiles() {
    if which("node").is_none() || which("npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }

    let spec_path = repo_root().join("fixtures/real-world/multi-tenant-shape/spec.json");
    let parse_out = forge_parser::parse_path(&spec_path).unwrap_or_else(|e| panic!("parse: {e}"));
    let errors: Vec<_> = parse_out
        .diagnostics
        .iter()
        .filter(|d| d.severity == forge_ir::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "parser errors: {errors:?}");
    let ir = parse_out.spec.expect("ir");

    // Sanity: the fixture should contain the three quirks this test covers.
    let by_id: std::collections::HashMap<&str, &forge_ir::Operation> =
        ir.operations.iter().map(|o| (o.id.as_str(), o)).collect();
    let upload = by_id
        .get("uploadDocumentAttachment")
        .expect("uploadDocumentAttachment op present");
    assert!(
        upload.request_body.as_ref().is_some_and(|b| b
            .content
            .iter()
            .any(|c| c.media_type == "multipart/form-data")),
        "uploadDocumentAttachment should declare a multipart body",
    );
    let upd = by_id
        .get("updateNoteSavedView")
        .expect("updateNoteSavedView op present");
    let declared: std::collections::HashSet<&str> =
        upd.path_params.iter().map(|p| p.name.as_str()).collect();
    assert!(
        upd.path_template.contains("{viewId}") && !declared.contains("viewId"),
        "fixture should leave a stray {{viewId}} placeholder undeclared",
    );
    let list = by_id.get("listUsers").expect("listUsers op present");
    assert!(
        list.query_params
            .iter()
            .any(|p| p.name == "limit" && p.required),
        "listUsers should declare a required `limit` query param",
    );

    // PR (a) staging: the fixture also declares an oauth2 authorizationCode
    // scheme. Without `oauth.clientId` in plugin config, the typescript-cli
    // should NOT emit login/logout — falls through to an info diagnostic.
    let oauth_scheme = ir
        .security_schemes
        .iter()
        .find(|s| matches!(s.kind, forge_ir::SecuritySchemeKind::Oauth2(_)))
        .expect("multi-tenant-shape declares an oauth2 scheme (PR (a) staging)");
    let _ = oauth_scheme;

    let runner = build_and_load();
    let out = runner
        .generate(ir, serde_json::json!({"name": "multi-tenant-shape"}))
        .expect("generate");

    let dir = tempfile::tempdir().expect("tempdir");
    write_files(&out, dir.path());
    npm_install(dir.path());
    // `tsc` (build) must accept the output. A regression in any of the
    // three bug classes above surfaces here as a typed-error /
    // undefined-name fail. `tsc` type-checks before emitting, so a
    // separate `tsc --noEmit` would be redundant.
    tsc_build(dir.path());

    let help = cli_help(dir.path(), "multi-tenant-shape");
    // Stray-placeholder op still becomes a subcommand; --help works.
    assert!(
        help.contains("update-note-saved-view"),
        "missing update-note-saved-view in --help"
    );
    // Required-opts op shows --limit in its subcommand help.
    let bin = dir.path().join("bin/multi-tenant-shape.js");
    let sub = std::process::Command::new("node")
        .args([bin.to_str().unwrap(), "list-users", "--help"])
        .output()
        .expect("spawn node");
    assert!(sub.status.success(), "list-users --help failed");
    let sub_help = String::from_utf8(sub.stdout).expect("utf-8");
    assert!(
        sub_help.contains("--limit"),
        "expected --limit in list-users --help: {sub_help}"
    );

    // Without oauth.clientId, login/logout must NOT appear.
    assert!(
        !help.contains("login") && !help.contains("logout"),
        "login/logout leaked into --help without an explicit clientId: {help}"
    );
    // …but an info diagnostic should hint that login is opt-in.
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "generator-typescript-cli/I-OAUTH-LOGIN-NOT-ENABLED"),
        "expected I-OAUTH-LOGIN-NOT-ENABLED diagnostic: {:?}",
        out.diagnostics,
    );
}

#[test]
fn generated_multi_tenant_shape_with_oauth_emits_login() {
    if which("node").is_none() || which("npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }

    let spec_path = repo_root().join("fixtures/real-world/multi-tenant-shape/spec.json");
    let parse_out = forge_parser::parse_path(&spec_path).unwrap_or_else(|e| panic!("parse: {e}"));
    let ir = parse_out.spec.expect("ir");

    let runner = build_and_load();
    let out = runner
        .generate(
            ir,
            serde_json::json!({
                "name": "multi-tenant-shape",
                "oauth": {
                    "clientId": "test-client-id",
                    "redirectPort": 4747,
                    "scopes": ["read", "write"],
                },
            }),
        )
        .expect("generate");

    let dir = tempfile::tempdir().expect("tempdir");
    write_files(&out, dir.path());

    // The generated cli.ts and auth.ts must compile under strict tsc.
    // `tsc` (build) type-checks before emitting; the prior `tsc_check`
    // was redundant.
    npm_install(dir.path());
    tsc_build(dir.path());

    // Generated files contain the login surface.
    let cli_src = std::fs::read_to_string(dir.path().join("src/cli.ts")).expect("cli.ts");
    let auth_src = std::fs::read_to_string(dir.path().join("src/auth.ts")).expect("auth.ts");
    assert!(
        cli_src.contains("\"login\"") && cli_src.contains("\"logout\""),
        "cli.ts should register login/logout",
    );
    assert!(
        auth_src.contains("runLoginFlow") && auth_src.contains("export async function logout"),
        "auth.ts should export the OAuth helpers"
    );
    assert!(
        auth_src.contains("test-client-id")
            && auth_src.contains("https://auth.example.com/authorize"),
        "auth.ts should bake in OAUTH constants from spec + config"
    );

    // --help shows the new subcommands.
    let help = cli_help(dir.path(), "multi-tenant-shape");
    assert!(help.contains("login"), "missing login in --help: {help}");
    assert!(help.contains("logout"), "missing logout in --help: {help}");

    // `logout` is idempotent — running it on a fresh tempdir with no
    // stored token should exit 0 and print "no stored token". XDG_CONFIG_HOME
    // is redirected so we don't touch the user's real config.
    let xdg = dir.path().join(".xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let bin = dir.path().join("bin/multi-tenant-shape.js");
    let logout_out = std::process::Command::new("node")
        .args([bin.to_str().unwrap(), "logout"])
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .expect("spawn node logout");
    assert!(logout_out.status.success(), "logout failed: {logout_out:?}");
    let logout_stderr = String::from_utf8_lossy(&logout_out.stderr);
    assert!(
        logout_stderr.contains("no stored token"),
        "expected 'no stored token' on stderr, got: {logout_stderr}"
    );
}

// -- error paths -----------------------------------------------------------

#[test]
fn rejects_unknown_config_field() {
    let runner = build_and_load();
    let err = runner
        .generate(petstore_ir(), serde_json::json!({"unknown": "x"}))
        .expect_err("expected config-invalid");
    let msg = format!("{err:?}");
    // Host-side jsonschema validator catches unknown fields before the
    // plugin runs (CONFIG_SCHEMA has additionalProperties: false). The
    // error surfaces as ConfigInvalid.
    assert!(
        msg.to_lowercase().contains("config")
            || msg.to_lowercase().contains("additional")
            || msg.to_lowercase().contains("unknown"),
        "expected config error, got: {msg}"
    );
}
