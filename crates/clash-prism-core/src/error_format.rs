//! # User-Friendly Error Formatting
//!
//! Inspired by Claude Code's errors.ts error handling pattern:
//! - Error classifier: maps underlying `PrismError` to user-understandable categories
//! - Fix suggestions: provides actionable repair suggestions for each error category
//! - Structured output: `UserError` contains title, detail, and suggestion
//!
//! Design goals:
//! - Users can immediately understand the problem after seeing an error
//! - Every error includes a fix suggestion, reducing support cost
//! - Error categories can be used for log aggregation and monitoring

use crate::error::PrismError;

/// Error categories
///
/// Maps underlying `PrismError` to user-understandable classifications.
///
/// Security, Network, Script, Plugin, Unknown variants are reserved as extension
/// points for future error classification. They will be activated when new
/// `PrismError` variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// File system related (read/write permissions, path not found, etc.)
    FileSystem,
    /// Configuration structure errors (type mismatch, missing fields, etc.)
    ConfigStructure,
    /// Security related (insufficient permissions, illegal operations, etc.)
    ///
    /// Reserved for future use when security-related error classification is needed.
    #[allow(dead_code)]
    Security,
    /// Runtime errors (execution failure, insufficient resources, etc.)
    Runtime,
    /// Network related (connection failure, timeout, etc.)
    ///
    /// Reserved for future use when network-related error classification is needed.
    #[allow(dead_code)]
    Network,
    /// Script related (JS execution error, syntax error, etc.)
    ///
    /// Reserved for future use when script-related error classification is needed.
    #[allow(dead_code)]
    Script,
    /// Plugin related (load failure, version incompatibility, etc.)
    ///
    /// Reserved for future use when plugin-related error classification is needed.
    #[allow(dead_code)]
    Plugin,
    /// Data validation errors (duplicate names, missing references, etc.)
    Validation,
    /// Unknown errors (unclassifiable errors)
    Unknown,
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCategory::FileSystem => write!(f, "FileSystem"),
            ErrorCategory::ConfigStructure => write!(f, "ConfigStructure"),
            ErrorCategory::Security => write!(f, "Security"),
            ErrorCategory::Runtime => write!(f, "Runtime"),
            ErrorCategory::Network => write!(f, "Network"),
            ErrorCategory::Script => write!(f, "Script"),
            ErrorCategory::Plugin => write!(f, "Plugin"),
            ErrorCategory::Validation => write!(f, "Validation"),
            ErrorCategory::Unknown => write!(f, "Unknown"),
        }
    }
}

/// User-facing error information
///
/// Contains error category, title, detail, and optional fix suggestion.
/// Designed to be displayed directly to end users.
pub struct UserError {
    /// Error category
    pub category: ErrorCategory,
    /// Error title (short description)
    pub title: String,
    /// Error detail (full description)
    pub detail: String,
    /// Fix suggestion (optional)
    pub suggestion: Option<String>,
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "[{}] {}", self.category, self.title)?;
        writeln!(f, "  {}", self.detail)?;
        if let Some(suggestion) = &self.suggestion {
            writeln!(f, "  Suggestion: {}", suggestion)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UserError {{ category: {:?}, title: {:?}, detail: {:?}, suggestion: {:?} }}",
            self.category, self.title, self.detail, self.suggestion
        )
    }
}

