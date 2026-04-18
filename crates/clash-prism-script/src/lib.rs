//! # Prism Script — rquickjs 脚本引擎
//!
//! 基于 rquickjs（纯 Rust，零 C 依赖）的脚本运行时，
//! 提供 ES2023+ 完整支持的安全沙箱环境。
//!
//! ## 能力
//!
//! - 结构化 API（proxies / rules / groups 工具）— §5.2 PrismContext
//! - Patch 生成（高级条件化配置变换）
//! - KV 存储（跨脚本持久化）
//! - 日志输出
//! - 环境信息只读访问
//!
//! ## 安全限制
//!
//! - 最大执行时间：5 秒
//! - 最大内存：50MB
//! - 最大输出大小：1MB
//! - 最大日志条数：500

pub mod api;
pub mod limits;
pub mod runtime;
pub mod sandbox;

pub use api::{KvStore, PatchCollector, ScriptContext};
pub use runtime::{ScriptResult, ScriptRuntime};
pub use sandbox::SandboxConfig;
