//! 多组件插件架构
//!
//! 参考 Claude Code LoadedPlugin: 一个插件可同时提供
//! patches / scripts / hooks / templates / scorers / validators。
//!
//! ## 设计原则
//!
//! - 每种组件独立声明、按需加载
//! - 组件之间通过 PrismContext 共享状态
//! - 加载失败的组件不影响其他组件（软失败策略）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════

/// 验证相对路径是否安全（无路径遍历攻击）
///
/// 检查项：
/// 1. 不包含 null 字节
/// 2. 不包含 `..` 路径段
/// 3. 不以 `/` 或 `\` 开头（绝对路径伪装）
/// 4. canonicalize 后仍在 base_dir 内
fn validate_path_within_base(rel: &str, base_dir: &Path) -> Result<PathBuf, String> {
    // 1. 检查 null 字节
    if rel.contains('\0') {
        return Err(format!("路径包含 null 字节: {}", rel));
    }

    // 2. 检查 .. 路径段
    let path = Path::new(rel);
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(format!("路径包含 '..' 遍历: {}", rel));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("路径包含绝对路径前缀: {}", rel));
            }
            _ => {}
        }
    }

    // 3. 检查是否以 / 或 \ 开头
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(format!("路径以路径分隔符开头: {}", rel));
    }

    let abs = base_dir.join(rel);

    // 4. canonicalize 后确认仍在 base_dir 内
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| format!("无法 canonicalize base_dir '{}': {}", base_dir.display(), e))?;
    let canonical_abs = abs.canonicalize().map_err(|_e| {
        let abs_str = abs.to_string_lossy();
        let base_str = canonical_base.to_string_lossy();
        if !abs_str.starts_with(&*base_str)
            || abs_str.len() > base_str.len()
                && !abs_str[base_str.len()..].starts_with(std::path::MAIN_SEPARATOR)
        {
            format!(
                "路径遍历攻击检测（字符串级别回退）: '{}' 不在 base_dir '{}' 内",
                abs.display(),
                canonical_base.display()
            )
        } else {
            // 路径前缀检查通过，但文件不存在 — 返回友好错误
            format!("路径不存在或无法解析: {}", abs.display())
        }
    })?;

    if !canonical_abs.starts_with(&canonical_base) {
        return Err(format!(
            "路径遍历攻击检测: '{}' 解析后 '{}' 不在 base_dir '{}' 内",
            rel,
            canonical_abs.display(),
            canonical_base.display()
        ));
    }

    Ok(abs)
}

// ══════════════════════════════════════════════════════════
// 组件加载辅助函数
// ══════════════════════════════════════════════════════════

/// 加载单个文件组件（路径遍历验证 + 存在性检查）
///
/// 返回验证通过的绝对路径；失败时向 warnings 追加原因并返回 None。
fn load_file_component(
    rel: &str,
    base_dir: &Path,
    component_kind: &str,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    match validate_path_within_base(rel, base_dir) {
        Ok(abs) if abs.exists() => Some(abs),
        Ok(_) => {
            warnings.push(format!("{}文件不存在: {}", component_kind, rel));
            None
        }
        Err(e) => {
            warnings.push(format!("{}路径验证失败 '{}': {}", component_kind, rel, e));
            None
        }
    }
}

// ══════════════════════════════════════════════════════════
// Manifest 扩展
// ══════════════════════════════════════════════════════════

/// 多组件插件的 manifest 扩展
///
/// 嵌入到 `manifest.json` 的 `components` 字段中，
/// 声明插件提供的所有可选组件。
///
/// ```json
/// {
///     "components": {
///         "patches": ["patches/dns.prism.yaml"],
///         "scripts": ["scripts/check.js"],
///         "hooks": { "OnBeforeWrite": "scripts/validate.js" },
///         "templates": ["templates/custom.yaml"],
///         "scorers": ["scorers/latency.js"],
///         "validators": ["validators/syntax.js"]
///     }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComponentManifest {
    /// DSL 增强文件目录（相对于插件根目录）
    #[serde(default)]
    pub patches: Vec<String>,

    /// JS 脚本目录（相对于插件根目录）
    #[serde(default)]
    pub scripts: Vec<String>,

    /// 生命周期钩子配置：事件名称 → 脚本路径
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,

    /// 配置模板（相对于插件根目录）
    #[serde(default)]
    pub templates: Vec<String>,

    /// 自定义评分算法（相对于插件根目录）
    #[serde(default)]
    pub scorers: Vec<String>,

    /// 自定义校验规则（相对于插件根目录）
    #[serde(default)]
    pub validators: Vec<String>,
}

impl ComponentManifest {
    /// 检查 manifest 是否声明了任何组件
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
            && self.scripts.is_empty()
            && self.hooks.is_empty()
            && self.templates.is_empty()
            && self.scorers.is_empty()
            && self.validators.is_empty()
    }

    /// 获取声明组件的总数量
    pub fn component_count(&self) -> usize {
        self.patches.len()
            + self.scripts.len()
            + self.hooks.len()
            + self.templates.len()
            + self.scorers.len()
            + self.validators.len()
    }
}

