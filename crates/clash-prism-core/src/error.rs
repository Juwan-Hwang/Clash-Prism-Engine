//! # Error Type Definitions
//!
//! Unified error hierarchy for Prism Engine:
//!
//! - [`PrismError`] — Runtime/execution errors (the main error type)
//! - [`CompileError`] — Compile-time / DSL parsing errors
//! - [`TransformWarning`] — Non-fatal warnings from `$transform` validation (§2.9)
//! - [`Result`] — Type alias for `Result<T, PrismError>`

use std::path::PathBuf;
use thiserror::Error;

/// Prism Engine unified error type.
///
/// Covers all error categories: compilation, execution, validation, and I/O.
#[derive(Debug, Error)]
pub enum PrismError {
    // ─── Compilation Errors ───
    #[error("DSL 解析错误: {message}")]
    DslParse {
        message: String,
        file: Option<PathBuf>,
        line: Option<u32>,
    },

    #[error("运行时字段在静态过滤器中引用: `{field}`. {hint}")]
    RuntimeFieldInStaticFilter { field: String, hint: String },

    #[error("表达式编译失败: {expr} — {reason}")]
    ExpressionCompileFailed { expr: String, reason: String },

    #[error("循环依赖检测到: {cycle}")]
    CircularDependency { cycle: String },

    #[error("依赖解析失败: `{dep}` 在文件 `{file}` 中未找到")]
    DependencyNotFound { dep: String, file: String },

    // ─── Execution Errors ───
    #[error("Patch 执行失败 (id={patch_id}): {reason}")]
    PatchExecutionFailed { patch_id: String, reason: String },

    #[error("路径不存在: {path}")]
    PathNotFound { path: String },

    #[error("类型不匹配: 期望 {expected}, 实际 {actual}, 路径: {path}")]
    TypeMismatch {
        expected: String,
        actual: String,
        path: String,
    },

    #[error("$override 与其他操作混用: 字段 `{field}` 不允许同时使用 $override 和其他操作")]
    OverrideConflict { field: String },

    // ─── Validation Errors ───
    #[error("校验失败: {message}")]
    Validation {
        message: String,
        path: Option<String>,
    },

    #[error("代理名称重复: `{name}` 出现 {count} 次")]
    DuplicateProxyName { name: String, count: usize },

    #[error("代理组引用不存在的代理: 组=`{group}`, 代理=`{proxy}`")]
    ProxyGroupMissingProxy { group: String, proxy: String },

    // ─── I/O / System ───
    #[error("IO 错误: {detail}")]
    Io {
        detail: String,
        #[source]
        source: std::io::Error,
    },

    #[error("序列化错误: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("YAML 解析错误: {0}")]
    YamlParse(#[from] serde_yml::Error),

    // ─── Output Errors (§4.6) ───
    #[error("配置输出失败: {message}")]
    TargetOutput { message: String },
}

/// Compile-time专用 errors (for DSL → IR phase).
#[derive(Debug, Error)]
pub enum CompileError {
    #[error("{0}")]
    DslParse(PrismError),

    #[error("运行时字段在静态上下文中引用: `{field}` — {hint}")]
    RuntimeFieldInStaticContext { field: String, hint: String },

    #[error("表达式语法错误: `{expr}` — {reason}")]
    SyntaxError { expr: String, reason: String },

    #[error("__when__ 重复声明: 一个 .prism.yaml 文件只能有一个 __when__")]
    DuplicateWhenClause,

    #[error("条件预编译失败: {0}")]
    ConditionPrecompile(String),

    #[error("循环依赖: {0}")]
    CircularDependency(String),

    #[error("依赖未找到: {0}")]
    DependencyNotFound(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    Serialization(#[from] serde_json::Error),
}

// 手动实现使转换路径更显式，保留原始错误语义。
impl From<std::io::Error> for PrismError {
    fn from(e: std::io::Error) -> Self {
        PrismError::Io {
            detail: e.to_string(),
            source: e,
        }
    }
}

impl From<PrismError> for CompileError {
    fn from(e: PrismError) -> Self {
        match e {
            PrismError::CircularDependency { cycle } => CompileError::CircularDependency(cycle),
            PrismError::DependencyNotFound { dep, file } => {
                CompileError::DependencyNotFound(format!("`{dep}` 在文件 `{file}` 中未找到"))
            }
            PrismError::Io { detail, source } => CompileError::Io(std::io::Error::new(
                source.kind(),
                Box::new(PrismError::Io { detail, source })
                    as Box<dyn std::error::Error + Send + Sync>,
            )),
            PrismError::Serialization(e) => CompileError::Serialization(e),
            other => CompileError::DslParse(other),
        }
    }
}

/// $transform runtime warning (non-fatal, logged but does not block execution).
#[derive(Debug, Clone)]
pub struct TransformWarning {
    pub node_index: usize,
    pub node_name: String,
    pub field: String,
    pub hint: String,
}

impl std::fmt::Display for TransformWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Node #{} '{}' missing required field '{}': {}",
            self.node_index, self.node_name, self.field, self.hint
        )
    }
}

impl std::error::Error for TransformWarning {}

/// Prism Core专用 Result type alias.
pub type Result<T> = std::result::Result<T, PrismError>;
