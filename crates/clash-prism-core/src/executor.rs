//! # Patch Executor — Executes Patch IR to Produce Final Configuration
//!
//! ## Responsibilities
//!
//! - Profile-level concurrent execution (conceptual; single-threaded in v1)
//! - 8 operation implementations with **fixed execution order** (§2.4)
//! - Composite operation handling (`$filter` + `$prepend` on same key)
//! - Condition evaluation (`condition` + `scope` matching)
//! - [`ExecutionTrace`] recording for Explain View / Diff View
//! - `$transform` runtime validation (§2.9 anti-corruption mechanism)
//!
//! ## Performance: In-Place Mutation
//!
//! All 8 operations mutate `config` **in-place** via `*_in_place` methods,
//! avoiding per-operation deep clone of the entire JSON tree.
//! Only [`ExecutionTrace`] is returned (not a cloned config).
//!
//! ## Operation Execution Order (§2.4)
//!
//! When multiple operations target the same key, they execute in this fixed order:
//!
//! ```text
//! $filter → $remove → $transform → $prepend → $append → $default → DeepMerge → Override
//! ```

use std::sync::Arc;
use std::time::Instant;

use crate::error::{PrismError, Result, TransformWarning};
use crate::ir::{Patch, PatchOp};
use crate::json_path::{
    get_array_at_path_mut, get_array_len, get_json_path, get_json_path_mut,
    get_or_create_json_path_mut,
};
use crate::trace::{AffectedItem, ExecutionTrace, TraceSummary};

mod expr;
pub use expr::{ExprValue, evaluate_predicate, evaluate_transform_expr};

// ── Guarded Fields (§架构文档) ──────────────────────────────────────
// These fields are managed by the external controller and must NOT be
// overwritten by Prism patches.  The executor skips them with a warning.

/// Configuration paths that Prism must never overwrite.
///
/// These are typically set by the external controller (e.g. mihomo dashboard)
/// and should remain under user / operator control.
pub const GUARDED_FIELDS: &[&str] = &[
    "external-controller",
    "secret",
    "mixed-port",
    "external-ui",
    "external-ui-url",
    "api-secret",
    "authentication",
];

/// Returns `true` if `path` matches a guarded field (exact top-level match
/// or starts with a guarded prefix followed by `.`).
///
/// Uses a precomputed list of guarded prefixes (with trailing `.`) via
/// `LazyLock` to avoid allocating a temporary `format!("{}.", guarded)`
/// string on every call.
pub fn is_guarded_path(path: &str) -> bool {
    static GUARDED_PREFIXES: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| GUARDED_FIELDS.iter().map(|&g| format!("{}.", g)).collect());

    GUARDED_FIELDS.contains(&path)
        || GUARDED_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix.as_str()))
}

// Avoids redundant regex compilation in profile_matches and expression evaluator.
// Uses IndexMap for insertion-order tracking, enabling LRU eviction.
const MAX_REGEX_CACHE_SIZE: usize = 512;

/// 单次 append/prepend 操作超过此阈值时，启用摘要模式（不逐条创建 AffectedItem）
const TRACE_SUMMARY_THRESHOLD: usize = 100;

/// 从 JSON 值中提取人类可读描述（用于 trace）
///
/// - 字符串元素（如 rules 数组）：直接使用字符串值
/// - 对象元素（如 proxies 数组）：使用 "name" 字段
/// - 兜底：JSON 序列化整个值
fn extract_item_description(item: &serde_json::Value) -> String {
    item.as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            item.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| serde_json::to_string(item).unwrap_or_else(|_| "$item".to_string()))
}

thread_local! {
    /// Thread-local regex cache for the executor.
    ///
    /// Each thread maintains its own independent cache, which is intentional:
    /// - No synchronization overhead (lock-free per-thread access)
    /// - Thread-locality improves cache hit rates (each thread compiles patterns it uses)
    /// - Uses IndexMap for insertion-order tracking (LRU eviction on overflow)
    static EXEC_REGEX_CACHE: std::cell::RefCell<indexmap::IndexMap<String, regex::Regex>> =
        std::cell::RefCell::new(indexmap::IndexMap::new());
}

