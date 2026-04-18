//! # Execution Tracing (Explain View & Diff View Data Foundation)
//!
//! ## Design Decision (§3.2)
//!
//! **Store only diffs, not full snapshots.** With Patch IR, Explain View can
//! replay to any step on demand — no need to pre-compute full snapshots.
//!
//! ## Capabilities
//!
//! - **TraceManager**: Manages traces, provides explain/replay/diff API
//! - **ExecutionTrace**: Per-patch execution record (lightweight, no full snapshot)
//! - **TraceSummary**: Aggregate statistics per operation
//! - **AffectedItem**: Individual element change records (added/removed/modified)
//! - **ExplainEntry**: Source query result for "Why is this rule here?" (§3.2)

use serde::Serialize;

use crate::ir::{Patch, PatchId, PatchOp};
use crate::json_path::get_json_path_mut;
use crate::source::PatchSource;

/// Execution trace manager — provides Explain View and Replay functionality.
///
/// Manages a list of [`ExecutionTrace`] records and their corresponding [`Patch`] objects,
/// enabling source tracing, step replay, and diff reporting.
pub struct TraceManager {
    /// All execution trace records
    pub traces: Vec<ExecutionTrace>,
    /// Corresponding Patches (for replay)
    patches: Vec<Patch>,
    /// PatchId → index in patches vec (for O(1) lookup)
    /// Uses String key (PatchId.as_str()) instead of PatchId directly to avoid
    /// requiring PatchId: Hash on the HashMap key, since PatchId is already
    /// stored in the patches vec and the String form is sufficient for lookup.
    patch_index: std::collections::HashMap<String, usize>,
}

impl TraceManager {
    /// Create a new empty trace manager.
    pub fn new() -> Self {
        Self {
            traces: vec![],
            patches: vec![],
            patch_index: std::collections::HashMap::new(),
        }
    }

    /// Push a trace record and its corresponding Patch.
    pub fn push(&mut self, trace: ExecutionTrace, patch: Patch) {
        let idx = self.patches.len();
        self.patch_index.insert(patch.id.as_str().to_string(), idx);
        self.patches.push(patch);
        self.traces.push(trace);
    }

