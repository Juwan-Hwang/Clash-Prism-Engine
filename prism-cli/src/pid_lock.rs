//! PID 文件锁 — 防止多个 Prism 实例同时操作同一个工作区
//!
//! 设计原则：
//!
//! - 使用 PID 文件（`<lock_dir>/prism.lock`）标识持有锁的进程
//! - 启动时检查 PID 文件，如果进程存在则拒绝启动
//! - 退出时通过 `Drop` 自动清理 PID 文件
//! - 支持强制覆盖（`--force` 参数）
//!
//! 平台兼容性：
//! - Unix: 使用 `libc::kill(pid, 0)` 检测进程是否存活
//! - Windows: 使用 `CreateToolhelp32Snapshot` 遍历进程列表检测进程是否存活

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// PidLock — PID 文件锁
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// PID 文件锁
///
/// 通过在指定目录创建 `prism.lock` 文件来互斥访问工作区。
/// 文件内容为当前进程的 PID（十进制 ASCII）。
///
/// # 生命周期
///
/// ```ignore
/// let lock = PidLock::acquire(&lock_dir, false)?;
/// // ... 执行工作 ...
/// // lock 离开作用域时自动释放（删除 PID 文件）
/// ```
///
/// # 错误处理
///
/// - 如果 `lock_dir` 不存在或无法创建，返回错误
/// - 如果已有锁且持有者进程仍在运行且 `force=false`，返回错误
/// - 如果 `force=true`，强制覆盖已有锁
pub struct PidLock {
    /// 锁文件路径
    lock_file: PathBuf,
    /// 是否成功获取锁
    acquired: bool,
}

impl PidLock {
    /// 尝试获取锁
    ///
    /// # 参数
    /// - `lock_dir`: 锁文件所在目录（会自动创建）
    /// - `force`: 是否强制覆盖已有锁
    ///
    /// # 返回
    /// - `Ok(PidLock)`: 成功获取锁
    /// - `Err(String)`: 锁被其他进程持有，或文件系统错误
    pub fn acquire(lock_dir: &Path, force: bool) -> Result<Self, String> {
        // 1. 确保 lock_dir 存在
        fs::create_dir_all(lock_dir)
            .map_err(|e| format!("创建锁目录失败 [{}]: {}", lock_dir.display(), e))?;

        let lock_file = lock_file_path(lock_dir);

        // 消除 TOCTOU 竞态条件。
        // 直接尝试原子创建（create_new），仅在文件已存在时才检查 PID 状态。
        // 如果 PID 已过期或 force=true，删除残留文件后重试一次。
        let current_pid = std::process::id();
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_file)
        {
            Ok(mut file) => {
                use std::io::Write;
                write!(file, "{}", current_pid).map_err(|e| format!("写入 PID 文件失败: {}", e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // 文件已存在，检查是否为残留的过期 PID
                if let Ok(existing_pid) = read_pid_from_file(&lock_file) {
                    if is_process_alive(existing_pid) && !force {
                        return Err(format!(
                            "另一个 Prism 实例正在运行 (PID: {})，\
                             使用 --force 强制获取锁",
                            existing_pid
                        ));
                    }
                }
                // 进程已不存在（残留的 PID 文件）或 force=true，删除后重新创建
                let _ = fs::remove_file(&lock_file);

                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lock_file)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        write!(file, "{}", current_pid)
                            .map_err(|e| format!("写入 PID 文件失败: {}", e))?;
                    }
                    Err(_) => {
                        // 极端竞态：另一个进程在我们删除和创建之间抢占了锁
                        return Err(
                            "无法获取 PID 锁（文件被占用），请稍后重试或使用 --force".to_string()
                        );
                    }
                }
            }
            Err(e) => {
                return Err(format!("创建 PID 文件失败: {}", e));
            }
        }

        Ok(Self {
            lock_file,
            acquired: true,
        })
    }

    /// 释放锁（删除 PID 文件）
    ///
    /// 可手动调用，也可通过 `Drop` 自动调用。
    /// 重复调用是安全的（幂等）。
    pub fn release(&mut self) {
        if self.acquired {
            let _ = fs::remove_file(&self.lock_file);
            self.acquired = false;
        }
    }

    /// 是否已获取锁
    #[allow(dead_code)] // 公开 API，供外部消费者使用
    pub fn is_acquired(&self) -> bool {
        self.acquired
    }

    /// 获取锁文件路径
    #[allow(dead_code)] // 公开 API，供外部消费者使用
    pub fn lock_file_path(&self) -> &Path {
        &self.lock_file
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        self.release();
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 内部辅助函数
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 获取锁文件路径
fn lock_file_path(lock_dir: &Path) -> PathBuf {
    lock_dir.join("prism.lock")
}

/// 从 PID 文件读取 PID 值
fn read_pid_from_file(path: &Path) -> Result<u32, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("读取 PID 文件失败: {}", e))?;
    content
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("解析 PID 失败: {}", e))
}