/// Get a cached Regex from the executor's thread-local cache.
///
/// Uses [`regex::RegexBuilder`] with explicit `size_limit` and `dfa_size_limit`
/// to prevent ReDoS (Regular Expression Denial of Service) attacks from
/// maliciously crafted patterns that could cause exponential backtracking.
///
/// When the cache exceeds MAX_REGEX_CACHE_SIZE, evicts the oldest
/// 25% of entries (LRU strategy) instead of clearing all entries (avalanche risk).
fn get_exec_cached_regex(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    EXEC_REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        // Check cache hit first
        if cache.contains_key(pattern) {
            // Use move_index to promote to back for LRU ordering.
            // This is an acceptable trade-off because:
            //   1. The cache is capped at MAX_REGEX_CACHE_SIZE = 512 entries
            //   2. Regex compilation (~microseconds) dominates the O(n) shift cost (~nanoseconds)
            //   3. IndexMap's contiguous memory layout provides better cache locality than linked-list LRU
            // If this ever becomes a bottleneck, consider replacing with a dedicated LRU crate
            // (e.g., lru::LruCache) which provides O(1) promotion via intrusive linked lists.
            if let Some(idx) = cache.get_index_of(pattern) {
                let last = cache.len() - 1;
                cache.move_index(idx, last);
            }
            return Ok(cache.get(pattern).unwrap().clone());
        }
        // LRU eviction: when cache is full, remove oldest 25% of entries
        if cache.len() >= MAX_REGEX_CACHE_SIZE {
            let evict_count = MAX_REGEX_CACHE_SIZE / 4;
            for _ in 0..evict_count {
                cache.shift_remove_index(0);
            }
            tracing::debug!(
                "Regex cache exceeded max size ({}), evicted {} oldest entries (LRU)",
                MAX_REGEX_CACHE_SIZE,
                evict_count
            );
        }
        let re = regex::RegexBuilder::new(pattern)
            .size_limit(1024 * 1024)
            .dfa_size_limit(1024 * 1024)
            .build()?;
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// Execution context — used for condition evaluation during patch execution.
///
/// Provides runtime information that scoped patches can check against:
/// - Which proxy core is running (mihomo, clash-rs, etc.)
/// - Which OS platform (Windows, macOS, Linux)
/// - Which profile is currently active
/// - Current WiFi SSID (for SSID-based conditional scoping)
///
/// Set via [`PatchExecutor::with_context()`] or defaults to empty.
#[derive(Debug, Clone, Default)]
pub struct ExecutionContext {
    /// Current proxy core type (e.g., "mihomo", "clash-rs")
    pub core_type: Option<String>,
    /// Current operating system platform
    pub platform: Option<String>,
    /// Current profile name (for Profile/Scoped scope matching)
    pub profile_name: Option<String>,
    /// Current WiFi SSID (set by external SSID monitoring)
    pub ssid: Option<String>,
}

/// Patch executor — applies Patches to a base configuration JSON.
///
/// Maintains an execution trace ([`ExecutionTrace`]) for each applied Patch,
/// enabling Explain View and Diff View functionality.
///
/// # Example
///
/// ```ignore
/// let mut executor = PatchExecutor::new();
/// let config = json!({"dns": {}});
/// let result = executor.execute_owned(config, &patches)?;
/// // result = modified config
/// // executor.traces = vec of ExecutionTrace for each patch
/// ```
pub struct PatchExecutor {
    /// 执行追踪记录
    pub traces: Vec<ExecutionTrace>,
    /// 执行上下文（用于条件判断）
    pub context: ExecutionContext,
    /// Whether execution tracing is enabled (controls clone optimization).
    /// When false, snapshot clones for diff counting are skipped.
    tracing_enabled: bool,
}

impl PatchExecutor {
    /// Create a new executor with default (empty) execution context.
    pub fn new() -> Self {
        Self {
            traces: vec![],
            context: ExecutionContext::default(),
            tracing_enabled: true,
        }
    }

    /// Create an executor with the specified execution context.
    ///
    /// Use this when you need condition evaluation based on platform/core/profile.
    pub fn with_context(context: ExecutionContext) -> Self {
        Self {
            traces: vec![],
            context,
            tracing_enabled: true,
        }
    }

    /// Decompose a composite Patch into individual sub-Patches.
    ///
    /// For composite operations (multiple ops on the same key, e.g., `$filter` + `$prepend`),
    /// each sub-operation becomes its own `Patch` inheriting metadata from the parent.
    /// For non-composite Patches, returns a single-element vector containing the patch itself.
    fn prepare_sub_patches(patch: &Patch) -> Vec<Patch> {
        if !patch.is_composite() {
            return vec![patch.clone()];
        }
        patch
            .all_ops()
            .iter()
            .map(|sub_op| Patch {
                id: patch.id.clone(),
                source: patch.source.clone(),
                scope: patch.scope.clone(),
                path: patch.path.clone(),
                op: sub_op.op.clone(),
                value: sub_op.value.clone(),
                condition: patch.condition.clone(),
                after: vec![],
                sub_ops: vec![],
            })
            .collect()
    }

    /// Execute a list of Patches against a base configuration.
    ///
    /// Patches should already be topologically sorted (via [`PatchCompiler::resolve_dependencies()`]).
    ///
    /// For composite operation Patches (multiple ops on same key, e.g., `$filter` + `$prepend`),
    /// each sub-operation executes in fixed order and generates its own trace.
    ///
    /// # Arguments
    /// * `config` — Base configuration JSON (will be consumed and mutated in-place)
    /// * `patches` — Sorted list of Patches to apply
    ///
    /// # Returns
    /// The final configuration after all patches have been applied.
    pub fn execute(
        &mut self,
        mut config: serde_json::Value,
        patches: &[&Patch],
    ) -> Result<serde_json::Value> {
        // to prevent stale traces from previous executions accumulating.
        self.traces.clear();

        for patch in patches {
            for sub_patch in Self::prepare_sub_patches(patch) {
                let op_start = Instant::now();
                let mut trace = self.apply_patch_in_place(&mut config, &sub_patch)?;
                trace.duration_us = op_start.elapsed().as_micros() as u64;
                self.traces.push(trace);
            }
        }
        Ok(config)
    }

    /// 便捷方法：接受 `&[Patch]`（owned），内部转换为引用。
    /// 用于向后兼容测试代码和 watcher 等调用方。
    pub fn execute_owned(
        &mut self,
        config: serde_json::Value,
        patches: &[Patch],
    ) -> Result<serde_json::Value> {
        let refs: Vec<&Patch> = patches.iter().collect();
        self.execute(config, &refs)
    }

    /// 两阶段管线执行（§4.1 / §9）
    ///
    /// Phase 1: Profile 级并发 — 每个 Profile 的 patches 独立执行到各自的 config 副本
    /// Phase 2: 合并后叠加 — 将所有 Profile 结果合并到 base config，
    ///          然后按 Global → Scoped → Runtime 顺序依次执行剩余 patches
    ///
    /// # Arguments
    /// * `base_config` — 基础配置（将被 Phase 2 的结果修改）。
    ///   **注意**: `base_config` 仅在 Phase 2 结束后被写回（`*base_config = merged_config`）；
    ///   Phase 1 各 Profile 从 `base_snapshot`（base_config 的 clone）开始独立执行，
    ///   Phase 1 中对 base config 的任何修改不会影响原始 base_config。
    /// * `profile_groups` — Profile 分组列表 `(profile_name, patches)`
    /// * `shared_patches` — Global/Scoped/Runtime 级 patches
    ///
    /// # Returns
    /// 所有阶段的执行追踪记录
    pub fn execute_pipeline(
        &mut self,
        base_config: &serde_json::Value,
        profile_groups: Vec<(String, Vec<&Patch>)>,
        shared_patches: Vec<&Patch>,
    ) -> Result<(Vec<ExecutionTrace>, serde_json::Value)> {
        self.traces.clear();
        let mut all_traces: Vec<ExecutionTrace> = Vec::new();

        // ── Phase 1: Profile 级并发执行 ──
        // 每个 Profile 从 base_config 的 clone 开始，独立执行自己的 patches。
        // 使用 std::thread::scope 实现零依赖并发。
        // Wrap base_snapshot in Arc to avoid N+1 clones.
        // Each thread gets a cheap Arc::clone (pointer increment, ~ns) instead of
        // a deep JSON clone (which could be MB-scale for large proxy configs).
        // Design note: Arc is used here instead of Cow because the base config is
        // immutable during profile execution — no mutation is needed, only reads.
        let base_snapshot = Arc::new(base_config.clone());
        let context_snapshot = Arc::new(self.context.clone());

        let profile_results: Vec<(String, serde_json::Value, Vec<ExecutionTrace>)> =
            std::thread::scope(
                |s| -> Result<Vec<(String, serde_json::Value, Vec<ExecutionTrace>)>> {
                    let handles: Vec<std::thread::ScopedJoinHandle<_>> = profile_groups
                        .into_iter()
                        .map(|(profile_name, patches)| {
                            let base_clone = Arc::clone(&base_snapshot);
                            let ctx_clone = Arc::clone(&context_snapshot);
                            s.spawn(move || {
                                // 使用 std::panic::catch_unwind 包装执行逻辑，
                                // 确保 panic 不会逃逸 scoped thread，同时保留 patch_id 信息。
                                // Clone profile_name for error reporting outside catch_unwind.
                                let profile_name_for_err = profile_name.clone();
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        execute_profile_patches(
                                            &base_clone,
                                            &ctx_clone,
                                            &profile_name,
                                            &patches,
                                        )
                                    }));

                                match result {
                                    Ok(Ok(result)) => Ok(result),
                                    Ok(Err(e)) => Err(e),
                                    Err(panic_payload) => {
                                        // 在 panic 消息中包含 profile_name 以便定位
                                        let panic_msg = if let Some(s) =
                                            panic_payload.downcast_ref::<&str>()
                                        {
                                            format!(
                                                "Profile '{}' 执行线程 panic: {}",
                                                profile_name_for_err, s
                                            )
                                        } else if let Some(s) =
                                            panic_payload.downcast_ref::<String>()
                                        {
                                            format!(
                                                "Profile '{}' 执行线程 panic: {}",
                                                profile_name_for_err, s
                                            )
                                        } else {
                                            format!(
                                                "Profile '{}' 执行线程 panic（未知 payload 类型）",
                                                profile_name_for_err
                                            )
                                        };
                                        tracing::error!("{}", panic_msg);
                                        Err(PrismError::PatchExecutionFailed {
                                            patch_id: format!("profile:{}", profile_name_for_err),
                                            reason: panic_msg,
                                        })
                                    }
                                }
                            })
                        })
                        .collect();

                    // catch_unwind 已在闭包内处理所有 panic 并转换为 Err(PrismError)，
                    // 因此 join() 不会收到 panic payload。使用 expect 提供明确的不可达断言。
                    let mut results = Vec::with_capacity(handles.len());
                    for h in handles {
                        let thread_result = h
                            .join()
                            .expect("catch_unwind should prevent all panics from reaching join");
                        results.push(thread_result.map_err(|e| {
                            crate::error::PrismError::PatchExecutionFailed {
                                patch_id: "pipeline-phase1".to_string(),
                                reason: e.to_string(),
                            }
                        })?);
                    }
                    Ok(results)
                },
            )?;

        // 收集 Phase 1 的 traces
        let mut merged_config = Arc::try_unwrap(base_snapshot).unwrap_or_else(|arc| (*arc).clone());
        // Sort profile results by name for deterministic merge order.
        // Thread join order is non-deterministic; sorting ensures consistent output.
        let mut profile_results = profile_results;
        profile_results.sort_by(|a, b| a.0.cmp(&b.0));
        for (_name, profile_config, traces) in &profile_results {
            all_traces.extend(traces.iter().cloned());
            // 将 Profile 结果 deep merge 到合并配置中
            // Profile 间按顺序覆盖（后面的覆盖前面的）
            deep_merge_json(&mut merged_config, profile_config);
        }

        // ── Phase 2: 合并后按 scope priority 排序执行 shared_patches ──
        // Global(1) → Scoped(2) → Runtime(3)
        // Use a composite sort key (priority, original_index) to preserve the
        // existing __after__ dependency order within the same priority level.
        // The shared_patches are already sorted by dependency order from
        // compile_and_execute_pipeline, so the original index encodes that order.
        let mut indexed_shared: Vec<(usize, &Patch)> = shared_patches
            .iter()
            .enumerate()
            .map(|(i, p)| (i, *p))
            .collect();
        indexed_shared.sort_by_key(|(idx, patch)| (patch.scope.priority(), *idx));
        let sorted_shared: Vec<&Patch> = indexed_shared.into_iter().map(|(_, p)| p).collect();

        for patch in sorted_shared {
            for sub_patch in Self::prepare_sub_patches(patch) {
                let op_start = Instant::now();
                let mut trace = self.apply_patch_in_place(&mut merged_config, &sub_patch)?;
                trace.duration_us = op_start.elapsed().as_micros() as u64;
                all_traces.push(trace);
            }
        }

        // Return merged config instead of writing back via &mut.
        // Caller is responsible for assigning the result.
        Ok((all_traces, merged_config))
    }

    /// Apply a single Patch to config (**in-place mutation**).
    ///
    /// Mutates `config` directly instead of cloning the entire JSON tree.
    /// Returns only the [`ExecutionTrace`], avoiding per-op deep clone overhead.
    ///
    /// Condition check happens first — even unmatched patches produce a trace
    /// (recording skip reason), so Explain View shows full coverage.
    fn apply_patch_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        // Condition check — verify scope matches current context
        let condition_matched = self.check_condition(patch);

        // Return trace even when condition doesn't match (records skip reason)
        if !condition_matched {
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                false, // condition_matched = false
                TraceSummary::new(0, 0, 0, 0, 0, 0),
                vec![],
            );
            return Ok(trace);
        }

        // 统一 guarded field 检查 — 覆盖所有操作类型。
        // DeepMerge 和 Override 已在各自的 execute_*_in_place 方法中有单独检查，
        // 此处对其他操作（$prepend / $append / $filter / $transform / $remove / $set_default）
        // 进行统一拦截，防止意外修改受保护字段。
        if is_guarded_path(&patch.path)
            && !matches!(&patch.op, PatchOp::DeepMerge | PatchOp::Override)
        {
            tracing::warn!(
                path = %patch.path,
                patch_id = %patch.id,
                op = ?patch.op,
                "apply_patch_in_place: 跳过 guarded field 的非 DeepMerge/Override 操作"
            );
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
                vec![],
            );
            return Ok(trace);
        }

        match &patch.op {
            PatchOp::DeepMerge => self.execute_deep_merge_in_place(config, patch),
            PatchOp::Override => self.execute_override_in_place(config, patch),
            PatchOp::Prepend => self.execute_prepend_in_place(config, patch),
            PatchOp::Append => self.execute_append_in_place(config, patch),
            PatchOp::Filter { expr, .. } => self.execute_filter_in_place(config, patch, &expr.expr),
            PatchOp::Transform { expr, .. } => {
                self.execute_transform_in_place(config, patch, &expr.expr)
            }
            PatchOp::Remove { expr, .. } => self.execute_remove_in_place(config, patch, &expr.expr),
            PatchOp::SetDefault => self.execute_set_default_in_place(config, patch),
        }
    }

    /// Check if the Patch's scope and condition match the current execution context.
    ///
    /// Matching rules per scope type:
    /// - **Global**: always matches
    /// - **Profile**: matches if `context.profile_name` equals or regex-matches the scope profile
    /// - **Scoped**: checks platform, core type, profile name, and time range
    /// - **Runtime**: always matches in CLI mode (UI-driven in GUI mode)
    fn check_condition(&self, patch: &Patch) -> bool {
        check_patch_condition(patch, &self.context)
    }

    /// Check if a profile name matches a scope profile pattern.
    ///
    /// Supports three matching strategies:
    /// - **Exact match**: `scope_profile == profile_name`
    /// - **Regex match**: `/pattern/` syntax (e.g., `/work-.*/`)
    /// - **Wildcard match**: patterns containing `*` or `?`
    /// - **Plain substring**: falls back to exact equality
    pub fn profile_matches(profile_name: &str, scope_profile: &str) -> bool {
        if profile_name == scope_profile {
            return true;
        }
        let pattern = scope_profile.trim();
        // Full regex — /pattern/ syntax
        if pattern.starts_with('/') && pattern.ends_with('/') && pattern.len() > 2 {
            let inner = &pattern[1..pattern.len() - 1];
            if let Ok(re) = get_exec_cached_regex(inner) {
                return re.is_match(profile_name);
            }
            tracing::warn!(pattern = inner, "Invalid regex in profile match");
            return false;
        }
        // Wildcard — patterns containing * or ?
        if pattern.contains('*') || pattern.contains('?') {
            let escaped = regex::escape(pattern);
            let regex_pattern = escaped.replace("\\*", ".*").replace("\\?", ".");
            if let Ok(re) = get_exec_cached_regex(&format!("^{}$", regex_pattern)) {
                return re.is_match(profile_name);
            }
            return false;
        }
        // Plain: exact match only (no substring fallback to avoid false positives)
        false
    }

    // ─── 各操作实现 ───

    /// Operation 1: Deep Merge (default behavior) — **in-place mutation**.
    ///
    /// Recursively merges `patch.value` into the target path.
    /// Objects are merged key-by-key; non-object values are replaced.
    ///
    /// Guarded field check: recursively inspects all keys in the overlay value.
    /// If any key path (relative to `patch.path`) matches a guarded field,
    /// that specific key is skipped with a warning.
    fn execute_deep_merge_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        // Guarded field check for DeepMerge: recursively inspect overlay keys.
        // DeepMerge merges individual keys, so we check each key path.
        if is_guarded_path(&patch.path) {
            tracing::warn!(
                path = %patch.path,
                patch_id = %patch.id,
                "DeepMerge: target path is guarded, skipping entirely"
            );
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
                vec![],
            );
            return Ok(trace);
        }

        let target = self.get_or_create_path_mut(config, &patch.path);

        if let Some(target) = target {
            // Only clone snapshot for diff counting when trace is enabled.
            // In non-trace mode (typical production), skip the clone entirely.
            let (modified, total) = if self.tracing_enabled {
                let snapshot = target.clone();
                deep_merge_json(target, &patch.value);
                count_merge_diff(&snapshot, target)
            } else {
                deep_merge_json(target, &patch.value);
                (0, 0) // Placeholder values when tracing is disabled
            };

            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(0, 0, modified, total.saturating_sub(modified), total, total),
                vec![],
            );
            Ok(trace)
        } else {
            tracing::debug!(
                path = %patch.path,
                "DeepMerge: target path does not exist, skipping"
            );
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
                vec![],
            );
            Ok(trace)
        }
    }

    /// Operation 2: Force Override (exclusive key) — **in-place mutation**.
    ///
    /// `$override` replaces the entire target path value. Cannot be combined
    /// with other operations on the same key (enforced at parse time).
    ///
    /// Guarded field check: rejects override on guarded paths.
    fn execute_override_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        // Guarded field check for Override: reject entirely.
        if is_guarded_path(&patch.path) {
            tracing::warn!(
                path = %patch.path,
                patch_id = %patch.id,
                "Override: target path is guarded, skipping"
            );
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(0, 0, 0, 0, 0, 0),
                vec![],
            );
            return Ok(trace);
        }

        let mut executed = true;
        // (e.g., "dns.nameserver" instead of only top-level keys).
        if let Some(target) = get_or_create_json_path_mut(config, &patch.path) {
            *target = patch.value.clone();
        } else {
            // Fallback: if path navigation fails, try top-level insert
            if let Some(obj) = config.as_object_mut() {
                obj.insert(patch.path.clone(), patch.value.clone());
            } else {
                executed = false;
                tracing::warn!(
                    path = %patch.path,
                    "execute_override_in_place: config root is not an Object, cannot insert key"
                );
            }
        }

        // condition_matched is always true here (we only reach this code when condition matched).
        // `executed` tracks whether the override actually modified the config.
        let summary = if executed {
            TraceSummary::new(0, 0, 1, 0, 1, 1) // Override: modified=1(替换操作), total_before=1(原值)
        } else {
            TraceSummary::new(0, 0, 0, 0, 0, 0) // No modification occurred
        };

        let trace = ExecutionTrace::new(
            patch.id.clone(),
            patch.source.clone(),
            patch.op.clone(),
            0,
            true, // condition_matched: always true (condition already matched to reach here)
            summary,
            vec![],
        );

        Ok(trace)
    }

    /// Operation 3: Array Prepend Insert — **in-place mutation**.
    ///
    /// Inserts items at the beginning of the target array.
    fn execute_prepend_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        let new_items: &[serde_json::Value] = match &patch.value {
            serde_json::Value::Array(arr) => arr.as_slice(),
            _ => std::slice::from_ref(&patch.value),
        };
        let items_to_prepend = new_items.len();
        let original_len = get_array_len(config, &patch.path);

        let actually_prepend;
        if let Some(serde_json::Value::Array(existing)) = get_array_at_path_mut(config, &patch.path)
        {
            // Batch prepend: O(n+m) instead of O(n*m)
            existing.splice(0..0, new_items.iter().cloned());
            actually_prepend = items_to_prepend;
        } else {
            tracing::debug!(
                path = %patch.path,
                "Prepend: target path does not exist or is not an array, skipping"
            );
            actually_prepend = 0;
        }

        // Extract actual item text for annotation matching:
        // - String elements (rules array): use the string value directly
        // - Object elements (proxies array): use the "name" field
        // - Fallback: JSON-serialize the entire value
        //
        // 大批量时启用摘要模式：只生成一条摘要 AffectedItem，
        // 完整描述存入 bulk_items（Arc 共享，clone 开销 O(1)）
        let (affected, bulk_items) = if new_items.len() >= TRACE_SUMMARY_THRESHOLD {
            let descs: Arc<[String]> = new_items.iter().map(extract_item_description).collect();
            let summary_item = AffectedItem::added(0, format!("{} items prepended", descs.len()));
            (vec![summary_item], Some(descs))
        } else {
            let items: Vec<AffectedItem> = new_items
                .iter()
                .enumerate()
                .map(|(i, item)| AffectedItem::added(i, extract_item_description(item)))
                .collect();
            (items, None)
        };

        let trace = if let Some(bulk) = bulk_items {
            ExecutionTrace::with_bulk_items(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(
                    actually_prepend,
                    0,
                    0,
                    original_len,
                    original_len,
                    original_len + actually_prepend,
                ),
                affected,
                bulk,
            )
        } else {
            ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(
                    actually_prepend,
                    0,
                    0,
                    original_len,
                    original_len,
                    original_len + actually_prepend,
                ),
                affected,
            )
        };

        Ok(trace)
    }

    /// Operation 4: Array Append Insert — **in-place mutation**.
    ///
    /// Appends items to the end of the target array.
    fn execute_append_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        let original_len = get_array_len(config, &patch.path);
        let items_to_append = match &patch.value {
            serde_json::Value::Array(arr) => arr.len(),
            _ => 1,
        };

        let actually_append;
        if let Some(serde_json::Value::Array(existing)) = get_array_at_path_mut(config, &patch.path)
        {
            if let serde_json::Value::Array(new_items) = &patch.value {
                existing.extend(new_items.iter().cloned());
            } else {
                existing.push(patch.value.clone());
            }
            actually_append = items_to_append;
        } else {
            actually_append = 0;
        }

        // Extract actual item text for annotation matching (same strategy as prepend)
        let new_items: Vec<&serde_json::Value> = match &patch.value {
            serde_json::Value::Array(arr) => arr.iter().collect(),
            single => vec![single],
        };

        // 大批量时启用摘要模式：只生成一条摘要 AffectedItem，
        // 完整描述存入 bulk_items（Arc 共享，clone 开销 O(1)）
        let (affected, bulk_items) = if new_items.len() >= TRACE_SUMMARY_THRESHOLD {
            let descs: Arc<[String]> = new_items
                .iter()
                .map(|item| extract_item_description(item))
                .collect();
            let summary_item =
                AffectedItem::added(original_len, format!("{} items appended", descs.len()));
            (vec![summary_item], Some(descs))
        } else {
            let items: Vec<AffectedItem> = new_items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    AffectedItem::added(original_len + i, extract_item_description(item))
                })
                .collect();
            (items, None)
        };

        let trace = if let Some(bulk) = bulk_items {
            ExecutionTrace::with_bulk_items(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(
                    actually_append,
                    0,
                    0,
                    original_len,
                    original_len,
                    original_len + actually_append,
                ),
                affected,
                bulk,
            )
        } else {
            ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(
                    actually_append,
                    0,
                    0,
                    original_len,
                    original_len,
                    original_len + actually_append,
                ),
                affected,
            )
        };

        Ok(trace)
    }

    /// Operation 5: Conditional Filter — keep elements matching expression (**in-place**).
    ///
    /// Evaluates predicate expression against each element in the target array,
    /// keeping only those where the predicate returns `true`.
    /// Elements that fail evaluation (error) are kept by default (conservative).
    fn execute_filter_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
        filter_expr: &str,
    ) -> Result<ExecutionTrace> {
        let mut affected = vec![];
        let mut kept_count = 0;
        let mut removed_count = 0;

        if let Some(serde_json::Value::Array(items)) = get_array_at_path_mut(config, &patch.path) {
            // 第一遍：评估每个元素，记录被过滤掉的元素（用于 trace 的 affected_items）
            // 只克隆被移除的元素，避免全量克隆
            let eval_results: Vec<(usize, bool, bool)> = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    match evaluate_predicate(filter_expr, item) {
                        Ok(true) => (idx, true, false),   // 保留
                        Ok(false) => (idx, false, false), // 移除
                        Err(_) => (idx, true, true),      // 错误，保留
                    }
                })
                .collect();

            for (idx, keep, is_err) in &eval_results {
                if *is_err {
                    tracing::warn!(
                        "Filter expression error on element #{}: keeping element",
                        idx
                    );
                }
                if !keep && !is_err {
                    removed_count += 1;
                    // 只克隆被移除的元素用于 trace
                    let item = &items[*idx];
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    affected.push(AffectedItem::removed(*idx, name));
                }
                if *keep {
                    kept_count += 1;
                }
            }

            // 第二遍：使用 retain 原地过滤（避免构建新 Vec）
            let mut eval_iter = eval_results.into_iter();
            items.retain(|_| eval_iter.next().is_none_or(|(_, keep, _)| keep));
        }

        let total_after = kept_count;
        let trace = ExecutionTrace::new(
            patch.id.clone(),
            patch.source.clone(),
            patch.op.clone(),
            0,
            true,
            TraceSummary::new(
                0,
                removed_count,
                0,
                kept_count,
                kept_count + removed_count,
                total_after,
            ),
            affected,
        );

        Ok(trace)
    }

    /// Operation 6: Map Transform — apply transform expression to each element (**in-place**).
    ///
    /// Applies a transform expression to each element in the target array.
    /// After transformation, runs runtime validation (§2.9 anti-corruption mechanism)
    /// checking that the first N results contain required fields (`name`, `type`, `server`, `port`).
    fn execute_transform_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
        transform_expr: &str,
    ) -> Result<ExecutionTrace> {
        let mut affected = vec![];
        let mut modified_count = 0;

        let mut original_items: Vec<serde_json::Value> = vec![];

        if let Some(serde_json::Value::Array(items)) = get_array_at_path_mut(config, &patch.path) {
            // Early return for empty arrays — nothing to transform
            if items.is_empty() {
                let trace = ExecutionTrace::new(
                    patch.id.clone(),
                    patch.source.clone(),
                    patch.op.clone(),
                    0,
                    true,
                    TraceSummary::new(0, 0, 0, 0, 0, 0),
                    vec![],
                );
                return Ok(trace);
            }

            // 收集原始数据用于校验
            // validate_transform_results() 通过比较变换前后的字段来检测缺失的必要字段。
            // 仅在 tracing 启用时克隆采样窗口（前 N 个元素），避免大数组全量克隆。
            let sample_size = items.len().min(TRANSFORM_VALIDATE_SAMPLE_SIZE);
            original_items = if self.tracing_enabled {
                items.iter().take(sample_size).cloned().collect()
            } else {
                vec![]
            };

            // 对每个元素应用变换
            let results: Result<Vec<serde_json::Value>> = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let before_name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();

                    match evaluate_transform_expr(transform_expr, item) {
                        Ok(result) => {
                            let after_name = result
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string();

                            affected.push(AffectedItem::modified(idx, before_name, after_name));
                            modified_count += 1;
                            Ok(result)
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Transform error on element #{}: {}, keeping original",
                                idx,
                                e
                            );
                            Ok(item.clone())
                        }
                    }
                })
                .collect();

            match results {
                Ok(transformed) => {
                    // 先做校验再赋值，避免额外克隆
                    let warnings = validate_transform_results(&original_items, &transformed);
                    for w in &warnings {
                        tracing::warn!("Transform warning: {}", w);
                    }

                    let total = items.len();
                    let trace = ExecutionTrace::new(
                        patch.id.clone(),
                        patch.source.clone(),
                        patch.op.clone(),
                        0,
                        true,
                        TraceSummary::new(
                            0,
                            0,
                            modified_count,
                            total - modified_count,
                            total,
                            transformed.len(),
                        ),
                        affected,
                    );
                    *items = transformed;
                    return Ok(trace);
                }
                Err(e) => {
                    // 不应发生（我们在 map 内部处理了错误）
                    tracing::error!("Unexpected transform collection error: {}", e);
                }
            }
        }

        // Fallback: array path not found — return empty trace
        let total = original_items.len();
        let trace = ExecutionTrace::new(
            patch.id.clone(),
            patch.source.clone(),
            patch.op.clone(),
            0,
            true,
            TraceSummary::new(0, 0, modified_count, total - modified_count, total, total),
            affected,
        );

        Ok(trace)
    }

    /// Operation 7: Conditional Remove — delete elements matching expression (**in-place**).
    ///
    /// Semantically opposite to `$filter`: `$filter` keeps matching, `$remove` deletes matching.
    /// Removes elements from the end of the array first to maintain index validity.
    fn execute_remove_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
        remove_expr: &str,
    ) -> Result<ExecutionTrace> {
        let mut affected = vec![];
        let mut removed_count = 0;
        let mut kept_count = 0;

        if let Some(serde_json::Value::Array(items)) = get_array_at_path_mut(config, &patch.path) {
            // 第一遍：评估每个元素，记录被移除的元素（用于 trace 的 affected_items）
            // 只克隆被移除的元素，避免全量克隆
            let eval_results: Vec<(usize, bool, bool)> = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    match evaluate_predicate(remove_expr, item) {
                        Ok(true) => (idx, false, false), // 移除
                        Ok(false) => (idx, true, false), // 保留
                        Err(_) => (idx, true, true),     // 错误，保留
                    }
                })
                .collect();

            for (idx, keep, is_err) in &eval_results {
                if *is_err {
                    tracing::warn!(
                        "Remove expression error on element #{}: keeping element",
                        idx
                    );
                }
                if !keep && !is_err {
                    removed_count += 1;
                    // 只克隆被移除的元素用于 trace
                    let item = &items[*idx];
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    affected.push(AffectedItem::removed(*idx, name));
                }
                if *keep {
                    kept_count += 1;
                }
            }

            // 第二遍：使用 retain 原地过滤（避免构建新 Vec）
            let mut eval_iter = eval_results.into_iter();
            items.retain(|_| eval_iter.next().is_none_or(|(_, keep, _)| keep));

            let total_after = kept_count;
            let trace = ExecutionTrace::new(
                patch.id.clone(),
                patch.source.clone(),
                patch.op.clone(),
                0,
                true,
                TraceSummary::new(
                    0,
                    removed_count,
                    0,
                    kept_count,
                    kept_count + removed_count,
                    total_after,
                ),
                affected,
            );

            return Ok(trace);
        }

        // 路径不存在或不是数组
        let trace = ExecutionTrace::new(
            patch.id.clone(),
            patch.source.clone(),
            patch.op.clone(),
            0,
            true,
            TraceSummary::new(0, 0, 0, 0, 0, 0),
            vec![],
        );
        Ok(trace)
    }

    /// Operation 8: Default Value Injection (§2.7 boundary logic) — **in-place mutation**.
    ///
    /// Triggers when **either** condition is met:
    /// - Field does not exist
    /// - Field value is `null`
    ///
    /// Does **NOT** trigger (preserves existing value):
    /// - Field exists with any non-null value (including empty array `[]` or empty object `{}`)
    fn execute_set_default_in_place(
        &self,
        config: &mut serde_json::Value,
        patch: &Patch,
    ) -> Result<ExecutionTrace> {
        let mut triggered = false;

        // §2.7 边界逻辑：只在字段不存在或为 null 时设置
        // 空数组 [] 和空字典 {} 视为有效值，不触发默认值注入
        // 策略：先尝试 get（不创建），如果不存在则用 get_or_create 创建后注入
        let parts: Vec<&str> = patch.path.split('.').collect();
        let field_exists = if parts.len() == 1 {
            config
                .as_object()
                .is_some_and(|obj| obj.contains_key(&patch.path))
        } else {
            get_json_path(config, &patch.path).is_some()
        };

        if !field_exists {
            // 字段不存在 → 创建路径并注入默认值
            if let Some(target) = get_or_create_json_path_mut(config, &patch.path) {
                *target = patch.value.clone();
                triggered = true;
            }
        } else if let Some(target) = get_json_path_mut(config, &patch.path) {
            // 字段存在 → 仅当值为 null 时注入
            let should_inject = matches!(target, serde_json::Value::Null);
            if should_inject {
                *target = patch.value.clone();
                triggered = true;
            }
        }

        let summary = if triggered {
            TraceSummary::new(0, 0, 1, 0, 0, 1)
        } else {
            TraceSummary::new(0, 0, 0, 1, 0, 0)
        };

        let trace = ExecutionTrace::new(
            patch.id.clone(),
            patch.source.clone(),
            patch.op.clone(),
            0,
            true,
            summary,
            vec![],
        );

        Ok(trace)
    }

    // ─── 辅助方法 ───

    /// Get or create a mutable reference at the given dot-notation path.
    /// Creates intermediate objects if they don't exist.
    fn get_or_create_path_mut<'a>(
        &'a self,
        config: &'a mut serde_json::Value,
        path: &str,
    ) -> Option<&'a mut serde_json::Value> {
        get_or_create_json_path_mut(config, path)
    }
}

