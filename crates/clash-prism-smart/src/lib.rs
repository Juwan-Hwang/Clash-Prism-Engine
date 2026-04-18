//! # Smart Selector — 独立运行时模块
//!
//! 与配置增强管线**完全解耦**：
//!
//! - 配置增强运行在**配置生成阶段**（静态数据）
//! - Smart Selector 运行在**内核运行阶段**（动态数据）
//!
//! ## 核心能力
//!
//! - EMA 评分算法
//! - P90 延迟计算
//! - 时间衰减权重
//! - 自适应测速调度
//!
//! ## 依赖
//!
//! - `clash-prism-core` — 共享基础类型（error、ir 等）
//! - `serde` / `serde_json` — 序列化
//! - `chrono` — 时间处理（EMA 衰减计算）
//! - `thiserror` — 错误派生
//! - `tracing` — 结构化日志

pub mod config;
pub mod history;
pub mod scheduler;
pub mod scorer;

pub use config::SmartConfig;
pub use scheduler::AdaptiveScheduler;
pub use scorer::SmartScorer;