    /// Bulk import from Executor's traces + patches.
    ///
    /// Returns `Err` if `traces` and `patches` have different lengths,
    /// which would cause out-of-bounds access or incorrect trace-to-patch mapping.
    pub fn import(
        &mut self,
        traces: Vec<ExecutionTrace>,
        patches: Vec<Patch>,
    ) -> std::result::Result<(), crate::error::PrismError> {
        if traces.len() != patches.len() {
            return Err(crate::error::PrismError::PatchExecutionFailed {
                patch_id: "TraceManager::import".to_string(),
                reason: format!(
                    "traces 与 patches 长度不一致: traces={} patches={}",
                    traces.len(),
                    patches.len()
                ),
            });
        }
        self.patch_index = patches
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.as_str().to_string(), i))
            .collect();
        self.traces = traces;
        self.patches = patches;
        Ok(())
    }

    /// Source traceability query: "Why is this rule here?" (§3.2).
    ///
    /// Iterates all traces to find changes affecting the given path,
    /// returning time-sorted explain entries.
    pub fn explain_field(&self, field_path: &str, item_key: Option<&str>) -> Vec<ExplainEntry> {
        self.traces
            .iter()
            .filter(|t| {
                // 只看条件匹配且成功的 trace
                t.condition_matched && t.source.file.is_some()
            })
            .filter(|t| {
                // 检查是否影响指定路径
                self.trace_affects_path(t, field_path)
            })
            .filter(|t| {
                // 如果指定了 item_key，进一步过滤 affected_items
                // false positives (e.g., searching for "dns" should not match "dns-nameserver")
                if let Some(key) = item_key {
                    t.affected_items.iter().any(|item| {
                        // searching "dns" matches "dns" and "dns.nameserver" but not "xdns"
                        let before_prefix = item
                            .before
                            .as_ref()
                            .is_some_and(|b| b == key || b.starts_with(&format!("{}.", key)));
                        let after_prefix = item
                            .after
                            .as_ref()
                            .is_some_and(|a| a == key || a.starts_with(&format!("{}.", key)));
                        before_prefix || after_prefix
                    })
                } else {
                    true
                }
            })
            .map(|t| ExplainEntry {
                source: t.source.clone(),
                op_name: t.op.display_name().to_string(),
                detail: t.describe_change(),
            })
            .collect()
    }

    /// Replay to a specific execution step (§3.2).
    ///
    /// Starting from `base_config`, applies the first `target_step+1` Patches,
    /// returning the full config snapshot at that moment.
    /// Does NOT pre-store full snapshots per trace; computes on-demand when user views.
    ///
    /// Condition checking is performed during replay to ensure consistency with
    /// the original execution (patches whose conditions don't match are skipped).
    pub fn replay_at_step(
        &self,
        target_step: usize,
        base_config: &serde_json::Value,
        context: &crate::executor::ExecutionContext,
    ) -> Option<serde_json::Value> {
        if self.patches.is_empty() || target_step >= self.patches.len() {
            return None;
        }

        let mut config = base_config.clone();
        for patch in &self.patches[..=target_step.min(self.patches.len() - 1)] {
            // Check scope-level condition
            if !check_patch_condition(patch, context) {
                continue;
            }
            // Check patch-level condition
            if let Some(pred) = &patch.condition
                && !crate::executor::evaluate_predicate(&pred.expr, &config).unwrap_or(false)
            {
                continue;
            }
            apply_patch_simple(&mut config, patch);
        }
        Some(config)
    }

    /// Get complete Diff View text report.
    pub fn diff_view_report(&self) -> String {
        if self.traces.is_empty() {
            return "(无执行记录)".to_string();
        }

        let mut report = String::new();
        for (i, trace) in self.traces.iter().enumerate() {
            let source_desc = trace.source.short_description();
            report.push_str(&format!(
                "  [{}] {} — {}\n",
                i + 1,
                source_desc,
                trace.describe_change()
            ));

            // 显示受影响的元素
            if !trace.affected_items.is_empty() {
                for item in &trace.affected_items {
                    match &item.action {
                        crate::trace::TraceAction::Added => {
                            report.push_str(&format!(
                                "      + {} (新增)\n",
                                item.after.as_deref().unwrap_or("?")
                            ));
                        }
                        crate::trace::TraceAction::Removed => {
                            report.push_str(&format!(
                                "      - {} (删除)\n",
                                item.before.as_deref().unwrap_or("?")
                            ));
                        }
                        crate::trace::TraceAction::Modified => {
                            report.push_str(&format!(
                                "      ~ {} → {}\n",
                                item.before.as_deref().unwrap_or("?"),
                                item.after.as_deref().unwrap_or("?")
                            ));
                        }
                    }
                }
            }

            // 显示摘要
            let s = &trace.summary;
            if s.added > 0 || s.removed > 0 || s.modified > 0 {
                report.push_str(&format!(
                    "      摘要: +{} -{} ~{} [{} unchanged]\n",
                    s.added, s.removed, s.modified, s.kept
                ));
            }
        }

        report
    }

    /// 判断某个 trace 是否影响指定路径
    ///
    /// 使用路径段级别匹配（按 `.` 分割后逐段比较），避免字符串前缀误报。
    /// 例如 "dns.nameserver" 不会误匹配 "dns.name"。
    fn trace_affects_path(&self, trace: &ExecutionTrace, path: &str) -> bool {
        // 通过 HashMap 索引查找对应 patch（O(1) 查找）
        let trace_idx = trace.patch_id.as_str();
        if let Some(&patch_idx) = self.patch_index.get(trace_idx) {
            let patch = &self.patches[patch_idx];
            // 精确匹配
            if patch.path == path {
                return true;
            }
            // 子路径匹配：按 '.' 分割后逐段比较
            let patch_segments: Vec<&str> = patch.path.split('.').collect();
            let query_segments: Vec<&str> = path.split('.').collect();
            // patch.path 是 path 的祖先（如 "dns" 影响 "dns.nameserver"）
            if patch_segments.len() < query_segments.len()
                && query_segments[..patch_segments.len()] == *patch_segments
            {
                return true;
            }
            // path 是 patch.path 的祖先（如 "dns.nameserver" 影响 "dns"）
            if query_segments.len() < patch_segments.len()
                && patch_segments[..query_segments.len()] == *query_segments
            {
                return true;
            }
        }
        false
    }
}

