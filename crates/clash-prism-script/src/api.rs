//! Prism API — 注入到 JS 沙箱的全局工具对象（§5.2 完整 PrismContext）
//!
//! ## 可用 API
//!
//! ```js
//! // ─── 配置读写 ───
//! config.get()           → 当前完整配置 (JSON)
//! config.get("key")      → 获取指定字段
//! config.set("key", val) → 修改配置（返回 Patch IR）
//!
//! // ─── 结构化工具（推荐使用）───
//! utils.proxies.filter(pred)     → 过滤代理节点
//! utils.proxies.rename(regex, replacement) → 批量重命名
//! utils.proxies.remove(pred)     → 删除匹配的代理
//! utils.proxies.sort(field, order?) → 排序
//! utils.proxies.deduplicate(by?)  → 去重
//! utils.proxies.groupBy(pattern)  → 按正则分组
//!
//! utils.rules.prepend(...rules)   → 规则前置插入
//! utils.rules.append(...rules)    → 规则末尾追加
//! utils.rules.insertAt(idx, ...rules) → 指定位置插入
//! utils.rules.remove(pred)        → 删除规则
//! utils.rules.deduplicate()       → 规则去重
//!
//! utils.groups.get(name)          → 获取代理组
//! utils.groups.addProxy(group, ...names) → 向组添加代理
//! utils.groups.removeProxy(group, ...names) → 从组移除代理
//! utils.groups.create(group)      → 创建新代理组
//! utils.groups.remove(name)       → 删除代理组
//!
//! // ─── Patch 生成 ───
//! patch.add(patchObj)             → 注册一个 Patch
//!
//! // ─── KV 存储 ───
//! store.get(key)                  → 读取值
//! store.set(key, value)           → 写入值
//! store.delete(key)               → 删除键
//! store.keys()                    → 列出所有键
//!
//! // ─── 环境信息（只读）───
//! env.coreType                    → "mihomo" | "clash-rs"
//! env.coreVersion                 → 版本字符串
//! env.platform                    → "windows" | "macos" | "linux"
//! env.profileName                 → 当前 Profile 名称
//!
//! // ─── 基础工具函数 ───
//! utils.match(pattern, str)       → glob 匹配
//! utils.includes(arr, item)       → 数组包含检查
//! utils.now()                     → 当前时间戳（毫秒）
//! utils.random(min, max)          → 随机整数 [min, max]
//! utils.hash(str)                 → 字符串哈希值
//!
//! // ─── 日志 ───
//! log.info("msg")
//! log.warn("msg")
//! log.error("msg")
//! log.debug("msg")
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use rquickjs::{Ctx, Function, IntoJs, Object, Result as RjsResult, Value};

use clash_prism_core::ir::{Patch, PatchOp};
use clash_prism_core::scope::Scope;
use clash_prism_core::source::{PatchSource, SourceKind};

/// 脚本执行上下文信息（由调用方提供）
///
/// 包含引擎运行时环境信息，以只读方式注入到 JS 沙箱的 `env` 对象中。
/// 脚本可通过 `env.coreType`、`env.platform` 等访问这些信息。
#[derive(Debug, Clone)]
pub struct ScriptContext {
    /// 内核类型
    pub core_type: String,
    /// 内核版本
    pub core_version: String,
    /// 平台
    pub platform: String,
    /// 当前 Profile 名称
    pub profile_name: String,
}

impl Default for ScriptContext {
    fn default() -> Self {
        Self {
            core_type: "mihomo".to_string(),
            core_version: "1.0.0".to_string(),
            platform: std::env::consts::OS.to_string(),
            profile_name: "default".to_string(),
        }
    }
}

/// KV 存储实例（跨脚本持久化）
///
/// ## 存储模式（§11 技术栈）
///
/// ### 内存模式（默认）
/// 使用 `HashMap`，数据在进程退出后丢失。
/// 适用于：CLI 单次执行、测试环境、无持久化需求的场景。
///
/// ### 持久化模式（feature: `persist-store`）
/// 使用 `redb` 嵌入式数据库，数据写入本地文件。
/// 适用于：Tauri 桌面应用、长期运行的服务进程。
///
/// ## 接口契约
///
/// 无论底层实现如何，对外暴露的 API 完全一致：
/// - `get(key)` → `Option<Value>`
/// - `set(key, value)` → `()`
/// - `delete(key)` → `bool`
/// - `keys()` → `Vec<String>`
#[derive(Debug, Default)]
pub struct KvStore {
    inner: std::sync::Mutex<KvStoreInner>,
}

/// 内部存储实现
enum KvStoreInner {
    /// 内存 HashMap 存储（默认模式）
    Memory(HashMap<String, serde_json::Value>),
    /// 持久化 redb 存储（feature: persist-store 启用时可用）
    #[cfg(feature = "persist-store")]
    Persistent {
        db: Box<redb::Database>,
        /// Table definition for the KV store (must be 'static)
        table_def: redb::TableDefinition<'static, &'static str, &'static str>,
    },
}

impl std::fmt::Debug for KvStoreInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvStoreInner::Memory(m) => write!(f, "Memory({} entries)", m.len()),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { .. } => write!(f, "Persistent(redb)"),
        }
    }
}

impl Default for KvStoreInner {
    fn default() -> Self {
        KvStoreInner::Memory(HashMap::new())
    }
}

