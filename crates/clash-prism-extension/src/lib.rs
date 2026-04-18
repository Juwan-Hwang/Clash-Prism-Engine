//! # clash-prism-extension — UI Extension 通用接口
//!
//! 三层架构中的 Layer 2（Host Bridge），提供 GUI 客户端接入 Prism Engine 的统一接口。
//!
//! ## 概述
//!
//! `clash-prism-extension` 是 Prism Engine 的 GUI 集成层。任何 Mihomo GUI 客户端
//! 只需实现 [`PrismHost`] trait（约 80-120 行 Rust 代码），即可获得完整的
//! Prism 配置编译、规则管理、文件监听等能力。
//!
//! ## 架构
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────┐
//! │ Layer 3: Frontend JSON API (Tauri IPC / Electron IPC / HTTP) │
//! │ ← React/Vue/Svelte 前端调用                                  │
//! ├───────────────────────────────────────────────────────────────┤
//! │ Layer 2: Host Bridge (Rust trait PrismHost)  ← 本 crate      │
//! │ ← GUI 实现适配层，桥接 Prism 和 GUI 内部 API                   │
//! ├───────────────────────────────────────────────────────────────┤
//! │ Layer 1: Prism Core Engine (clash-prism-core / clash-prism-dsl / ...)    │
//! │ ← 纯 Rust 库，无 GUI 依赖                                     │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## 核心类型
//!
//! | 类型 | 说明 |
//! |------|------|
//! | [`PrismHost`] | GUI 宿主接口 trait（GUI 必须实现） |
//! | [`PrismExtension`] | Extension 主入口（提供高层 API） |
//! | [`ApplyOptions`] | 编译选项 |
//! | [`ApplyResult`] | 编译结果 |
//! | [`PrismStatus`] | Extension 运行状态 |
//! | [`RuleGroup`] | 规则分组（供 GUI 展示和管理） |
//! | [`RuleAnnotation`] | 规则注解（标记规则归属） |
//!
//! ## 模块结构
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`host`] | [`PrismHost`] trait 和宿主相关类型 |
//! | [`extension`] | [`PrismExtension`] 主入口结构 |
//! | [`types`] | API 数据结构定义 |
//! | [`annotation`] | 规则注解提取和分组 |
//!
//! ## 快速开始
//!
//! ```rust,ignore
//! use clash_prism_extension::{PrismHost, PrismExtension, ApplyOptions};
//!
//! struct MyGuiHost { /* ... */ }
//!
//! impl PrismHost for MyGuiHost {
//!     fn read_running_config(&self) -> Result<String, String> { /* ... */ }
//!     fn apply_config(&self, config: &str) -> Result<ApplyStatus, String> { /* ... */ }
//!     fn get_prism_workspace(&self) -> Result<std::path::PathBuf, String> { /* ... */ }
//!     fn notify(&self, event: PrismEvent) { /* ... */ }
//! }
//!
//! let ext = PrismExtension::new(host);
//! let result = ext.apply(ApplyOptions::default())?;
//! ```
//!
//! ## 主要 API
//!
//! ```rust,ignore
//! // 执行编译
//! let result = ext.apply(ApplyOptions::default())?;
//!
//! // 查看运行状态
//! let status = ext.status();
//!
//! // 列出规则组
//! let groups = ext.list_rules()?;
//!
//! // 启用/禁用规则组
//! ext.toggle_group("ad-filter.prism.yaml", false)?;
//!
//! // 判断规则归属
//! let info = ext.is_prism_rule(5)?;
//! ```
//!
//! ## Feature Flags
//!
//! | Feature | 说明 |
//! |---------|------|
//! | `watcher` | 启用文件监听功能（依赖 `notify` crate） |

mod annotation;
mod extension;
mod host;
mod types;

pub use extension::{IsPrismRule, PrismExtension};
pub use host::{ApplyStatus, CoreInfo, PatchStats, PrismEvent, PrismHost, ProfileInfo};
pub use types::{
    ApplyOptions, ApplyResult, CompileStats, PrismStatus, RuleAnnotation, RuleDiff, RuleGroup,
    RuleInsertPosition, TraceView,
};