// ══════════════════════════════════════════════════════════
// 已加载组件
// ══════════════════════════════════════════════════════════

/// 已加载的组件实例
///
/// 包含所有组件的解析后绝对路径，供运行时直接使用。
/// 加载失败的组件会被静默跳过（软失败策略），
/// 通过 `load_warnings` 记录跳过原因。
#[derive(Debug, Clone)]
pub struct LoadedComponents {
    /// 所属插件 ID
    pub plugin_id: String,

    /// 插件根目录（绝对路径）
    pub base_dir: PathBuf,

    /// 已加载的 patch 文件路径
    pub patches: Vec<PathBuf>,

    /// 已加载的脚本文件路径
    pub scripts: Vec<PathBuf>,

    /// 已加载的钩子：(事件名称, 脚本绝对路径)
    pub hooks: Vec<(String, PathBuf)>,

    /// 已加载的模板文件路径
    pub templates: Vec<PathBuf>,

    /// 已加载的评分算法文件路径
    pub scorers: Vec<PathBuf>,

    /// 已加载的校验规则文件路径
    pub validators: Vec<PathBuf>,

    /// 加载过程中的警告信息（文件不存在等）
    pub load_warnings: Vec<String>,
}

impl LoadedComponents {
    /// 从 manifest 和插件目录加载所有组件
    ///
    /// 采用软失败策略：单个组件加载失败不会中断整体加载，
    /// 而是记录警告并继续处理其他组件。
    ///
    /// # Arguments
    /// * `plugin_id` - 插件唯一标识符
    /// * `base_dir` - 插件根目录（绝对路径）
    /// * `manifest` - 组件清单
    ///
    /// # Returns
    /// 加载完成的组件实例（可能包含部分失败的警告）
    pub fn load(
        plugin_id: &str,
        base_dir: &Path,
        manifest: &ComponentManifest,
    ) -> Result<Self, String> {
        let mut warnings = Vec::new();

        // 加载 patches
        let patches: Vec<PathBuf> = manifest
            .patches
            .iter()
            .filter_map(|rel| load_file_component(rel, base_dir, "patch", &mut warnings))
            .collect();

        // 加载 scripts
        let scripts: Vec<PathBuf> = manifest
            .scripts
            .iter()
            .filter_map(|rel| load_file_component(rel, base_dir, "脚本", &mut warnings))
            .collect();

        // 加载 hooks（键值对形式）
        let hooks: Vec<(String, PathBuf)> = manifest
            .hooks
            .iter()
            .filter_map(|(event, rel)| {
                load_file_component(rel, base_dir, &format!("钩子 '{}'", event), &mut warnings)
                    .map(|abs| (event.clone(), abs))
            })
            .collect();

        // 加载 templates
        let templates: Vec<PathBuf> = manifest
            .templates
            .iter()
            .filter_map(|rel| load_file_component(rel, base_dir, "模板", &mut warnings))
            .collect();

        // 加载 scorers
        let scorers: Vec<PathBuf> = manifest
            .scorers
            .iter()
            .filter_map(|rel| load_file_component(rel, base_dir, "评分算法", &mut warnings))
            .collect();

        // 加载 validators
        let validators: Vec<PathBuf> = manifest
            .validators
            .iter()
            .filter_map(|rel| load_file_component(rel, base_dir, "校验规则", &mut warnings))
            .collect();

        Ok(Self {
            plugin_id: plugin_id.to_string(),
            base_dir: base_dir.to_path_buf(),
            patches,
            scripts,
            hooks,
            templates,
            scorers,
            validators,
            load_warnings: warnings,
        })
    }