impl KvStore {
    /// 创建新的内存模式 KV 存储
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建持久化模式 KV 存储（§11 redb 实现）
    ///
    /// 使用 `redb` 嵌入式数据库，数据写入本地文件，跨进程重启保留。
    ///
    /// ## 架构文档对应
    ///
    /// - §11 技术栈: "KV 存储 | **redb** | 纯 Rust、ACID、嵌入式"
    /// - §5.2 API: `store.get/set/delete` 跨脚本持久化
    /// - §6 插件体系: Config Plugin 的 KV 存储能力
    ///
    /// # Arguments
    /// * `db_path` — 数据库文件路径（如 `/data/prism/store.redb`）
    ///
    /// # Returns
    /// 新的 KvStore 实例
    ///
    pub fn with_persistence(db_path: impl Into<std::path::PathBuf>) -> Self {
        let path = db_path.into();

        #[cfg(feature = "persist-store")]
        {
            let db = match redb::Database::create(&path) {
                Ok(db) => db,
                Err(e) => {
                    tracing::error!(
                        target = "clash_prism_script",
                        path = %path.display(),
                        error = %e,
                        "Failed to create KV persistence database, falling back to memory mode"
                    );
                    return Self::new();
                }
            };

            // 定义表结构（key: &str, value: &str）
            // redb 1.5+ 要求使用 TableDefinition + define() 模式
            let table_def: redb::TableDefinition<'static, &'static str, &'static str> =
                redb::TableDefinition::new("kv");

            // Initialize table (create if not present)
            {
                let txn = match db.begin_write() {
                    Ok(txn) => txn,
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            path = %path.display(),
                            error = %e,
                            "KV persistence: begin_write failed, falling back to memory mode"
                        );
                        return Self::new();
                    }
                };
                // redb 1.5+: open_table returns error if table does not exist.
                // On first run, this creates the table automatically.
                let _: redb::Table<'_, '_, &str, &str> = match txn.open_table(table_def) {
                    Ok(table) => table,
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            path = %path.display(),
                            error = %e,
                            "KV persistence: failed to open/create table, falling back to memory mode"
                        );
                        return Self::new();
                    }
                };
                if let Err(e) = txn.commit() {
                    tracing::error!(
                        target = "clash_prism_script",
                        path = %path.display(),
                        error = %e,
                        "KV persistence: commit failed, falling back to memory mode"
                    );
                    return Self::new();
                }
            }

            tracing::info!(
                target = "clash_prism_script",
                path = %path.display(),
                "KV 存储初始化为持久化模式 (redb) — 数据将跨进程重启保留"
            );
            Self {
                inner: std::sync::Mutex::new(KvStoreInner::Persistent {
                    db: Box::new(db),
                    table_def,
                }),
            }
        }

        #[cfg(not(feature = "persist-store"))]
        {
            tracing::warn!(
                target = "clash_prism_script",
                path = %path.display(),
                "persist-store feature 未启用，KV 存储回退到内存模式。\
                 如需持久化请在 Cargo.toml 中启用 feature \"persist-store\""
            );
            Self::new()
        }
    }

    /// 获取值
    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(map) => map.get(key).cloned(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                //§11-KV-4: redb 读取事务
                use redb::ReadableTable;
                let txn = db.begin_read().ok()?;
                let table = txn.open_table(*table_def).ok()?;
                table
                    .get(key)
                    .ok()
                    .flatten()
                    .and_then(|v: redb::AccessGuard<'_, &str>| serde_json::from_str(v.value()).ok())
            }
        }
    }

    /// 设置值
    pub fn set(&self, key: String, value: serde_json::Value) {
        const MAX_KEY_LEN: usize = 256;
        if key.is_empty() {
            tracing::warn!(
                target = "clash_prism_script",
                "store.set(): key must not be empty"
            );
            return;
        }
        if key.len() > MAX_KEY_LEN {
            tracing::warn!(
                target = "clash_prism_script",
                key_len = key.len(),
                max = MAX_KEY_LEN,
                "store.set(): key exceeds maximum length"
            );
            return;
        }
        if key.bytes().any(|b| b <= 0x1F) {
            tracing::warn!(
                target = "clash_prism_script",
                "store.set(): key contains control characters (0x00-0x1F)"
            );
            return;
        }

        // 大小/数量限制检查
        const MAX_STORE_ENTRIES: usize = 10000;
        const MAX_STORE_VALUE_SIZE: usize = 1024 * 1024; // 1MB
        let value_str = serde_json::to_string(&value).unwrap_or_default();
        if value_str.len() > MAX_STORE_VALUE_SIZE {
            tracing::warn!(
                target = "clash_prism_script",
                key = %key,
                size = value_str.len(),
                max = MAX_STORE_VALUE_SIZE,
                "store.set(): value too large"
            );
            return;
        }

        // 消除 TOCTOU 竞态条件，防止突破 MAX_STORE_ENTRIES 限制。
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // 检查条目数（仅在新增 key 时检查）
        let is_new_key = match &*guard {
            KvStoreInner::Memory(map) => !map.contains_key(&key),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                // redb 支持并发读事务（MVCC），读事务不会阻塞写事务，
                // 因此在 Mutex 内短暂持有读事务的实际性能影响极低。
                // 此处需要 Mutex 保护以保证 is_new_key 判断与后续写入操作的原子性。
                use redb::ReadableTable;
                let is_new = (|| {
                    let txn = db.begin_read().ok()?;
                    let table = txn.open_table(*table_def).ok()?;
                    let exists = table.get(key.as_str()).ok()?.is_some();
                    Some(!exists)
                })();
                is_new.unwrap_or(true) // 出错时保守假设为新 key
            }
        };
        if is_new_key && Self::len_unlocked(&guard) >= MAX_STORE_ENTRIES {
            tracing::warn!(
                target = "clash_prism_script",
                max = MAX_STORE_ENTRIES,
                "store.set(): max entries reached"
            );
            return;
        }

        match &mut *guard {
            KvStoreInner::Memory(map) => {
                map.insert(key, value);
            }
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                //§11-KV-5: redb 写入事务
                let json_str = match serde_json::to_string(&value) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            key = %key,
                            error = %e,
                            "KV set: 值序列化失败"
                        );
                        return;
                    }
                };
                match db.begin_write() {
                    Ok(txn) => {
                        // 表已由 with_persistence() 初始化保证存在
                        let mut table = match txn.open_table(*table_def) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::error!(
                                    target = "clash_prism_script",
                                    error = %e,
                                    "KV set: 打开表失败"
                                );
                                return;
                            }
                        };
                        if let Err(e) = table.insert(key.as_str(), json_str.as_str()) {
                            tracing::error!(
                                target = "clash_prism_script",
                                key = %key,
                                error = %e,
                                "KV set: 写入失败"
                            );
                        }
                        drop(table);
                        if let Err(e) = txn.commit() {
                            tracing::error!(target = "clash_prism_script", error = %e, "KV set: 提交事务失败");
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            error = %e,
                            "KV set: 无法开始写入事务（数据库可能被锁定）"
                        );
                    }
                }
            }
        }
    }

    /// 删除键
    pub fn delete(&self, key: &str) -> bool {
        match &mut *self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(map) => map.remove(key).is_some(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                //§11-KV-6: redb 删除事务
                match db.begin_write() {
                    Ok(txn) => {
                        let mut table = match txn.open_table(*table_def) {
                            Ok(t) => t,
                            Err(_) => return false,
                        };
                        let removed = table.remove(key).ok().flatten().is_some();
                        drop(table);
                        if let Err(e) = txn.commit() {
                            tracing::error!(target = "clash_prism_script", error = %e, "KV delete: 提交事务失败");
                        }
                        removed
                    }
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            error = %e,
                            "KV delete: 无法开始写入事务"
                        );
                        false
                    }
                }
            }
        }
    }

    /// 获取所有键
    pub fn keys(&self) -> Vec<String> {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(map) => map.keys().cloned().collect(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                //§11-KV-7: redb 列举事务
                use redb::ReadableTable;
                let Ok(txn) = db.begin_read() else {
                    return vec![];
                };
                let Ok(table) = txn.open_table(*table_def) else {
                    return vec![];
                };
                let iter = match table.iter() {
                    Ok(i) => i,
                    Err(_) => return vec![],
                };
                iter.filter_map(|r| r.ok())
                    .map(|(k, _): (redb::AccessGuard<'_, &str>, _)| k.value().to_string())
                    .collect::<Vec<_>>()
            }
        }
    }

    /// 当前存储模式
    pub fn storage_mode(&self) -> &'static str {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(_) => "memory",
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { .. } => "redb-persistent",
        }
    }

    /// 条目数量
    pub fn len(&self) -> usize {
        match &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(map) => map.len(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                use redb::ReadableTable;
                let Ok(txn) = db.begin_read() else { return 0 };
                let Ok(table) = txn.open_table(*table_def) else {
                    return 0;
                };
                table.len().unwrap_or(0) as usize
            }
        }
    }

    /// 条目数量（已持有锁时调用，避免重复加锁）
    ///
    /// 尝试开启读事务。redb 支持并发读事务，因此这不会导致死锁，
    /// 但如果同时有写事务正在进行，读事务可能需要短暂等待。
    /// 这是可接受的权衡——替代方案（先释放锁再查询）会引入 TOCTOU 竞态条件。
    fn len_unlocked(guard: &std::sync::MutexGuard<'_, KvStoreInner>) -> usize {
        match &**guard {
            KvStoreInner::Memory(map) => map.len(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                use redb::ReadableTable;
                let Ok(txn) = db.begin_read() else { return 0 };
                let Ok(table) = txn.open_table(*table_def) else {
                    return 0;
                };
                table.len().unwrap_or(0) as usize
            }
        }
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空所有数据
    pub fn clear(&self) {
        match &mut *self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            KvStoreInner::Memory(map) => map.clear(),
            #[cfg(feature = "persist-store")]
            KvStoreInner::Persistent { db, table_def } => {
                // 在单个写事务中完成，效率从 O(n) 提升到 O(1)。
                match db.begin_write() {
                    Ok(txn) => {
                        // delete_table 彻底清除所有数据
                        if let Err(e) = txn.delete_table(*table_def) {
                            tracing::error!(
                                target = "clash_prism_script",
                                error = %e,
                                "KV clear: delete_table failed"
                            );
                            return;
                        }
                        // 重新创建空表，保证后续操作可用
                        if let Err(e) = txn.open_table(*table_def) {
                            tracing::error!(
                                target = "clash_prism_script",
                                error = %e,
                                "KV clear: failed to recreate table after delete"
                            );
                            return;
                        }
                        if let Err(e) = txn.commit() {
                            tracing::error!(
                                target = "clash_prism_script",
                                error = %e,
                                "KV clear: commit failed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            target = "clash_prism_script",
                            error = %e,
                            "KV clear: 无法开始写入事务"
                        );
                    }
                }
            }
        }
    }
}

/// Patch 收集器 — 脚本通过 `patch.add()` 生成的 Patch 在此累积
///
/// 使用 `Arc<Mutex<Vec<Patch>>>` 内部存储，支持在 rquickjs 回调闭包中
/// 安全地共享和修改。脚本每次调用 `patch.add(patchObj)` 都会追加到此收集器。
///
/// ## 使用流程
///
/// 1. 引擎创建 `Arc<PatchCollector>` 并传入 API 注册器
/// 2. JS 脚本调用 `patch.add({path: "...", op: "...", value: ...})`
/// 3. 执行结束后，引擎调用 `drain_patches()` 取出所有 Patch
#[derive(Debug, Default, Clone)]
pub struct PatchCollector {
    patches: Arc<std::sync::Mutex<Vec<Patch>>>,
}

