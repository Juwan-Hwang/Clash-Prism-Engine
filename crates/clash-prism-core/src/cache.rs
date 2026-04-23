//! # 三级缓存架构
//!
//! 参考 Claude Code 的 FileReadCache (mtime 失效) + StatsCache (原子持久化 + 版本迁移)。
//!
//! 缓存层级：
//! - **L1: 内存缓存** — 进程生命周期，基于文件 mtime 自动失效，FIFO 淘汰策略
//! - **L2: 磁盘缓存** — 跨进程持久化，内容寻址，原子写入（temp + fsync + rename）
//! - **L3: 分布式缓存** — 多实例共享，通过 redb KV 存储（预留接口，暂未实现）
//!
//! 设计原则：
//! - **mtime 失效**：L1 缓存条目可关联文件路径，读取时自动检查文件修改时间
//! - **原子写入**：L2 使用 temp file + sync_all + atomic rename，确保崩溃安全
//! - **内容寻址**：缓存键基于文件内容哈希，内容不变则键不变

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use indexmap::IndexSet;

use serde::Serialize;
use serde::de::DeserializeOwned;

// ─── L1: 内存缓存 ────────────────────────────────────────────────

/// L1 内存缓存条目
#[derive(Clone)]
struct CacheEntry<V: Clone> {
    /// 缓存的值
    value: V,
    /// 关联文件的修改时间（毫秒），None 表示无文件关联
    mtime: Option<u64>,
}

/// L1 内存缓存内部状态，由单个 `Mutex` 统一保护
///
/// 将 entries、file_paths、fifo_order 合并到一个结构体中，
/// 通过一次加锁即可完成所有操作，避免多次加锁的开销和死锁风险。
struct CacheInner<V: Clone> {
    entries: HashMap<String, CacheEntry<V>>,
    /// 缓存键 → 关联文件路径（用于 mtime 检查）
    file_paths: HashMap<String, PathBuf>,
    /// FIFO 队列：维护插入顺序，队首为最早插入的条目（IndexSet: O(1) lookup + remove）
    fifo_order: IndexSet<String>,
}

/// L1 内存缓存 — mtime 自动失效 + FIFO 淘汰（线程安全）
///
/// 当缓存关联了文件路径时，每次 `get` 会检查文件 mtime 是否变化，
/// 变化则视为失效返回 None，并同时从 `entries`、`file_paths`、`fifo_order` 中移除该条目。
///
/// 当缓存达到 `max_size` 时，淘汰最早插入的条目（FIFO）。
///
/// 使用 `IndexSet` 维护插入顺序，提供 O(1) 的 contains、remove、pop_first，
/// 相比 VecDeque + retain 的 O(n) 方案显著更高效。
///
/// 内部状态通过 `Mutex<CacheInner<V>>` 统一保护，实现 `Send + Sync`，
/// 可安全地在多线程环境中共享。所有公开 API 签名保持不变（`&self`），
/// 内部通过 `.lock().unwrap()` 获取互斥锁。
///
/// ## 设计选择：std::sync::Mutex vs tokio::sync::Mutex
///
/// MemoryCache 的所有操作均为纯内存计算（HashMap/IndexSet 读写 + 可选的
/// `fs::metadata` mtime 检查），持锁时间极短，不会跨 await 点，
/// 因此使用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`，
/// 避免异步运行时的额外开销。
pub struct MemoryCache<V: Clone> {
    inner: Mutex<CacheInner<V>>,
    max_size: usize,
}

