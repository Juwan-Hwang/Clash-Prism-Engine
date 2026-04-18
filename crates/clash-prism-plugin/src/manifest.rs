//! 插件清单（manifest.json）
//!
//! ## 示例
//!
//! ```json
//! {
//!     "id": "smart-grouping",
//!     "name": "智能分组",
//!     "version": "1.2.0",
//!     "type": "config",
//!     "permissions": ["config:read", "config:write"],
//!     "hooks": ["onSubscribeParsed"],
//!     "entry": "main.js",
//!     "scope": "subscribe",
//!     "timeout": 5000,
//!     "author": "user",
//!     "description": "按地区自动分组代理节点"
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::hook::Hook;
use crate::permission::Permission;

/// 插件清单
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件唯一标识符
    pub id: String,

    /// 插件显示名称
    pub name: String,

    /// 插件版本（语义化版本）
    pub version: String,

    /// 插件类型
    #[serde(rename = "type")]
    pub plugin_type: PluginType,

    /// 声明的权限列表
    pub permissions: Vec<Permission>,

    /// 注册的生命周期钩子
    #[serde(default)]
    pub hooks: Vec<Hook>,

    /// 入口文件路径
    pub entry: String,

    /// 作用域：global / subscribe / all
    #[serde(default = "default_scope")]
    pub scope: String,

    /// 超时时间（毫秒）
    #[serde(default = "default_timeout")]
    pub timeout: u64,

    /// 作者
    #[serde(default)]
    pub author: Option<String>,

    /// 描述
    #[serde(default)]
    pub description: Option<String>,

    /// 最低引擎版本要求
    #[serde(default)]
    pub min_engine_version: Option<String>,

    /// 入口脚本 SHA256 校验和（可选，用于完整性验证）
    #[serde(default)]
    pub checksum: Option<String>,
}

fn default_scope() -> String {
    "all".into()
}
fn default_timeout() -> u64 {
    5000 // 5 秒
}

