//! 沙箱安全配置
//!
//! 定义 rquickjs 运行时的安全边界：
//! - 禁止文件系统访问
//! - 禁止网络访问（Config Plugin 通过引擎代理请求）
//! - 禁止进程操作
//! - 限制内存和执行时间
//!
//! ## 集成状态
//!
//! `SandboxConfig` 已完全集成到 `ScriptRuntime`，通过以下方式生效：
//! 1. **沙箱感知安全验证** — `runtime.rs::execute()` 根据 `allow_network` /
//!    `allow_filesystem` 等标志在执行前进行词法级模式检测
//! 2. **运行时全局对象加固** — `Object.defineProperty` per-property locking 阻止脚本修改/重新引入危险属性（兼容 quickjs-ng）
//! 3. **rquickjs 内置限制** — 无文件系统/网络 API，内存上限，超时中断
//! 4. **Builder 模式** — 通过 `ScriptRuntime::with_sandbox()` 或 `.sandbox()` 注入配置
//!
//! 插件级别的细粒度权限控制通过 `permitted_plugins` 映射实现，
//! 每个插件可拥有独立的 `SandboxConfig`。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 沙箱权限枚举
///
/// 表示单个可授权的能力维度，用于插件级别的细粒度权限检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxPermission {
    /// 网络请求权限（fetch, XMLHttpRequest, WebSocket 等）
    Network,
    /// 文件系统访问权限（fs, path 等）
    Filesystem,
    /// 子进程创建权限（spawn, exec, child_process 等）
    ChildProcess,
    /// Worker 线程权限（用于并行脚本执行）
    Workers,
}

/// 沙箱配置
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// 是否允许网络请求（v1 Config Plugin 不允许）
    pub allow_network: bool,

    /// 是否允许文件系统访问（不允许）
    pub allow_filesystem: bool,

    /// 是否允许子进程（不允许）
    pub allow_child_process: bool,

    /// 是否允许 Worker 线程（可选，用于并行脚本）
    pub allow_workers: bool,

    /// 插件级别的沙箱配置映射
    ///
    /// 键为 plugin_id，值为该插件专属的沙箱配置。
    /// 未在此映射中注册的插件将使用默认的 strict 配置。
    pub permitted_plugins: HashMap<String, SandboxConfig>,
}

impl Default for SandboxConfig {
    /// Default 与 strict() 行为一致：所有危险能力均被禁止。
    /// 显式委托给 strict() 以明确语义（L-15）。
    fn default() -> Self {
        Self::strict()
    }
}

impl SandboxConfig {
    /// 创建 v1 标准沙箱（最严格模式）
    pub fn strict() -> Self {
        Self {
            allow_network: false,
            allow_filesystem: false,
            allow_child_process: false,
            allow_workers: false,
            permitted_plugins: HashMap::new(),
        }
    }

    /// 创建开发用沙箱
    ///
    /// ## 安全警告
    ///
    /// 开发模式开放了网络出站权限（`allow_network: true`），
    /// 允许脚本使用 `fetch()` 等网络 API 请求外部资源。
    /// **切勿在生产环境中使用开发模式沙箱。**
    ///
    /// 其他危险能力（文件系统、子进程、Worker 线程）仍然被禁止。
    ///
    /// 开发模式仅开放网络出站权限（`allow_network: true`），
    /// 文件系统、子进程、Worker 线程等危险能力仍然被禁止。
    /// 这允许开发者在调试时使用 `fetch()` 请求外部 API，
    /// 同时保持其他安全边界不变。
    pub fn development() -> Self {
        Self {
            allow_network: true, // 开发时允许调试请求
            ..Self::default()
        }
    }

