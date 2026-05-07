//! Real-world smoke tests (issue #17, MVP gate).
//!
//! For each fixture under `fixtures/real-world/`, runs the spec through
//! both generators and asserts the emitted code compiles. Gated behind
//! the `real-world` feature so day-to-day `cargo test` skips the slow
//! tooling. CI runs this via the dedicated `real-world` job.

#![cfg(feature = "real-world")]

mod common;

use std::path::{Path, PathBuf};

use common::{repo_root, runner_for};
use forge_host::GenerationOutput;
use forge_ir::{Ir, Severity};

/// Parse a real-world fixture's `spec.json` and assert no error-severity
/// diagnostics. Returns the canonical IR for downstream tests.
fn parse_clean(fixture: &str) -> Ir {
    let spec_path = repo_root()
        .join("fixtures/real-world")
        .join(fixture)
        .join("spec.json");
    let out = forge_parser::parse_path(&spec_path)
        .unwrap_or_else(|e| panic!("parse {}: {e}", spec_path.display()));

    let errors: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "{fixture}: parser produced {} error-severity diagnostics:\n{}",
        errors.len(),
        errors
            .iter()
            .map(|d| format!("  {} ({:?}): {}", d.code, d.severity, d.message))
            .collect::<Vec<_>>()
            .join("\n")
    );

    out.spec.expect("parser returned no IR despite no errors")
}

/// Materialise a `GenerationOutput` into a tempdir.
fn write_to(out: &GenerationOutput, dir: &Path) {
    for f in &out.files {
        let target = dir.join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&target, &f.content).unwrap();
    }
}

/// `cargo check` the generated Rust crate. Skips if `cargo` isn't on
/// PATH (matches the pattern in `generator_rust_reqwest.rs`).
fn cargo_check(dir: &Path) {
    if which("cargo").is_none() {
        eprintln!("skipping cargo_check: `cargo` not on PATH");
        return;
    }
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("check")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"));
    // See `generator_rust_reqwest.rs` — CI-only target-dir redirect.
    if let Some(td) = std::env::var_os("FORGE_ITEST_CARGO_TARGET_DIR") {
        cmd.arg("--target-dir").arg(td);
    }
    let status = cmd.status().expect("spawn cargo");
    assert!(status.success(), "cargo check failed (status {status:?})");
}

/// `tsc --noEmit` the generated TypeScript crate. Skips if `tsc` isn't on
/// PATH. The generated `tsconfig.json` already sets `noEmit: true` and
/// `strict: true`, so this catches any type-level mistake the generator
/// makes.
fn tsc_check(dir: &Path) {
    if which("tsc").is_none() {
        eprintln!("skipping tsc_check: `tsc` not on PATH");
        return;
    }
    let status = std::process::Command::new("tsc")
        .arg("--noEmit")
        .arg("-p")
        .arg(dir.join("tsconfig.json"))
        .status()
        .expect("spawn tsc");
    assert!(status.success(), "tsc failed (status {status:?})");
}

fn which(bin: &str) -> Option<PathBuf> {
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

fn run_ts(ir: Ir) -> GenerationOutput {
    runner_for("generator-typescript-fetch")
        .generate(ir, serde_json::json!({}))
        .expect("ts generate")
}

fn run_rust(ir: Ir) -> GenerationOutput {
    runner_for("generator-rust-reqwest")
        .generate(ir, serde_json::json!({}))
        .expect("rust generate")
}

// -- stripe-customers ------------------------------------------------------

#[test]
fn stripe_customers_parses_clean() {
    let _ir = parse_clean("stripe-customers");
}

#[test]
fn stripe_customers_typescript_compiles() {
    let ir = parse_clean("stripe-customers");
    let out = run_ts(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    tsc_check(dir.path());
}

#[test]
fn stripe_customers_rust_compiles() {
    let ir = parse_clean("stripe-customers");
    let out = run_rust(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    cargo_check(dir.path());
}

// -- github-issues ---------------------------------------------------------

#[test]
fn github_issues_parses_clean() {
    let _ir = parse_clean("github-issues");
}

#[test]
fn github_issues_typescript_compiles() {
    let ir = parse_clean("github-issues");
    let out = run_ts(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    tsc_check(dir.path());
}

#[test]
fn github_issues_rust_compiles() {
    let ir = parse_clean("github-issues");
    let out = run_rust(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    cargo_check(dir.path());
}

// -- multi-tenant-shape (split-document spec) ------------------------------

#[test]
fn multi_tenant_shape_parses_clean() {
    let ir = parse_clean("multi-tenant-shape");
    // Pin the structural goods: $ref-to-paths-file unrolled, component
    // schemas pulled in, parameter / request-body / response / security
    // refs all resolved.
    assert!(
        ir.operations.iter().any(|o| o.id == "listUsers"),
        "split-document path-item ref didn't unroll: {:?}",
        ir.operations.iter().map(|o| &o.id).collect::<Vec<_>>()
    );
    assert!(
        ir.types.iter().any(|t| t.id == "User"),
        "component-schema ref didn't pull `User` into the type pool"
    );
    assert!(
        !ir.security_schemes.is_empty(),
        "security-scheme ref didn't resolve"
    );
}

#[test]
fn multi_tenant_shape_typescript_compiles() {
    let ir = parse_clean("multi-tenant-shape");
    let out = run_ts(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    tsc_check(dir.path());
}

#[test]
fn multi_tenant_shape_rust_compiles() {
    let ir = parse_clean("multi-tenant-shape");
    let out = run_rust(ir);
    let dir = tempfile::tempdir().expect("tempdir");
    write_to(&out, dir.path());
    cargo_check(dir.path());
}
