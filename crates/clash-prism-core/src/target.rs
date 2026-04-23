//! Target Compiler — 将最终配置输出为目标内核格式
//!
//! ## 职责（架构 §1 管线末尾）
//!
//! Patch Executor 输出的 JSON 配置经过 Validator 校验后，
//! 由 Target Compiler 序列化为目标内核可识别的格式：
//!
//! - **mihomo** (YAML) — Clash Meta 内核的标准配置格式
//! - **clash-rs** (YAML) — Rust 重写的 Clash 内核，字段略有差异
//! - **json** (JSON) — 原始 JSON 输出（调试用途）
//!
//! ## 设计决策
//!
//! 1. mihomo 和 clash-rs 的 YAML 格式高度兼容（95%+ 字段相同），
//!    差异主要集中在少量扩展字段上。
//! 2. Target Compiler 不做语义转换，只做序列化格式适配。
//! 3. 字段重命名/映射通过 `FieldMapping` 表驱动，便于扩展新内核。

use crate::error::Result;
use crate::json_path::{get_json_path_mut, set_json_path};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 目标内核类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TargetCore {
    /// mihomo (Clash Meta) — YAML 格式
    #[default]
    Mihomo,
    /// clash-rs — YAML 格式（与 mihomo 高度兼容）
    ClashRs,
    /// 原始 JSON — 调试用途
    Json,
}

impl std::fmt::Display for TargetCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mihomo => write!(f, "mihomo"),
            Self::ClashRs => write!(f, "clash-rs"),
            Self::Json => write!(f, "json"),
        }
    }
}

/// Target Compiler — 配置输出编译器
///
/// 将 Prism Engine 执行后的最终 JSON 配置转换为
/// 目标内核可加载的文件格式。
pub struct TargetCompiler {
    /// 目标内核类型
    target: TargetCore,
    /// 是否美化输出（缩进、换行等）
    pretty: bool,
}

impl TargetCompiler {
    /// 创建针对 mihomo 内核的编译器
    pub fn mihomo() -> Self {
        Self {
            target: TargetCore::Mihomo,
            pretty: true,
        }
    }