    /// Validate that the sandbox configuration is in strict (safe) mode.
    ///
    /// Returns `true` only when all dangerous capabilities are disabled:
    ///
    /// Worker threads could be used to bypass sandbox restrictions by
    /// offloading privileged operations to a worker context.
    ///
    /// ## 维护说明
    ///
    /// **当新增沙箱能力维度时，必须同步更新此方法！**
    /// 具体步骤：
    /// 1. 在 `SandboxConfig` 结构体中添加新的 `allow_*` 布尔字段
    /// 2. 在 `SandboxPermission` 枚举中添加对应的变体
    /// 3. 在此 `is_safe()` 方法中添加 `&& !self.allow_new_capability` 检查
    /// 4. 在 `is_plugin_permitted()` 中添加对应的 match 分支
    /// 5. 在 `runtime.rs::execute()` 的沙箱感知安全检查中添加对应的词法检测
    ///
    /// 遗漏任何一步都可能导致沙箱绕过漏洞。
    ///
    /// 下面的 `_SANDBOX_FIELD_COUNT` 常量用于在编译时提醒开发者同步更新。
    /// 当新增 `allow_*` 字段时，请递增此常量值，否则 `is_safe()` 中遗漏新字段
    /// 检查的风险极高。
    pub fn is_safe(&self) -> bool {
        // 编译时提醒：SandboxConfig 的 allow_* 布尔字段数量。
        // 新增能力时必须递增此值，并同步更新下方检查逻辑。
        // 当前字段：allow_network, allow_filesystem, allow_child_process, allow_workers
        const _SANDBOX_FIELD_COUNT: usize = 4;

        !self.allow_filesystem
            && !self.allow_child_process
            && !self.allow_network
            && !self.allow_workers
    }

    /// 获取指定插件的沙箱配置
    ///
    /// 如果插件已注册，返回其专属配置；否则返回默认的 strict 配置。
    pub fn get_config_for_plugin(&self, plugin_id: &str) -> &SandboxConfig {
        self.permitted_plugins
            .get(plugin_id)
            .unwrap_or_else(|| strict_config())
    }

    /// 为指定插件注册沙箱权限
    ///
    /// 将 plugin_id 映射到其专属的 `SandboxConfig`。
    pub fn grant(&mut self, plugin_id: impl Into<String>, config: SandboxConfig) {
        self.permitted_plugins.insert(plugin_id.into(), config);
    }

    /// 撤销指定插件的全部沙箱权限
    ///
    /// 移除后，该插件将回退到默认的 strict 配置。
    pub fn revoke(&mut self, plugin_id: &str) {
        self.permitted_plugins.remove(plugin_id);
    }

    /// 检查指定插件是否拥有某项权限
    ///
    /// 先查找插件专属配置，若未注册则使用 strict 默认值（全部拒绝）。
    pub fn is_plugin_permitted(&self, plugin_id: &str, permission: SandboxPermission) -> bool {
        let config = self.get_config_for_plugin(plugin_id);
        match permission {
            SandboxPermission::Network => config.allow_network,
            SandboxPermission::Filesystem => config.allow_filesystem,
            SandboxPermission::ChildProcess => config.allow_child_process,
            SandboxPermission::Workers => config.allow_workers,
        }
    }
}

/// 全局 strict 沙箱配置（OnceLock 延迟初始化，避免 static 中的 HashMap::new()）
static STRICT_CONFIG: OnceLock<SandboxConfig> = OnceLock::new();

