//! 节点历史数据 — 用于评分计算的输入

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `add_record` 的返回结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddRecordResult {
    /// 记录正常添加
    Accepted,
    /// 记录被标记为无效（NaN 或负值延迟）
    InvalidLatency,
}

/// 单个节点的历史测速记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHistory {
    /// 节点名称
    pub name: String,

    /// 历史延迟记录（毫秒）
    ///
    /// 使用 `VecDeque` 实现 O(1) 头部裁剪，当记录数超过 [`max_records`](Self::max_records)
    /// 时自动丢弃最旧的记录，防止内存无限膨胀。
    #[serde(deserialize_with = "deserialize_vec_to_vecdeque")]
    pub latency_records: VecDeque<LatencyRecord>,

    /// 成功率（0.0 ~ 1.0）
    pub success_rate: f64,

    /// P90 延迟（毫秒）
    pub p90_latency: f64,

    /// 延迟标准差
    pub latency_stddev: f64,

    /// 最后一次测速时间
    pub last_test: DateTime<Utc>,

    /// 自动 trim 上限：超过此数量时自动裁剪旧记录（默认 1000）
    #[serde(default = "default_max_records")]
    pub max_records: usize,
}

fn default_max_records() -> usize {
    1000
}

impl NodeHistory {
    /// 创建新的空历史记录
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            latency_records: VecDeque::new(),
            // 选择 0.5 的理由：
            // - 避免未测试节点获得过高的评分（1.0 会导致新节点排名靠前）
            // - 避免未测试节点被完全忽略（0.0 会导致新节点永远无法被选中）
            // - 0.5 表示"未知"，与贝叶斯先验的中性估计一致
            success_rate: 0.5,
            p90_latency: 0.0,
            latency_stddev: 0.0,
            last_test: Utc::now(),
            max_records: default_max_records(),
        }
    }

    /// 添加一条延迟记录并重新计算统计量
    ///
    /// 当记录数超过 max限制时自动裁剪旧记录，防止内存无限膨胀。
    ///
    /// 返回 [`AddRecordResult`] 指示记录是否被正常添加，
    /// 若延迟为 NaN 或负值则返回 `InvalidLatency`。
    pub fn add_record(&mut self, latency_ms: f64, success: bool) -> AddRecordResult {
        // NaN 或负值记录为 f64::NAN（标记为无效），在统计计算中过滤
        let is_invalid = latency_ms.is_nan() || latency_ms < 0.0;
        let latency_ms = if is_invalid { f64::NAN } else { latency_ms };
        let success = if is_invalid { false } else { success };

        let now = Utc::now();
        self.latency_records.push_back(LatencyRecord {
            timestamp: now,
            latency_ms,
            success,
        });
        self.last_test = now;

        // 自动 trim：超过上限时裁剪旧记录
        if self.latency_records.len() > self.max_records {
            self.trim(self.max_records);
        } else {
            self.recalculate();
        }

        if is_invalid {
            AddRecordResult::InvalidLatency
        } else {
            AddRecordResult::Accepted
        }
    }

    /// 批量添加延迟记录，最后统一重算一次统计量
    ///
    /// 相比逐条调用 [`add_record`](Self::add_record)，此方法在添加所有记录后
    /// 仅执行一次 [`recalculate`](Self::recalculate)，避免 O(N*M) 的重复计算。
    ///
    /// 当记录数超过 `max_records` 时，自动裁剪旧记录。
    ///
    /// 批量记录使用**相同的时间戳**（`Utc::now()` 在方法入口获取一次）。
    /// 这是有意为之的设计选择：
    /// - 批量记录通常来自同一次批量测速操作，时间戳一致更符合语义
    /// - 避免批量添加时时间漂移导致评分偏差
    /// - `last_test` 字段统一更新为批量操作的时间
    ///
    /// # Returns
    ///
    /// `(accepted, invalid)` — 有效记录数和无效记录数（NaN 或负值延迟）
    pub fn add_records(&mut self, records: &[(f64, bool)]) -> (usize, usize) {
        let now = Utc::now();
        let mut invalid_count = 0usize;
        for &(latency_ms, success) in records {
            let is_invalid = latency_ms.is_nan() || latency_ms < 0.0;
            if is_invalid {
                invalid_count += 1;
            }
            let latency_ms = if is_invalid { f64::NAN } else { latency_ms };
            let success = if is_invalid { false } else { success };
            self.latency_records.push_back(LatencyRecord {
                timestamp: now,
                latency_ms,
                success,
            });
        }
        self.last_test = now;

        // 自动 trim：超过上限时裁剪旧记录
        if self.latency_records.len() > self.max_records {
            self.trim(self.max_records);
        } else {
            self.recalculate();
        }

        (records.len() - invalid_count, invalid_count)
    }

    /// 重新计算 P90、标准差和成功率
    fn recalculate(&mut self) {
        if self.latency_records.is_empty() {
            return;
        }

        // 成功率计算
        let total = self.latency_records.len();
        let successes = self.latency_records.iter().filter(|r| r.success).count();

        // 直接使用除法即可，无需 checked_div(1)
        self.success_rate = successes as f64 / total as f64;

        // 只用成功的记录计算延迟统计，同时过滤 NaN 值
        let successful_latencies: Vec<f64> = self
            .latency_records
            .iter()
            .filter(|r| r.success)
            .map(|r| r.latency_ms)
            .filter(|l| !l.is_nan())
            .collect();

        if !successful_latencies.is_empty() {
            // P90 延迟 — 线性插值法（对小样本更准确）
            let mut sorted = successful_latencies;
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = sorted.len();
            if n == 1 {
                self.p90_latency = sorted[0];
            } else {
                let p90_index = (n - 1) as f64 * 0.9;
                let floor_idx = p90_index.floor() as usize;
                let ceil_idx = p90_index.ceil() as usize;
                let fraction = p90_index - floor_idx as f64;
                // 当 p90_index 恰好等于 n-1（如 n=1 时 p90_index=0.0，ceil=0），
                // 或浮点精度导致 ceil_idx 超出有效范围时，.min(n-1) 确保安全访问。
                // 此时 fraction=0，插值退化为直接取 sorted[floor_idx] 的值。
                self.p90_latency =
                    sorted[floor_idx] * (1.0 - fraction) + sorted[ceil_idx.min(n - 1)] * fraction;
            }

            // 标准差 — 样本标准差（Bessel 校正，分母 N-1）
            let n = sorted.len();
            if n == 1 {
                self.latency_stddev = 0.0;
            } else {
                let mean = sorted.iter().copied().sum::<f64>() / n as f64;
                let variance = sorted.iter().copied().fold(0.0f64, |acc, l| {
                    let diff = l - mean;
                    acc + diff * diff
                }) / (n - 1) as f64;
                self.latency_stddev = variance.sqrt();
            }
        } else {
            // 无成功记录时重置统计量，防止评分器基于过时数据给出偏高评分。
            // score_at() 已正确处理 NaN 情况（latency_score 和 stability_score 均返回 0.0）。
            self.p90_latency = f64::NAN;
            self.latency_stddev = f64::NAN;
        }
    }

    /// 清理过期记录（保留最近 N 条或指定时间范围内的）
    ///
    /// 使用 `VecDeque::drain(..)` 实现 O(1) 头部裁剪，比 `Vec::split_off` 更高效。
    pub fn trim(&mut self, max_records: usize) {
        if self.latency_records.len() > max_records {
            let excess = self.latency_records.len().saturating_sub(max_records);
            self.latency_records.drain(..excess);
            self.recalculate();
        }
    }
}

