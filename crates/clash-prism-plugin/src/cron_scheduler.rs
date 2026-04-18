//! Cron 调度器 — 基于 tokio 的轻量级定时任务调度
//!
//! ## 设计决策
//!
//! 不引入第三方 cron 库（如 tokio-cron-scheduler），
//! 保持与项目"零 C 依赖、纯 Rust"原则一致。
//!
//! 使用 `tokio::time::sleep` + `tokio::spawn` 实现定时调度，
//! 支持 5 字段标准 cron 表达式（分 时 日 月 周）。
//!
//! ## 与 hook.rs 的关系
//!
//! - `hook.rs` 中的 `HookScheduler` / `ScheduledHook` 负责
//!   cron 表达式的**解析与匹配**（同步、无状态）
//! - 本模块的 `CronScheduler` 负责**异步定时执行**（有状态、tokio 驱动）
//!
//! 两者可独立使用，也可组合：`CronScheduler` 注册回调后，
//!   回调内部调用 `HookScheduler::poll_due_hooks()` 完成联动。

use std::sync::Arc;

use chrono::{Datelike, TimeZone, Timelike, Utc};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::{Duration, sleep};
use tracing;

// ══════════════════════════════════════════════════════════
// 错误类型
// ══════════════════════════════════════════════════════════

/// Cron 调度器错误
#[derive(Debug, Error)]
pub enum CronError {
    /// Cron 表达式格式无效
    #[error("无效的 Cron 表达式「{expr}」：{reason}")]
    InvalidExpression {
        /// 原始表达式
        expr: String,
        /// 错误原因
        reason: String,
    },

    /// 重复的任务名称
    #[error("任务名称「{name}」已存在")]
    DuplicateTask {
        /// 任务名称
        name: String,
    },

    /// 调度器未启动
    #[error("调度器未启动，无法注册任务")]
    NotRunning,

    /// 调度器已在运行
    #[error("调度器已在运行")]
    AlreadyRunning,
}

// ══════════════════════════════════════════════════════════
// Cron 表达式 — 解析与匹配
// ══════════════════════════════════════════════════════════

/// 已解析的 Cron 表达式
///
/// 5 个字段分别对应：分(0-59) 时(0-23) 日(1-31) 月(1-12) 周(0=Mon..6=Sun)
#[derive(Debug, Clone)]
pub struct CronExpr {
    /// 分钟字段匹配器
    minutes: FieldMatcher,
    /// 小时字段匹配器
    hours: FieldMatcher,
    /// 日字段匹配器
    days_of_month: FieldMatcher,
    /// 月字段匹配器
    months: FieldMatcher,
    /// 星期字段匹配器
    days_of_week: FieldMatcher,
}

/// 单个 cron 字段的匹配器
///
/// 将 cron 字段解析为一系列匹配规则，支持：
/// - 通配符 `*`
/// - 步长 `*/N`
/// - 范围 `1-5`
/// - 范围+步长 `1-10/2`
/// - 列表 `1,3,5`
/// - 月份名 `JAN`..`DEC`、星期名 `MON`..`SUN`
///
/// 内部同时维护排序 `Vec`（用于有序迭代 / `min_value`）与 `HashSet`（用于 O(1) 匹配查询）。
/// 字段最大基数不超过 60（分钟），因此双存储开销可忽略不计。
#[derive(Debug, Clone)]
struct FieldMatcher {
    /// 排序后的允许值列表（用于有序迭代）
    allowed: Vec<u32>,
    /// 允许值集合（O(1) 查询）
    allowed_set: std::collections::HashSet<u32>,
    /// 该字段的有效范围（用于验证）
    range: (u32, u32),
}

impl FieldMatcher {
    /// 判断给定值是否匹配（O(1) HashSet 查询）
    fn matches(&self, value: u32) -> bool {
        self.allowed_set.contains(&value)
    }

    /// 判断此字段是否为"无约束"（匹配范围内所有值）
    ///
    /// 用于 POSIX cron day_of_month / day_of_week 的特殊语义：
    /// 当其中一个字段无约束时，仅使用另一个字段过滤。
    fn is_unconstrained(&self) -> bool {
        let full_count = (self.range.1 - self.range.0 + 1) as usize;
        self.allowed.len() == full_count
    }

    /// 获取最小允许值
    fn min_value(&self) -> u32 {
        *self.allowed.first().unwrap_or(&self.range.0)
    }
}

impl CronExpr {
    /// 判断给定时间是否匹配此 cron 表达式
    ///
    /// POSIX cron day_of_month / day_of_week 语义：
    /// - 两者都非通配符 → OR 关系（任一匹配即可）
    /// - 其中一个是通配符 → 仅使用非通配符字段过滤
    /// - 两者都是通配符 → 匹配任何日期
    pub(crate) fn matches(&self, dt: chrono::DateTime<Utc>) -> bool {
        self.minutes.matches(dt.minute())
            && self.hours.matches(dt.hour())
            && self.months.matches(dt.month())
            && self.day_matches(dt.day(), dt.weekday().num_days_from_monday())
    }

