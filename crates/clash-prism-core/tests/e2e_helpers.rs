// 集成测试 — 共享辅助函数和 use 语句
//
// 此文件由 e2e_tests.rs 和 tests/integration/mod.rs 共同 include，
// 避免重复定义。
// 注意：rust-analyzer 会对此文件报告 unused 警告（误报），
// 因为 RA 将其视为独立模块分析，看不到 include! 它的上下文。
// 这些警告不影响实际编译和测试。

#[allow(unused_imports)]
use clash_prism_core::compiler::PatchCompiler;
#[allow(unused_imports, dead_code)]
use clash_prism_core::executor::PatchExecutor;
use clash_prism_core::ir::{Patch, PatchOp};
use clash_prism_core::scope::Scope;
use clash_prism_core::source::{PatchSource, SourceKind};
#[allow(unused_imports)]
use clash_prism_core::validator::Validator;
#[allow(unused_imports)]
use clash_prism_dsl::DslParser;

/// Fixture 文件目录（使用 CARGO_MANIFEST_DIR 环境变量确保路径正确）
#[allow(dead_code)]
fn fixture_dir() -> String {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| format!("{}/tests/fixtures", dir))
        .unwrap_or_else(|_| "tests/fixtures".to_string())
}

/// Helper: create a simple Patch for testing
#[allow(dead_code)]
fn make_patch(path: &str, op: PatchOp, value: serde_json::Value, scope: Scope) -> Patch {
    Patch::new(
        PatchSource {
            kind: SourceKind::YamlFile,
            file: Some("test.yaml".to_string()),
            line: None,
            plugin_id: None,
        },
        scope,
        path.to_string(),
        op,
        value,
    )
}
