//! `forge` CLI.
//!
//! Two configuration modes:
//!
//! - **Project mode** — read `forge.toml` from the project directory.
//!   Supports per-plugin config blocks and is the recommended layout
//!   for repeatable runs.
//! - **Config-less mode** — pass `--input`, `--transformer`,
//!   `--generator`, and `--out` directly on the command line. Useful
//!   for one-off runs and shell scripting; per-plugin config defaults
//!   to `{}`. Triggered when `--input` is set.
//!
//! Two input forms (project mode supports both via `forge.toml`;
//! config-less mode supports spec only):
//!
//! - spec — parse an OpenAPI 3.0 JSON document through `forge-parser`.
//! - ir — load a canonical `forge_ir::Ir` directly (debugging escape
//!   hatch; bypasses the parser). Only reachable via
//!   `[input] ir = "..."` in `forge.toml`.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use forge_host::{Engine, Plugin};
use forge_pipeline::{run as run_pipeline, PipelineConfig, PipelineError};
use serde::Deserialize;

mod docker_auth;
mod oci;

#[derive(Debug, Parser)]
#[command(name = "forge", version, about = "OpenAPI Forge")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the configured pipeline.
    ///
    /// Without `--input`, reads `forge.toml` from `<project>` (project
    /// mode). With `--input`, builds the config from CLI flags and
    /// ignores `forge.toml` (config-less mode). The pre-parsed IR input
    /// form is reachable via `[input] ir = "..."` in `forge.toml`.
    Generate {
        /// Project directory containing `forge.toml`. Used as the path
        /// resolution base in project mode. Ignored in config-less mode.
        #[arg(default_value = ".")]
        project: PathBuf,
        /// OpenAPI 3.0 JSON spec. Triggers config-less mode.
        #[arg(short = 'i', long = "input", value_name = "SPEC")]
        input: Option<PathBuf>,
        /// Transformer plugin (`.wasm` path or OCI ref). Repeat to chain.
        /// Config-less mode only.
        #[arg(long = "transformer", value_name = "REF")]
        transformer: Vec<String>,
        /// Generator plugin (`.wasm` path or OCI ref). Required in
        /// config-less mode.
        #[arg(long = "generator", value_name = "REF")]
        generator: Option<String>,
        /// Output directory. Required in config-less mode; overrides
        /// `[output] dir` from `forge.toml` in project mode.
        #[arg(short = 'o', long = "out")]
        out: Option<PathBuf>,
    },
    /// Print the version of the IR contract this CLI was built against.
    IrVersion,
}

/// Top-level `forge.toml`. Supports two mutually-exclusive layouts:
///
/// - **Single pipeline** (original layout): top-level `[[transformers]]`,
///   `[generator]`, and `[output]` describe one transforms → generator
///   stack, with optional top-level `[hooks]`.
/// - **Multiple pipelines**: one or more `[[pipelines]]` tables, each its
///   own transforms → generator stack writing to its own `[output]` with
///   its own `[pipelines.hooks]`. The top-level `[input]` and `[limits]`
///   become shared defaults that each pipeline may override.
///
/// Mixing the two — top-level stack fields alongside `[[pipelines]]` — is
/// rejected as ambiguous (see [`resolve_pipelines`]).
#[derive(Debug, Deserialize)]
struct Project {
    /// Shared input. Required for the single-pipeline layout; for the
    /// multi-pipeline layout it is the default used by any pipeline that
    /// doesn't declare its own `[pipelines.input]`.
    #[serde(default)]
    input: Option<Input>,
    #[serde(default)]
    transformers: Vec<PluginRef>,
    #[serde(default)]
    generator: Option<PluginRef>,
    #[serde(default)]
    output: Option<Output>,
    /// Shared sandbox-limit overrides. For the multi-pipeline layout, a
    /// pipeline's own `[pipelines.limits]` is layered on top of these.
    #[serde(default)]
    limits: LimitsSection,
    /// Lifecycle hooks for the single-pipeline layout. In the
    /// multi-pipeline layout hooks live per-pipeline instead (use
    /// `[pipelines.hooks]`); a top-level `[hooks]` alongside
    /// `[[pipelines]]` is rejected.
    #[serde(default)]
    hooks: HooksSection,
    /// Multi-pipeline layout. Empty for the single-pipeline layout.
    #[serde(default)]
    pipelines: Vec<Pipeline>,
}