// ──────────────────────────────────────────────────────
// JSON 工具函数
// ──────────────────────────────────────────────────────

/// 递归统计 `merged` 相对于 `base` 的差异。
///
/// 对于嵌套对象，递归计算子键的差异数量；
/// 对于数组和其他类型，直接比较是否相等。
///
/// 返回 `(modified_count, total_keys)`：
/// - `total_keys` 为合并结果中所有层级的键总数
/// - `modified_count` 为与 base 不同的键数量（包括新增和修改的）
fn count_merge_diff(base: &serde_json::Value, merged: &serde_json::Value) -> (usize, usize) {
    match (base.as_object(), merged.as_object()) {
        (Some(base_obj), Some(merged_obj)) => {
            let mut total = 0usize;
            let mut modified = 0usize;
            for (k, v) in merged_obj {
                total += 1;
                match base_obj.get(k) {
                    // 两个值都是对象：递归统计子键差异
                    Some(bv) if bv.is_object() && v.is_object() => {
                        let (sub_modified, sub_total) = count_merge_diff(bv, v);
                        // 如果子对象有任何差异，则当前键也算作已修改
                        if sub_modified > 0 {
                            modified += 1;
                        }
                        total += sub_total;
                        modified += sub_modified;
                    }
                    // 两个值都是数组：直接比较
                    Some(bv) if bv.is_array() && v.is_array() => {
                        if *bv != *v {
                            modified += 1;
                        }
                    }
                    // 其他类型：直接比较
                    Some(bv) => {
                        if *bv != *v {
                            modified += 1;
                        }
                    }
                    // base 中不存在此键：新增
                    None => {
                        modified += 1;
                    }
                }
            }
            (modified, total)
        }
        _ => {
            // 非对象合并：如果值不同则计为 1 处修改
            if base != merged { (1, 1) } else { (0, 0) }
        }
    }
}

