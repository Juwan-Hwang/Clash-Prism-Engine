//! # Source Tracking Information
//!
//! Records where each [`Patch`](crate::ir::Patch) originated from.
//! Used by Trace View and Explain View to show users
//! which file, plugin, or editor action produced each configuration change.

use serde::{Deserialize, Serialize};

/// Source information for a Patch (used in Trace View and Explain View).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchSource {
    /// Source kind (YAML file, visual editor, script, plugin, or builtin)
    pub kind: SourceKind,
    /// Source file path (if applicable)
    pub file: Option<String>,
    /// Line number in source file (if applicable)
    pub line: Option<u32>,
    /// Plugin ID (if source is a plugin)
    pub plugin_id: Option<String>,
}

impl PatchSource {
    /// Create source from a YAML file with optional line number.
    pub fn yaml_file(file: impl Into<String>, line: Option<u32>) -> Self {
        Self {
            kind: SourceKind::YamlFile,
            file: Some(file.into()),
            line,
            plugin_id: None,
        }
    }

    /// Create source from a visual editor action with a custom source name.
    ///
    /// The `source` parameter is a free-form string that the GUI integrator
    /// uses to identify the specific editor component that produced the Patch,
    /// e.g. `"rule-editor"`, `"quick-toggle"`, `"template-wizard"`, `"bulk-import"`.
    ///
    /// # Validation
    ///
    /// The `source` string must satisfy:
    /// - Non-empty after trimming
    /// - Length <= 256 characters
    /// - No control characters (U+0000..=U+001F, U+007F, U+0080..=U+009F)
    /// - No null bytes
    ///
    /// # Errors
    ///
    /// Returns `Err(PrismError::Validation)` if any validation rule is violated.
    pub fn visual_editor(source: impl Into<String>) -> crate::error::Result<Self> {
        let source = source.into();

        // Trim whitespace for validation, but preserve original for display.
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return Err(crate::error::PrismError::Validation {
                message: "VisualEditor.source 不能为空字符串".into(),
                path: None,
            });
        }
        if trimmed.len() > 256 {
            return Err(crate::error::PrismError::Validation {
                message: format!(
                    "VisualEditor.source 长度不能超过 256 字符 (当前 {} 字符)",
                    trimmed.len()
                ),
                path: None,
            });
        }
        if source.contains('\0') {
            return Err(crate::error::PrismError::Validation {
                message: "VisualEditor.source 不能包含 null 字节".into(),
                path: None,
            });
        }
        if source.chars().any(|c| {
            // C0 controls (U+0000..=U+001F), DEL (U+007F), C1 controls (U+0080..=U+009F)
            c <= '\u{001F}' || c == '\u{007F}' || ('\u{0080}'..='\u{009F}').contains(&c)
        }) {
            return Err(crate::error::PrismError::Validation {
                message: "VisualEditor.source 不能包含控制字符".into(),
                path: None,
            });
        }

        Ok(Self {
            kind: SourceKind::VisualEditor { source },
            file: None,
            line: None,
            plugin_id: None,
        })
    }

    /// Create source from a JS script with given name.
    pub fn script(name: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::Script { name: name.into() },
            file: None,
            line: None,
            plugin_id: None,
        }
    }

    /// Create source from an installed plugin with given ID.
    pub fn plugin(id: impl Into<String>) -> Self {
        let id_str = id.into();
        Self {
            kind: SourceKind::Plugin { id: id_str.clone() },
            file: None,
            line: None,
            plugin_id: Some(id_str),
        }
    }

    /// Create a built-in engine source.
    pub fn builtin() -> Self {
        Self {
            kind: SourceKind::Builtin,
            file: None,
            line: None,
            plugin_id: None,
        }
    }

    /// Get a short description string for UI display.
    pub fn short_description(&self) -> String {
        match &self.kind {
            SourceKind::YamlFile => self.file.clone().unwrap_or_else(|| "未知文件".into()),
            SourceKind::VisualEditor { source } => format!("编辑器({})", source),
            SourceKind::Script { name } => format!("脚本: {}", name),
            SourceKind::Plugin { id } => format!("插件: {}", id),
            SourceKind::Builtin => "引擎内建".to_string(),
        }
    }
}

/// Source kind enum — classifies where a Patch originated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SourceKind {
    /// User-written Prism DSL file (.prism.yaml)
    YamlFile,

    /// Visual editor auto-generated, with a custom source name
    /// identifying the specific editor component (e.g. "rule-editor", "quick-toggle").
    VisualEditor { source: String },

    /// JavaScript script
    Script { name: String },

    /// Installed plugin
    Plugin { id: String },

    /// Engine built-in (internal defaults)
    Builtin,
}