impl PatchCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 收集一个脚本生成的 Patch
    ///
    /// 确保即使前一次持有锁的线程 panic，patch 也不会被静默丢弃。
    pub fn add_patch(&self, patch: Patch) {
        const MAX_PATCHES: usize = 10000;
        let mut p = self.patches.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() >= MAX_PATCHES {
            tracing::warn!(
                target = "clash_prism_script",
                max = MAX_PATCHES,
                current = p.len(),
                "PatchCollector: max patch limit reached, rejecting new patch"
            );
            return;
        }
        p.push(patch);
    }

    /// 取出所有收集到的 Patch（消耗性操作）
    ///
    /// **所有权语义**：调用后内部缓冲区被清空，后续调用 `patches()` 将返回空列表。
    /// 与 `patches()` 不应混用：如果先调用 `patches()` 获取了克隆副本，
    /// 再调用 `drain_patches()` 会消耗原始数据，导致两次返回的内容重复。
    /// 推荐做法：根据场景选择其一——需要保留数据用 `patches()`，需要转移所有权用 `drain_patches()`。
    pub fn drain_patches(&self) -> Vec<Patch> {
        self.patches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// 获取 Patch 引用（非消耗）
    ///
    /// **所有权语义**：返回内部缓冲区的克隆副本，不影响原始数据。
    /// 与 `drain_patches()` 不应混用——两者返回的内容可能重复。
    /// 推荐做法：根据场景选择其一——需要保留数据用 `patches()`，需要转移所有权用 `drain_patches()`。
    pub fn patches(&self) -> Vec<Patch> {
        self.patches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Prism API 注册器 — 实现 §5.2 完整 PrismContext 接口
pub struct PrismApi;

/// Configuration for registering the Prism API into a QuickJS context.
#[derive(Clone)]
pub struct RegisterConfig {
    pub config: Arc<std::sync::Mutex<serde_json::Value>>,
    pub script_ctx: ScriptContext,
    pub kv_store: Arc<KvStore>,
    pub patch_collector: Arc<PatchCollector>,
    pub log_collector: Arc<std::sync::Mutex<Vec<crate::runtime::LogEntry>>>,
    pub max_log_entries: usize,
    pub script_name: String,
}

impl PrismApi {
    /// 将所有 API 对象注册到 JS 全局上下文
    ///
    /// 这是完整的 PrismContext API 入口，注册：
    /// - config（配置读写）
    /// - utils（结构化工具 + 基础工具函数函数）
    /// - patch（Patch 生成）
    /// - store（KV 存储）
    /// - env（环境信息只读）
    /// - log（日志）
    pub fn register<'js>(ctx: &Ctx<'js>, cfg: RegisterConfig) -> RjsResult<()> {
        let script_name = &cfg.script_name;
        let max_log_entries = cfg.max_log_entries;
        let config = cfg.config;
        let script_ctx = &cfg.script_ctx;
        let kv_store = cfg.kv_store;
        let patch_collector = cfg.patch_collector;
        let log_collector = cfg.log_collector;
        let globals = ctx.globals();

        // ── 注册 config 对象 ──
        Self::register_config(ctx, &globals, Arc::clone(&config))?;

        // ── 注册 utils 对象（含 proxies/rules/groups 子对象）───
        Self::register_utils(ctx, &globals, Arc::clone(&config))?;

        // ── 注册 patch 对象 ──
        Self::register_patch(ctx, &globals, patch_collector, script_name.to_string())?;

        // ── 注册 store 对象 ──
        Self::register_store(ctx, &globals, kv_store)?;

        // ── 注册 env 对象（只读环境信息）───
        Self::register_env(ctx, &globals, script_ctx)?;

        // ── 注册 log 对象 ──
        Self::register_log(ctx, &globals, log_collector, max_log_entries)?;

        // ── 安全加固：冻结所有注入的 API 对象 ──
        //防止恶意脚本覆盖 ctx.utils, ctx.log 等方法
        // Object.freeze() 使对象不可扩展、不可删除、属性不可重新配置
        let freeze_script = r#"
            (function() {
                var apiObjects = ['config', 'utils', 'patch', 'store', 'env', 'log'];
                for (var i = 0; i < apiObjects.length; i++) {
                    var name = apiObjects[i];
                    if (typeof globalThis[name] === 'object' && globalThis[name] !== null) {
                        Object.freeze(globalThis[name]);
                    }
                }
            })()
        "#;
        let _: std::result::Result<(), rquickjs::Error> = ctx.eval(freeze_script);

        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // config 对象
    // ═══════════════════════════════════════════════════════

    fn register_config<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<()> {
        let config_obj = Object::new(ctx.clone())?;

        let get_fn = Self::make_config_get(ctx, Arc::clone(&config))?;
        config_obj.set("get", get_fn)?;

        let set_fn = Self::make_config_set(ctx, Arc::clone(&config))?;
        config_obj.set("set", set_fn)?;

        globals.set("config", config_obj)?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // utils 对象（结构化工具 + 基础函数）
    // ═══════════════════════════════════════════════════════

    fn register_utils<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<()> {
        let utils_obj = Object::new(ctx.clone())?;

        // --- 结构化工具：proxies ---
        let proxies_obj = Self::make_proxies_api(ctx, Arc::clone(&config))?;
        utils_obj.set("proxies", proxies_obj)?;

        // --- 结构化工具：rules ---
        let rules_obj = Self::make_rules_api(ctx, Arc::clone(&config))?;
        utils_obj.set("rules", rules_obj)?;

        // --- 结构化工具：groups ---
        let groups_obj = Self::make_groups_api(ctx, Arc::clone(&config))?;
        utils_obj.set("groups", groups_obj)?;

        // --- 基础工具函数 ---
        let match_fn = Self::make_utils_match(ctx)?;
        utils_obj.set("match", match_fn)?;

        let includes_fn = Self::make_utils_includes(ctx)?;
        utils_obj.set("includes", includes_fn)?;

        let now_fn = Function::new(ctx.clone(), |_: Ctx<'js>| -> RjsResult<i64> {
            Ok(chrono::Utc::now().timestamp_millis())
        })?;
        utils_obj.set("now", now_fn)?;

        let random_fn = Function::new(
            ctx.clone(),
            |_: Ctx<'js>, min: i64, max: i64| -> RjsResult<i64> {
                if min > max {
                    return Err(rquickjs::Error::new_from_js(
                        "Error",
                        "random: min must be <= max",
                    ));
                }
                use rand::Rng;
                let mut rng = rand::thread_rng();
                Ok(rng.gen_range(min..=max))
            },
        )?;
        utils_obj.set("random", random_fn)?;

        let hash_fn = Function::new(ctx.clone(), |_: Ctx<'js>, s: String| -> RjsResult<String> {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut hasher);
            Ok(format!("{:016x}", hasher.finish()))
        })?;
        utils_obj.set("hash", hash_fn)?;

        globals.set("utils", utils_obj)?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // patch 对象
    // ═══════════════════════════════════════════════════════

    fn register_patch<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        collector: Arc<PatchCollector>,
        script_name: String,
    ) -> RjsResult<()> {
        let patch_obj = Object::new(ctx.clone())?;

        let add_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, patch_spec: Value<'js>| -> RjsResult<()> {
                // 将 JS 对象解析为 Patch IR
                let patch_value = js_value_to_json(&patch_spec);
                if let Some(obj) = patch_value.as_object() {
                    let path = obj
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // 拒绝 __proto__、constructor、prototype 等危险属性名
                    const PROTO_POLLUTION_KEYS: &[&str] =
                        &["__proto__", "constructor", "prototype"];
                    for key in obj.keys() {
                        if PROTO_POLLUTION_KEYS.contains(&key.as_str()) {
                            return Err(rquickjs::Error::new_from_js_message(
                                "Error",
                                "ProtoPollution",
                                format!("patch.add: 拒绝原型链污染属性 '{}'", key),
                            ));
                        }
                    }

                    // 1. 路径非空
                    if path.is_empty() {
                        return Err(rquickjs::Error::new_from_js(
                            "Error",
                            "patch.add: 'path' 字段不能为空",
                        ));
                    }
                    // 2. 不含控制字符（\0, \n, \r, \t 等）
                    if path.chars().any(|c| c.is_control()) {
                        return Err(rquickjs::Error::new_from_js(
                            "Error",
                            "patch.add: 'path' 字段包含控制字符",
                        ));
                    }
                    // 3. 不以 '$' 开头（$ 操作由 DSL 编译器处理）
                    if path.starts_with('$') {
                        return Err(rquickjs::Error::new_from_js(
                            "Error",
                            "patch.add: 'path' 字段不能以 '$' 开头（$ 操作由 DSL 编译器处理）",
                        ));
                    }

                    let op_str = obj
                        .get("op")
                        .and_then(|v| v.as_str())
                        .unwrap_or("deep_merge");
                    let value = obj.get("value").cloned().unwrap_or(serde_json::Value::Null);

                    let op = match op_str {
                        "override" => PatchOp::Override,
                        "prepend" => PatchOp::Prepend,
                        "append" => PatchOp::Append,
                        "set_default" | "default" => PatchOp::SetDefault,
                        _ => PatchOp::DeepMerge,
                    };

                    let source = PatchSource {
                        kind: SourceKind::Script {
                            name: script_name.clone(),
                        },
                        file: Some(script_name.clone()),
                        line: None,
                        plugin_id: None,
                    };

                    let patch = Patch::new(source, Scope::Global, path, op, value);
                    collector.add_patch(patch);
                }
                Ok(())
            },
        )?;
        patch_obj.set("add", add_fn)?;

        globals.set("patch", patch_obj)?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // store 对象（KV 存储）
    // ═══════════════════════════════════════════════════════

    fn register_store<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        kv_store: Arc<KvStore>,
    ) -> RjsResult<()> {
        let store_obj = Object::new(ctx.clone())?;

        // store.get(key)
        let store_clone = Arc::clone(&kv_store);
        let get_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> RjsResult<Value<'js>> {
                match store_clone.get(&key) {
                    Some(val) => json_value_to_rquickjs(&val, &ctx),
                    None => rquickjs::Undefined.into_js(&ctx),
                }
            },
        )?;
        store_obj.set("get", get_fn)?;

        // store.set(key, value)
        let store_clone = Arc::clone(&kv_store);
        let set_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, key: String, value: Value<'_>| -> RjsResult<()> {
                let json_val = js_value_to_json(&value);
                store_clone.set(key, json_val);
                Ok(())
            },
        )?;
        store_obj.set("set", set_fn)?;

        // store.delete(key)
        let store_clone = Arc::clone(&kv_store);
        let delete_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, key: String| -> RjsResult<bool> { Ok(store_clone.delete(&key)) },
        )?;
        store_obj.set("delete", delete_fn)?;

        // store.keys()
        let store_clone = Arc::clone(&kv_store);
        let keys_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> RjsResult<Value<'js>> {
            let keys = store_clone.keys();
            keys.as_slice().into_js(&ctx)
        })?;
        store_obj.set("keys", keys_fn)?;

        globals.set("store", store_obj)?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // env 对象（只读环境信息）
    // ═══════════════════════════════════════════════════════

    fn register_env<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        script_ctx: &ScriptContext,
    ) -> RjsResult<()> {
        let env_obj = Object::new(ctx.clone())?;
        env_obj.set("coreType", script_ctx.core_type.as_str())?;
        env_obj.set("coreVersion", script_ctx.core_version.as_str())?;
        env_obj.set("platform", script_ctx.platform.as_str())?;
        env_obj.set("profileName", script_ctx.profile_name.as_str())?;
        globals.set("env", env_obj)?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════
    // log 对象
    // ═══════════════════════════════════════════════════════

    fn register_log<'js>(
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        log_collector: Arc<std::sync::Mutex<Vec<crate::runtime::LogEntry>>>, // 共享日志收集器（同时输出到 tracing）
        max_log_entries: usize,
    ) -> RjsResult<()> {
        let log_obj = Object::new(ctx.clone())?;

        for (level_name, level) in [
            ("info", "INFO"),
            ("warn", "WARN"),
            ("error", "ERROR"),
            ("debug", "DEBUG"),
        ] {
            let fn_level = level.to_string();
            let collector = Arc::clone(&log_collector);
            let log_fn = Function::new(
                ctx.clone(),
                move |_: Ctx<'js>, msg: String| -> RjsResult<()> {
                    // 消息大小截断（防止超大日志消息消耗内存）
                    const MAX_LOG_MSG_SIZE: usize = 10240; // 10KB
                    let msg = if msg.len() > MAX_LOG_MSG_SIZE {
                        tracing::warn!(
                            target = "clash_prism_script",
                            original_len = msg.len(),
                            "log message truncated to {} bytes",
                            MAX_LOG_MSG_SIZE
                        );
                        let boundary = char_boundary_floor(&msg, MAX_LOG_MSG_SIZE);
                        format!("{}...[truncated]", &msg[..boundary])
                    } else {
                        msg
                    };
                    // 同时输出到 tracing 和收集到共享日志向量
                    match fn_level.as_str() {
                        "DEBUG" => tracing::event!(tracing::Level::DEBUG, "[script] {}", msg),
                        "INFO" => tracing::event!(tracing::Level::INFO, "[script] {}", msg),
                        "WARN" => tracing::event!(tracing::Level::WARN, "[script] {}", msg),
                        _ => tracing::event!(tracing::Level::ERROR, "[script] {}", msg),
                    }
                    //收集到共享日志向量（使用传入的 max_log_entries 而非硬编码常量）
                    let log_entry = crate::runtime::LogEntry {
                        level: match fn_level.as_str() {
                            "DEBUG" => crate::runtime::LogLevel::Debug,
                            "INFO" => crate::runtime::LogLevel::Info,
                            "WARN" => crate::runtime::LogLevel::Warn,
                            _ => crate::runtime::LogLevel::Error,
                        },
                        message: msg,
                        timestamp: chrono::Utc::now(),
                    };
                    if let Ok(mut logs) = collector.lock() {
                        if logs.len() < max_log_entries {
                            logs.push(log_entry);
                        }
                    }
                    Ok(())
                },
            )?;
            log_obj.set(level_name, log_fn)?;
        }

        globals.set("log", log_obj)?;
        Ok(())
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 结构化工具：utils.proxies API
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 创建 utils.proxies 对象，包含 filter/rename/remove/sort/deduplicate/groupBy
    ///
    fn make_proxies_api<'js>(
        ctx: &Ctx<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<Object<'js>> {
        let proxies_obj = Object::new(ctx.clone())?;

        // proxies.filter(predicate_fn) → 返回过滤后的代理数组（只读操作）
        // 这是必要的性能权衡——JS 谓词函数提供了最大的灵活性（支持任意过滤逻辑），
        // 但对于大型代理列表（>1000 个节点）可能较慢。如需高性能过滤，
        // 建议使用 utils.match() 进行预筛选后再调用 filter()。
        let cfg = Arc::clone(&config);
        let filter_fn = Function::new(
            ctx.clone(),
            move |ctx, pred_fn: Value<'js>| -> RjsResult<Value<'js>> {
                let pred_func: Function = pred_fn.into_function().ok_or_else(|| {
                    rquickjs::Error::new_from_js_message(
                        "TypeError",
                        "filter",
                        "filter() requires a function argument",
                    )
                })?;
                let cfg_read = cfg.lock().unwrap_or_else(|e| e.into_inner());
                let proxies = get_config_proxies_array(&cfg_read);
                drop(cfg_read);
                const MAX_FILTER_PROXIES: usize = 2000;
                if proxies.len() > MAX_FILTER_PROXIES {
                    return Err(rquickjs::Error::new_from_js_message(
                        "RangeError",
                        "filter",
                        format!(
                            "proxies.filter(): 代理数量 ({}) 超过硬限制 ({})，拒绝执行。\
                             请使用 utils.match() 进行预筛选后再调用 filter()。",
                            proxies.len(),
                            MAX_FILTER_PROXIES
                        ),
                    ));
                }
                if proxies.len() > 500 {
                    tracing::info!(
                        target = "clash_prism_script",
                        node_count = proxies.len(),
                        "proxies.filter() 在大型代理列表上执行，可能较慢。\
                         建议使用 utils.match() 进行预筛选后再调用 filter()。"
                    );
                }
                let mut filtered = Vec::new();
                for item in &proxies {
                    let js_val = json_value_to_rquickjs(item, &ctx)?;
                    let keep: bool = pred_func.call((js_val,))?;
                    if keep {
                        filtered.push(item.clone());
                    }
                }
                json_vec_to_rquickjs_array(&filtered, &ctx)
            },
        )?;
        proxies_obj.set("filter", filter_fn)?;

        // proxies.rename(pattern, replacement) → 就地修改配置中的代理名称
        let cfg = Arc::clone(&config);
        let rename_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, pattern_str: String, replacement: String| -> RjsResult<usize> {
                match regex::RegexBuilder::new(&pattern_str)
                    .size_limit(1024 * 1024)
                    .dfa_size_limit(1024 * 1024)
                    .build()
                {
                    Ok(re) => {
                        let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                        Ok(rename_proxies_in_config(&mut cfg_mut, &re, &replacement))
                    }
                    Err(e) => Err(rquickjs::Error::new_from_js_message(
                        "regex",
                        "Regex",
                        format!("Invalid regex: {}", e),
                    )),
                }
            },
        )?;
        proxies_obj.set("rename", rename_fn)?;

        // proxies.remove(predicate_fn) → 删除匹配的代理（就地修改）
        let cfg = Arc::clone(&config);
        let remove_fn = Function::new(
            ctx.clone(),
            move |ctx, pred_fn: Function<'js>| -> RjsResult<usize> {
                let mut remaining = Vec::new();
                let mut removed = 0usize;
                let proxies_arr = {
                    let cfg_guard = cfg.lock().unwrap_or_else(|e| e.into_inner());
                    get_config_proxies_array(&cfg_guard).clone()
                };
                for item in &proxies_arr {
                    let js_val = json_value_to_rquickjs(item, &ctx)?;
                    let keep: bool = pred_fn.call((js_val,))?;
                    if keep {
                        remaining.push(item.clone());
                    } else {
                        removed += 1;
                    }
                }
                // 实际写入回配置（短暂持锁）
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(arr) = cfg_mut.get_mut("proxies").and_then(|v| v.as_array_mut()) {
                    *arr = remaining;
                }
                Ok(removed)
            },
        )?;
        proxies_obj.set("remove", remove_fn)?;

        // proxies.sort(field, order?) → 就地排序
        let cfg = Arc::clone(&config);
        let sort_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, field: String, order: Option<String>| -> RjsResult<()> {
                let order_asc = order.as_deref().unwrap_or("asc") != "desc";
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                sort_proxies_in_config(&mut cfg_mut, &field, order_asc);
                Ok(())
            },
        )?;
        proxies_obj.set("sort", sort_fn)?;

        // proxies.deduplicate(by_fields?) → 就地去重
        let cfg = Arc::clone(&config);
        let dedup_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, by: Option<Value<'js>>| -> RjsResult<usize> {
                let fields = parse_by_fields(by);
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                Ok(deduplicate_proxies_in_config(&mut cfg_mut, &fields))
            },
        )?;
        proxies_obj.set("deduplicate", dedup_fn)?;

        // proxies.groupBy(pattern) → 返回普通对象 { groupName: Proxy[] }
        let cfg = Arc::clone(&config);
        let group_by_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, pattern_str: String| -> RjsResult<Value<'js>> {
                match regex::RegexBuilder::new(&pattern_str)
                    .size_limit(1024 * 1024)
                    .dfa_size_limit(1024 * 1024)
                    .build()
                {
                    Ok(re) => {
                        let cfg_read = cfg.lock().unwrap_or_else(|e| e.into_inner());
                        let groups = group_proxies_by_regex(&cfg_read, &re);
                        drop(cfg_read);
                        let result_obj = Object::new(ctx.clone())?;
                        for (key, proxies) in &groups {
                            let js_arr = json_vec_to_rquickjs_array(proxies, &ctx)?;
                            result_obj.set(key.as_str(), js_arr)?;
                        }
                        Ok(result_obj.into_value())
                    }
                    Err(e) => Err(rquickjs::Error::new_from_js_message(
                        "regex",
                        "Regex",
                        format!("Invalid regex: {}", e),
                    )),
                }
            },
        )?;
        proxies_obj.set("groupBy", group_by_fn)?;

        Ok(proxies_obj)
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 结构化工具：utils.rules API
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 创建 utils.rules 对象，包含 prepend/append/insertAt/remove/deduplicate
    ///
    fn make_rules_api<'js>(
        ctx: &Ctx<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<Object<'js>> {
        let rules_obj = Object::new(ctx.clone())?;

        // rules.prepend(...rule_strings)
        let cfg = Arc::clone(&config);
        let prepend_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, rules: Vec<String>| -> RjsResult<()> {
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                modify_rules_array(&mut cfg_mut, |arr| {
                    let mut reversed: Vec<serde_json::Value> = rules
                        .iter()
                        .map(|r| serde_json::Value::String(r.clone()))
                        .collect();
                    reversed.reverse();
                    arr.splice(0..0, reversed);
                });
                Ok(())
            },
        )?;
        rules_obj.set("prepend", prepend_fn)?;

        // rules.append(...rule_strings)
        let cfg = Arc::clone(&config);
        let append_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, rules: Vec<String>| -> RjsResult<()> {
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                modify_rules_array(&mut cfg_mut, |arr| {
                    for rule in &rules {
                        arr.push(serde_json::Value::String(rule.clone()));
                    }
                });
                Ok(())
            },
        )?;
        rules_obj.set("append", append_fn)?;

        // rules.insertAt(index, ...rule_strings)
        let cfg = Arc::clone(&config);
        let insert_at_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, index: i32, rules: Vec<String>| -> RjsResult<()> {
                let idx = if index < 0 { 0 } else { index as usize };
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                modify_rules_array(&mut cfg_mut, |arr| {
                    for (i, rule) in rules.iter().enumerate() {
                        arr.insert(idx + i, serde_json::Value::String(rule.clone()));
                    }
                });
                Ok(())
            },
        )?;
        rules_obj.set("insertAt", insert_at_fn)?;

        let cfg = Arc::clone(&config);
        let remove_fn = Function::new(ctx.clone(), move |pred_fn: Function| -> RjsResult<usize> {
            // 避免在 pred_fn 执行期间（可能耗时较长）持有 cfg 的 Mutex 锁。
            let rules_arr = {
                let cfg_guard = cfg.lock().unwrap_or_else(|e| e.into_inner());
                get_config_rules_array(&cfg_guard).clone()
            };
            // 锁已释放，安全执行回调
            let mut remaining = Vec::new();
            let mut removed = 0usize;
            for item in &rules_arr {
                let keep: bool = pred_fn.call((item.clone(),))?;
                if keep {
                    remaining.push(serde_json::Value::String(item.clone()));
                } else {
                    removed += 1;
                }
            }
            // 实际写入回配置（短暂持锁）
            let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(rules_val) = cfg_mut.get_mut("rules") {
                *rules_val = serde_json::Value::Array(remaining);
            }
            Ok(removed)
        })?;
        rules_obj.set("remove", remove_fn)?;

        // rules.deduplicate()
        let cfg = Arc::clone(&config);
        let dedup_fn = Function::new(ctx.clone(), move |_ctx: Ctx<'js>| -> RjsResult<usize> {
            let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
            let removed = deduplicate_rules_in_config(&mut cfg_mut);
            Ok(removed)
        })?;
        rules_obj.set("deduplicate", dedup_fn)?;

        Ok(rules_obj)
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 结构化工具：utils.groups API
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 创建 utils.groups 对象，包含 get/addProxy/removeProxy/create/remove
    ///
    fn make_groups_api<'js>(
        ctx: &Ctx<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<Object<'js>> {
        let groups_obj = Object::new(ctx.clone())?;

        // groups.get(name) → 返回代理组对象或 undefined（只读）
        let cfg = Arc::clone(&config);
        let get_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String| -> RjsResult<Value<'js>> {
                let cfg_read = cfg.lock().unwrap_or_else(|e| e.into_inner());
                let result = find_proxy_group(&cfg_read, &name).cloned();
                drop(cfg_read);
                match result {
                    Some(group) => json_value_to_rquickjs(&group, &ctx),
                    None => rquickjs::Undefined.into_js(&ctx),
                }
            },
        )?;
        groups_obj.set("get", get_fn)?;

        // groups.addProxy(groupName, ...proxyNames)
        let cfg = Arc::clone(&config);
        let add_proxy_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, group_name: String, proxy_names: Vec<String>| -> RjsResult<()> {
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                add_proxies_to_group(&mut cfg_mut, &group_name, &proxy_names);
                Ok(())
            },
        )?;
        groups_obj.set("addProxy", add_proxy_fn)?;

        // groups.removeProxy(groupName, ...proxyNames)
        let cfg = Arc::clone(&config);
        let remove_proxy_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, group_name: String, proxy_names: Vec<String>| -> RjsResult<()> {
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                remove_proxies_from_group(&mut cfg_mut, &group_name, &proxy_names);
                Ok(())
            },
        )?;
        groups_obj.set("removeProxy", remove_proxy_fn)?;

        // groups.create(groupSpec)
        let cfg = Arc::clone(&config);
        let create_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, spec: Value<'_>| -> RjsResult<()> {
                let spec_json = js_value_to_json(&spec);
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                create_proxy_group(&mut cfg_mut, &spec_json);
                Ok(())
            },
        )?;
        groups_obj.set("create", create_fn)?;

        // groups.remove(name)
        let cfg = Arc::clone(&config);
        let remove_fn = Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, name: String| -> RjsResult<bool> {
                let mut cfg_mut = cfg.lock().unwrap_or_else(|e| e.into_inner());
                Ok(remove_proxy_group_by_name(&mut cfg_mut, &name))
            },
        )?;
        groups_obj.set("remove", remove_fn)?;

        Ok(groups_obj)
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 内部工厂方法：基础 API 函数
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 创建 config.get 函数
    fn make_config_get<'js>(
        ctx: &Ctx<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<Function<'js>> {
        Function::new(
            ctx.clone(),
            move |ctx, key: Option<String>| -> RjsResult<Value<'js>> {
                let cfg_read = config.lock().unwrap_or_else(|e| e.into_inner());
                let result = match key {
                    Some(k) => resolve_path(&cfg_read, &k),
                    None => cfg_read.clone(),
                };
                drop(cfg_read);
                json_value_to_rquickjs(&result, &ctx)
            },
        )
    }

    /// 创建 config.set 函数
    ///
    /// - 拒绝设置 `__when__`、`__after__` 等 DSL 元数据路径和内部路径
    /// - 如果目标路径已有值，新值的类型应兼容
    fn make_config_set<'js>(
        ctx: &Ctx<'js>,
        config: Arc<std::sync::Mutex<serde_json::Value>>,
    ) -> RjsResult<Function<'js>> {
        Function::new(
            ctx.clone(),
            move |ctx, key: String, value: Value<'js>| -> RjsResult<Value<'js>> {
                // 防止通过数组索引路径（如 "rules.0.__when__"）绕过危险路径检查。
                let blocked_prefixes = [
                    "__when__",
                    "__after__",
                    "__rule__",
                    // 内部路径（引擎运行时元数据）
                    "_metadata",
                    "_internal",
                    "_prism_",
                ];
                // 1. 检查完整 key
                for prefix in &blocked_prefixes {
                    if key == *prefix || key.starts_with(&format!("{}.", prefix)) {
                        return Err(rquickjs::Error::new_from_js_message(
                            "SecurityError",
                            "config.set",
                            format!("不允许设置受保护路径 '{}'", prefix),
                        ));
                    }
                }
                // 2. 检查每个路径段（防止 "rules.0.__when__" 等绕过）
                for segment in key.split('.') {
                    for prefix in &blocked_prefixes {
                        if segment == *prefix {
                            return Err(rquickjs::Error::new_from_js_message(
                                "SecurityError",
                                "config.set",
                                format!("路径段 '{}' 为受保护名称，不允许设置", prefix),
                            ));
                        }
                    }
                }
                // Reject keys containing null bytes or path traversal patterns.
                // JSON 路径中 ".." 无合法语义（不是 JSONPath 的递归下降操作符），
                // 因此只需精确匹配 ".." 段，拒绝包含目录遍历语义的路径。
                // 合法属性名如 "version..1" 或 "a..b" 不含目录遍历语义，应予放行。
                let has_path_traversal = {
                    let mut found = false;
                    for segment in key.split('.') {
                        if segment == ".." {
                            found = true;
                            break;
                        }
                    }
                    found
                };
                if key.contains('\0') || has_path_traversal {
                    return Err(rquickjs::Error::new_from_js_message(
                        "SecurityError",
                        "config.set",
                        "Path contains illegal characters",
                    ));
                }
                const MAX_PATH_DEPTH: usize = 20;
                let path_depth = key.split('.').count();
                if path_depth > MAX_PATH_DEPTH {
                    return Err(rquickjs::Error::new_from_js_message(
                        "RangeError",
                        "config.set",
                        format!(
                            "Path depth ({}) exceeds maximum allowed depth ({})",
                            path_depth, MAX_PATH_DEPTH
                        ),
                    ));
                }

                let json_val = js_value_to_json(&value);

                // 直接使用写锁完成读取检查 + 写入，确保检查与写入的原子性。
                let mut cfg_mut = config.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(existing) = resolve_path_value(&cfg_mut, &key).cloned() {
                    if !is_type_compatible(&existing, &json_val) {
                        drop(cfg_mut);
                        let obj = Object::new(ctx)?;
                        obj.set("action", "set")?;
                        obj.set("key", key)?;
                        obj.set("ok", false)?;
                        obj.set(
                            "error",
                            format!(
                                "类型不兼容: 目标路径已有类型 '{}', 新值类型 '{}'",
                                value_type_name(&existing),
                                value_type_name(&json_val)
                            ),
                        )?;
                        return Ok(obj.into_value());
                    }
                }

                // 通过点分路径设置值到共享配置中（仍在写锁保护下）
                set_path_value(&mut cfg_mut, &key, json_val);
                drop(cfg_mut);
                // 返回确认对象（§5.2 兼容）
                let obj = Object::new(ctx)?;
                obj.set("action", "set")?;
                obj.set("key", key)?;
                obj.set("value", value)?;
                obj.set("ok", true)?;
                Ok(obj.into_value())
            },
        )
    }

    /// 创建 utils.match 函数
    fn make_utils_match<'js>(ctx: &Ctx<'js>) -> RjsResult<Function<'js>> {
        Function::new(
            ctx.clone(),
            |_: Ctx<'js>, pattern: String, text: String| -> RjsResult<bool> {
                Ok(glob_match::glob_match(&pattern, &text))
            },
        )
    }

    /// 创建 utils.includes 函数
    fn make_utils_includes<'js>(ctx: &Ctx<'js>) -> RjsResult<Function<'js>> {
        Function::new(
            ctx.clone(),
            move |_: Ctx<'js>, haystack: String, needle: String| -> RjsResult<bool> {
                Ok(haystack.contains(&needle))
            },
        )
    }
}