impl Default for TraceManager {
    fn default() -> Self {
        Self::new()
    }
}

// ══════════════════════════════════════════════════════════
// §10 调试系统 — 统计与过滤增强
// ══════════════════════════════════════════════════════════

/// Aggregated statistics for execution traces (§10 Diff View summary data).
#[derive(Debug, Clone, Serialize)]
pub struct TraceStatistics {
    /// Total number of Patches
    pub total_patches: usize,
    /// Number of successfully executed Patches
    pub succeeded: usize,
    /// Number of Patches skipped (condition not matched)
    pub skipped: usize,
    /// Total elements added across all operations
    pub total_added: usize,
    /// Total elements removed across all operations
    pub total_removed: usize,
    /// Total elements modified across all operations
    pub total_modified: usize,
    /// Total execution time in microseconds
    pub total_duration_us: u64,
    /// Average execution time per patch in microseconds
    pub avg_duration_us: u64,
}

impl TraceManager {
    /// 生成聚合统计（§10 Diff View 摘要数据）
    ///
    /// 返回所有 trace 的汇总统计数据，用于调试面板顶部展示。
    pub fn statistics(&self) -> TraceStatistics {
        let total = self.traces.len();
        let mut stats = TraceStatistics {
            total_patches: total,
            succeeded: 0,
            skipped: 0,
            total_added: 0,
            total_removed: 0,
            total_modified: 0,
            total_duration_us: 0,
            avg_duration_us: 0,
        };

        for t in &self.traces {
            if t.condition_matched {
                stats.succeeded += 1;
            } else {
                stats.skipped += 1;
            }
            stats.total_added += t.summary.added;
            stats.total_removed += t.summary.removed;
            stats.total_modified += t.summary.modified;
            stats.total_duration_us += t.duration_us;
        }

        if stats.succeeded > 0 {
            stats.avg_duration_us = stats.total_duration_us / stats.succeeded as u64;
        }

        stats
    }

    /// 按来源文件过滤 traces（§10 来源过滤）
    ///
    /// 返回来自指定文件的所有执行追踪记录。
    /// 支持精确匹配和后缀匹配（如 `"base-dns.yaml"` 匹配完整路径中的文件名）。
    pub fn filter_by_source_file(&self, file_pattern: &str) -> Vec<&ExecutionTrace> {
        self.traces
            .iter()
            .filter(|t| {
                t.source
                    .file
                    .as_ref()
                    .is_some_and(|f| f.contains(file_pattern))
            })
            .collect()
    }

    /// 按操作类型过滤 traces（§10 操作类型过滤）
    ///
    /// 返回指定操作类型的所有执行追踪记录。
    pub fn filter_by_op(&self, op: &PatchOp) -> Vec<&ExecutionTrace> {
        self.traces.iter().filter(|t| t.op == *op).collect()
    }

    /// 获取所有受影响的路径列表（去重，§10 影响范围概览）
    ///
    /// 返回被任何 Patch 修改过的配置路径列表，按首次出现顺序排列。
    pub fn affected_paths(&self) -> Vec<String> {
        let mut paths = std::collections::BTreeSet::new();
        for patch in &self.patches {
            paths.insert(patch.path.clone());
        }
        paths.into_iter().collect()
    }

