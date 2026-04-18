//! 插件生命周期管理
//!
//! 管理插件从加载到卸载的完整生命周期：
//! - 发现 → 加载 → 验证 → 注册钩子 → 执行 → 卸载

use crate::hook::Hook;
use crate::manifest::PluginManifest;

/// 非法状态转换错误
///
/// 调用方必须显式处理非法转换，防止静默忽略导致的状态不一致。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// 插件未在状态表中找到
    #[error("插件 '{plugin_id}' 未在状态表中找到")]
    NotFound { plugin_id: String },

    /// 非法状态转换
    #[error("插件 '{plugin_id}' 状态转换不合法：当前 {current_state}，需要 {required_states}")]
    InvalidTransition {
        plugin_id: String,
        current_state: String,
        required_states: String,
    },
}

/// 插件生命周期状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// 已发现但未加载
    Discovered,

    /// 已加载（清单验证通过）
    Loaded,

    /// 已激活（钩子已注册，可接收事件）
    Active,

    /// 已暂停（因错误或用户操作暂停）
    Suspended(String), // 原因

    /// 已卸载
    Unloaded,
}

impl std::fmt::Display for PluginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginState::Discovered => write!(f, "已发现"),
            PluginState::Loaded => write!(f, "已加载"),
            PluginState::Active => write!(f, "运行中"),
            PluginState::Suspended(reason) => write!(f, "已暂停: {}", reason),
            PluginState::Unloaded => write!(f, "已卸载"),
        }
    }
}

/// 生命周期管理器
///
/// 管理所有插件的状态转换和钩子注册/注销。
/// 提供从加载到卸载的完整状态机：
///
/// ```text
/// Discovered → Loaded → Active → Suspended ↔ Active → Unloaded
///    ↑                                    |
///    └────────── (重新发现) ←──────────────┘
/// ```
pub struct LifecycleManager {
    /// 插件状态表
    states: std::collections::HashMap<String, PluginState>,

    /// 钩子注册表：hook → [plugin_id]
    hook_registry: std::collections::HashMap<Hook, Vec<String>>,
}

impl LifecycleManager {
    /// 创建新的生命周期管理器（空状态）
    pub fn new() -> Self {
        Self {
            states: std::collections::HashMap::new(),
            hook_registry: std::collections::HashMap::new(),
        }
    }

    /// 将清单中声明的所有钩子注册到 `hook_registry`（内部辅助方法）。
    ///
    /// 对每个钩子检查是否已存在该插件的注册记录，避免重复注册。
    fn add_hooks_to_registry(&mut self, plugin_id: &str, manifest: &PluginManifest) {
        for hook in &manifest.hooks {
            // 检查重复注册，避免同一插件多次注册同一钩子
            if let Some(listeners) = self.hook_registry.get(hook) {
                if listeners.contains(&plugin_id.to_string()) {
                    tracing::warn!(
                        plugin = plugin_id,
                        hook = %hook,
                        "插件已注册此钩子，跳过重复注册"
                    );
                    continue;
                }
            }
            self.hook_registry
                .entry(hook.clone())
                .or_default()
                .push(plugin_id.to_string());
        }
    }