    /// POSIX cron 日期匹配语义
    fn day_matches(&self, day: u32, dow: u32) -> bool {
        let dom_unconstrained = self.days_of_month.is_unconstrained();
        let dow_unconstrained = self.days_of_week.is_unconstrained();

        match (dom_unconstrained, dow_unconstrained) {
            // 两者都无约束 → 匹配任何日期
            (true, true) => true,
            // 仅 dom 无约束 → 只看 dow
            (true, false) => self.days_of_week.matches(dow),
            // 仅 dow 无约束 → 只看 dom
            (false, true) => self.days_of_month.matches(day),
            // 两者都有约束 → OR 关系
            (false, false) => self.days_of_month.matches(day) || self.days_of_week.matches(dow),
        }
    }

    /// 计算从指定时间之后的下一次匹配时间
    ///
    /// 算法：从 `from` 的下一分钟开始逐分钟搜索，
    /// 利用字段排序跳过不可能匹配的月份/日/小时，避免暴力遍历。
    ///
    /// 最坏情况：搜索 4 年（闰年周期），保证一定能找到匹配。
    fn next_after(&self, from: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
        // 从下一分钟开始搜索（避免重复触发当前分钟）
        let mut candidate = from + chrono::Duration::minutes(1);
        // 对齐到整分钟
        candidate = candidate
            .with_second(0)
            .unwrap_or(candidate)
            .with_nanosecond(0)
            .unwrap_or(candidate);

        // 最大搜索范围：8 年（覆盖两个闰年周期，确保所有 cron 表达式都能找到匹配）
        let deadline = from + chrono::Duration::days(365 * 8 + 2);

        // ── 月份级别跳跃 ──
        // 如果当前月份不在允许列表中，直接跳到下一个允许的月份
        loop {
            if candidate > deadline {
                // 理论上不会发生（除非 cron 表达式永远无法匹配）
                tracing::warn!("Cron 表达式在 8 年内未找到匹配时间，返回 from + 1 分钟");
                return from + chrono::Duration::minutes(1);
            }

            let month = candidate.month();
            if !self.months.matches(month) {
                // 跳到下一个允许的月份的第一天 00:00
                let next_month = self
                    .months
                    .allowed
                    .iter()
                    .find(|&&m| m > month)
                    .copied()
                    .unwrap_or_else(|| self.months.min_value());

                if next_month <= month {
                    // 需要跨年：直接跳到下一年的目标月份
                    let next_year = candidate.year() + 1;
                    candidate = chrono::Utc
                        .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
                        .single()
                        .unwrap_or(candidate);
                } else {
                    candidate = candidate
                        .with_month(next_month)
                        .and_then(|d| d.with_day(1))
                        .and_then(|d| d.with_hour(0))
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                }
                continue;
            }

            // ── 日级别跳跃（POSIX cron day_of_month / day_of_week 语义） ──
            let day = candidate.day();
            let chrono_dow = candidate.weekday().num_days_from_monday();

            if !self.day_matches(day, chrono_dow) {
                // 两者都不匹配，跳到下一天
                candidate += chrono::Duration::days(1);
                candidate = candidate
                    .with_hour(0)
                    .and_then(|d| d.with_minute(0))
                    .unwrap_or(candidate);
                continue;
            }

            // ── 小时级别跳跃 ──
            let hour = candidate.hour();
            if !self.hours.matches(hour) {
                let next_hour = self.hours.allowed.iter().find(|&&h| h > hour).copied();

                if let Some(nh) = next_hour {
                    candidate = candidate
                        .with_hour(nh)
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                } else {
                    // 当天没有更多匹配小时，跳到明天
                    candidate += chrono::Duration::days(1);
                    candidate = candidate
                        .with_hour(0)
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                    continue; // 回到日/星期检查
                }
                continue;
            }

            // ── 分钟级别检查 ──
            let minute = candidate.minute();
            if !self.minutes.matches(minute) {
                let next_minute = self.minutes.allowed.iter().find(|&&m| m > minute).copied();

                if let Some(nm) = next_minute {
                    candidate = candidate.with_minute(nm).unwrap_or(candidate);
                } else {
                    // 当小时没有更多匹配分钟，跳到下一个匹配小时
                    let next_hour = self.hours.allowed.iter().find(|&&h| h > hour).copied();

                    if let Some(nh) = next_hour {
                        candidate = candidate
                            .with_hour(nh)
                            .and_then(|d| d.with_minute(self.minutes.min_value()))
                            .unwrap_or(candidate);
                    } else {
                        // 跳到明天
                        candidate += chrono::Duration::days(1);
                        candidate = candidate
                            .with_hour(0)
                            .and_then(|d| d.with_minute(0))
                            .unwrap_or(candidate);
                        continue;
                    }
                }
                continue;
            }

            // 所有字段都匹配
            return candidate;
        }
    }
}