    /// 获取完整的执行链路文本报告（§10 完整调试视图）
    ///
    /// 包含：统计摘要 + 每个 trace 的详情 + 受影响路径总览。
    pub fn full_report(&self) -> String {
        let mut report = String::new();

        // 1. 统计摘要
        let stats = self.statistics();
        report.push_str("╔══ Prism Engine 执行追踪报告 ══╗\n\n");
        report.push_str(&format!(
            "📊 总览: {} 个 Patch | ✅ 成功 {} | ⏭️ 跳过 {}\n",
            stats.total_patches, stats.succeeded, stats.skipped
        ));
        report.push_str(&format!(
            "📝 变更: +{} -{} ~{} | ⏱️ 总耗时 {}μs (均 {}μs/patch)\n\n",
            stats.total_added,
            stats.total_removed,
            stats.total_modified,
            stats.total_duration_us,
            stats.avg_duration_us
        ));

        // 2. 受影响路径
        let paths = self.affected_paths();
        if !paths.is_empty() {
            report.push_str("📂 影响范围:\n");
            for path in &paths {
                report.push_str(&format!("   • {}\n", path));
            }
            report.push('\n');
        }

        // 3. 详细 trace 列表
        report.push_str("─".repeat(50).as_str());
        report.push('\n');
        report.push_str(&self.diff_view_report());

        // 4. 品牌信息（Explain View 页脚）
        report.push_str("─".repeat(50).as_str());
        report.push('\n');
        report.push_str("Powered by Prism Engine — Apache 2.0\n");
        report.push_str("Copyright 2026 Juwan Hwang (黄治文)\n");

        report
    }
}

/// Check if a patch's condition is satisfied given the execution context.
/// Used by replay_at_step to ensure consistency with original execution.
///
/// Delegates to [`crate::executor::check_patch_condition`] — the single source of truth
/// for condition matching logic (ensures executor and trace always agree).
fn check_patch_condition(patch: &Patch, context: &crate::executor::ExecutionContext) -> bool {
    crate::executor::check_patch_condition(patch, context)
}

/// Simple Patch application function (for replay, no full Executor needed).
///
/// For composite patches (multiple ops on same key), iterates all sub-operations
/// via `all_ops()` and applies each in the executor's fixed execution order.
///
/// ## Why a simplified implementation instead of reusing the executor?
///
/// This replay function intentionally uses a simplified implementation rather than
/// delegating to `PatchExecutor::apply_patch_in_place` to avoid a **circular dependency**:
/// `trace` → `executor` → `trace`. The executor already depends on trace types
/// (e.g., `ExecutionTrace`), so trace cannot depend on the executor.
///
/// Key behavioral consistency with the executor is maintained by:
/// 1. Using the same public helper functions (`deep_merge_json`, `apply_override`, etc.)
/// 2. Performing the same guarded field check (`is_guarded_path`)
/// 3. Delegating condition checking to `check_patch_condition` (the single source of truth)
///
///
/// **IMPORTANT**: When the executor's behavior changes (e.g., new PatchOp variants,
/// modified operation semantics, or changed guarded field logic), this replay function
/// MUST be updated to stay in sync. Failure to do so will cause replay results to
/// diverge from actual execution results, leading to incorrect Explain View output.
/// Review this function whenever modifying `PatchExecutor::apply_patch_in_place`.
fn apply_patch_simple(config: &mut serde_json::Value, patch: &Patch) {
    if patch.is_composite() {
        // Composite: apply each sub-operation in fixed order.
        // all_ops() returns SubOps sorted by execution priority.
        let all_ops = patch.all_ops();
        for sub_op in &all_ops {
            apply_single_op(config, &patch.path, &sub_op.op, &sub_op.value);
        }
    } else {
        // Single operation: apply directly
        apply_single_op(config, &patch.path, &patch.op, &patch.value);
    }
}

