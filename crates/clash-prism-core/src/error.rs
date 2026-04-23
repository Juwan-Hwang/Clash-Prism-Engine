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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prism_error_display_circular_dependency() {
        let err = PrismError::CircularDependency {
            cycle: "A -> B -> A".to_string(),
        };
        let display = format!("{}", err);
        assert!(
            display.contains("A -> B -> A"),
            "Display 应包含循环路径: {}",
            display
        );
    }

    #[test]
    fn test_prism_error_display_dsl_parse() {
        let err = PrismError::DslParse {
            message: "unexpected token".to_string(),
            file: Some(PathBuf::from("test.prism.yaml")),
            line: Some(42),
        };
        let display = format!("{}", err);
        assert!(
            display.contains("unexpected token"),
            "Display 应包含 message: {}",
            display
        );
        assert!(
            display.contains("DSL 解析错误"),
            "Display 应包含错误类型: {}",
            display
        );
    }

    #[test]
    fn test_prism_error_display_validation() {
        let err = PrismError::Validation {
            message: "port must be positive".to_string(),
            path: Some("mixed-port".to_string()),
        };
        let display = format!("{}", err);
        assert!(
            display.contains("port must be positive"),
            "Display 应包含 message: {}",
            display
        );
    }

    #[test]
    fn test_from_io_error_to_prism_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let prism_err: PrismError = io_err.into();

        match &prism_err {
            PrismError::Io { detail, source } => {
                assert_eq!(detail, "file not found");
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("Expected PrismError::Io, got {:?}", other),
        }
    }

    #[test]
    fn test_from_prism_error_to_compile_error_circular() {
        let prism_err = PrismError::CircularDependency {
            cycle: "A -> B -> A".to_string(),
        };
        let compile_err: CompileError = prism_err.into();

        match &compile_err {
            CompileError::CircularDependency(cycle) => {
                assert_eq!(cycle, "A -> B -> A");
            }
            other => panic!("Expected CompileError::CircularDependency, got {:?}", other),
        }
    }

    #[test]
    fn test_from_prism_error_to_compile_error_not_found() {
        let prism_err = PrismError::DependencyNotFound {
            dep: "base-dns".to_string(),
            file: "rules.prism.yaml".to_string(),
        };
        let compile_err: CompileError = prism_err.into();

        match &compile_err {
            CompileError::DependencyNotFound(msg) => {
                assert!(msg.contains("base-dns"), "应包含 dep 名称: {}", msg);
                assert!(
                    msg.contains("rules.prism.yaml"),
                    "应包含 file 名称: {}",
                    msg
                );
            }
            other => panic!("Expected CompileError::DependencyNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_from_prism_error_to_compile_error_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let prism_err = PrismError::Io {
            detail: io_err.to_string(),
            source: io_err,
        };
        let compile_err: CompileError = prism_err.into();

        match &compile_err {
            CompileError::Io(io) => {
                assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("Expected CompileError::Io, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_warning_display() {
        let warning = TransformWarning {
            node_index: 3,
            node_name: "proxy-groups".to_string(),
            field: "name".to_string(),
            hint: "field is required for GUI rendering".to_string(),
        };
        let display = format!("{}", warning);
        assert!(
            display.contains("proxy-groups"),
            "Display 应包含 node_name: {}",
            display
        );
        assert!(
            display.contains("name"),
            "Display 应包含 field: {}",
            display
        );
        assert!(
            display.contains("field is required for GUI rendering"),
            "Display 应包含 hint: {}",
            display
        );
    }
}
