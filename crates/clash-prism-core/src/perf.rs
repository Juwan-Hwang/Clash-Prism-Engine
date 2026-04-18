//! # 性能追踪器
//!
//! 参考 Claude Code withDiagnosticsTiming 的高阶函数模式：
//! 自动记录各阶段耗时，生成人类可读的性能报告。
//!
//! 用法：
//! ```ignore
//! let mut tracker = PerfTracker::new();
//! let result = tracker.measure("解析 DSL", || parse_file("config.prism.yaml"));
//! let result = tracker.measure("编译 IR", || compile(&ast));
//! let result = tracker.measure("执行 Patch", || execute(&mut config, &patches));
//! println!("{}", tracker.report());
//! ```

use std::fmt;
use std::time::{Duration, Instant};

/// 单个阶段的性能指标
#[derive(Debug, Clone)]
pub struct PhaseMetric {
    /// 阶段名称
    pub name: String,
    /// 耗时（微秒）
    pub duration_us: u64,
}

/// 性能追踪器 — 记录各阶段耗时
///
/// 支持两种记录方式：
/// - `record()` — 手动记录已知的 Duration
/// - `measure()` — 执行闭包并自动记录耗时（推荐）
pub struct PerfTracker {
    phases: Vec<PhaseMetric>,
}

impl PerfTracker {
    /// 创建新的性能追踪器
    pub fn new() -> Self {
        Self { phases: Vec::new() }
    }

    /// 记录一个阶段的耗时
    pub fn record(&mut self, name: &str, duration: Duration) {
        self.phases.push(PhaseMetric {
            name: name.to_string(),
            duration_us: duration.as_micros().min(u64::MAX as u128) as u64,
        });
    }

    /// 执行一个闭包并自动记录耗时
    ///
    /// 如果闭包 panic，panic 会直接传播，耗时不会被记录。
    /// 如需 panic-safe 版本，请使用 `std::panic::catch_unwind` 手动包装。
    pub fn measure<F, R>(&mut self, name: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let start = Instant::now();
        let result = f();
        let elapsed = start.elapsed();
        self.phases.push(PhaseMetric {
            name: name.to_string(),
            duration_us: elapsed.as_micros().min(u64::MAX as u128) as u64,
        });
        result
    }

    /// 执行一个可能失败的闭包并自动记录耗时
    ///
    /// 与 `measure` 不同，此方法接受返回 `Result` 的闭包。
    /// 无论闭包返回 `Ok` 还是 `Err`，耗时都会被记录。
    /// 这对于测量可能失败的操作（如文件 I/O、网络请求）特别有用。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// let mut tracker = PerfTracker::new();
    /// let result = tracker.try_measure("读取配置", || std::fs::read_to_string("config.yaml"));
    /// ```
    pub fn try_measure<F, T, E>(&mut self, name: &str, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let start = Instant::now();
        let result = f();
        let elapsed = start.elapsed();
        self.phases.push(PhaseMetric {
            name: name.to_string(),
            duration_us: elapsed.as_micros().min(u64::MAX as u128) as u64,
        });
        result
    }

    /// 获取所有阶段指标
    pub fn phases(&self) -> &[PhaseMetric] {
        &self.phases
    }

    /// 获取总耗时
    pub fn total_duration(&self) -> Duration {
        let total_us: u64 = self.phases.iter().map(|p| p.duration_us).sum();
        Duration::from_micros(total_us)
    }

    /// 获取阶段数量
    pub fn phase_count(&self) -> usize {
        self.phases.len()
    }

    /// 生成人类可读的性能报告
    ///
    /// 输出格式示例：
    /// ```text
    /// 性能报告:
    ///   解析 DSL:     1,234 us
    ///   编译 IR:       567 us
    ///   执行 Patch:   2,345 us
    ///   ─────────────────────
    ///   总计:        4,146 us
    /// ```
    pub fn report(&self) -> String {
        if self.phases.is_empty() {
            return "性能报告: 无记录阶段".to_string();
        }

        let mut lines = Vec::with_capacity(self.phases.len() + 4);
        lines.push("性能报告:".to_string());

        // 计算名称最大宽度，用于对齐
        let max_name_len = self
            .phases
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(0);

        for phase in &self.phases {
            let name_padding = max_name_len - phase.name.chars().count();
            let padding = " ".repeat(name_padding);
            lines.push(format!(
                "  {}{}: {:>10} us",
                padding,
                phase.name,
                format_us(phase.duration_us)
            ));
        }

        // 分隔线
        let separator_len = max_name_len + 20;
        lines.push(format!("  {}", "-".repeat(separator_len)));

        // 总计
        let total_us: u64 = self.phases.iter().map(|p| p.duration_us).sum();
        let total_padding = " ".repeat(max_name_len);
        lines.push(format!(
            "  {}总计: {:>10} us",
            total_padding,
            format_us(total_us)
        ));

        lines.join("\n")
    }
}

