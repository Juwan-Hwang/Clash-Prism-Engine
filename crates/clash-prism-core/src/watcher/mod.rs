//! File Watcher — 文件系统监听与热重载（§4.6 watch 模式）
//!
//! 监听指定目录下的 `.prism.yaml` 文件变化，自动触发
//! DSL 解析 → Patch 执行 → 目标配置写入 → 内核重载 的完整管线。

//! ## 设计
//!
//! ```text
//! ┌──────────────────────┐
//! │  notify crate        │  ← 跨平台文件系统事件（inotify/FSEvents/kqueue/ReadDirectoryChangesW）
//! └──────────┬───────────┘
//!            │ 文件变化事件
//!            ▼
//! ┌──────────────────────┐
//! │  Debounce（300ms）   │  ← 合并短时间内的多次写入（编辑器保存可能触发多次事件）
//! └──────────┬───────────┘
//!            │ 防抖后触发
//!            ▼
//! ┌──────────────────────┐
//! │  Compile Pipeline    │  ← DslParser → PatchExecutor → TargetCompiler → atomic_write → notify_hot_reload
//! └──────────────────────┘
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::target::{HotReloadStrategy, TargetCompiler};

/// 文件变化事件（防抖后）
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// 变化的文件路径
    pub path: PathBuf,
    /// 变化类型
    pub kind: ChangeKind,
}

/// 变化类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 文件内容修改（或创建）
    Modified,
    /// 文件被删除
    Removed,
}

/// Watcher 配置
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// 监听的目录路径
    pub watch_dir: PathBuf,

    /// 输出目标配置文件路径
    pub output_path: PathBuf,

    /// 热重载策略
    pub hot_reload: HotReloadStrategy,

    /// 防抖时间（毫秒）。编辑器保存可能触发多次文件事件，
    /// 防抖窗口内的多次事件会被合并为一次触发。
    pub debounce_ms: u64,

    /// 是否在启动时立即执行一次编译
    pub compile_on_start: bool,

    /// 扫描时跳过的目录名称列表。
    /// 默认跳过隐藏目录（以 `.` 开头）、`node_modules` 和 `target`。
    pub skip_dirs: Vec<String>,
}

/// Default skip directories for file scanning.
const DEFAULT_SKIP_DIRS: &[&str] = &["node_modules", "target"];

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            watch_dir: PathBuf::from("."),
            output_path: PathBuf::from("./config.yaml"),
            hot_reload: HotReloadStrategy::None,
            debounce_ms: 300,
            compile_on_start: true,
            skip_dirs: DEFAULT_SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 编译结果统计
#[derive(Debug, Clone)]
pub struct CompileStats {
    /// 编译的 Patch 数量
    pub patch_count: usize,
    /// 编译耗时（微秒）
    pub duration_us: u64,
    /// 输出文件大小（字节）
    pub output_bytes: usize,
    /// 是否成功写入
    pub write_success: bool,
    /// 热重载结果（如果执行了重载）
    pub reload_detail: Option<String>,
}

impl std::fmt::Display for CompileStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} patches, {}μs, {} bytes written",
            if self.write_success { "OK" } else { "FAIL" },
            self.patch_count,
            self.duration_us,
            self.output_bytes,
        )?;
        if let Some(detail) = &self.reload_detail {
            write!(f, ", reload: {}", detail)?;
        }
        Ok(())
    }
}