impl PluginManifest {
    /// 从 JSON 字符串解析清单
    pub fn from_json(json_str: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_str)
    }

    /// 获取超时时间（毫秒）
    ///
    /// 此值应在执行时传递给 ScriptRuntime，用于限制脚本的最大执行时间。
    pub fn timeout_ms(&self) -> u64 {
        self.timeout
    }

    /// 验证清单的合法性
    ///
    /// # 设计说明
    ///
    /// 返回 `Result<(), Vec<String>>` 而非 Rust 惯例的 `Result<(), E>` 是有意为之：
    /// - 清单验证通常会产生**多个**独立错误（如 ID 格式 + 路径遍历 + 超时范围），
    ///   使用 `Vec<String>` 可以一次性报告所有问题，避免用户反复修改-验证循环。
    /// - `PluginLoadError::Validation` 会将错误列表 join 为可读的多行消息，
    ///   调用方无需关心具体错误类型。
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = vec![];

        if self.id.is_empty() {
            errors.push("插件 ID 不能为空".into());
        }

        // ID 格式校验：只允许小写字母、数字、连字符，且不能以连字符开头
        if self.id.starts_with('-') {
            errors.push(format!("插件 ID「{}」不能以连字符「-」开头", self.id));
        }
        if self.id.ends_with('-') {
            errors.push(format!("插件 ID「{}」不能以连字符「-」结尾", self.id));
        }
        if self.id.contains("--") {
            errors.push(format!(
                "插件 ID「{}」包含连续连字符「--」，不允许使用",
                self.id
            ));
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            errors.push(format!(
                "插件 ID「{}」格式无效，只允许小写字母、数字和连字符",
                self.id
            ));
        }

        if self.entry.is_empty() {
            errors.push("入口文件不能为空".into());
        }

        // 路径遍历防护 — 统一检查（使用 std::path::Component 方式）
        let entry_path = std::path::Path::new(&self.entry);

        // 1. 空字节注入检测
        if self.entry.contains('\0') {
            errors.push(format!(
                "入口文件「{}」包含空字节，可能为注入攻击",
                self.entry.replace('\0', "\\0")
            ));
        }

        // 2. 长度限制（防止超长路径）
        if self.entry.len() > 255 {
            errors.push(format!(
                "入口文件路径过长 ({} 字符 > 255 限制)",
                self.entry.len()
            ));
        }

        // 3. Component 级别路径遍历检查（统一检查，替代冗余的字符串级别检查）
        if entry_path.is_absolute() {
            errors.push(format!(
                "入口文件「{}」不允许使用绝对路径，请使用相对于插件目录的文件名",
                self.entry
            ));
        }
        if entry_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(format!(
                "入口文件「{}」包含 .. 路径遍历成分，可能逃逸插件目录",
                self.entry
            ));
        }

        // 4. Windows 盘符检测（防止 C:\evil 等绝对路径）
        let first_two: String = self.entry.chars().take(2).collect();
        if let Some(&byte) = first_two.as_bytes().first()
            && byte.is_ascii_alphabetic()
            && first_two.len() == 2
            && &first_two[1..] == ":"
        {
            errors.push(format!(
                "入口文件「{}」不允许使用 Windows 盘符路径",
                self.entry
            ));
        }

        // Config Plugin 不能申请 network 权限
        if self.plugin_type == PluginType::Config
            && self.permissions.contains(&Permission::NetworkOutbound)
        {
            errors.push(
                "Config Plugin 不允许申请 network:outbound 权限。\
                 如需下载外部资源，请使用 onSubscribeFetch 钩子由引擎代为请求。"
                    .into(),
            );
        }

        if self.timeout < 100 || self.timeout > 300_000 {
            errors.push(format!(
                "超时时间 {}ms 超出允许范围 (100 ~ 300,000 ms)",
                self.timeout
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// 插件类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// 配置插件（运行在 rquickjs 沙箱中）
    Config,

    /// UI 扩展（运行在前端 iframe 中）
    Ui,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_minimal_json() -> &'static str {
        r#"{
            "id": "test-plugin",
            "name": "Test Plugin",
            "version": "1.0.0",
            "type": "config",
            "permissions": ["config:read"],
            "entry": "index.js"
        }"#
    }

    fn valid_full_json() -> &'static str {
        r#"{
            "id": "full-plugin",
            "name": "Full Plugin",
            "version": "2.1.0",
            "type": "ui",
            "permissions": ["config:read", "config:write", "store:readwrite"],
            "hooks": ["OnSubscribeParsed", "OnMerged"],
            "entry": "main.js",
            "scope": "subscribe",
            "timeout": 10000,
            "author": "tester",
            "description": "A comprehensive test plugin",
            "min_engine_version": "0.1.0"
        }"#
    }

    #[test]
    fn from_json_valid_minimal() {
        let manifest = PluginManifest::from_json(valid_minimal_json()).unwrap();
        assert_eq!(manifest.id, "test-plugin");
        assert_eq!(manifest.name, "Test Plugin");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.plugin_type, PluginType::Config);
        assert_eq!(manifest.entry, "index.js");
        assert_eq!(manifest.scope, "all"); // default
        assert_eq!(manifest.timeout, 5000); // default
        assert!(manifest.author.is_none());
        assert!(manifest.description.is_none());
    }

    #[test]
    fn from_json_valid_full() {
        let manifest = PluginManifest::from_json(valid_full_json()).unwrap();
        assert_eq!(manifest.id, "full-plugin");
        assert_eq!(manifest.name, "Full Plugin");
        assert_eq!(manifest.version, "2.1.0");
        assert_eq!(manifest.plugin_type, PluginType::Ui);
        assert_eq!(manifest.entry, "main.js");
        assert_eq!(manifest.scope, "subscribe");
        assert_eq!(manifest.timeout, 10000);
        assert_eq!(manifest.author.as_deref(), Some("tester"));
        assert_eq!(
            manifest.description.as_deref(),
            Some("A comprehensive test plugin")
        );
        assert_eq!(manifest.min_engine_version.as_deref(), Some("0.1.0"));
        assert_eq!(manifest.hooks.len(), 2);
    }

    #[test]
    fn validate_valid_entry() {
        let manifest = PluginManifest::from_json(valid_minimal_json()).unwrap();
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_entry() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": ""
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("入口文件不能为空")));
    }

    #[test]
    fn validate_rejects_null_byte_injection() {
        // JSON strings cannot contain literal null bytes, so construct directly
        let mut manifest = PluginManifest::from_json(valid_minimal_json()).unwrap();
        manifest.entry = "index.js\0".to_string();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("空字节")));
    }

    #[test]
    fn validate_rejects_path_traversal() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "../etc/passwd"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("路径遍历") || e.contains("非法路径"))
        );
    }

    #[test]
    fn validate_rejects_absolute_path() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "/etc/passwd"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("非法路径") || e.contains("绝对路径"))
        );
    }

    #[test]
    fn validate_rejects_windows_path() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "C:\\Windows\\System32\\cmd.exe"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("Windows 盘符")));
    }

    #[test]
    fn validate_rejects_double_slash() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "/etc/passwd"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("绝对路径")));
    }

    #[test]
    fn validate_rejects_dot_segment() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "../etc/passwd"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("路径遍历")));
    }

    #[test]
    fn validate_rejects_too_long_path() {
        let long_entry = "a".repeat(256);
        let json = format!(
            r#"{{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "{}"
        }}"#,
            long_entry
        );
        let manifest = PluginManifest::from_json(&json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("路径过长")));
    }

    #[test]
    fn validate_rejects_empty_id() {
        let json = r#"{
            "id": "",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("ID 不能为空")));
    }

    #[test]
    fn validate_rejects_invalid_id_chars() {
        let json = r#"{
            "id": "My Plugin!",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("格式无效")));
    }

    #[test]
    fn validate_rejects_config_with_network_outbound() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": ["network:outbound"],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("network:outbound")));
    }

    #[test]
    fn validate_rejects_timeout_too_low() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js",
            "timeout": 50
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("超时时间")));
    }

    #[test]
    fn validate_rejects_timeout_too_high() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js",
            "timeout": 500000
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("超时时间")));
    }

    #[test]
    fn timeout_ms_custom() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js",
            "timeout": 15000
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        assert_eq!(manifest.timeout_ms(), 15_000);
    }

    #[test]
    fn timeout_ms_default() {
        let manifest = PluginManifest::from_json(valid_minimal_json()).unwrap();
        assert_eq!(manifest.timeout_ms(), 5_000);
    }

    #[test]
    fn plugin_type_variants_are_distinct() {
        assert_ne!(PluginType::Config, PluginType::Ui);
    }

    #[test]
    fn plugin_type_config_serde_roundtrip() {
        let json = r#""config""#;
        let parsed: PluginType = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, PluginType::Config);
    }

    #[test]
    fn plugin_type_ui_serde_roundtrip() {
        let json = r#""ui""#;
        let parsed: PluginType = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, PluginType::Ui);
    }

    #[test]
    fn validate_accepts_subdirectory_entry() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "src/index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn validate_accepts_valid_timeout_boundary_low() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js",
            "timeout": 100
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        assert!(
            manifest.validate().is_ok(),
            "timeout=100 is the minimum allowed"
        );
    }

    #[test]
    fn validate_accepts_valid_timeout_boundary_high() {
        let json = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js",
            "timeout": 300000
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        assert!(
            manifest.validate().is_ok(),
            "timeout=300000 is the maximum allowed"
        );
    }

    // ─── 插件 ID 验证扩展测试 ───

    /// 合法 ID：标准格式的小写字母+数字+连字符组合。
    #[test]
    fn test_validate_plugin_id_valid() {
        let valid_ids = vec![
            "my-plugin",
            "smart-grouping",
            "a",
            "plugin-123",
            "abc-def-ghi",
            "x1-y2-z3",
        ];
        for id in valid_ids {
            let json = format!(
                r#"{{
                    "id": "{}",
                    "name": "Test",
                    "version": "1.0.0",
                    "type": "config",
                    "permissions": [],
                    "entry": "index.js"
                }}"#,
                id
            );
            let manifest = PluginManifest::from_json(&json).unwrap();
            assert!(manifest.validate().is_ok(), "ID '{}' 应通过验证", id);
        }
    }

    /// 非法 ID：连续连字符 "my--plugin"。
    /// 验证逻辑应拒绝包含连续连字符的 ID。
    #[test]
    fn test_validate_plugin_id_consecutive_hyphens() {
        let json = r#"{
            "id": "my--plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let result = manifest.validate();
        assert!(result.is_err(), "连续连字符「--」应被拒绝");
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("连续连字符")),
            "错误消息应提及连续连字符，实际: {:?}",
            errors
        );
    }

    /// 非法 ID：前导连字符 "-plugin"。
    /// 验证逻辑应拒绝以连字符开头的 ID。
    #[test]
    fn test_validate_plugin_id_leading_hyphen() {
        let json = r#"{
            "id": "-plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("连字符") && e.contains("开头")),
            "前导连字符应被拒绝，实际错误: {:?}",
            errors
        );
    }

    /// 非法 ID：尾部连字符 "plugin-"。
    /// 验证逻辑应拒绝以连字符结尾的 ID。
    #[test]
    fn test_validate_plugin_id_trailing_hyphen() {
        let json = r#"{
            "id": "plugin-",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let result = manifest.validate();
        assert!(result.is_err(), "尾部连字符「-」应被拒绝");
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("结尾") || e.contains("连字符")),
            "错误消息应提及尾部连字符，实际: {:?}",
            errors
        );
    }

    /// 非法 ID：空字符串。
    /// 验证逻辑应拒绝空 ID。
    #[test]
    fn test_validate_plugin_id_empty() {
        let json = r#"{
            "id": "",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("ID") && e.contains("空")),
            "空 ID 应被拒绝，实际错误: {:?}",
            errors
        );
    }

    /// 非法 ID：包含点号 "my.plugin"。
    /// 验证逻辑应拒绝包含非 [a-z0-9-] 字符的 ID。
    #[test]
    fn test_validate_plugin_id_with_dots() {
        let json = r#"{
            "id": "my.plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("格式无效")),
            "包含点号的 ID 应被拒绝，实际错误: {:?}",
            errors
        );
    }

    /// 非法 ID：大写字母 "MyPlugin"。
    /// 验证逻辑应拒绝包含大写字母的 ID（仅允许小写）。
    #[test]
    fn test_validate_plugin_id_uppercase() {
        let json = r#"{
            "id": "MyPlugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("格式无效")),
            "大写字母 ID 应被拒绝，实际错误: {:?}",
            errors
        );
    }

    /// 合法 ID：包含数字 "plugin-123"。
    /// 验证逻辑应允许数字出现在 ID 中。
    #[test]
    fn test_validate_plugin_id_with_numbers() {
        let json = r#"{
            "id": "plugin-123",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        assert!(
            manifest.validate().is_ok(),
            "包含数字的 ID 'plugin-123' 应通过验证"
        );
    }

    /// 非法 ID：包含下划线 "my_plugin"。
    /// 验证逻辑应拒绝包含下划线的 ID（仅允许连字符作为分隔符）。
    #[test]
    fn test_validate_plugin_id_with_underscores() {
        let json = r#"{
            "id": "my_plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("格式无效")),
            "包含下划线的 ID 应被拒绝，实际错误: {:?}",
            errors
        );
    }

    /// 非法 ID：包含空格 "my plugin"。
    /// 验证逻辑应拒绝包含空格的 ID。
    #[test]
    fn test_validate_plugin_id_with_spaces() {
        let json = r#"{
            "id": "my plugin",
            "name": "Test",
            "version": "1.0.0",
            "type": "config",
            "permissions": [],
            "entry": "index.js"
        }"#;
        let manifest = PluginManifest::from_json(json).unwrap();
        let errors = manifest.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("格式无效")),
            "包含空格的 ID 应被拒绝，实际错误: {:?}",
            errors
        );
    }
}