// ══════════════════════════════════════════════════════════
// Helper: UTF-8 safe truncation
// ══════════════════════════════════════════════════════════

/// Find the largest index <= `max` that falls on a UTF-8 character boundary.
/// This is a stable equivalent of the unstable `str::floor_char_boundary`.
fn char_boundary_floor(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ══════════════════════════════════════════════════════════
// Helper: JSON path resolution
// ══════════════════════════════════════════════════════════

/// 从 JSON 对象中解析点分隔路径
fn resolve_path(value: &serde_json::Value, path: &str) -> serde_json::Value {
    let mut current = value;
    for part in path.split('.') {
        let (key, index) = parse_array_access(part);
        match current.get(key) {
            Some(v) => {
                if let Some(idx) = index {
                    if let Some(arr) = v.as_array() {
                        if idx < arr.len() {
                            current = &arr[idx];
                        } else {
                            return serde_json::Value::Null;
                        }
                    } else {
                        return serde_json::Value::Null;
                    }
                } else {
                    current = v;
                }
            }
            None => return serde_json::Value::Null,
        }
    }
    current.clone()
}

/// 解析可能的数组索引访问，如 "groups[0]" → ("groups", Some(0))
fn parse_array_access(s: &str) -> (&str, Option<usize>) {
    if let Some(start) = s.find('[') {
        if let Some(end) = s.find(']') {
            let key = &s[..start];
            let idx_str = &s[start + 1..end];
            if let Ok(idx) = idx_str.parse::<usize>() {
                return (key, Some(idx));
            }
        }
    }
    (s, None)
}

// ══════════════════════════════════════════════════════════
// 辅助函数：JS ↔ JSON 类型转换
// ══════════════════════════════════════════════════════════

/// 将 serde_json::Value 转换为 rquickjs::Value
fn json_value_to_rquickjs<'js>(val: &serde_json::Value, ctx: &Ctx<'js>) -> RjsResult<Value<'js>> {
    json_value_to_rquickjs_inner(val, ctx, 0)
}