    /// Clean up stale temporary files left from previous atomic_write calls.
    ///
    /// On process crash between writing the temp file and renaming it,
    /// `.prism.tmp.*` files may remain on disk. This method scans the target
    /// file's parent directory and removes any such orphaned temp files.
    ///
    /// Should be called once at startup.
    pub fn cleanup_stale_temp_files(target_path: &std::path::Path) {
        if let Some(parent) = target_path.parent()
            && let Ok(entries) = std::fs::read_dir(parent)
        {
            let file_name = target_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // deleting unrelated files that happen to start with the same
                // name (e.g., "config.yaml.bak" would match the old loose check).
                // The exact temp file pattern is: "{target_filename}.prism.tmp.{uuid}"
                let expected_prefix = format!("{}.prism.tmp.", file_name);
                if name_str.starts_with(&expected_prefix) {
                    if let Err(e) = std::fs::remove_file(entry.path()) {
                        tracing::debug!(
                            path = %entry.path().display(),
                            error = %e,
                            "Failed to clean up stale temp file"
                        );
                    } else {
                        tracing::info!(
                            path = %entry.path().display(),
                            "Cleaned up stale temp file from previous crash"
                        );
                    }
                }
            }
        }
    }

    /// 创建针对 clash-rs 内核的编译器
    pub fn clash_rs() -> Self {
        Self {
            target: TargetCore::ClashRs,
            pretty: true,
        }
    }

    /// 创建 JSON 输出编译器（调试用）
    pub fn json_output() -> Self {
        Self {
            target: TargetCore::Json,
            pretty: true,
        }
    }

    /// 使用自定义目标创建编译器
    pub fn with_target(target: TargetCore) -> Self {
        Self {
            target,
            pretty: true,
        }
    }

    /// 设置是否美化输出
    pub fn pretty(mut self, pretty: bool) -> Self {
        self.pretty = pretty;
        self
    }

    /// 编译最终配置为目标格式的字符串
    ///
    /// # Arguments
    /// * `config` — Patch Executor 输出的最终 JSON 配置
    ///
    /// # Returns
    /// 目标内核可直接加载的配置文本（YAML 或 JSON）
    pub fn compile(&self, config: &Value) -> Result<String> {
        match self.target {
            TargetCore::Mihomo => self.compile_to_yaml(config, CoreFlavor::Mihomo),
            TargetCore::ClashRs => self.compile_to_yaml(config, CoreFlavor::ClashRs),
            TargetCore::Json => self.compile_to_json(config),
        }
    }

    /// 编译并写入到文件（普通写入）
    pub fn compile_to_file(&self, config: &Value, path: &std::path::Path) -> Result<()> {
        let output = self.compile(config)?;
        std::fs::write(path, output)?;
        Ok(())
    }

    /// §4.6 原子写入配置文件
    ///
    /// 先将内容写入同目录下的临时文件（`.tmp` 后缀），
    /// 然后通过 `rename` 原子替换目标文件。
    ///
    /// `rename` 在大多数文件系统（NTFS、ext4、APFS）上是原子操作，
    /// 保证：读取方要么看到旧文件完整内容，要么看到新文件完整内容，
    /// 绝不会遇到"写了一半"的半成品配置。
    ///
    /// # Arguments
    /// * `config` — 最终配置 JSON
    /// * `path` — 目标配置文件路径
    ///
    /// # Returns
    /// 写入的字节数（内容字节数，即 `output.len()`，非磁盘实际写入字节数。
    /// 磁盘写入字节数可能因文件系统块对齐、压缩等因素而不同。）
    pub fn atomic_write(&self, config: &Value, path: &std::path::Path) -> Result<usize> {
        let output = self.compile(config)?;
        let bytes = output.len();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                crate::error::PrismError::TargetOutput {
                    message: format!("无法创建目标目录 {}: {}", parent.display(), e),
                }
            })?;
        }

        // 生成临时文件路径（同目录下，加随机后缀 .tmp）
        // 使用 simple() 格式（无连字符，32字符）避免路径过长
        let random_suffix = uuid::Uuid::new_v4().simple().to_string();
        let tmp_path =
            std::path::PathBuf::from(format!("{}.prism.tmp.{}", path.display(), random_suffix));

        // 1. 写入临时文件（OpenOptions + write_all + sync_all 确保数据落盘）
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| crate::error::PrismError::TargetOutput {
                message: format!("创建临时文件失败 {}: {}", tmp_path.display(), e),
            })?;
        file.write_all(output.as_bytes())
            .map_err(|e| crate::error::PrismError::TargetOutput {
                message: format!("写入临时文件失败 {}: {}", tmp_path.display(), e),
            })?;
        file.sync_all()
            .map_err(|e| crate::error::PrismError::TargetOutput {
                message: format!("sync 临时文件失败 {}: {}", tmp_path.display(), e),
            })?;

        // When atomic_write replaces an existing file, the temp file may have
        // restrictive permissions (e.g., 0600). We preserve the original file's
        // permissions so the replacement has the same access mode.
        // to eliminate TOCTOU race condition between the two syscalls.
        if let Ok(metadata) = std::fs::metadata(path) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode();
                let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode));
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::set_permissions(&tmp_path, metadata.permissions().clone());
            }
        }

        // 2. 原子 rename 替换目标文件
        // 注意：跨文件系统时 rename 非原子操作（会失败并返回 EXDEV），
        // 此处已实现 copy + delete 回退逻辑。残留临时文件由
        // `cleanup_stale_temp_files` 统一清理。
        if let Err(rename_err) = std::fs::rename(&tmp_path, path) {
            // 检查是否是跨文件系统错误
            // EXDEV (errno 18) 仅在 Unix 系统上定义；非 Unix 平台直接走 copy+delete 回退
            #[cfg(unix)]
            let is_cross_fs = rename_err.raw_os_error() == Some(libc::EXDEV);
            #[cfg(not(unix))]
            let is_cross_fs = true;
            if is_cross_fs {
                tracing::debug!(
                    target = "clash_prism_core",
                    "跨文件系统 rename 失败，回退到 copy + delete"
                );
                // copy + delete 失败时记录临时文件路径以便后续清理
                if let Err(copy_err) = std::fs::copy(&tmp_path, path) {
                    tracing::error!(
                        target = "clash_prism_core",
                        tmp_path = %tmp_path.display(),
                        "跨文件系统复制失败，临时文件残留: {}",
                        copy_err
                    );
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(crate::error::PrismError::TargetOutput {
                        message: format!(
                            "跨文件系统复制失败 ({} → {}): {}",
                            tmp_path.display(),
                            path.display(),
                            copy_err
                        ),
                    });
                }
                if let Err(del_err) = std::fs::remove_file(&tmp_path) {
                    tracing::warn!(
                        target = "clash_prism_core",
                        tmp_path = %tmp_path.display(),
                        "临时文件删除失败（可手动清理）: {}",
                        del_err
                    );
                }
            } else {
                // 清理临时文件（尽力而为）
                let _ = std::fs::remove_file(&tmp_path);
                return Err(crate::error::PrismError::TargetOutput {
                    message: format!(
                        "原子替换失败 ({} → {}): {}",
                        tmp_path.display(),
                        path.display(),
                        rename_err
                    ),
                });
            }
        }

        tracing::info!(
            target = "clash_prism_core",
            path = %path.display(),
            size_bytes = bytes,
            "配置文件原子写入完成"
        );

        Ok(bytes)
    }

    /// §4.6 通知内核热重载
    ///
    /// 向运行中的内核发送信号通知其重新加载配置。
    /// 具体实现取决于部署方式：
    ///
    /// - **Tauri 桌面应用**：通过内部命令通道通知
    /// - **独立 CLI**：发送 SIGHUP / SIGUSR1 信号
    /// - **Docker/服务**：通过 HTTP API 调用 `/configs?force=true`
    ///
    /// 此方法提供统一接口，实际策略由 `HotReloadStrategy` 决定。
    pub fn notify_hot_reload(
        &self,
        config_path: &std::path::Path,
        strategy: HotReloadStrategy,
    ) -> Result<HotReloadResult> {
        match strategy {
            HotReloadStrategy::Signal { pid } => self.reload_by_signal(pid),
            HotReloadStrategy::HttpApi { url } => self.reload_by_http_api(&url, config_path),
            HotReloadStrategy::None => {
                tracing::info!(
                    target = "clash_prism_core",
                    "热重载已禁用（None 策略），跳过通知"
                );
                Ok(HotReloadResult {
                    strategy: "none".to_string(),
                    success: true,
                    detail: "热重载已禁用，配置文件已更新但未通知内核".to_string(),
                })
            }
        }
    }

    /// 发送 SIGHUP 信号通知进程重新加载配置。
    ///
    /// # Safety
    ///
    /// **调用方必须确保 PID 正确性。** 此方法会验证 PID 指向的进程是否存在
    /// （通过 `/proc/{pid}/comm` 检查进程名），但无法保证 PID 指向的是预期的
    /// 目标进程（PID 可能被回收）。在高安全性要求的场景中，调用方应使用
    /// HTTP API 热重载策略代替信号策略。
    ///
    /// # Platform
    ///
    /// 仅在 Unix-like 系统上可用（Linux / macOS / BSD）。
    /// Windows 平台会返回错误。
    #[cfg_attr(not(unix), allow(unused_variables))]
    fn reload_by_signal(&self, pid: u32) -> Result<HotReloadResult> {
        #[cfg(unix)]
        {
            // 检查 PID 有效性（PID=0 表示发给所有进程，不允许）
            if pid == 0 {
                return Err(crate::error::PrismError::TargetOutput {
                    message: "无效的 PID: 0（不允许向所有进程广播信号）".to_string(),
                });
            }

            // 向 PID 1 发送 SIGHUP 可能导致系统级服务重启或不可预期的行为。
            // 在容器环境中，PID 1 通常是应用进程本身，但仍然不应由
            // Prism Engine 自动发送信号，应由用户或编排系统（如 Docker/K8s）管理。
            if pid == 1 {
                return Err(crate::error::PrismError::TargetOutput {
                    message: "不允许向 PID 1 (init/systemd) 发送信号".to_string(),
                });
            }

            if pid > i32::MAX as u32 {
                return Err(crate::error::PrismError::TargetOutput {
                    message: format!(
                        "PID {} exceeds i32::MAX ({}), cannot send POSIX signal",
                        pid,
                        i32::MAX
                    ),
                });
            }

            // PID 身份验证：通过 /proc/{pid}/comm 检查进程是否存在。
            // 这不能保证 PID 指向的是预期的目标进程（PID 可能被回收），
            // 但可以快速检测明显的无效 PID（进程不存在）。
            //
            // macOS 没有 /proc 文件系统，使用 kill(pid, 0) 作为替代验证方式。
            // kill(pid, 0) 不发送信号，仅检查进程是否存在（ESRCH = 不存在）。
            // 注意：kill(pid, 0) 仍有 PID 回收的 TOCTOU 竞争，但比完全跳过验证更安全。
            //
            // 策略：先尝试读取 /proc/{pid}/comm（Linux 特有，可获取进程名），
            // 若 /proc 不可用（macOS / BSD 等非 Linux 平台），回退到 kill(pid, 0)。
            // /proc 读取失败在 macOS 上是预期行为，因此使用 debug 级别日志。
            let proc_comm_path = format!("/proc/{}/comm", pid);
            if let Ok(comm) = std::fs::read_to_string(&proc_comm_path) {
                let comm_name = comm.trim();
                tracing::debug!(
                    pid = pid,
                    comm = comm_name,
                    "PID 验证: 进程存在 (comm={comm_name}, via /proc)"
                );
            } else {
                // /proc 不可用是 macOS / BSD 等非 Linux 平台的预期行为，
                // 不是异常情况，因此使用 debug 级别日志而非 warn。
                // 回退到 kill(pid, 0) 进行进程存在性检查。
                tracing::debug!(
                    pid = pid,
                    "/proc 不可用（非 Linux 平台预期行为），回退到 kill(pid, 0) 验证"
                );
                let check_ret = unsafe { libc::kill(pid as i32, 0) };
                if check_ret == 0 {
                    tracing::debug!(
                        pid = pid,
                        "PID 验证: 进程存在 (via kill(pid, 0) 回退，/proc 不可用)"
                    );
                } else if check_ret == libc::ESRCH {
                    tracing::warn!(
                        pid = pid,
                        "PID 验证失败: 进程不存在 (kill(pid, 0) 返回 ESRCH)。\
                         /proc 不可用，已使用 kill(pid, 0) 回退验证。\
                         调用方需确保 PID 正确性。"
                    );
                    return Err(crate::error::PrismError::TargetOutput {
                        message: format!(
                            "目标进程不存在（PID={}），kill(pid, 0) 返回 ESRCH。\
                             可能已退出或 PID 错误。",
                            pid
                        ),
                    });
                } else {
                    // EPERM 等其他错误：进程可能存在但当前用户无权发送信号。
                    // 不视为致命错误，继续执行（后续实际 SIGHUP 会给出明确错误）。
                    tracing::warn!(
                        pid = pid,
                        error_code = check_ret,
                        "PID 验证: kill(pid, 0) 返回非 ESRCH 错误，进程可能存在但无权限。\
                         /proc 不可用，无法读取进程名。调用方需确保 PID 正确性。"
                    );
                }
            }

            // 发送 SIGHUP 通知进程重新加载配置
            let ret = unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            match ret {
                0 => Ok(HotReloadResult {
                    strategy: "signal".to_string(),
                    success: true,
                    detail: format!("已向 PID {} 发送 SIGHUP 信号", pid),
                }),
                libc::ESRCH => {
                    // ESRCH = 进程不存在 (No such process)
                    Err(crate::error::PrismError::TargetOutput {
                        message: format!("目标进程不存在（PID={}），可能已退出或 PID 错误", pid),
                    })
                }
                libc::EPERM => {
                    // EPERM = 权限不足 (Operation not permitted)
                    Err(crate::error::PrismError::TargetOutput {
                        message: format!(
                            "权限不足，无法向 PID {} 发送信号（可能需要 root/管理员权限）",
                            pid
                        ),
                    })
                }
                _ => Err(crate::error::PrismError::TargetOutput {
                    message: format!("发送 SIGHUP 信号失败，PID={}，错误码={}", pid, ret),
                }),
            }
        }
        #[cfg(not(unix))]
        {
            // Windows 平台不支持 POSIX 信号
            Err(crate::error::PrismError::TargetOutput {
                message: "Signal 热重载策略仅在 Unix-like 系统上可用".to_string(),
            })
        }
    }

    #[cfg(feature = "http-reload")]
    fn reload_by_http_api(
        &self,
        url: &str,
        _config_path: &std::path::Path,
    ) -> Result<HotReloadResult> {
        // 通过 mihomo/clash-rs 的 RESTful API 触发热重载
        // PUT /configs?force=true
        //
        // Uses ureq (pure-Rust sync HTTP client) instead of
        // reqwest::blocking. ureq does not depend on tokio's I/O driver,
        // so it will not block the async runtime even when called from
        // a tokio context.
        let reload_url = format!("{}/configs?force=true", url.trim_end_matches('/'));
        match ureq::put(&reload_url).call() {
            Ok(resp) => Ok(HotReloadResult {
                strategy: "http-api".to_string(),
                success: true,
                detail: format!("HTTP API 热重载成功 (status={})", resp.status()),
            }),
            Err(ureq::Error::Status(code, _resp)) => Err(crate::error::PrismError::TargetOutput {
                message: format!("HTTP API 返回错误: {}", code),
            }),
            Err(e) => Err(crate::error::PrismError::TargetOutput {
                message: format!("HTTP API 请求失败: {}", e),
            }),
        }
    }

    #[cfg(not(feature = "http-reload"))]
    fn reload_by_http_api(
        &self,
        url: &str,
        _config_path: &std::path::Path,
    ) -> Result<HotReloadResult> {
        // 无 http-reload feature 时返回提示信息
        Ok(HotReloadResult {
            strategy: "http-api".to_string(),
            success: false,
            detail: format!(
                "HTTP API 热重载需要启用 http-reload feature (target url: {})",
                url
            ),
        })
    }

    /// 便捷方法：原子写入 + 热重载一步完成（§4.6 完整输出流程）
    pub fn write_and_reload(
        &self,
        config: &Value,
        path: &std::path::Path,
        strategy: HotReloadStrategy,
    ) -> Result<(usize, HotReloadResult)> {
        // 步骤 1：原子写入
        let written = self.atomic_write(config, path)?;

        // 步骤 2：通知热重载
        let reload_result = self.notify_hot_reload(path, strategy)?;

        Ok((written, reload_result))
    }

    // ─── 内部实现 ───

    /// 编译为 YAML 格式（mihomo / clash-rs）
    fn compile_to_yaml(&self, config: &Value, flavor: CoreFlavor) -> Result<String> {
        // 1. 应用目标内核的字段映射
        let adapted = self.adapt_for_target(config, flavor);

        // 2. 序列化为 YAML
        let yaml_value = json_to_yaml_value(&adapted)?;

        // 3. 根据pretty设置选择输出格式
        let yaml_str = serde_yml::to_string(&yaml_value)?;
        if self.pretty {
            // pretty 模式：保留 serde_yml 默认的多行格式化输出（缩进、换行等）
            Ok(yaml_str)
        } else {
            // non-pretty 模式：压缩 YAML，去除多余空行，生成紧凑输出
            Ok(compact_yaml(&yaml_str))
        }
    }

    /// 编译为 JSON 格式
    fn compile_to_json(&self, config: &Value) -> Result<String> {
        if self.pretty {
            Ok(serde_json::to_string_pretty(config)?)
        } else {
            Ok(serde_json::to_string(config)?)
        }
    }

    /// Adapt configuration fields for the target core flavor.
    ///
    /// Uses a table-driven approach ([`field_mappings()`]) to automatically
    /// transform fields that are incompatible between mihomo and clash-rs.
    ///
    /// ## Mapping Actions
    ///
    /// - **Remove**: Field is not supported by the target (e.g., `tun.device` in clash-rs)
    /// - **Rename**: Field has a different name in the target (e.g., `tun.stack` → `tun.interface-name`)
    /// - **MoveTo**: Field value should be moved to a different path
    ///
    /// ## Extensibility
    ///
    /// To add a new field mapping, simply add an entry to the [`field_mappings()`] table.
    /// No changes to this method are needed.
    fn adapt_for_target(&self, config: &Value, flavor: CoreFlavor) -> Value {
        // NOTE: Intentional full clone of `config`. The caller (`compile_to_yaml`)
        // receives a shared reference `&Value` and must not mutate the original.
        // The clone is then modified in-place by the field mapping actions below.
        // This is the simplest correct approach; a zero-copy alternative would
        // require changing the entire compile pipeline to pass `Value` by-value,
        // which is not worth the API breakage for the marginal perf gain.
        let mut adapted = config.clone();

        for mapping in field_mappings() {
            if mapping.target != flavor {
                continue;
            }

            match &mapping.action {
                FieldMappingAction::Remove => {
                    Self::remove_field(&mut adapted, &mapping.path);
                }
                FieldMappingAction::Rename { to } => {
                    Self::rename_field(&mut adapted, &mapping.path, to);
                }
                FieldMappingAction::MoveTo { new_path } => {
                    Self::move_field(&mut adapted, &mapping.path, new_path);
                }
            }
        }

        adapted
    }

    /// Remove a field at the given dot-notation path from a JSON value.
    fn remove_field(config: &mut Value, path: &str) {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() {
            return;
        }

        if parts.len() == 1 {
            if let Some(obj) = config.as_object_mut() {
                obj.remove(parts[0]);
            }
            return;
        }

        // Navigate to parent, then remove child
        let parent_path = parts[..parts.len() - 1].join(".");
        let field_name = parts[parts.len() - 1];

        if let Some(parent) = get_json_path_mut(config, &parent_path)
            && let Some(obj) = parent.as_object_mut()
        {
            obj.remove(field_name);
        }
    }

    /// Rename a field: remove from old path, insert at new path with same value.
    fn rename_field(config: &mut Value, old_path: &str, new_path: &str) {
        let parts: Vec<&str> = old_path.split('.').collect();

        // Get the value at old path
        let value = if parts.len() == 1 {
            config.as_object_mut().and_then(|obj| obj.remove(parts[0]))
        } else {
            let parent_path = parts[..parts.len() - 1].join(".");
            let field_name = parts[parts.len() - 1];
            get_json_path_mut(config, &parent_path)
                .and_then(|parent| parent.as_object_mut())
                .and_then(|obj| obj.remove(field_name))
        };

        if let Some(value) = value {
            // Set at new path
            set_json_path(config, new_path, value);
        }
    }

    /// Move a field value from one path to another.
    fn move_field(config: &mut Value, from_path: &str, to_path: &str) {
        Self::rename_field(config, from_path, to_path);
    }
}