/// One `[[pipelines]]` entry: a self-contained transforms → generator
/// stack. `input` and `limits` fall back to the top-level shared values
/// when omitted; `hooks` are per-pipeline.
#[derive(Debug, Deserialize)]
struct Pipeline {
    /// Optional label, surfaced in progress output to tell pipelines
    /// apart. Purely cosmetic.
    #[serde(default)]
    name: Option<String>,
    /// Per-pipeline input override. Falls back to the top-level `[input]`
    /// when omitted.
    #[serde(default)]
    input: Option<Input>,
    #[serde(default)]
    transformers: Vec<PluginRef>,
    generator: PluginRef,
    output: Output,
    /// Per-pipeline limit overrides, layered on top of the top-level
    /// shared `[limits]`.
    #[serde(default)]
    limits: LimitsSection,
    /// Lifecycle hooks run after this pipeline's output is written,
    /// against this pipeline's output dir.
    #[serde(default)]
    hooks: HooksSection,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Input {
    /// Parse an OpenAPI 3.0 JSON document through `forge-parser`.
    Spec { spec: PathBuf },
    /// Load a canonical `forge_ir::Ir` JSON directly. Bypasses the parser
    /// — useful when iterating on transformers/generators without a real
    /// spec.
    Ir { ir: PathBuf },
}

#[derive(Debug, Deserialize)]
struct PluginRef {
    #[serde(flatten)]
    source: PluginSource,
    /// Plugin-specific JSON-shaped config. Serialized to JSON and handed
    /// to the plugin as the `config` argument.
    #[serde(default = "empty_config")]
    config: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PluginSource {
    /// `.wasm` component path, relative to the project directory.
    Wasm { wasm: PathBuf },
    /// OCI reference, e.g. `ghcr.io/org/plugin:1.0.0` or
    /// `registry.example/repo@sha256:...`. Pulled lazily on
    /// `forge generate` and cached under the user's XDG cache dir.
    Oci { oci: String },
}

fn empty_config() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize)]
struct Output {
    dir: PathBuf,
}

/// `[limits]` section: per-stage-kind overrides for the WASM sandbox
/// limits. Every field is optional; anything unset keeps the built-in
/// default from [`forge_host::Limits`]. Unknown keys are rejected so a
/// typo'd limit fails the run instead of silently keeping the default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsSection {
    #[serde(default)]
    transformer: LimitOverrides,
    #[serde(default)]
    generator: LimitOverrides,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitOverrides {
    fuel: Option<u64>,
    memory_bytes: Option<usize>,
    wall_clock_ms: Option<u64>,
    /// Output caps only apply to the generator stage; transformers
    /// return IR, not files.
    output_files_max: Option<u32>,
    output_total_bytes_max: Option<u64>,
    output_per_file_bytes_max: Option<u64>,
}

impl LimitOverrides {
    fn apply(&self, mut base: forge_host::Limits) -> forge_host::Limits {
        if let Some(v) = self.fuel {
            base.fuel = v;
        }
        if let Some(v) = self.memory_bytes {
            base.memory_bytes = v;
        }
        if let Some(v) = self.wall_clock_ms {
            base.wall_clock_ms = v;
        }
        if let Some(v) = self.output_files_max {
            base.output_files_max = v;
        }
        if let Some(v) = self.output_total_bytes_max {
            base.output_total_bytes_max = v;
        }
        if let Some(v) = self.output_per_file_bytes_max {
            base.output_per_file_bytes_max = v;
        }
        base
    }
}

/// `[hooks]` section: commands to run at lifecycle points. Currently
/// only `post_generate`, which runs once all generated files have been
/// written to the output directory — handy for invoking formatters
/// (`prettier --write .`, `cargo fmt`, ...) over the output. Commands
/// run in order, with the output directory as their working directory,
/// and the first non-zero exit aborts the run. Unknown keys are rejected
/// so a typo fails the run instead of silently doing nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksSection {
    #[serde(default)]
    post_generate: Vec<Hook>,
}

/// A single hook entry. Either a bare command (string or argv array) or
/// a table that wraps a command with per-hook options:
///
/// ```toml
/// post_generate = [
///   "cargo fmt",                                          # bare, shell form
///   ["prettier", "--write", "."],                        # bare, exec form
///   { cmd = "optional-linter", continue_on_error = true } # table form
/// ]
/// ```
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Hook {
    Bare(HookCmd),
    Table(HookTable),
}

/// Table form of a hook: a command plus per-hook options. Unknown keys
/// are rejected so a typo fails the run instead of silently doing
/// nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookTable {
    cmd: HookCmd,
    /// When true, a non-zero exit (or a failure to start) logs a warning
    /// and continues to the next hook instead of aborting the run.
    #[serde(default)]
    continue_on_error: bool,
}

