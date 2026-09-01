//! Plugin runtime built on `wasmtime`.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{AsContextMut, Engine as WtEngine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use forge_ir::{Diagnostic, Ir, PluginInfo};
use forge_ir_bindgen::bindings;
use forge_ir_bindgen::convert::{self, ResourceKindRepr, StageErrorRepr};

// -----------------------------------------------------------------------------
// Public outputs
// -----------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StageError {
    #[error("plugin rejected input: {reason}")]
    Rejected {
        reason: String,
        diagnostics: Vec<Diagnostic>,
    },
    #[error("plugin trap or malformed return: {0}")]
    PluginBug(String),
    #[error("plugin config invalid: {0}")]
    ConfigInvalid(String),
    #[error("plugin exceeded {0:?}")]
    ResourceExceeded(ResourceKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Fuel,
    Memory,
    Time,
    OutputSize,
}

#[derive(Debug, Clone)]
pub struct TransformOutput {
    pub spec: Ir,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
pub struct GenerationOutput {
    pub files: Vec<OutputFile>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
pub struct OutputFile {
    pub path: String,
    pub content: Vec<u8>,
    pub mode: FileMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    Text,
    Binary,
    Executable,
}

// -----------------------------------------------------------------------------
// Limits
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub wall_clock_ms: u64,
    pub output_files_max: u32,
    pub output_total_bytes_max: u64,
    pub output_per_file_bytes_max: u64,
}

impl Limits {
    pub const fn transformer() -> Self {
        Self {
            fuel: 5_000_000_000,
            memory_bytes: 128 * 1024 * 1024,
            wall_clock_ms: 5_000,
            output_files_max: 0,
            output_total_bytes_max: 0,
            output_per_file_bytes_max: 0,
        }
    }

    pub const fn generator() -> Self {
        Self {
            fuel: 50_000_000_000,
            memory_bytes: 512 * 1024 * 1024,
            wall_clock_ms: 30_000,
            output_files_max: 10_000,
            output_total_bytes_max: 256 * 1024 * 1024,
            output_per_file_bytes_max: 16 * 1024 * 1024,
        }
    }
}

// -----------------------------------------------------------------------------
// Engine
// -----------------------------------------------------------------------------

/// Shared `wasmtime::Engine` plus a background thread that ticks the epoch
/// counter so per-store wall-clock deadlines can fire.
///
/// One `Engine` per process is enough; cloning a `Plugin` reuses the engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    wt: WtEngine,
    /// Held to keep the epoch ticker alive for the engine's lifetime.
    _ticker: EpochTicker,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    /// Build an engine with no on-disk compilation cache. Every plugin is
    /// recompiled from scratch on load. Used by tests and the harness where
    /// a shared cache directory would add cross-run coupling.
    pub fn new() -> Result<Self, EngineError> {
        Self::build(None)
    }

    /// Build an engine backed by wasmtime's on-disk compilation cache rooted
    /// at `cache_dir`. Compiling a plugin component (`Plugin::load_*`) is the
    /// dominant per-invocation cost — hundreds of milliseconds of Cranelift
    /// codegen for a multi-MB plugin — and it is pure: a function of the wasm
    /// bytes, the compiler config, and the wasmtime version. The cache keys on
    /// exactly those, so a second `forge` run (or a sibling process in a
    /// parallel regen) deserialises the prior artifact in single-digit
    /// milliseconds instead of recompiling.
    ///
    /// Cache hits never change generated output: the cached entry is the same
    /// machine code Cranelift would have produced, and wasmtime's own
    /// version/config fingerprint invalidates stale entries (a wasmtime bump
    /// simply misses and recompiles).
    pub fn with_cache(cache_dir: &std::path::Path) -> Result<Self, EngineError> {
        let mut cc = wasmtime::CacheConfig::new();
        cc.with_directory(cache_dir);
        let cache = wasmtime::Cache::new(cc).map_err(|e| EngineError::Cache(e.to_string()))?;
        Self::build(Some(cache))
    }