/// 解析 5 字段标准 cron 表达式
///
/// # 格式
///
/// ```text
/// ┌────────── 分钟 (0-59)
/// │ ┌──────── 小时 (0-23)
/// │ │ ┌────── 日 (1-31)
/// │ │ │ ┌──── 月 (1-12)
/// │ │ │ │ ┌── 星期 (0=Mon .. 6=Sun)
/// * * * * *
/// ```
///
/// # 支持的语法
///
/// | 语法 | 说明 | 示例 |
/// |------|------|------|
/// | `*` | 通配符 | `*` |
/// | `*/N` | 从最小值开始，步长 N | `*/5` |
/// | `N` | 固定值 | `30` |
/// | `N-M` | 范围 | `1-5` |
/// | `N-M/S` | 范围+步长 | `0-23/2` |
/// | `A,B,C` | 列表 | `1,15,30` |
/// | `JAN`..`DEC` | 月份名 | `JAN` |
/// | `MON`..`SUN` | 星期名 | `MON-FRI` |
///
/// # Errors
///
/// 返回 `CronError::InvalidExpression` 如果表达式格式无效。
pub fn parse_cron(expr: &str) -> Result<CronExpr, CronError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(CronError::InvalidExpression {
            expr: expr.to_string(),
            reason: format!("需要 5 个字段（分 时 日 月 周），实际 {} 个", parts.len()),
        });
    }

    let minutes = parse_field(parts[0], 0, 59, "分钟")?;
    let hours = parse_field(parts[1], 0, 23, "小时")?;
    let days_of_month = parse_field(parts[2], 1, 31, "日")?;
    let months = parse_field(parts[3], 1, 12, "月")?;
    let days_of_week = parse_field(parts[4], 0, 6, "星期")?;

    Ok(CronExpr {
        minutes,
        hours,
        days_of_month,
        months,
        days_of_week,
    })
}

/// 解析单个 cron 字段
///
/// 将字段表达式解析为 `FieldMatcher`，预计算所有允许的值。
fn parse_field(
    expr: &str,
    min: u32,
    max: u32,
    field_name: &str,
) -> Result<FieldMatcher, CronError> {
    // 复用 hook.rs 中的公共函数，避免重复实现
    let normalized = crate::hook::normalize_cron_field_name(expr, &(min..=max));

    let mut allowed_values: Vec<u32> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 逗号分隔的列表：每个子项独立解析
    for part in normalized.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let values = parse_field_part(part, min, max, field_name)?;
        for v in values {
            if seen.insert(v) {
                allowed_values.push(v);
            }
        }
    }

    if allowed_values.is_empty() {
        return Err(CronError::InvalidExpression {
            expr: expr.to_string(),
            reason: format!("{}字段「{}」未产生任何有效值", field_name, expr),
        });
    }

    allowed_values.sort_unstable();

    let allowed_set: std::collections::HashSet<u32> = allowed_values.iter().copied().collect();

    Ok(FieldMatcher {
        allowed: allowed_values,
        allowed_set,
        range: (min, max),
    })
}

/// 解析单个字段部分（不含逗号）
fn parse_field_part(
    part: &str,
    min: u32,
    max: u32,
    field_name: &str,
) -> Result<Vec<u32>, CronError> {
    // 通配符：* 等同于 min-max
    if part == "*" {
        return Ok((min..=max).collect());
    }

    // 步长模式：*/N
    if let Some(step_str) = part.strip_prefix("*/") {
        let step: u32 = step_str.parse().map_err(|_| CronError::InvalidExpression {
            expr: part.to_string(),
            reason: format!("{}字段步长「{}」不是有效数字", field_name, step_str),
        })?;
        if step == 0 {
            return Err(CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段步长不能为 0", field_name),
            });
        }
        return Ok((min..=max).step_by(step as usize).collect());
    }

    // 范围模式（可能带步长）：N-M 或 N-M/S
    if part.contains('-') {
        let (range_part, step) = if let Some(slash_pos) = part.find('/') {
            let range_part = &part[..slash_pos];
            let step_str = &part[slash_pos + 1..];
            let step: u32 = step_str.parse().map_err(|_| CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段步长「{}」不是有效数字", field_name, step_str),
            })?;
            if step == 0 {
                return Err(CronError::InvalidExpression {
                    expr: part.to_string(),
                    reason: format!("{}字段步长不能为 0", field_name),
                });
            }
            (range_part, step)
        } else {
            (part, 1)
        };

        let dash_pos = range_part
            .find('-')
            .ok_or_else(|| CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段范围格式无效", field_name),
            })?;

        let start: u32 =
            range_part[..dash_pos]
                .parse()
                .map_err(|_| CronError::InvalidExpression {
                    expr: part.to_string(),
                    reason: format!(
                        "{}字段范围起始值「{}」不是有效数字",
                        field_name,
                        &range_part[..dash_pos]
                    ),
                })?;

        let end: u32 =
            range_part[dash_pos + 1..]
                .parse()
                .map_err(|_| CronError::InvalidExpression {
                    expr: part.to_string(),
                    reason: format!(
                        "{}字段范围结束值「{}」不是有效数字",
                        field_name,
                        &range_part[dash_pos + 1..]
                    ),
                })?;

        // 纵深防御：验证范围两端和步长均在合法区间内
        if start < min {
            return Err(CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段范围起始值 {} 小于最小值 {}", field_name, start, min),
            });
        }
        if end > max {
            return Err(CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段范围结束值 {} 超出最大值 {}", field_name, end, max),
            });
        }
        if start > end {
            return Err(CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!("{}字段范围起始值 {} 大于结束值 {}", field_name, start, end),
            });
        }
        // 步长合理性检查：步长为 0 已在上面处理，这里检查步长不超过范围跨度
        if step > (end - start + 1) {
            return Err(CronError::InvalidExpression {
                expr: part.to_string(),
                reason: format!(
                    "{}字段步长 {} 超过范围跨度 {} ({}-{})",
                    field_name,
                    step,
                    end - start + 1,
                    start,
                    end
                ),
            });
        }

        return Ok((start..=end).step_by(step as usize).collect());
    }

    // 固定值
    let value: u32 = part.parse().map_err(|_| CronError::InvalidExpression {
        expr: part.to_string(),
        reason: format!("{}字段值「{}」不是有效数字", field_name, part),
    })?;

    if value < min || value > max {
        return Err(CronError::InvalidExpression {
            expr: part.to_string(),
            reason: format!(
                "{}字段值 {} 超出有效范围 [{}, {}]",
                field_name, value, min, max
            ),
        });
    }

    Ok(vec![value])
}

