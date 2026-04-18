//! NodeFailPolicy — 节点连续失败自动切换策略
//!
//! ## 设计决策（§7.2）
//!
//! 由 **Rust 原生实现**，不暴露给 JS 插件。
//!
//! 为什么不走 JS 回调？
//!   - 连接建立/断开是**毫秒级高频事件**
//!   - JS 回调会有 ~1ms 的额外延迟
//!   - 在高并发连接场景下会显著影响性能
//!   - 而且节点切换逻辑很简单，不需要脚本灵活性
//!
//! ## 使用场景
//!
//! 当代理组中的某个节点连续失败达到阈值时，
//! 自动切换到下一个可用节点或指定的 fallback 组。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 节点失败自动切换策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeFailPolicy {
    /// 连续失败多少次后触发切换（默认 3）
    #[serde(default = "default_threshold")]
    pub threshold: u32,

    /// 切换到哪个目标
    /// - `"next"`: 当前组的下一个节点（默认）
    /// - 具体组名: 切换到指定代理组
    #[serde(default = "default_fallback")]
    pub fallback_group: String,

    /// 冷却时间（避免频繁切换），默认 30 秒
    #[serde(
        default = "default_cooldown",
        serialize_with = "serialize_duration_as_secs",
        deserialize_with = "deserialize_duration_from_secs"
    )]
    pub cooldown: Duration,

    /// 是否启用（默认 true）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_threshold() -> u32 {
    3
}
fn default_fallback() -> String {
    "next".to_string()
}
fn default_cooldown() -> Duration {
    Duration::from_secs(30)
}
fn default_enabled() -> bool {
    true
}

/// 自定义序列化：Duration → 秒数（f64，保留亚秒精度）
fn serialize_duration_as_secs<S>(d: &Duration, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_f64(d.as_secs_f64())
}

/// Parse a human-readable duration string (e.g., "30s", "1m", "1h30m", "1h30m15s").
///
/// Supported units: `h` (hours), `m` (minutes), `s` (seconds).
/// Components can be combined in any order.
fn parse_human_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let mut total_secs_f64: f64 = 0.0;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Parse numeric part (ASCII digits, optionally with decimal point)
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == num_start {
            return None; // Expected digit
        }
        let num: f64 = std::str::from_utf8(&bytes[num_start..i])
            .ok()?
            .parse()
            .ok()?;
        if num < 0.0 {
            return None;
        }

        // Parse unit
        if i >= bytes.len() {
            return None; // Expected unit after number
        }
        let unit = bytes[i];
        i += 1;
        match unit {
            b'h' => total_secs_f64 += num * 3600.0,
            b'm' => total_secs_f64 += num * 60.0,
            b's' => total_secs_f64 += num,
            _ => return None,
        }
    }

    if total_secs_f64 <= 0.0 {
        None
    } else {
        Some(Duration::from_secs_f64(total_secs_f64))
    }
}

fn deserialize_duration_from_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct DurationVisitor;

    impl de::Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an integer (seconds) or a string duration (e.g., \"30s\")")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Duration::from_secs(v))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v < 0 {
                return Err(de::Error::custom("duration cannot be negative"));
            }
            Ok(Duration::from_secs(v as u64))
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v < 0.0 {
                return Err(de::Error::custom("duration cannot be negative"));
            }
            Ok(Duration::from_secs_f64(v))
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            // 支持纯数字字符串（整数或小数）
            if let Ok(secs) = v.parse::<f64>() {
                if secs < 0.0 {
                    return Err(de::Error::custom("duration cannot be negative"));
                }
                return Ok(Duration::from_secs_f64(secs));
            }
            // 支持人类可读格式: "30s", "1m", "1h30m", "1h30m15s", "1.5s"
            if let Some(dur) = parse_human_duration(v) {
                return Ok(dur);
            }
            Err(de::Error::custom(format!(
                "invalid duration format: '{}', expected number (seconds) or human-readable (e.g., \"30s\", \"1m30s\", \"1.5s\")",
                v
            )))
        }
    }

    deserializer.deserialize_any(DurationVisitor)
}

impl Default for NodeFailPolicy {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            fallback_group: default_fallback(),
            cooldown: default_cooldown(),
            enabled: default_enabled(),
        }
    }
}

impl NodeFailPolicy {
    /// 创建使用默认值的策略
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置失败阈值
    pub fn with_threshold(mut self, threshold: u32) -> Self {
        self.threshold = threshold;
        self
    }

