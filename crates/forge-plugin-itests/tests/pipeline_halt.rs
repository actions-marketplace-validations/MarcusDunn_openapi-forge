//! Pipeline halt-path coverage.
//!
//! Under the default `HaltOnError` policy, a stage that emits
//! error-severity diagnostics stops the pipeline. `forge_pipeline::run`
//! must carry that stage's diagnostics out in `PipelineError::StageErrors`
//! — not just a count — so the CLI (and any other caller) can render
//! *what* was wrong. This exercises that surfacing against real `.wasm`
//! plugins through the host runtime.

mod common;

use common::runner_for;
use forge_ir::{ApiInfo, HttpMethod, Ir, Operation, Severity};
use forge_pipeline::{run, PipelineConfig, PipelineError};

fn op(id: &str, original_id: Option<&str>) -> Operation {
    Operation {
        id: id.into(),
        original_id: original_id.map(str::to_string),
        method: HttpMethod::Get,
        path_template: format!("/{id}"),
        path_params: vec![],
        query_params: vec![],
        header_params: vec![],
        cookie_params: vec![],
        querystring_params: vec![],
        request_body: None,
        responses: vec![],
        security: vec![],
        tags: vec![],
        summary: None,
        description: None,
        deprecated: false,
        external_docs: None,
        extensions: vec![],
        servers: vec![],
        callbacks: vec![],
        location: None,
    }
}

fn ir(ops: Vec<Operation>) -> Ir {
    Ir {
        info: ApiInfo {
            title: "test-api".into(),
            version: "1.0.0".into(),
            summary: None,
            description: None,
            terms_of_service: None,
            contact: None,
            license_name: None,
            license_url: None,
            license_identifier: None,
            extensions: vec![],
        },
        operations: ops,
        types: vec![],
        security_schemes: vec![],
        servers: vec![],
        webhooks: vec![],
        external_docs: None,
        tags: vec![],
        json_schema_dialect: None,
        self_url: None,
        values: vec![],
    }
}

#[test]
fn halt_surfaces_the_stage_diagnostics() {
    // `validator-required-operation-id` emits one error diagnostic for the
    // op missing `operationId`; `generator-debug-dump` only fills the
    // mandatory generator slot (it never runs — the transformer halts the
    // pipeline first). `run` ignores its `engine` argument and drives each
    // self-contained `Plugin`, so the two runners' separate engines are
    // fine.
    let validator = runner_for("validator-required-operation-id");
    let generator = runner_for("generator-debug-dump");

    let err = run(
        validator.engine(),
        ir(vec![op("derived", None)]),
        &[validator.plugin()],
        generator.plugin(),
        &PipelineConfig::default(),
    )
    .expect_err("a stage emitting error-severity diagnostics must halt the pipeline");

    let PipelineError::StageErrors {
        plugin,
        diagnostics,
        ..
    } = err
    else {
        panic!("expected StageErrors, got {err:?}");
    };

    assert_eq!(plugin, "validator-required-operation-id");
    assert_eq!(
        diagnostics.len(),
        1,
        "the halting stage's diagnostics must be carried out, not dropped"
    );
    assert_eq!(diagnostics[0].severity, Severity::Error);
    assert_eq!(
        diagnostics[0].code,
        "validator-required-operation-id/E-MISSING-ID"
    );
}