/// Recursively deep-merge two JSON values.
///
/// Uses a two-phase strategy to avoid borrow checker conflicts:
/// First checks types with `is_object()` (no borrow), then handles in branches.
///
/// Merge rules:
/// - Object + Object → recursive key-by-key merge
/// - Anything else → `base` is replaced by `overlay`
pub fn deep_merge_json(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    let is_base_obj = base.is_object();
    let is_overlay_obj = overlay.is_object();

    if is_base_obj && is_overlay_obj {
        // 两者都是 Object，安全地分别获取可变/不可变借用进行递归合并
        // 实际代码使用安全的 as_object_mut() + as_object() 模式
        if let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) {
            for (key, value) in overlay_obj {
                base_obj
                    .entry(key.clone())
                    .and_modify(|existing| deep_merge_json(existing, value))
                    .or_insert_with(|| value.clone());
            }
        }
    } else {
        // 非对象情况（数组、null、数字等）：直接覆盖
        *base = overlay.clone();
    }
}

// ─── 公共操作辅助函数（供 executor 和 trace replay 共用）───
//
// 将 PatchOp 的核心操作逻辑提取为独立函数，
// executor 的 execute_*_in_place 和 trace 的 apply_single_op 共用，
// 避免代码重复并确保行为一致。

/// Override 操作：在指定路径强制替换值。
///
/// 支持嵌套路径（如 "dns.nameserver"），自动创建中间对象。
pub fn apply_override(config: &mut serde_json::Value, path: &str, value: &serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        if let Some(obj) = config.as_object_mut() {
            obj.insert(path.to_string(), value.clone());
        }
    } else {
        let mut current = config;
        for &part in &parts[..parts.len() - 1] {
            if let Some(obj) = current.as_object_mut() {
                current = obj
                    .entry(part.to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            } else {
                return;
            }
        }
        if let Some(obj) = current.as_object_mut() {
            obj.insert(parts[parts.len() - 1].to_string(), value.clone());
        }
    }
}