/// 反序列化辅助：将 JSON 数组（`Vec`）转换为 `VecDeque`
///
/// JSON 不区分 `Vec` 和 `VecDeque`，serde 默认将数组反序列化为 `Vec`。
/// 此函数接受 `Vec` 并转换为 `VecDeque`，确保从 JSON 加载的数据正确还原。
fn deserialize_vec_to_vecdeque<'de, D>(deserializer: D) -> Result<VecDeque<LatencyRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Vec<LatencyRecord> = Vec::deserialize(deserializer)?;
    Ok(v.into_iter().collect())
}

/// 单次测速记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyRecord {
    /// 记录时间
    pub timestamp: DateTime<Utc>,

    /// 延迟（毫秒）
    pub latency_ms: f64,

    /// 是否成功
    pub success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_node_has_defaults() {
        let node = NodeHistory::new("test-node");
        assert_eq!(node.name, "test-node");
        assert!(node.latency_records.is_empty());
        assert_eq!(node.success_rate, 0.5); // neutral default (no data yet)
        assert_eq!(node.p90_latency, 0.0);
        assert_eq!(node.latency_stddev, 0.0);
        assert_eq!(node.max_records, 1000);
    }

    #[test]
    fn add_record_successful() {
        let mut node = NodeHistory::new("test");
        node.add_record(50.0, true);
        assert_eq!(node.latency_records.len(), 1);
        assert_eq!(node.success_rate, 1.0);
        assert_eq!(node.p90_latency, 50.0);
        assert_eq!(node.latency_stddev, 0.0); // single record → stddev = 0
    }

    #[test]
    fn add_record_failed() {
        let mut node = NodeHistory::new("test");
        node.add_record(100.0, false);
        assert_eq!(node.latency_records.len(), 1);
        assert_eq!(node.success_rate, 0.0);
        // Failed records are excluded from latency stats → reset to NaN
        assert!(node.p90_latency.is_nan());
        assert!(node.latency_stddev.is_nan());
    }

    #[test]
    fn add_record_nan_latency() {
        let mut node = NodeHistory::new("test");
        node.add_record(f64::NAN, true);
        assert_eq!(node.latency_records.len(), 1);
        // NaN latency → success forced to false → success_rate = 0.0
        assert_eq!(node.success_rate, 0.0);
        // No valid latencies → stats reset to NaN
        assert!(node.p90_latency.is_nan());
    }

    #[test]
    fn add_record_negative_latency() {
        let mut node = NodeHistory::new("test");
        node.add_record(-10.0, true);
        assert_eq!(node.latency_records.len(), 1);
        // Negative latency is converted to NaN and filtered
        assert!(node.latency_records[0].latency_ms.is_nan());
        // Negative latency → success forced to false → success_rate = 0.0
        assert_eq!(node.success_rate, 0.0);
        // No valid latencies → stats reset to NaN
        assert!(node.p90_latency.is_nan());
    }

    #[test]
    fn recalculate_empty_records() {
        let node = NodeHistory::new("test");
        // Don't add any records — manually trigger recalculate
        // recalculate() is private, but we can verify the initial state
        // which represents the "empty" case
        assert_eq!(node.success_rate, 0.5); // neutral default (no data yet)
        assert_eq!(node.p90_latency, 0.0);
        assert_eq!(node.latency_stddev, 0.0);
    }

    #[test]
    fn single_record_p90_equals_value() {
        let mut node = NodeHistory::new("test");
        node.add_record(123.45, true);
        assert_eq!(node.p90_latency, 123.45);
        assert_eq!(node.latency_stddev, 0.0);
    }

    #[test]
    fn multiple_records_p90_interpolation() {
        let mut node = NodeHistory::new("test");
        // Add 10 records: [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
        for i in 1..=10 {
            node.add_record(i as f64 * 10.0, true);
        }

        // P90 index = (10-1) * 0.9 = 8.1
        // floor=8 → sorted[8]=90, ceil=9 → sorted[9]=100
        // P90 = 90 * (1 - 0.1) + 100 * 0.1 = 81 + 10 = 91
        assert!(
            (node.p90_latency - 91.0).abs() < 0.01,
            "Expected P90=91.0, got {}",
            node.p90_latency
        );
    }

    #[test]
    fn trim_triggers_when_exceeding_max() {
        let mut node = NodeHistory::new("test");
        node.max_records = 5;

        for i in 1..=10 {
            node.add_record(i as f64 * 10.0, true);
        }

        // Should have been trimmed to 5
        assert_eq!(node.latency_records.len(), 5);
        // Records should be the last 5: [60, 70, 80, 90, 100]
        assert_eq!(node.latency_records[0].latency_ms, 60.0);
        assert_eq!(node.latency_records[4].latency_ms, 100.0);
    }

    #[test]
    fn trim_does_nothing_when_under_limit() {
        let mut node = NodeHistory::new("test");
        node.max_records = 100;

        for i in 1..=10 {
            node.add_record(i as f64, true);
        }

        assert_eq!(node.latency_records.len(), 10);
    }

    #[test]
    fn success_rate_all_success() {
        let mut node = NodeHistory::new("test");
        for _ in 0..5 {
            node.add_record(50.0, true);
        }
        assert_eq!(node.success_rate, 1.0);
    }

    #[test]
    fn success_rate_all_fail() {
        let mut node = NodeHistory::new("test");
        for _ in 0..5 {
            node.add_record(50.0, false);
        }
        assert_eq!(node.success_rate, 0.0);
    }

    #[test]
    fn success_rate_mixed() {
        let mut node = NodeHistory::new("test");
        // 3 successes, 2 failures
        node.add_record(50.0, true);
        node.add_record(50.0, true);
        node.add_record(50.0, false);
        node.add_record(50.0, true);
        node.add_record(50.0, false);

        assert!(
            (node.success_rate - 0.6).abs() < 1e-10,
            "Expected 0.6, got {}",
            node.success_rate
        );
    }

    #[test]
    fn stddev_with_two_records() {
        let mut node = NodeHistory::new("test");
        node.add_record(100.0, true);
        node.add_record(200.0, true);

        // Mean = 150, variance = ((100-150)^2 + (200-150)^2) / (2-1) = 5000
        // stddev = sqrt(5000) ≈ 70.71
        let expected = 5000_f64.sqrt();
        assert!(
            (node.latency_stddev - expected).abs() < 0.01,
            "Expected stddev≈{:.2}, got {:.2}",
            expected,
            node.latency_stddev
        );
    }

    #[test]
    fn p90_with_three_records() {
        let mut node = NodeHistory::new("test");
        node.add_record(10.0, true);
        node.add_record(20.0, true);
        node.add_record(30.0, true);

        // P90 index = (3-1) * 0.9 = 1.8
        // floor=1 → sorted[1]=20, ceil=2 → sorted[2]=30
        // P90 = 20 * 0.2 + 30 * 0.8 = 4 + 24 = 28
        assert!(
            (node.p90_latency - 28.0).abs() < 0.01,
            "Expected P90=28.0, got {}",
            node.p90_latency
        );
    }

    #[test]
    fn failed_records_excluded_from_latency_stats() {
        let mut node = NodeHistory::new("test");
        node.add_record(1000.0, false); // huge latency but failed
        node.add_record(10.0, true);
        node.add_record(20.0, true);

        // Only successful records: [10, 20]
        // P90 index = (2-1) * 0.9 = 0.9
        // floor=0 → 10, ceil=1 → 20
        // P90 = 10 * 0.1 + 20 * 0.9 = 1 + 18 = 19
        assert!(
            (node.p90_latency - 19.0).abs() < 0.01,
            "Expected P90=19.0, got {}",
            node.p90_latency
        );
    }

    #[test]
    fn last_test_updates_on_add() {
        let mut node = NodeHistory::new("test");
        let before = node.last_test;
        // Small sleep to ensure timestamp difference
        std::thread::sleep(std::time::Duration::from_millis(2));
        node.add_record(50.0, true);
        assert!(node.last_test > before);
    }

    #[test]
    fn trim_manual_call() {
        let mut node = NodeHistory::new("test");
        for i in 1..=20 {
            node.add_record(i as f64, true);
        }
        assert_eq!(node.latency_records.len(), 20);

        node.trim(5);
        assert_eq!(node.latency_records.len(), 5);
        // Should keep the last 5
        assert_eq!(node.latency_records[0].latency_ms, 16.0);
        assert_eq!(node.latency_records[4].latency_ms, 20.0);
    }
}
