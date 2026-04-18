//! # Prism Plugin — 插件系统
//!
//! 两类插件（最小权限原则）：
//!
//! ## Config Plugin（配置插件）
//!
//! - 运行环境：后端 rquickjs 沙箱
//! - 能力：读写配置、注册生命周期钩子、使用 KV 存储
//! - 不能：访问文件系统、直接访问网络、访问前端 DOM
//! - 用途：配置变换、节点重命名、规则注入、智能分组
//!
//! ## UI Extension（界面扩展）
//!
//! - 运行环境：前端 iframe（受控沙箱）
//! - 能力：注册自定义页面/按钮/通知，通过受限 IPC 与后端通信
//! - 不能：直接读写配置、访问文件系统
//! - 用途：自定义设置面板、快捷操作按钮、状态仪表盘

pub mod components;
pub mod cron_scheduler;
pub mod failover;
pub mod hook;
pub mod hook_result;
pub mod lifecycle;
pub mod loader;
pub mod manifest;
pub mod permission;

pub use cron_scheduler::CronScheduler;
pub use failover::{CooldownState, FallbackTarget, NodeFailPolicy};
pub use hook::{Hook, HookScheduler, ScheduledHook};
pub use loader::PluginLoader;
pub use manifest::{PluginManifest, PluginType};
pub use permission::Permission;