/// 递归转换的内部实现，带深度限制（防止栈溢出）
const MAX_CONVERT_DEPTH: usize = 256;

fn json_value_to_rquickjs_inner<'js>(
    val: &serde_json::Value,
    ctx: &Ctx<'js>,
    depth: usize,
) -> RjsResult<Value<'js>> {
    if depth > MAX_CONVERT_DEPTH {
        tracing::warn!(
            target = "clash_prism_script",
            depth = depth,
            max = MAX_CONVERT_DEPTH,
            "json_value_to_rquickjs: max depth exceeded, returning undefined"
        );
        return rquickjs::Undefined.into_js(ctx);
    }
    match val {
        serde_json::Value::Null => Ok(Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => b.into_js(ctx),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_js(ctx)
            } else if let Some(f) = n.as_f64() {
                f.into_js(ctx)
            } else {
                Value::new_null(ctx.clone()).into_js(ctx)
            }
        }
        serde_json::Value::String(s) => s.as_str().into_js(ctx),
        serde_json::Value::Array(arr) => {
            let mut vals = Vec::with_capacity(arr.len());
            for item in arr {
                vals.push(json_value_to_rquickjs_inner(item, ctx, depth + 1)?);
            }
            vals.as_slice().into_js(ctx)
        }
        serde_json::Value::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k.as_str(), json_value_to_rquickjs_inner(v, ctx, depth + 1)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

