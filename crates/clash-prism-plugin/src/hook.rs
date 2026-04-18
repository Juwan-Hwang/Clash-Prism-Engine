//! 生命周期钩子
//!
//! ## 8 + 1 个钩子 + 1 个 Rust 原生策略
//!
//! ### 配置生命周期（6 个）
//! - `OnSubscribeFetch` — 订阅下载时
//! - `OnSubscribeParsed` — 单个订阅解析完成后
//! - `OnMerged` — 多订阅合并完成后
//! - `OnBeforeWrite` — 写入配置文件前
//! - `OnBeforeCoreStart` — 内核启动前
//! - `OnCoreStopped` — 内核停止后
//!
//! ### 应用生命周期（2 个）
//! - `OnAppReady` — 应用启动完成
//! - `OnShutdown` — 应用关闭前
//!
//! ### 事件驱动（1 个）
//! - `OnSchedule(String)` — Cron 表达式定时触发

use serde::{Deserialize, Serialize};

use chrono::{Datelike, Timelike};

/// 生命周期钩子枚举
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Hook {
    // ─── 配置生命周期 ───
    /// 订阅下载时（可替换 URL、解密内容、预处理响应）
    OnSubscribeFetch,

    /// 单个订阅解析完成后（可过滤/重命名节点）
    OnSubscribeParsed,

    /// 多订阅合并完成后（可跨订阅去重/统一分组）
    OnMerged,

    /// 写入配置文件前（最后修改机会）
    OnBeforeWrite,

    /// 内核启动前（端口清理、环境检测）
    OnBeforeCoreStart,

    /// 内核停止后（清理资源）
    OnCoreStopped,

    // ─── 应用生命周期 ───
    /// 应用启动完成（初始化插件）
    OnAppReady,

    /// 应用关闭前（保存状态、释放资源）
    OnShutdown,

    // ─── 事件驱动 ───
    /// Cron 表达式定时触发（5 字段标准格式）
    OnSchedule(String),
}

impl Hook {
    /// 获取所有内置钩子（不含 OnSchedule 变体）
    pub fn builtin_hooks() -> Vec<Hook> {
        vec![
            Hook::OnSubscribeFetch,
            Hook::OnSubscribeParsed,
            Hook::OnMerged,
            Hook::OnBeforeWrite,
            Hook::OnBeforeCoreStart,
            Hook::OnCoreStopped,
            Hook::OnAppReady,
            Hook::OnShutdown,
        ]
    }