/// 内核变体（用于字段映射）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreFlavor {
    Mihomo,
    ClashRs,
}

/// Field mapping rule for adapting config between different core flavors.
#[derive(Debug, Clone)]
struct FieldMapping {
    /// JSON path to the field (dot-notation, e.g., "tun.device")
    path: String,
    /// Target flavor where this mapping applies
    target: CoreFlavor,
    /// Mapping action
    action: FieldMappingAction,
}

/// Action to perform for a field mapping.
#[derive(Debug, Clone)]
enum FieldMappingAction {
    /// Remove the field entirely (not supported by target)
    Remove,
    /// Rename the field to a new name
    Rename { to: String },
    /// 将字段值移动到嵌套路径（预留功能，暂未使用）
    #[allow(dead_code)]
    MoveTo { new_path: String },
}

/// Known field differences between mihomo and clash-rs.
///
/// This table drives the `adapt_for_target()` method.
/// Each entry describes a transformation needed when outputting
/// config for a specific target core.
///
/// ## Sources
/// - mihomo wiki: https://wiki.metacubex.one/config/
/// - clash-rs docs: https://clash-rs.github.io/clash-rs/config/
///
/// Cached via `LazyLock` — built once on first access, reused thereafter.
fn field_mappings() -> &'static Vec<FieldMapping> {
    static MAPPINGS: std::sync::LazyLock<Vec<FieldMapping>> = std::sync::LazyLock::new(|| {
        vec![
            // mihomo-specific fields (remove when targeting clash-rs)
            FieldMapping {
                path: "tun.device".to_string(),
                target: CoreFlavor::ClashRs,
                action: FieldMappingAction::Remove,
            },
            FieldMapping {
                path: "tun.stack".to_string(),
                target: CoreFlavor::ClashRs,
                action: FieldMappingAction::Rename {
                    to: "tun.interface-name".to_string(),
                },
            },
            FieldMapping {
                path: "experimental.ignore-certificate-errors".to_string(),
                target: CoreFlavor::ClashRs,
                action: FieldMappingAction::Remove,
            },
            FieldMapping {
                path: "external-controller-tls".to_string(),
                target: CoreFlavor::ClashRs,
                action: FieldMappingAction::Remove,
            },
            FieldMapping {
                path: "secret".to_string(),
                target: CoreFlavor::ClashRs,
                action: FieldMappingAction::Remove,
            },
            // clash-rs-specific fields (remove when targeting mihomo)
            FieldMapping {
                path: "log.level".to_string(),
                target: CoreFlavor::Mihomo,
                action: FieldMappingAction::Rename {
                    to: "log-level".to_string(),
                },
            },
        ]
    });
    &MAPPINGS
}