/// The command part of a hook, in one of two forms (cf. Docker's shell
/// vs exec form):
///
/// - **shell form** — a string run through the platform shell
///   (`"eslint --fix && prettier --write ."`). Globs, pipes, `&&`,
///   redirection and `$VAR` expansion all work.
/// - **exec form** — an argv array run directly with no shell
///   (`["cargo", "fmt"]`). Arguments pass through literally (no
///   word-splitting or glob/var expansion) and no shell is required.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HookCmd {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse forge.toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("failed to parse {path} as IR JSON: {source}")]
    BadIr {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("parser failed: {0}")]
    Parse(#[from] forge_parser::ParseError),
    #[error("parser produced {count} error-severity diagnostic(s); halting")]
    ParseDiagnostics { count: usize },
    #[error("engine init: {0}")]
    Engine(String),
    #[error("plugin load ({origin}): {reason}")]
    PluginLoad { origin: String, reason: String },
    #[error("oci pull ({reference}): {source}")]
    Oci {
        reference: String,
        #[source]
        source: oci::OciError,
    },
    #[error("plugin {plugin}: invalid config_schema(): {reason}")]
    ConfigSchemaInvalid { plugin: String, reason: String },
    #[error("plugin {plugin}: config validation failed:\n  - {}", errors.join("\n  - "))]
    ConfigValidation { plugin: String, errors: Vec<String> },
    #[error("pipeline: {0}")]
    Pipeline(#[from] forge_pipeline::PipelineError),
    #[error("output guard: {0}")]
    Output(#[from] forge_host::filesystem::OutputError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("--generator is required in config-less mode (when --input is set)")]
    MissingGenerator,
    #[error("--out is required in config-less mode (when --input is set)")]
    MissingOut,
    #[error("post_generate hook could not start ({command}): {source}")]
    PostGenHookSpawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("post_generate hook failed (exit {code}): {command}")]
    PostGenHookFailed { command: String, code: String },
    #[error(
        "forge.toml mixes the single-pipeline layout (top-level [generator]/[[transformers]]/[output]/[hooks]) \
         with [[pipelines]]; use one or the other"
    )]
    MixedLayout,
    #[error("forge.toml: missing [input] (required when not using [[pipelines]])")]
    MissingInput,
    #[error("forge.toml: missing [generator] (define one, or use [[pipelines]])")]
    MissingGeneratorBlock,
    #[error("forge.toml: missing [output] (required when not using [[pipelines]])")]
    MissingOutput,
    #[error("forge.toml: pipeline `{pipeline}` has no [pipelines.input] and no shared top-level [input]")]
    PipelineNoInput { pipeline: String },
    #[error("--out cannot be used with multiple [[pipelines]]; each pipeline writes to its own [output]")]
    OutOverrideMultiPipeline,
}

impl CliError {
    /// Process exit code for this error. Post-generate hook failures get
    /// their own code (3) so callers can tell "a hook failed" apart from
    /// "forge itself failed" (2) — generation succeeded and files are on
    /// disk in the hook case.
    fn exit_code(&self) -> i32 {
        match self {
            CliError::PostGenHookFailed { .. } | CliError::PostGenHookSpawn { .. } => 3,
            _ => 2,
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Generate {
            project,
            input,
            transformer,
            generator,
            out,
        } => match input {
            Some(spec) => {
                generate_from_args(spec, &transformer, generator.as_deref(), out.as_deref())
            }
            None => generate_from_project(&project, out.as_deref()),
        },
        Cmd::IrVersion => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        let mut src = std::error::Error::source(&e);
        while let Some(err) = src {
            eprintln!("  caused by: {err}");
            src = err.source();
        }
        std::process::exit(e.exit_code());
    }
}

/// Project mode: read `forge.toml` from `project`, then run.
fn generate_from_project(project: &Path, out_override: Option<&Path>) -> Result<(), CliError> {
    let manifest_path = project.join("forge.toml");
    let manifest_str = std::fs::read_to_string(&manifest_path).map_err(|e| CliError::Read {
        path: manifest_path.clone(),
        source: e,
    })?;
    let cfg: Project = toml::from_str(&manifest_str)?;
    run_generate(project, &cfg, out_override)
}

/// Config-less mode: build a `Project` from CLI flags. Plugin paths
/// are resolved relative to the current working directory.
fn generate_from_args(
    spec: PathBuf,
    transformer: &[String],
    generator: Option<&str>,
    out_override: Option<&Path>,
) -> Result<(), CliError> {
    let generator = generator.ok_or(CliError::MissingGenerator)?;
    let out_dir = out_override.ok_or(CliError::MissingOut)?.to_path_buf();

    let cfg = Project {
        input: Some(Input::Spec { spec }),
        transformers: transformer
            .iter()
            .map(|s| PluginRef {
                source: parse_plugin_arg(s),
                config: empty_config(),
            })
            .collect(),
        generator: Some(PluginRef {
            source: parse_plugin_arg(generator),
            config: empty_config(),
        }),
        output: Some(Output {
            dir: out_dir.clone(),
        }),
        limits: LimitsSection::default(),
        hooks: HooksSection::default(),
        pipelines: Vec::new(),
    };

    // In config-less mode, paths in CLI args are relative to CWD; pass
    // `.` as the resolution base. The output dir is already absolute or
    // CWD-relative, so we forward it as the override too.
    run_generate(Path::new("."), &cfg, Some(&out_dir))
}

/// `s` is either a `.wasm` path on disk or an OCI reference. Heuristic:
/// if it ends in `.wasm` or names an existing file, treat as a path;
/// otherwise treat as an OCI ref. The OCI puller will surface a parse
/// error if the string is neither.
fn parse_plugin_arg(s: &str) -> PluginSource {
    let path = Path::new(s);
    let looks_like_wasm = s.ends_with(".wasm") || path.is_file();
    if looks_like_wasm {
        PluginSource::Wasm {
            wasm: path.to_path_buf(),
        }
    } else {
        PluginSource::Oci { oci: s.to_owned() }
    }
}

/// A fully-resolved pipeline: everything needed to run one transforms →
/// generator stack, with shared top-level defaults already folded in.
#[derive(Debug)]
struct ResolvedPipeline<'a> {
    name: Option<&'a str>,
    input: &'a Input,
    transformers: &'a [PluginRef],
    generator: &'a PluginRef,
    output: &'a Output,
    hooks: &'a HooksSection,
    transformer_limits: forge_host::Limits,
    generator_limits: forge_host::Limits,
}