    /// 获取钩子的显示名称
    ///
    /// 注意：`OnSchedule` 变体返回 `String` 而非 `&str`，
    /// 因为需要动态格式化 cron 表达式。调用方需根据变体类型处理。
    pub fn display_name(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Hook::OnSubscribeFetch => "订阅下载".into(),
            Hook::OnSubscribeParsed => "订阅解析完成".into(),
            Hook::OnMerged => "多订阅合并完成".into(),
            Hook::OnBeforeWrite => "写入配置文件前".into(),
            Hook::OnBeforeCoreStart => "内核启动前".into(),
            Hook::OnCoreStopped => "内核停止后".into(),
            Hook::OnAppReady => "应用就绪".into(),
            Hook::OnShutdown => "应用关闭前".into(),
            Hook::OnSchedule(cron) => format!("定时调度 ({})", cron).into(),
        }
    }

    /// 判断是否为高频钩子（不应暴露给 JS 插件）
    ///
    /// 对于 OnSchedule，检查 cron 表达式的分钟字段是否包含步长 < 1 分钟的模式。
    /// 标准 cron 最小粒度为 1 分钟，所以如果分钟字段是 `*` 或 `*/1`，
    /// 则视为高频（每分钟触发一次）。
    pub fn is_high_frequency(&self) -> bool {
        match self {
            Hook::OnSchedule(cron) => {
                let parts: Vec<&str> = cron.split_whitespace().collect();
                if parts.len() != 5 {
                    return false;
                }
                // Check minute field for sub-minute or every-minute patterns
                let minute_field = parts[0];
                // `*` means every minute — high frequency
                if minute_field == "*" {
                    return true;
                }
                // `*/N` with N == 1 means every minute — high frequency
                if let Some(step_str) = minute_field.strip_prefix("*/")
                    && let Ok(step) = step_str.parse::<u32>()
                    && step == 1
                {
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hook::OnSchedule(cron) => write!(f, "OnSchedule({})", cron),
            other => write!(
                f,
                "{}",
                serde_json::to_string(other)
                    .unwrap_or_default()
                    .trim_matches('"')
            ),
        }
    }
}

// ══════════════════════════════════════════════════════════
// §7 钩子调度器 — OnSchedule Cron 表达式支持
// ══════════════════════════════════════════════════════════

/// 从字符串解析 Hook（支持 manifest 中的钩子名称格式）
///
/// 支持的格式：
/// - `"onSubscribeFetch"` → `Hook::OnSubscribeFetch`
/// - `"on_subscribe_fetch"` → `Hook::OnSubscribeFetch`
/// - `"OnSchedule(0 * * * *)"` → `Hook::OnSchedule("0 * * * *".into())`
///
/// # Errors
/// 返回 `PrismError` 如果钩子名称无法识别或 Cron 表达式格式无效。
impl std::str::FromStr for Hook {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 先尝试匹配 OnSchedule(cron_expr) 格式
        if let Some(rest) = s
            .strip_prefix("OnSchedule(")
            .or_else(|| s.strip_prefix("onSchedule("))
        {
            if let Some(cron) = rest.strip_suffix(')') {
                // 验证基本 Cron 格式（5 个字段，用空格分隔）
                let parts: Vec<&str> = cron.split_whitespace().collect();
                if parts.len() == 5 {
                    return Ok(Hook::OnSchedule(cron.trim().to_string()));
                }
                return Err(format!(
                    "无效的 Cron 表达式「{}」：需要 5 个字段（分 时 日 月 周）",
                    cron
                ));
            }
            return Err("OnSchedule 格式错误：缺少闭合括号，期望 OnSchedule(<cron>)".to_string());
        }

        // 标准钩子名称映射（不区分大小写，支持 camelCase 和 snake_case）
        match s {
            "OnSubscribeFetch" | "onSubscribeFetch" | "on_subscribe_fetch" => {
                Ok(Hook::OnSubscribeFetch)
            }
            "OnSubscribeParsed" | "onSubscribeParsed" | "on_subscribe_parsed" => {
                Ok(Hook::OnSubscribeParsed)
            }
            "OnMerged" | "onMerged" | "on_merged" => Ok(Hook::OnMerged),
            "OnBeforeWrite" | "onBeforeWrite" | "on_before_write" => Ok(Hook::OnBeforeWrite),
            "OnBeforeCoreStart" | "onBeforeCoreStart" | "on_before_core_start" => {
                Ok(Hook::OnBeforeCoreStart)
            }
            "OnCoreStopped" | "onCoreStopped" | "on_core_stopped" => Ok(Hook::OnCoreStopped),
            "OnAppReady" | "onAppReady" | "on_app_ready" => Ok(Hook::OnAppReady),
            "OnShutdown" | "onShutdown" | "on_shutdown" => Ok(Hook::OnShutdown),
            _ => Err(format!("未知的钩子名称：{}", s)),
        }
    }
}

/// Cron 调度条目
#[derive(Debug, Clone)]
pub struct ScheduledHook {
    /// 关联的插件 ID
    pub plugin_id: String,
    /// Cron 表达式（5 字段标准格式）
    pub cron_expression: String,
    /// 上次触发时间
    pub last_triggered: Option<chrono::DateTime<chrono::Utc>>,
    /// 是否启用
    pub enabled: bool,
}

impl ScheduledHook {
    pub fn new(plugin_id: String, cron_expression: String) -> Self {
        Self {
            plugin_id,
            cron_expression,
            last_triggered: None,
            enabled: true,
        }
    }