/// 将 serde_json::Value 转换为 serde_yml::Value
///
/// 这是必要的中间步骤，因为 serde_yml 需要自己的 Value 类型来正确处理：
/// - YAML 的 null vs JSON 的 null
/// - 数字类型的表示差异
/// - 键的排序和格式化
fn json_to_yaml_value(json: &Value) -> Result<serde_yml::Value> {
    match json {
        Value::Null => Ok(serde_yml::Value::Null),
        Value::Bool(b) => Ok(serde_yml::Value::Bool(*b)),
        Value::Number(n) => {
            // 优先尝试整数表示，避免不必要的精度丢失：
            // i64 → u64 → f64，确保整数不会被转为浮点数
            if let Some(i) = n.as_i64() {
                Ok(serde_yml::Value::Number(i.into()))
            } else if let Some(u) = n.as_u64() {
                Ok(serde_yml::Value::Number(u.into()))
            } else if let Some(f) = n.as_f64() {
                let yaml_num = serde_yml::Number::from(f);
                Ok(serde_yml::Value::Number(yaml_num))
            } else {
                Ok(serde_yml::Value::Null)
            }
        }
        Value::String(s) => Ok(serde_yml::Value::String(s.clone())),
        Value::Array(arr) => {
            let yaml_arr: Result<Vec<serde_yml::Value>> =
                arr.iter().map(json_to_yaml_value).collect();
            Ok(serde_yml::Value::Sequence(yaml_arr?))
        }
        Value::Object(map) => {
            let mut yaml_map = serde_yml::Mapping::new();
            for (k, v) in map {
                let key = serde_yml::Value::String(k.clone());
                let value = json_to_yaml_value(v)?;
                yaml_map.insert(key, value);
            }
            Ok(serde_yml::Value::Mapping(yaml_map))
        }
    }
}