/// 编译管线回调 — 用户可自定义编译逻辑
///
/// 收到防抖后的文件变化事件时调用。
/// 返回 `Ok(stats)` 表示编译成功，`Err` 表示编译失败。
pub trait WatchCallback: Send + 'static {
    /// 处理单个文件变化，执行完整的编译管线
    fn on_change(&mut self, event: &FileChangeEvent) -> Result<CompileStats, String>;

    /// 处理批量文件变化（启动时编译所有文件）。
    ///
    /// 默认实现逐个调用 `on_change`，但子类可以覆盖以实现
    /// 先收集所有 Patches 再统一执行的优化逻辑。
    fn on_batch_change(&mut self, events: &[FileChangeEvent]) -> Result<CompileStats, String> {
        let mut total_stats = CompileStats {
            patch_count: 0,
            duration_us: 0,
            output_bytes: 0,
            write_success: false,
            reload_detail: None,
        };
        let start = Instant::now();

        for event in events {
            match self.on_change(event) {
                Ok(stats) => {
                    total_stats.patch_count += stats.patch_count;
                    total_stats.output_bytes = stats.output_bytes;
                    total_stats.write_success = stats.write_success;
                    total_stats.reload_detail = stats.reload_detail;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %event.path.display(),
                        error = %e,
                        "批量编译中单个文件失败"
                    );
                }
            }
        }

        total_stats.duration_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
        Ok(total_stats)
    }
}

/// 默认编译管线回调
///
/// 执行标准管线：扫描目录 → 解析 DSL → 执行 Patch → 写入目标文件 → 热重载
pub struct DefaultPipeline {
    /// 目标编译器
    pub compiler: TargetCompiler,
    /// 输出路径
    pub output_path: PathBuf,
    /// 热重载策略
    pub hot_reload: HotReloadStrategy,
    /// 基础配置（可选）
    pub base_config: serde_json::Value,
    /// DSL 解析函数：(content, source_path) → Result<Vec<Patch>, String>
    /// 由调用方注入（通常是 clash_prism_dsl::DslParser::parse_str）
    pub parse_fn: fn(&str, Option<PathBuf>) -> Result<Vec<crate::ir::Patch>, String>,
    /// 复用 PatchExecutor 实例，避免每次 on_change 都重新分配。
    /// PatchExecutor 内部持有 trace 缓冲区和执行上下文，复用可减少开销。
    executor: crate::executor::PatchExecutor,
}

impl DefaultPipeline {
    /// 创建默认管线
    ///
    /// `base_config` 为基础配置，watcher 模式下每次编译以此为起点。
    /// 如果传入 `None`，将在首次编译时从 `output_path` 读取当前配置文件。
    pub fn new(
        compiler: TargetCompiler,
        output_path: PathBuf,
        hot_reload: HotReloadStrategy,
        parse_fn: fn(&str, Option<PathBuf>) -> Result<Vec<crate::ir::Patch>, String>,
        base_config: Option<serde_json::Value>,
    ) -> Self {
        let config = base_config.unwrap_or_else(|| {
            if !output_path.exists() {
                tracing::warn!(
                    path = %output_path.display(),
                    "输出配置文件不存在，将使用空配置作为基础。\
                     首次编译可能产生不完整的配置，请确认配置文件路径正确。"
                );
            }
            std::fs::read_to_string(&output_path)
                .ok()
                .and_then(|s| serde_yml::from_str(&s).ok())
                .unwrap_or(serde_json::json!({}))
        });
        Self {
            compiler,
            output_path,
            hot_reload,
            base_config: config,
            parse_fn,
            executor: crate::executor::PatchExecutor::new(),
        }
    }
}