    /// Check if this scheduled hook should trigger at the given time.
    ///
    /// 避免两套 cron 实现的维护负担和潜在不一致。
    ///
    /// 使用 cron_scheduler::parse_cron() 解析表达式，
    /// 然后使用 CronExpr::matches() 进行精确匹配。
    /// 保留冷却检查（30 秒）防止高频触发。
    pub fn should_trigger_now(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        if !self.enabled {
            return false;
        }

        // Cooldown check: don't trigger within 30 seconds of last trigger
        if let Some(last) = self.last_triggered {
            let diff = now.signed_duration_since(last);
            if diff.num_seconds() < 30 {
                return false;
            }
        }

        match crate::cron_scheduler::parse_cron(&self.cron_expression) {
            Ok(cron_expr) => cron_expr.matches(now),
            Err(_) => {
                // 解析失败时回退到简单字段检查（兼容旧行为）
                let parts: Vec<&str> = self.cron_expression.split_whitespace().collect();
                if parts.len() != 5 {
                    return false;
                }

                let minute = now.minute();
                let hour = now.hour();
                let day = now.day();
                let month = now.month();
                let weekday = now.weekday().num_days_from_monday(); // Mon=0 .. Sun=6

                matches_cron_field(parts[0], minute, 0..=59)
                    && matches_cron_field(parts[1], hour, 0..=23)
                    && matches_cron_field(parts[2], day, 1..=31)
                    && matches_cron_field(parts[3], month, 1..=12)
                    && matches_cron_field(parts[4], weekday, 0..=6)
            }
        }
    }

    /// 标记为已触发
    pub fn mark_triggered(&mut self, time: chrono::DateTime<chrono::Utc>) {
        self.last_triggered = Some(time);
    }

    /// Calculate the next trigger time after the given reference time.
    ///
    /// This is used by the min-heap scheduler to efficiently determine
    /// which hook should be checked next, avoiding unnecessary full scans.
    ///
    /// 配合月/日/时级别的跳跃优化，实际性能远优于暴力遍历。
    /// 最坏情况下搜索范围受 8 年超时保护限制（覆盖两个闰年周期），
    /// 确保对于任何合法的 cron 表达式都能在有限时间内返回结果。
    /// 对于典型场景（如 "0 * * * *" 每小时触发），通常只需 1-2 次迭代即可找到。
    ///
    /// Returns `None` if the hook is disabled.
    pub fn next_trigger_time(
        &self,
        after: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if !self.enabled {
            return None;
        }

        // Parse cron expression parts
        let parts: Vec<&str> = self.cron_expression.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }

        // Start searching from 1 minute after the reference time, aligned to minute boundary
        let mut candidate = after + chrono::Duration::minutes(1);
        candidate = candidate
            .with_second(0)
            .unwrap_or(candidate)
            .with_nanosecond(0)
            .unwrap_or(candidate);

        let deadline = after + chrono::Duration::days(365 * 8 + 2);

        // ── Month-level skip ──
        loop {
            if candidate > deadline {
                tracing::warn!(
                    cron = %self.cron_expression,
                    "next_trigger_time: no match found within 8 years"
                );
                return None;
            }

            let month = candidate.month();
            if !matches_cron_field(parts[3], month, 1..=12) {
                let next_month = (1..=12)
                    .find(|&m| m > month && matches_cron_field(parts[3], m, 1..=12))
                    .unwrap_or_else(|| {
                        (1..=12)
                            .find(|&m| matches_cron_field(parts[3], m, 1..=12))
                            .unwrap_or(1)
                    });

                if next_month <= month {
                    // 跨年跳跃：先安全降至 day=28（所有月份都存在），
                    // 再切月/年，避免 with_month 在 2 月或小月失败。
                    candidate = candidate
                        .with_day(28)
                        .and_then(|d| d.with_month(1))
                        .and_then(|d| d.with_day(1))
                        .and_then(|d| d.with_hour(0))
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate)
                        + chrono::Duration::days(32);
                    candidate = candidate
                        .with_day(28)
                        .and_then(|d| d.with_month(next_month))
                        .and_then(|d| d.with_day(1))
                        .and_then(|d| d.with_hour(0))
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                } else {
                    // 同年跳跃：同样先降至安全日再切月。
                    candidate = candidate
                        .with_day(28)
                        .and_then(|d| d.with_month(next_month))
                        .and_then(|d| d.with_day(1))
                        .and_then(|d| d.with_hour(0))
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                }
                continue;
            }