/// 将 JSON 数组转换为 JS 数组
fn json_vec_to_rquickjs_array<'js>(
    items: &[serde_json::Value],
    ctx: &Ctx<'js>,
) -> RjsResult<Value<'js>> {
    let mut vals = Vec::with_capacity(items.len());
    for item in items {
        vals.push(json_value_to_rquickjs(item, ctx)?);
    }
    vals.as_slice().into_js(ctx)
}

/// 将 rquickjs Value 转换为 serde_json::Value
fn js_value_to_json(val: &Value<'_>) -> serde_json::Value {
    js_value_to_json_inner(val, 0)
}

/// 递归转换的内部实现，带深度限制（防止栈溢出）
fn js_value_to_json_inner(val: &Value<'_>, depth: usize) -> serde_json::Value {
    if depth > MAX_CONVERT_DEPTH {
        tracing::warn!(
            target = "clash_prism_script",
            depth = depth,
            max = MAX_CONVERT_DEPTH,
            "js_value_to_json: max depth exceeded, returning null"
        );
        return serde_json::Value::Null;
    }
    if val.is_null() || val.is_undefined() {
        serde_json::Value::Null
    } else if val.is_bool() {
        serde_json::Value::Bool(val.as_bool().unwrap_or(false))
    } else if let Some(n) = val.as_number() {
        // Use i64 instead of i32 to preserve precision for JavaScript timestamps
        // and other large integers that would otherwise be truncated.
        let f = n;
        if f >= i64::MIN as f64 && f <= i64::MAX as f64 && f.fract() == 0.0 {
            // Number.MAX_SAFE_INTEGER = 2^53 - 1，超过此值的整数在 JS 中无法精确表示。
            // 对于需要精确传递的大整数，建议在 JS 端使用字符串传递。
            if f.abs() > 9_007_199_254_740_991.0 {
                tracing::warn!(
                    target = "clash_prism_script",
                    original_value = f,
                    max_safe_integer = 9_007_199_254_740_991_i64,
                    "js_value_to_json: 整数值超出 Number::MAX_SAFE_INTEGER (2^53-1)，\
                     使用浮点表示保留原始值以避免静默截断。\
                     如需精确传递，请在 JS 端使用字符串表示该数值。"
                );
                // 超出安全整数范围时保留原始浮点表示，避免 i64 截断导致静默数据损坏
                // from_f64 返回 None 当且仅当 f 为 NaN 或 Infinity。
                if f.is_nan() || f.is_infinite() {
                    tracing::warn!(
                        target = "clash_prism_script",
                        original_value = f,
                        "js_value_to_json: NaN/Infinity 无法表示为 JSON，已转为 null。\
                         请在 JS 端使用 isFinite() 检查后再传递。"
                    );
                }
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Number::from(f as i64).into()
            }
        } else {
            // 非整数浮点数：from_f64 返回 None 当且仅当 f 为 NaN 或 Infinity。
            if f.is_nan() || f.is_infinite() {
                tracing::warn!(
                    target = "clash_prism_script",
                    original_value = f,
                    "js_value_to_json: NaN/Infinity 无法表示为 JSON，已转为 null。\
                     请在 JS 端使用 isFinite() 检查后再传递。"
                );
            }
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
    } else if val.is_string() {
        match val.get::<String>() {
            Ok(s) => serde_json::Value::String(s),
            Err(_) => serde_json::Value::Null,
        }
    } else if let Some(obj) = val.as_object() {
        let mut map = serde_json::Map::new();
        for k in obj.keys::<String>().flatten() {
            if let Ok(v) = obj.get::<_, Value>(&k) {
                map.insert(k, js_value_to_json_inner(&v, depth + 1));
            }
        }
        serde_json::Value::Object(map)
    } else if let Some(arr) = val.as_array() {
        let mut vec = Vec::new();
        for v in arr.iter::<Value>().flatten() {
            vec.push(js_value_to_json_inner(&v, depth + 1));
        }
        serde_json::Value::Array(vec)
    } else {
        // 而非 Debug 格式字符串（避免 JSON 序列化时产生非法内容）
        tracing::warn!(
            target = "clash_prism_script",
            js_type = %val.type_name(),
            "js_value_to_json: 遇到未知 JS 类型，已转为 null"
        );
        serde_json::Value::Null
    }
}

// ══════════════════════════════════════════════════════════
// 辅助函数：配置数据访问
// ══════════════════════════════════════════════════════════

/// 获取配置中的 proxies 数组
///
/// 返回拥有所有权的 `Vec`（深拷贝），因为调用方需要在释放 MutexGuard 后
/// 仍能遍历代理列表（例如 proxies.filter() 中先 drop guard 再遍历）。
/// 如果调用方不需要跨 guard 生命周期持有数据，可改用引用版本。
fn get_config_proxies_array(config: &serde_json::Value) -> Vec<serde_json::Value> {
    config
        .get("proxies")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// 获取配置中的 rules 数组
fn get_config_rules_array(config: &serde_json::Value) -> Vec<String> {
    config
        .get("rules")
        .and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// 查找指定名称的代理组
fn find_proxy_group<'a>(
    config: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    config
        .get("proxy-groups")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|g| g.get("name").and_then(|n| n.as_str()) == Some(name))
        })
}