impl Default for TargetCompiler {
    fn default() -> Self {
        Self::mihomo()
    }
}

// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════

/// 将 YAML 字符串压缩为紧凑格式（去除多余空行）
///
/// 用于生成紧凑的 YAML 输出（适合生产环境，减少文件体积）。
/// Compact 模式有意移除空行以减小输出体积，因为空行对 YAML 语义
/// 没有影响，仅用于人类可读性。在生产部署和内核加载场景中，
/// 移除空行可以减少文件大小和解析开销。
///
/// **注意**: 保留 YAML block scalar（`|` / `>`）内的空行，
/// 因为 block scalar 中的空行是语义相关的（保留换行）。
///
///
/// 当前检测逻辑通过检查行尾是否为 `|` 或 `>` 来识别 block scalar 指示符。
/// 这在大多数情况下是正确的，但存在以下已知局限性：
/// - 行内注释中的 `|` 或 `>` 可能导致误判（如 `key: value # | comment`）
/// - 多行字符串值中恰好以 `|` 或 `>` 结尾的非 block scalar 行会被误判
/// - 不支持 flow scalar（单引号/双引号）内部的 `|` `>` 检测
///   由于 Prism Engine 生成的配置中极少出现上述边界情况，当前实现已足够。
fn compact_yaml(yaml: &str) -> String {
    let mut result = String::with_capacity(yaml.len());
    let mut in_block_scalar = false;
    // Track the indentation level of the block scalar indicator line.
    // Content lines with indentation strictly greater than this level belong to the block scalar.
    // Lines with indentation <= this level (and non-empty) terminate the block scalar.
    let mut block_scalar_indent: usize = 0;

    for line in yaml.lines() {
        if line.trim().is_empty() {
            if in_block_scalar {
                // block scalar 内的空行必须保留（语义相关）
                result.push('\n');
            }
            // 非 block scalar 的空行直接跳过
            continue;
        }

        // 检测 block scalar 指示符: 行尾的 | 或 > (可能带缩进修饰符 +/-)
        // block scalar 以 | 或 > 开头（可能带 clip/keep/strip 修饰符）
        let trimmed = line.trim();
        if trimmed.ends_with('|') || trimmed.ends_with('>') {
            // 确认是 block scalar 指示符而非普通值
            // block scalar 指示符格式: "key: |", "key: >", "key: |2", "key: >-"
            if trimmed.len() >= 2 {
                let before_indicator = trimmed[..trimmed.len() - 1].trim_end();
                if before_indicator.ends_with(':') {
                    in_block_scalar = true;
                    // Record the indentation of the block scalar indicator line.
                    // Content must be indented more than this to be part of the scalar.
                    block_scalar_indent = line.len() - line.trim_start().len();
                }
            }
        } else if in_block_scalar {
            // block scalar 内容结束条件：遇到非空行且缩进不大于 block scalar 指示符的缩进
            let line_indent = line.len() - line.trim_start().len();
            if line_indent <= block_scalar_indent {
                in_block_scalar = false;
            }
        }

        result.push_str(line);
        result.push('\n');
    }
    // 去除末尾多余换行
    result.trim_end().to_string() + "\n"
}

