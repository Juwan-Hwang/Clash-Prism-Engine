//! DSL 操作定义和执行顺序
//!
//! 固定执行顺序（与 `clash_prism_core::ir::execution_order()` 完全一致）：
//!
//! ```text
//! $filter -> $remove -> $transform -> $default -> $prepend -> $append -> DeepMerge -> Override
//! ```
//!
//! 分层语义：
//! - 数组流水线：$filter → $remove → $transform → $prepend → $append
//! - 对象兜底：  $default（在插入之前执行，确保后续操作有基础可操作）
//! - 对象合并：  DeepMerge（无标签默认行为）
//! - 独占操作：  $override（不可混用，不参与排序）
//!
//! $default 在 $prepend/$append 之前执行的原因：
//! $default 的本质是"打底"——为缺失字段注入兜底值。如果 $prepend 先执行，
//! 它会创建字段（即使是空数组），导致 $default 永远不触发（变成"死操作"）。

use clash_prism_core::ir::PatchOp;

/// Get the execution priority for a DSL operation (lower value = executes earlier).
///
/// Delegates to `clash_prism_core::ir::execution_order()` — the single source of truth.
pub fn op_priority(op: &PatchOp) -> u8 {
    clash_prism_core::ir::execution_order(op)
}

/// 按固定执行顺序排序操作列表
pub fn sort_ops_by_execution_order(ops: &mut [(String, PatchOp, serde_json::Value)]) {
    ops.sort_by_key(|(_, op, _)| op_priority(op));
}