impl WatchCallback for DefaultPipeline {
    fn on_change(&mut self, event: &FileChangeEvent) -> Result<CompileStats, String> {
        let start = Instant::now();

        if event.kind == ChangeKind::Removed {
            tracing::warn!(
                path = %event.path.display(),
                "文件已删除，跳过编译"
            );
            return Err("文件已删除".into());
        }

        // 1. 读取 DSL 文件
        let content = std::fs::read_to_string(&event.path)
            .map_err(|e| format!("读取文件失败 {}: {}", event.path.display(), e))?;

        // 2. 解析 DSL → Patch IR（通过注入的 parse_fn 回调）
        let patches = (self.parse_fn)(&content, Some(event.path.clone()))
            .map_err(|e| format!("DSL 解析失败: {}", e))?;

        let patch_count = patches.len();

        // 3. 执行 Patch（复用 executor 实例，execute() 内部会清理 traces）
        // 设计说明：base_config.clone() 是有意的设计选择。
        // PatchExecutor::execute() 需要 owned Value（mut config），因此每次编译必须 clone 一份独立副本。
        //
        // 性能影响：
        // - 对于典型配置（< 100KB），clone 开销可忽略（< 1ms）
        // - 对于超大型配置（> 1MB），clone 可能引入可观测延迟
        // - 未来优化方向：让 execute 接受 &mut Value 以避免 clone，
        //   或引入 COW（Copy-on-Write）数据结构减少深拷贝
        //
        // 当前选择 clone 的原因：
        // - 保证每次编译的独立性（前一次编译不会影响后一次）
        // - 代码简洁性优于微优化
        // - watcher 模式下编译频率受防抖窗口限制（默认 300ms），clone 不是瓶颈
        let final_config = self
            .executor
            .execute(self.base_config.clone(), &patches)
            .map_err(|e| format!("Patch 执行失败: {}", e))?;

        // 4. 原子写入目标文件
        let write_result = self.compiler.atomic_write(&final_config, &self.output_path);
        let (output_bytes, write_success) = match &write_result {
            Ok(bytes) => (*bytes, true),
            Err(_) => (0, false),
        };

        // 5. 热重载通知
        let reload_detail = if write_success {
            match self
                .compiler
                .notify_hot_reload(&self.output_path, self.hot_reload.clone())
            {
                Ok(result) => Some(result.detail),
                Err(e) => {
                    tracing::warn!("热重载通知失败: {}", e);
                    Some(format!("失败: {}", e))
                }
            }
        } else {
            None
        };

        let duration_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;

        Ok(CompileStats {
            patch_count,
            duration_us,
            output_bytes,
            write_success,
            reload_detail,
        })
    }

    ///
    /// 相比逐文件调用 `on_change`（每个文件独立执行管线导致后者覆盖前者），
    /// 批量模式将所有文件的 Patches 合并后统一执行，确保依赖关系正确。
    fn on_batch_change(&mut self, events: &[FileChangeEvent]) -> Result<CompileStats, String> {
        let start = Instant::now();

        // 1. 解析所有文件，收集所有 Patches
        let mut all_patches: Vec<crate::ir::Patch> = Vec::new();
        for event in events {
            if event.kind == ChangeKind::Removed {
                tracing::warn!(
                    path = %event.path.display(),
                    "文件已删除，跳过"
                );
                continue;
            }

            let content = std::fs::read_to_string(&event.path)
                .map_err(|e| format!("读取文件失败 {}: {}", event.path.display(), e))?;

            let patches = (self.parse_fn)(&content, Some(event.path.clone()))
                .map_err(|e| format!("DSL 解析失败 {}: {}", event.path.display(), e))?;

            all_patches.extend(patches);
        }

        let patch_count = all_patches.len();

        // 2. 一次性执行所有 Patches（复用 self.executor，traces 保存到 self.executor.traces）
        // NOTE: 同 on_change，base_config.clone() 是有意的设计选择（execute 需要 owned Value）。
        let final_config = self
            .executor
            .execute(self.base_config.clone(), &all_patches)
            .map_err(|e| format!("Patch 执行失败: {}", e))?;

        // 3. 原子写入目标文件
        let write_result = self.compiler.atomic_write(&final_config, &self.output_path);
        let (output_bytes, write_success) = match &write_result {
            Ok(bytes) => (*bytes, true),
            Err(_) => (0, false),
        };

        // 4. 热重载通知
        let reload_detail = if write_success {
            match self
                .compiler
                .notify_hot_reload(&self.output_path, self.hot_reload.clone())
            {
                Ok(result) => Some(result.detail),
                Err(e) => {
                    tracing::warn!("热重载通知失败: {}", e);
                    Some(format!("失败: {}", e))
                }
            }
        } else {
            None
        };

        let duration_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;

        Ok(CompileStats {
            patch_count,
            duration_us,
            output_bytes,
            write_success,
            reload_detail,
        })
    }
}