// ══════════════════════════════════════════════════════════
// §4.6 热重载策略
// ══════════════════════════════════════════════════════════

/// 热重载策略（§4.6）
///
/// 决定配置文件写入后如何通知内核重新加载：
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotReloadStrategy {
    /// 不通知（仅写入文件，适用于手动重载场景）
    None,

    /// 通过 POSIX 信号通知（Unix-like 系统）
    /// 发送 SIGHUP 给指定 PID
    Signal { pid: u32 },

    /// 通过 HTTP API 通知（mihomo/clash-rs RESTful API）
    /// 调用 PUT /configs?force=true
    HttpApi { url: String },
}

impl std::fmt::Display for HotReloadStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Signal { pid } => write!(f, "signal(pid={})", pid),
            Self::HttpApi { url } => write!(f, "http-api({})", url),
        }
    }
}

/// 热重载操作结果（§4.6）
#[derive(Debug, Clone)]
pub struct HotReloadResult {
    /// 使用的策略名称
    pub strategy: String,
    /// 是否成功
    pub success: bool,
    /// 详细信息
    pub detail: String,
}

impl std::fmt::Display for HotReloadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.success {
            write!(f, "[✅ {}] {}", self.strategy, self.detail)
        } else {
            write!(f, "[❌ {}] {}", self.strategy, self.detail)
        }
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_mihomo_yaml() {
        let compiler = TargetCompiler::mihomo();
        let config = serde_json::json!({
            "proxies": [
                {"name": "test", "type": "ss", "server": "example.com", "port": 8388}
            ],
            "rules": ["MATCH,DIRECT"]
        });

        let result = compiler.compile(&config).unwrap();
        assert!(result.contains("proxies:"));
        assert!(result.contains("name: test"));
        assert!(result.contains("rules:"));
        assert!(result.contains("MATCH,DIRECT"));
    }

    #[test]
    fn test_compile_clash_rs_yaml() {
        let compiler = TargetCompiler::clash_rs();
        let config = serde_json::json!({
            "mixed-port": 7890,
            "mode": "rule"
        });

        let result = compiler.compile(&config).unwrap();
        assert!(result.contains("mixed-port: 7890"));
        assert!(result.contains("mode: rule"));
    }

    #[test]
    fn test_compile_json_output() {
        let compiler = TargetCompiler::json_output();
        let config = serde_json::json!({"key": "value"});

        let result = compiler.compile(&config).unwrap();
        assert!(result.starts_with("{"));
        assert!(result.contains("\"key\""));
        assert!(result.contains("\"value\""));
    }

    #[test]
    fn test_empty_config() {
        let compiler = TargetCompiler::mihomo();
        let config = serde_json::json!({});

        let result = compiler.compile(&config).unwrap();
        assert_eq!(result.trim(), "{}");
    }

    #[test]
    fn test_complex_config_roundtrip() {
        let compiler = TargetCompiler::mihomo();
        let config = serde_json::json!({
            "mixed-port": 7890,
            "allow-lan": true,
            "mode": "rule",
            "log-level": "info",
            "dns": {
                "enable": true,
                "ipv6": false,
                "nameserver": ["https://dns.alidns.com/dns-query"]
            },
            "proxies": [],
            "proxy-groups": [
                {"name": "PROXY", "type": "select", "proxies": ["DIRECT"]}
            ],
            "rules": ["MATCH,DIRECT"]
        });

        let yaml_output = compiler.compile(&config).unwrap();

        // 验证关键结构存在
        assert!(yaml_output.contains("mixed-port:"));
        assert!(yaml_output.contains("dns:"));
        assert!(yaml_output.contains("enable:"));
        assert!(yaml_output.contains("proxy-groups:"));
        assert!(yaml_output.contains("rules:"));

        // 验证可以反向解析（round-trip）
        let parsed: serde_yml::Value = serde_yml::from_str(&yaml_output).unwrap();
        assert!(parsed.get("mixed-port").is_some());
        assert!(parsed.get("dns").is_some());
    }

    // ─── §4.6 原子写入测试 ───

    #[test]
    fn test_atomic_write_creates_file() {
        let compiler = TargetCompiler::mihomo();
        let config = serde_json::json!({"mixed-port": 7890});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        let bytes = compiler.atomic_write(&config, &path).unwrap();
        assert!(bytes > 0);
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("mixed-port:"));
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let compiler = TargetCompiler::mihomo();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        // 第一次写入
        compiler
            .atomic_write(&serde_json::json!({"port": 1111}), &path)
            .unwrap();
        // 第二次写入（原子替换）
        compiler
            .atomic_write(&serde_json::json!({"port": 2222}), &path)
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("2222"));
        assert!(!content.contains("1111"));
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let compiler = TargetCompiler::mihomo();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("sub").join("config.yaml");

        let result = compiler.atomic_write(&serde_json::json!({}), &path);
        assert!(result.is_ok());
        assert!(path.exists());
    }

    // ─── §4.6 热重载测试 ───

    #[test]
    fn test_hot_reload_none_strategy() {
        let compiler = TargetCompiler::mihomo();
        let result = compiler.notify_hot_reload(
            std::path::Path::new("/tmp/test.yaml"),
            HotReloadStrategy::None,
        );
        let reload = result.unwrap();
        assert!(reload.success);
        assert_eq!(reload.strategy, "none");
    }

    #[test]
    fn test_write_and_reload_combined() {
        let compiler = TargetCompiler::json_output();
        let config = serde_json::json!({"key": "value"});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.json");

        let (bytes, reload) = compiler
            .write_and_reload(&config, &path, HotReloadStrategy::None)
            .unwrap();

        assert!(bytes > 0);
        assert!(reload.success);
        assert!(path.exists());
    }

    // ─── v2 表驱动字段映射测试 ───

    #[test]
    fn test_adapt_remove_field_for_clash_rs() {
        let compiler = TargetCompiler::clash_rs();
        let config = serde_json::json!({
            "tun": {
                "enable": true,
                "device": "Meta",
                "stack": "mixed"
            }
        });

        let result = compiler.compile(&config).unwrap();
        let parsed: serde_yml::Value = serde_yml::from_str(&result).unwrap();
        let tun = &parsed["tun"];
        assert!(tun.get("device").is_none(), "tun.device 应被移除");
        assert!(
            tun.get("interface-name").is_some(),
            "tun.interface-name 应存在"
        );
        assert!(tun.get("enable").is_some(), "tun.enable 应存在");
    }

    #[test]
    fn test_adapt_no_change_for_mihomo() {
        let compiler = TargetCompiler::mihomo();
        let config = serde_json::json!({
            "tun": {
                "enable": true,
                "device": "Meta",
                "stack": "mixed"
            }
        });

        let result = compiler.compile(&config).unwrap();
        // All fields should remain for mihomo
        assert!(result.contains("device"));
        assert!(result.contains("stack"));
        assert!(result.contains("enable"));
    }

    #[test]
    fn test_adapt_remove_nonexistent_field() {
        let compiler = TargetCompiler::clash_rs();
        let config = serde_json::json!({
            "dns": {"enable": true}
        });

        // Should not panic when field doesn't exist
        let result = compiler.compile(&config).unwrap();
        assert!(result.contains("enable"));
    }

    // ─── 原子写入扩展测试 ───

    /// 权限保留：替换已存在的文件后，新文件应继承原文件的权限模式。
    /// 验证 atomic_write 在 Unix 系统上正确复制 metadata.permissions。
    #[test]
    fn test_atomic_write_preserves_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let compiler = TargetCompiler::mihomo();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.yaml");

            // 第一次写入
            compiler
                .atomic_write(&serde_json::json!({"port": 1111}), &path)
                .unwrap();

            // 设置特定权限 0o644
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

            // 第二次原子写入（应保留权限）
            compiler
                .atomic_write(&serde_json::json!({"port": 2222}), &path)
                .unwrap();

            let metadata = std::fs::metadata(&path).unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "原子替换后文件权限应保持 0o644");
        }
        #[cfg(not(unix))]
        {
            // 非 Unix 系统跳过权限测试
        }
    }

    /// 边界：空内容写入。空 JSON 对象 {} 编译后应产生有效输出。
    #[test]
    fn test_atomic_write_empty_content() {
        let compiler = TargetCompiler::mihomo();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.yaml");

        let bytes = compiler
            .atomic_write(&serde_json::json!({}), &path)
            .unwrap();
        assert!(bytes > 0, "空配置编译后仍应产生输出");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.is_empty(), "文件不应为空");
    }

    /// 边界：大文件写入（>1MB）。验证原子写入能正确处理大配置。
    #[test]
    fn test_atomic_write_large_content() {
        let compiler = TargetCompiler::mihomo();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.yaml");

        // 构造一个超过 1MB 的 JSON 配置（大量代理节点）
        let mut proxies = Vec::new();
        for i in 0..5000 {
            proxies.push(serde_json::json!({
                "name": format!("proxy-{:04}-长名称测试节点用于验证大文件写入功能", i),
                "type": "ss",
                "server": format!("server-{}.example.com", i),
                "port": 8388 + (i % 1000)
            }));
        }
        let config = serde_json::json!({
            "proxies": proxies,
            "rules": ["MATCH,DIRECT"]
        });

        let bytes = compiler.atomic_write(&config, &path).unwrap();
        assert!(bytes > 100_000, "写入字节数应超过 100KB，实际: {}", bytes);
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("proxies:"));
    }

    /// Unicode 内容：中文、emoji、零宽字符。验证原子写入正确处理多字节字符。
    #[test]
    fn test_atomic_write_unicode_content() {
        let compiler = TargetCompiler::mihomo();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.yaml");

        let config = serde_json::json!({
            "proxies": [
                {
                    "name": "🇭🇰 香港 IPLC 节点 \u{200B}零宽空格\u{200C}零宽非连接符",
                    "type": "ss",
                    "server": "hk.example.com"
                },
                {
                    "name": "🇯🇵 日本 🇺🇸 美国 🇸🇬 新加坡",
                    "type": "vmess",
                    "server": "jp.example.com"
                },
                {
                    "name": "测试中文混合English和特殊字符★☆♪♫",
                    "type": "trojan",
                    "server": "test.example.com"
                }
            ]
        });

        let bytes = compiler.atomic_write(&config, &path).unwrap();
        assert!(bytes > 0);
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        // 验证 Unicode 内容被正确写入
        assert!(content.contains("香港"));
        assert!(content.contains("日本"));
        assert!(content.contains("美国"));
    }

    /// 并发：多线程同时原子写入不同文件，验证线程安全性。
    /// 每个线程写入不同的目标文件，不应出现数据竞争或文件损坏。
    #[test]
    fn test_atomic_write_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let compiler = Arc::new(TargetCompiler::mihomo());
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let num_threads = 8;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let compiler = Arc::clone(&compiler);
                let dir = Arc::clone(&dir);
                thread::spawn(move || {
                    let path = dir.path().join(format!("concurrent-{}.yaml", i));
                    let config = serde_json::json!({
                        "thread": i,
                        "data": format!("来自线程 {} 的数据", i)
                    });
                    let bytes = compiler.atomic_write(&config, &path).unwrap();
                    (i, bytes, path)
                })
            })
            .collect();

        for handle in handles {
            let (i, bytes, path) = handle.join().unwrap();
            assert!(bytes > 0, "线程 {} 写入字节数应大于 0", i);
            assert!(path.exists(), "线程 {} 的文件应存在", i);

            // 验证文件内容正确（未被其他线程覆盖）
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.contains(&format!("thread: {}", i)),
                "线程 {} 的文件内容应包含正确的 thread 标识",
                i
            );
        }
    }
}