/// Apply a single PatchOp to config at the given path with the given value.
///
/// This is the core replay logic, factored out so both single and composite
/// patches share the same implementation.
///
/// Delegates to executor's public helper functions to avoid
/// code duplication and ensure behavioral consistency between runtime execution
/// and trace replay.
///
/// Added guarded field check consistent with executor behavior.
/// Operations on guarded paths (except DeepMerge/Override) are skipped with a warning.
fn apply_single_op(
    config: &mut serde_json::Value,
    path: &str,
    op: &PatchOp,
    value: &serde_json::Value,
) {
    // Guarded field check — consistent with executor's apply_patch_in_place.
    // DeepMerge and Override have their own guarded checks inside the executor.
    // For other operations, skip and warn to match runtime behavior.
    let is_guarded = crate::executor::is_guarded_path(path);
    let is_deep_merge_or_override = matches!(op, PatchOp::DeepMerge | PatchOp::Override);
    if is_guarded && !is_deep_merge_or_override {
        tracing::warn!(
            path = path,
            op = ?op,
            "replay: skipping guarded field (consistent with executor)"
        );
        return;
    }

    match op {
        PatchOp::DeepMerge => {
            if let Some(target) = get_json_path_mut(config, path) {
                super::executor::deep_merge_json(target, value);
            }
        }
        PatchOp::Override => {
            super::executor::apply_override(config, path, value);
        }
        PatchOp::Prepend => {
            super::executor::apply_prepend(config, path, value);
        }
        PatchOp::Append => {
            super::executor::apply_append(config, path, value);
        }
        PatchOp::SetDefault => {
            super::executor::apply_set_default(config, path, value);
        }
        PatchOp::Filter { expr } => {
            if let Some(arr) = get_json_path_mut(config, path)
                && let Some(existing) = arr.as_array_mut()
            {
                let expr_str = &expr.expr;
                // matching executor's execute_filter_in_place behavior.
                existing.retain(|item| {
                    super::executor::evaluate_predicate(expr_str, item).unwrap_or(true)
                });
            }
        }
        PatchOp::Transform { expr } => {
            if let Some(arr) = get_json_path_mut(config, path)
                && let Some(existing) = arr.as_array_mut()
            {
                let expr_str = &expr.expr;
                // Apply transform expression to each element
                for item in existing.iter_mut() {
                    if let Ok(transformed) =
                        super::executor::evaluate_transform_expr(expr_str, item)
                    {
                        *item = transformed;
                    }
                }
            }
        }
        PatchOp::Remove { expr } => {
            if let Some(arr) = get_json_path_mut(config, path)
                && let Some(existing) = arr.as_array_mut()
            {
                let expr_str = &expr.expr;
                // Remove matching elements (semantically opposite to Filter)
                existing.retain(|item| {
                    !super::executor::evaluate_predicate(expr_str, item).unwrap_or(true)
                });
            }
        }
    }
}

/// Per-Patch execution trace record (lightweight, no full snapshot).
///
/// Records what happened when a single Patch was applied, including
/// duration, condition match status, summary statistics, and affected elements.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionTrace {
    /// Corresponding Patch ID (links to the Patch that produced this trace)
    pub patch_id: PatchId,

    /// Source information (file, line, plugin, etc.)
    pub source: PatchSource,

    /// Operation that was executed
    pub op: PatchOp,

    /// Execution duration in microseconds
    pub duration_us: u64,

    /// Whether condition was matched (for Scoped Patches)
    pub condition_matched: bool,

    /// Execution summary (replaces before/after full snapshot)
    pub summary: TraceSummary,

    /// List of affected elements (only changed ones, not unchanged)
    pub affected_items: Vec<AffectedItem>,
}

impl ExecutionTrace {
    /// 创建新的执行追踪
    pub fn new(
        patch_id: PatchId,
        source: PatchSource,
        op: PatchOp,
        duration_us: u64,
        condition_matched: bool,
        summary: TraceSummary,
        affected_items: Vec<AffectedItem>,
    ) -> Self {
        Self {
            patch_id,
            source,
            op,
            duration_us,
            condition_matched,
            summary,
            affected_items,
        }
    }