    /// 注册插件的钩子
    ///
    /// 将插件清单中声明的所有钩子添加到注册表，
    /// 并将插件状态更新为 [`PluginState::Active`]。
    ///
    /// # 前置条件
    /// 插件必须处于 `Loaded` 或 `Suspended` 状态，否则返回错误。
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] — 插件未在状态表中找到
    /// - [`TransitionError::InvalidTransition`] — 插件当前状态不合法
    pub fn register_hooks(
        &mut self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), TransitionError> {
        // 状态转换验证：只允许从 Loaded 或 Suspended 状态调用
        let current_state = match self.states.get(plugin_id) {
            Some(state) => state,
            None => {
                return Err(TransitionError::NotFound {
                    plugin_id: plugin_id.to_string(),
                });
            }
        };

        if !matches!(
            current_state,
            PluginState::Loaded | PluginState::Suspended(_)
        ) {
            return Err(TransitionError::InvalidTransition {
                plugin_id: plugin_id.to_string(),
                current_state: current_state.to_string(),
                required_states: "Loaded | Suspended".to_string(),
            });
        }

        self.add_hooks_to_registry(plugin_id, manifest);

        // 更新状态为 Active
        self.states
            .insert(plugin_id.to_string(), PluginState::Active);
        Ok(())
    }

    /// 获取监听指定钩子的所有插件 ID
    ///
    /// 返回已注册到该钩子的插件 ID 列表（按注册顺序）。
    /// 如果没有监听者则返回空列表。
    pub fn get_listeners(&self, hook: &Hook) -> Vec<&str> {
        self.hook_registry
            .get(hook)
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// 暂停插件
    ///
    /// 将插件状态设为 [`PluginState::Suspended`]，并从**所有**钩子注册中移除。
    /// 暂停的插件不再接收任何事件回调。
    ///
    /// # 前置条件
    /// 插件必须处于 `Active` 状态，否则返回错误。
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] — 插件未在状态表中找到
    /// - [`TransitionError::InvalidTransition`] — 插件当前状态不合法
    ///
    /// # Arguments
    /// * `plugin_id` — 要暂停的插件唯一标识符
    /// * `reason` — 暂停原因（用于日志和 UI 显示）
    pub fn suspend(
        &mut self,
        plugin_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), TransitionError> {
        // 状态转换验证：只允许从 Active 状态调用
        let current_state = match self.states.get(plugin_id) {
            Some(state) => state.clone(),
            None => {
                return Err(TransitionError::NotFound {
                    plugin_id: plugin_id.to_string(),
                });
            }
        };

        if !matches!(current_state, PluginState::Active) {
            return Err(TransitionError::InvalidTransition {
                plugin_id: plugin_id.to_string(),
                current_state: current_state.to_string(),
                required_states: "Active".to_string(),
            });
        }

        self.states
            .insert(plugin_id.to_string(), PluginState::Suspended(reason.into()));

        // 从所有钩子注册中移除此插件，防止暂停期间仍收到回调
        for listeners in self.hook_registry.values_mut() {
            listeners.retain(|id| id != plugin_id);
        }
        Ok(())
    }

    /// Resume a suspended plugin (restore to Active state + re-register hooks).
    ///
    /// without re-registering hooks. After `suspend()` removes the plugin from
    /// `hook_registry`, the resumed plugin would never receive event callbacks.
    /// Now accepts `manifest` to re-register hooks. Callers must pass the plugin's
    /// original manifest to fully restore hook subscriptions.
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] — 插件未在状态表中找到
    /// - [`TransitionError::InvalidTransition`] — 插件当前状态不合法
    pub fn resume(
        &mut self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), TransitionError> {
        let state = match self.states.get(plugin_id) {
            Some(state) => state.clone(),
            None => {
                return Err(TransitionError::NotFound {
                    plugin_id: plugin_id.to_string(),
                });
            }
        };

        if matches!(state, PluginState::Suspended(_)) {
            // Re-register hooks that were removed during suspend
            self.add_hooks_to_registry(plugin_id, manifest);
            self.states
                .insert(plugin_id.to_string(), PluginState::Active);
            Ok(())
        } else {
            Err(TransitionError::InvalidTransition {
                plugin_id: plugin_id.to_string(),
                current_state: state.to_string(),
                required_states: "Suspended".to_string(),
            })
        }
    }

    /// 获取插件当前状态
    ///
    /// 返回 `None` 表示插件未被此管理器追踪（可能从未加载或已被完全移除）。
    pub fn get_state(&self, plugin_id: &str) -> Option<&PluginState> {
        self.states.get(plugin_id)
    }

    /// 发现插件（将插件标记为 Discovered 状态）
    ///
    /// 如果插件已存在于状态表中，则跳过。
    /// 返回 `true` 表示新添加，`false` 表示已存在。
    pub fn discover(&mut self, plugin_id: &str) -> bool {
        if self.states.contains_key(plugin_id) {
            return false;
        }
        self.states
            .insert(plugin_id.to_string(), PluginState::Discovered);
        true
    }

    /// 加载插件（将状态从 Discovered 转为 Loaded）
    ///
    /// # 前置条件
    /// 插件必须处于 `Discovered` 状态，否则返回错误。
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] — 插件未在状态表中找到
    /// - [`TransitionError::InvalidTransition`] — 插件当前状态不合法
    pub fn load(&mut self, plugin_id: &str) -> Result<(), TransitionError> {
        let current_state = match self.states.get(plugin_id) {
            Some(state) => state,
            None => {
                return Err(TransitionError::NotFound {
                    plugin_id: plugin_id.to_string(),
                });
            }
        };

        if !matches!(current_state, PluginState::Discovered) {
            return Err(TransitionError::InvalidTransition {
                plugin_id: plugin_id.to_string(),
                current_state: current_state.to_string(),
                required_states: "Discovered".to_string(),
            });
        }

        self.states
            .insert(plugin_id.to_string(), PluginState::Loaded);
        Ok(())
    }

    /// 卸载插件
    ///
    /// 将插件状态设为 [`PluginState::Unloaded`]，并从所有钩子注册中移除。
    /// 与 `suspend` 不同，`unload` 是不可逆的最终状态。
    ///
    /// # 前置条件
    /// 插件必须处于 `Active` 或 `Suspended` 状态，否则返回错误。
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] — 插件未在状态表中找到
    /// - [`TransitionError::InvalidTransition`] — 插件当前状态不合法
    pub fn unload(&mut self, plugin_id: &str) -> Result<(), TransitionError> {
        // 状态转换验证：只允许从 Active 或 Suspended 状态调用
        let current_state = match self.states.get(plugin_id) {
            Some(state) => state.clone(),
            None => {
                return Err(TransitionError::NotFound {
                    plugin_id: plugin_id.to_string(),
                });
            }
        };

        if !matches!(
            current_state,
            PluginState::Active | PluginState::Suspended(_)
        ) {
            return Err(TransitionError::InvalidTransition {
                plugin_id: plugin_id.to_string(),
                current_state: current_state.to_string(),
                required_states: "Active | Suspended".to_string(),
            });
        }

        self.states
            .insert(plugin_id.to_string(), PluginState::Unloaded);

        // 从所有钩子注册中移除此插件
        for listeners in self.hook_registry.values_mut() {
            listeners.retain(|id| id != plugin_id);
        }
        Ok(())
    }
}