    /// 设置冷却时间
    pub fn with_cooldown(mut self, secs: u64) -> Self {
        self.cooldown = Duration::from_secs(secs);
        self
    }

    /// 设置 fallback 目标
    pub fn with_fallback(mut self, group: impl Into<String>) -> Self {
        self.fallback_group = group.into();
        self
    }
}

/// 节点失败状态跟踪器
///
/// 每个代理组一个实例，跟踪组内各节点的连续失败次数和冷却状态。
/// 当某节点连续失败次数达到阈值时，自动触发节点切换（FailoverAction）。
///
/// # Thread Safety
///
/// `FailoverTracker` 是 `Send + Sync` 的，可以安全地在多线程环境中使用。
/// 如果需要在多线程中共享同一个实例，请使用 `Mutex<FailoverTracker>` 包装。
///
/// # Memory Usage
///
/// 内部使用 HashMap 存储失败计数，当节点数超过 100,000 时会触发 LRU 淘汰
/// （移除 10% 最旧的条目）。在典型使用场景中（<1000 个节点），内存占用可忽略。
#[derive(Debug)]
pub struct FailoverTracker {
    /// 策略配置
    policy: NodeFailPolicy,
    /// 各节点的连续失败计数
    failure_counts: HashMap<String, u32>,
    /// 上次切换的时间（per-node 冷却）
    last_switch: HashMap<String, Instant>,
    /// VecDeque 维护节点访问/插入时间顺序，用于 O(1) LRU 淘汰
    access_order: VecDeque<String>,
    /// 组级冷却时间戳（任何节点触发切换后更新，防止组内频繁切换）
    group_last_switch: Option<Instant>,
}

impl FailoverTracker {
    /// 创建新的跟踪器
    pub fn new(policy: NodeFailPolicy) -> Self {
        Self {
            policy,
            failure_counts: HashMap::new(),
            last_switch: HashMap::new(),
            access_order: VecDeque::new(),
            group_last_switch: None,
        }
    }

    /// 报告一次连接结果
    ///
    /// # Arguments
    /// * `node_name` — 节点名称
    /// * `success` — 连接是否成功
    ///
    /// # Returns
    /// 如果触发了节点切换，返回 `Some(FailoverAction)` 描述切换动作；
    /// 否则返回 `None`
    pub fn report(&mut self, node_name: &str, success: bool) -> Option<FailoverAction> {
        if !self.policy.enabled {
            return None;
        }

        let now = Instant::now();

        if success {
            // 成功：重置该节点的失败计数，并更新 access_order（LRU 位置提升）
            self.failure_counts.insert(node_name.to_string(), 0);
            // Update access_order: move to back (most recently accessed).
            // This ensures that successfully accessed nodes are considered
            // "more recent" for LRU eviction, preventing premature eviction
            // of nodes that are actively working.
            if self.access_order.contains(&node_name.to_string()) {
                self.access_order.retain(|k| k != node_name);
            }
            self.access_order.push_back(node_name.to_string());
            None
        } else {
            // Record insertion time for nodes not yet tracked (LRU fallback)
            if !self.failure_counts.contains_key(node_name) {
                self.access_order.push_back(node_name.to_string());
            }

            // 失败：递增计数
            // LRU-style eviction using VecDeque for O(1) access-order tracking.
            // When exceeding threshold, remove oldest entries from the front of the deque.
            //
            // Performance note: The eviction scan is O(n) where n = failure_counts.len().
            // This is acceptable because:
            // 1. Eviction only triggers when count > 100,000 (very rare in practice)
            // 2. The scan is a simple linear filter, not nested
            // 3. VecDeque::retain is O(n) but cache-friendly
            //
            // Future optimization: If eviction becomes a bottleneck, consider using
            // an LRU cache crate (e.g., lru::LruCache) which provides O(1) eviction,
            // or a min-heap indexed by failure count for O(log n) threshold eviction.
            if self.failure_counts.len() > 100000 {
                let evict_count = self.failure_counts.len() / 10; // Remove 10% oldest

                // Strategy 1: Evict zero-count entries (already recovered nodes)
                let mut to_evict: Vec<String> = self
                    .failure_counts
                    .iter()
                    .filter_map(|(k, &v)| if v == 0 { Some(k.clone()) } else { None })
                    .take(evict_count)
                    .collect();

                // Strategy 2: If no zero-count entries, evict by oldest access order.
                if to_evict.is_empty() {
                    to_evict = self
                        .access_order
                        .iter()
                        .take(evict_count)
                        .cloned()
                        .collect();
                }

                for key in &to_evict {
                    self.failure_counts.remove(key);
                    self.last_switch.remove(key);
                }
                // Clean up evicted keys from access_order
                let evict_set: std::collections::HashSet<&str> =
                    to_evict.iter().map(|s| s.as_str()).collect();
                self.access_order
                    .retain(|k| !evict_set.contains(k.as_str()));
                tracing::debug!(
                    evicted = to_evict.len(),
                    remaining = self.failure_counts.len(),
                    "LRU eviction: removed stale entries from failure_counts"
                );
            }

            let count = {
                let entry = self
                    .failure_counts
                    .entry(node_name.to_string())
                    .or_insert(0);
                *entry += 1;
                *entry
            };

            // 检查是否达到阈值
            if count >= self.policy.threshold {
                // 检查组级冷却时间（优先于 per-node 冷却）
                if let Some(group_switch_time) = self.group_last_switch
                    && now.duration_since(group_switch_time) < self.policy.cooldown
                {
                    // 组级冷却期内，不触发切换
                    return None;
                }

                // 检查 per-node 冷却时间
                if let Some(switch_time) = self.last_switch.get(node_name)
                    && now.duration_since(*switch_time) < self.policy.cooldown
                {
                    // 冷却期内，不触发切换
                    return None;
                }

                // 触发切换 — 同时更新组级和节点级冷却
                self.last_switch.insert(node_name.to_string(), now);
                self.group_last_switch = Some(now);
                self.failure_counts.insert(node_name.to_string(), 0); // 重置计数

                Some(FailoverAction {
                    failed_node: node_name.to_string(),
                    failure_count: count,
                    target: self.policy.fallback_group.clone(),
                })
            } else {
                None
            }
        }
    }