// ══════════════════════════════════════════════════════════
// 调度任务
// ══════════════════════════════════════════════════════════

/// 已注册的调度任务
struct ScheduledTask {
    /// 任务唯一名称
    name: String,
    /// 已解析的 cron 表达式
    cron: CronExpr,
    /// 原始 cron 表达式字符串（用于日志）
    cron_raw: String,
    /// 异步回调函数
    callback: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("name", &self.name)
            .field("cron", &self.cron_raw)
            .field("callback", &"<fn>")
            .finish()
    }
}

impl Clone for ScheduledTask {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            cron: self.cron.clone(),
            cron_raw: self.cron_raw.clone(),
            callback: Arc::clone(&self.callback),
        }
    }
}

// ══════════════════════════════════════════════════════════
// Cron 调度器
// ══════════════════════════════════════════════════════════

/// 默认优雅关闭超时时间（秒）
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// 基于 tokio 的 Cron 调度器
///
/// 支持注册多个 cron 任务，异步定时触发回调。
/// 内部使用 `tokio::spawn` 为每个任务创建独立的调度协程，
/// 通过 `watch` channel 实现 graceful shutdown。
///
/// # 使用示例
///
/// ```ignore
/// use clash_prism_plugin::CronScheduler;
///
/// #[tokio::main]
/// async fn main() {
///     let scheduler = CronScheduler::new();
///
///     scheduler.register("heartbeat", "*/5 * * * *", || {
///         println!("心跳检测");
///     }).unwrap();
///
///     scheduler.start().await;
///
///     // 运行一段时间后...
///     scheduler.shutdown().await;
/// }
/// ```
pub struct CronScheduler {
    /// 已注册的任务列表
    tasks: Arc<RwLock<Vec<ScheduledTask>>>,
    /// shutdown 信号发送端
    shutdown_tx: watch::Sender<bool>,
    /// shutdown 信号接收端（每个任务协程持有一份克隆）
    shutdown_rx: watch::Receiver<bool>,
    /// 调度器是否已启动
    running: Arc<Mutex<bool>>,
    /// 已启动的 tokio task JoinHandle（用于等待所有任务退出）
    handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl CronScheduler {
    /// 创建新的 Cron 调度器
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            tasks: Arc::new(RwLock::new(Vec::new())),
            shutdown_tx,
            shutdown_rx,
            running: Arc::new(Mutex::new(false)),
            handles: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 注册一个 cron 定时任务
    ///
    /// # Arguments
    ///
    /// * `name` — 任务唯一名称（重复名称返回错误）
    /// * `cron_expr` — 标准 5 字段 cron 表达式
    /// * `callback` — 触发时执行的回调函数（在 tokio 工作线程上执行）
    ///
    /// # Errors
    ///
    /// - `CronError::DuplicateTask` — 同名任务已存在
    /// - `CronError::InvalidExpression` — cron 表达式格式无效
    pub async fn register(
        &self,
        name: impl Into<String>,
        cron_expr: &str,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), CronError> {
        let name = name.into();
        let cron = parse_cron(cron_expr)?;

        let mut tasks = self.tasks.write().await;

        // 检查重名
        if tasks.iter().any(|t| t.name == name) {
            return Err(CronError::DuplicateTask { name });
        }

        tasks.push(ScheduledTask {
            name: name.clone(),
            cron,
            cron_raw: cron_expr.to_string(),
            callback: Arc::new(callback),
        });

        tracing::info!(
            task_name = %name,
            cron_expr = cron_expr,
            "Cron 任务已注册"
        );

        // 避免 start() 后注册的任务永远不会启动。
        //
        // 注意：释放 running 锁和调用 spawn_task 之间存在 TOCTOU 窗口，
        // 即 shutdown 可能在检查后、spawn 前将 running 设为 false。
        // 因此 spawn_task 内部会再次检查 running 状态，如果已 shutdown 则立即退出。
        {
            let running = self.running.lock().await;
            if *running {
                // 先释放 running 锁，避免 spawn_task 内部再次获取时死锁
                drop(running);
                self.spawn_task(&name).await;
            }
        }

        Ok(())
    }

    /// 为指定任务 spawn 一个独立的调度协程
    ///
    /// 协程内部会检查任务是否仍被注册（支持 unregister 后自动退出），
    /// 并通过 `watch` channel 响应 shutdown 信号。
    async fn spawn_task(&self, task_name: &str) {
        // TOCTOU 防护：register() 释放 running 锁后、调用 spawn_task 前可能发生 shutdown。
        // 在 spawn_task 入口处再次检查 running，如果已 shutdown 则立即返回，避免产生孤儿协程。
        {
            let running = self.running.lock().await;
            if !*running {
                tracing::info!(
                    task_name = %task_name,
                    "spawn_task: 调度器已停止，跳过 spawn"
                );
                return;
            }
        }

        let task = {
            let tasks = self.tasks.read().await;
            tasks.iter().find(|t| t.name == task_name).cloned()
        };

        let Some(task) = task else {
            tracing::warn!(task_name = %task_name, "spawn_task: 任务未找到，跳过");
            return;
        };

        let cron = task.cron.clone();
        let name = task.name.clone();
        let cron_raw = task.cron_raw.clone();
        let callback = Arc::clone(&task.callback);
        let mut shutdown_rx = self.shutdown_rx.clone();
        let tasks_ref = Arc::clone(&self.tasks);

        let handle = tokio::spawn(async move {
            tracing::info!(
                task_name = %name,
                cron_expr = %cron_raw,
                "Cron 任务协程已启动"
            );

            loop {
                // 检查任务是否仍被注册
                {
                    let tasks = tasks_ref.read().await;
                    if !tasks.iter().any(|t| t.name == name) {
                        tracing::info!(
                            task_name = %name,
                            "Cron 任务已被移除，协程退出"
                        );
                        break;
                    }
                }

                let now = Utc::now();
                let next = cron.next_after(now);
                let delay = next.signed_duration_since(now);

                // 等待直到下次执行时间或收到 shutdown 信号
                // .max(1) 防止整分钟边界 delay=0 导致 sleep(0) 空转循环 (CPU 100%)
                let sleep_duration = Duration::from_secs(delay.num_seconds().max(1) as u64);
                tokio::select! {
                    _ = sleep(sleep_duration) => {
                        // 再次检查任务是否仍被注册（可能在等待期间被移除）
                        let still_registered = {
                            let tasks = tasks_ref.read().await;
                            tasks.iter().any(|t| t.name == name)
                        };

                        if still_registered {
                            tracing::debug!(
                                task_name = %name,
                                "Cron 任务触发"
                            );
                            // 执行回调（捕获 panic，防止单个任务崩溃影响调度器）
                            // 使用 error 级别确保在默认日志配置下可见。
                            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                callback();
                            }));
                            if let Err(panic_payload) = result {
                                let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                    s.to_string()
                                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                                    s.clone()
                                } else {
                                    "未知 panic（非字符串类型）".to_string()
                                };
                                tracing::error!(
                                    task_name = %name,
                                    cron_expr = %cron_raw,
                                    error = %msg,
                                    "Cron 任务回调 panic！此任务已被跳过，不会影响调度器运行。\
                                     请检查回调函数实现，修复后重新注册任务。"
                                );
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        tracing::info!(
                            task_name = %name,
                            "Cron 任务收到 shutdown 信号，协程退出"
                        );
                        break;
                    }
                }
            }
        });