    fn build(cache: Option<wasmtime::Cache>) -> Result<Self, EngineError> {
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        // Determinism: disable nondeterministic relaxed-simd lowerings.
        cfg.relaxed_simd_deterministic(true);
        if let Some(cache) = cache {
            cfg.cache(Some(cache));
        }

        let wt = WtEngine::new(&cfg).map_err(|e| EngineError::Init(e.to_string()))?;

        // Tick at 10ms granularity. Wall-clock deadlines are coarse — that's
        // fine; the goal is "kill runaway plugins", not millisecond precision.
        let ticker = EpochTicker::spawn(wt.clone(), Duration::from_millis(10));

        Ok(Engine {
            inner: Arc::new(EngineInner {
                wt,
                _ticker: ticker,
            }),
        })
    }

    pub fn raw(&self) -> &WtEngine {
        &self.inner.wt
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("wasmtime engine init failed: {0}")]
    Init(String),
    #[error("compilation cache init failed: {0}")]
    Cache(String),
}

/// Background thread that increments the engine's epoch counter at a fixed
/// cadence. Dropping it stops the thread.
struct EpochTicker {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn spawn(engine: WtEngine, cadence: Duration) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_t = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_t.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(cadence);
                engine.increment_epoch();
            }
        });
        EpochTicker {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// -----------------------------------------------------------------------------
// HostState — per-Store
// -----------------------------------------------------------------------------

/// Per-invocation state held in `wasmtime::Store<HostState>`.
///
/// The `wasi` field is a *deny-all* `WasiCtx`: no preopens, no environment,
/// no inherited stdio. We wire `wasmtime_wasi` into the linker only because
/// the `wasm32-wasip2` rust libstd unconditionally imports WASI interfaces
/// (clocks, filesystem, stdio, exit, ...). With a deny-all context, calls
/// fail at runtime — same outcome as a trap, but using the well-tested
/// wasmtime-wasi resource handling instead of hand-rolled stubs.
pub struct HostState {
    pub limits: Limits,
    pub log_lines: Vec<(forge_ir::LogLevel, String)>,
    pub store_limits: StoreLimits,
    pub resource_table: ResourceTable,
    pub wasi: WasiCtx,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("limits", &self.limits)
            .field("log_lines", &self.log_lines.len())
            .finish_non_exhaustive()
    }
}

impl HostState {
    fn new(limits: Limits) -> Self {
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .build();
        // Deny-all WASI: no preopens, no env, no stdio inheritance. Plugins
        // that try to read/write/exit get a runtime error from wasmtime-wasi.
        let wasi = WasiCtxBuilder::new().build();
        HostState {
            limits,
            log_lines: Vec::new(),
            store_limits,
            resource_table: ResourceTable::new(),
            wasi,
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.resource_table,
        }
    }
}

