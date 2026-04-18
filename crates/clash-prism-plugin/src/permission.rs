//! 权限系统 — 最小权限原则
//!
//! ## 权限清单
//!
//! | 权限 | Config Plugin | UI Extension |
//! |------|:------------:|:------------:|
//! | `config:read` | ✅ | ✅ (受限) |
//! | `config:write` | ✅ | ❌ |
//! | `proxy:test` | ✅ (引擎代为发起) | ❌ |
//! | `proxy:select` | ✅ | ✅ |
//! | `store:readwrite` | ✅ | ✅ |
//! | `network:outbound` | ❌ v1 | ✅ |
//! | `ui:notify` | ✅ | ✅ |
//! | `ui:dialog` | ❌ | ✅ |
//! | `ui:page` | ❌ | ✅ |
//! | `ui:tray` | ❌ | ✅ |

use serde::{Deserialize, Serialize};

/// 插件权限枚举
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Permission {
    /// 读取配置
    #[serde(rename = "config:read")]
    ConfigRead,

    /// 修改配置
    #[serde(rename = "config:write")]
    ConfigWrite,

    /// 请求测速（由引擎代为发起）
    #[serde(rename = "proxy:test")]
    ProxyTest,

    /// 切换代理
    #[serde(rename = "proxy:select")]
    ProxySelect,

    /// KV 持久存储
    #[serde(rename = "store:readwrite")]
    StoreReadWrite,

    /// 外部网络请求（v1 仅限 UI Extension）
    #[serde(rename = "network:outbound")]
    NetworkOutbound,

    /// 显示通知
    #[serde(rename = "ui:notify")]
    UiNotify,

    /// 弹出对话框
    #[serde(rename = "ui:dialog")]
    UiDialog,

    /// 注册自定义页面
    #[serde(rename = "ui:page")]
    UiPage,

    /// 托盘图标/菜单
    #[serde(rename = "ui:tray")]
    UiTray,
}

impl Permission {
    /// 获取所有可用权限列表
    pub fn all() -> Vec<Permission> {
        vec![
            Permission::ConfigRead,
            Permission::ConfigWrite,
            Permission::ProxyTest,
            Permission::ProxySelect,
            Permission::StoreReadWrite,
            Permission::NetworkOutbound,
            Permission::UiNotify,
            Permission::UiDialog,
            Permission::UiPage,
            Permission::UiTray,
        ]
    }

    /// 获取权限的显示名称
    pub fn display_name(&self) -> &str {
        match self {
            Permission::ConfigRead => "读取配置",
            Permission::ConfigWrite => "修改配置",
            Permission::ProxyTest => "请求测速",
            Permission::ProxySelect => "切换代理",
            Permission::StoreReadWrite => "持久化存储",
            Permission::NetworkOutbound => "外部网络请求",
            Permission::UiNotify => "显示通知",
            Permission::UiDialog => "弹出对话框",
            Permission::UiPage => "注册自定义页面",
            Permission::UiTray => "托盘图标/菜单",
        }
    }

    /// 检查此权限是否允许 Config Plugin 使用
    ///
    /// Config Plugin 运行在后端 rquickjs 沙箱中，不允许：
    /// - 网络请求（v1 通过引擎代理）
    /// - UI 操作（对话框、自定义页面、托盘）— 这些是 UI Extension 专属
    pub fn allowed_for_config_plugin(&self) -> bool {
        !matches!(
            self,
            Permission::NetworkOutbound
                | Permission::UiDialog
                | Permission::UiPage
                | Permission::UiTray
        )
    }

    /// 检查此权限是否允许 UI Extension 使用
    pub fn allowed_for_ui_extension(&self) -> bool {
        !matches!(self, Permission::ConfigWrite | Permission::ProxyTest)
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Permission::ConfigRead => write!(f, "config:read"),
            Permission::ConfigWrite => write!(f, "config:write"),
            Permission::ProxyTest => write!(f, "proxy:test"),
            Permission::ProxySelect => write!(f, "proxy:select"),
            Permission::StoreReadWrite => write!(f, "store:readwrite"),
            Permission::NetworkOutbound => write!(f, "network:outbound"),
            Permission::UiNotify => write!(f, "ui:notify"),
            Permission::UiDialog => write!(f, "ui:dialog"),
            Permission::UiPage => write!(f, "ui:page"),
            Permission::UiTray => write!(f, "ui:tray"),
        }
    }
}

/// API 动作枚举 — 用于运行时权限检查
#[derive(Debug, Clone, Copy)]
pub enum PermissionAction {
    ConfigRead,
    ConfigWrite,
    ProxyTest,
    ProxySelect,
    StoreRead,
    StoreWrite,
    NetworkOutbound,
    UiNotify,
    UiDialog,
    UiPage,
    UiTray,
}

impl Permission {
    /// 检查此权限是否覆盖指定的 API 动作
    pub fn matches_action(&self, action: PermissionAction) -> bool {
        matches!(
            (self, action),
            (Permission::ConfigRead, PermissionAction::ConfigRead)
                | (Permission::ConfigWrite, PermissionAction::ConfigWrite)
                | (Permission::ProxyTest, PermissionAction::ProxyTest)
                | (Permission::ProxySelect, PermissionAction::ProxySelect)
                | (Permission::StoreReadWrite, PermissionAction::StoreRead)
                | (Permission::StoreReadWrite, PermissionAction::StoreWrite)
                | (
                    Permission::NetworkOutbound,
                    PermissionAction::NetworkOutbound
                )
                | (Permission::UiNotify, PermissionAction::UiNotify)
                | (Permission::UiDialog, PermissionAction::UiDialog)
                | (Permission::UiPage, PermissionAction::UiPage)
                | (Permission::UiTray, PermissionAction::UiTray)
        )
    }
}

/// 检查给定权限集合是否允许指定的 API 动作
pub fn is_permitted(permissions: &[Permission], action: PermissionAction) -> bool {
    permissions.iter().any(|p| p.matches_action(action))
}