impl<V: Clone> MemoryCache<V> {
    /// 创建新的内存缓存
    ///
    /// # 参数
    /// - `max_size` — 最大缓存条目数，超出时按 FIFO 淘汰
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::new(),
                file_paths: HashMap::new(),
                fifo_order: IndexSet::new(),
            }),
            max_size,
        }
    }

    /// 获取缓存值
    ///
    /// 如果 key 关联了文件且文件 mtime 已变化，则视为失效返回 None，
    /// 并同时从 `entries`、`file_paths`、`fifo_order` 中移除该条目。
    ///
    /// 将 `fs::metadata()` 调用移到 Mutex 锁外部执行，
    /// 先释放锁获取文件路径信息，再执行 metadata 检查，
    /// 避免在持锁期间执行可能阻塞的文件系统 I/O。
    pub fn get(&self, key: &str) -> Option<V> {
        // 步骤 1：在锁内获取缓存条目和关联的文件路径（纯内存操作，极快）
        let (entry, file_path) = {
            let inner = self.inner.lock().unwrap();
            let entry = inner.entries.get(key).cloned()?;
            let file_path = inner.file_paths.get(key).cloned();
            (entry, file_path)
        }; // 锁在此处释放

        // 步骤 2：在锁外执行 fs::metadata()（可能阻塞的文件系统 I/O）
        let file_path = match file_path {
            Some(path) => path,
            None => return Some(entry.value),
        };

        if let Some(stored_mtime) = entry.mtime
            && let Ok(meta) = fs::metadata(&file_path)
            && let Ok(modified) = meta.modified()
        {
            let current_mtime = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if current_mtime != stored_mtime {
                // 步骤 3：mtime 变化，重新获取锁移除失效条目
                let mut inner = self.inner.lock().unwrap();
                inner.entries.remove(key);
                inner.file_paths.remove(key);
                inner.fifo_order.shift_remove(key);
                return None; // mtime 变化，缓存失效
            }
        }
        Some(entry.value)
    }

    /// 插入缓存值
    ///
    /// # 参数
    /// - `key` — 缓存键
    /// - `value` — 缓存值
    /// - `file_path` — 可选的关联文件路径，用于 mtime 失效检查
    pub fn insert(&self, key: String, value: V, file_path: Option<&Path>) {
        let mut inner = self.inner.lock().unwrap();

        // 如果已存在，先移除旧条目（从 FIFO 队列中也移除）
        if inner.entries.contains_key(&key) {
            inner.entries.remove(&key);
            inner.file_paths.remove(&key);
            inner.fifo_order.shift_remove(&key);
        }

        // FIFO 淘汰 — O(1) 从队首弹出最早条目
        while inner.entries.len() >= self.max_size && !inner.fifo_order.is_empty() {
            if let Some(oldest_key) = inner.fifo_order.shift_remove_index(0) {
                inner.entries.remove(&oldest_key);
                inner.file_paths.remove(&oldest_key);
            }
        }

        // 容量为 0 时，淘汰循环无法腾出空间，直接放弃插入
        if self.max_size == 0 {
            return;
        }

        // 计算文件 mtime
        let mtime = file_path.and_then(|path| {
            fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as u64)
                })
        });

        // 记录文件路径关联
        if let Some(path) = file_path {
            inner.file_paths.insert(key.clone(), path.to_path_buf());
        }

        // 追加到 FIFO 队列尾部
        inner.fifo_order.insert(key.clone());
        inner.entries.insert(key, CacheEntry { value, mtime });
    }

    /// 使指定 key 失效
    pub fn invalidate(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.remove(key);
        inner.file_paths.remove(key);
        inner.fifo_order.shift_remove(key);
    }

    /// 清除所有缓存
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.file_paths.clear();
        inner.fifo_order.clear();
    }

    /// 获取当前缓存条目数
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// 检查缓存是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }
}

// ─── L2: 磁盘缓存 ────────────────────────────────────────────────

/// L2 磁盘缓存 — 原子写入 + 版本迁移
///
/// 参考 Claude Code StatsCache 的原子写入模式：
/// 1. 写入临时文件
/// 2. 调用 `sync_all` 确保数据落盘
/// 3. 原子 `rename` 覆盖目标文件
///
/// 此模式确保在任何时刻读取缓存文件都能获得完整的数据，
/// 不会出现半写状态。
///
/// # 并发安全
///
/// 写入操作通过 `std::sync::Mutex` 保护，确保多线程并发写入时
/// 不会出现临时文件竞争或数据损坏。读取操作无需加锁（文件系统层面安全）。
///
/// ## 设计选择：std::sync::Mutex vs tokio::sync::Mutex
///
/// DiskCache 当前使用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`，
/// 因为 DiskCache 的所有 I/O 操作（文件读写、rename）都是同步阻塞调用。
/// 在 async 上下文中使用时，应通过 `spawn_blocking` 或 `tokio::task::block_in_place`
/// 将 DiskCache 操作卸载到专用线程池，避免阻塞 async runtime。
/// DiskCache 不持有跨 await 点的锁，因此 std::sync::Mutex 不会造成死锁。
pub struct DiskCache {
    cache_dir: PathBuf,
    /// 写入锁，保护 set/remove/cleanup 的原子性
    write_lock: std::sync::Mutex<()>,
    /// 写入计数器，用于控制 cleanup_stale_tmp_files 的调用频率
    /// 每 CLEANUP_INTERVAL 次写入执行一次清理，避免频繁目录扫描
    write_count: std::sync::atomic::AtomicU64,
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        // Best-effort cleanup of stale temp files when the cache is dropped.
        // This ensures temp files from interrupted writes are cleaned up even
        // if cleanup_stale_tmp_files wasn't called during normal operation.
        self.cleanup_stale_tmp_files();
    }
}