    /// 检查是否有任何已加载的组件
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
            && self.scripts.is_empty()
            && self.hooks.is_empty()
            && self.templates.is_empty()
            && self.scorers.is_empty()
            && self.validators.is_empty()
    }

    /// 获取已加载组件的总数量
    pub fn total_count(&self) -> usize {
        self.patches.len()
            + self.scripts.len()
            + self.hooks.len()
            + self.templates.len()
            + self.scorers.len()
            + self.validators.len()
    }

    /// 获取组件摘要（人类可读）
    pub fn summary(&self) -> String {
        let total = self.total_count();
        if total == 0 {
            return format!("[插件 {}] 无已加载组件", self.plugin_id);
        }

        let mut parts = Vec::new();
        if !self.patches.is_empty() {
            parts.push(format!("{} 个 patch", self.patches.len()));
        }
        if !self.scripts.is_empty() {
            parts.push(format!("{} 个脚本", self.scripts.len()));
        }
        if !self.hooks.is_empty() {
            parts.push(format!("{} 个钩子", self.hooks.len()));
        }
        if !self.templates.is_empty() {
            parts.push(format!("{} 个模板", self.templates.len()));
        }
        if !self.scorers.is_empty() {
            parts.push(format!("{} 个评分算法", self.scorers.len()));
        }
        if !self.validators.is_empty() {
            parts.push(format!("{} 个校验规则", self.validators.len()));
        }

        format!(
            "[插件 {}] 共 {} 个组件: {}",
            self.plugin_id,
            total,
            parts.join(", ")
        )
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_component_manifest_default() {
        let manifest: ComponentManifest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(manifest.patches.is_empty());
        assert!(manifest.scripts.is_empty());
        assert!(manifest.hooks.is_empty());
        assert!(manifest.templates.is_empty());
        assert!(manifest.scorers.is_empty());
        assert!(manifest.validators.is_empty());
        assert!(manifest.is_empty());
        assert_eq!(manifest.component_count(), 0);
    }

    #[test]
    fn test_component_manifest_parse() {
        let json = serde_json::json!({
            "patches": ["patches/dns.prism.yaml"],
            "scripts": ["scripts/check.js"],
            "hooks": { "OnBeforeWrite": "scripts/validate.js" }
        });
        let manifest: ComponentManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.patches.len(), 1);
        assert_eq!(manifest.patches[0], "patches/dns.prism.yaml");
        assert_eq!(manifest.scripts.len(), 1);
        assert_eq!(manifest.hooks.len(), 1);
        assert_eq!(
            manifest.hooks.get("OnBeforeWrite").unwrap(),
            "scripts/validate.js"
        );
        assert!(!manifest.is_empty());
        assert_eq!(manifest.component_count(), 3);
    }

    #[test]
    fn test_component_manifest_full() {
        let json = serde_json::json!({
            "patches": ["p1.yaml", "p2.yaml"],
            "scripts": ["s1.js"],
            "hooks": { "OnMerged": "h1.js", "OnBeforeWrite": "h2.js" },
            "templates": ["t1.yaml"],
            "scorers": ["sc1.js"],
            "validators": ["v1.js"]
        });
        let manifest: ComponentManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.component_count(), 8);
        assert_eq!(manifest.patches.len(), 2);
        assert_eq!(manifest.hooks.len(), 2);
    }

    #[test]
    fn test_component_manifest_serde_roundtrip() {
        let original = ComponentManifest {
            patches: vec!["a.yaml".into()],
            scripts: vec!["b.js".into()],
            hooks: BTreeMap::from([("OnTest".into(), "t.js".into())]),
            templates: vec!["c.yaml".into()],
            scorers: vec![],
            validators: vec![],
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ComponentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.patches.len(), 1);
        assert_eq!(deserialized.hooks.len(), 1);
    }

    #[test]
    fn test_loaded_components_with_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // 创建一些测试文件
        std::fs::create_dir_all(base.join("patches")).unwrap();
        std::fs::create_dir_all(base.join("scripts")).unwrap();
        std::fs::write(base.join("patches/dns.yaml"), "test").unwrap();
        std::fs::write(base.join("scripts/check.js"), "test").unwrap();

        let manifest = ComponentManifest {
            patches: vec!["patches/dns.yaml".into()],
            scripts: vec!["scripts/check.js".into(), "scripts/missing.js".into()],
            hooks: BTreeMap::from([("OnBeforeWrite".into(), "scripts/validate.js".into())]),
            templates: vec![],
            scorers: vec![],
            validators: vec![],
        };

        let loaded = LoadedComponents::load("test-plugin", base, &manifest).unwrap();
        assert_eq!(loaded.patches.len(), 1);
        assert_eq!(loaded.scripts.len(), 1); // missing.js 被跳过
        assert_eq!(loaded.hooks.len(), 0); // validate.js 不存在
        assert_eq!(loaded.load_warnings.len(), 2);
        assert!(!loaded.is_empty());
        assert_eq!(loaded.total_count(), 2);

        let summary = loaded.summary();
        assert!(summary.contains("test-plugin"));
        assert!(summary.contains("2 个组件"));
    }

    #[test]
    fn test_loaded_components_empty() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = ComponentManifest::default();

        let loaded = LoadedComponents::load("empty-plugin", dir.path(), &manifest).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.total_count(), 0);
        assert!(loaded.load_warnings.is_empty());

        let summary = loaded.summary();
        assert!(summary.contains("无已加载组件"));
    }

    #[test]
    fn test_loaded_components_all_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = ComponentManifest {
            patches: vec!["missing.yaml".into()],
            scripts: vec!["missing.js".into()],
            hooks: BTreeMap::from([("OnTest".into(), "missing_hook.js".into())]),
            templates: vec!["missing.tpl".into()],
            scorers: vec!["missing_sc.js".into()],
            validators: vec!["missing_v.js".into()],
        };

        let loaded = LoadedComponents::load("bad-plugin", dir.path(), &manifest).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.load_warnings.len(), 6);
    }
}