/// 文件监听器
///
/// 监听目录中的 `.prism.yaml` / `.prism.yml` 文件变化，
/// 通过防抖合并后触发编译管线回调。
pub struct FileWatcher {
    config: WatchConfig,
}

impl FileWatcher {
    /// 创建文件监听器
    pub fn new(config: WatchConfig) -> Result<Self, String> {
        if !config.watch_dir.exists() {
            return Err(format!("监听目录不存在: {}", config.watch_dir.display()));
        }

        Ok(Self { config })
    }

    /// 阻塞运行监听循环，直到收到 Ctrl+C (SIGINT)
    ///
    /// 使用 `notify` crate 进行跨平台文件系统监听，
    /// 配合 mpsc channel + 防抖逻辑合并短时间内的多次事件。
    pub fn run_with_callback(&self, mut callback: Box<dyn WatchCallback>) -> Result<(), String> {
        use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

        // 创建文件系统监听器
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                // 忽略发送错误（channel 满或接收端关闭）
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .map_err(|e| format!("创建文件监听器失败: {}", e))?;

        watcher
            .watch(&self.config.watch_dir, RecursiveMode::Recursive)
            .map_err(|e| format!("注册监听目录失败: {}", e))?;

        tracing::info!(
            dir = %self.config.watch_dir.display(),
            output = %self.config.output_path.display(),
            debounce_ms = self.config.debounce_ms,
            "👀 文件监听已启动（Ctrl+C 退出）"
        );

        // 设置 Ctrl+C (SIGINT) 信号处理，实现优雅退出
        // 使用 try_set_handler 以避免与已注册的 handler 冲突（如测试框架）
        let ctrlc_received = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctrlc_flag = ctrlc_received.clone();
        match ctrlc::try_set_handler(move || {
            ctrlc_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }) {
            Ok(()) => {
                tracing::debug!("Ctrl+C handler registered successfully");
            }
            Err(_) => {
                tracing::warn!(
                    "Ctrl+C handler already registered (e.g., by a test framework). \
                     Graceful shutdown via Ctrl+C may not work; use SIGTERM as alternative."
                );
            }
        }

        // 启动时立即编译一次
        if self.config.compile_on_start {
            self.compile_all_files(&mut *callback)?;
        }

        // 防抖状态
        let debounce_duration = Duration::from_millis(self.config.debounce_ms);
        /// Maximum debounce wait time: even if events keep arriving, force a flush
        /// after this duration to prevent indefinite event buffering.
        const MAX_DEBOUNCE_WAIT: Duration = Duration::from_secs(5);
        let mut pending_events: Vec<FileChangeEvent> = Vec::new();
        let mut last_trigger = Instant::now()
            .checked_sub(debounce_duration)
            .unwrap_or_else(Instant::now);
        let mut first_pending_event: Option<Instant> = None;

        // 事件循环
        loop {
            // 检查是否收到 Ctrl+C 信号
            if ctrlc_received.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("收到 Ctrl+C 信号，优雅退出");
                break;
            }

            // 等待下一个文件事件，带超时以检查防抖和信号
            let timeout = debounce_duration;
            match rx.recv_timeout(timeout) {
                Ok(Ok(event)) => {
                    // 过滤：只关注 .prism.yaml / .prism.yml 文件的修改/创建事件
                    let change_events = self.filter_event(&event);
                    for change_event in change_events {
                        if first_pending_event.is_none() {
                            first_pending_event = Some(Instant::now());
                        }
                        pending_events.push(change_event);
                    }
                    if !pending_events.is_empty() {
                        let now = Instant::now();
                        let max_wait_exceeded = first_pending_event
                            .map(|t| now.duration_since(t) >= MAX_DEBOUNCE_WAIT)
                            .unwrap_or(false);
                        if now.duration_since(last_trigger) >= debounce_duration
                            || max_wait_exceeded
                        {
                            // 防抖窗口外或超过最大等待时间，立即触发
                            self.flush_events(&mut pending_events, &mut *callback)?;
                            last_trigger = now;
                            first_pending_event = None;
                        }
                        // 否则继续收集事件，等待超时后统一触发
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("文件监听错误: {}", e);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 超时：如果有待处理事件，执行防抖触发
                    if !pending_events.is_empty() {
                        self.flush_events(&mut pending_events, &mut *callback)?;
                        last_trigger = Instant::now();
                        first_pending_event = None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::info!("监听通道已关闭，退出");
                    break;
                }
            }
        }

        Ok(())
    }

    /// 使用默认管线运行
    /// 注意：parse_fn 需要由调用方注入（通常在 CLI 层提供 clash_prism_dsl::DslParser::parse_str）
    pub fn run(
        &self,
        parse_fn: fn(&str, Option<PathBuf>) -> Result<Vec<crate::ir::Patch>, String>,
    ) -> Result<(), String> {
        let compiler = TargetCompiler::mihomo();
        let pipeline = DefaultPipeline::new(
            compiler,
            self.config.output_path.clone(),
            self.config.hot_reload.clone(),
            parse_fn,
            None,
        );
        self.run_with_callback(Box::new(pipeline))
    }

    /// 过滤文件系统事件，只保留 .prism.yaml / .prism.yml 的修改/创建
    ///
    /// notify 事件可能包含多个路径（如 hardlink 操作），遍历所有路径
    /// 生成对应的 FileChangeEvent 列表。
    fn filter_event(&self, event: &notify::Event) -> Vec<FileChangeEvent> {
        use notify::EventKind;

        let is_create_or_modify = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));