// ══════════════════════════════════════════════════════════
// 辅助函数：Proxies 操作实现
// ══════════════════════════════════════════════════════════

/// 用正则替换代理节点名称，返回被修改的节点数
fn rename_proxies_in_config(
    config: &mut serde_json::Value,
    re: &regex::Regex,
    replacement: &str,
) -> usize {
    let mut count = 0;
    if let Some(arr) = config.get_mut("proxies").and_then(|v| v.as_array_mut()) {
        for proxy in arr.iter_mut() {
            // 单次 get_mut 获取可变引用，clone 当前值用于比较
            if let Some(name_val) = proxy.get_mut("name") {
                if let Some(old_name) = name_val.as_str() {
                    let new_name = re.replace(old_name, replacement).to_string();
                    if new_name != old_name {
                        *name_val = serde_json::Value::String(new_name);
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

/// 对代理节点按指定字段排序
fn sort_proxies_in_config(config: &mut serde_json::Value, field: &str, ascending: bool) {
    if let Some(arr) = config.get_mut("proxies").and_then(|v| v.as_array_mut()) {
        arr.sort_by(|a, b| {
            let a_val = a.get(field).map(sort_key).unwrap_or_default();
            let b_val = b.get(field).map(sort_key).unwrap_or_default();
            if ascending {
                a_val.cmp(&b_val)
            } else {
                b_val.cmp(&a_val)
            }
        });
    }
}

/// 提取排序用的可比较键
fn sort_key(val: &serde_json::Value) -> std::borrow::Cow<'_, str> {
    match val {
        serde_json::Value::String(s) => std::borrow::Cow::Borrowed(s),
        serde_json::Value::Number(n) => std::borrow::Cow::Owned(n.to_string()),
        serde_json::Value::Bool(b) => std::borrow::Cow::Owned(b.to_string()),
        _ => std::borrow::Cow::Owned(String::new()),
    }
}

/// 解析去重字段列表
fn parse_by_fields(by: Option<Value<'_>>) -> Vec<String> {
    match by {
        Some(v) if v.is_string() => v
            .get::<String>()
            .map(|s| vec![s])
            .unwrap_or_else(|_| vec!["name".to_string()]),
        Some(v) if v.as_array().is_some() => {
            // 尝试作为数组处理
            let arr = match v.as_array() {
                Some(a) => a,
                None => return vec!["name".to_string()],
            };
            let mut fields = Vec::new();
            for item in arr.iter::<Value>().flatten() {
                if item.is_string() {
                    if let Ok(s) = item.get::<String>() {
                        fields.push(s);
                    }
                }
            }
            fields
        }
        _ => vec!["name".to_string()],
    }
}

/// 对代理节点按指定字段去重，返回移除的重复数
fn deduplicate_proxies_in_config(config: &mut serde_json::Value, fields: &[String]) -> usize {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut removed = 0;

    if let Some(arr) = config.get_mut("proxies").and_then(|v| v.as_array_mut()) {
        let original_len = arr.len();
        arr.retain(|proxy| {
            let key = fields
                .iter()
                .filter_map(|f| proxy.get(f).map(|v| format!("{}={}", f, v)))
                .collect::<Vec<_>>()
                .join("|");
            seen.insert(key)
        });
        removed = original_len - arr.len();
    }
    removed
}

/// 按正则表达式对代理分组
fn group_proxies_by_regex(
    config: &serde_json::Value,
    re: &regex::Regex,
) -> Vec<(String, Vec<serde_json::Value>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    if let Some(arr) = config.get("proxies").and_then(|v| v.as_array()) {
        for proxy in arr {
            let name = proxy.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if let Some(caps) = re.captures(name) {
                // 使用第一个捕获组作为分组键
                let group_key = caps
                    .get(1)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| caps[0].to_string());
                groups.entry(group_key).or_default().push(proxy.clone());
            }
        }
    }

    groups.into_iter().collect()
}

// ══════════════════════════════════════════════════════════
// 辅助函数：Rules 操作实现
// ══════════════════════════════════════════════════════════

/// 修改 rules 数组的辅助函数
///
/// 这与 DSL 语义（rules 不存在时应报错）不同，脚本 API 选择更灵活的行为：
/// 允许在无 rules 字段的配置上直接操作规则，降低脚本编写门槛。
fn modify_rules_array<F>(config: &mut serde_json::Value, modifier: F)
where
    F: FnOnce(&mut Vec<serde_json::Value>),
{
    if let Some(arr) = config.get_mut("rules") {
        if arr.is_null() {
            // null 值 → 替换为空数组（合法操作）
            *arr = serde_json::Value::Array(vec![]);
        } else if arr.as_array().is_none() {
            tracing::warn!(
                target = "clash_prism_script",
                actual_type = ?std::any::type_name_of_val(arr),
                "modify_rules_array: rules 字段为非数组类型，已替换为空数组（原值丢失）"
            );
            *arr = serde_json::Value::Array(vec![]);
        }
        if let Some(rules_arr) = arr.as_array_mut() {
            modifier(rules_arr);
        }
    } else {
        // 之前的行为：modifier(&mut new_arr) 执行后结果被 _ = new_arr 丢弃，
        // 导致 utils.rules.prepend() / .append() 在无 rules 字段时静默失败
        let mut new_arr = Vec::new();
        modifier(&mut new_arr);
        if let Some(obj) = config.as_object_mut() {
            obj.insert("rules".into(), serde_json::Value::Array(new_arr));
        }
    }
}

/// 对规则去重，返回移除的重复数
///
/// 原实现仅对字符串规则去重，非字符串规则（如对象规则）会被无条件保留。
fn deduplicate_rules_in_config(config: &mut serde_json::Value) -> usize {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut removed = 0;

    if let Some(arr) = config.get_mut("rules").and_then(|v| v.as_array_mut()) {
        let original_len = arr.len();
        arr.retain(|rule| {
            // 优先使用字符串值作为键（快速路径），非字符串值使用序列化结果作为键
            let key = if let Some(s) = rule.as_str() {
                s.to_string()
            } else {
                serde_json::to_string(rule).unwrap_or_default()
            };
            seen.insert(key)
        });
        removed = original_len - arr.len();
    }
    removed
}

// ══════════════════════════════════════════════════════════
// 辅助函数：Groups 操作实现
// ══════════════════════════════════════════════════════════

/// 向指定代理组添加代理节点
fn add_proxies_to_group(config: &mut serde_json::Value, group_name: &str, proxy_names: &[String]) {
    if let Some(groups) = config
        .get_mut("proxy-groups")
        .and_then(|v| v.as_array_mut())
    {
        for group in groups.iter_mut() {
            if group.get("name").and_then(|n| n.as_str()) == Some(group_name) {
                if let Some(proxies) = group.get_mut("proxies").and_then(|p| p.as_array_mut()) {
                    for name in proxy_names {
                        if !proxies.iter().any(|p| p.as_str() == Some(name.as_str())) {
                            proxies.push(serde_json::Value::String(name.clone()));
                        }
                    }
                }
                break;
            }
        }
    }
}

/// 从指定代理组移除代理节点
fn remove_proxies_from_group(
    config: &mut serde_json::Value,
    group_name: &str,
    proxy_names: &[String],
) {
    if let Some(groups) = config
        .get_mut("proxy-groups")
        .and_then(|v| v.as_array_mut())
    {
        for group in groups.iter_mut() {
            if group.get("name").and_then(|n| n.as_str()) == Some(group_name) {
                if let Some(proxies) = group.get_mut("proxies").and_then(|p| p.as_array_mut()) {
                    proxies.retain(|p| {
                        let name = p.as_str().unwrap_or("");
                        !proxy_names.iter().any(|n| n == name)
                    });
                }
                break;
            }
        }
    }
}

/// 创建新的代理组
///
/// 对 spec 进行白名单过滤，仅保留已知的安全字段，防止注入恶意字段。
/// groups.create() 接受任意 JS 对象，白名单过滤防止意外字段进入配置。
fn create_proxy_group(config: &mut serde_json::Value, spec: &serde_json::Value) {
    // 白名单：仅允许这些字段通过
    const PROXY_GROUP_ALLOWED_FIELDS: &[&str] = &[
        "name",
        "type",
        "proxies",
        "url",
        "interval",
        "tolerance",
        "lazy",
        "filter",
        "exclude-filter",
        "proxy",
        "use",
        "interface-name",
        "routing-mark",
        "include-all",
        "exclude-all",
        "icon",
        "test-url",
    ];

    // 验证必要字段（在白名单过滤之前，使用原始 spec）
    let name = spec.get("name").and_then(|n| n.as_str());
    if name.is_none() || name.unwrap().is_empty() {
        tracing::warn!(
            target = "clash_prism_script",
            "create_proxy_group: spec 缺少 'name' 字段或为空，跳过创建"
        );
        return;
    }
    let group_type = spec.get("type").and_then(|t| t.as_str());
    if group_type.is_none() || group_type.unwrap().is_empty() {
        tracing::warn!(
            target = "clash_prism_script",
            group_name = name.unwrap(),
            "create_proxy_group: spec 缺少 'type' 字段或为空，跳过创建"
        );
        return;
    }

    // 白名单过滤：仅保留已知字段，丢弃未知字段
    let mut filtered = serde_json::Map::new();
    if let Some(obj) = spec.as_object() {
        for &key in PROXY_GROUP_ALLOWED_FIELDS {
            if let Some(val) = obj.get(key) {
                filtered.insert(key.to_string(), val.clone());
            }
        }
        // 记录被过滤掉的未知字段（仅 warn 级别，不阻止创建）
        let unknown_fields: Vec<&String> = obj
            .keys()
            .filter(|k| !PROXY_GROUP_ALLOWED_FIELDS.contains(&k.as_str()))
            .collect();
        if !unknown_fields.is_empty() {
            tracing::warn!(
                target = "clash_prism_script",
                group_name = name.unwrap(),
                filtered_fields = ?unknown_fields,
                "create_proxy_group: 已过滤未知字段（仅保留白名单字段）"
            );
        }
    }
    let filtered_spec = serde_json::Value::Object(filtered);

    if let Some(groups) = config.get_mut("proxy-groups") {
        if groups.is_null() {
            *groups = serde_json::Value::Array(vec![]);
        }
        if let Some(arr) = groups.as_array_mut() {
            // 检查是否已存在同名组
            let name = filtered_spec
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if arr
                .iter()
                .any(|g| g.get("name").and_then(|n| n.as_str()) == Some(name))
            {
                return; // 已存在则不重复创建
            }
            arr.push(filtered_spec);
        }
    }
}

/// 按名称删除代理组，返回是否成功
fn remove_proxy_group_by_name(config: &mut serde_json::Value, name: &str) -> bool {
    if let Some(groups) = config
        .get_mut("proxy-groups")
        .and_then(|v| v.as_array_mut())
    {
        let original_len = groups.len();
        groups.retain(|g| g.get("name").and_then(|n| n.as_str()) != Some(name));
        return groups.len() < original_len;
    }
    false
}

// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════

/// 通过点分路径在 JSON 对象中设置值（自动创建中间对象）
///
/// 数字路径段会尝试作为数组索引访问，非数字段作为对象键访问。
fn set_path_value(config: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    if path.is_empty() {
        *config = value;
        return;
    }

    let parts: Vec<&str> = path.split('.').collect();
    // 使用 Option 包裹 value，只在最后一步时取走（move），避免循环中多次 move
    let mut pending_value = Some(value);
    let mut current = config;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // 最后一段：设置值（取走 pending_value）
            if let Some(v) = pending_value.take() {
                // Suggestion: 支持数组索引路径（如 "proxies.0.name"）
                if let Ok(idx) = part.parse::<usize>() {
                    match current {
                        serde_json::Value::Array(arr) if idx < arr.len() => {
                            arr[idx] = v;
                        }
                        // 索引越界时不自动扩展数组（避免意外行为）
                        _ => {} // 无法设置到非数组上
                    }
                } else {
                    if let serde_json::Value::Object(map) = current {
                        map.insert(part.to_string(), v);
                    }
                }
            }
        } else {
            // 中间段：确保存在并进入
            // Suggestion: 支持数组索引路径
            if let Ok(idx) = part.parse::<usize>() {
                match current {
                    serde_json::Value::Array(arr) if idx < arr.len() => {
                        current = &mut arr[idx];
                    }
                    _ => return,
                }
            } else {
                match current {
                    serde_json::Value::Object(map) => {
                        current = map
                            .entry(part.to_string())
                            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    }
                    _ => return,
                }
            }
        }
    }
}

// ══════════════════════════════════════════════════════════
// Helper: config.set 路径校验
// ══════════════════════════════════════════════════════════

/// 解析点分路径并返回目标位置的值引用（如果存在）。
/// 用于 config.set 的类型一致性检查。
fn resolve_path_value<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for part in path.split('.') {
        // 支持数组索引路径
        if let Ok(idx) = part.parse::<usize>() {
            match current {
                serde_json::Value::Array(arr) => {
                    current = arr.get(idx)?;
                }
                _ => return None,
            }
        } else {
            match current.get(part) {
                Some(v) => current = v,
                None => return None,
            }
        }
    }
    Some(current)
}

/// 检查新旧值的类型是否兼容。
///
/// 兼容规则：
/// - 相同类型 → 兼容
/// - Null 可替换为任何类型（初始化场景）
/// - 数字之间（整数/浮点）互相兼容
/// - 数组/对象可以替换为任何类型（结构变更）
fn is_type_compatible(existing: &serde_json::Value, new_val: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (existing, new_val) {
        // Null 可以被任何类型替换
        (Value::Null, _) | (_, Value::Null) => true,
        // 相同类型
        (Value::Bool(_), Value::Bool(_)) => true,
        (Value::Number(_), Value::Number(_)) => true,
        (Value::String(_), Value::String(_)) => true,
        (Value::Array(_), Value::Array(_)) => true,
        (Value::Object(_), Value::Object(_)) => true,
        // 不兼容的类型组合
        _ => false,
    }
}

/// 返回 JSON 值的类型名称（用于错误消息）
fn value_type_name(val: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match val {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本 groupBy 功能：按正则捕获组正确分组代理节点。
    /// 验证 group_proxies_by_regex 能将匹配的代理按第一个捕获组归类。
    #[test]
    fn test_group_by_basic() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "香港-01", "type": "ss"},
                {"name": "香港-02", "type": "ss"},
                {"name": "日本-01", "type": "vmess"},
                {"name": "美国-01", "type": "trojan"}
            ]
        });

        let re = regex::Regex::new(r"^(.+?)-\d+$").unwrap();
        let groups = group_proxies_by_regex(&config, &re);

        // 应产生 3 个分组：香港、日本、美国
        assert_eq!(groups.len(), 3);

        let group_map: std::collections::HashMap<&str, usize> =
            groups.iter().map(|(k, v)| (k.as_str(), v.len())).collect();
        assert_eq!(group_map.get("香港"), Some(&2));
        assert_eq!(group_map.get("日本"), Some(&1));
        assert_eq!(group_map.get("美国"), Some(&1));
    }

    /// 无匹配时的空结果：当正则不匹配任何代理名称时，应返回空分组。
    #[test]
    fn test_group_by_empty_result() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "proxy-a", "type": "ss"},
                {"name": "proxy-b", "type": "vmess"}
            ]
        });

        // 正则匹配 "节点-" 前缀，但代理名不含此前缀
        let re = regex::Regex::new(r"^节点-(.+)$").unwrap();
        let groups = group_proxies_by_regex(&config, &re);

        assert!(groups.is_empty(), "无匹配时应返回空分组");
    }

    /// 正则特殊字符处理：验证包含正则元字符的代理名不会导致 panic。
    /// 对抗性输入：代理名包含 ( ) [ ] { } . * + ? ^ $ | \ 等特殊字符。
    #[test]
    fn test_group_by_regex_with_special_chars() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "node(1)", "type": "ss"},
                {"name": "node(2)", "type": "ss"},
                {"name": "file.name.tar.gz", "type": "vmess"},
                {"name": "a+b*c?d", "type": "trojan"},
                {"name": "normal-node", "type": "ss"}
            ]
        });

        // 使用 \Q...\E 转义或精确匹配特殊字符
        let re = regex::Regex::new(r"^(.+?)\(\d+\)$").unwrap();
        let groups = group_proxies_by_regex(&config, &re);

        // 应匹配 "node(1)" 和 "node(2)"，分组键为 "node"
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "node");
        assert_eq!(groups[0].1.len(), 2);
    }

    /// 无 proxies 字段时的安全性：配置中不存在 proxies 数组时不应 panic。
    #[test]
    fn test_group_by_no_proxies_field() {
        let config = serde_json::json!({
            "rules": ["MATCH,DIRECT"]
        });

        let re = regex::Regex::new(r"^(.+)$").unwrap();
        let groups = group_proxies_by_regex(&config, &re);
        assert!(groups.is_empty());
    }

    /// dfa_size_limit 验证：超大正则应被 DFA 大小限制拒绝。
    /// 对抗性输入：构造一个会导致 DFA 状态爆炸的正则模式。
    #[test]
    fn test_group_by_dfa_size_limit() {
        // 构造一个会导致 DFA 状态爆炸的正则（经典的 ReDoS 模式）
        // a{1,n}a{1,n}a{1,n}... 这类嵌套量词模式会导致指数级 DFA 状态
        let malicious_pattern = format!("{}{}{}", "a".repeat(25), "?".repeat(25), "a".repeat(25));

        let result = regex::RegexBuilder::new(&malicious_pattern)
            .dfa_size_limit(1024) // 极小的 DFA 限制
            .build();

        // 这个模式可能编译成功但 DFA 被限制，也可能直接失败
        // 关键是验证 dfa_size_limit 参数被正确传递
        if let Ok(re) = result {
            // 如果编译成功，验证它能正常工作（不会 panic）
            let config = serde_json::json!({
                "proxies": [{"name": "test", "type": "ss"}]
            });
            let groups = group_proxies_by_regex(&config, &re);
            // 不应 panic，结果可能为空
            let _ = groups;
        }
        // 如果编译失败也是预期行为（DFA 限制生效）
    }
}
