//! # Host Bridge — PrismHost trait 定义
//!
//! 任何 Mihomo GUI 只需实现 [`PrismHost`] trait，即可接入 Prism Engine。
//! GUI 侧实现量约 80-120 行 Rust 代码。
//!
//! ## 概述
//!
//! 本模块定义了 GUI 客户端与 Prism Engine 之间的桥接接口。[`PrismHost`] trait
//! 是整个 Extension 层的核心抽象，GUI 只需实现 4 个必须方法即可完成接入。
//!
//! ## 必须实现的方法
//!
//! | 方法 | 说明 |
//! |------|------|
//! | [`read_running_config`](PrismHost::read_running_config) | 读取当前运行中的配置（YAML 字符串） |
//! | [`apply_config`](PrismHost::apply_config) | 将处理后的配置写回并触发热重载 |
//! | [`get_prism_workspace`](PrismHost::get_prism_workspace) | 获取 Prism 工作目录路径 |
//! | [`notify`](PrismHost::notify) | 向前端发送事件通知 |
//!
//! ## 可选方法
//!
//! | 方法 | 默认行为 |
//! |------|---------|
//! | [`read_raw_profile`](PrismHost::read_raw_profile) | 返回 `Err("not implemented")` |
//! | [`list_profiles`](PrismHost::list_profiles) | 返回 `Err("not implemented")` |
//! | [`get_core_info`](PrismHost::get_core_info) | 返回 `Err("not implemented")` |
//! | [`validate_config`](PrismHost::validate_config) | 返回 `Err("not implemented")` |
//! | [`script_count`](PrismHost::script_count) | 返回 `Ok(0)` |
//! | [`plugin_count`](PrismHost::plugin_count) | 返回 `Ok(0)` |
//! | [`get_current_profile`](PrismHost::get_current_profile) | 返回 `None`（`__when__.profile` 条件匹配依赖此方法） |
//! | [`get_variables`](PrismHost::get_variables) | 返回空映射（`{{var}}` 模板替换依赖此方法） |

use std::path::PathBuf;

/// 配置应用结果
///
/// 表示一次配置写回操作的结果，包含文件保存状态、热重载状态和状态消息。
/// 由 [`PrismHost::apply_config`] 返回。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyStatus {
    /// 配置文件是否已成功保存到磁盘
    pub files_saved: bool,

    /// 热重载是否成功（mihomo 是否已加载新配置）
    pub hot_reload_success: bool,

    /// 状态消息（用于日志/调试，包含操作描述或错误详情）
    pub message: String,

    /// 是否因配置变更触发了核心重启
    ///
    /// 某些配置变更（如 `external-controller` 端口修改）无法通过热重载生效，
    /// 需要重启 mihomo 核心。此字段标记本次操作是否触发了重启。
    /// 默认为 `false`。
    #[serde(default)]
    pub restarted: bool,
}

/// 前端事件通知
///
/// Prism Engine 通过此枚举向 GUI 前端发送各类事件通知，
/// 包括 Patch 执行结果、配置重载状态、规则变更统计和文件监听事件。
///
/// 使用 `#[serde(tag = "type")]` 进行带标签的序列化，
/// 前端可根据 `type` 字段分发处理不同事件。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum PrismEvent {
    /// Patch 执行成功
    ///
    /// 包含 patch 标识和执行统计信息。
    PatchApplied {
        /// Patch 标识（UUID 或自定义标识符）
        patch_id: String,
        /// Patch 执行统计（新增/删除/修改数量和耗时）
        stats: PatchStats,
    },

    /// Patch 执行失败
    ///
    /// 包含 patch 标识和错误描述。
    PatchFailed {
        /// Patch 标识（UUID 或自定义标识符）
        patch_id: String,
        /// 错误描述信息
        error: String,
    },

    /// 配置热重载完成
    ///
    /// 在配置写回后触发，表示 mihomo 已尝试加载新配置。
    ConfigReloaded {
        /// 热重载是否成功
        success: bool,
        /// 状态消息（包含重载结果描述）
        message: String,
    },

    /// 规则变更通知
    ///
    /// 在 Patch 执行后触发，汇总本次编译中规则的变更统计。
    RulesChanged {
        /// 新增的规则数量
        added: usize,
        /// 删除的规则数量
        removed: usize,
        /// 修改的规则数量
        modified: usize,
    },

    /// 文件监听事件
    ///
    /// 当工作目录中的文件发生变化时触发。
    WatcherEvent {
        /// 发生变化的文件路径（相对于工作目录）
        file: String,
        /// 变更类型（如 "created"、"modified"、"removed"）
        change_type: String,
    },

    /// 监听器状态变更
    ///
    /// 当文件监听器启动或停止时触发。
    WatcherStatus {
        /// 监听器是否正在运行
        running: bool,
        /// 当前监听的文件/目录数量
        watching_count: usize,
    },
}