        let is_remove = matches!(event.kind, EventKind::Remove(_));

        if !is_create_or_modify && !is_remove {
            return vec![];
        }

        let kind = if is_remove {
            ChangeKind::Removed
        } else {
            ChangeKind::Modified
        };

        let mut result = vec![];
        for path in &event.paths {
            // 只监听 .prism.yaml / .prism.yml 文件
            // Precisely match the LAST ".prism" in the file stem to avoid
            // edge cases like "my.prism.extra.prism.yaml" matching incorrectly.
            let is_prism_file = path
                .extension()
                .map(|ext| ext == "yaml" || ext == "yml")
                .unwrap_or(false);

            let is_prism_file = is_prism_file
                && path
                    .file_stem()
                    .map(|stem| {
                        let s = stem.to_string_lossy();
                        // Must end with ".prism" and the part before ".prism" must be non-empty
                        if let Some(rest) = s.strip_suffix(".prism") {
                            !rest.is_empty() && !rest.contains(".prism")
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);

            if is_prism_file {
                result.push(FileChangeEvent {
                    path: path.clone(),
                    kind,
                });
            }
        }

        result
    }

    /// Execute debounced compilation using batch processing.
    fn flush_events(
        &self,
        events: &mut Vec<FileChangeEvent>,
        callback: &mut dyn WatchCallback,
    ) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }

        // Deduplicate: keep the last event per file path
        let mut last_events: HashMap<PathBuf, FileChangeEvent> = HashMap::new();
        for event in events.drain(..) {
            last_events.insert(event.path.clone(), event);
        }
        let unique: Vec<FileChangeEvent> = last_events.into_values().collect();

        tracing::info!(
            count = unique.len(),
            "detected {} file change(s), compiling as batch",
            unique.len()
        );

        for event in &unique {
            tracing::info!(path = %event.path.display(), "compiling: {}", event.path.display());
        }