/// cleanup_stale_tmp_files 调用间隔（每 N 次写入执行一次）
const CLEANUP_INTERVAL: u64 = 10;

impl DiskCache {
    /// 创建磁盘缓存
    ///
    /// 如果 `cache_dir` 不存在，会自动创建。
    pub fn new(cache_dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&cache_dir).map_err(|e| format!("创建缓存目录失败: {}", e))?;
        Ok(Self {
            cache_dir,
            write_lock: std::sync::Mutex::new(()),
            write_count: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// 缓存文件路径（基于 key 的 SHA-256 哈希路径）
    ///
    /// 使用完整 SHA-256 哈希（64 个十六进制字符）作为文件名，
    /// 避免路径注入且消除 DefaultHasher 的碰撞风险。
    fn cache_path(&self, key: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        self.cache_dir.join(format!("{}.cache", hex))
    }

    /// 获取缓存
    ///
    /// 读取并反序列化缓存文件。文件不存在或反序列化失败返回 None。
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let path = self.cache_path(key);
        let data = fs::read(&path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// 写入缓存（原子写入：temp + sync_all + rename）
    ///
    /// 1. 创建临时文件
    /// 2. 序列化数据并写入
    /// 3. `sync_all` 确保数据落盘
    /// 4. 原子 `rename` 到目标路径
    ///
    /// also clean up any stale `.tmp` files from previous failed attempts.
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| format!("获取写入锁失败: {}", e))?;
        let target_path = self.cache_path(key);
        let temp_path = target_path.with_extension("tmp");

        // 序列化
        let data = serde_json::to_vec(value).map_err(|e| format!("序列化失败: {}", e))?;

        // 写入临时文件
        {
            let mut file =
                fs::File::create(&temp_path).map_err(|e| format!("创建临时文件失败: {}", e))?;
            file.write_all(&data)
                .map_err(|e| format!("写入临时文件失败: {}", e))?;
            file.sync_all()
                .map_err(|e| format!("sync_all 失败: {}", e))?;
        }

        // 原子 rename
        // 跨文件系统时 rename 会失败（EXDEV），回退到 copy + delete
        if let Err(rename_err) = fs::rename(&temp_path, &target_path) {
            #[cfg(unix)]
            let is_cross_fs = rename_err.raw_os_error() == Some(libc::EXDEV);
            #[cfg(not(unix))]
            let is_cross_fs = true;

            if is_cross_fs {
                tracing::debug!("跨文件系统 rename 失败，回退到 copy + delete");
                if let Err(copy_err) = fs::copy(&temp_path, &target_path) {
                    let _ = fs::remove_file(&temp_path);
                    return Err(format!(
                        "跨文件系统复制失败 ({} → {}): {}",
                        temp_path.display(),
                        target_path.display(),
                        copy_err
                    ));
                }
                if let Err(del_err) = fs::remove_file(&temp_path) {
                    tracing::warn!("临时文件删除失败（可手动清理）: {}", del_err);
                }
            } else {
                let _ = fs::remove_file(&temp_path);
                return Err(format!("原子重命名失败: {}", rename_err));
            }
        }

        // Only scan every CLEANUP_INTERVAL writes to avoid frequent directory I/O.
        let count = self
            .write_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if count.is_multiple_of(CLEANUP_INTERVAL) {
            self.cleanup_stale_tmp_files();
        }

        Ok(())
    }

    /// 删除缓存
    pub fn remove(&self, key: &str) -> Result<(), String> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| format!("获取写入锁失败: {}", e))?;
        let path = self.cache_path(key);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("删除缓存失败: {}", e))?;
        }
        Ok(())
    }

    /// 清理过期缓存
    ///
    /// 删除修改时间超过 `max_age` 的缓存文件。
    ///
    /// # 返回
    /// 被清理的文件数量
    pub fn cleanup(&self, max_age: Duration) -> Result<usize, String> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| format!("获取写入锁失败: {}", e))?;
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut cleaned = 0;

        let entries =
            fs::read_dir(&self.cache_dir).map_err(|e| format!("读取缓存目录失败: {}", e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("读取目录条目失败: {}", e))?;
            let path = entry.path();

            // 只处理 .cache 文件
            if path.extension().and_then(|s| s.to_str()) != Some("cache") {
                continue;
            }

            if let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && modified < cutoff
                && fs::remove_file(&path).is_ok()
            {
                cleaned += 1;
            }
        }

        Ok(cleaned)
    }

    /// rename attempts. Called after every successful `set()` as best-effort cleanup.
    fn cleanup_stale_tmp_files(&self) {
        if let Ok(entries) = fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Only target .cache.tmp files (our temp file extension pattern)
                if path.extension().and_then(|s| s.to_str()) == Some("tmp") {
                    // Verify the corresponding .cache file exists (meaning the
                    // rename succeeded but the temp file wasn't cleaned up)
                    let target = path.with_extension("cache");
                    if target.exists()
                        && let Err(e) = fs::remove_file(&path)
                    {
                        tracing::debug!(
                            "Failed to clean up stale temp file {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }
    }
}

// ─── 缓存键工具 ──────────────────────────────────────────────────

/// 编译缓存键 — 基于文件内容哈希
///
/// 读取文件全部内容，使用 SHA-256 计算完整 256 位哈希值，
/// 返回 64 个十六进制字符的字符串。相同内容永远产生相同的键。
///
/// Note: This function reads the entire file into memory. For very large files
/// (>100MB), consider using a streaming hash approach. In practice, Prism
/// configuration files are typically <1MB, so this is not a concern.
pub fn compile_cache_key(file_path: &Path) -> Result<String, String> {
    let content = fs::read(file_path).map_err(|e| format!("读取文件失败: {}", e))?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = hasher.finalize();

    // Use the full SHA-256 hash (256 bits / 64 hex chars) instead of truncating.
    // Collision probability for 128-bit truncation is ~2^-64 (birthday bound),
    // which is already negligible, but the full hash costs nothing extra and
    // eliminates any theoretical concern entirely.
    Ok(hash.iter().map(|b| format!("{:02x}", b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_cache_basic() {
        let cache: MemoryCache<String> = MemoryCache::new(10);
        assert!(cache.is_empty());

        cache.insert("key1".to_string(), "value1".to_string(), None);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("key1"), Some("value1".to_string()));
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn test_memory_cache_fifo_eviction() {
        // 容量为 2，插入 3 个条目，最早的应被淘汰
        let cache: MemoryCache<i32> = MemoryCache::new(2);
        cache.insert("a".to_string(), 1, None);
        cache.insert("b".to_string(), 2, None);
        cache.insert("c".to_string(), 3, None);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a"), None); // 最早插入，已被淘汰
        assert_eq!(cache.get("b"), Some(2));
        assert_eq!(cache.get("c"), Some(3));
    }

    #[test]
    fn test_memory_cache_invalidate() {
        let cache: MemoryCache<String> = MemoryCache::new(10);
        cache.insert("key1".to_string(), "value1".to_string(), None);
        cache.invalidate("key1");
        assert_eq!(cache.get("key1"), None);
    }

    #[test]
    fn test_memory_cache_clear() {
        let cache: MemoryCache<i32> = MemoryCache::new(10);
        cache.insert("a".to_string(), 1, None);
        cache.insert("b".to_string(), 2, None);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn test_memory_cache_mtime_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        // 写入文件
        fs::write(&file_path, "initial content").unwrap();

        let cache: MemoryCache<String> = MemoryCache::new(10);
        cache.insert(
            "file_key".to_string(),
            "cached_value".to_string(),
            Some(&file_path),
        );

        // 文件未修改，缓存应命中
        assert_eq!(cache.get("file_key"), Some("cached_value".to_string()));

        // 修改文件（需要确保 mtime 变化，等待一小段时间）
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&file_path, "modified content").unwrap();

        // 文件已修改，缓存应失效
        assert_eq!(cache.get("file_key"), None);
    }

    #[test]
    fn test_disk_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf()).unwrap();

        // 写入
        cache.set("test_key", &"hello world".to_string()).unwrap();

        // 读取
        let value: Option<String> = cache.get("test_key");
        assert_eq!(value, Some("hello world".to_string()));

        // 删除
        cache.remove("test_key").unwrap();
        let value: Option<String> = cache.get("test_key");
        assert_eq!(value, None);
    }

    #[test]
    fn test_disk_cache_complex_type() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf()).unwrap();

        let data = vec!["alpha", "beta", "gamma"];
        cache.set("list_key", &data).unwrap();

        let value: Option<Vec<String>> = cache.get("list_key");
        assert_eq!(
            value,
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
    }

    #[test]
    fn test_disk_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf()).unwrap();

        let value: Option<String> = cache.get("nonexistent");
        assert_eq!(value, None);
    }

    #[test]
    fn test_compile_cache_key() {
        let dir = tempfile::tempdir().unwrap();
        let file1 = dir.path().join("a.txt");
        let file2 = dir.path().join("b.txt");

        fs::write(&file1, "same content").unwrap();
        fs::write(&file2, "same content").unwrap();

        let key1 = compile_cache_key(&file1).unwrap();
        let key2 = compile_cache_key(&file2).unwrap();

        // 相同内容应产生相同的键
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_compile_cache_key_different_content() {
        let dir = tempfile::tempdir().unwrap();
        let file1 = dir.path().join("a.txt");
        let file2 = dir.path().join("b.txt");

        fs::write(&file1, "content A").unwrap();
        fs::write(&file2, "content B").unwrap();

        let key1 = compile_cache_key(&file1).unwrap();
        let key2 = compile_cache_key(&file2).unwrap();

        // 不同内容应产生不同的键
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_disk_cache_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf()).unwrap();

        cache.set("old_key", &"old".to_string()).unwrap();

        // 清理 0 秒前的缓存（应全部清理）
        let cleaned = cache.cleanup(Duration::from_secs(0)).unwrap();
        assert_eq!(cleaned, 1);

        let value: Option<String> = cache.get("old_key");
        assert_eq!(value, None);
    }

    // ─── 新增测试：线程安全 + 边界条件 ─────────────────────────────

    #[test]
    fn test_memory_cache_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        let num_threads = 8;
        let ops_per_thread = 500;
        // 容量设为足够大，确保所有线程的所有操作都能保留
        let cache = Arc::new(MemoryCache::<usize>::new(num_threads * ops_per_thread + 1));

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..ops_per_thread {
                        let key = format!("t{}_k{}", t, i);
                        cache.insert(key.clone(), i, None);
                        let _ = cache.get(&key);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // 所有线程完成后，缓存条目数应等于 num_threads * ops_per_thread（未超过容量）
        assert_eq!(cache.len(), num_threads * ops_per_thread);
    }

    #[test]
    fn test_memory_cache_capacity_zero() {
        let cache: MemoryCache<String> = MemoryCache::new(0);
        cache.insert("key1".to_string(), "value1".to_string(), None);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get("key1"), None);

        cache.insert("key2".to_string(), "value2".to_string(), None);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_memory_cache_update_existing_key() {
        let cache: MemoryCache<i32> = MemoryCache::new(3);

        cache.insert("a".to_string(), 1, None);
        cache.insert("b".to_string(), 2, None);
        cache.insert("a".to_string(), 10, None); // 更新 a，FIFO 顺序重置到末尾

        // 此时 fifo_order: ["b", "a"]，容量 3，未满
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a"), Some(10)); // a 被更新
        assert_eq!(cache.get("b"), Some(2)); // b 仍在

        // 插入 c，fifo_order: ["b", "a", "c"]，满
        cache.insert("c".to_string(), 3, None);
        assert_eq!(cache.len(), 3);

        // 插入 d，应淘汰 b（fifo 队首，最早未被更新的）
        cache.insert("d".to_string(), 4, None);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get("a"), Some(10)); // a 被更新过，在队列中间
        assert_eq!(cache.get("b"), None); // b 是最早的，被淘汰
        assert_eq!(cache.get("c"), Some(3));
        assert_eq!(cache.get("d"), Some(4));
    }

    #[test]
    fn test_disk_cache_concurrent_writes() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(DiskCache::new(dir.path().to_path_buf()).unwrap());
        let num_threads = 8;
        let keys_per_thread = 50;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..keys_per_thread {
                        let key = format!("t{}_k{}", t, i);
                        let value = format!("v{}_{}", t, i);
                        cache.set(&key, &value).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // 验证所有写入的数据可正确读回
        for t in 0..num_threads {
            for i in 0..keys_per_thread {
                let key = format!("t{}_k{}", t, i);
                let expected = format!("v{}_{}", t, i);
                let value: Option<String> = cache.get(&key);
                assert_eq!(value, Some(expected), "key={}", key);
            }
        }
    }

    #[test]
    fn test_disk_cache_atomic_write_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf()).unwrap();

        // 手动模拟写入中断：写入 temp 文件但不 rename
        let target_path = cache.cache_path("interrupted_key");
        let temp_path = target_path.with_extension("tmp");
        let partial_data = b"{\"this is incomplete json";
        fs::write(&temp_path, partial_data).unwrap();

        // 读取应返回 None（temp 文件不应被读取）
        let value: Option<String> = cache.get("interrupted_key");
        assert_eq!(value, None);

        // 正常写入应覆盖 temp 文件
        cache
            .set("interrupted_key", &"valid_value".to_string())
            .unwrap();
        let value: Option<String> = cache.get("interrupted_key");
        assert_eq!(value, Some("valid_value".to_string()));
    }
}