/// Flatten a [`Project`] into the ordered list of pipelines to run.
///
/// Handles both layouts and folds the shared top-level `[input]` /
/// `[limits]` into each multi-pipeline entry. A manifest that mixes the
/// two layouts (top-level stack fields *and* `[[pipelines]]`) is rejected,
/// since which one wins would be ambiguous.
fn resolve_pipelines(cfg: &Project) -> Result<Vec<ResolvedPipeline<'_>>, CliError> {
    let has_top_stack = !cfg.transformers.is_empty()
        || cfg.generator.is_some()
        || cfg.output.is_some()
        || !cfg.hooks.post_generate.is_empty();

    if cfg.pipelines.is_empty() {
        // Single-pipeline layout: the top-level stack fields are the one
        // and only pipeline.
        let input = cfg.input.as_ref().ok_or(CliError::MissingInput)?;
        let generator = cfg
            .generator
            .as_ref()
            .ok_or(CliError::MissingGeneratorBlock)?;
        let output = cfg.output.as_ref().ok_or(CliError::MissingOutput)?;
        return Ok(vec![ResolvedPipeline {
            name: None,
            input,
            transformers: &cfg.transformers,
            generator,
            output,
            hooks: &cfg.hooks,
            transformer_limits: cfg
                .limits
                .transformer
                .apply(forge_host::Limits::transformer()),
            generator_limits: cfg.limits.generator.apply(forge_host::Limits::generator()),
        }]);
    }

    // Multi-pipeline layout.
    if has_top_stack {
        return Err(CliError::MixedLayout);
    }
    cfg.pipelines
        .iter()
        .map(|p| {
            let input = p.input.as_ref().or(cfg.input.as_ref()).ok_or_else(|| {
                CliError::PipelineNoInput {
                    pipeline: p.name.clone().unwrap_or_else(|| "<unnamed>".into()),
                }
            })?;
            // Limit overrides compose by field: layer the pipeline's own
            // overrides on top of the shared top-level overrides on top of
            // the built-in defaults.
            let transformer_limits = p.limits.transformer.apply(
                cfg.limits
                    .transformer
                    .apply(forge_host::Limits::transformer()),
            );
            let generator_limits = p
                .limits
                .generator
                .apply(cfg.limits.generator.apply(forge_host::Limits::generator()));
            Ok(ResolvedPipeline {
                name: p.name.as_deref(),
                input,
                transformers: &p.transformers,
                generator: &p.generator,
                output: &p.output,
                hooks: &p.hooks,
                transformer_limits,
                generator_limits,
            })
        })
        .collect()
}

fn run_generate(
    project: &Path,
    cfg: &Project,
    out_override: Option<&Path>,
) -> Result<(), CliError> {
    let pipelines = resolve_pipelines(cfg)?;

    // `--out` retargets a single output dir, which is meaningless when each
    // pipeline owns its own. Fail loudly instead of silently writing every
    // pipeline over the same directory.
    if out_override.is_some() && pipelines.len() > 1 {
        return Err(CliError::OutOverrideMultiPipeline);
    }

    let engine = build_engine()?;
    let total = pipelines.len();
    for (i, p) in pipelines.iter().enumerate() {
        run_one_pipeline(project, &engine, p, out_override, i, total)?;
    }
    Ok(())
}