        // This ensures all patches from all changed files are collected and
        // executed together, preserving cross-file dependencies.
        match callback.on_batch_change(&unique) {
            Ok(stats) => {
                tracing::info!("{}", stats);
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "batch compilation failed"
                );
            }
        }

        Ok(())
    }

    /// 扫描监听目录，编译所有 .prism.yaml 文件（启动时调用）
    fn compile_all_files(&self, callback: &mut dyn WatchCallback) -> Result<(), String> {
        tracing::info!("扫描目录: {}", self.config.watch_dir.display());

        let files = walkdir_prism_files(&self.config.watch_dir, &self.config.skip_dirs);

        if files.is_empty() {
            tracing::warn!("未找到 .prism.yaml 文件");
            return Ok(());
        }

        let events: Vec<FileChangeEvent> = files
            .iter()
            .map(|path| FileChangeEvent {
                path: path.clone(),
                kind: ChangeKind::Modified,
            })
            .collect();

        tracing::info!(
            count = events.len(),
            "批量初始编译: {} 个文件",
            events.len()
        );

        match callback.on_batch_change(&events) {
            Ok(stats) => {
                tracing::info!("{}", stats);
            }
            Err(e) => {
                tracing::warn!(error = %e, "批量初始编译失败");
            }
        }

        Ok(())
    }
}

/// 递归扫描目录中的 .prism.yaml / .prism.yml 文件
///
/// 使用迭代方式（Vec 作为栈）代替递归，避免深层嵌套时栈溢出。
///
/// 符号链接循环防护 — 使用 (device, inode) 对跟踪已访问的目录，
/// 遇到已访问的设备号+inode 组合时跳过，防止符号链接循环导致无限遍历。
/// 注意：在非 Unix 平台上（如 Windows），此防护退化为路径集合检查。
#[cfg(unix)]
fn walkdir_prism_files(dir: &Path, skip_dirs: &[String]) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let mut files = Vec::new();
    let mut dir_stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    // Track visited directories by (dev, inode) to detect symlink cycles
    let mut visited: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();

    while let Some(current_dir) = dir_stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&current_dir) {
            // Check for symlink cycle before processing entries
            if let Ok(meta) = std::fs::metadata(&current_dir) {
                let dev_ino = (meta.dev(), meta.ino());
                if !visited.insert(dev_ino) {
                    // Already visited this directory — symlink cycle detected
                    tracing::debug!(
                        path = %current_dir.display(),
                        "Skipping symlink cycle detected during directory walk"
                    );
                    continue;
                }
            }

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // 跳过隐藏目录（以 . 开头）和配置中指定的跳过目录
                    if let Some(name) = path.file_name() {
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with('.')
                            || skip_dirs.iter().any(|d| d == name_str.as_ref())
                        {
                            continue;
                        }
                    }
                    dir_stack.push(path);
                } else if path.is_file() {
                    let is_prism = path
                        .extension()
                        .map(|ext| ext == "yaml" || ext == "yml")
                        .unwrap_or(false)
                        && path
                            .file_stem()
                            .map(|stem| {
                                let s = stem.to_string_lossy();
                                // Same precise matching as filter_event
                                if let Some(rest) = s.strip_suffix(".prism") {
                                    !rest.is_empty() && !rest.contains(".prism")
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(false);

                    if is_prism {
                        files.push(path);
                    }
                }
            }
        }
    }

    // 按路径排序确保确定性
    files.sort();
    files
}

