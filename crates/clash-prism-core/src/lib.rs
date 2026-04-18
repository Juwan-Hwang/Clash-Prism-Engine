//! # Prism Core — Core Abstraction Layer
//!
//! Zero UI dependency, pure algorithm module. Defines the core data structures of Prism Engine:
//!
//! - **Patch IR** — Unified Intermediate Representation; all inputs (DSL / scripts / plugins) compile to this
//! - **Patch Compiler** — Compiles various inputs into Patch IR
//! - **Patch Executor** — Executes Patch IR to produce the final configuration
//! - **Validator** — Configuration legality validation + smart suggestions
//! - **Scope System** — Four-layer scoping (Global / Profile / Scoped / Runtime)
//! - **Failover** — Node failure automatic switch policy (§7.2)
//! - **Target Compiler** — Outputs final config in target core format (mihomo / clash-rs / json)
//! - **Trace System** — Execution tracing for Explain View and Diff View (§3.2, §10)
//!
//! ## Architecture Overview
//!
//! ```text
//! ┌─────────────┐    ┌──────────────┐    ┌────────────────┐
//! │  Prism DSL   │───▶│  Patch IR     │───▶│  Target Config  │
//! │  (.yaml)     │    │  (unified)    │    │  (mihomo/...)  │
//! └─────────────┘    └──────────────┘    └────────────────┘
//!        │                   │                     ▲
//!        ▼                   ▼                     │
//! ┌──────────────┐   ┌──────────────┐      ┌──────────┐
//! │ DslParser     │   │ PatchExecutor│      │ Validator │
//! │ + Scope      │   │ + ExprEval   │      │ + Hints   │
//! └──────────────┘   └──────────────┘      └──────────┘
//! ```
//!
//! ## Design Principles
//!
//! 1. **One engine**: rquickjs, zero C dependency
//! 2. **One IR**: All inputs compile to unified Patch IR
//! 3. **Strict static/runtime isolation**: Runtime fields (`delay`, `latency`, etc.) are rejected at compile time in `$filter`/`$transform`
//! 4. **In-place mutation**: `config` is mutated directly instead of cloned per-operation
//! 5. **Deterministic execution**: Topological sort with lexicographical ordering for siblings
//!
//! ## Module Structure
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`ir`] | Patch IR definitions: `Patch`, `PatchOp`, `PatchId`, `Scope` |
//! | [`compiler`] | Patch compilation: DSL → IR, dependency resolution, topological sort |
//! | [`executor`] | Patch execution: 8 operations, expression evaluator, in-place mutation |
//! | [`executor::expr`] | Static expression evaluator for `$filter`/`$remove`/`$transform` (no JS runtime needed) |
//! | [`validator`] | Configuration validation: uniqueness, references, DNS completeness |
//! | [`scope`] | Four-layer scope system: Global / Profile / Scoped / Runtime |
//! | [`source`] | Source tracking: file, plugin, script, visual editor origins |
//! | [`trace`] | Execution trace: Explain View, Diff View, Replay (§3.2, §10) |
//! | [`failover`] | Node failover policy: threshold-based auto-switch with cooldown (§7.2) |
//! | [`target`] | Target compiler: JSON → YAML/JSON output, atomic write, hot-reload (§4.6) |
//! | [`watcher`] | File watcher: notify-based FS monitoring with debounce + hot-reload pipeline |
//! | [`error`] | Unified error types: `PrismError`, `CompileError`, `TransformWarning` |
//!
//! ## Quick Start
//!
//! ```ignore
//! use clash_prism_core::{PatchExecutor, ExecutionContext, ir::Patch};
//!
//! let mut executor = PatchExecutor::new();
//! let config = serde_json::json!({"proxies": []});
//! let patches = vec![]; // from DslParser::parse_file(...)
//! let result = executor.execute(config, &patches).unwrap();
//! ```

pub mod compiler;
pub mod error;
pub mod error_format;
pub mod executor;
pub mod failover;
pub mod ir;
pub mod json_path;
pub use executor::GUARDED_FIELDS;
/// Public helper to check if a path matches a guarded field.
/// Returns `true` if `path` exactly matches or starts with a guarded prefix followed by `.`.
pub use executor::is_guarded_path;
pub mod cache;
pub mod migration;
pub mod perf;
pub mod sanitize;
pub mod scope;
pub mod serial;
pub mod source;
pub mod target;
pub mod trace;
pub mod validator;
#[cfg(feature = "watcher")]
pub mod watcher;

#[cfg(not(feature = "watcher"))]
/// Stub module when the `watcher` feature is disabled.
/// Provides a compile-time error message if watcher functionality is used without the feature.
pub mod watcher {
    /// File change event (stub).
    #[derive(Debug, Clone)]
    pub struct FileChangeEvent {
        /// File path (stub).
        pub path: std::path::PathBuf,
        /// Change kind (stub).
        pub kind: FileChangeKind,
    }

    /// Change kind (stub).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FileChangeKind {
        /// File modified or created.
        Modified,
        /// File removed.
        Removed,
    }

    /// Watch configuration (stub).
    #[derive(Debug, Clone)]
    pub struct WatchConfig {
        /// Watch directory.
        pub watch_dir: std::path::PathBuf,
        /// Output path.
        pub output_path: std::path::PathBuf,
        /// Hot reload strategy.
        pub hot_reload: crate::target::HotReloadStrategy,
        /// Debounce time in milliseconds.
        pub debounce_ms: u64,
        /// Whether to compile on start.
        pub compile_on_start: bool,
        /// Directories to skip.
        pub skip_dirs: Vec<String>,
    }

    impl Default for WatchConfig {
        fn default() -> Self {
            Self {
                watch_dir: std::path::PathBuf::from("."),
                output_path: std::path::PathBuf::from("./config.yaml"),
                hot_reload: crate::target::HotReloadStrategy::None,
                debounce_ms: 300,
                compile_on_start: true,
                skip_dirs: vec!["node_modules".to_string(), "target".to_string()],
            }
        }
    }

    /// Compile statistics (stub).
    #[derive(Debug, Clone)]
    pub struct CompileStats {
        /// Number of patches compiled.
        pub patch_count: usize,
        /// Duration in microseconds.
        pub duration_us: u64,
        /// Output bytes written.
        pub output_bytes: usize,
        /// Whether write succeeded.
        pub write_success: bool,
        /// Reload detail.
        pub reload_detail: Option<String>,
    }

    /// Watch callback trait (stub).
    pub trait WatchCallback: Send + 'static {
        fn on_change(&mut self, event: &FileChangeEvent) -> Result<CompileStats, String>;
        fn on_batch_change(&mut self, events: &[FileChangeEvent]) -> Result<CompileStats, String>;
    }

    /// File watcher (stub).
    pub struct FileWatcher {
        #[allow(dead_code)] // Used in real watcher (feature = "watcher"), stub only stores it
        config: WatchConfig,
    }

    impl FileWatcher {
        /// Create a new file watcher (stub).
        pub fn new(config: WatchConfig) -> Result<Self, String> {
            Ok(Self { config })
        }

        /// Run the watcher loop (stub — always returns error).
        pub fn run(
            &self,
            _parse_fn: fn(
                &str,
                Option<std::path::PathBuf>,
            ) -> Result<Vec<crate::ir::Patch>, String>,
        ) -> Result<(), String> {
            Err("watcher feature is not enabled".to_string())
        }

        /// Run with callback (stub — always returns error).
        pub fn run_with_callback(&self, _callback: Box<dyn WatchCallback>) -> Result<(), String> {
            Err("watcher feature is not enabled".to_string())
        }
    }
}
