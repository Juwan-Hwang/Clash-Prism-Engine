//! # PrismExtension — Extension 主入口
//!
//! `PrismExtension<H>` 是 Extension 的主入口结构，持有 Host 引用并提供高层 API。
//! GUI 通过此结构调用 Prism Core 的完整编译管道。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use clash_prism_core::compiler::PatchCompiler;
use clash_prism_core::executor::{ExecutionContext, PatchExecutor};
use clash_prism_core::ir::Patch;
use clash_prism_dsl::DslParser;

use crate::annotation::{extract_rule_annotations, group_annotations};
use crate::host::{PrismEvent, PrismHost};
use crate::types::*;

/// Mutex lock 辅助：poison 时恢复内部数据而非 panic
fn lock_or_err<T>(mutex: &std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, String> {
    mutex.lock().map_err(|e| format!("内部锁异常: {}", e))
}

/// 将多个独立 Mutex 合并为单个 Mutex 保护的状态结构体，
/// 减少锁竞争和潜在的死锁风险，同时保证状态更新的原子性。
#[derive(Debug)]
struct ExtensionState {
    last_traces: Vec<clash_prism_core::trace::ExecutionTrace>,
    last_output: std::sync::Arc<serde_json::Value>,
    last_patches: Vec<Patch>,
    last_compile_time: Option<chrono::DateTime<chrono::Local>>,
    last_compile_success: bool,
    /// 标记自上次完整 apply() 后是否有用户通过 insert_rule() 插入了自定义规则。
    /// 当此标志为 true 时，is_prism_rule() 对无注解匹配的规则返回 `user_custom: true`，
    /// 而非简单地标记为非 Prism 规则，使调用方能区分"从未编译过"和"用户手动插入了规则"。
    user_rules_inserted: bool,
    last_annotations: Vec<crate::types::RuleAnnotation>,
    annotation_index: std::collections::HashMap<usize, usize>,
}

/// 文件级解析缓存（SHA-256 内容哈希 → parsed patches）
/// 独立于 ExtensionState，通过 Arc 共享给 watcher 线程，避免重复解析。
type ParseCache = std::collections::HashMap<String, (String, Vec<Patch>)>;

impl Default for ExtensionState {
    fn default() -> Self {
        Self {
            last_traces: Vec::new(),
            last_output: std::sync::Arc::new(serde_json::Value::Null),
            last_patches: Vec::new(),
            last_compile_time: None,
            last_compile_success: false,
            user_rules_inserted: false,
            last_annotations: Vec::new(),
            annotation_index: std::collections::HashMap::new(),
        }
    }
}

/// Watcher 编译结果 — 用于在 watcher 线程与主线程之间同步编译状态
#[derive(Debug, Clone)]
struct WatchResult {
    traces: Vec<clash_prism_core::trace::ExecutionTrace>,
    output: std::sync::Arc<serde_json::Value>,
    patches: Vec<Patch>,
    compile_time: chrono::DateTime<chrono::Local>,
    compile_success: bool,
    annotations: Vec<crate::types::RuleAnnotation>,
}

/// Prism Extension 主入口
///
/// 持有 Host 引用和内部状态，提供完整的 Prism 编译管道。
///
/// # 示例
///
/// ```rust,ignore
/// let ext = PrismExtension::new(my_host);
/// let result = ext.apply(ApplyOptions::default())?;
/// println!("Applied {} patches", result.stats.succeeded);
/// ```
pub struct PrismExtension<H: PrismHost> {
    host: Arc<H>,
    state: std::sync::Mutex<ExtensionState>,
    /// Watcher 线程编译结果共享状态，用于解决 watcher 创建独立实例导致的状态不同步问题
    watcher_result: Arc<std::sync::Mutex<Option<WatchResult>>>,
    /// 文件级解析缓存，通过 Arc 共享给 watcher 线程
    parse_cache: Arc<std::sync::Mutex<ParseCache>>,
    #[cfg(feature = "watcher")]
    watcher_running: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "watcher")]
    watcher_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl<H: PrismHost + 'static> PrismExtension<H> {
    pub fn new(host: H) -> Self {
        Self::new_host_only(Arc::new(host))
    }

    fn new_host_only(host: Arc<H>) -> Self {
        Self::new_with_shared(
            host,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(ParseCache::new())),
        )
    }

    /// 使用共享的 watcher_result 和 parse_cache 创建实例（用于 watcher 线程状态同步）
    fn new_with_shared(
        host: Arc<H>,
        watcher_result: Arc<std::sync::Mutex<Option<WatchResult>>>,
        parse_cache: Arc<std::sync::Mutex<ParseCache>>,
    ) -> Self {
        Self {
            host,
            state: std::sync::Mutex::new(ExtensionState::default()),
            watcher_result,
            parse_cache,
            #[cfg(feature = "watcher")]
            watcher_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "watcher")]
            watcher_thread: std::sync::Mutex::new(None),
        }
    }

    /// 执行完整的 Prism 编译管道
    ///
    /// 流程：读取配置 → 扫描文件 → DSL 解析 → 编译 → 拓扑排序 → 执行 → 写回 → 通知
    pub fn apply(&self, options: ApplyOptions) -> Result<ApplyResult, String> {
        // 编译失败时更新状态（仅标记失败，不更新编译时间）
        struct FailGuard<'a> {
            state: &'a std::sync::Mutex<ExtensionState>,
            succeeded: std::cell::Cell<bool>,
        }
        impl Drop for FailGuard<'_> {
            fn drop(&mut self) {
                // 仅在成功路径未执行时触发（即编译失败）
                if !self.succeeded.get()
                    && let Ok(mut s) = self.state.lock()
                {
                    s.last_compile_success = false;
                }
            }
        }
        let guard = FailGuard {
            state: &self.state,
            succeeded: std::cell::Cell::new(false),
        };

        // Step 1: 读取当前运行配置
        let config_str = self.host.read_running_config()?;
        let base_config: serde_json::Value =
            serde_yml::from_str(&config_str).map_err(|e| format!("配置解析失败: {}", e))?;

        // Step 2: 获取 Prism 工作目录并扫描文件
        let workspace = self.host.get_prism_workspace()?;

        // Step 3: 编译管道
        let (raw_output, traces, patches) =
            self.compile_pipeline(base_config, workspace.as_path(), &options)?;
        let output_config = std::sync::Arc::new(raw_output);

        // Step 4: 序列化输出配置
        let output_yaml =
            serde_yml::to_string(&*output_config).map_err(|e| format!("配置序列化失败: {}", e))?;

        // Step 5: 提取规则注解
        let rule_annotations = extract_rule_annotations(&traces, &output_config);

        // Step 6: 生成编译统计和 TraceView
        let stats = self.build_compile_stats(&traces);
        let trace_views = Self::build_trace_views(&traces);

        // Step 7: 可选验证
        if options.validate_output {
            match self.host.validate_config(&output_yaml) {
                Ok(false) => {
                    self.host.notify(PrismEvent::PatchFailed {
                        patch_id: "validation".to_string(),
                        error: "配置验证失败 (mihomo -t)".to_string(),
                    });
                    return Err("配置验证失败 (mihomo -t)".to_string());
                }
                Ok(true) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Host 未实现 validate_config，配置验证已跳过"
                    );
                }
            }
        }

        // Step 8: 通过 Host 写回配置
        let status = self.host.apply_config(&output_yaml)?;

        // Step 9: 通知前端
        self.host.notify(PrismEvent::ConfigReloaded {
            success: status.hot_reload_success,
            message: status.message.clone(),
        });

        // 更新内部状态（成功路径，取消 FailGuard 的失败标记）
        {
            let mut state = lock_or_err(&self.state)?;
            state.last_compile_time = Some(chrono::Local::now());
            state.last_compile_success = true;
            state.last_traces = traces.clone();
            state.last_output = output_config.clone();
            state.last_patches = patches.clone();
            state.user_rules_inserted = false;
            state.last_annotations = rule_annotations.clone();
            state.annotation_index = rule_annotations
                .iter()
                .enumerate()
                .map(|(i, a)| (a.index_in_output, i))
                .collect();
        }

        // 同步写入 watcher_result（watcher 线程和主线程共享此状态）
        match lock_or_err(&self.watcher_result) {
            Ok(mut wr) => {
                *wr = Some(WatchResult {
                    traces,
                    output: output_config,
                    patches,
                    compile_time: chrono::Local::now(),
                    compile_success: true,
                    annotations: rule_annotations.clone(),
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to lock watcher_result after successful apply");
            }
        }

        // 标记编译成功（FailGuard Drop 中检查此标记，不会设置失败状态）
        guard.succeeded.set(true);

        Ok(ApplyResult {
            output_config: output_yaml,
            stats,
            trace: trace_views,
            rule_annotations,
        })
    }

    /// 列出 Prism 管理的规则组
    pub fn list_rules(&self) -> Result<Vec<RuleGroup>, String> {
        let annotations = self.get_current_annotations()?;
        let workspace = self.host.get_prism_workspace()?;
        Ok(group_annotations(&annotations, workspace.as_path()))
    }

    /// 预览指定 patch 的规则变更
    pub fn preview_rules(&self, patch_id: &str) -> Result<RuleDiff, String> {
        let state = lock_or_err(&self.state)?;
        let trace = state
            .last_traces
            .iter()
            .find(|t| t.patch_id.as_str() == patch_id)
            .ok_or_else(|| format!("未找到 patch: {}", patch_id))?;

        Ok(RuleDiff {
            added: trace
                .affected_items
                .iter()
                .filter(|i| matches!(i.action, clash_prism_core::trace::TraceAction::Added))
                .filter_map(|i| i.after.clone())
                .collect(),
            removed: trace
                .affected_items
                .iter()
                .filter(|i| matches!(i.action, clash_prism_core::trace::TraceAction::Removed))
                .filter_map(|i| i.before.clone())
                .collect(),
            modified: trace
                .affected_items
                .iter()
                .filter(|i| matches!(i.action, clash_prism_core::trace::TraceAction::Modified))
                .filter_map(|i| i.after.clone())
                .collect(),
            position_changes: Vec::new(),
        })
    }

    /// 判断指定索引的规则是否由 Prism 管理
    ///
    /// 返回三种状态：
    /// 1. `is_prism: true` — 规则由 Prism Patch 注入（有注解匹配）
    /// 2. `is_prism: false, user_custom: true` — 用户通过 `insert_rule()` 手动插入的自定义规则
    /// 3. `is_prism: false, user_custom: false` — 非 Prism 规则（从未编译过或规则索引超出范围）
    pub fn is_prism_rule(&self, index: usize) -> Result<IsPrismRule, String> {
        let state = lock_or_err(&self.state)?;
        let user_inserted = state.user_rules_inserted;
        match state.annotation_index.get(&index) {
            Some(&ann_idx) => {
                let a = &state.last_annotations[ann_idx];
                Ok(IsPrismRule {
                    is_prism: true,
                    group: Some(a.source_file.clone()),
                    label: Some(a.source_label.clone()),
                    immutable: a.immutable,
                    user_custom: false,
                })
            }
            None => Ok(IsPrismRule {
                is_prism: false,
                group: None,
                label: None,
                immutable: false,
                user_custom: user_inserted,
            }),
        }
    }

    /// 在指定位置插入用户自定义规则
    ///
    /// 根据 `position` 策略决定插入位置，然后通过 Host 写回配置。
    /// 返回插入后的规则索引。
    ///
    /// 支持字符串格式规则（如 `"DOMAIN-SUFFIX,example.com,PROXY"`）和
    /// 对象格式规则（如 mihomo 的 `RULE-SET`）。
    ///
    /// **注意**：调用此方法会清空内部的 trace 数据和 patch 缓存，
    /// 因为单条规则插入无法准确重建完整的编译追踪信息。
    /// 这意味着在下次完整 `apply()` 之前，`preview_rules()` 等依赖
    /// trace 数据的功能将不可用。
    ///
    /// # 位置策略
    ///
    /// | 策略 | 说明 | 示例场景 |
    /// |------|------|----------|
    /// | [`BeforePrism`] | 插入到所有 Prism 管理规则之前 | 用户希望自己的规则优先匹配 |
    /// | [`AfterGroup(id)`] | 插入到指定 Prism 分组的最后一条规则之后 | 在「广告过滤」组后追加自定义规则 |
    /// | [`AfterPrism`] | 插入到所有 Prism 管理规则之后 | 用户规则作为兜底 |
    /// | [`Append`] | 追加到 `rules` 数组末尾 | 不关心位置，只求追加 |
    ///
    /// 当没有 Prism 规则注解时（如首次编译前），`BeforePrism` 等同于索引 0，
    /// `AfterPrism` 和 `AfterGroup` 等同于 `Append`。
    ///
    /// [`BeforePrism`]: RuleInsertPosition::BeforePrism
    /// [`AfterGroup(id)`]: RuleInsertPosition::AfterGroup
    /// [`AfterPrism`]: RuleInsertPosition::AfterPrism
    /// [`Append`]: RuleInsertPosition::Append
    pub fn insert_rule(
        &self,
        rule: serde_json::Value,
        position: &RuleInsertPosition,
    ) -> Result<usize, String> {
        let annotations = self.get_current_annotations()?;
        let mut output = self.get_current_output()?;

        let rules = output
            .get_mut("rules")
            .and_then(|v| v.as_array_mut())
            .ok_or("配置中没有 rules 字段")?;

        let insert_index = match position {
            RuleInsertPosition::BeforePrism => {
                annotations.first().map(|a| a.index_in_output).unwrap_or(0)
            }
            RuleInsertPosition::AfterGroup(group_id) => {
                match annotations
                    .iter()
                    .rev()
                    .find(|a| a.source_file == *group_id)
                {
                    Some(a) => a.index_in_output + 1,
                    None => {
                        tracing::warn!(
                            group_id = %group_id,
                            "insert_rule: AfterGroup 指定的分组不存在，已回退到 Append。\
                             请检查 group_id 是否正确，或先执行 apply() 生成注解"
                        );
                        rules.len()
                    }
                }
            }
            RuleInsertPosition::AfterPrism => annotations
                .last()
                .map(|a| a.index_in_output + 1)
                .unwrap_or(rules.len()),
            RuleInsertPosition::Append => rules.len(),
        };

        let clamped = insert_index.min(rules.len());
        rules.insert(clamped, rule);

        // Write back via host
        let output_yaml =
            serde_yml::to_string(&output).map_err(|e| format!("配置序列化失败: {}", e))?;
        self.host.apply_config(&output_yaml)?;

        // Invalidate internal state so getters re-derive from updated output.
        // last_patches / last_traces 无法从单条规则插入中准确重建，
        // 因此在此处清空它们。这是一个有意为之的设计决策：
        // insert_rule 是一个"低级"操作，它直接修改输出配置，
        // 但不执行完整的编译流程，因此 trace 信息在此刻已失效。
        // 下一次完整 apply() 将重新填充所有状态。
        //
        // 设置 user_rules_inserted 标志，使 is_prism_rule() 能区分
        // "无注解数据（从未编译）" 和 "用户手动插入了自定义规则"。
        if let Ok(mut state) = lock_or_err(&self.state) {
            state.last_output = std::sync::Arc::new(output);
            state.last_patches.clear();
            state.last_traces.clear();
            state.last_annotations.clear();
            state.annotation_index.clear();
            state.user_rules_inserted = true;
        } else {
            tracing::error!("insert_rule: 无法获取 state 锁，内部状态可能不一致");
        }
        // Also clear watcher shared state to keep it consistent
        if let Ok(mut wr) = lock_or_err(&self.watcher_result) {
            *wr = None;
        }

        Ok(clamped)
    }

    /// 便捷方法：插入字符串格式规则（如 "DOMAIN-SUFFIX,example.com,PROXY"）
    pub fn insert_rule_str(
        &self,
        rule_text: &str,
        position: &RuleInsertPosition,
    ) -> Result<usize, String> {
        self.insert_rule(serde_json::Value::String(rule_text.to_string()), position)
    }

    /// 启用/禁用规则组
    ///
    /// `group_id` 是文件名（如 `ad-filter.prism.yaml`）。
    /// 通过重命名文件实现：启用时去掉 `.disabled` 后缀，禁用时添加 `.disabled` 后缀。
    pub fn toggle_group(&self, group_id: &str, enabled: bool) -> Result<bool, String> {
        // 空字符串检查
        if group_id.is_empty() {
            return Err("group_id 不能为空字符串".into());
        }

        // 路径遍历防护
        if group_id.contains("..")
            || group_id.contains('\0')
            || group_id.contains('/')
            || group_id.contains('\\')
            || group_id.contains('%')
        {
            return Err(format!("非法的 group_id: {}", group_id));
        }

        let workspace = self.host.get_prism_workspace()?;

        // 对 workspace 做 canonicalize，消除符号链接，确保路径比较基于真实路径
        let workspace_canonical = workspace
            .canonicalize()
            .map_err(|e| format!("无法解析工作目录路径: {}", e))?;

        let patch_file = workspace.join(group_id);

        // 验证路径仍在 workspace 内（基于 canonicalize 后的真实路径比较）
        // patch_file 可能不存在（正常情况），因此对 patch_file 的父目录做 canonicalize，
        // 再拼接 group_id，确保路径组件不含 ".."（已在上方检查）
        let patch_resolved = workspace_canonical.join(group_id);
        let patch_str = patch_resolved.to_string_lossy();
        let ws_str = workspace_canonical.to_string_lossy();
        if !patch_str.starts_with(ws_str.as_ref()) {
            return Err("路径超出工作目录范围".into());
        }

        if enabled {
            // 启用：查找追加 .disabled 后缀的文件并重命名回去
            // 直接尝试 rename 而非先检查 exists()，避免 TOCTOU 竞态条件。
            let disabled = format!("{}.disabled", patch_file.display());
            match std::fs::rename(&disabled, &patch_file) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(format!("启用失败: {}", e)),
            }
        } else {
            // 禁用：在原文件名后追加 .disabled
            // 直接尝试 rename 而非先检查 exists()，避免 TOCTOU 竞态条件。
            let disabled = format!("{}.disabled", patch_file.display());
            match std::fs::rename(&patch_file, &disabled) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(format!("禁用失败: {}", e)),
            }
        }
    }

    /// 获取 Extension 运行状态
    pub fn status(&self) -> PrismStatus {
        let watching = self.is_watching();

        // 优先从 watcher_result 读取（watcher 线程编译后的最新状态）
        let (compile_time, compile_success, patch_count) = match lock_or_err(&self.watcher_result) {
            Ok(guard) => {
                if let Some(ref wr) = *guard {
                    (
                        Some(wr.compile_time.to_rfc3339()),
                        wr.compile_success,
                        wr.patches
                            .iter()
                            .map(|pp| pp.source.file.clone().unwrap_or_default())
                            .collect::<std::collections::HashSet<_>>()
                            .len(),
                    )
                } else {
                    self.status_from_local()
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to lock watcher_result in status(), falling back to local state");
                self.status_from_local()
            }
        };

        PrismStatus {
            watching,
            watching_count: if watching { 1 } else { 0 },
            last_compile_time: compile_time,
            last_compile_success: compile_success,
            patch_count,
            script_count: self.host.script_count().unwrap_or(0),
            plugin_count: self.host.plugin_count().unwrap_or(0),
        }
    }

    /// 是否正在监听文件变更
    pub fn is_watching(&self) -> bool {
        #[cfg(feature = "watcher")]
        {
            self.watcher_running
                .load(std::sync::atomic::Ordering::SeqCst)
        }
        #[cfg(not(feature = "watcher"))]
        {
            false
        }
    }

    /// 启动文件监听（在独立线程中运行，非阻塞）
    ///
    /// 如果已有监听在运行，会先停止旧的监听线程。
    /// 注意：此方法为同步方法，内部通过直接设置标志位和短暂等待来停止旧线程。
    ///
    /// ## 设计决策：Watcher 线程与主线程状态同步
    ///
    /// Watcher 线程创建独立的 `PrismExtension` 实例执行编译，
    /// 通过共享的 `watcher_result: Arc<Mutex<Option<WatchResult>>>` 将编译结果
    /// 同步回主线程。主线程的 `status()`、`get_trace()`、`get_stats()` 等方法
    /// 优先从 `watcher_result` 读取最新状态。
    ///
    /// 这是刻意的设计选择：Watcher 线程无法直接访问主线程的 `state` Mutex
    /// （因为 `PrismExtension` 不是 `Sync`），因此使用共享状态桥接。
    /// 优点是解耦（watcher 崩溃不影响主线程），缺点是存在短暂的状态不一致窗口。
    #[cfg(feature = "watcher")]
    pub fn start_watching(&self, debounce_ms: u64) -> Result<(), String> {
        // 停止旧的监听线程（带超时保护，避免旧线程卡死时无限阻塞）
        self.watcher_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut thread) = lock_or_err(&self.watcher_thread)
            && let Some(handle) = thread.take()
        {
            // 在独立线程中 join 旧 watcher，通过 channel 实现超时控制
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            // 超时设置为 5 秒：watcher 线程在 debounce 窗口内可能正在执行编译，
            // 给予足够时间让当前编译完成并优雅退出。
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(()) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        "旧 watcher 线程未在 5 秒内退出，已强制继续（旧线程将在后台自行终止）"
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // join 线程已结束（正常情况）
                }
            }
        }

        let workspace = self.host.get_prism_workspace()?;
        let host = self.host.clone();
        let running = self.watcher_running.clone();
        let shared_result = self.watcher_result.clone();
        let shared_cache = self.parse_cache.clone();
        running.store(true, std::sync::atomic::Ordering::SeqCst);

        let handle = std::thread::Builder::new()
            .name("prism-watcher".into())
            .spawn(move || {
                use notify::{Event as NotifyEvent, RecursiveMode, Watcher};

                let (tx, rx) = std::sync::mpsc::channel::<notify::Result<NotifyEvent>>();

                let mut watcher = match notify::RecommendedWatcher::new(
                    move |res| {
                        let _ = tx.send(res);
                    },
                    notify::Config::default(),
                ) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::error!("创建文件监听器失败: {}", e);
                        running.store(false, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                };

                if let Err(e) = watcher.watch(&workspace, RecursiveMode::Recursive) {
                    tracing::error!("注册监听目录失败: {}", e);
                    running.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }

                tracing::info!(dir = %workspace.display(), debounce_ms, "文件监听已启动");

                let debounce = std::time::Duration::from_millis(debounce_ms);
                let mut last_event = std::time::Instant::now()
                    .checked_sub(debounce)
                    .unwrap_or_else(std::time::Instant::now);

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(Ok(_)) => {
                            last_event = std::time::Instant::now();
                            while rx.recv_timeout(debounce).is_ok() {
                                last_event = std::time::Instant::now();
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("文件监听错误: {}", e);
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if last_event.elapsed() >= debounce {
                                // 使用共享的 watcher_result 创建实例，确保状态同步
                                let ext = PrismExtension::new_with_shared(
                                    host.clone(),
                                    shared_result.clone(),
                                    shared_cache.clone(),
                                );
                                match ext.apply(ApplyOptions::default()) {
                                    Ok(_) => host.notify(PrismEvent::WatcherEvent {
                                        file: String::new(),
                                        change_type: "auto-recompile".into(),
                                    }),
                                    Err(e) => host.notify(PrismEvent::PatchFailed {
                                        patch_id: "watcher".into(),
                                        error: e,
                                    }),
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }

                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                }

                tracing::info!("文件监听已停止");
                running.store(false, std::sync::atomic::Ordering::SeqCst);
            })
            .map_err(|e| format!("启动监听线程失败: {}", e))?;

        if let Ok(mut thread) = lock_or_err(&self.watcher_thread) {
            *thread = Some(handle);
        }

        self.host.notify(PrismEvent::WatcherStatus {
            running: true,
            watching_count: 1,
        });
        Ok(())
    }

    /// 停止文件监听
    ///
    /// 设置停止标志后，在后台线程中 join 监听线程，不阻塞调用方。
    #[cfg(feature = "watcher")]
    pub fn stop_watching(&self) {
        self.watcher_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let handle = {
            let mut thread = match lock_or_err(&self.watcher_thread) {
                Ok(t) => t,
                Err(_) => return,
            };
            thread.take()
        };
        if let Some(handle) = handle {
            // 在后台线程中 join，避免阻塞调用方
            std::thread::spawn(move || {
                let _ = handle.join();
            });
        }
        self.host.notify(PrismEvent::WatcherStatus {
            running: false,
            watching_count: 0,
        });
    }

    #[cfg(not(feature = "watcher"))]
    pub fn start_watching(&self, _debounce_ms: u64) -> Result<(), String> {
        Err("文件监听功能需要启用 watcher feature".into())
    }

    #[cfg(not(feature = "watcher"))]
    pub fn stop_watching(&self) {}

    /// 获取执行追踪
    pub fn get_trace(&self, patch_id: &str) -> Result<TraceView, String> {
        // 优先从 watcher_result 读取
        match lock_or_err(&self.watcher_result) {
            Ok(guard) => {
                if let Some(ref wr) = *guard
                    && let Some(trace) = wr.traces.iter().find(|t| t.patch_id.as_str() == patch_id)
                {
                    return Ok(Self::trace_to_view(trace));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to lock watcher_result in get_trace(), falling back to local state");
            }
        }

        let state = lock_or_err(&self.state)?;
        let trace = state
            .last_traces
            .iter()
            .find(|t| t.patch_id.as_str() == patch_id)
            .ok_or_else(|| format!("未找到 patch: {}", patch_id))?;

        Ok(Self::trace_to_view(trace))
    }

    /// 获取编译统计
    pub fn get_stats(&self) -> Result<CompileStats, String> {
        // 优先从 watcher_result 读取
        match lock_or_err(&self.watcher_result) {
            Ok(guard) => {
                if let Some(ref wr) = *guard {
                    return Ok(Self::build_compile_stats_from(&wr.traces));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to lock watcher_result in get_stats(), falling back to local state");
            }
        }

        let state = lock_or_err(&self.state)?;
        Ok(Self::build_compile_stats_from(&state.last_traces))
    }

    /// 生成完整的执行追踪文本报告（Trace Report）
    ///
    /// 包含：统计摘要 + 影响范围 + 逐条变更详情 + 品牌信息。
    /// 需要先调用 `apply()` 才有数据，否则返回空报告。
    pub fn trace_report(&self) -> Result<String, String> {
        use clash_prism_core::trace::TraceManager;

        let state = lock_or_err(&self.state)?;
        if state.last_traces.is_empty() {
            return Ok("No traces available. Run apply() first.".to_string());
        }

        let mut mgr = TraceManager::new();
        mgr.import(state.last_traces.clone(), state.last_patches.clone())
            .map_err(|e| format!("Failed to build trace report: {}", e))?;
        Ok(mgr.full_report())
    }

    /// 读取指定 profile 的原始 YAML
    pub fn read_raw_profile(&self, profile_id: &str) -> Result<String, String> {
        self.host.read_raw_profile(profile_id)
    }

    /// 列出所有 profile
    pub fn list_profiles(&self) -> Result<Vec<crate::host::ProfileInfo>, String> {
        self.host.list_profiles()
    }

    /// 获取核心信息
    pub fn get_core_info(&self) -> Result<crate::host::CoreInfo, String> {
        self.host.get_core_info()
    }

    /// 验证配置文件
    pub fn validate_config(&self, config: &str) -> Result<bool, String> {
        self.host.validate_config(config)
    }

    // ── 内部方法 ──

    /// 从本地状态读取编译信息（watcher_result 无数据时的回退路径）
    fn status_from_local(&self) -> (Option<String>, bool, usize) {
        let state = match lock_or_err(&self.state) {
            Ok(s) => s,
            Err(_) => return (None, false, 0),
        };
        let compile_time = state.last_compile_time.map(|dt| dt.to_rfc3339());
        let compile_success = state.last_compile_success;
        let patch_count = state
            .last_patches
            .iter()
            .map(|pp| pp.source.file.clone().unwrap_or_default())
            .collect::<std::collections::HashSet<_>>()
            .len();
        (compile_time, compile_success, patch_count)
    }

    fn compile_pipeline(
        &self,
        base_config: serde_json::Value,
        prism_dir: &Path,
        options: &ApplyOptions,
    ) -> Result<
        (
            serde_json::Value,
            Vec<clash_prism_core::trace::ExecutionTrace>,
            Vec<Patch>,
        ),
        String,
    > {
        let prism_files = self.scan_prism_files(prism_dir, options.skip_disabled_patches)?;
        if prism_files.is_empty() {
            return Ok((base_config, Vec::new(), Vec::new()));
        }

        // 获取当前解析缓存快照
        let cached_hashes: std::collections::HashMap<String, String> = {
            let cache = lock_or_err(&self.parse_cache)?;
            cache
                .keys()
                .map(|k| (k.clone(), cache[k].0.clone()))
                .collect()
        };

        let mut compiler = PatchCompiler::new();
        let mut new_cache_entries: Vec<(String, (String, Vec<Patch>))> = Vec::new();

        for (file_name, content) in &prism_files {
            // 计算文件内容 SHA-256 hash
            let hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(content.as_bytes());
                hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect()
            };

            // 缓存命中：复用上次解析的 Patches
            if let Some(cached_hash) = cached_hashes.get(file_name)
                && *cached_hash == hash
                && let Ok(cache) = lock_or_err(&self.parse_cache)
                && let Some((_, cached_patches)) = cache.get(file_name)
            {
                compiler
                    .register_patches(file_name.clone(), cached_patches.clone())
                    .map_err(|e| format!("Patch 注册错误 [{}]: {}", file_name, e))?;
                continue;
            }

            // 缓存未命中：重新解析
            let patches = DslParser::parse_str(content, Some(std::path::PathBuf::from(file_name)))
                .map_err(|e| format!("DSL 解析错误 [{}]: {}", file_name, e))?;
            compiler
                .register_patches(file_name.clone(), patches.clone())
                .map_err(|e| format!("Patch 注册错误 [{}]: {}", file_name, e))?;
            new_cache_entries.push((file_name.clone(), (hash, patches)));
        }

        // 更新解析缓存
        if !new_cache_entries.is_empty()
            && let Ok(mut cache) = lock_or_err(&self.parse_cache)
        {
            for (name, entry) in new_cache_entries {
                cache.insert(name, entry);
            }
        }

        let sorted_ids = compiler
            .resolve_dependencies()
            .map_err(|e| format!("依赖解析错误: {}", e))?;

        // O(1) 查找：构建 id → Patch 索引
        let all_patches = compiler.get_all_patches();
        let patch_index: HashMap<&str, &Patch> =
            all_patches.iter().map(|p| (p.id.as_str(), *p)).collect();

        let sorted_patches: Vec<&Patch> = sorted_ids
            .iter()
            .filter_map(|id| patch_index.get(id.as_str()).copied())
            .collect();

        let mut executor = PatchExecutor::with_context(ExecutionContext {
            profile_name: self.host.get_current_profile(),
            ..ExecutionContext::default()
        });
        let config = executor
            .execute(base_config, &sorted_patches)
            .map_err(|e| format!("Patch 执行错误: {}", e))?;

        let traces = std::mem::take(&mut executor.traces);
        // 收集 owned patches 用于返回（仅在缓存未命中时需要 clone）
        let owned_patches: Vec<Patch> = sorted_patches.into_iter().cloned().collect();
        Ok((config, traces, owned_patches))
    }

    fn scan_prism_files(
        &self,
        prism_dir: &Path,
        skip_disabled: bool,
    ) -> Result<Vec<(String, String)>, String> {
        if !prism_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries: Vec<_> = std::fs::read_dir(prism_dir)
            .map_err(|e| format!("读取 Prism 目录失败: {}", e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut files = Vec::new();
        for entry in entries {
            let path = entry.path();
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if !file_name.ends_with(".prism.yaml") && !file_name.ends_with(".prism.yml") {
                continue;
            }
            if skip_disabled && file_name.ends_with(".disabled") {
                continue;
            }

            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("读取文件失败 [{}]: {}", file_name, e))?;
            files.push((file_name, content));
        }
        Ok(files)
    }

    fn get_current_annotations(&self) -> Result<Vec<RuleAnnotation>, String> {
        // 优先从 watcher_result 读取缓存（watcher_result 存在说明至少成功过一次）
        if let Ok(guard) = lock_or_err(&self.watcher_result)
            && let Some(ref wr) = *guard
            && wr.compile_success
        {
            return Ok(wr.annotations.clone());
        }
        let state = lock_or_err(&self.state)?;
        if state.last_compile_success {
            return Ok(state.last_annotations.clone());
        }
        // 回退：从未成功编译过，重新计算
        Ok(extract_rule_annotations(
            &state.last_traces,
            &state.last_output,
        ))
    }

    fn get_current_output(&self) -> Result<serde_json::Value, String> {
        if let Ok(guard) = lock_or_err(&self.watcher_result)
            && let Some(ref wr) = *guard
        {
            return Ok((*wr.output).clone());
        }
        let state = lock_or_err(&self.state)?;
        Ok((*state.last_output).clone())
    }

    fn build_trace_views(traces: &[clash_prism_core::trace::ExecutionTrace]) -> Vec<TraceView> {
        traces.iter().map(Self::trace_to_view).collect()
    }

    fn trace_to_view(t: &clash_prism_core::trace::ExecutionTrace) -> TraceView {
        TraceView {
            patch_id: t.patch_id.as_str().to_string(),
            source_file: t.source.file.clone(),
            op_name: t.op.display_name().to_string(),
            duration_us: t.duration_us,
            condition_matched: t.condition_matched,
            summary: TraceSummaryView {
                added: t.summary.added,
                removed: t.summary.removed,
                modified: t.summary.modified,
                kept: t.summary.kept,
                total_before: t.summary.total_before,
                total_after: t.summary.total_after,
            },
            diff: TraceDiffView {
                added: t
                    .affected_items
                    .iter()
                    .filter(|i| matches!(i.action, clash_prism_core::trace::TraceAction::Added))
                    .filter_map(|i| i.after.clone())
                    .collect(),
                removed: t
                    .affected_items
                    .iter()
                    .filter(|i| matches!(i.action, clash_prism_core::trace::TraceAction::Removed))
                    .filter_map(|i| i.before.clone())
                    .collect(),
            },
        }
    }

    fn build_compile_stats(
        &self,
        traces: &[clash_prism_core::trace::ExecutionTrace],
    ) -> CompileStats {
        Self::build_compile_stats_from(traces)
    }

    fn build_compile_stats_from(
        traces: &[clash_prism_core::trace::ExecutionTrace],
    ) -> CompileStats {
        let total = traces.len();
        let succeeded = traces.iter().filter(|t| t.condition_matched).count();
        let total_added: usize = traces.iter().map(|t| t.summary.added).sum();
        let total_removed: usize = traces.iter().map(|t| t.summary.removed).sum();
        let total_modified: usize = traces.iter().map(|t| t.summary.modified).sum();
        let total_duration_us: u64 = traces.iter().map(|t| t.duration_us).sum();

        CompileStats {
            total_patches: total,
            succeeded,
            skipped: total - succeeded,
            total_added,
            total_removed,
            total_modified,
            total_duration_us,
            avg_duration_us: if total > 0 {
                total_duration_us / total as u64
            } else {
                0
            },
        }
    }
}

/// 判断规则是否由 Prism 管理的结果
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IsPrismRule {
    pub is_prism: bool,
    pub group: Option<String>,
    pub label: Option<String>,
    pub immutable: bool,
    /// 是否为用户通过 `insert_rule()` 手动插入的自定义规则。
    /// 仅当 `is_prism == false` 时此字段有意义。
    #[serde(default)]
    pub user_custom: bool,
}