/// Non-Unix fallback: walk without inode-based cycle detection.
/// Uses path-based cycle detection as a less reliable fallback.
///
/// ## 跨平台限制说明
///
/// 在非 Unix 平台（如 Windows）上，无法通过 (device, inode) 对检测符号链接环，
/// 因为 Windows 不提供 POSIX inode 语义。此回退实现使用以下策略：
///
/// 1. **canonicalize() 优先**：尝试获取路径的规范形式（解析所有符号链接），
///    如果两个不同路径解析到相同的规范路径，说明存在符号链接环。
/// 2. **原始路径回退**：如果 canonicalize() 失败（如悬挂符号链接），
///    回退到使用原始路径字符串进行去重。
///
/// **已知局限性**：
/// - canonicalize() 在遇到悬挂符号链接时会失败（返回 Err），
///   此时回退到原始路径比较，可能无法检测到某些符号链接环。
/// - Windows 上 junction points 和 symbolic links 的行为不同，
///   canonicalize() 会解析 junction points 但可能不解析所有 symlink 类型。
/// - 路径大小写不敏感的文件系统（如 Windows NTFS）上，
///   原始路径字符串比较可能因大小写差异而漏检。
///
/// **未来改进方向**：Rust 1.75+ 提供了 `std::fs::Metadata::file_id()` API，
/// 可在所有平台上获取文件唯一标识符（Unix inode / Windows file ID），
/// 替代当前基于路径的回退检测，实现与 Unix 版本一致的符号链接环检测能力。
/// 参见：https://doc.rust-lang.org/std/fs/struct.Metadata.html#method.file_id
///
/// 在安全性要求较高的场景中，建议在 Unix 平台上运行 Prism Engine
/// 以获得完整的 inode 级符号链接环检测。
#[cfg(not(unix))]
fn walkdir_prism_files(dir: &Path, skip_dirs: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dir_stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    // Track visited paths: prefer canonical path, fall back to raw path string
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    while let Some(current_dir) = dir_stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&current_dir) {
            // but fall back to raw path on dangling symlinks instead of skipping.
            let canonical = current_dir.canonicalize().ok();
            let visit_key = canonical.as_ref().unwrap_or(&current_dir);
            if !visited.insert(visit_key.to_path_buf()) {
                tracing::debug!(
                    path = %current_dir.display(),
                    "Skipping symlink cycle detected during directory walk"
                );
                continue;
            }

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name() {
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with('.')
                            || skip_dirs.iter().any(|d| d == name_str.as_ref())
                        {
                            continue;
                        }
                    }
                    dir_stack.push(path);
                } else if path.is_file() {
                    let is_prism = path
                        .extension()
                        .map(|ext| ext == "yaml" || ext == "yml")
                        .unwrap_or(false)
                        && path
                            .file_stem()
                            .map(|stem| {
                                let s = stem.to_string_lossy();
                                if let Some(rest) = s.strip_suffix(".prism") {
                                    !rest.is_empty() && !rest.contains(".prism")
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(false);

                    if is_prism {
                        files.push(path);
                    }
                }
            }
        }
    }

    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walkdir_finds_prism_files() {
        let dir = std::env::temp_dir().join("prism_test_walkdir");
        let _ = std::fs::create_dir_all(&dir);

        // 创建测试文件
        let prism_file = dir.join("01-base.prism.yaml");
        let prism_yml = dir.join("02-rules.prism.yml");
        let other_file = dir.join("config.yaml");
        let _ = std::fs::write(&prism_file, "test");
        let _ = std::fs::write(&prism_yml, "test");
        let _ = std::fs::write(&other_file, "test");

        let files = walkdir_prism_files(&dir, &[]);

        assert_eq!(files.len(), 2);
        assert!(files.contains(&prism_file));
        assert!(files.contains(&prism_yml));
        assert!(!files.contains(&other_file));

        // 清理
        let _ = std::fs::remove_file(&prism_file);
        let _ = std::fs::remove_file(&prism_yml);
        let _ = std::fs::remove_file(&other_file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_walkdir_skips_hidden_dirs() {
        let dir = std::env::temp_dir().join("prism_test_hidden");
        let hidden = dir.join(".git");
        let _ = std::fs::create_dir_all(&hidden);

        let prism_file = hidden.join("test.prism.yaml");
        let _ = std::fs::write(&prism_file, "test");

        let files = walkdir_prism_files(&dir, &[]);
        assert!(files.is_empty());

        let _ = std::fs::remove_file(&prism_file);
        let _ = std::fs::remove_dir(&hidden);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_watch_config_default() {
        let config = WatchConfig::default();
        assert_eq!(config.debounce_ms, 300);
        assert!(config.compile_on_start);
        assert_eq!(config.hot_reload, HotReloadStrategy::None);
    }
}
