//! 节点故障切换策略（§7.2）
//!
//! ## Unified NodeFailPolicy
//!
//! Previously, `clash-prism-plugin` defined its own `NodeFailPolicy` with
//! inconsistent fields (`fallback_group: Option<String>`, no `enabled` field)
//! compared to `clash-prism-core`'s version (`fallback_group: String`, has `enabled`).
//! This caused type confusion for downstream crates depending on both.
//!
//! **Resolution**: `clash-prism-plugin` now re-exports `NodeFailPolicy` from `clash-prism-core`
//! as the single source of truth. Plugin-specific types (`FallbackTarget`,
//! `CooldownState`) remain here since they don't exist in core.

pub use clash_prism_core::failover::NodeFailPolicy;

/// 切换目标（plugin 扩展类型，用于 should_switch 返回值）
#[derive(Debug, Clone)]
pub struct FallbackTarget {
    /// 目标组名（None 表示当前组的下一个节点）
    pub group: Option<String>,
    /// 触发原因
    pub reason: String,
}

/// 冷却状态追踪
///
/// ## 设计决策
///
/// 使用 `std::time::Instant` 而非 `std::time::SystemTime`：
/// - `Instant` 是单调递增时钟，不受系统时间调整影响，
///   适合用于测量运行时冷却间隔（如 "故障后等待 30 秒再重试"）。
/// - 缺点：`Instant` 不可序列化，无法持久化到磁盘或跨进程传递。
/// - 如果需要序列化（如持久化冷却状态），请使用 [`CooldownStateSerializable`]。
///
/// ## 线程安全
///
/// `CooldownState` 自动实现 `Send + Sync`（所有字段均为 `Send + Sync`），
/// 可安全地跨线程共享（例如配合 `Arc<Mutex<CooldownState>>` 或 `Arc<RwLock<...>>`）。
#[derive(Debug, Clone)]
pub struct CooldownState {
    /// 冷却开始时间
    pub start: std::time::Instant,
    /// 冷却持续时间
    pub duration: std::time::Duration,
}

impl CooldownState {
    /// 冷却是否已结束
    pub fn is_expired(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// 剩余冷却时间（毫秒）
    pub fn remaining_ms(&self) -> u64 {
        if self.is_expired() {
            0
        } else {
            let remaining = self.duration - self.start.elapsed();
            remaining.as_millis() as u64
        }
    }
}

/// 可序列化的冷却状态（基于 `SystemTime`）
///
/// 适用于需要持久化到磁盘或跨进程传递冷却状态的场景。
/// 注意：`SystemTime` 受系统时间调整影响，不适合高精度计时。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CooldownStateSerializable {
    /// 冷却开始时间（可序列化）
    pub start: std::time::SystemTime,
    /// 冷却持续时间
    pub duration: std::time::Duration,
}

impl CooldownStateSerializable {
    /// 从运行时 `CooldownState` 创建可序列化版本
    ///
    /// **精度说明**：先获取 `elapsed` 再获取 `SystemTime::now()`，
    /// 使两次采样尽可能紧密，减少纳秒级竞态间隙。对于冷却场景（通常为秒级），
    /// 该间隙可忽略不计。
    pub fn from_instant(state: &CooldownState) -> Self {
        let elapsed = state.start.elapsed();
        let now = std::time::SystemTime::now();
        Self {
            start: now - elapsed,
            duration: state.duration,
        }
    }

    /// 冷却是否已结束
    pub fn is_expired(&self) -> bool {
        match self.start.elapsed() {
            Ok(elapsed) => elapsed >= self.duration,
            Err(_) => true, // 系统时间异常（早于 epoch），视为已过期
        }
    }

    /// 剩余冷却时间（毫秒）
    pub fn remaining_ms(&self) -> u64 {
        if self.is_expired() {
            0
        } else {
            match self.start.elapsed() {
                Ok(elapsed) => {
                    let remaining = self.duration.saturating_sub(elapsed);
                    remaining.as_millis() as u64
                }
                Err(_) => 0,
            }
        }
    }
}