/// 格式化微秒数为带千分位的字符串
///
/// # Safety
///
/// This function operates directly on the byte representation of the decimal string.
/// Since `u64::to_string()` always produces ASCII digits (0x30-0x39), casting `u8` to `char`
/// via `b as char` is safe — every byte is a valid single-byte UTF-8 character.
/// No multi-byte UTF-8 sequences or non-ASCII bytes can appear in the input.
fn format_us(us: u64) -> String {
    let s = us.to_string();
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

impl Default for PerfTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PerfTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_perf_tracker_new() {
        let tracker = PerfTracker::new();
        assert_eq!(tracker.phase_count(), 0);
        assert!(tracker.phases().is_empty());
    }

    #[test]
    fn test_perf_tracker_measure() {
        let mut tracker = PerfTracker::new();

        let result = tracker.measure("测试阶段", || {
            // 模拟一些工作
            std::thread::sleep(std::time::Duration::from_millis(10));
            42
        });

        assert_eq!(result, 42);
        assert_eq!(tracker.phase_count(), 1);
        assert_eq!(tracker.phases()[0].name, "测试阶段");
        // 至少 10000 us (10ms)
        assert!(tracker.phases()[0].duration_us >= 10_000);
    }

    #[test]
    fn test_perf_tracker_measure_multiple() {
        let mut tracker = PerfTracker::new();

        tracker.measure("阶段1", || {});
        tracker.measure("阶段2", || {});
        tracker.measure("阶段3", || {});

        assert_eq!(tracker.phase_count(), 3);
    }

    #[test]
    fn test_perf_tracker_record() {
        let mut tracker = PerfTracker::new();
        tracker.record("手动记录", Duration::from_micros(1234));

        assert_eq!(tracker.phase_count(), 1);
        assert_eq!(tracker.phases()[0].duration_us, 1234);
    }

    #[test]
    fn test_perf_tracker_total_duration() {
        let mut tracker = PerfTracker::new();
        tracker.record("a", Duration::from_micros(100));
        tracker.record("b", Duration::from_micros(200));
        tracker.record("c", Duration::from_micros(300));

        assert_eq!(tracker.total_duration(), Duration::from_micros(600));
    }

    #[test]
    fn test_perf_tracker_report() {
        let mut tracker = PerfTracker::new();
        tracker.record("解析", Duration::from_micros(1234));
        tracker.record("编译", Duration::from_micros(567));

        let report = tracker.report();
        assert!(report.contains("性能报告:"));
        assert!(report.contains("解析"));
        assert!(report.contains("编译"));
        assert!(report.contains("总计"));
    }

    #[test]
    fn test_perf_tracker_report_empty() {
        let tracker = PerfTracker::new();
        let report = tracker.report();
        assert_eq!(report, "性能报告: 无记录阶段");
    }

    #[test]
    fn test_perf_tracker_display() {
        let mut tracker = PerfTracker::new();
        tracker.record("测试", Duration::from_micros(100));
        let display = format!("{}", tracker);
        assert!(display.contains("性能报告:"));
    }

    #[test]
    fn test_perf_tracker_default() {
        let tracker = PerfTracker::default();
        assert_eq!(tracker.phase_count(), 0);
    }

    #[test]
    fn test_format_us() {
        assert_eq!(format_us(0), "0");
        assert_eq!(format_us(1), "1");
        assert_eq!(format_us(999), "999");
        assert_eq!(format_us(1000), "1,000");
        assert_eq!(format_us(12345), "12,345");
        assert_eq!(format_us(1234567), "1,234,567");
    }
}