// `wasmtime::component::bindgen!` emits a separate `Host` trait per world,
// even though both worlds import the same `host-api` interface. The `types`
// and `stage` interfaces also generate (empty) `Host` traits because they
// are `use`d. We have to implement all three per world; macro-expand the
// impls once per world and route through the world-neutral helpers in
// `case`.
macro_rules! impl_host_api {
    ($world:ident) => {
        impl bindings::$world::forge::plugin::types::Host for HostState {}
        impl bindings::$world::forge::plugin::stage::Host for HostState {}

        impl bindings::$world::forge::plugin::host_api::Host for HostState {
            fn log(
                &mut self,
                level: bindings::$world::forge::plugin::host_api::LogLevel,
                message: String,
            ) -> wasmtime::Result<()> {
                use bindings::$world::forge::plugin::host_api::LogLevel as L;
                let lv = match level {
                    L::Trace => forge_ir::LogLevel::Trace,
                    L::Debug => forge_ir::LogLevel::Debug,
                    L::Info => forge_ir::LogLevel::Info,
                    L::Warn => forge_ir::LogLevel::Warn,
                    L::Error => forge_ir::LogLevel::Error,
                };
                match lv {
                    forge_ir::LogLevel::Trace => {
                        tracing::trace!(target: "plugin", "{message}")
                    }
                    forge_ir::LogLevel::Debug => {
                        tracing::debug!(target: "plugin", "{message}")
                    }
                    forge_ir::LogLevel::Info => {
                        tracing::info!(target: "plugin", "{message}")
                    }
                    forge_ir::LogLevel::Warn => {
                        tracing::warn!(target: "plugin", "{message}")
                    }
                    forge_ir::LogLevel::Error => {
                        tracing::error!(target: "plugin", "{message}")
                    }
                }
                self.log_lines.push((lv, message));
                Ok(())
            }

            fn case_convert(
                &mut self,
                input: String,
                style: bindings::$world::forge::plugin::host_api::CaseStyle,
            ) -> wasmtime::Result<String> {
                use bindings::$world::forge::plugin::host_api::CaseStyle as S;
                let local = match style {
                    S::Snake => case::Style::Snake,
                    S::Kebab => case::Style::Kebab,
                    S::Camel => case::Style::Camel,
                    S::Pascal => case::Style::Pascal,
                    S::ScreamingSnake => case::Style::ScreamingSnake,
                };
                Ok(case::convert(&input, local))
            }
        }
    };
}

impl_host_api!(transformer);
impl_host_api!(generator);

mod case {
    /// World-neutral case style matching the WIT `case-style` enum.
    #[derive(Debug, Clone, Copy)]
    pub enum Style {
        Snake,
        Kebab,
        Camel,
        Pascal,
        ScreamingSnake,
    }

    /// Split an identifier into ASCII word fragments. Recognises the common
    /// boundaries — case changes, digits, and explicit `_`/`-`/space — and
    /// drops everything else. Pure, deterministic.
    fn split(input: &str) -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut prev_lower = false;
        for ch in input.chars() {
            if ch == '_' || ch == '-' || ch.is_whitespace() {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                prev_lower = false;
            } else if ch.is_ascii_uppercase() {
                if prev_lower && !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                cur.push(ch.to_ascii_lowercase());
                prev_lower = false;
            } else {
                cur.push(ch);
                prev_lower = ch.is_ascii_lowercase();
            }
        }
        if !cur.is_empty() {
            words.push(cur);
        }
        words
    }

    pub fn convert(input: &str, style: Style) -> String {
        let words = split(input);
        match style {
            Style::Snake => words.join("_"),
            Style::Kebab => words.join("-"),
            Style::ScreamingSnake => words
                .iter()
                .map(|w| w.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join("_"),
            Style::Camel => words
                .iter()
                .enumerate()
                .map(|(i, w)| if i == 0 { w.clone() } else { capitalize(w) })
                .collect::<String>(),
            Style::Pascal => words.iter().map(|w| capitalize(w)).collect::<String>(),
        }
    }

    fn capitalize(w: &str) -> String {
        let mut chars = w.chars();
        match chars.next() {
            None => String::new(),
            Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn snake() {
            assert_eq!(convert("HelloWorld", Style::Snake), "hello_world");
            assert_eq!(convert("hello-world", Style::Snake), "hello_world");
            assert_eq!(convert("hello world", Style::Snake), "hello_world");
        }
        #[test]
        fn pascal() {
            assert_eq!(convert("hello_world", Style::Pascal), "HelloWorld");
        }
        #[test]
        fn camel() {
            assert_eq!(convert("hello_world", Style::Camel), "helloWorld");
        }
        #[test]
        fn kebab() {
            assert_eq!(convert("HelloWorld", Style::Kebab), "hello-world");
        }
        #[test]
        fn screaming() {
            assert_eq!(convert("helloWorld", Style::ScreamingSnake), "HELLO_WORLD");
        }
    }
}

// -----------------------------------------------------------------------------
// Plugin
// -----------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("failed to compile plugin component: {0}")]
    Compile(String),
    #[error("failed to link plugin: {0}")]
    Link(String),
    #[error("failed to instantiate plugin: {0}")]
    Instantiate(String),
    #[error("failed to fetch plugin info: {0}")]
    Info(String),
    #[error("plugin info failed conversion: {0}")]
    Convert(String),
}