impl Default for LifecycleManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::Hook;
    use crate::manifest::{PluginManifest, PluginType};
    use crate::permission::Permission;

    fn make_manifest(id: &str, hooks: Vec<Hook>) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            name: format!("Test {}", id),
            version: "1.0.0".to_string(),
            plugin_type: PluginType::Config,
            permissions: vec![Permission::ConfigRead],
            hooks,
            entry: "index.js".to_string(),
            scope: "all".to_string(),
            timeout: 5000,
            author: None,
            description: None,
            min_engine_version: None,
            checksum: None,
        }
    }

    #[test]
    fn plugin_state_display_all_variants() {
        assert_eq!(format!("{}", PluginState::Discovered), "已发现");
        assert_eq!(format!("{}", PluginState::Loaded), "已加载");
        assert_eq!(format!("{}", PluginState::Active), "运行中");
        assert_eq!(
            format!("{}", PluginState::Suspended("error".into())),
            "已暂停: error"
        );
        assert_eq!(format!("{}", PluginState::Unloaded), "已卸载");
    }

    #[test]
    fn new_manager_is_empty() {
        let mgr = LifecycleManager::new();
        assert!(mgr.states.is_empty());
        assert!(mgr.hook_registry.is_empty());
    }

    #[test]
    fn register_hooks_from_loaded_to_active() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        // Manually set to Loaded state (simulating prior load)
        mgr.states.insert("p1".to_string(), PluginState::Loaded);

        mgr.register_hooks("p1", &manifest).unwrap();

        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Active));
        let listeners = mgr.get_listeners(&Hook::OnSubscribeParsed);
        assert!(listeners.contains(&"p1"));
    }

    #[test]
    fn register_hooks_from_discovered_returns_err() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        // Plugin is in Discovered state (default for unknown plugins)
        mgr.states.insert("p1".to_string(), PluginState::Discovered);

        let err = mgr.register_hooks("p1", &manifest).unwrap_err();
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));

        // Should remain Discovered (register_hooks rejects non-Loaded/Suspended)
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Discovered));
        let listeners = mgr.get_listeners(&Hook::OnSubscribeParsed);
        assert!(!listeners.contains(&"p1"));
    }

    #[test]
    fn register_hooks_unknown_plugin_returns_err() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("ghost", vec![Hook::OnSubscribeParsed]);

        // Plugin not in states at all
        let err = mgr.register_hooks("ghost", &manifest).unwrap_err();
        assert!(matches!(err, TransitionError::NotFound { .. }));

        assert_eq!(mgr.get_state("ghost"), None);
    }

    #[test]
    fn suspend_from_active_to_suspended() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.register_hooks("p1", &manifest).unwrap();
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Active));

        mgr.suspend("p1", "test suspension").unwrap();

        match mgr.get_state("p1") {
            Some(PluginState::Suspended(reason)) => assert_eq!(reason, "test suspension"),
            other => panic!("Expected Suspended state, got {:?}", other),
        }

        // Should be removed from hook registry
        let listeners = mgr.get_listeners(&Hook::OnSubscribeParsed);
        assert!(!listeners.contains(&"p1"));
    }

    #[test]
    fn suspend_from_non_active_returns_err() {
        let mut mgr = LifecycleManager::new();
        mgr.states.insert("p1".to_string(), PluginState::Loaded);

        let err = mgr.suspend("p1", "should not work").unwrap_err();
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));

        // Should remain Loaded
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Loaded));
    }

    #[test]
    fn resume_from_suspended_to_active() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.register_hooks("p1", &manifest).unwrap();
        mgr.suspend("p1", "pause").unwrap();

        assert!(matches!(
            mgr.get_state("p1"),
            Some(PluginState::Suspended(_))
        ));

        mgr.resume("p1", &manifest).unwrap();

        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Active));
        // Hooks should be re-registered
        let listeners = mgr.get_listeners(&Hook::OnSubscribeParsed);
        assert!(listeners.contains(&"p1"));
    }

    #[test]
    fn resume_from_non_suspended_returns_err() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        let err = mgr.resume("p1", &manifest).unwrap_err();
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));

        // Should remain Loaded (resume only works from Suspended)
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Loaded));
    }

    #[test]
    fn unload_from_active_to_unloaded() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.register_hooks("p1", &manifest).unwrap();

        mgr.unload("p1").unwrap();

        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Unloaded));
        let listeners = mgr.get_listeners(&Hook::OnSubscribeParsed);
        assert!(!listeners.contains(&"p1"));
    }

    #[test]
    fn unload_from_suspended_to_unloaded() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.register_hooks("p1", &manifest).unwrap();
        mgr.suspend("p1", "pause").unwrap();

        mgr.unload("p1").unwrap();

        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Unloaded));
    }

    #[test]
    fn unload_from_discovered_returns_err() {
        let mut mgr = LifecycleManager::new();
        mgr.states.insert("p1".to_string(), PluginState::Discovered);

        let err = mgr.unload("p1").unwrap_err();
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));

        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Discovered));
    }

    #[test]
    fn unload_unknown_plugin_returns_err() {
        let mut mgr = LifecycleManager::new();
        let err = mgr.unload("ghost").unwrap_err(); // Should not panic
        assert!(matches!(err, TransitionError::NotFound { .. }));
        assert_eq!(mgr.get_state("ghost"), None);
    }

    #[test]
    fn get_state_returns_correct_after_transitions() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest("p1", vec![Hook::OnSubscribeParsed]);

        // Initially not tracked
        assert_eq!(mgr.get_state("p1"), None);

        // Simulate discovery
        mgr.states.insert("p1".to_string(), PluginState::Discovered);
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Discovered));

        // Simulate load
        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Loaded));

        // Activate
        mgr.register_hooks("p1", &manifest).unwrap();
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Active));

        // Suspend
        mgr.suspend("p1", "test").unwrap();
        assert!(matches!(
            mgr.get_state("p1"),
            Some(PluginState::Suspended(_))
        ));

        // Resume
        mgr.resume("p1", &manifest).unwrap();
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Active));

        // Unload
        mgr.unload("p1").unwrap();
        assert_eq!(mgr.get_state("p1"), Some(&PluginState::Unloaded));
    }

    #[test]
    fn multiple_plugins_independent_lifecycle() {
        let mut mgr = LifecycleManager::new();
        let m1 = make_manifest("p1", vec![Hook::OnSubscribeParsed]);
        let m2 = make_manifest("p2", vec![Hook::OnMerged]);

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.states.insert("p2".to_string(), PluginState::Loaded);

        mgr.register_hooks("p1", &m1).unwrap();
        mgr.register_hooks("p2", &m2).unwrap();

        // Suspend p1 only
        mgr.suspend("p1", "p1 error").unwrap();

        assert!(matches!(
            mgr.get_state("p1"),
            Some(PluginState::Suspended(_))
        ));
        assert_eq!(mgr.get_state("p2"), Some(&PluginState::Active));

        // p1 should be removed from hooks, p2 should remain
        assert!(!mgr.get_listeners(&Hook::OnSubscribeParsed).contains(&"p1"));
        assert!(mgr.get_listeners(&Hook::OnMerged).contains(&"p2"));
    }

    #[test]
    fn register_hooks_multiple_hooks() {
        let mut mgr = LifecycleManager::new();
        let manifest = make_manifest(
            "p1",
            vec![Hook::OnSubscribeParsed, Hook::OnMerged, Hook::OnBeforeWrite],
        );

        mgr.states.insert("p1".to_string(), PluginState::Loaded);
        mgr.register_hooks("p1", &manifest).unwrap();

        assert!(mgr.get_listeners(&Hook::OnSubscribeParsed).contains(&"p1"));
        assert!(mgr.get_listeners(&Hook::OnMerged).contains(&"p1"));
        assert!(mgr.get_listeners(&Hook::OnBeforeWrite).contains(&"p1"));
    }
}