/// Prepend 操作：在指定路径的数组头部插入元素。
///
/// 若 `value` 不是数组，则将其包装为单元素数组后插入，
/// 与内部 `execute_prepend_in_place` 行为一致。
pub fn apply_prepend(config: &mut serde_json::Value, path: &str, value: &serde_json::Value) {
    if let Some(arr) = get_json_path_mut(config, path)
        && let Some(existing) = arr.as_array_mut()
    {
        let new_items = match value.as_array() {
            Some(arr) => arr,
            None => std::slice::from_ref(value),
        };
        existing.splice(0..0, new_items.iter().cloned());
    }
}

/// Append 操作：在指定路径的数组尾部追加元素。
///
/// 若 `value` 不是数组，则将其包装为单元素数组后追加，
/// 与内部 `execute_prepend_in_place` 行为一致。
pub fn apply_append(config: &mut serde_json::Value, path: &str, value: &serde_json::Value) {
    if let Some(arr) = get_json_path_mut(config, path)
        && let Some(existing) = arr.as_array_mut()
    {
        let new_items = match value.as_array() {
            Some(arr) => arr,
            None => std::slice::from_ref(value),
        };
        existing.extend(new_items.iter().cloned());
    }
}

/// SetDefault 操作：仅在指定路径不存在或为 null 时设置值。
pub fn apply_set_default(config: &mut serde_json::Value, path: &str, value: &serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        if let Some(obj) = config.as_object_mut() {
            let should_inject = match obj.get(path) {
                None => true,
                Some(serde_json::Value::Null) => true,
                Some(_) => false,
            };
            if should_inject {
                obj.insert(path.to_string(), value.clone());
            }
        }
    } else {
        let mut current = config;
        for &part in &parts[..parts.len() - 1] {
            match current.as_object_mut() {
                Some(obj) => {
                    current = obj
                        .entry(part.to_string())
                        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                }
                None => return,
            }
        }
        if let Some(obj) = current.as_object_mut() {
            let last_key = parts[parts.len() - 1];
            let should_inject = match obj.get(last_key) {
                None => true,
                Some(serde_json::Value::Null) => true,
                Some(_) => false,
            };
            if should_inject {
                obj.insert(last_key.to_string(), value.clone());
            }
        }
    }
}

/// $transform runtime validation (§2.9 anti-corruption mechanism).
///
/// **Sampling validation**: Only checks the first `min(node_count, 5)` transformed results
/// for required fields (`name`, `type`, `server`, `port`). This is a deliberate trade-off:
/// full validation on large proxy lists (thousands of entries) would add unacceptable latency
/// to every transform execution. The sampling catches the most common mistake (forgetting to
/// spread the original proxy) in the first few results, which is sufficient for practical use.
///
/// Warns if any result is missing required fields.
/// Hint suggests using spread operator: `{...p, name: ...}` instead of `{name: ...}`.
///
/// TODO: Consider making sample size configurable or adding an opt-in full validation mode
/// for CI/CD pipelines where correctness is more important than latency.
const TRANSFORM_VALIDATE_SAMPLE_SIZE: usize = 5;

fn validate_transform_results(
    original: &[serde_json::Value],
    results: &[serde_json::Value],
) -> Vec<TransformWarning> {
    let sample_size = results.len().min(TRANSFORM_VALIDATE_SAMPLE_SIZE);
    let mut warnings = vec![];

    for i in 0..sample_size {
        if let Some(result) = results.get(i) {
            for required_field in &["name", "type", "server", "port"] {
                if result.get(*required_field).is_none() {
                    // 尝试从原始数据获取节点名称用于警告消息
                    let node_name = original
                        .get(i)
                        .and_then(|o| o.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            result
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                        })
                        .to_string();

                    warnings.push(TransformWarning {
                        node_index: i,
                        node_name,
                        field: required_field.to_string(),
                        hint: "Did you forget to spread the original proxy? \
                               Use ({...p, name: ...}) instead of ({name: ...})."
                            .to_string(),
                    });
                }
            }
        }
    }

    warnings
}

impl Default for PatchExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Execute all patches for a single profile within a scoped thread.
///
/// Extracted from the `catch_unwind` closure in [`PatchExecutor::execute`]
/// to minimize the `AssertUnwindSafe` scope — only this thin wrapper is
/// wrapped, keeping the actual execution logic free of unsafe unwind assumptions.
fn execute_profile_patches(
    base_clone: &Arc<serde_json::Value>,
    ctx_clone: &Arc<ExecutionContext>,
    profile_name: &str,
    patches: &[&Patch],
) -> std::result::Result<(String, serde_json::Value, Vec<ExecutionTrace>), crate::error::PrismError>
{
    let mut profile_executor = PatchExecutor::with_context((**ctx_clone).clone());
    profile_executor.context.profile_name = Some(profile_name.to_string());
    let mut config = (**base_clone).clone();

    for patch in patches {
        for sub_patch in PatchExecutor::prepare_sub_patches(patch) {
            let op_start = Instant::now();
            let mut trace = profile_executor.apply_patch_in_place(&mut config, &sub_patch)?;
            trace.duration_us = op_start.elapsed().as_micros() as u64;
            profile_executor.traces.push(trace);
        }
    }

    Ok((profile_name.to_string(), config, profile_executor.traces))
}