    /// 获取指定节点的当前连续失败次数
    pub fn failure_count(&self, node_name: &str) -> u32 {
        self.failure_counts.get(node_name).copied().unwrap_or(0)
    }

    /// Reset all node failure states (counts, per-node cooldown, access order, and group-level cooldown).
    pub fn reset_all(&mut self) {
        self.failure_counts.clear();
        self.last_switch.clear();
        self.access_order.clear();
        self.group_last_switch = None;
    }

    /// 获取策略配置的引用
    pub fn policy(&self) -> &NodeFailPolicy {
        &self.policy
    }
}

impl Default for FailoverTracker {
    fn default() -> Self {
        Self::new(NodeFailPolicy::default())
    }
}

/// 节点切换动作描述
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverAction {
    /// 触发切换的失败节点名称
    pub failed_node: String,
    /// 该节点的连续失败次数
    pub failure_count: u32,
    /// 切换目标（"next" 或具体组名）
    pub target: String,
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy() {
        let policy = NodeFailPolicy::default();
        assert_eq!(policy.threshold, 3);
        assert_eq!(policy.fallback_group, "next");
        assert_eq!(policy.cooldown, Duration::from_secs(30));
        assert!(policy.enabled);
    }

    #[test]
    fn test_failover_trigger_on_threshold() {
        let policy = NodeFailPolicy::new().with_threshold(3).with_cooldown(0); // 无冷却
        let mut tracker = FailoverTracker::new(policy);

        // 前两次失败不应触发
        assert!(tracker.report("node-a", false).is_none());
        assert_eq!(tracker.failure_count("node-a"), 1);

        assert!(tracker.report("node-a", false).is_none());
        assert_eq!(tracker.failure_count("node-a"), 2);

        // 第三次失败应触发切换
        let action = tracker
            .report("node-a", false)
            .expect("should trigger failover");
        assert_eq!(action.failed_node, "node-a");
        assert_eq!(action.failure_count, 3);
        assert_eq!(action.target, "next");

        // 触发后计数应重置
        assert_eq!(tracker.failure_count("node-a"), 0);
    }

    #[test]
    fn test_success_resets_count() {
        let policy = NodeFailPolicy::new().with_threshold(3);
        let mut tracker = FailoverTracker::new(policy);

        tracker.report("node-a", false);
        tracker.report("node-a", false);
        assert_eq!(tracker.failure_count("node-a"), 2);

        // 成功重置
        tracker.report("node-a", true);
        assert_eq!(tracker.failure_count("node-a"), 0);
    }

    #[test]
    fn test_cooldown_prevents_rapid_switch() {
        let policy = NodeFailPolicy::new().with_threshold(1).with_cooldown(60); // 60秒冷却
        let mut tracker = FailoverTracker::new(policy);

        // 第一次触发
        let action1 = tracker.report("node-a", false);
        assert!(action1.is_some());

        // 立即再次触发应在冷却期内被抑制
        let action2 = tracker.report("node-b", false);
        assert!(action2.is_none(), "should be suppressed by cooldown");
    }

    #[test]
    fn test_disabled_policy_no_action() {
        let policy = NodeFailPolicy {
            enabled: false,
            ..Default::default()
        };
        let mut tracker = FailoverTracker::new(policy);

        // 即使超过阈值也不触发
        for _ in 0..5 {
            assert!(tracker.report("node-a", false).is_none());
        }
    }

    #[test]
    fn test_custom_fallback_group() {
        let policy = NodeFailPolicy::new()
            .with_threshold(2)
            .with_fallback("DIRECT")
            .with_cooldown(0);
        let mut tracker = FailoverTracker::new(policy);

        tracker.report("proxy-node", false);
        let action = tracker.report("proxy-node", false).expect("should trigger");
        assert_eq!(action.target, "DIRECT");
    }

    #[test]
    fn test_multiple_nodes_independent() {
        let policy = NodeFailPolicy::new().with_threshold(2).with_cooldown(0);
        let mut tracker = FailoverTracker::new(policy);

        // node-a 失败 1 次
        tracker.report("node-a", false);
        // node-b 失败 2 次 → 应触发
        let action = tracker.report("node-b", false);
        assert!(action.is_none()); // node-b 只失败了 1 次

        let action = tracker.report("node-b", false);
        assert!(action.is_some());
        assert_eq!(action.as_ref().unwrap().failed_node, "node-b");

        // node-a 不受影响，仍为 1
        assert_eq!(tracker.failure_count("node-a"), 1);
    }

    // ─── Duration 亚秒精度 ───

    #[test]
    fn test_duration_roundtrip_subsecond_precision() {
        // 验证亚秒 Duration 序列化/反序列化往返不丢失精度
        let d = Duration::from_millis(1500); // 1.5s
        let policy = NodeFailPolicy {
            cooldown: d,
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let restored: NodeFailPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored.cooldown);
    }

    #[test]
    fn test_duration_roundtrip_integer_seconds() {
        // 整数秒也应正确往返
        let d = Duration::from_secs(30);
        let policy = NodeFailPolicy {
            cooldown: d,
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let restored: NodeFailPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored.cooldown);
    }

    #[test]
    fn test_duration_roundtrip_zero() {
        let d = Duration::ZERO;
        let policy = NodeFailPolicy {
            cooldown: d,
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let restored: NodeFailPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored.cooldown);
    }

    #[test]
    fn test_duration_roundtrip_mixed() {
        // 混合：1小时30分15.5秒
        let d = Duration::from_secs(5415) + Duration::from_millis(500);
        let policy = NodeFailPolicy {
            cooldown: d,
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let restored: NodeFailPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored.cooldown);
    }

    #[test]
    fn test_parse_human_duration_fractional_seconds() {
        // 支持小数秒格式
        assert_eq!(
            parse_human_duration("1.5s"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_human_duration("0.5s"),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse_human_duration("0.001s"),
            Some(Duration::from_micros(1000))
        );
    }

    #[test]
    fn test_parse_human_duration_fractional_minutes() {
        assert_eq!(parse_human_duration("1.5m"), Some(Duration::from_secs(90)));
        assert_eq!(
            parse_human_duration("0.1m"),
            Some(Duration::from_millis(6000))
        );
    }

    #[test]
    fn test_parse_human_duration_combined_fractional() {
        // "1h30m15.5s" = 3600 + 1800 + 15.5 = 5415.5s
        let result = parse_human_duration("1h30m15.5s").unwrap();
        assert_eq!(
            result,
            Duration::from_secs(5415) + Duration::from_millis(500)
        );
    }

    #[test]
    fn test_parse_human_duration_edge_cases() {
        assert_eq!(parse_human_duration("0s"), None); // 零值拒绝
        assert_eq!(parse_human_duration(""), None);
        assert_eq!(parse_human_duration("abc"), None);
        assert_eq!(parse_human_duration("1x"), None); // 无效单位
        assert_eq!(parse_human_duration("s"), None); // 缺少数值
    }
}