/// Error classifier
///
/// Maps `PrismError` to `ErrorCategory`.
/// Classification is based on the semantic meaning of each error variant.
pub fn classify_error(err: &PrismError) -> ErrorCategory {
    match err {
        // File system errors
        PrismError::Io { .. } => ErrorCategory::FileSystem,
        PrismError::PathNotFound { .. } => ErrorCategory::FileSystem,
        PrismError::TargetOutput { .. } => ErrorCategory::FileSystem,

        // Configuration structure errors
        PrismError::TypeMismatch { .. } => ErrorCategory::ConfigStructure,
        PrismError::OverrideConflict { .. } => ErrorCategory::ConfigStructure,
        PrismError::Serialization(_) => ErrorCategory::ConfigStructure,
        PrismError::YamlParse(_) => ErrorCategory::ConfigStructure,

        // Data validation errors
        PrismError::Validation { .. } => ErrorCategory::Validation,
        PrismError::DuplicateProxyName { .. } => ErrorCategory::Validation,
        PrismError::ProxyGroupMissingProxy { .. } => ErrorCategory::Validation,

        // Compilation / runtime errors
        PrismError::DslParse { .. } => ErrorCategory::ConfigStructure,
        PrismError::RuntimeFieldInStaticFilter { .. } => ErrorCategory::ConfigStructure,
        PrismError::ExpressionCompileFailed { .. } => ErrorCategory::ConfigStructure,
        PrismError::CircularDependency { .. } => ErrorCategory::ConfigStructure,
        PrismError::DependencyNotFound { .. } => ErrorCategory::ConfigStructure,
        PrismError::PatchExecutionFailed { .. } => ErrorCategory::Runtime,
    }
}