fn strict_config() -> &'static SandboxConfig {
    STRICT_CONFIG.get_or_init(|| SandboxConfig {
        allow_network: false,
        allow_filesystem: false,
        allow_child_process: false,
        allow_workers: false,
        permitted_plugins: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_config_is_safe() {
        let config = SandboxConfig::strict();
        assert!(!config.allow_network);
        assert!(!config.allow_filesystem);
        assert!(!config.allow_child_process);
        assert!(!config.allow_workers);
        assert!(config.permitted_plugins.is_empty());
    }

    #[test]
    fn development_allows_network() {
        let config = SandboxConfig::development();
        assert!(
            config.allow_network,
            "Development mode should allow network"
        );
        assert!(!config.allow_filesystem);
        assert!(!config.allow_child_process);
        assert!(!config.allow_workers);
    }

    #[test]
    fn is_safe_strict_returns_true() {
        let config = SandboxConfig::strict();
        assert!(config.is_safe());
    }

    #[test]
    fn is_safe_development_returns_false() {
        let config = SandboxConfig::development();
        assert!(
            !config.is_safe(),
            "Development mode with network should not be safe"
        );
    }

    #[test]
    fn is_safe_partial_open_returns_false() {
        let mut config = SandboxConfig::strict();
        config.allow_workers = true;
        assert!(!config.is_safe(), "Enabling workers should break safety");
    }

    #[test]
    fn grant_and_revoke_permission() {
        let mut config = SandboxConfig::strict();
        let plugin_config = SandboxConfig {
            allow_network: true,
            ..SandboxConfig::default()
        };

        // Grant
        config.grant("test-plugin", plugin_config);
        assert!(config.permitted_plugins.contains_key("test-plugin"));

        // Verify granted config
        let retrieved = config.get_config_for_plugin("test-plugin");
        assert!(retrieved.allow_network);

        // Revoke
        config.revoke("test-plugin");
        assert!(!config.permitted_plugins.contains_key("test-plugin"));

        // After revoke, falls back to strict
        let fallback = config.get_config_for_plugin("test-plugin");
        assert!(!fallback.allow_network);
    }

    #[test]
    fn is_plugin_permitted_unknown_plugin_denied() {
        let config = SandboxConfig::strict();
        assert!(!config.is_plugin_permitted("unknown", SandboxPermission::Network));
        assert!(!config.is_plugin_permitted("unknown", SandboxPermission::Filesystem));
        assert!(!config.is_plugin_permitted("unknown", SandboxPermission::ChildProcess));
        assert!(!config.is_plugin_permitted("unknown", SandboxPermission::Workers));
    }

    #[test]
    fn is_plugin_permitted_granted_plugin_allowed() {
        let mut config = SandboxConfig::strict();
        let plugin_config = SandboxConfig {
            allow_network: true,
            allow_filesystem: true,
            ..SandboxConfig::default()
        };
        config.grant("my-plugin", plugin_config);

        assert!(config.is_plugin_permitted("my-plugin", SandboxPermission::Network));
        assert!(config.is_plugin_permitted("my-plugin", SandboxPermission::Filesystem));
        assert!(!config.is_plugin_permitted("my-plugin", SandboxPermission::ChildProcess));
        assert!(!config.is_plugin_permitted("my-plugin", SandboxPermission::Workers));
    }

    #[test]
    fn permission_variants_are_distinct() {
        let all = [
            SandboxPermission::Network,
            SandboxPermission::Filesystem,
            SandboxPermission::ChildProcess,
            SandboxPermission::Workers,
        ];
        // All variants should be unique
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
    }

    #[test]
    fn default_matches_strict() {
        let default = SandboxConfig::default();
        let strict = SandboxConfig::strict();
        assert_eq!(default.allow_network, strict.allow_network);
        assert_eq!(default.allow_filesystem, strict.allow_filesystem);
        assert_eq!(default.allow_child_process, strict.allow_child_process);
        assert_eq!(default.allow_workers, strict.allow_workers);
    }

    #[test]
    fn revoke_nonexistent_plugin_does_not_panic() {
        let mut config = SandboxConfig::strict();
        config.revoke("does-not-exist"); // Should not panic
        assert!(config.permitted_plugins.is_empty());
    }

    #[test]
    fn grant_overwrites_previous_config() {
        let mut config = SandboxConfig::strict();
        let config_v1 = SandboxConfig {
            allow_network: true,
            ..SandboxConfig::default()
        };
        let config_v2 = SandboxConfig {
            allow_filesystem: true,
            ..SandboxConfig::default()
        };

        config.grant("plugin", config_v1);
        config.grant("plugin", config_v2);

        let retrieved = config.get_config_for_plugin("plugin");
        assert!(!retrieved.allow_network, "Should be overwritten to v2");
        assert!(retrieved.allow_filesystem, "Should have v2's filesystem");
    }
}
