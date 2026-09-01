//! Pipeline driver.
//!
//! Run a parsed IR through a chain of transformer plugins, then a single
//! generator plugin, collecting diagnostics and respecting the halt-on-error
//! policy.
//!
//! The driver does **not** call the parser: it accepts an already-parsed
//! [`forge_ir::Ir`]. This keeps `forge-pipeline` independent of the spec
//! parser, so it composes cleanly in tests and in the test harness.

use forge_host::{Engine, GenerationOutput, Limits, Plugin, StageError};
use forge_ir::{Diagnostic, Ir, Severity};

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// JSON config strings per stage. Indexed parallel to `transformers`,
    /// followed by the generator config at the end. Stages with no config
    /// receive the empty object `"{}"`.
    pub configs: Vec<String>,
    pub policy: StagePolicy,
    /// Sandbox limits applied to every transformer stage.
    pub transformer_limits: Limits,
    /// Sandbox limits applied to the generator stage.
    pub generator_limits: Limits,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            configs: Vec::new(),
            policy: StagePolicy::default(),
            transformer_limits: Limits::transformer(),
            generator_limits: Limits::generator(),
        }
    }
}

/// Halt-on-error policy. Default halts the pipeline before the next stage
/// if the preceding stage produced any `error`-severity diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StagePolicy {
    #[default]
    HaltOnError,
    AllowErrors,
}

#[derive(Debug)]
pub struct PipelineOutput {
    pub generation: GenerationOutput,
    /// Aggregated diagnostics from every stage, in execution order.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("transformer `{plugin}` failed: {source}")]
    Transformer {
        plugin: String,
        #[source]
        source: StageError,
    },
    #[error("generator `{plugin}` failed: {source}")]
    Generator {
        plugin: String,
        #[source]
        source: StageError,
    },
    #[error("stage `{plugin}` produced {count} error-severity diagnostics; halting")]
    StageErrors {
        plugin: String,
        count: usize,
        /// The halting stage's diagnostics, carried out so callers can
        /// render them (the CLI prints each one) instead of being told
        /// only how many there were.
        diagnostics: Vec<Diagnostic>,
    },
}

/// Run the configured pipeline.
///
/// `transformers` are applied in order. The result is then fed to
/// `generator`. Diagnostics are aggregated and surfaced via
/// [`PipelineOutput::diagnostics`]; the generator's files come back via
/// [`PipelineOutput::generation`].
pub fn run(
    _engine: &Engine,
    spec: Ir,
    transformers: &[&Plugin],
    generator: &Plugin,
    cfg: &PipelineConfig,
) -> Result<PipelineOutput, PipelineError> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut current = spec;

    let empty = "{}".to_string();
    let cfg_or_empty = |i: usize| cfg.configs.get(i).unwrap_or(&empty).clone();

    for (i, t) in transformers.iter().enumerate() {
        let stage_cfg = cfg_or_empty(i);
        let out = t
            .transform(current, &stage_cfg, cfg.transformer_limits)
            .map_err(|e| PipelineError::Transformer {
                plugin: t.info().name.clone(),
                source: e,
            })?;
        let err_count = out
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        if err_count > 0 && cfg.policy == StagePolicy::HaltOnError {
            return Err(PipelineError::StageErrors {
                plugin: t.info().name.clone(),
                count: err_count,
                diagnostics: out.diagnostics,
            });
        }
        diagnostics.extend(out.diagnostics);
        current = out.spec;
    }

    let gen_cfg = cfg_or_empty(transformers.len());
    let gen_out = generator
        .generate(current, &gen_cfg, cfg.generator_limits)
        .map_err(|e| PipelineError::Generator {
            plugin: generator.info().name.clone(),
            source: e,
        })?;
    let err_count = gen_out
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    if err_count > 0 && cfg.policy == StagePolicy::HaltOnError {
        return Err(PipelineError::StageErrors {
            plugin: generator.info().name.clone(),
            count: err_count,
            diagnostics: gen_out.diagnostics,
        });
    }
    diagnostics.extend(gen_out.diagnostics.iter().cloned());

    Ok(PipelineOutput {
        generation: gen_out,
        diagnostics,
    })
}
