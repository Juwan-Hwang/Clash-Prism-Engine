//! # Scope System — Four-Layer Scoping
//!
//! ## Hierarchy (lower priority → higher priority)
//!
//! | Layer | Variant | Description |
//! |-------|---------|-------------|
//! | 0 | [`Scope::Profile`] | Applies to a specific profile only |
//! | 1 | [`Scope::Global`] | Applies to all configurations |
//! | 2 | [`Scope::Scoped`] | Conditional: platform / core / profile / time |
//! | 3 | [`Scope::Runtime`] | UI settings layer (TUN switch, DNS mode, etc.) |
//!
//! ## Time Range Support
//!
//! [`Scope::Scoped`] supports `__when__.time` with format `"HH:mm-HH:mm"`.
//! Cross-midnight ranges like `"23:00-06:00"` are handled correctly.

use serde::{Deserialize, Serialize};

// Timelike trait 提供 .hour() / .minute() 方法
use chrono::Timelike;

/// Four-layer scope system for conditional Patch execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum Scope {
    /// Global scope — applies to all configurations
    #[default]
    Global,

    /// Profile-level scope — applies only to the specified profile
    Profile(String),

    /// Conditional scope — applies only when conditions are met
    Scoped {
        /// Profile name or regex
        profile: Option<String>,
        /// Operating system platform
        platform: Option<Vec<Platform>>,
        /// Proxy core type
        core: Option<String>,
        /// Time range (format "HH:mm-HH:mm", e.g., "08:00-23:00")
        /// time-based conditional scoping
        #[serde(default)]
        time_range: Option<TimeRange>,
        /// File-level enabled flag. When false, the patch is skipped.
        /// When None, no enabled restriction is applied.
        #[serde(default)]
        enabled: Option<bool>,
        /// WiFi SSID condition. When set, the patch only applies when
        /// the current SSID matches. Requires external SSID monitoring.
        #[serde(default)]
        ssid: Option<String>,
    },

    /// Runtime scope — UI-driven quick toggles (TUN/DNS mode, etc.)
    Runtime,
}

impl Scope {
    /// Create a Global scope.
    pub fn global() -> Self {
        Self::Global
    }

    /// Create a Profile scope for the given name.
    pub fn profile(name: impl Into<String>) -> Self {
        Self::Profile(name.into())
    }

    /// Create a Scoped builder for constructing conditional scopes.
    pub fn scoped() -> ScopedBuilder {
        ScopedBuilder::new()
    }

    /// Create a Runtime scope (UI-driven).
    pub fn runtime() -> Self {
        Self::Runtime
    }

    /// Check if this is a Global scope.
    pub fn is_global(&self) -> bool {
        matches!(self, Scope::Global)
    }

    /// Get the scope's priority level (higher = overrides lower).
    ///
    /// Note: This reflects **override priority**, not execution order.
    /// Priority is only used for Phase 2 internal sorting within the
    /// two-stage pipeline (Profile → Shared). It does NOT determine
    /// the physical execution order of patches across phases.
    pub fn priority(&self) -> u8 {
        match self {
            Scope::Global => 1,
            Scope::Profile(_) => 0,
            Scope::Scoped { .. } => 2,
            Scope::Runtime => 3,
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Global => write!(f, "Global"),
            Scope::Profile(name) => write!(f, "Profile({})", name),
            Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                write!(f, "Scoped(")?;
                let mut parts = vec![];
                if let Some(p) = profile {
                    parts.push(format!("profile={}", p));
                }
                if let Some(p) = platform {
                    parts.push(format!(
                        "platform=[{}]",
                        p.iter()
                            .map(|pl| pl.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if let Some(c) = core {
                    parts.push(format!("core={}", c));
                }
                if let Some(t) = time_range {
                    parts.push(format!("time={}", t));
                }
                if let Some(e) = enabled {
                    parts.push(format!("enabled={}", e));
                }
                if let Some(s) = ssid {
                    parts.push(format!("ssid={}", s));
                }
                write!(f, "{})", parts.join(", "))
            }
            Scope::Runtime => write!(f, "Runtime"),
        }
    }
}

/// Operating system platform enum.
///
/// Note: BSD variants (FreeBSD, OpenBSD, NetBSD) are not listed as separate
/// variants. Prism currently targets desktop and mobile platforms where
/// mihomo/clash-rs kernels are commonly deployed. If BSD support is needed
/// in the future, add variants here and update `parse_platform()` in compiler.rs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    MacOS,
    Linux,
    Android,
    IOS,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Windows => write!(f, "windows"),
            Platform::MacOS => write!(f, "macos"),
            Platform::Linux => write!(f, "linux"),
            Platform::Android => write!(f, "android"),
            Platform::IOS => write!(f, "ios"),
        }
    }
}