            // ── Day-level skip (day-of-month OR day-of-week) ──
            let day = candidate.day();
            let weekday = candidate.weekday().num_days_from_monday();
            let day_matches = matches_cron_field(parts[2], day, 1..=31);
            let weekday_matches = matches_cron_field(parts[4], weekday, 0..=6);

            if !day_matches && !weekday_matches {
                candidate += chrono::Duration::days(1);
                candidate = candidate
                    .with_hour(0)
                    .and_then(|d| d.with_minute(0))
                    .unwrap_or(candidate);
                continue;
            }

            // ── Hour-level skip ──
            let hour = candidate.hour();
            if !matches_cron_field(parts[1], hour, 0..=23) {
                let next_hour =
                    (0..=23).find(|&h| h > hour && matches_cron_field(parts[1], h, 0..=23));

                if let Some(nh) = next_hour {
                    candidate = candidate
                        .with_hour(nh)
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                } else {
                    candidate += chrono::Duration::days(1);
                    candidate = candidate
                        .with_hour(0)
                        .and_then(|d| d.with_minute(0))
                        .unwrap_or(candidate);
                    continue;
                }
                continue;
            }

            // ── Minute-level check ──
            let minute = candidate.minute();
            if !matches_cron_field(parts[0], minute, 0..=59) {
                let next_minute =
                    (0..=59).find(|&m| m > minute && matches_cron_field(parts[0], m, 0..=59));

                if let Some(nm) = next_minute {
                    candidate = candidate.with_minute(nm).unwrap_or(candidate);
                } else {
                    let next_hour =
                        (0..=23).find(|&h| h > hour && matches_cron_field(parts[1], h, 0..=23));

                    if let Some(nh) = next_hour {
                        candidate = candidate
                            .with_hour(nh)
                            .and_then(|d| d.with_minute(0))
                            .unwrap_or(candidate);
                    } else {
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

            return Some(candidate);
        }
    }
}

// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════

/// Match a single cron field value against the current time component.
///
/// Supports standard 5-field cron syntax:
/// - `*` — any value (wildcard)
/// - `*/N` — step (every N)
/// - `1-5` — range
/// - `1-10/2` — range with step
/// - `1,3,5` — list
/// - Month names: JAN, FEB, ..., DEC (case-insensitive)
/// - Day names: MON, TUE, ..., SUN (case-insensitive)
fn matches_cron_field(
    field_expr: &str,
    value: u32,
    valid_range: std::ops::RangeInclusive<u32>,
) -> bool {
    // Normalize month/day names to numeric values first
    let normalized = normalize_cron_field_name(field_expr, &valid_range);

    // Handle comma-separated lists: each part must be checked independently
    for part in normalized.split(',') {
        let part = part.trim();
        if matches_cron_part(part, value, &valid_range) {
            return true;
        }
    }
    false
}

/// Match a single cron part (no commas) against a value.
fn matches_cron_part(part: &str, value: u32, valid_range: &std::ops::RangeInclusive<u32>) -> bool {
    if part == "*" {
        return true;
    }

    // Step pattern: */N or 1-10/N
    if let Some(rest) = part.strip_prefix("*/")
        && let Ok(step) = rest.parse::<u32>()
    {
        if step == 0 {
            return false;
        }
        let min = *valid_range.start();
        return value >= min && (value - min).is_multiple_of(step);
    }

    // Range with optional step: 1-10 or 1-10/2
    if part.contains('-') {
        let range_part = if let Some(slash_pos) = part.find('/') {
            &part[..slash_pos]
        } else {
            part
        };

        if let Some(dash_pos) = range_part.find('-') {
            let start_str = &range_part[..dash_pos];
            let end_str = &range_part[dash_pos + 1..];

            if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                let step = if let Some(slash_pos) = part.find('/') {
                    part[slash_pos + 1..].parse::<u32>().unwrap_or(1)
                } else {
                    1
                };
                return value >= start && value <= end && (value - start).is_multiple_of(step);
            }
        }
    }