        self.handles.lock().await.push(handle);
    }

    /// 移除指定名称的任务
    ///
    /// 如果任务正在运行，会在下一个调度周期停止。
    /// 注意：已 spawn 的 tokio task 会在检测到任务被移除后自行退出。
    pub async fn unregister(&self, name: &str) -> bool {
        let mut tasks = self.tasks.write().await;
        let len_before = tasks.len();
        tasks.retain(|t| t.name != name);
        let removed = tasks.len() < len_before;
        if removed {
            tracing::info!(task_name = %name, "Cron 任务已移除");
        }
        removed
    }

    /// 启动调度器
    ///
    /// 为每个已注册的任务 spawn 一个独立的 tokio 协程。
    /// 如果调度器已在运行，返回 `CronError::AlreadyRunning`。
    pub async fn start(&self) -> Result<(), CronError> {
        {
            let mut running = self.running.lock().await;
            if *running {
                return Err(CronError::AlreadyRunning);
            }
            *running = true;
        }

        let task_names: Vec<String> = {
            let tasks = self.tasks.read().await;
            tasks.iter().map(|t| t.name.clone()).collect()
        };

        for name in &task_names {
            self.spawn_task(name).await;
        }

        tracing::info!(task_count = task_names.len(), "Cron 调度器已启动");

        Ok(())
    }

    /// 优雅关闭调度器
    ///
    /// 向所有任务协程发送 shutdown 信号，并等待它们全部退出。
    /// 超时时间由 `DEFAULT_SHUTDOWN_TIMEOUT_SECS` 常量控制（默认 30 秒）。
    pub async fn shutdown(&self) {
        // 发送 shutdown 信号
        let _ = self.shutdown_tx.send(true);

        {
            let mut running = self.running.lock().await;
            *running = false;
        }

        // 等待所有协程退出（全局超时预算，所有任务共享）
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        let mut handles = self.handles.lock().await;
        for handle in handles.drain(..) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    "Cron 调度器全局关闭超时（{}s），强制终止剩余任务",
                    DEFAULT_SHUTDOWN_TIMEOUT_SECS
                );
                break;
            }
            match tokio::time::timeout(remaining, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("Cron 任务协程异常退出: {:?}", e);
                }
                Err(_) => {
                    tracing::warn!("Cron 任务协程退出超时（剩余 {}ms）", remaining.as_millis());
                }
            }
        }

        tracing::info!("Cron 调度器已关闭");
    }

    /// 获取已注册的任务数量
    pub async fn task_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    /// 获取所有已注册任务的名称
    pub async fn task_names(&self) -> Vec<String> {
        self.tasks
            .read()
            .await
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    /// 检查调度器是否正在运行
    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }
}

