//! # Prism DSL 解析器
//!
//! 将 `.prism.yaml` 文件解析为 Patch IR。
//!
//! ## 设计原则
//!
//! - 操作符以 `$` 开头，作为标准 YAML 映射键名使用
//! - 使用 `serde_yml` 的 `Value` 解析后提取 `$` 前缀键（无需自定义 YAML 解析层）
//! - 同一键下的多个操作按**固定执行顺序**执行，不依赖 YAML 键的书写顺序
//! - 一个文件只能有一个 `__when__` 声明

pub mod ops;
pub mod parser;
pub mod schema;

pub use parser::DslParser;