/// Builder for constructing [`Scope::Scoped`] conditions.
pub struct ScopedBuilder {
    profile: Option<String>,
    platform: Option<Vec<Platform>>,
    core: Option<String>,
    time_range: Option<TimeRange>,
    enabled: Option<bool>,
    ssid: Option<String>,
}

impl Default for ScopedBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedBuilder {
    /// Create a new ScopedBuilder with all fields unset.
    pub fn new() -> Self {
        Self {
            profile: None,
            platform: None,
            core: None,
            time_range: None,
            enabled: None,
            ssid: None,
        }
    }

    /// Set the profile name condition.
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Set the platform condition (supports multiple platforms).
    pub fn platform(mut self, platforms: Vec<Platform>) -> Self {
        self.platform = Some(platforms);
        self
    }

    /// Set the proxy core type condition.
    pub fn core(mut self, core: impl Into<String>) -> Self {
        self.core = Some(core.into());
        self
    }

    /// Set time range condition (format `"HH:mm-HH:mm"`).
    pub fn time(mut self, time_range: TimeRange) -> Self {
        self.time_range = Some(time_range);
        self
    }

    /// Set the enabled condition. When false, the scope will never match.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Set the WiFi SSID condition.
    pub fn ssid(mut self, ssid: impl Into<String>) -> Self {
        self.ssid = Some(ssid.into());
        self
    }

    /// Build the final [`Scope::Scoped`] variant from accumulated conditions.
    pub fn build(self) -> Scope {
        Scope::Scoped {
            profile: self.profile,
            platform: self.platform,
            core: self.core,
            time_range: self.time_range,
            enabled: self.enabled,
            ssid: self.ssid,
        }
    }
}

/// Time range for time-based conditional scoping.
///
/// Format: `"HH:mm-HH:mm"`, e.g., `"08:00-23:00"` means active from 8:00 to 23:00.
///
/// # Cross-Midnight Handling
///
/// If end time < start time (e.g., `"23:00-06:00"`), represents a cross-midnight range:
/// from 23:00 today to 06:00 next day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    /// Start time as (hour, minute) tuple
    pub start: (u8, u8),
    /// End time as (hour, minute) tuple
    pub end: (u8, u8),
}

impl TimeRange {
    /// Parse a time range from string.
    ///
    /// Supported format: `"HH:mm-HH:mm"`
    ///
    /// # Separator
    ///
    /// The separator between start and end times is a single `-` (hyphen).
    /// Since both time parts are in `HH:mm` format (always ending with digits),
    /// there is no ambiguity with the `-` separator.
    ///
    /// # Timezone
    ///
    /// The parsed time range is **timezone-naive** — it represents local time
    /// of day only. When evaluating [`is_active_now`](Self::is_active_now), the
    /// engine uses `chrono::Local::now()` which respects the system's local
    /// timezone. There is no support for UTC offsets (e.g., `+08:00`) or
    /// named timezones (e.g., `America/New_York`) in the format string.
    ///
    /// **Future extension note**: If timezone-aware time ranges are needed,
    /// consider extending the format to `"HH:mm±HH:mm-HH:mm±HH:mm"` or
    /// accepting a separate `timezone` field in `__when__`. Any change must
    /// remain backward-compatible with the current `"HH:mm-HH:mm"` syntax.
    ///
    /// # Errors
    /// Returns error if format is invalid or time values are out of range (hour > 23, minute > 59).
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 2 {
            return Err(format!("时间格式无效「{}」，期望 \"HH:mm-HH:mm\"", s));
        }

        let start = Self::parse_time(parts[0])?;
        let end = Self::parse_time(parts[1])?;