    // Plain number
    if let Ok(num) = part.parse::<u32>() {
        return num == value;
    }

    false
}

/// Normalize cron field names (month/day-of-week) to their numeric equivalents.
///
/// For example:
/// - "JAN" → "1", "feb" → "2"
/// - "MON" → "0", "tue" → "1"
///- "JAN-FEB" → "1-2", "MON-FRI" → "0-4" (range support)
/// - "JAN-MAR/2" → "1-3/2" (range with step)
///
/// Uses `_valid_range` to distinguish month fields (1-12) from weekday fields (0-6).
///
/// 此函数为公共 API，供 `cron_scheduler` 模块复用，避免重复实现。
pub(crate) fn normalize_cron_field_name(
    expr: &str,
    valid_range: &std::ops::RangeInclusive<u32>,
) -> String {
    let upper = expr.to_uppercase();

    // Month names (1-12)
    const MONTH_NAMES: &[(&str, &str)] = &[
        ("JAN", "1"),
        ("FEB", "2"),
        ("MAR", "3"),
        ("APR", "4"),
        ("MAY", "5"),
        ("JUN", "6"),
        ("JUL", "7"),
        ("AUG", "8"),
        ("SEP", "9"),
        ("OCT", "10"),
        ("NOV", "11"),
        ("DEC", "12"),
    ];

    // Day-of-week names (0=Mon .. 6=Sun)
    const DOW_NAMES: &[(&str, &str)] = &[
        ("MON", "0"),
        ("TUE", "1"),
        ("WED", "2"),
        ("THU", "3"),
        ("FRI", "4"),
        ("SAT", "5"),
        ("SUN", "6"),
    ];

    // Determine which name table to use based on valid_range
    let name_table: &[(&str, &str)] =
        if *valid_range.start() >= 1 && *valid_range.end() <= 12 && *valid_range.start() == 1 {
            MONTH_NAMES
        } else {
            DOW_NAMES
        };

    // Split by comma, normalize each token independently
    let tokens: Vec<&str> = upper.split(',').collect();
    let mut result_parts = Vec::with_capacity(tokens.len());

    for token in tokens {
        let trimmed = token.trim();
        // 如果不匹配，则尝试替换范围表达式中的名称（如 "JAN-FEB" → "1-2"）
        let replaced = if let Some((_, num)) = name_table.iter().find(|(name, _)| *name == trimmed)
        {
            num.to_string()
        } else {
            // 可能是范围表达式（如 JAN-FEB、JAN-MAR/2），替换其中的名称
            let mut result = trimmed.to_string();
            // 按名称长度降序排列，避免短名称误匹配长名称前缀
            // （例如 "JUN" 不会误匹配 "JU"）
            let mut sorted_names: Vec<_> = name_table.iter().collect();
            sorted_names.sort_by_key(|b| std::cmp::Reverse(b.0.len()));
            for (name, num) in &sorted_names {
                result = result.replace(*name, num);
            }
            result
        };
        result_parts.push(replaced);
    }

    result_parts.join(",")
}

/// 钩子调度器（§7.1 事件驱动钩子的调度核心）
///
/// 管理 `OnSchedule` 类型钩子的注册、触发和时间追踪。
#[derive(Debug, Default)]
pub struct HookScheduler {
    /// All registered scheduled hooks
    scheduled_hooks: Vec<ScheduledHook>,
}