/// Loaded plugin component. Holds the compiled component + cached `info()`.
/// Each invocation builds a fresh `Store` with its own resource budget.
pub struct Plugin {
    engine: Engine,
    component: Component,
    info: PluginInfo,
    config_schema: String,
    kind: PluginKind,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("info", &self.info)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Transformer,
    Generator,
}

impl Plugin {
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn config_schema(&self) -> &str {
        &self.config_schema
    }

    pub fn kind(&self) -> PluginKind {
        self.kind
    }

    /// Load a transformer plugin from raw component bytes.
    pub fn load_transformer(engine: &Engine, bytes: &[u8]) -> Result<Self, LoadError> {
        let component =
            Component::new(engine.raw(), bytes).map_err(|e| LoadError::Compile(e.to_string()))?;
        let linker = build_transformer_linker(engine, &component).map_err(LoadError::Link)?;
        let mut store = make_store(engine, Limits::transformer());
        let inst =
            bindings::transformer::IrTransformer::instantiate(&mut store, &component, &linker)
                .map_err(|e| LoadError::Instantiate(e.to_string()))?;
        let info_wit = inst
            .forge_plugin_transformer_api()
            .call_info(&mut store)
            .map_err(|e| LoadError::Info(e.to_string()))?;
        let schema = inst
            .forge_plugin_transformer_api()
            .call_config_schema(&mut store)
            .map_err(|e| LoadError::Info(e.to_string()))?;
        let info = convert::transformer::plugin_info_from_wit(info_wit);
        Ok(Plugin {
            engine: engine.clone(),
            component,
            info,
            config_schema: schema,
            kind: PluginKind::Transformer,
        })
    }

    /// Load a generator plugin from raw component bytes.
    pub fn load_generator(engine: &Engine, bytes: &[u8]) -> Result<Self, LoadError> {
        let component =
            Component::new(engine.raw(), bytes).map_err(|e| LoadError::Compile(e.to_string()))?;
        let linker = build_generator_linker(engine, &component).map_err(LoadError::Link)?;
        let mut store = make_store(engine, Limits::generator());
        let inst = bindings::generator::CodeGenerator::instantiate(&mut store, &component, &linker)
            .map_err(|e| LoadError::Instantiate(e.to_string()))?;
        let info_wit = inst
            .forge_plugin_generator_api()
            .call_info(&mut store)
            .map_err(|e| LoadError::Info(e.to_string()))?;
        let schema = inst
            .forge_plugin_generator_api()
            .call_config_schema(&mut store)
            .map_err(|e| LoadError::Info(e.to_string()))?;
        let info = convert::generator::plugin_info_from_wit(info_wit);
        Ok(Plugin {
            engine: engine.clone(),
            component,
            info,
            config_schema: schema,
            kind: PluginKind::Generator,
        })
    }