/// 将 PID 写入文件
///
/// 保留原因：测试中使用（test_pid_lock_stale_process），
/// 用于构造指向不存在进程的残留 PID 文件以验证自动清理逻辑。
#[allow(dead_code)]
fn write_pid_to_file(path: &Path, pid: u32) -> io::Result<()> {
    // 使用原子写入：先写临时文件，再 rename
    let tmp_path = path.with_extension("lock.tmp");
    fs::write(&tmp_path, pid.to_string())?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// 检查指定 PID 的进程是否还在运行
///
/// - Unix: 使用 `libc::kill(pid, 0)` — 信号 0 不发送信号，
///   仅检查进程是否存在。返回 `true` 表示进程存在。
/// - Windows: 使用 `CreateToolhelp32Snapshot` 遍历进程列表检测进程是否存活。
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) 是标准的进程存在性检查，
    // 不会对目标进程产生任何副作用。
    // pid_t 在所有 Unix 平台上都是 i32（或 c_int），
    // u32 → i32 的转换对合理 PID 值是安全的。
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Windows 平台：使用 CreateToolhelp32Snapshot 遍历进程列表检查进程是否存在
#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    use std::mem;

    #[repr(C)]
    struct ProcessEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u8; 260],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        fn Process32First(h_snapshot: isize, entry: *mut ProcessEntry32) -> i32;
        fn Process32Next(h_snapshot: isize, entry: *mut ProcessEntry32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const INVALID_HANDLE_VALUE: isize = -1;

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }

        let mut entry = ProcessEntry32 {
            dw_size: mem::size_of::<ProcessEntry32>() as u32,
            ..mem::zeroed()
        };

        let mut found = false;

        if Process32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32_process_id == pid {
                    found = true;
                    break;
                }
                if Process32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
        found
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 创建临时测试目录
    fn temp_lock_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prism_test_pid_lock_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// 清理临时测试目录
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_pid_lock_acquire_release() {
        let dir = temp_lock_dir();
        let lock = PidLock::acquire(&dir, false);
        assert!(lock.is_ok(), "首次获取锁应成功");

        let mut lock = lock.unwrap();
        assert!(lock.is_acquired());
        assert!(lock.lock_file_path().exists());

        // 手动释放
        lock.release();
        assert!(!lock.is_acquired());
        assert!(!lock.lock_file_path().exists());

        cleanup(&dir);
    }

    #[test]
    fn test_pid_lock_drop_auto_release() {
        let dir = temp_lock_dir();
        {
            let lock = PidLock::acquire(&dir, false).unwrap();
            assert!(lock.lock_file_path().exists());
            // lock 离开作用域，Drop 自动释放
        }
        // 验证锁文件已被删除
        assert!(!lock_file_path(&dir).exists(), "Drop 后锁文件应被自动删除");
        cleanup(&dir);
    }

    #[test]
    fn test_pid_lock_force() {
        let dir = temp_lock_dir();

        // 第一次获取锁
        let _lock1 = PidLock::acquire(&dir, false).unwrap();

        // 第二次获取锁（不强制）— 应失败，因为当前进程 PID 文件存在且进程存活
        let result2 = PidLock::acquire(&dir, false);
        // 当前进程的 PID 文件存在且进程（自己）还在运行，所以应该失败
        assert!(result2.is_err(), "不强制时应无法获取已被持有的锁");

        // 强制获取 — 应成功
        let result3 = PidLock::acquire(&dir, true);
        assert!(result3.is_ok(), "强制获取锁应成功");

        cleanup(&dir);
    }

    #[test]
    fn test_pid_lock_stale_process() {
        let dir = temp_lock_dir();

        // 手动创建一个指向不存在进程的 PID 文件
        let lock_file = lock_file_path(&dir);
        // 使用一个不太可能存在的 PID（PID 1 在 Linux 上是 init/systemd，但超大 PID 通常不存在）
        let stale_pid: u32 = 7_999_999;
        write_pid_to_file(&lock_file, stale_pid).unwrap();

        // 尝试获取锁（不强制）— 应成功，因为旧进程已不存在
        let result = PidLock::acquire(&dir, false);
        assert!(
            result.is_ok(),
            "过期 PID 的锁应能被自动覆盖: {:?}",
            result.err()
        );

        cleanup(&dir);
    }

    #[test]
    fn test_read_pid_from_file_valid() {
        let dir = temp_lock_dir();
        let lock_file = lock_file_path(&dir);
        fs::write(&lock_file, "12345\n").unwrap();

        let pid = read_pid_from_file(&lock_file).unwrap();
        assert_eq!(pid, 12345);

        cleanup(&dir);
    }

    #[test]
    fn test_read_pid_from_file_invalid() {
        let dir = temp_lock_dir();
        let lock_file = lock_file_path(&dir);
        fs::write(&lock_file, "not_a_number\n").unwrap();

        let result = read_pid_from_file(&lock_file);
        assert!(result.is_err());

        cleanup(&dir);
    }

    #[test]
    fn test_read_pid_from_file_missing() {
        let dir = temp_lock_dir();
        let lock_file = lock_file_path(&dir);

        let result = read_pid_from_file(&lock_file);
        assert!(result.is_err());

        cleanup(&dir);
    }

    #[test]
    fn test_is_process_alive_self() {
        // 当前进程一定存活
        assert!(
            is_process_alive(std::process::id()),
            "当前进程应被检测为存活"
        );
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        // 一个几乎不可能存在的 PID
        assert!(!is_process_alive(7_999_999), "不存在的进程应返回 false");
    }
}