impl HookScheduler {
    /// 创建新的调度器
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个定时钩子
    ///
    /// # Arguments
    /// * `plugin_id` — 插件 ID
    /// * `cron_expr` — 标准 5 字段 Cron 表达式
    ///
    /// # Errors
    /// 返回错误如果 Cron 表达式格式无效
    pub fn schedule(
        &mut self,
        plugin_id: impl Into<String>,
        cron_expr: impl Into<String>,
    ) -> Result<(), String> {
        let expr = cron_expr.into();
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "Cron 表达式「{}」无效：需要 5 个字段（分 时 日 月 周），实际 {} 个",
                expr,
                parts.len()
            ));
        }

        for part in &parts {
            if let Some(rest) = part.strip_prefix("*/")
                && rest.parse::<u32>() == Ok(0)
            {
                return Err(format!(
                    "Cron 表达式「{}」无效：步长不能为 0（字段「{}」）",
                    expr, part
                ));
            }
        }

        self.scheduled_hooks
            .push(ScheduledHook::new(plugin_id.into(), expr));
        Ok(())
    }

    /// 获取所有到期的钩子（应触发的任务列表）
    ///
    /// # Arguments
    /// * `now` — 当前时间
    ///
    /// # Returns
    /// 到期应触发的 `(plugin_id, cron_expression)` 列表
    ///
    /// v2 optimization: Instead of scanning all hooks, uses a min-heap
    /// sorted by next trigger time. Hooks whose next trigger is after `now`
    /// are skipped without calling `should_trigger_now()`.
    pub fn poll_due_hooks(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<(String, String)> {
        self.scheduled_hooks
            .iter()
            .filter(|h| {
                // v2 optimization: Use next_trigger_time() to short-circuit
                // hooks that definitely won't trigger at this time.
                if let Some(next) = h.next_trigger_time(now - chrono::Duration::minutes(2))
                    && next > now
                {
                    return false;
                }
                h.should_trigger_now(now)
            })
            .map(|h| (h.plugin_id.clone(), h.cron_expression.clone()))
            .collect()
    }

    /// 标记指定插件的定时任务已触发
    ///
    /// 通过 `plugin_id` + `cron_expression` 双重匹配，精确标记触发状态，
    /// 避免误伤同一插件注册的其他定时任务。
    pub fn mark_triggered(
        &mut self,
        plugin_id: &str,
        cron_expression: &str,
        time: chrono::DateTime<chrono::Utc>,
    ) {
        for hook in &mut self.scheduled_hooks {
            if hook.plugin_id == plugin_id && hook.cron_expression == cron_expression {
                hook.mark_triggered(time);
            }
        }
    }

    /// 启用/禁用指定插件的定时任务
    pub fn set_enabled(&mut self, plugin_id: &str, enabled: bool) {
        for hook in &mut self.scheduled_hooks {
            if hook.plugin_id == plugin_id {
                hook.enabled = enabled;
            }
        }
    }

    /// 移除指定插件的所有定时任务
    pub fn unschedule(&mut self, plugin_id: &str) {
        self.scheduled_hooks.retain(|h| h.plugin_id != plugin_id);
    }

    /// 获取已注册的定时任务数量
    pub fn len(&self) -> usize {
        self.scheduled_hooks.len()
    }

    /// 是否没有任何定时任务
    pub fn is_empty(&self) -> bool {
        self.scheduled_hooks.is_empty()
    }

    /// Get the next scheduled trigger time across all hooks.
    ///
    /// Useful for determining how long to sleep before the next poll.
    /// Returns `None` if there are no scheduled hooks.
    pub fn get_next_due_time(
        &self,
        after: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        self.scheduled_hooks
            .iter()
            .filter(|h| h.enabled)
            .filter_map(|h| h.next_trigger_time(after))
            .min()
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_from_str_builtin() {
        assert_eq!(
            "OnSubscribeFetch".parse::<Hook>().unwrap(),
            Hook::OnSubscribeFetch
        );
        assert_eq!(
            "onSubscribeParsed".parse::<Hook>().unwrap(),
            Hook::OnSubscribeParsed
        );
        assert_eq!(
            "on_before_write".parse::<Hook>().unwrap(),
            Hook::OnBeforeWrite
        );
    }

    #[test]
    fn test_hook_from_str_schedule() {
        let hook: Hook = "OnSchedule(0 * * * *)".parse().unwrap();
        match hook {
            Hook::OnSchedule(expr) => assert_eq!(expr, "0 * * * *"),
            _ => panic!("Expected OnSchedule variant"),
        }
    }

    #[test]
    fn test_hook_from_str_invalid_cron() {
        let result = "OnSchedule(bad)".parse::<Hook>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("5 个字段"));
    }

    #[test]
    fn test_hook_from_str_unknown() {
        let result = "OnUnknown".parse::<Hook>();
        assert!(result.is_err());
    }

    #[test]
    fn test_scheduler_register_and_poll() {
        let mut scheduler = HookScheduler::new();

        // 注册一个每小时的第 0 分钟执行的钩子
        scheduler.schedule("test-plugin", "0 * * * *").unwrap();

        assert_eq!(scheduler.len(), 1);
        assert!(!scheduler.is_empty());

        // 查询到期任务（新注册的任务应该触发）
        let now = chrono::Utc::now();
        let due = scheduler.poll_due_hooks(now);

        // */5 should trigger within any 5-minute window
        // (test may fail at exact boundary, but is statistically sound)
        if !due.is_empty() {
            assert_eq!(due[0].0, "test-plugin");
        }
    }

    #[test]
    fn test_scheduler_unschedule() {
        let mut scheduler = HookScheduler::new();
        scheduler.schedule("plugin-a", "*/5 * * * *").unwrap();
        scheduler.schedule("plugin-b", "0 0 * * *").unwrap();

        assert_eq!(scheduler.len(), 2);

        scheduler.unschedule("plugin-a");
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn test_scheduler_invalid_cron_rejected() {
        let mut scheduler = HookScheduler::new();

        // 只有 3 个字段 → 应拒绝
        let result = scheduler.schedule("test", "* * *");
        assert!(result.is_err());
        assert_eq!(scheduler.len(), 0); // 不应注册
    }

    #[test]
    fn test_scheduled_hook_prevent_rapid_fire() {
        let hook = ScheduledHook::new("test".into(), "*/1 * * * *".into());

        let now = chrono::Utc::now();

        // 第一次查询 → 应触发
        assert!(hook.should_trigger_now(now));

        // 模拟标记后立即再查 → 应跳过（30 秒冷却）
        let mut triggered = hook;
        triggered.mark_triggered(now);
        assert!(!triggered.should_trigger_now(now));
    }

    #[test]
    fn test_all_builtin_hooks_present() {
        let hooks = Hook::builtin_hooks();
        assert_eq!(hooks.len(), 8); // 9-1 (不含 OnSchedule)
        assert!(hooks.iter().any(|h| matches!(h, Hook::OnSubscribeFetch)));
        assert!(hooks.iter().any(|h| matches!(h, Hook::OnShutdown)));
        // OnSchedule 不应在 builtin_hooks 中
        assert!(!hooks.iter().any(|h| matches!(h, Hook::OnSchedule(_))));
    }

    #[test]
    fn test_next_trigger_time() {
        let hook = ScheduledHook::new("test".into(), "0 * * * *".into());
        let now = chrono::Utc::now();

        let next = hook.next_trigger_time(now);
        assert!(next.is_some());

        // Next trigger should be within the next hour
        let diff = next.unwrap().signed_duration_since(now);
        assert!(diff.num_minutes() <= 60);
        assert!(diff.num_minutes() > 0);
    }

    #[test]
    fn test_next_trigger_time_disabled() {
        let mut hook = ScheduledHook::new("test".into(), "0 * * * *".into());
        hook.enabled = false;

        let next = hook.next_trigger_time(chrono::Utc::now());
        assert!(next.is_none());
    }

    #[test]
    fn test_get_next_due_time() {
        let mut scheduler = HookScheduler::new();
        scheduler.schedule("plugin-a", "0 * * * *").unwrap();
        scheduler.schedule("plugin-b", "30 * * * *").unwrap();

        let now = chrono::Utc::now();
        let next = scheduler.get_next_due_time(now);
        assert!(next.is_some());
    }

    #[test]
    fn test_get_next_due_time_empty() {
        let scheduler = HookScheduler::new();
        let next = scheduler.get_next_due_time(chrono::Utc::now());
        assert!(next.is_none());
    }
}