        Ok(Self { start, end })
    }

    /// 解析 "HH:mm" 为 (u8, u8)
    fn parse_time(s: &str) -> std::result::Result<(u8, u8), String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            return Err(format!("时间部分格式无效「{}」，期望 \"HH:mm\"", s));
        }
        let hour: u8 = parts[0]
            .parse()
            .map_err(|_| format!("无效的小时值「{}」", parts[0]))?;
        let minute: u8 = parts[1]
            .parse()
            .map_err(|_| format!("无效的分钟值「{}」", parts[1]))?;

        if hour > 23 {
            return Err(format!("小时值超出范围 (0-23): {}", hour));
        }
        if minute > 59 {
            return Err(format!("分钟值超出范围 (0-59): {}", minute));
        }

        Ok((hour, minute))
    }

    /// Check if current local time falls within this time range.
    /// Uses `chrono::Local::now()` for local time. Supports cross-midnight ranges.
    pub fn is_active_now(&self) -> bool {
        let now = chrono::Local::now().time();
        self.is_active_at(now)
    }

    /// Check if a given time falls within this time range.
    /// Accepts a custom `NaiveTime` for testability and deterministic behavior.
    pub fn is_active_at(&self, time: chrono::NaiveTime) -> bool {
        let now_minutes = (time.hour() * 60 + time.minute()) as u16;
        self.contains(now_minutes)
    }

    /// Check if a given time-of-day (minutes since midnight) falls within this range.
    ///
    /// # Special Case: `00:00-00:00`
    ///
    /// When `start == end` (e.g., `"00:00-00:00"`), the range is treated as
    /// **all day** (always active). This matches the intuitive expectation that
    /// a time range with identical start and end times should cover the entire day.
    ///
    ///
    /// Uses `u16` instead of `u32` since a day has at most 1440 minutes (fits in u16).
    /// This makes the type more precise and prevents accidental misuse with large values.
    pub fn contains(&self, total_minutes: u16) -> bool {
        let start_minutes = self.start.0 as u16 * 60 + self.start.1 as u16;
        let end_minutes = self.end.0 as u16 * 60 + self.end.1 as u16;

        // start == end means "all day"
        if start_minutes == end_minutes {
            return true;
        }

        if start_minutes < end_minutes {
            total_minutes >= start_minutes && total_minutes <= end_minutes
        } else {
            total_minutes >= start_minutes || total_minutes <= end_minutes
        }
    }
}