    /// Run a transformer.
    pub fn transform(
        &self,
        spec: Ir,
        config: &str,
        limits: Limits,
    ) -> Result<TransformOutput, StageError> {
        if self.kind != PluginKind::Transformer {
            return Err(StageError::PluginBug(
                "plugin loaded as transformer but called as generator".into(),
            ));
        }
        let linker = build_transformer_linker(&self.engine, &self.component)
            .map_err(|e| StageError::PluginBug(format!("link: {e}")))?;
        let mut store = make_store(&self.engine, limits);
        let inst =
            bindings::transformer::IrTransformer::instantiate(&mut store, &self.component, &linker)
                .map_err(|e| StageError::PluginBug(format!("instantiate: {e}")))?;
        let wit_ir = convert::transformer::ir_to_wit(spec);
        let result = inst.forge_plugin_transformer_api().call_transform(
            store.as_context_mut(),
            &wit_ir,
            config,
        );
        let result = map_call_error(result, &store)?;
        match result {
            Ok(out) => {
                let spec = convert::transformer::ir_from_wit(out.spec)
                    .map_err(|e| StageError::PluginBug(format!("ir convert: {e}")))?;
                let diagnostics = out
                    .diagnostics
                    .into_iter()
                    .map(convert::transformer::diagnostic_from_wit)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| StageError::PluginBug(format!("diag convert: {e}")))?;
                Ok(TransformOutput { spec, diagnostics })
            }
            Err(stage_err) => Err(stage_error_from_repr(
                convert::transformer::stage_error_from_wit(stage_err),
            )),
        }
    }

    /// Run a generator.
    pub fn generate(
        &self,
        spec: Ir,
        config: &str,
        limits: Limits,
    ) -> Result<GenerationOutput, StageError> {
        if self.kind != PluginKind::Generator {
            return Err(StageError::PluginBug(
                "plugin loaded as generator but called as transformer".into(),
            ));
        }
        let linker = build_generator_linker(&self.engine, &self.component)
            .map_err(|e| StageError::PluginBug(format!("link: {e}")))?;
        let mut store = make_store(&self.engine, limits);
        let inst =
            bindings::generator::CodeGenerator::instantiate(&mut store, &self.component, &linker)
                .map_err(|e| StageError::PluginBug(format!("instantiate: {e}")))?;
        let wit_ir = convert::generator::ir_to_wit(spec);
        let result = inst.forge_plugin_generator_api().call_generate(
            store.as_context_mut(),
            &wit_ir,
            config,
        );
        let result = map_call_error(result, &store)?;
        match result {
            Ok(out) => {
                let mut total_bytes: u64 = 0;
                let files: Vec<OutputFile> = out
                    .files
                    .into_iter()
                    .map(|f| {
                        total_bytes = total_bytes.saturating_add(f.content.len() as u64);
                        OutputFile {
                            path: f.path,
                            content: f.content,
                            mode: match f.mode {
                                bindings::generator::exports::forge::plugin::generator_api::FileMode::Text => FileMode::Text,
                                bindings::generator::exports::forge::plugin::generator_api::FileMode::Binary => FileMode::Binary,
                                bindings::generator::exports::forge::plugin::generator_api::FileMode::Executable => FileMode::Executable,
                            },
                        }
                    })
                    .collect();
                if files.len() as u64 > limits.output_files_max as u64 {
                    return Err(StageError::ResourceExceeded(ResourceKind::OutputSize));
                }
                if total_bytes > limits.output_total_bytes_max {
                    return Err(StageError::ResourceExceeded(ResourceKind::OutputSize));
                }
                let diagnostics = out
                    .diagnostics
                    .into_iter()
                    .map(convert::generator::diagnostic_from_wit)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| StageError::PluginBug(format!("diag convert: {e}")))?;
                Ok(GenerationOutput { files, diagnostics })
            }
            Err(stage_err) => Err(stage_error_from_repr(
                convert::generator::stage_error_from_wit(stage_err),
            )),
        }
    }
}

