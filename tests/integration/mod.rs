//! 集成测试 — 端到端流程验证
//!
//! 测试 Prism Engine 的完整工作流：
//! 1. 解析 .prism.yaml fixture 文件
//! 2. 编译为 Patch IR
//! 3. 执行 Patch 生成最终配置
//! 4. 校验结果正确性
//!
//! 所有 fixture 文件位于 `tests/fixtures/` 目录。

mod smart_selector;

// NOTE: 使用 `include!` 而非 `mod` 声明引用共享测试辅助代码。
//
// 权衡说明：
// - `include!` 方案：将 `e2e_helpers.rs` 的内容文本替换到此处，实现代码复用。
//   优点：跨 crate 共享测试代码无需额外模块结构；缺点：rust-analyzer 无法正确
//   分析被 include 的文件，会产生误报的 unused 警告。
// - `mod` 声明方案：需要将共享代码提取为独立 crate 或使用 `#[path]` 属性，
//   但 `fixture_dir()` 依赖 `CARGO_MANIFEST_DIR` 环境变量，在不同 crate 上下文中
//   会解析到不同的路径，因此直接 `mod` 引用会导致 fixture 路径错误。
//
// 当前选择 `include!` 是因为这是跨 crate 测试代码共享的最简单可靠方案，
// 且 `fixture_dir()` 中的 `CARGO_MANIFEST_DIR` 在 include 后仍指向当前 crate。
// 如果未来需要更完善的模块化，可考虑将共享辅助代码提取为 `test-utils` crate。
include!("../../crates/clash-prism-core/tests/e2e_helpers.rs");