impl std::fmt::Display for TimeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}-{:02}:{:02}",
            self.start.0, self.start.1, self.end.0, self.end.1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Scope 构造器 ───

    #[test]
    fn test_scope_global() {
        let scope = Scope::global();
        assert!(scope.is_global());
        assert_eq!(scope.priority(), 1);
    }

    #[test]
    fn test_scope_profile() {
        let scope = Scope::profile("work");
        assert!(!scope.is_global());
        assert_eq!(scope.priority(), 0);
        match scope {
            Scope::Profile(name) => assert_eq!(name, "work"),
            _ => panic!("Expected Profile variant"),
        }
    }

    #[test]
    fn test_scope_runtime() {
        let scope = Scope::runtime();
        assert!(!scope.is_global());
        assert_eq!(scope.priority(), 3);
    }

    #[test]
    fn test_scope_default_is_global() {
        let scope = Scope::default();
        assert!(scope.is_global());
        assert_eq!(scope.priority(), 1);
    }

    #[test]
    fn test_scope_scoped_builder() {
        let scope = Scope::scoped().build();
        assert!(!scope.is_global());
        assert_eq!(scope.priority(), 2);
    }

    // ─── Scope::is_global ───

    #[test]
    fn test_is_global_true_for_global() {
        assert!(Scope::global().is_global());
    }

    #[test]
    fn test_is_global_false_for_others() {
        assert!(!Scope::profile("x").is_global());
        assert!(!Scope::runtime().is_global());
        assert!(!Scope::scoped().build().is_global());
    }

    // ─── Scope::priority ───

    #[test]
    fn test_priority_values() {
        assert_eq!(Scope::global().priority(), 1);
        assert_eq!(Scope::profile("x").priority(), 0);
        assert_eq!(Scope::scoped().build().priority(), 2);
        assert_eq!(Scope::runtime().priority(), 3);
    }

    // ─── Scope Display ───

    #[test]
    fn test_scope_display_global() {
        assert_eq!(format!("{}", Scope::global()), "Global");
    }

    #[test]
    fn test_scope_display_profile() {
        assert_eq!(format!("{}", Scope::profile("work")), "Profile(work)");
    }

    #[test]
    fn test_scope_display_runtime() {
        assert_eq!(format!("{}", Scope::runtime()), "Runtime");
    }

    #[test]
    fn test_scope_display_scoped() {
        let scope = ScopedBuilder::new()
            .core("clash".to_string())
            .platform(vec![Platform::MacOS])
            .profile("home")
            .build();
        let display = format!("{}", scope);
        assert!(display.starts_with("Scoped("));
        assert!(display.contains("core=clash"));
        assert!(display.contains("profile=home"));
        assert!(display.contains("platform=[macos]"));
    }

    // ─── ScopedBuilder 完整链式调用 ───

    #[test]
    fn test_scoped_builder_full_chain() {
        let tr = TimeRange::parse("09:00-17:00").unwrap();
        let scope = ScopedBuilder::new()
            .core("clash")
            .platform(vec![Platform::MacOS, Platform::Linux])
            .time(tr)
            .profile("work")
            .enabled(true)
            .ssid("OfficeWiFi")
            .build();

        match scope {
            Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                assert_eq!(profile.as_deref(), Some("work"));
                assert_eq!(platform.as_deref().map(|v| v.len()), Some(2));
                assert!(platform.as_ref().unwrap().contains(&Platform::MacOS));
                assert!(platform.as_ref().unwrap().contains(&Platform::Linux));
                assert_eq!(core.as_deref(), Some("clash"));
                assert_eq!(time_range.as_ref().unwrap().start, (9, 0));
                assert_eq!(time_range.as_ref().unwrap().end, (17, 0));
                assert_eq!(enabled, Some(true));
                assert_eq!(ssid.as_deref(), Some("OfficeWiFi"));
            }
            _ => panic!("Expected Scoped variant"),
        }
    }

    #[test]
    fn test_scoped_builder_enabled_false() {
        let scope = ScopedBuilder::new().enabled(false).build();
        match scope {
            Scope::Scoped { enabled, .. } => assert_eq!(enabled, Some(false)),
            _ => panic!("Expected Scoped variant"),
        }
    }

    #[test]
    fn test_scoped_builder_default_all_none() {
        let scope = ScopedBuilder::new().build();
        match scope {
            Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                assert!(profile.is_none());
                assert!(platform.is_none());
                assert!(core.is_none());
                assert!(time_range.is_none());
                assert!(enabled.is_none());
                assert!(ssid.is_none());
            }
            _ => panic!("Expected Scoped variant"),
        }
    }

    // ─── TimeRange::parse ───

    #[test]
    fn test_time_range_parse_normal() {
        let tr = TimeRange::parse("09:00-17:00").unwrap();
        assert_eq!(tr.start, (9, 0));
        assert_eq!(tr.end, (17, 0));
    }

    #[test]
    fn test_time_range_parse_midnight_crossover() {
        let tr = TimeRange::parse("23:00-06:00").unwrap();
        assert_eq!(tr.start, (23, 0));
        assert_eq!(tr.end, (6, 0));
    }

    #[test]
    fn test_time_range_parse_full_day() {
        let tr = TimeRange::parse("00:00-00:00").unwrap();
        assert_eq!(tr.start, (0, 0));
        assert_eq!(tr.end, (0, 0));
    }

    #[test]
    fn test_time_range_parse_invalid_format() {
        let result = TimeRange::parse("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_time_range_parse_invalid_hours() {
        let result = TimeRange::parse("25:00-26:00");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("小时值超出范围"), "错误信息: {}", err);
    }

    #[test]
    fn test_time_range_parse_invalid_minutes() {
        let result = TimeRange::parse("10:60-12:00");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("分钟值超出范围"), "错误信息: {}", err);
    }

    #[test]
    fn test_time_range_parse_missing_dash() {
        let result = TimeRange::parse("09:00");
        assert!(result.is_err());
    }

    #[test]
    fn test_time_range_parse_non_numeric() {
        let result = TimeRange::parse("ab:cd-ef:gh");
        assert!(result.is_err());
    }

    #[test]
    fn test_time_range_parse_single_digit_hour() {
        let tr = TimeRange::parse("9:5-18:30").unwrap();
        assert_eq!(tr.start, (9, 5));
        assert_eq!(tr.end, (18, 30));
    }

    // ─── TimeRange::contains ───

    #[test]
    fn test_contains_normal_range() {
        let tr = TimeRange::parse("09:00-17:00").unwrap();
        // 09:00 = 540 min, 12:00 = 720 min, 17:00 = 1020 min
        assert!(tr.contains(540)); // 09:00 边界
        assert!(tr.contains(720)); // 12:00 中间
        assert!(tr.contains(1020)); // 17:00 边界
        assert!(!tr.contains(539)); // 08:59 不在范围内
        assert!(!tr.contains(1021)); // 17:01 不在范围内
    }

    #[test]
    fn test_contains_midnight_crossover() {
        let tr = TimeRange::parse("23:00-06:00").unwrap();
        // 23:00 = 1380 min, 00:00 = 0 min, 06:00 = 360 min
        assert!(tr.contains(1380)); // 23:00
        assert!(tr.contains(0)); // 00:00
        assert!(tr.contains(360)); // 06:00
        assert!(tr.contains(1439)); // 23:59
        assert!(!tr.contains(720)); // 12:00 不在范围内
        assert!(!tr.contains(361)); // 06:01 不在范围内
    }

    #[test]
    fn test_contains_full_day() {
        let tr = TimeRange::parse("00:00-00:00").unwrap();
        // 00:00-00:00: start == end → treated as all day
        assert!(tr.contains(0));
        assert!(tr.contains(1));
        assert!(tr.contains(720)); // 12:00
        assert!(tr.contains(1439)); // 23:59
    }

    #[test]
    fn test_contains_edge_case_2359() {
        let tr = TimeRange::parse("00:00-23:59").unwrap();
        assert!(tr.contains(0));
        assert!(tr.contains(1439)); // 23:59
        assert!(!tr.contains(1440)); // 不可能值但测试边界
    }

    // ─── TimeRange Display ───

    #[test]
    fn test_time_range_display() {
        let tr = TimeRange::parse("09:00-17:00").unwrap();
        assert_eq!(format!("{}", tr), "09:00-17:00");
    }

    #[test]
    fn test_time_range_display_midnight() {
        let tr = TimeRange::parse("23:00-06:00").unwrap();
        assert_eq!(format!("{}", tr), "23:00-06:00");
    }

    // ─── Platform Display ───

    #[test]
    fn test_platform_display() {
        assert_eq!(format!("{}", Platform::Windows), "windows");
        assert_eq!(format!("{}", Platform::MacOS), "macos");
        assert_eq!(format!("{}", Platform::Linux), "linux");
    }

    // ─── ScopedBuilder Default ───

    #[test]
    fn test_scoped_builder_default() {
        let builder = ScopedBuilder::default();
        let scope = builder.build();
        match scope {
            Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                assert!(profile.is_none());
                assert!(platform.is_none());
                assert!(core.is_none());
                assert!(time_range.is_none());
                assert!(enabled.is_none());
                assert!(ssid.is_none());
            }
            _ => panic!("Expected Scoped variant"),
        }
    }

    // ══════════════════════════════════════════════════════════
    // 边界测试 — 刁难、临界、对抗性情况
    // ══════════════════════════════════════════════════════════

    /// 1. 正常时间范围 08:00-23:00
    #[test]
    fn test_time_range_normal() {
        let tr = TimeRange::parse("08:00-23:00").unwrap();
        assert_eq!(tr.start, (8, 0));
        assert_eq!(tr.end, (23, 0));
        // 08:00 = 480, 12:00 = 720, 23:00 = 1380
        assert!(tr.contains(480), "08:00 应在范围内");
        assert!(tr.contains(720), "12:00 应在范围内");
        assert!(tr.contains(1380), "23:00 应在范围内");
        assert!(!tr.contains(479), "07:59 不应在范围内");
        assert!(!tr.contains(1381), "23:01 不应在范围内");
    }

    /// 2. 跨午夜 23:00-06:00
    #[test]
    fn test_time_range_cross_midnight() {
        let tr = TimeRange::parse("23:00-06:00").unwrap();
        assert_eq!(tr.start, (23, 0));
        assert_eq!(tr.end, (6, 0));
        // 23:00 = 1380, 00:00 = 0, 03:00 = 180, 06:00 = 360
        assert!(tr.contains(1380), "23:00 应在范围内");
        assert!(tr.contains(0), "00:00 应在范围内");
        assert!(tr.contains(180), "03:00 应在范围内");
        assert!(tr.contains(360), "06:00 应在范围内");
        assert!(tr.contains(1439), "23:59 应在范围内");
        assert!(!tr.contains(480), "08:00 不应在范围内");
        assert!(!tr.contains(361), "06:01 不应在范围内");
    }

    /// 3. 全天 00:00-23:59
    #[test]
    fn test_time_range_full_day() {
        let tr = TimeRange::parse("00:00-23:59").unwrap();
        assert_eq!(tr.start, (0, 0));
        assert_eq!(tr.end, (23, 59));
        // start < end → normal range, covers 0..1439
        assert!(tr.contains(0), "00:00 应在范围内");
        assert!(tr.contains(1439), "23:59 应在范围内");
        assert!(tr.contains(720), "12:00 应在范围内");
    }

    /// 4. 单分钟 12:30-12:30
    #[test]
    fn test_time_range_single_minute() {
        let tr = TimeRange::parse("12:30-12:30").unwrap();
        assert_eq!(tr.start, (12, 30));
        assert_eq!(tr.end, (12, 30));
        // start == end → treated as all day (per spec)
        assert!(tr.contains(0), "00:00 应匹配（start==end 视为全天）");
        assert!(tr.contains(750), "12:30 应匹配");
        assert!(tr.contains(1439), "23:59 应匹配");
    }

    /// 5. 反转范围 23:00-08:00（等同于跨午夜）
    #[test]
    fn test_time_range_inverted() {
        let tr = TimeRange::parse("23:00-08:00").unwrap();
        assert_eq!(tr.start, (23, 0));
        assert_eq!(tr.end, (8, 0));
        // start > end → cross-midnight
        assert!(tr.contains(1380), "23:00 应在范围内");
        assert!(tr.contains(0), "00:00 应在范围内");
        assert!(tr.contains(480), "08:00 应在范围内");
        assert!(tr.contains(1439), "23:59 应在范围内");
        assert!(!tr.contains(540), "09:00 不应在范围内");
        assert!(!tr.contains(481), "08:01 不应在范围内");
    }

    /// 6. 精确平台匹配
    #[test]
    fn test_platform_match_exact() {
        let scope = Scope::scoped().platform(vec![Platform::MacOS]).build();
        match scope {
            Scope::Scoped { platform, .. } => {
                let platforms = platform.unwrap();
                assert_eq!(platforms.len(), 1);
                assert_eq!(platforms[0], Platform::MacOS);
            }
            _ => panic!("Expected Scoped variant"),
        }
    }

    /// 7. 多平台匹配
    #[test]
    fn test_platform_match_multiple() {
        let scope = Scope::scoped()
            .platform(vec![Platform::Windows, Platform::Linux, Platform::MacOS])
            .build();
        match scope {
            Scope::Scoped { platform, .. } => {
                let platforms = platform.unwrap();
                assert_eq!(
                    platforms,
                    &[Platform::Windows, Platform::Linux, Platform::MacOS]
                );
            }
            _ => panic!("Expected Scoped variant"),
        }
    }

    /// 8. 默认作用域是 Global
    #[test]
    fn test_scope_default_is_global_boundary() {
        let scope = Scope::default();
        assert!(scope.is_global(), "Default scope 应为 Global");
        assert_eq!(scope.priority(), 1, "Global 优先级应为 1");
        // 验证 Display 输出
        assert_eq!(format!("{}", scope), "Global");
    }

    /// 9. Scoped builder 链式调用
    #[test]
    fn test_scoped_builder_chaining() {
        let tr = TimeRange::parse("08:00-23:00").unwrap();
        let scope = Scope::scoped()
            .core("mihomo")
            .platform(vec![Platform::MacOS, Platform::Linux])
            .time(tr.clone())
            .profile("work")
            .enabled(true)
            .ssid("OfficeWiFi")
            .build();

        // 验证所有字段通过链式调用正确设置
        match &scope {
            Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                assert_eq!(profile.as_deref(), Some("work"));
                assert_eq!(platform.as_ref().map(|v| v.len()), Some(2));
                assert_eq!(core.as_deref(), Some("mihomo"));
                assert_eq!(time_range.as_ref().unwrap().start, (8, 0));
                assert_eq!(time_range.as_ref().unwrap().end, (23, 0));
                assert_eq!(*enabled, Some(true));
                assert_eq!(ssid.as_deref(), Some("OfficeWiFi"));
            }
            _ => panic!("Expected Scoped variant"),
        }

        // 验证 Display 输出包含关键信息
        let display = format!("{}", scope);
        assert!(display.starts_with("Scoped("));
        assert!(display.contains("core=mihomo"));
        assert!(display.contains("work"));
    }
}