/// Run a single resolved pipeline: load its IR, load its plugins, drive
/// the transforms → generator stack, write the output files, and run its
/// post-generate hooks.
fn run_one_pipeline(
    project: &Path,
    engine: &Engine,
    p: &ResolvedPipeline<'_>,
    out_override: Option<&Path>,
    index: usize,
    total: usize,
) -> Result<(), CliError> {
    let ir = load_ir(project, p.input)?;

    let mut transformers: Vec<Plugin> = Vec::with_capacity(p.transformers.len());
    let mut configs: Vec<String> = Vec::with_capacity(p.transformers.len() + 1);
    for t in p.transformers {
        let (bytes, label) = load_plugin_bytes(project, &t.source)?;
        let plugin =
            Plugin::load_transformer(engine, &bytes).map_err(|e| CliError::PluginLoad {
                origin: label.clone(),
                reason: e.to_string(),
            })?;
        validate_config(plugin.config_schema(), &t.config, &label)?;
        transformers.push(plugin);
        configs.push(t.config.to_string());
    }

    let (gen_bytes, gen_label) = load_plugin_bytes(project, &p.generator.source)?;
    let generator =
        Plugin::load_generator(engine, &gen_bytes).map_err(|e| CliError::PluginLoad {
            origin: gen_label.clone(),
            reason: e.to_string(),
        })?;
    validate_config(generator.config_schema(), &p.generator.config, &gen_label)?;
    configs.push(p.generator.config.to_string());

    let pipe_cfg = PipelineConfig {
        configs,
        transformer_limits: p.transformer_limits,
        generator_limits: p.generator_limits,
        ..Default::default()
    };
    let xforms: Vec<&Plugin> = transformers.iter().collect();
    let out = match run_pipeline(engine, ir, &xforms, &generator, &pipe_cfg) {
        Ok(out) => out,
        // A stage that halts the pipeline with error-severity diagnostics
        // carries them out; print each one (same rendering as parse-time
        // diagnostics) so the failure says *what* was wrong, not just how
        // many things were.
        Err(e) => {
            if let PipelineError::StageErrors { diagnostics, .. } = &e {
                print_diagnostics(diagnostics);
            }
            return Err(e.into());
        }
    };

    // Validate output before writing. Use the generator's limits to seed
    // the caps; this matches what the host enforced inside the WASM call.
    let caps = forge_host::filesystem::Caps::from_limits(p.generator_limits);
    forge_host::filesystem::validate_output(&out.generation.files, caps)?;

    let out_dir = out_override
        .map(|o| o.to_path_buf())
        .unwrap_or_else(|| project.join(&p.output.dir));
    std::fs::create_dir_all(&out_dir)?;
    for f in &out.generation.files {
        let target = out_dir.join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &f.content)?;
    }

    // Tag the summary with the pipeline's name / position only when there
    // is more than one, so the single-pipeline output stays unchanged.
    let tag = if total > 1 {
        let label = p
            .name
            .map(|n| format!(" {n}"))
            .unwrap_or_else(|| format!(" {}/{}", index + 1, total));
        format!("[pipeline{label}] ")
    } else {
        String::new()
    };
    println!(
        "{tag}wrote {} file(s) to {} ({} diagnostic(s))",
        out.generation.files.len(),
        out_dir.display(),
        out.diagnostics.len(),
    );

    run_post_generate_hooks(p.hooks, &out_dir, project)?;
    Ok(())
}

impl Hook {
    /// The command to run, regardless of bare vs table form.
    fn cmd(&self) -> &HookCmd {
        match self {
            Hook::Bare(c) => c,
            Hook::Table(t) => &t.cmd,
        }
    }

    /// Whether a failure of this hook should be tolerated. Only the table
    /// form can opt in; bare hooks always abort the run on failure.
    fn continue_on_error(&self) -> bool {
        match self {
            Hook::Bare(_) => false,
            Hook::Table(t) => t.continue_on_error,
        }
    }
}

impl HookCmd {
    /// A human-readable rendering of the command, used in log lines and
    /// error messages.
    fn label(&self) -> String {
        match self {
            HookCmd::Shell(s) => s.clone(),
            HookCmd::Exec(argv) => argv.join(" "),
        }
    }

    /// Build the process to spawn. Shell form goes through the platform
    /// shell; exec form runs the argv directly. Returns `None` for an
    /// empty exec array, which has no program to run.
    fn command(&self) -> Option<std::process::Command> {
        match self {
            HookCmd::Shell(s) => Some(shell_command(s)),
            HookCmd::Exec(argv) => {
                let (program, args) = argv.split_first()?;
                let mut c = std::process::Command::new(program);
                c.args(args);
                Some(c)
            }
        }
    }
}