/// Patch 执行统计
///
/// 记录单个 Patch 执行过程中的变更数量和耗时。
/// 作为 [`PrismEvent::PatchApplied`] 事件的 payload 使用。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PatchStats {
    /// 新增的元素数量
    pub added: usize,

    /// 删除的元素数量
    pub removed: usize,

    /// 修改的元素数量
    pub modified: usize,

    /// 执行耗时（微秒）
    pub duration_us: u64,
}

/// Profile 信息
///
/// 描述一个 mihomo 配置档案（Profile）的元数据。
/// 通过 [`PrismHost::list_profiles`] 获取所有 Profile 列表。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProfileInfo {
    /// Profile 唯一标识符
    pub id: String,

    /// Profile 显示名称
    pub name: String,

    /// Profile 类型：`"remote"` | `"local"` | `"script"` | `"merge"`
    pub profile_type: String,

    /// 是否为当前正在使用的 Profile
    pub is_current: bool,
}

/// 核心信息
///
/// 描述 mihomo 核心的运行状态和连接参数。
/// 通过 [`PrismHost::get_core_info`] 获取。
///
/// 注意：`Debug` 实现中 `api_secret` 会被脱敏显示为 `"[REDACTED]"`，
/// 防止密钥通过日志意外泄露。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CoreInfo {
    /// mihomo 核心版本号（如 "1.18.0"）
    pub version: String,

    /// RESTful API 监听端口（如 9090）
    pub api_port: u16,

    /// RESTful API 认证密钥
    ///
    /// **安全注意**：
    /// - JSON 序列化时会被跳过（`#[serde(skip_serializing)]`），
    ///   防止密钥通过 API 响应意外泄露给前端。反序列化不受影响，可从配置中正常读取。
    /// - `Debug` 实现中会被脱敏显示为 `"[REDACTED]"`，防止密钥通过日志意外泄露。
    ///
    /// **数据流向限制**：
    /// 此字段仅用于内部逻辑（如构造 mihomo API 请求头），**禁止**通过以下途径传出：
    /// - HTTP API 响应体（已被 `skip_serializing` 拦截）
    /// - 日志输出（已被自定义 `Debug` 拦截）
    /// - 前端 IPC 通知（实现方应确保不将此字段传递给前端）
    ///
    /// 实现方在自定义序列化或日志记录时，也应对此字段做脱敏处理。
    #[serde(skip_serializing)]
    pub api_secret: String,

    /// 核心是否正在运行
    pub is_running: bool,
}

impl std::fmt::Debug for CoreInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreInfo")
            .field("version", &self.version)
            .field("api_port", &self.api_port)
            .field("api_secret", &"[REDACTED]")
            .field("is_running", &self.is_running)
            .finish()
    }
}

/// 通用 Extension 宿主接口
///
/// 任何 Mihomo GUI 只需实现这个 trait，即可接入 Prism Engine。
/// Tauri 2 + Rust 的 GUI（Zephyr、Clash Verge Rev、Clash Nyanpasu）直接实现；
/// Electron + TypeScript 的 GUI（mihomo-party）可通过 napi-rs 或 CLI 方式桥接。
///
/// # 必须实现的方法
///
/// - [`read_running_config`](PrismHost::read_running_config) — 读取当前运行配置
/// - [`apply_config`](PrismHost::apply_config) — 写回配置并触发热重载
/// - [`get_prism_workspace`](PrismHost::get_prism_workspace) — 获取工作目录
/// - [`notify`](PrismHost::notify) — 发送前端事件通知
///
/// # 线程安全
///
/// 实现 `Send + Sync`，确保可以在多线程环境中安全使用。
///
/// # 示例
///
/// ```rust,ignore
/// use clash_prism_extension::host::{PrismHost, ApplyStatus};
/// use std::path::PathBuf;
///
/// struct MyGuiHost;
///
/// impl PrismHost for MyGuiHost {
///     fn read_running_config(&self) -> Result<String, String> {
///         // 从 GUI 内部状态读取当前配置
///         Ok("rules:\n  - MATCH,DIRECT".to_string())
///     }
///
///     fn apply_config(&self, config: &str) -> Result<ApplyStatus, String> {
///         // 将配置写回 GUI 并触发热重载
///         Ok(ApplyStatus {
///             files_saved: true,
///             hot_reload_success: true,
///             message: "配置已更新".to_string(),
///             restarted: false,
///         })
///     }
///
///     fn get_prism_workspace(&self) -> Result<PathBuf, String> {
///         Ok(PathBuf::from("/path/to/prism-workspace"))
///     }
///
///     fn notify(&self, event: clash_prism_extension::PrismEvent) {
///         // 通过 IPC 发送事件到前端
///         println!("Event: {:?}", event);
///     }
/// }
/// ```
pub trait PrismHost: Send + Sync {
    // ── 必须实现 (4 个) ──

    /// 读取当前运行中的配置
    ///
    /// 返回 mihomo 当前正在使用的完整配置（YAML 字符串）。
    /// GUI 应从其内部状态或配置文件中读取。
    ///
    /// # 错误
    ///
    /// 当配置不可读或不存在时返回错误字符串。
    fn read_running_config(&self) -> Result<String, String>;