impl Default for CronScheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── CronExpr 解析测试 ──

    #[test]
    fn test_parse_wildcard() {
        let cron = parse_cron("* * * * *").unwrap();
        // 通配符应匹配所有值
        assert!(cron.minutes.matches(0));
        assert!(cron.minutes.matches(59));
        assert!(cron.hours.matches(0));
        assert!(cron.hours.matches(23));
    }

    #[test]
    fn test_parse_step() {
        let cron = parse_cron("*/5 * * * *").unwrap();
        assert!(cron.minutes.matches(0));
        assert!(cron.minutes.matches(5));
        assert!(cron.minutes.matches(55));
        assert!(!cron.minutes.matches(3));
    }

    #[test]
    fn test_parse_range() {
        let cron = parse_cron("0 9-17 * * *").unwrap();
        assert!(cron.hours.matches(9));
        assert!(cron.hours.matches(17));
        assert!(!cron.hours.matches(8));
        assert!(!cron.hours.matches(18));
    }

    #[test]
    fn test_parse_range_with_step() {
        let cron = parse_cron("0 0-23/2 * * *").unwrap();
        assert!(cron.hours.matches(0));
        assert!(cron.hours.matches(2));
        assert!(cron.hours.matches(22));
        assert!(!cron.hours.matches(1));
        assert!(!cron.hours.matches(23));
    }

    #[test]
    fn test_parse_list() {
        let cron = parse_cron("0 0 1,15 * *").unwrap();
        assert!(cron.days_of_month.matches(1));
        assert!(cron.days_of_month.matches(15));
        assert!(!cron.days_of_month.matches(5));
    }

    #[test]
    fn test_parse_month_names() {
        let cron = parse_cron("0 0 1 JAN *").unwrap();
        assert!(cron.months.matches(1));
        assert!(!cron.months.matches(2));
    }

    #[test]
    fn test_parse_dow_names() {
        let cron = parse_cron("0 0 * * MON-FRI").unwrap();
        assert!(cron.days_of_week.matches(0)); // Mon
        assert!(cron.days_of_week.matches(4)); // Fri
        assert!(!cron.days_of_week.matches(5)); // Sat
        assert!(!cron.days_of_week.matches(6)); // Sun
    }

    #[test]
    fn test_parse_complex_expression() {
        let cron = parse_cron("30 4 1,15 * 1-5").unwrap();
        // 分钟 = 30
        assert!(cron.minutes.matches(30));
        assert!(!cron.minutes.matches(0));
        // 小时 = 4
        assert!(cron.hours.matches(4));
        assert!(!cron.hours.matches(5));
        // 日 = 1, 15
        assert!(cron.days_of_month.matches(1));
        assert!(cron.days_of_month.matches(15));
        assert!(!cron.days_of_month.matches(10));
        // 月 = *
        assert!(cron.months.matches(1));
        assert!(cron.months.matches(12));
        // 周 = 1-5（Tue-Sat，0=Mon..6=Sun）
        assert!(cron.days_of_week.matches(1));
        assert!(cron.days_of_week.matches(5));
        assert!(!cron.days_of_week.matches(0));
        assert!(!cron.days_of_week.matches(6));
    }

    // ── 错误处理测试 ──

    #[test]
    fn test_parse_invalid_field_count() {
        let result = parse_cron("* * *");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("5 个字段"));
    }

    #[test]
    fn test_parse_step_zero() {
        let result = parse_cron("*/0 * * * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("不能为 0"));
    }

    #[test]
    fn test_parse_out_of_range() {
        let result = parse_cron("60 * * * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("超出有效范围"));
    }

    #[test]
    fn test_parse_invalid_number() {
        let result = parse_cron("abc * * * *");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_inverted_range() {
        let result = parse_cron("0 23-1 * * *");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_range_end_exceeds_max() {
        let result = parse_cron("* * * 1-13 *");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("超出最大值") || err.contains("超出有效范围"));
    }

    #[test]
    fn test_parse_range_start_below_min() {
        let result = parse_cron("* * * 0-6 *");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("小于最小值") || err.contains("超出有效范围"));
    }

    #[test]
    fn test_parse_range_step_exceeds_span() {
        let result = parse_cron("1-3/5 * * * *");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("步长") || err.contains("超过范围跨度"));
    }

    #[test]
    fn test_parse_range_with_step_valid() {
        // 确保合法的步长表达式仍然正常工作
        let result = parse_cron("1-10/3 * * * *");
        assert!(result.is_ok());
        let cron = result.unwrap();
        assert!(cron.minutes.matches(1));
        assert!(cron.minutes.matches(4));
        assert!(cron.minutes.matches(7));
        assert!(cron.minutes.matches(10));
        assert!(!cron.minutes.matches(2));
    }

    // ── next_after 测试 ──

    #[test]
    fn test_next_after_every_minute() {
        let cron = parse_cron("* * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap();
        let next = cron.next_after(from);
        // 下一分钟
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 12, 1, 0).unwrap());
    }

    #[test]
    fn test_next_after_every_5_minutes() {
        let cron = parse_cron("*/5 * * * *").unwrap();
        // 当前 12:03，下一个应该是 12:05
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 12, 3, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 12, 5, 0).unwrap());
    }

    #[test]
    fn test_next_after_hourly() {
        let cron = parse_cron("0 * * * *").unwrap();
        // 当前 12:30，下一个应该是 13:00
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 12, 30, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 13, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_daily() {
        let cron = parse_cron("0 0 * * *").unwrap();
        // 当前 12:00，下一个应该是次日 00:00
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_midnight_boundary() {
        let cron = parse_cron("30 23 * * *").unwrap();
        // 当前 23:30，下一个应该是次日 23:30
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 23, 30, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 2, 23, 30, 0).unwrap());
    }

    #[test]
    fn test_next_after_month_boundary() {
        let cron = parse_cron("0 0 1 * *").unwrap();
        // 当前 1月15日，下一个应该是 2月1日
        let from = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 2, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_year_boundary() {
        let cron = parse_cron("0 0 1 1 *").unwrap();
        // 当前 2025年6月，下一个应该是 2026年1月1日
        let from = Utc.with_ymd_and_hms(2025, 6, 15, 0, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_weekday_only() {
        let cron = parse_cron("0 0 * * 1").unwrap(); // 每周二（0=Mon..6=Sun）
        // 2025-01-01 是周三，下一个周二应该是 2025-01-07
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 7, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_specific_time() {
        let cron = parse_cron("30 14 * * *").unwrap(); // 每天 14:30
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 14, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 14, 30, 0).unwrap());
    }

    #[test]
    fn test_next_after_leap_year_feb29() {
        let cron = parse_cron("0 0 29 2 *").unwrap(); // 2月29日
        // 2024 是闰年，从 2024-02-29 之后搜索，下一个应该是 2028-02-29
        let from = Utc.with_ymd_and_hms(2024, 2, 29, 12, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2028, 2, 29, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_after_with_seconds() {
        // 确保 next_after 正确处理非整分钟输入
        let cron = parse_cron("0 * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 30).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 13, 0, 0).unwrap());
    }

    // ── CronScheduler 集成测试 ──

    #[tokio::test]
    async fn test_scheduler_register_and_count() {
        let scheduler = CronScheduler::new();

        scheduler
            .register("task-a", "*/5 * * * *", || {})
            .await
            .unwrap();
        scheduler
            .register("task-b", "0 * * * *", || {})
            .await
            .unwrap();

        assert_eq!(scheduler.task_count().await, 2);
    }

    #[tokio::test]
    async fn test_scheduler_duplicate_name() {
        let scheduler = CronScheduler::new();

        scheduler
            .register("task-a", "*/5 * * * *", || {})
            .await
            .unwrap();

        let result = scheduler.register("task-a", "0 * * * *", || {}).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("已存在"));
    }

    #[tokio::test]
    async fn test_scheduler_unregister() {
        let scheduler = CronScheduler::new();

        scheduler
            .register("task-a", "*/5 * * * *", || {})
            .await
            .unwrap();
        assert_eq!(scheduler.task_count().await, 1);

        let removed = scheduler.unregister("task-a").await;
        assert!(removed);
        assert_eq!(scheduler.task_count().await, 0);

        // 再次移除应返回 false
        let removed = scheduler.unregister("task-a").await;
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_scheduler_task_names() {
        let scheduler = CronScheduler::new();

        scheduler
            .register("heartbeat", "*/5 * * * *", || {})
            .await
            .unwrap();
        scheduler
            .register("cleanup", "0 0 * * *", || {})
            .await
            .unwrap();

        let names = scheduler.task_names().await;
        assert!(names.contains(&"heartbeat".to_string()));
        assert!(names.contains(&"cleanup".to_string()));
    }

    #[tokio::test]
    async fn test_scheduler_start_and_shutdown() {
        let scheduler = CronScheduler::new();

        scheduler
            .register("test-task", "*/5 * * * *", || {})
            .await
            .unwrap();

        // 启动
        scheduler.start().await.unwrap();
        assert!(scheduler.is_running().await);

        // 重复启动应报错
        let result = scheduler.start().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("已在运行"));

        // 关闭
        scheduler.shutdown().await;
        assert!(!scheduler.is_running().await);
    }

    #[tokio::test]
    async fn test_scheduler_callback_execution() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);

        let scheduler = CronScheduler::new();

        // 注册一个不可能触发的时间（每年 2 月 30 号）来验证启动/关闭流程
        // 不使用 "* * * * *" 因为它在整分钟边界会产生 0 延迟导致密集触发
        scheduler
            .register("counter", "30 2 30 2 *", move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            })
            .await
            .unwrap();

        scheduler.start().await.unwrap();

        // 等待一小段时间后关闭
        sleep(Duration::from_millis(200)).await;
        scheduler.shutdown().await;

        // 回调不应被触发（2 月 30 号不存在）
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_scheduler_invalid_cron_rejected() {
        let scheduler = CronScheduler::new();

        let result = scheduler.register("bad", "invalid", || {}).await;
        assert!(result.is_err());
        assert_eq!(scheduler.task_count().await, 0);
    }

    #[tokio::test]
    async fn test_scheduler_default() {
        let scheduler = CronScheduler::default();
        assert!(!scheduler.is_running().await);
        assert_eq!(scheduler.task_count().await, 0);
    }

    // ── 边界条件测试 ──

    #[test]
    fn test_parse_all_zeros() {
        // 月=0 超出范围 [1,12]，应该失败
        let result = parse_cron("0 0 0 0 0");
        assert!(result.is_err());
        // 正确的用法：0 0 1 1 0
        let cron = parse_cron("0 0 1 1 0").unwrap();
        assert!(cron.minutes.matches(0));
        assert!(cron.hours.matches(0));
        assert!(cron.days_of_month.matches(1));
        assert!(cron.months.matches(1));
        assert!(cron.days_of_week.matches(0)); // Mon
    }

    #[test]
    fn test_parse_comma_with_range() {
        let cron = parse_cron("0,30 9-17 * * *").unwrap();
        assert!(cron.minutes.matches(0));
        assert!(cron.minutes.matches(30));
        assert!(!cron.minutes.matches(15));
        assert!(cron.hours.matches(9));
        assert!(cron.hours.matches(17));
    }

    #[test]
    fn test_parse_step_with_range() {
        let cron = parse_cron("0-30/10 * * * *").unwrap();
        assert!(cron.minutes.matches(0));
        assert!(cron.minutes.matches(10));
        assert!(cron.minutes.matches(20));
        assert!(cron.minutes.matches(30));
        assert!(!cron.minutes.matches(5));
    }

    #[test]
    fn test_parse_month_name_lowercase() {
        let cron = parse_cron("0 0 1 jan *").unwrap();
        assert!(cron.months.matches(1));
    }

    #[test]
    fn test_parse_dow_name_lowercase() {
        let cron = parse_cron("0 0 * * mon").unwrap();
        assert!(cron.days_of_week.matches(0));
    }

    #[test]
    fn test_matches_datetime() {
        // "30 14 1 1 1" = 1月1日 14:30 或每周二 14:30
        let cron = parse_cron("30 14 1 1 1").unwrap();
        // 2025-01-01 14:30:00 应匹配（日匹配）
        let dt = Utc.with_ymd_and_hms(2025, 1, 1, 14, 30, 0).unwrap();
        assert!(cron.matches(dt));

        // 2025-01-01 14:31:00 不应匹配（分钟不对）
        let dt = Utc.with_ymd_and_hms(2025, 1, 1, 14, 31, 0).unwrap();
        assert!(!cron.matches(dt));

        // 2025-01-07 14:30:00 应匹配（星期二匹配，虽然日不匹配）
        // 2025-01-01 是周三(2)，所以 2025-01-07 是周二(1)
        let dt = Utc.with_ymd_and_hms(2025, 1, 7, 14, 30, 0).unwrap();
        assert!(cron.matches(dt));

        // 2025-01-02 14:30:00 不应匹配（日不对，星期也不对）
        let dt = Utc.with_ymd_and_hms(2025, 1, 2, 14, 30, 0).unwrap();
        assert!(!cron.matches(dt));
    }

    #[test]
    fn test_next_after_does_not_loop_forever() {
        // 确保极端表达式不会导致无限循环
        // 使用一个罕见但有效的日期：每月 31 日（只有 1/3/5/7/8/10/12 月有 31 日）
        let cron = parse_cron("0 0 31 * *").unwrap();
        let from = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let next = cron.next_after(from);
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 31, 0, 0, 0).unwrap());
    }
}