/// Run `[hooks] post_generate` commands in order, once generated files
/// are on disk. Each command runs with the output directory as its
/// working directory and inherits stdio, so a formatter's output reaches
/// the user.
///
/// Two absolute paths are exported for commands that need to anchor
/// arguments: `FORGE_OUT_DIR` (where files were written) and
/// `FORGE_MANIFEST_DIR` (the directory containing `forge.toml`). Both are
/// made absolute so hooks can build paths regardless of their working
/// directory — relative values would be near-useless once the hook runs
/// with `out_dir` as its cwd. The first command to exit non-zero aborts
/// the run.
fn run_post_generate_hooks(
    hooks: &HooksSection,
    out_dir: &Path,
    manifest_dir: &Path,
) -> Result<(), CliError> {
    // Absolutize without touching the filesystem (no symlink resolution),
    // falling back to the original path if the cwd can't be read.
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let abs_out = abs(out_dir);
    let abs_manifest = abs(manifest_dir);
    for hook in &hooks.post_generate {
        let label = hook.cmd().label();
        // An empty exec array is malformed config, not a runtime failure;
        // reject it regardless of continue_on_error.
        let mut command = hook
            .cmd()
            .command()
            .ok_or_else(|| CliError::PostGenHookFailed {
                command: "[]".to_owned(),
                code: "empty command".to_owned(),
            })?;
        println!("running post_generate hook: {label}");
        let outcome = command
            .current_dir(&abs_out)
            .env("FORGE_OUT_DIR", &abs_out)
            .env("FORGE_MANIFEST_DIR", &abs_manifest)
            .status();
        match outcome {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let code = status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_owned());
                if hook.continue_on_error() {
                    eprintln!(
                        "warning: post_generate hook failed (exit {code}), continuing: {label}"
                    );
                } else {
                    return Err(CliError::PostGenHookFailed {
                        command: label,
                        code,
                    });
                }
            }
            Err(source) => {
                if hook.continue_on_error() {
                    eprintln!(
                        "warning: post_generate hook could not start ({label}), continuing: {source}"
                    );
                } else {
                    return Err(CliError::PostGenHookSpawn {
                        command: label,
                        source,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Build a shell invocation for a hook command line.
#[cfg(unix)]
fn shell_command(command: &str) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(windows)]
fn shell_command(command: &str) -> std::process::Command {
    let mut c = std::process::Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

/// Build the wasmtime engine, backed by an on-disk compilation cache so
/// plugin components aren't recompiled from scratch on every run (the
/// dominant per-invocation cost). The cache lives alongside the OCI plugin
/// store under the forge cache dir. If the cache directory can't be set up
/// (e.g. no resolvable cache dir), fall back to an uncached engine with a
/// warning rather than failing the run — caching is an optimisation, not a
/// correctness requirement.
fn build_engine() -> Result<Engine, CliError> {
    match oci::compiled_cache_dir() {
        Ok(dir) => match Engine::with_cache(&dir) {
            Ok(engine) => return Ok(engine),
            Err(e) => tracing::warn!(
                "compilation cache disabled ({e}); plugins will be recompiled each run"
            ),
        },
        Err(e) => tracing::warn!(
            "compilation cache disabled (no cache dir: {e}); plugins will be recompiled each run"
        ),
    }
    Engine::new().map_err(|e| CliError::Engine(e.to_string()))
}

/// Load the IR for the configured `[input]`, branching on whether the
/// project asks for a parsed spec or raw IR JSON.
fn load_ir(project: &Path, input: &Input) -> Result<forge_ir::Ir, CliError> {
    match input {
        Input::Spec { spec } => {
            let spec_path = project.join(spec);
            // `parse_path` enables the file-based $ref resolver so specs
            // that split components / paths across sibling files (the
            // common multi-document layout) work. The in-memory
            // `parse_str_with_file` rejects external $refs outright;
            // using it here would regress split-document support shipped
            // in #56.
            let out = forge_parser::parse_path(&spec_path)?;
            print_diagnostics(&out.diagnostics);
            let errs = out
                .diagnostics
                .iter()
                .filter(|d| d.severity == forge_ir::Severity::Error)
                .count();
            if errs > 0 {
                return Err(CliError::ParseDiagnostics { count: errs });
            }
            out.spec
                .ok_or(CliError::ParseDiagnostics { count: errs.max(1) })
        }
        Input::Ir { ir } => {
            let ir_path = project.join(ir);
            let ir_str = std::fs::read_to_string(&ir_path).map_err(|e| CliError::Read {
                path: ir_path.clone(),
                source: e,
            })?;
            serde_json::from_str(&ir_str).map_err(|e| CliError::BadIr {
                path: ir_path,
                source: e,
            })
        }
    }
}

/// Friendly label for diagnostics — the wasm filename, falling back to the
/// full path if it has no filename component.
fn plugin_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Resolve a `PluginSource` to raw component bytes plus a human label
/// for diagnostics. Filesystem refs read from `project`-relative paths;
/// OCI refs are pulled (or served from cache) by `oci::fetch_to_bytes`.
fn load_plugin_bytes(project: &Path, source: &PluginSource) -> Result<(Vec<u8>, String), CliError> {
    match source {
        PluginSource::Wasm { wasm } => {
            let path = project.join(wasm);
            let bytes = std::fs::read(&path).map_err(|e| CliError::Read {
                path: path.clone(),
                source: e,
            })?;
            Ok((bytes, plugin_label(&path)))
        }
        PluginSource::Oci { oci } => {
            let bytes = oci::fetch_to_bytes(oci).map_err(|source| CliError::Oci {
                reference: oci.clone(),
                source,
            })?;
            Ok((bytes, oci.clone()))
        }
    }
}

/// Validate `config` against the JSON Schema returned by a plugin's
/// `config_schema()` export. The schema is parsed once per plugin load; on
/// validation failure we surface every error so the user can fix them in one
/// pass instead of re-running for each.
fn validate_config(
    schema_str: &str,
    config: &serde_json::Value,
    plugin_label: &str,
) -> Result<(), CliError> {
    let schema_value: serde_json::Value =
        serde_json::from_str(schema_str).map_err(|e| CliError::ConfigSchemaInvalid {
            plugin: plugin_label.to_owned(),
            reason: e.to_string(),
        })?;
    let validator =
        jsonschema::validator_for(&schema_value).map_err(|e| CliError::ConfigSchemaInvalid {
            plugin: plugin_label.to_owned(),
            reason: e.to_string(),
        })?;
    let errors: Vec<String> = validator
        .iter_errors(config)
        .map(|e| format!("{} (at {})", e, e.instance_path()))
        .collect();
    if !errors.is_empty() {
        return Err(CliError::ConfigValidation {
            plugin: plugin_label.to_owned(),
            errors,
        });
    }
    Ok(())
}

fn print_diagnostics(diagnostics: &[forge_ir::Diagnostic]) {
    use forge_ir::Severity;
    for d in diagnostics {
        let label = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        };
        let location = d
            .location
            .as_ref()
            .map(|l| {
                if let Some(file) = &l.file {
                    format!("{}#{}", file, l.pointer)
                } else {
                    l.pointer.clone()
                }
            })
            .unwrap_or_default();
        eprintln!("{label} [{}] {} ({})", d.code, d.message, location);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_MANIFEST: &str = r#"
[input]
spec = "openapi.json"

[generator]
wasm = "gen.wasm"

[output]
dir = "out"
"#;

    #[test]
    fn limits_section_defaults_when_absent() {
        let cfg: Project = toml::from_str(BASE_MANIFEST).unwrap();
        let defaults = forge_host::Limits::generator();
        let resolved = cfg.limits.generator.apply(defaults);
        assert_eq!(resolved.fuel, defaults.fuel);
        assert_eq!(
            resolved.output_total_bytes_max,
            defaults.output_total_bytes_max
        );
    }

    #[test]
    fn limits_overrides_apply_per_stage_kind() {
        let manifest = format!(
            "{BASE_MANIFEST}\n\
             [limits.transformer]\n\
             fuel = 9000000000\n\n\
             [limits.generator]\n\
             fuel = 100000000000\n\
             output_files_max = 50000\n\
             output_total_bytes_max = 1073741824\n\
             output_per_file_bytes_max = 134217728\n\
             memory_bytes = 1073741824\n\
             wall_clock_ms = 60000\n"
        );
        let cfg: Project = toml::from_str(&manifest).unwrap();

        let t = cfg
            .limits
            .transformer
            .apply(forge_host::Limits::transformer());
        assert_eq!(t.fuel, 9_000_000_000);
        // Untouched fields keep the built-in defaults.
        assert_eq!(
            t.memory_bytes,
            forge_host::Limits::transformer().memory_bytes
        );

        let g = cfg.limits.generator.apply(forge_host::Limits::generator());
        assert_eq!(g.fuel, 100_000_000_000);
        assert_eq!(g.output_files_max, 50_000);
        assert_eq!(g.output_total_bytes_max, 1024 * 1024 * 1024);
        assert_eq!(g.output_per_file_bytes_max, 128 * 1024 * 1024);
        assert_eq!(g.memory_bytes, 1024 * 1024 * 1024);
        assert_eq!(g.wall_clock_ms, 60_000);
    }

    #[test]
    fn limits_unknown_key_is_rejected() {
        let manifest = format!("{BASE_MANIFEST}\n[limits.generator]\nfeul = 1\n");
        let err = toml::from_str::<Project>(&manifest).unwrap_err();
        assert!(err.to_string().contains("feul"), "{err}");
    }

    #[test]
    fn hooks_default_to_empty_when_absent() {
        let cfg: Project = toml::from_str(BASE_MANIFEST).unwrap();
        assert!(cfg.hooks.post_generate.is_empty());
    }

    #[test]
    fn hooks_post_generate_parses_all_forms_in_order() {
        let manifest = format!(
            "{BASE_MANIFEST}\n\
             [hooks]\n\
             post_generate = [\
               \"prettier --write .\", \
               [\"cargo\", \"fmt\"], \
               {{ cmd = \"optional-linter\", continue_on_error = true }}\
             ]\n"
        );
        let cfg: Project = toml::from_str(&manifest).unwrap();
        let hooks = &cfg.hooks.post_generate;
        let labels: Vec<String> = hooks.iter().map(|h| h.cmd().label()).collect();
        assert_eq!(
            labels,
            vec!["prettier --write .", "cargo fmt", "optional-linter"]
        );
        assert!(matches!(hooks[0], Hook::Bare(HookCmd::Shell(_))));
        assert!(matches!(hooks[1], Hook::Bare(HookCmd::Exec(_))));
        assert!(matches!(hooks[2], Hook::Table(_)));
        // continue_on_error only the table form opts into.
        assert_eq!(
            hooks
                .iter()
                .map(Hook::continue_on_error)
                .collect::<Vec<_>>(),
            vec![false, false, true]
        );
    }

    #[test]
    fn hooks_table_continue_on_error_defaults_false() {
        let manifest = format!("{BASE_MANIFEST}\n[hooks]\npost_generate = [{{ cmd = \"x\" }}]\n");
        let cfg: Project = toml::from_str(&manifest).unwrap();
        assert!(!cfg.hooks.post_generate[0].continue_on_error());
    }

    #[test]
    fn hooks_table_supports_exec_form_cmd() {
        let manifest = format!(
            "{BASE_MANIFEST}\n[hooks]\npost_generate = [{{ cmd = [\"cargo\", \"fmt\"] }}]\n"
        );
        let cfg: Project = toml::from_str(&manifest).unwrap();
        assert!(matches!(
            cfg.hooks.post_generate[0],
            Hook::Table(HookTable {
                cmd: HookCmd::Exec(_),
                ..
            })
        ));
    }

    #[test]
    fn hooks_empty_exec_form_has_no_command() {
        // An empty argv array parses but produces no program to run; the
        // runner turns this into an error rather than spawning nothing.
        let cmd = HookCmd::Exec(vec![]);
        assert!(cmd.command().is_none());
    }

    #[test]
    fn hooks_unknown_key_is_rejected() {
        let manifest = format!("{BASE_MANIFEST}\n[hooks]\npost_gen = [\"x\"]\n");
        let err = toml::from_str::<Project>(&manifest).unwrap_err();
        assert!(err.to_string().contains("post_gen"), "{err}");
    }

    #[test]
    fn hooks_table_unknown_key_is_rejected() {
        // A typo'd option inside the table form must not be silently
        // ignored. (Reached through an untagged enum, so the message is
        // generic — the important property is that it errors.)
        let manifest = format!(
            "{BASE_MANIFEST}\n[hooks]\npost_generate = [{{ cmd = \"x\", contineu_on_error = true }}]\n"
        );
        assert!(toml::from_str::<Project>(&manifest).is_err());
    }

    #[test]
    fn single_pipeline_layout_resolves_to_one() {
        let cfg: Project = toml::from_str(BASE_MANIFEST).unwrap();
        let pipelines = resolve_pipelines(&cfg).unwrap();
        assert_eq!(pipelines.len(), 1);
        assert!(pipelines[0].name.is_none());
    }

    const MULTI_MANIFEST: &str = r#"
[input]
spec = "openapi.json"

[[pipelines]]
name = "a"

[pipelines.generator]
wasm = "gen-a.wasm"

[pipelines.output]
dir = "out/a"

[[pipelines]]
name = "b"
[pipelines.input]
ir = "b.json"

[pipelines.generator]
wasm = "gen-b.wasm"

[pipelines.output]
dir = "out/b"
"#;

    #[test]
    fn multi_pipeline_resolves_each_with_shared_input_fallback() {
        let cfg: Project = toml::from_str(MULTI_MANIFEST).unwrap();
        let pipelines = resolve_pipelines(&cfg).unwrap();
        assert_eq!(pipelines.len(), 2);

        assert_eq!(pipelines[0].name, Some("a"));
        // Pipeline `a` has no [pipelines.input]; it inherits the shared spec.
        assert!(matches!(pipelines[0].input, Input::Spec { .. }));

        assert_eq!(pipelines[1].name, Some("b"));
        // Pipeline `b` overrides the input with its own IR.
        assert!(matches!(pipelines[1].input, Input::Ir { .. }));
    }

    #[test]
    fn multi_pipeline_limits_layer_over_shared_defaults() {
        // Top-level fuel is the shared default; pipeline `b` overrides it.
        let manifest = r#"
[input]
spec = "openapi.json"

[limits.generator]
fuel = 5000

[[pipelines]]
name = "inherits"
[pipelines.generator]
wasm = "a.wasm"
[pipelines.output]
dir = "out/a"

[[pipelines]]
name = "overrides"
[pipelines.generator]
wasm = "b.wasm"
[pipelines.output]
dir = "out/b"
[pipelines.limits.generator]
fuel = 9999
"#;
        let cfg: Project = toml::from_str(manifest).unwrap();
        let pipelines = resolve_pipelines(&cfg).unwrap();
        assert_eq!(pipelines[0].generator_limits.fuel, 5000);
        assert_eq!(pipelines[1].generator_limits.fuel, 9999);
    }

    #[test]
    fn mixed_layout_is_rejected() {
        let manifest = format!(
            "{BASE_MANIFEST}\n[[pipelines]]\nname = \"x\"\n\
             [pipelines.generator]\nwasm = \"g.wasm\"\n\
             [pipelines.output]\ndir = \"o\"\n"
        );
        let cfg: Project = toml::from_str(&manifest).unwrap();
        let err = resolve_pipelines(&cfg).unwrap_err();
        assert!(matches!(err, CliError::MixedLayout), "{err}");
    }

    #[test]
    fn top_level_hooks_with_pipelines_is_rejected() {
        // [hooks] is part of the single-pipeline stack; pairing it with
        // [[pipelines]] is the same ambiguity as a top-level [generator].
        let manifest = r#"
[input]
spec = "openapi.json"

[hooks]
post_generate = ["echo hi"]

[[pipelines]]
name = "x"
[pipelines.generator]
wasm = "g.wasm"
[pipelines.output]
dir = "o"
"#;
        let cfg: Project = toml::from_str(manifest).unwrap();
        let err = resolve_pipelines(&cfg).unwrap_err();
        assert!(matches!(err, CliError::MixedLayout), "{err}");
    }

    #[test]
    fn pipeline_without_any_input_is_rejected() {
        // No top-level [input] and the pipeline declares none either.
        let manifest = r#"
[[pipelines]]
name = "orphan"
[pipelines.generator]
wasm = "g.wasm"
[pipelines.output]
dir = "o"
"#;
        let cfg: Project = toml::from_str(manifest).unwrap();
        let err = resolve_pipelines(&cfg).unwrap_err();
        assert!(
            matches!(err, CliError::PipelineNoInput { ref pipeline } if pipeline == "orphan"),
            "{err}"
        );
    }
}