/// Check if a patch's condition is satisfied given the execution context.
///
/// This is the single source of truth for condition matching logic, used by both
/// [`PatchExecutor::check_condition`] (runtime execution) and
/// [`crate::trace::check_patch_condition`] (replay). Extracting it as a free
/// function ensures the two code paths always stay in sync.
///
/// Matching rules per scope type:
/// - **Global**: always matches
/// - **Profile**: matches if `context.profile_name` equals or pattern-matches the scope profile
/// - **Scoped**: checks enabled, SSID, time range, core type, platform, and profile
/// - **Runtime**: always matches in CLI mode
pub fn check_patch_condition(patch: &Patch, context: &ExecutionContext) -> bool {
    match &patch.scope {
        crate::scope::Scope::Global => true,
        crate::scope::Scope::Profile(_) => {
            if let Some(pname) = &context.profile_name
                && let crate::scope::Scope::Profile(scope_profile) = &patch.scope
            {
                return PatchExecutor::profile_matches(pname, scope_profile);
            }
            // No profile_name in context (e.g., CLI without --profile flag):
            // Profile-level patches are NOT applied by default. This prevents
            // unintended patch application when the user hasn't explicitly
            // selected a profile. In GUI mode, the context should always carry
            // a profile_name to ensure correct filtering.
            false
        }
        crate::scope::Scope::Scoped {
            profile,
            platform,
            core,
            time_range,
            enabled,
            ssid,
        } => {
            if let Some(false) = enabled {
                return false;
            }
            if let Some(expected_ssid) = ssid {
                if let Some(current_ssid) = &context.ssid {
                    if current_ssid != expected_ssid {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            if let Some(tr) = time_range
                && !tr.is_active_now()
            {
                return false;
            }
            if let (Some(scope_core), Some(ctx_core)) = (core, &context.core_type)
                && scope_core != ctx_core
            {
                return false;
            }
            if let Some(scope_platforms) = platform
                && let Some(ctx_platform) = &context.platform
            {
                // repeated allocation inside the closure for each scope_platform entry.
                let ctx_platform_lower = ctx_platform.to_lowercase();
                let matched = scope_platforms.iter().any(|p| {
                    let p_str = format!("{}", p).to_lowercase();
                    p_str == ctx_platform_lower || p_str == "all" || ctx_platform_lower == "all"
                });
                if !matched {
                    return false;
                }
            }
            if let Some(scope_profile) = profile
                && let Some(ctx_profile) = &context.profile_name
            {
                return PatchExecutor::profile_matches(ctx_profile, scope_profile);
            }
            // scope 声明了 profile 条件但 context 未指定 profile → 不执行
            if profile.is_some() && context.profile_name.is_none() {
                return false;
            }
            true
        }
        crate::scope::Scope::Runtime => true,
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompiledPredicate, Patch, PatchOp, SubOp};
    use crate::scope::Scope;
    use crate::source::{PatchSource, SourceKind};

    fn make_set_default_patch(path: &str, value: serde_json::Value) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::SetDefault,
            value,
        )
    }

    /// §2.7 边界测试：字段不存在时应注入默认值
    #[test]
    fn test_set_default_field_not_exists() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_set_default_patch("dns", serde_json::json!({"enable": true}));

        let result = executor.execute_owned(config.clone(), &[patch]).unwrap();
        assert!(executor.traces.last().unwrap().condition_matched);
        assert_eq!(result["dns"]["enable"], true);
    }

    /// §2.7 边界测试：字段为 null 时应注入默认值
    #[test]
    fn test_set_default_field_is_null() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": null});
        let patch = make_set_default_patch("dns", serde_json::json!({"enable": true}));

        let result = executor.execute_owned(config.clone(), &[patch]).unwrap();
        assert!(executor.traces.last().unwrap().condition_matched);
        assert_eq!(result["dns"]["enable"], true);
    }

    /// §2.7 边界测试：空数组 [] 不触发默认值注入（核心边界！）
    #[test]
    fn test_set_default_empty_array_no_trigger() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"rules": []});
        let patch = make_set_default_patch("rules", serde_json::json!(["MATCH,DIRECT"]));

        let _result = executor.execute_owned(config.clone(), &[patch]).unwrap();
        // 空数组是有效值，不应触发注入（modified=0, added=0 表示未触发）
        assert_eq!(executor.traces.last().unwrap().summary.modified, 0);
        assert_eq!(executor.traces.last().unwrap().summary.added, 0);
    }

    /// §2.7 边界测试：空字典 {} 不触发默认值注入（核心边界！）
    #[test]
    fn test_set_default_empty_object_no_trigger() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {}});
        let patch = make_set_default_patch(
            "dns",
            serde_json::json!({"enable": true, "enhanced-mode": "fake-ip"}),
        );

        let _result = executor.execute_owned(config.clone(), &[patch]).unwrap();
        // 空字典是有效值，不应触发注入（modified=0, added=0 表示未触发）
        assert_eq!(executor.traces.last().unwrap().summary.modified, 0);
        assert_eq!(executor.traces.last().unwrap().summary.added, 0);
    }

    /// §2.7 边界测试：已有非空值时不覆盖
    #[test]
    fn test_set_default_existing_value_no_override() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"enable": false, "enhanced-mode": "redir-host"}});
        let patch = make_set_default_patch(
            "dns",
            serde_json::json!({"enable": true, "enhanced-mode": "fake-ip"}),
        );

        let result = executor.execute_owned(config.clone(), &[patch]).unwrap();
        // 原有值应保留
        assert_eq!(result["dns"]["enable"], false);
        assert_eq!(result["dns"]["enhanced-mode"], "redir-host");
    }

    // ══════════════════════════════════════════════════════════
    // deep_merge_json 测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_deep_merge_basic() {
        let mut base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({"c": 3, "d": 4});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["a"], 1);
        assert_eq!(base["b"], 2);
        assert_eq!(base["c"], 3);
        assert_eq!(base["d"], 4);
    }

    #[test]
    fn test_deep_merge_nested_object() {
        let mut base = serde_json::json!({"dns": {"enable": false, "port": 53}});
        let overlay = serde_json::json!({"dns": {"enable": true, "nameservers": ["8.8.8.8"]}});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["dns"]["enable"], true); // overwritten
        assert_eq!(base["dns"]["port"], 53); // preserved
        assert_eq!(base["dns"]["nameservers"][0], "8.8.8.8"); // added
    }

    #[test]
    fn test_deep_merge_array_replacement() {
        // Arrays are replaced, not concatenated
        let mut base = serde_json::json!({"rules": ["a", "b"]});
        let overlay = serde_json::json!({"rules": ["c", "d", "e"]});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["rules"].as_array().unwrap().len(), 3);
        assert_eq!(base["rules"][0], "c");
    }

    #[test]
    fn test_deep_merge_null_overwrites_value() {
        // null in overlay replaces the base value (it's a valid JSON value)
        let mut base = serde_json::json!({"key": "value"});
        let overlay = serde_json::json!({"key": serde_json::Value::Null});
        deep_merge_json(&mut base, &overlay);
        assert!(base["key"].is_null());
    }

    #[test]
    fn test_deep_merge_scalar_replaces_object() {
        let mut base = serde_json::json!({"key": {"nested": true}});
        let overlay = serde_json::json!({"key": "now-a-string"});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["key"], "now-a-string");
    }

    #[test]
    fn test_deep_merge_object_replaces_scalar() {
        let mut base = serde_json::json!({"key": "a-string"});
        let overlay = serde_json::json!({"key": {"nested": true}});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["key"]["nested"], true);
    }

    #[test]
    fn test_deep_merge_empty_overlay_no_change() {
        let mut base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({});
        deep_merge_json(&mut base, &overlay);
        assert_eq!(base["a"], 1);
        assert_eq!(base["b"], 2);
    }

    // ══════════════════════════════════════════════════════════
    // count_merge_diff 测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_count_merge_diff_no_changes() {
        let base = serde_json::json!({"a": 1, "b": 2, "c": 3});
        let merged = base.clone();
        let (modified, total) = count_merge_diff(&base, &merged);
        assert_eq!(modified, 0);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_count_merge_diff_partial_changes() {
        let base = serde_json::json!({"a": 1, "b": 2, "c": 3});
        let merged = serde_json::json!({"a": 1, "b": 99, "c": 3, "d": 4});
        let (modified, total) = count_merge_diff(&base, &merged);
        assert_eq!(modified, 2); // b changed + d added
        assert_eq!(total, 4);
    }

    #[test]
    fn test_count_merge_diff_all_new_keys() {
        let base = serde_json::json!({});
        let merged = serde_json::json!({"x": 1, "y": 2});
        let (modified, total) = count_merge_diff(&base, &merged);
        assert_eq!(modified, 2);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_count_merge_diff_non_object() {
        let base = serde_json::json!("old");
        let merged = serde_json::json!("new");
        let (modified, total) = count_merge_diff(&base, &merged);
        assert_eq!(modified, 1);
        assert_eq!(total, 1);
    }

    #[test]
    fn test_count_merge_diff_non_object_same() {
        let base = serde_json::json!(42);
        let merged = serde_json::json!(42);
        let (modified, total) = count_merge_diff(&base, &merged);
        assert_eq!(modified, 0);
        assert_eq!(total, 0);
    }

    // ══════════════════════════════════════════════════════════
    // profile_matches 测试 (wildcard / exact / regex)
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_profile_matches_exact() {
        assert!(PatchExecutor::profile_matches("hello", "hello"));
        assert!(!PatchExecutor::profile_matches("hello", "world"));
    }

    #[test]
    fn test_profile_matches_star_matches_anything() {
        assert!(PatchExecutor::profile_matches("anything", "*"));
        assert!(PatchExecutor::profile_matches("", "*"));
        assert!(PatchExecutor::profile_matches("some.long.domain.name", "*"));
    }

    #[test]
    fn test_profile_matches_domain_prefix() {
        assert!(PatchExecutor::profile_matches("DOMAIN-KEYWORD", "DOMAIN-*"));
        assert!(PatchExecutor::profile_matches("DOMAIN-SUFFIX", "DOMAIN-*"));
        assert!(!PatchExecutor::profile_matches(
            "domain-lowercase",
            "DOMAIN-*"
        )); // case-sensitive
    }

    #[test]
    fn test_profile_matches_subdomain_pattern() {
        assert!(PatchExecutor::profile_matches(
            "www.google.com",
            "*.google.com"
        ));
        assert!(PatchExecutor::profile_matches(
            "mail.google.com",
            "*.google.com"
        ));
        assert!(!PatchExecutor::profile_matches(
            "google.com",
            "*.google.com"
        ));
        assert!(!PatchExecutor::profile_matches(
            "notgoogle.com",
            "*.google.com"
        ));
    }

    #[test]
    fn test_profile_matches_question_mark() {
        assert!(PatchExecutor::profile_matches("abc", "a?c"));
        assert!(PatchExecutor::profile_matches("axc", "a?c"));
        assert!(!PatchExecutor::profile_matches("abbc", "a?c"));
    }

    #[test]
    fn test_profile_matches_multiple_stars() {
        assert!(PatchExecutor::profile_matches("a-b-c", "*-*-*"));
        assert!(PatchExecutor::profile_matches("x-y-z", "*-*-*"));
    }

    #[test]
    fn test_profile_matches_plain_exact_only() {
        // plain mode uses exact match, NOT substring
        assert!(PatchExecutor::profile_matches("mihomo", "mihomo"));
        assert!(!PatchExecutor::profile_matches("mihomo-core", "mihomo")); // no substring
        assert!(!PatchExecutor::profile_matches(
            "prefix-mihomo-suffix",
            "mihomo"
        )); // no substring
        assert!(!PatchExecutor::profile_matches("clash", "mihomo"));
    }

    #[test]
    fn test_profile_matches_full_regex() {
        assert!(PatchExecutor::profile_matches("proxy-123", r"/proxy-\d+/"));
        assert!(!PatchExecutor::profile_matches("proxy-abc", r"/proxy-\d+/"));
    }

    #[test]
    fn test_profile_matches_invalid_regex_returns_false() {
        // invalid regex returns false (no silent fallback to substring)
        assert!(!PatchExecutor::profile_matches("hello[world", "[world"));
    }

    // ══════════════════════════════════════════════════════════
    // validate_transform_results 测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_validate_transform_results_valid_no_warnings() {
        let original =
            vec![serde_json::json!({"name": "p1", "type": "ss", "server": "1.1.1.1", "port": 443})];
        let results = vec![
            serde_json::json!({"name": "p1-mod", "type": "ss", "server": "2.2.2.2", "port": 8080}),
        ];
        let warnings = validate_transform_results(&original, &results);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_validate_transform_results_missing_name() {
        let original = vec![serde_json::json!({"name": "p1"})];
        let results = vec![serde_json::json!({"type": "ss"})]; // missing name
        let warnings = validate_transform_results(&original, &results);
        assert!(!warnings.is_empty());
        assert!(warnings.iter().any(|w| w.field == "name"));
    }

    #[test]
    fn test_validate_transform_results_missing_type() {
        let original = vec![serde_json::json!({"name": "p1"})];
        let results = vec![serde_json::json!({"name": "p1"})]; // missing type
        let warnings = validate_transform_results(&original, &results);
        assert!(warnings.iter().any(|w| w.field == "type"));
    }

    #[test]
    fn test_validate_transform_results_missing_server_and_port() {
        let original = vec![serde_json::json!({"name": "p1"})];
        let results = vec![serde_json::json!({"name": "p1", "type": "ss"})];
        let warnings = validate_transform_results(&original, &results);
        assert!(warnings.iter().any(|w| w.field == "server"));
        assert!(warnings.iter().any(|w| w.field == "port"));
    }

    #[test]
    fn test_validate_transform_results_empty_no_warnings() {
        let original = vec![];
        let results = vec![];
        let warnings = validate_transform_results(&original, &results);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_validate_transform_results_hint_contains_spread() {
        let original = vec![serde_json::json!({"name": "p1"})];
        let results = vec![serde_json::json!({"name": "new-name"})];
        let warnings = validate_transform_results(&original, &results);
        assert!(!warnings.is_empty());
        assert!(warnings[0].hint.contains("spread"));
    }

    // ══════════════════════════════════════════════════════════
    // ExecutionContext 测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_execution_context_defaults() {
        let ctx = ExecutionContext::default();
        assert!(ctx.core_type.is_none());
        assert!(ctx.platform.is_none());
        assert!(ctx.profile_name.is_none());
        assert!(ctx.ssid.is_none());
    }

    #[test]
    fn test_execution_context_with_values() {
        let ctx = ExecutionContext {
            core_type: Some("mihomo".into()),
            platform: Some("macos".into()),
            profile_name: Some("work".into()),
            ssid: Some("HomeWiFi".into()),
        };
        assert_eq!(ctx.core_type.as_deref(), Some("mihomo"));
        assert_eq!(ctx.platform.as_deref(), Some("macos"));
        assert_eq!(ctx.profile_name.as_deref(), Some("work"));
        assert_eq!(ctx.ssid.as_deref(), Some("HomeWiFi"));
    }

    #[test]
    fn test_execution_context_clone() {
        let ctx = ExecutionContext {
            core_type: Some("clash".into()),
            platform: None,
            profile_name: Some("home".into()),
            ssid: None,
        };
        let cloned = ctx.clone();
        assert_eq!(cloned.core_type, ctx.core_type);
        assert_eq!(cloned.profile_name, ctx.profile_name);
        assert_eq!(cloned.platform, ctx.platform);
        assert_eq!(cloned.ssid, ctx.ssid);
    }

    // ══════════════════════════════════════════════════════════
    // PatchExecutor 构造器
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_executor_new_empty_traces() {
        let executor = PatchExecutor::new();
        assert!(executor.traces.is_empty());
        assert!(executor.context.core_type.is_none());
    }

    #[test]
    fn test_executor_with_context() {
        let ctx = ExecutionContext {
            core_type: Some("mihomo".into()),
            platform: Some("linux".into()),
            profile_name: Some("prod".into()),
            ssid: None,
        };
        let executor = PatchExecutor::with_context(ctx);
        assert_eq!(executor.context.core_type.as_deref(), Some("mihomo"));
        assert_eq!(executor.context.platform.as_deref(), Some("linux"));
        assert_eq!(executor.context.profile_name.as_deref(), Some("prod"));
    }

    #[test]
    fn test_executor_default_trait() {
        let executor = PatchExecutor::default();
        assert!(executor.traces.is_empty());
    }

    // ══════════════════════════════════════════════════════════
    // DeepMerge 执行测试
    // ══════════════════════════════════════════════════════════

    fn make_deep_merge_patch(path: &str, value: serde_json::Value) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::DeepMerge,
            value,
        )
    }

    #[test]
    fn test_execute_deep_merge_creates_new_key() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_deep_merge_patch("dns", serde_json::json!({"enable": true}));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["dns"]["enable"], true);
    }

    #[test]
    fn test_execute_deep_merge_merges_nested() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"port": 53}});
        let patch = make_deep_merge_patch("dns", serde_json::json!({"enable": true}));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["dns"]["port"], 53);
        assert_eq!(result["dns"]["enable"], true);
    }

    // ══════════════════════════════════════════════════════════
    // Append 执行测试
    // ══════════════════════════════════════════════════════════

    fn make_append_patch(path: &str, value: serde_json::Value) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Append,
            value,
        )
    }

    #[test]
    fn test_execute_append_to_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"rules": ["MATCH,DIRECT"]});
        let patch = make_append_patch("rules", serde_json::json!(["DOMAIN,example.com,DIRECT"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["rules"].as_array().unwrap().len(), 2);
        assert_eq!(result["rules"][1], "DOMAIN,example.com,DIRECT");
    }

    #[test]
    fn test_execute_append_to_nonexistent_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_append_patch("rules", serde_json::json!(["RULE"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        // Path doesn't exist → append is a no-op (result 中无 rules 字段)
        assert!(result.get("rules").is_none());
        assert_eq!(
            executor.traces[0].summary.added, 0,
            "路径不存在时 added 应为 0"
        );
    }

    // ══════════════════════════════════════════════════════════
    // Prepend 执行测试
    // ══════════════════════════════════════════════════════════

    fn make_prepend_patch(path: &str, value: serde_json::Value) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Prepend,
            value,
        )
    }

    #[test]
    fn test_execute_prepend_to_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"rules": ["MATCH,DIRECT"]});
        let patch = make_prepend_patch("rules", serde_json::json!(["DOMAIN,example.com,DIRECT"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["rules"].as_array().unwrap().len(), 2);
        assert_eq!(result["rules"][0], "DOMAIN,example.com,DIRECT");
        assert_eq!(result["rules"][1], "MATCH,DIRECT");
    }

    // ══════════════════════════════════════════════════════════
    // Override 执行测试
    // ══════════════════════════════════════════════════════════

    fn make_override_patch(path: &str, value: serde_json::Value) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Override,
            value,
        )
    }

    #[test]
    fn test_execute_override_replaces_value() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"enable": false, "port": 53}});
        let patch = make_override_patch("dns", serde_json::json!({"enable": true}));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["dns"]["enable"], true);
        // port should be gone since Override replaces the entire value
        assert!(result["dns"].get("port").is_none());
    }

    // ══════════════════════════════════════════════════════════
    // 条件匹配测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_global_scope_always_matches() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_deep_merge_patch("dns", serde_json::json!({"enable": true}));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["dns"]["enable"], true);
        assert!(executor.traces[0].condition_matched);
    }

    #[test]
    fn test_scoped_disabled_never_matches() {
        let ctx = ExecutionContext::default();
        let mut executor = PatchExecutor::with_context(ctx);
        let config = serde_json::json!({});
        let patch = Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Scoped {
                profile: None,
                platform: None,
                core: None,
                time_range: None,
                enabled: Some(false),
                ssid: None,
            },
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({"enable": true}),
        );
        let result = executor.execute_owned(config, &[patch]).unwrap();
        // Patch should be skipped
        assert!(result.get("dns").is_none());
        assert!(!executor.traces[0].condition_matched);
    }

    // ══════════════════════════════════════════════════════════
    // 多 Patch 顺序执行测试
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_multiple_patches_execute_in_order() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"port": 53}});
        let patches = vec![
            make_deep_merge_patch("dns", serde_json::json!({"enable": true})),
            make_deep_merge_patch("dns", serde_json::json!({"enhanced-mode": "fake-ip"})),
        ];
        let result = executor.execute_owned(config, &patches).unwrap();
        assert_eq!(result["dns"]["port"], 53);
        assert_eq!(result["dns"]["enable"], true);
        assert_eq!(result["dns"]["enhanced-mode"], "fake-ip");
        assert_eq!(executor.traces.len(), 2);
    }

    #[test]
    fn test_execute_clears_previous_traces() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_deep_merge_patch("a", serde_json::json!(1));
        executor
            .execute_owned(config.clone(), std::slice::from_ref(&patch))
            .unwrap();
        assert_eq!(executor.traces.len(), 1);

        // Second execution should clear previous traces
        executor.execute_owned(config.clone(), &[patch]).unwrap();
        assert_eq!(executor.traces.len(), 1);
    }

    // ══════════════════════════════════════════════════════════
    // 边界测试 — 刁难、临界、对抗性情况
    // ══════════════════════════════════════════════════════════

    fn make_filter_patch(path: &str, expr: &str) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Filter {
                expr: CompiledPredicate::new(expr, vec![]),
            },
            serde_json::Value::Null,
        )
    }

    fn make_transform_patch(path: &str, expr: &str) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Transform {
                expr: CompiledPredicate::new(expr, vec![]),
            },
            serde_json::Value::Null,
        )
    }

    fn make_remove_patch(path: &str, expr: &str) -> Patch {
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            PatchOp::Remove {
                expr: CompiledPredicate::new(expr, vec![]),
            },
            serde_json::Value::Null,
        )
    }

    fn make_composite_patch(path: &str, sub_ops: Vec<SubOp>) -> Patch {
        let primary_op = sub_ops
            .first()
            .map(|s| s.op.clone())
            .unwrap_or(PatchOp::DeepMerge);
        let primary_val = sub_ops
            .first()
            .map(|s| s.value.clone())
            .unwrap_or(serde_json::Value::Null);
        Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::Global,
            path.to_string(),
            primary_op,
            primary_val,
        )
        .with_sub_ops(sub_ops)
        .unwrap()
    }

    /// 1. 空 patches 列表执行，验证配置不变
    #[test]
    fn test_empty_patches_execution() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"enable": true}, "rules": ["MATCH,DIRECT"]});
        let original = config.clone();
        let result = executor.execute_owned(config, &[]).unwrap();
        assert_eq!(result, original, "空 patches 列表不应修改配置");
        assert!(executor.traces.is_empty(), "空 patches 应产生 0 条 trace");
    }

    /// 2. 嵌套路径的深度合并（如 "dns.nameserver"）
    #[test]
    fn test_nested_path_deep_merge() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "dns": {
                "enable": true,
                "port": 53
            }
        });
        let patch =
            make_deep_merge_patch("dns.nameserver", serde_json::json!(["8.8.8.8", "1.1.1.1"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["dns"]["enable"], true, "原有字段应保留");
        assert_eq!(result["dns"]["port"], 53, "原有字段应保留");
        assert_eq!(result["dns"]["nameserver"].as_array().unwrap().len(), 2);
        assert_eq!(result["dns"]["nameserver"][0], "8.8.8.8");
        assert_eq!(result["dns"]["nameserver"][1], "1.1.1.1");
    }

    /// 3. 对不存在的路径执行 override，验证创建该路径
    #[test]
    fn test_override_on_nonexistent_path() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"enable": true}});
        let patch = make_override_patch("tun.stack", serde_json::json!("mixed"));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["tun"]["stack"], "mixed", "不存在的嵌套路径应被创建");
        assert_eq!(result["dns"]["enable"], true, "无关字段不受影响");
    }

    /// 4. 过滤器匹配所有元素，验证无删除
    #[test]
    fn test_filter_all_match() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "a", "type": "ss"},
                {"name": "b", "type": "ss"},
                {"name": "c", "type": "ss"}
            ]
        });
        let patch = make_filter_patch("proxies", "p.type == 'ss'");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["proxies"].as_array().unwrap().len(),
            3,
            "所有元素都匹配，不应删除"
        );
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 0, "removed 应为 0");
        assert_eq!(trace.summary.total_after, 3);
    }

    /// 5. 过滤器不匹配任何元素，验证全部删除
    #[test]
    fn test_filter_none_match() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "a", "type": "ss"},
                {"name": "b", "type": "vmess"}
            ]
        });
        let patch = make_filter_patch("proxies", "p.type == 'trojan'");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["proxies"].as_array().unwrap().len(),
            0,
            "无匹配应全部删除"
        );
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 2, "removed 应为 2");
    }

    /// 6. 对空数组执行 filter，验证无 panic
    #[test]
    fn test_filter_empty_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"proxies": []});
        let patch = make_filter_patch("proxies", "p.type == 'ss'");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["proxies"].as_array().unwrap().len(), 0);
        let trace = &executor.traces[0];
        assert!(trace.condition_matched);
        assert_eq!(trace.summary.removed, 0, "空数组 filter 不应有删除");
    }

    /// 7. 对空数组执行 transform，验证无 panic
    #[test]
    fn test_transform_empty_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"proxies": []});
        let patch = make_transform_patch("proxies", "{...p, tagged: true}");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["proxies"].as_array().unwrap().len(), 0);
        let trace = &executor.traces[0];
        assert!(trace.condition_matched);
        assert_eq!(trace.summary.modified, 0, "空数组 transform 不应有修改");
    }

    /// 8. 对不存在的数组执行 prepend，验证跳过
    #[test]
    fn test_prepend_to_nonexistent_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_prepend_patch("rules", serde_json::json!(["RULE-A"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert!(result.get("rules").is_none(), "不存在的路径不应被创建");
        assert_eq!(
            executor.traces[0].summary.added, 0,
            "路径不存在时 added 应为 0"
        );
    }

    /// 9. 对不存在的数组执行 append，验证跳过
    #[test]
    fn test_append_to_nonexistent_array() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({});
        let patch = make_append_patch("rules", serde_json::json!(["RULE-A"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert!(result.get("rules").is_none(), "不存在的路径不应被创建");
    }

    /// 10. 验证 guarded field 的 prepend 被阻止
    #[test]
    fn test_guarded_field_prepend_blocked() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"external-controller": "127.0.0.1:9090"});
        let patch = make_prepend_patch("external-controller", serde_json::json!(["0.0.0.0:9090"]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        // Guarded field — prepend should be skipped (not an array, so no-op anyway)
        assert_eq!(
            result["external-controller"], "127.0.0.1:9090",
            "guarded field 不应被修改"
        );
        // Guarded field 的非 DeepMerge/Override 操作被跳过，
        // trace 的 condition_matched 仍为 true（表示 patch 被处理了，但操作被拦截）
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.added, 0, "不应添加任何元素");
        assert_eq!(trace.summary.removed, 0, "不应删除任何元素");
    }

    /// 11. 验证 guarded field 的 filter 被阻止
    #[test]
    fn test_guarded_field_filter_blocked() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"secret": "my-secret"});
        let patch = make_filter_patch("secret", "true");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["secret"], "my-secret", "guarded field 不应被修改");
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 0, "不应删除任何元素");
    }

    /// 12. null 值触发 default 注入
    #[test]
    fn test_set_default_null_triggers() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"mode": null});
        let patch = make_set_default_patch("mode", serde_json::json!("rule"));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["mode"], "rule", "null 值应触发 default 注入");
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.modified, 1, "应记录 1 次修改");
    }

    /// 13. 空数组不触发 default
    #[test]
    fn test_set_default_empty_array_no_trigger_boundary() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"proxies": []});
        let patch =
            make_set_default_patch("proxies", serde_json::json!([{"name": "default-proxy"}]));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["proxies"].as_array().unwrap().len(),
            0,
            "空数组不应触发 default"
        );
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.modified, 0);
    }

    /// 14. 空对象不触发 default
    #[test]
    fn test_set_default_empty_object_no_trigger_boundary() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {}});
        let patch = make_set_default_patch("dns", serde_json::json!({"enable": true}));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["dns"].as_object().unwrap().len(),
            0,
            "空对象不应触发 default"
        );
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.modified, 0);
    }

    /// 15. 已有值时不覆盖
    #[test]
    fn test_set_default_existing_value_no_override_boundary() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"log-level": "warning"});
        let patch = make_set_default_patch("log-level", serde_json::json!("info"));
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(result["log-level"], "warning", "已有值不应被覆盖");
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.modified, 0);
    }

    /// 16. remove 删除所有元素
    #[test]
    fn test_remove_all_elements() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "a", "type": "ss"},
                {"name": "b", "type": "ss"},
                {"name": "c", "type": "ss"}
            ]
        });
        let patch = make_remove_patch("proxies", "p.type == 'ss'");
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["proxies"].as_array().unwrap().len(),
            0,
            "所有元素应被删除"
        );
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 3);
        assert_eq!(trace.summary.total_after, 0);
    }

    /// 17. 复合操作 filter + prepend
    #[test]
    fn test_composite_filter_prepend() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "hk-1", "type": "ss"},
                {"name": "us-old", "type": "vmess"},
                {"name": "hk-2", "type": "ss"}
            ]
        });
        let sub_ops = vec![
            SubOp {
                op: PatchOp::Filter {
                    expr: CompiledPredicate::new("p.type == 'ss'", vec![]),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Prepend,
                value: serde_json::json!([{"name": "manual", "type": "ss", "server": "1.2.3.4", "port": 443}]),
            },
        ];
        let patch = make_composite_patch("proxies", sub_ops);
        let result = executor.execute_owned(config, &[patch]).unwrap();
        let proxies = result["proxies"].as_array().unwrap();
        // filter 保留 2 个 ss 节点 + prepend 添加 1 个 = 3
        assert_eq!(proxies.len(), 3, "filter 保留 2 个 + prepend 添加 1 个 = 3");
        assert_eq!(proxies[0]["name"], "manual", "prepend 的元素应在头部");
        assert_eq!(proxies[1]["name"], "hk-1");
        assert_eq!(proxies[2]["name"], "hk-2");
        // filter 和 prepend 各产生一条 trace
        assert_eq!(executor.traces.len(), 2);
    }

    /// 18. 复合操作 filter + remove + transform
    #[test]
    fn test_composite_filter_remove_transform() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "hk-1", "type": "ss"},
                {"name": "us-1", "type": "vmess"},
                {"name": "hk-2", "type": "ss"},
                {"name": "jp-old", "type": "ss"},
                {"name": "us-2", "type": "vmess"}
            ]
        });
        let sub_ops = vec![
            SubOp {
                op: PatchOp::Filter {
                    expr: CompiledPredicate::new("p.type == 'ss'", vec![]),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Remove {
                    expr: CompiledPredicate::new("p.name.includes('old')", vec![]),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Transform {
                    expr: CompiledPredicate::new("{...p, name: '[HK] ' + p.name}", vec![]),
                },
                value: serde_json::Value::Null,
            },
        ];
        let patch = make_composite_patch("proxies", sub_ops);
        let result = executor.execute_owned(config, &[patch]).unwrap();
        let proxies = result["proxies"].as_array().unwrap();
        // filter: 保留 3 个 ss → remove: 移除 jp-old → 剩 2 个 → transform
        assert_eq!(proxies.len(), 2);
        assert!(proxies[0]["name"].as_str().unwrap().starts_with("[HK]"));
        assert!(proxies[1]["name"].as_str().unwrap().starts_with("[HK]"));
        // 3 sub-operations → 3 traces
        assert_eq!(executor.traces.len(), 3);
    }

    /// 19. 条件不匹配时跳过 patch
    #[test]
    fn test_condition_mismatch_skip() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({"dns": {"enable": false}});
        // Profile scope without matching profile_name in context → skip
        let patch = Patch::new(
            PatchSource {
                kind: SourceKind::Builtin,
                file: None,
                line: None,
                plugin_id: None,
            },
            Scope::profile("nonexistent-profile"),
            "dns".to_string(),
            PatchOp::DeepMerge,
            serde_json::json!({"enable": true}),
        );
        let result = executor.execute_owned(config, &[patch]).unwrap();
        assert_eq!(
            result["dns"]["enable"], false,
            "条件不匹配时 patch 应被跳过"
        );
        assert!(!executor.traces[0].condition_matched);
    }

    /// 20. 验证 trace 数量和统计正确性
    #[test]
    fn test_execution_trace_counts() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "rules": ["A", "B", "C"],
            "proxies": [
                {"name": "p1", "type": "ss"},
                {"name": "p2", "type": "vmess"},
                {"name": "p3", "type": "ss"}
            ]
        });
        let patches = vec![
            make_append_patch("rules", serde_json::json!(["D"])),
            make_filter_patch("proxies", "p.type == 'ss'"),
            make_deep_merge_patch("dns", serde_json::json!({"enable": true})),
        ];
        let result = executor.execute_owned(config, &patches).unwrap();

        // 3 个 patch → 3 条 trace
        assert_eq!(executor.traces.len(), 3, "应有 3 条 trace");

        // Trace 0: Append — 添加 1 个元素
        assert_eq!(executor.traces[0].summary.added, 1);
        assert_eq!(executor.traces[0].summary.total_after, 4);
        assert!(result["rules"].as_array().unwrap().len() == 4);

        // Trace 1: Filter — 保留 2 个，移除 1 个
        assert_eq!(executor.traces[1].summary.removed, 1);
        assert_eq!(executor.traces[1].summary.total_after, 2);
        assert!(result["proxies"].as_array().unwrap().len() == 2);

        // Trace 2: DeepMerge — 新增 1 个键
        assert_eq!(executor.traces[2].summary.modified, 1);
        assert_eq!(result["dns"]["enable"], true);
    }

    /// test_filter_affected_items_content -- 验证 $filter 的 affected_items 记录了正确的被移除元素
    ///
    /// retain 模式下，被移除元素的 before 字段应包含原始值。
    #[test]
    fn test_filter_affected_items_content() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "a", "type": "ss", "server": "1.1.1.1"},
                {"name": "b", "type": "vmess", "server": "2.2.2.2"},
                {"name": "c", "type": "ss", "server": "3.3.3.3"},
                {"name": "d", "type": "trojan", "server": "4.4.4.4"},
            ]
        });
        let patch = make_filter_patch("proxies", "type == 'ss'");
        let result = executor.execute_owned(config, &[patch]).unwrap();

        // 应保留 2 个 ss 节点，移除 2 个
        let proxies = result["proxies"].as_array().unwrap();
        assert_eq!(proxies.len(), 2);

        // trace 应记录 2 个 removed items
        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 2);
        assert_eq!(trace.affected_items.len(), 2);

        // 验证被移除元素的 before 内容（before 存储元素的 name 字段值）
        let removed_names: Vec<&str> = trace
            .affected_items
            .iter()
            .filter_map(|item| item.before.as_deref())
            .collect();
        assert_eq!(
            removed_names,
            vec!["b", "d"],
            "affected_items 应精确记录被移除节点的 name"
        );
    }

    /// test_remove_affected_items_content -- 验证 $remove 的 affected_items 记录了正确的被移除元素
    #[test]
    fn test_remove_affected_items_content() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "alpha", "type": "ss", "server": "1.1.1.1"},
                {"name": "beta", "type": "vmess", "server": "2.2.2.2"},
                {"name": "gamma", "type": "trojan", "server": "3.3.3.3"},
                {"name": "delta", "type": "ss", "server": "4.4.4.4"},
            ]
        });
        let patch = make_remove_patch("proxies", "p.type == 'ss'");
        let result = executor.execute_owned(config, &[patch]).unwrap();

        let proxies = result["proxies"].as_array().unwrap();
        assert_eq!(proxies.len(), 2, "应移除 2 个 ss 节点，保留 2 个");

        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 2);
        assert_eq!(trace.affected_items.len(), 2);

        // 验证被移除节点的 name 精确匹配
        let removed_names: Vec<&str> = trace
            .affected_items
            .iter()
            .filter_map(|item| item.before.as_deref())
            .collect();
        assert_eq!(
            removed_names,
            vec!["alpha", "delta"],
            "affected_items 应精确记录被移除节点的 name"
        );
    }

    /// test_remove_partial_match -- $remove 部分匹配场景
    #[test]
    fn test_remove_partial_match() {
        let mut executor = PatchExecutor::new();
        let config = serde_json::json!({
            "proxies": [
                {"name": "keep-a", "type": "ss"},
                {"name": "remove-b", "type": "vmess"},
                {"name": "keep-c", "type": "trojan"},
            ]
        });
        let patch = make_remove_patch("proxies", "p.type == 'vmess'");
        let result = executor.execute_owned(config, &[patch]).unwrap();

        let proxies = result["proxies"].as_array().unwrap();
        assert_eq!(proxies.len(), 2, "应移除 1 条，保留 2 条");

        let trace = &executor.traces[0];
        assert_eq!(trace.summary.removed, 1);
        assert_eq!(trace.affected_items.len(), 1);

        // 验证被移除的是 remove-b（before 存储元素描述，非完整 JSON）
        let removed = &trace.affected_items[0];
        assert_eq!(removed.before.as_deref(), Some("remove-b"));
    }
}