/// Format a user-facing error
///
/// Converts `PrismError` to `UserError` with category, title, detail, and fix suggestion.
pub fn format_user_facing_error(err: &PrismError) -> UserError {
    let category = classify_error(err);

    match err {
        PrismError::Io { detail, .. } => UserError {
            category,
            title: "File operation failed".to_string(),
            detail: format!("IO error: {}", detail),
            suggestion: Some("Check file permissions and disk space.".to_string()),
        },

        PrismError::PathNotFound { path } => UserError {
            category,
            title: "Path not found".to_string(),
            detail: format!("Path not found: {}", path),
            suggestion: Some(
                "Verify the path is spelled correctly and the file/directory exists.".to_string(),
            ),
        },

        PrismError::TypeMismatch {
            expected,
            actual,
            path,
        } => UserError {
            category,
            title: "Type mismatch".to_string(),
            detail: format!(
                "Path '{}' expected type '{}', got '{}'",
                path, expected, actual
            ),
            suggestion: Some(format!(
                "Change the value at path '{}' to type '{}'.",
                path, expected
            )),
        },

        PrismError::OverrideConflict { field } => UserError {
            category,
            title: "Operation conflict".to_string(),
            detail: format!(
                "Field '{}' does not allow mixing $override with other operations.",
                field
            ),
            suggestion: Some(format!(
                "Remove either $override or other operations from field '{}'; they cannot be mixed.",
                field
            )),
        },

        PrismError::Validation { message, path } => UserError {
            category,
            title: "Configuration validation failed".to_string(),
            detail: format!(
                "{}{}",
                message,
                path.as_ref()
                    .map(|p| format!(" (path: {})", p))
                    .unwrap_or_default()
            ),
            suggestion: Some("Check the relevant fields in the configuration file.".to_string()),
        },

        PrismError::DuplicateProxyName { name, count } => UserError {
            category,
            title: "Duplicate proxy name".to_string(),
            detail: format!(
                "Proxy '{}' appears {} times; names must be unique.",
                name, count
            ),
            suggestion: Some(format!(
                "Rename duplicate proxy '{}' to a unique name.",
                name
            )),
        },

        PrismError::ProxyGroupMissingProxy { group, proxy } => UserError {
            category,
            title: "Proxy reference not found".to_string(),
            detail: format!(
                "Proxy group '{}' references non-existent proxy '{}'.",
                group, proxy
            ),
            suggestion: Some(format!(
                "Ensure proxy '{}' is defined in 'proxies', or remove the reference from group '{}'.",
                proxy, group
            )),
        },

        PrismError::DslParse {
            message,
            file,
            line,
        } => UserError {
            category,
            title: "DSL parse error".to_string(),
            detail: format!(
                "{}{}",
                message,
                file.as_ref()
                    .map(|f| format!(
                        " (file: {}{})",
                        f.display(),
                        line.map(|l| format!(", line: {}", l)).unwrap_or_default()
                    ))
                    .unwrap_or_default()
            ),
            suggestion: Some("Check the DSL file syntax for errors.".to_string()),
        },

        PrismError::RuntimeFieldInStaticFilter { field, hint } => UserError {
            category,
            title: "Runtime field referenced in static context".to_string(),
            detail: format!(
                "Field '{}' is a runtime field and cannot be used in static filters. {}",
                field, hint
            ),
            suggestion: Some(format!(
                "Move field '{}' to a runtime context, or use a static field instead.",
                field
            )),
        },

        PrismError::ExpressionCompileFailed { expr, reason } => UserError {
            category,
            title: "Expression compilation failed".to_string(),
            detail: format!("Expression '{}' failed to compile: {}", expr, reason),
            suggestion: Some("Check the expression syntax for errors.".to_string()),
        },

        PrismError::CircularDependency { cycle } => UserError {
            category,
            title: "Circular dependency".to_string(),
            detail: format!("Circular dependency detected: {}", cycle),
            suggestion: Some(
                "Check import/dependency declarations and remove circular references.".to_string(),
            ),
        },

        PrismError::DependencyNotFound { dep, file } => UserError {
            category,
            title: "Dependency not found".to_string(),
            detail: format!("Dependency '{}' not found in file '{}'.", dep, file),
            suggestion: Some(format!(
                "Ensure the file corresponding to dependency '{}' exists and the path is correct.",
                dep
            )),
        },

        PrismError::PatchExecutionFailed { patch_id, reason } => UserError {
            category,
            title: "Patch execution failed".to_string(),
            detail: format!("Patch '{}' execution failed: {}", patch_id, reason),
            suggestion: Some("Check the target path and operation of the patch.".to_string()),
        },

        PrismError::Serialization(e) => UserError {
            category,
            title: "Serialization error".to_string(),
            detail: format!("JSON serialization failed: {}", e),
            suggestion: Some(
                "Check if the configuration value contains unsupported data types.".to_string(),
            ),
        },

        PrismError::YamlParse(e) => UserError {
            category,
            title: "YAML parse error".to_string(),
            detail: format!("YAML parse failed: {}", e),
            suggestion: Some("Check YAML syntax (indentation, colons, quotes, etc.).".to_string()),
        },

        PrismError::TargetOutput { message } => UserError {
            category,
            title: "Configuration output failed".to_string(),
            detail: message.clone(),
            suggestion: Some(
                "Check write permissions and disk space for the output directory.".to_string(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_classify_error_io() {
        let err = PrismError::Io {
            detail: "not found".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(classify_error(&err), ErrorCategory::FileSystem);
    }

    #[test]
    fn test_classify_error_path_not_found() {
        let err = PrismError::PathNotFound {
            path: "/tmp/test".to_string(),
        };
        assert_eq!(classify_error(&err), ErrorCategory::FileSystem);
    }

    #[test]
    fn test_classify_error_type_mismatch() {
        let err = PrismError::TypeMismatch {
            expected: "string".to_string(),
            actual: "number".to_string(),
            path: "/config/name".to_string(),
        };
        assert_eq!(classify_error(&err), ErrorCategory::ConfigStructure);
    }

    #[test]
    fn test_classify_error_validation() {
        let err = PrismError::Validation {
            message: "无效值".to_string(),
            path: Some("/config/port".to_string()),
        };
        assert_eq!(classify_error(&err), ErrorCategory::Validation);
    }

    #[test]
    fn test_classify_error_duplicate_proxy() {
        let err = PrismError::DuplicateProxyName {
            name: "proxy1".to_string(),
            count: 2,
        };
        assert_eq!(classify_error(&err), ErrorCategory::Validation);
    }

    #[test]
    fn test_classify_error_circular_dependency() {
        let err = PrismError::CircularDependency {
            cycle: "A -> B -> A".to_string(),
        };
        assert_eq!(classify_error(&err), ErrorCategory::ConfigStructure);
    }

    #[test]
    fn test_classify_error_patch_execution() {
        let err = PrismError::PatchExecutionFailed {
            patch_id: "p1".to_string(),
            reason: "路径不存在".to_string(),
        };
        assert_eq!(classify_error(&err), ErrorCategory::Runtime);
    }

    #[test]
    fn test_format_user_error_io() {
        let err = PrismError::Io {
            detail: "permission denied".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        };
        let user_err = format_user_facing_error(&err);
        assert_eq!(user_err.category, ErrorCategory::FileSystem);
        assert_eq!(user_err.title, "File operation failed");
        assert!(user_err.detail.contains("permission denied"));
        assert!(user_err.suggestion.is_some());
    }

    #[test]
    fn test_format_user_error_type_mismatch() {
        let err = PrismError::TypeMismatch {
            expected: "number".to_string(),
            actual: "string".to_string(),
            path: "/proxies/0/port".to_string(),
        };
        let user_err = format_user_facing_error(&err);
        assert_eq!(user_err.category, ErrorCategory::ConfigStructure);
        assert!(user_err.detail.contains("number"));
        assert!(user_err.detail.contains("string"));
        assert!(user_err.detail.contains("/proxies/0/port"));
    }

    #[test]
    fn test_format_user_error_duplicate_proxy() {
        let err = PrismError::DuplicateProxyName {
            name: "my-proxy".to_string(),
            count: 3,
        };
        let user_err = format_user_facing_error(&err);
        assert_eq!(user_err.category, ErrorCategory::Validation);
        assert!(user_err.detail.contains("my-proxy"));
        assert!(user_err.detail.contains("3"));
    }

    #[test]
    fn test_format_user_error_display() {
        let err = PrismError::PathNotFound {
            path: "/missing/file.yaml".to_string(),
        };
        let user_err = format_user_facing_error(&err);
        let display = format!("{}", user_err);
        assert!(display.contains("[FileSystem]"));
        assert!(display.contains("Path not found"));
        assert!(display.contains("/missing/file.yaml"));
        assert!(display.contains("Suggestion:"));
    }

    #[test]
    fn test_error_category_display() {
        assert_eq!(format!("{}", ErrorCategory::FileSystem), "FileSystem");
        assert_eq!(
            format!("{}", ErrorCategory::ConfigStructure),
            "ConfigStructure"
        );
        assert_eq!(format!("{}", ErrorCategory::Security), "Security");
        assert_eq!(format!("{}", ErrorCategory::Runtime), "Runtime");
        assert_eq!(format!("{}", ErrorCategory::Network), "Network");
        assert_eq!(format!("{}", ErrorCategory::Script), "Script");
        assert_eq!(format!("{}", ErrorCategory::Plugin), "Plugin");
        assert_eq!(format!("{}", ErrorCategory::Validation), "Validation");
        assert_eq!(format!("{}", ErrorCategory::Unknown), "Unknown");
    }

    #[test]
    fn test_format_user_error_dsl_parse() {
        let err = PrismError::DslParse {
            message: "意外的标记".to_string(),
            file: Some(PathBuf::from("/test/config.prism.yaml")),
            line: Some(42),
        };
        let user_err = format_user_facing_error(&err);
        assert!(user_err.detail.contains("意外的标记"));
        assert!(user_err.detail.contains("config.prism.yaml"));
        assert!(user_err.detail.contains("42"));
    }

    #[test]
    fn test_format_user_error_circular_dependency() {
        let err = PrismError::CircularDependency {
            cycle: "A -> B -> C -> A".to_string(),
        };
        let user_err = format_user_facing_error(&err);
        assert!(user_err.detail.contains("A -> B -> C -> A"));
        assert!(user_err.suggestion.is_some());
    }

    #[test]
    fn test_format_user_error_proxy_group_missing() {
        let err = PrismError::ProxyGroupMissingProxy {
            group: "auto".to_string(),
            proxy: "missing-proxy".to_string(),
        };
        let user_err = format_user_facing_error(&err);
        assert!(user_err.detail.contains("auto"));
        assert!(user_err.detail.contains("missing-proxy"));
    }
}