/// Translate the world-neutral `StageErrorRepr` from `forge-ir-bindgen` into
/// the host's `StageError`. They are isomorphic; the indirection only exists
/// to avoid pulling `forge-host` into `forge-ir-bindgen`'s dependency graph.
fn stage_error_from_repr(r: StageErrorRepr) -> StageError {
    match r {
        StageErrorRepr::Rejected {
            reason,
            diagnostics,
        } => StageError::Rejected {
            reason,
            diagnostics,
        },
        StageErrorRepr::PluginBug(s) => StageError::PluginBug(s),
        StageErrorRepr::ConfigInvalid(s) => StageError::ConfigInvalid(s),
        StageErrorRepr::ResourceExceeded(k) => StageError::ResourceExceeded(match k {
            ResourceKindRepr::Fuel => ResourceKind::Fuel,
            ResourceKindRepr::Memory => ResourceKind::Memory,
            ResourceKindRepr::Time => ResourceKind::Time,
            ResourceKindRepr::OutputSize => ResourceKind::OutputSize,
        }),
    }
}

/// Build a `Linker` for the transformer world: registers `host-api` and
/// stubs every other import declared by the loaded component as a trap.
/// The latter handles WASI imports that the rust libstd `wasm32-wasip2`
/// target inserts unconditionally — `wasi:cli/exit`, `wasi:cli/stderr`,
/// `wasi:filesystem/*`, etc. The sandbox model rejects all of them, so
/// satisfying the linker with traps is the right behaviour: a plugin that
/// actually exercises one fails loudly rather than silently leaking out
/// of the sandbox.
fn build_transformer_linker(
    engine: &Engine,
    _component: &Component,
) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::<HostState>::new(engine.raw());
    bindings::transformer::IrTransformer::add_to_linker::<HostState, HasSelf<HostState>>(
        &mut linker,
        |s| s,
    )
    .map_err(|e| e.to_string())?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    Ok(linker)
}

fn build_generator_linker(
    engine: &Engine,
    _component: &Component,
) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::<HostState>::new(engine.raw());
    bindings::generator::CodeGenerator::add_to_linker::<HostState, HasSelf<HostState>>(
        &mut linker,
        |s| s,
    )
    .map_err(|e| e.to_string())?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    Ok(linker)
}

fn make_store(engine: &Engine, limits: Limits) -> Store<HostState> {
    let mut store = Store::new(engine.raw(), HostState::new(limits));
    let _ = store.set_fuel(limits.fuel);
    // Epoch ticks at 10ms; deadline = wall_clock_ms / 10, minimum 1.
    let deadline = limits.wall_clock_ms.div_ceil(10).max(1);
    store.set_epoch_deadline(deadline);
    store.epoch_deadline_trap();
    store.limiter(|s| &mut s.store_limits);
    store
}

/// Translate the wasmtime call result. The `Result` we're handed already
/// distinguishes traps (anyhow::Error) from successful component returns
/// (which may themselves be `Result<T, stage_error>` if the WIT signature
/// includes a `result<...>`).
fn map_call_error<T>(res: wasmtime::Result<T>, store: &Store<HostState>) -> Result<T, StageError> {
    match res {
        Ok(v) => Ok(v),
        Err(e) => {
            // Distinguish resource exhaustion from generic traps. wasmtime
            // surfaces these as specific error variants via `downcast_ref`.
            if let Some(t) = e.downcast_ref::<wasmtime::Trap>() {
                match t {
                    wasmtime::Trap::OutOfFuel => {
                        return Err(StageError::ResourceExceeded(ResourceKind::Fuel))
                    }
                    wasmtime::Trap::Interrupt => {
                        return Err(StageError::ResourceExceeded(ResourceKind::Time))
                    }
                    _ => {}
                }
            }
            // Memory limit violations surface as a generic error from
            // ResourceLimiter; check via the message + state.
            let msg = format!("{e:#}");
            if msg.contains("memory") && store.data().limits.memory_bytes > 0 {
                // Best-effort detection. Fall through to plugin-bug if not
                // matched.
                if msg.contains("grow") || msg.contains("limit") {
                    return Err(StageError::ResourceExceeded(ResourceKind::Memory));
                }
            }
            Err(StageError::PluginBug(msg))
        }
    }
}