    /// 将处理后的配置写回并触发热重载
    ///
    /// 接收 Prism Engine 编译后的配置（YAML 字符串），
    /// GUI 应将其写入配置文件并通知 mihomo 进行热重载。
    ///
    /// # 参数
    ///
    /// - `config` — 编译后的完整配置（YAML 字符串）
    ///
    /// # 错误
    ///
    /// 当文件写入失败或热重载触发失败时返回错误字符串。
    fn apply_config(&self, config: &str) -> Result<ApplyStatus, String>;

    /// 获取 Prism 工作目录
    ///
    /// 返回用于存放 `.prism.yaml` 文件、脚本、插件等资源的目录路径。
    /// Prism Engine 会扫描此目录下的所有 `.prism.yaml` / `.prism.yml` 文件。
    ///
    /// # 错误
    ///
    /// 当目录不存在或无法访问时返回错误字符串。
    fn get_prism_workspace(&self) -> Result<PathBuf, String>;

    /// 向前端发送事件通知
    ///
    /// GUI 应将事件通过 IPC（Tauri IPC / Electron IPC）转发到前端。
    /// 前端可根据事件类型更新 UI 状态。
    ///
    /// # 参数
    ///
    /// - `event` — 要发送的事件（参见 [`PrismEvent`] 各变体）
    fn notify(&self, event: PrismEvent);

    // ── 可选实现 (8 个，有默认实现) ──

    /// 读取指定 Profile 的原始 YAML
    ///
    /// 返回指定 Profile 的原始配置内容（YAML 字符串）。
    /// 默认返回 `Err("not implemented")`。
    ///
    /// # 参数
    ///
    /// - `profile_id` — Profile 的唯一标识符
    fn read_raw_profile(&self, _profile_id: &str) -> Result<String, String> {
        Err("not implemented".into())
    }

    /// 列出所有 Profile
    ///
    /// 返回所有可用 Profile 的元信息列表。
    /// 默认返回 `Err("not implemented")`。
    fn list_profiles(&self) -> Result<Vec<ProfileInfo>, String> {
        Err("not implemented".into())
    }

    /// 获取核心信息
    ///
    /// 返回 mihomo 核心的版本、端口、密钥和运行状态。
    /// 默认返回 `Err("not implemented")`。
    fn get_core_info(&self) -> Result<CoreInfo, String> {
        Err("not implemented".into())
    }

    /// 验证配置文件
    ///
    /// 使用 `mihomo -t` 验证配置文件是否合法。
    /// 默认返回 `Err("not implemented")`。
    ///
    /// # 参数
    ///
    /// - `config` — 待验证的配置（YAML 字符串）
    ///
    /// # 返回
    ///
    /// - `Ok(true)` — 配置合法
    /// - `Ok(false)` — 配置非法
    /// - `Err(..)` — 验证过程出错（如 mihomo 未安装）
    fn validate_config(&self, _config: &str) -> Result<bool, String> {
        Err("not implemented".into())
    }

    /// 获取已注册的脚本数量（由 Host 实现，默认返回 0）
    fn script_count(&self) -> Result<usize, String> {
        Ok(0)
    }

    /// 获取已注册的插件数量（由 Host 实现，默认返回 0）
    fn plugin_count(&self) -> Result<usize, String> {
        Ok(0)
    }

    /// 获取当前激活的 Profile 名称
    ///
    /// 编译管道通过此方法获取当前激活的订阅/Profile 名称，
    /// 用于 `__when__.profile` 条件匹配。当 Patch 声明了 profile 条件时，
    /// 编译器会将此返回值与 Patch 的 profile 进行比较，决定是否执行。
    ///
    /// 默认返回 `None`，此时所有带 `__when__.profile` 条件的 Patch 都会被跳过。
    ///
    /// # 值格式契约
    ///
    /// **此方法的返回值必须与 DSL 文件中 `__when__.profile` 的写法一致。**
    ///
    /// 引擎使用精确匹配（支持正则 `/pattern/` 和通配符 `*?`），
    /// 不会对返回值做任何 normalize。如果返回值与 DSL 中的值不一致，
    /// 匹配将失败，Patch 会被跳过。
    ///
    /// ```yaml
    /// # DSL 中的写法
    /// __when__:
    ///   profile: "我的订阅"        # ← get_current_profile() 应返回 "我的订阅"
    ///   profile: "/work-.*/"       # ← get_current_profile() 返回 "work-prod" 即可匹配
    ///   profile: "我的订阅.yaml"   # ← get_current_profile() 应返回 "我的订阅.yaml"
    /// ```
    ///
    /// 如果 GUI 侧的 profile 标识包含 `.yaml` 后缀但 DSL 中不写后缀，
    /// 实现方应在此方法中 strip 后缀（或反过来），确保两端一致。
    fn get_current_profile(&self) -> Option<String> {
        None
    }

    /// 获取变量模板映射
    ///
    /// 返回键值对，用于 `.prism.yaml` 中 `{{var_name}}` 模板替换。
    /// 此方法的返回值优先级最高，会覆盖 DSL 文件中 `__vars__` 声明的同名变量。
    ///
    /// 默认返回空映射。
    fn get_variables(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }
}