    /// Check whether this trace affects the given config path.
    ///
    /// Uses **whole-word boundary matching** to avoid false positives from
    /// substring containment (e.g., `"dns".contains("dn")` → false).
    ///
    /// For DeepMerge operations where affected_items may be empty (because the merge
    /// doesn't track individual element changes), falls back to patch path matching
    /// via the `patch_path` parameter.
    pub fn affects_path(&self, path: &str, patch_path: Option<&str>) -> bool {
        // If affected_items is non-empty, use item-level matching
        if !self.affected_items.is_empty() {
            return self.affected_items.iter().any(|item| {
                let before_match = item.before.as_ref().is_some_and(|b| {
                    b == path
                        || b.starts_with(&format!("{}.", path))
                        || path.starts_with(&format!("{}.", b))
                });
                let after_match = item.after.as_ref().is_some_and(|a| {
                    a == path
                        || a.starts_with(&format!("{}.", path))
                        || path.starts_with(&format!("{}.", a))
                });
                before_match || after_match
            });
        }

        // Fallback: for operations like DeepMerge where affected_items is empty,
        // check the patch path directly
        if let Some(pp) = patch_path {
            if pp == path {
                return true;
            }
            if pp.starts_with(&format!("{}.", path)) || path.starts_with(&format!("{}.", pp)) {
                return true;
            }
        }

        false
    }

    /// 描述变更内容（用于 Explain View）
    pub fn describe_change(&self) -> String {
        format!(
            "{} — +{} -{} ~{} ({}μs)",
            self.op.display_name(),
            self.summary.added,
            self.summary.removed,
            self.summary.modified,
            self.duration_us
        )
    }
}

/// Execution summary — aggregate counts for a single Patch execution.
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    /// Number of added elements
    pub added: usize,
    /// Number of removed elements
    pub removed: usize,
    /// Number of modified elements
    pub modified: usize,
    /// Number of unchanged elements
    pub kept: usize,
    /// Total element count before operation
    pub total_before: usize,
    /// Total element count after operation
    pub total_after: usize,
}

impl TraceSummary {
    /// 创建执行摘要
    pub fn new(
        added: usize,
        removed: usize,
        modified: usize,
        kept: usize,
        total_before: usize,
        total_after: usize,
    ) -> Self {
        Self {
            added,
            removed,
            modified,
            kept,
            total_before,
            total_after,
        }
    }
}

/// A single affected element (only stores change info, not full before/after state).
#[derive(Debug, Clone, Serialize)]
pub struct AffectedItem {
    /// Index in the target array
    pub index: usize,

    /// Description before change (None = newly added)
    pub before: Option<String>,

    /// Description after change (None = deleted)
    pub after: Option<String>,

    /// Action type (added / removed / modified)
    pub action: TraceAction,
}

impl AffectedItem {
    /// Create an AffectedItem representing an addition.
    pub fn added(index: usize, description: impl Into<String>) -> Self {
        Self {
            index,
            before: None,
            after: Some(description.into()),
            action: TraceAction::Added,
        }
    }

    /// Create an AffectedItem representing a removal.
    pub fn removed(index: usize, description: impl Into<String>) -> Self {
        Self {
            index,
            before: Some(description.into()),
            after: None,
            action: TraceAction::Removed,
        }
    }

    /// Create an AffectedItem representing a modification.
    pub fn modified(index: usize, before: impl Into<String>, after: impl Into<String>) -> Self {
        Self {
            index,
            before: Some(before.into()),
            after: Some(after.into()),
            action: TraceAction::Modified,
        }
    }
}

/// Trace action type enum.
#[derive(Debug, Clone, Serialize)]
pub enum TraceAction {
    Added,
    Removed,
    Modified,
}

impl std::fmt::Display for TraceAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceAction::Added => write!(f, "新增"),
            TraceAction::Removed => write!(f, "删除"),
            TraceAction::Modified => write!(f, "修改"),
        }
    }
}

/// Source query result — "Why is this rule here?" (§3.2).
#[derive(Debug, Clone, Serialize)]
pub struct ExplainEntry {
    /// Source information (taken directly from trace, no Patch lookup needed)
    pub source: PatchSource,

    /// Operation display name (e.g., "DeepMerge", "Filter")
    pub op_name: String,

    /// Human-readable change description
    pub detail: String,
}

// ══════════════════════════════════════════════════════════
// §10 测试 — 统计与过滤增强
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::PatchOp;
    use crate::source::SourceKind;

    fn make_test_trace(
        _id: &str,
        file: Option<&str>,
        op: PatchOp,
        matched: bool,
        summary: TraceSummary,
    ) -> ExecutionTrace {
        ExecutionTrace::new(
            PatchId::new(),
            crate::source::PatchSource {
                kind: SourceKind::YamlFile,
                file: file.map(|s| s.to_string()),
                line: None,
                plugin_id: None,
            },
            op,
            42,
            matched,
            summary,
            vec![],
        )
    }

    #[test]
    fn test_statistics_empty() {
        let mgr = TraceManager::new();
        let stats = mgr.statistics();
        assert_eq!(stats.total_patches, 0);
        assert_eq!(stats.succeeded, 0);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn test_statistics_mixed_results() {
        let mut mgr = TraceManager::new();
        mgr.push(
            make_test_trace(
                "1",
                Some("a.yaml"),
                PatchOp::DeepMerge,
                true,
                TraceSummary::new(1, 0, 2, 5, 8, 10),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "dns",
                PatchOp::DeepMerge,
                serde_json::json!({}),
            ),
        );
        mgr.push(
            make_test_trace(
                "2",
                Some("b.yaml"),
                PatchOp::Append,
                false,
                TraceSummary::new(0, 0, 0, 3, 3, 3),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "rules",
                PatchOp::Append,
                serde_json::json!([]),
            ),
        );
        mgr.push(
            make_test_trace(
                "3",
                Some("a.yaml"),
                PatchOp::SetDefault,
                true,
                TraceSummary::new(0, 0, 1, 0, 0, 1),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "tun",
                PatchOp::SetDefault,
                serde_json::json!({}),
            ),
        );

        let stats = mgr.statistics();
        assert_eq!(stats.total_patches, 3);
        assert_eq!(stats.succeeded, 2);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.total_added, 1);
        assert_eq!(stats.total_modified, 3); // 2 + 1
        assert_eq!(stats.total_duration_us, 126); // 42 * 3
    }

    #[test]
    fn test_filter_by_source_file() {
        let mut mgr = TraceManager::new();
        mgr.push(
            make_test_trace(
                "1",
                Some("/path/to/base-dns.yaml"),
                PatchOp::DeepMerge,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "dns",
                PatchOp::DeepMerge,
                serde_json::json!({}),
            ),
        );
        mgr.push(
            make_test_trace(
                "2",
                Some("/path/to/rules.yaml"),
                PatchOp::Append,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "rules",
                PatchOp::Append,
                serde_json::json!([]),
            ),
        );

        let filtered = mgr.filter_by_source_file("base-dns");
        assert_eq!(filtered.len(), 1);

        let all = mgr.filter_by_source_file(".yaml");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_filter_by_op() {
        let mut mgr = TraceManager::new();
        mgr.push(
            make_test_trace(
                "1",
                None,
                PatchOp::DeepMerge,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "a",
                PatchOp::DeepMerge,
                serde_json::json!({}),
            ),
        );
        mgr.push(
            make_test_trace(
                "2",
                None,
                PatchOp::DeepMerge,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "b",
                PatchOp::DeepMerge,
                serde_json::json!({}),
            ),
        );
        mgr.push(
            make_test_trace(
                "3",
                None,
                PatchOp::Append,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "c",
                PatchOp::Append,
                serde_json::json!([]),
            ),
        );

        let merges = mgr.filter_by_op(&PatchOp::DeepMerge);
        assert_eq!(merges.len(), 2);

        let appends = mgr.filter_by_op(&PatchOp::Append);
        assert_eq!(appends.len(), 1);
    }

    #[test]
    fn test_affected_paths_dedup() {
        let mut mgr = TraceManager::new();
        // 同一路径被多个 patch 修改 → 去重后只出现一次
        for _ in 0..3 {
            mgr.push(
                make_test_trace(
                    "x",
                    None,
                    PatchOp::DeepMerge,
                    true,
                    TraceSummary::new(0, 0, 0, 0, 0, 0),
                ),
                Patch::new(
                    crate::source::PatchSource {
                        kind: SourceKind::Builtin,
                        file: None,
                        line: None,
                        plugin_id: None,
                    },
                    crate::scope::Scope::Global,
                    "dns",
                    PatchOp::DeepMerge,
                    serde_json::json!({}),
                ),
            );
        }
        mgr.push(
            make_test_trace(
                "y",
                None,
                PatchOp::Append,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
            ),
            Patch::new(
                crate::source::PatchSource {
                    kind: SourceKind::Builtin,
                    file: None,
                    line: None,
                    plugin_id: None,
                },
                crate::scope::Scope::Global,
                "rules",
                PatchOp::Append,
                serde_json::json!([]),
            ),
        );

        let paths = mgr.affected_paths();
        assert_eq!(paths.len(), 2); // dns + rules（去重）
        assert!(paths.contains(&"dns".to_string()));
        assert!(paths.contains(&"rules".to_string()));
    }

    #[test]
    fn test_full_report_contains_header() {
        let mgr = TraceManager::new();
        let report = mgr.full_report();
        assert!(report.contains("Prism Engine 执行追踪报告"));
        assert!(report.contains("总览"));
    }

    /// 验证 replay_at_step 与手动 apply_patch_simple 的一致性。
    ///
    /// 确保 replay 机制能正确复现单步 Patch 执行的结果，
    /// 这是 Explain View 和 Diff View 功能正确性的基础。
    #[test]
    fn test_replay_consistency_basic() {
        use crate::scope::Scope;

        // 1. 创建简单的 base_config
        let base_config = serde_json::json!({
            "dns": { "enable": false },
            "mixed-port": 7890
        });

        // 2. 创建一个简单的 Patch（DeepMerge 操作）
        let patch = Patch::new(
            crate::source::PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({ "enable": true, "nameserver": ["8.8.8.8"] }),
        );

        // 3. 手动执行 apply_patch_simple 得到结果 A
        let mut config_a = base_config.clone();
        apply_patch_simple(&mut config_a, &patch);

        // 4. 创建 TraceManager 并 push patch，然后使用 replay_at_step(0) 得到结果 B
        let mut mgr = TraceManager::new();
        let trace = ExecutionTrace::new(
            patch.id.clone(),
            crate::source::PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            PatchOp::DeepMerge,
            0,
            true,
            TraceSummary::new(0, 0, 1, 1, 1, 2),
            vec![],
        );
        mgr.push(trace, patch.clone());

        let context = crate::executor::ExecutionContext::default();
        let config_b = mgr
            .replay_at_step(0, &base_config, &context)
            .expect("replay_at_step(0) 应返回 Some");

        // 5. 断言 A == B
        assert_eq!(
            config_a, config_b,
            "手动 apply_patch_simple 结果应与 replay_at_step(0) 一致"
        );

        // 额外验证：DeepMerge 应正确合并字段
        assert_eq!(config_a["dns"]["enable"], true);
        assert_eq!(
            config_a["dns"]["nameserver"],
            serde_json::json!(["8.8.8.8"])
        );
        // 原始字段不受影响
        assert_eq!(config_a["mixed-port"], 7890);
    }
}
