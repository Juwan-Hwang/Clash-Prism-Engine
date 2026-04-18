//! EMA 评分算法 — Smart Selector 的核心
//!
//! ## 评分公式
//!
//! ```text
//! score = (
//!     latency_score × weights.latency_p90
//!   + success_rate_score × weights.success_rate
//!   + stability_score × weights.stability
//! ) × time_weight
//! ```
//!
//! - **P90 延迟分数**：对抗偶发高延迟假象
//! - **成功率分数**：直接使用 success_rate
//! - **稳定性分数**：标准差越小越稳定
//! - **时间衰减（EMA）**：旧数据权重递减

use crate::history::NodeHistory;

/// P90 延迟优秀阈值（毫秒）— 低于此值得满分
const LATENCY_EXCELLENT_MS: f64 = 50.0;
/// P90 延迟良好阈值（毫秒）— 低于此值线性衰减
const LATENCY_GOOD_MS: f64 = 200.0;
/// 良好区间衰减斜率（每毫秒扣分）
const LATENCY_GOOD_DECAY: f64 = 0.4;
/// 超过良好阈值后的衰减斜率
const LATENCY_EXCESS_DECAY: f64 = 0.05;
/// 良好阈值处的分数（也是超过阈值的起始分数）
const LATENCY_GOOD_FLOOR: f64 = 40.0;

/// 评分权重配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScoreWeights {
    /// P90 延迟权重
    pub latency_p90: f64,
    /// 成功率权重
    pub success_rate: f64,
    /// 稳定性（延迟标准差）权重
    pub stability: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            latency_p90: 0.4,
            success_rate: 0.4,
            stability: 0.2,
        }
    }
}

/// 时间衰减配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecayConfig {
    /// 半衰期（小时）— 1 小时前的数据权重衰减到 50%
    #[serde(default = "default_half_life")]
    pub half_life_hours: f64,
}

fn default_half_life() -> f64 {
    1.0
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            half_life_hours: 1.0,
        }
    }
}

/// Smart 评分器
pub struct SmartScorer {
    /// 评分权重
    pub weights: ScoreWeights,

    /// 时间衰减配置
    pub decay: DecayConfig,

    /// 衰减系数（预计算）
    decay_coefficient: f64,
}

impl SmartScorer {
    /// 创建新的评分器（使用默认参数）
    pub fn new() -> Self {
        Self::with_config(ScoreWeights::default(), DecayConfig::default())
    }

    /// 使用自定义配置创建评分器
    ///
    /// 权重总和为 0 时会在 score_at 中返回安全默认值 0.0，
    /// 但在此处提前警告可以帮助调用方尽早发现配置错误。
    pub fn with_config(weights: ScoreWeights, decay: DecayConfig) -> Self {
        let weight_sum = weights.latency_p90 + weights.success_rate + weights.stability;
        if weight_sum <= 0.0 {
            tracing::warn!(
                "SmartScorer::with_config: 权重总和为 {} (<= 0)，所有评分将返回 0.0",
                weight_sum
            );
        } else if weight_sum > 10.0 {
            tracing::warn!(
                "SmartScorer::with_config: 权重总和为 {} (> 10)，评分可能超出预期范围",
                weight_sum
            );
        }

        // 预计算衰减系数: λ = ln(2) / T_half
        let decay_coefficient = if decay.half_life_hours <= 0.0 {
            tracing::warn!(
                "half_life_hours must be positive, got {}. Using default 1.0",
                decay.half_life_hours
            );
            std::f64::consts::LN_2 / 1.0
        } else {
            std::f64::consts::LN_2 / decay.half_life_hours
        };
        Self {
            weights,
            decay,
            decay_coefficient,
        }
    }

    /// 计算单个节点的综合评分
    ///
    /// 返回值范围：0.0 ~ 100.0，越高越好
    pub fn score(&self, node: &NodeHistory) -> f64 {
        let now = chrono::Utc::now();
        self.score_at(node, now)
    }

    /// 计算单个节点的综合评分（可注入当前时间，便于测试）
    ///
    /// 返回值范围：0.0 ~ 100.0，越高越好
    pub fn score_at(&self, node: &NodeHistory, now: chrono::DateTime<chrono::Utc>) -> f64 {
        // 置信度惩罚：无历史数据的节点评分打折
        let confidence = if node.latency_records.is_empty() {
            0.5 // 无数据节点置信度减半
        } else {
            1.0
        };

        // P90 延迟分数（而非平均延迟，对抗偶发高延迟假象）
        let latency_score = if node.p90_latency.is_nan() {
            tracing::debug!(
                node = %node.name,
                "p90_latency is NaN, treating as worst-case latency"
            );
            0.0
        } else {
            match node.p90_latency {
                d if d < LATENCY_EXCELLENT_MS => 100.0,
                d if d < LATENCY_GOOD_MS => 100.0 - (d - LATENCY_EXCELLENT_MS) * LATENCY_GOOD_DECAY,
                d => (LATENCY_GOOD_FLOOR - (d - LATENCY_GOOD_MS) * LATENCY_EXCESS_DECAY).max(0.0),
            }
        };

        // 成功率分数
        let success_rate_score = node.success_rate * 100.0;

        // 稳定性分数（标准差越小越稳定）
        // NaN 检查：防止 latency_stddev 为 NaN 时评分传播 NaN
        let stability_score = if node.latency_stddev.is_nan() {
            tracing::debug!(
                node = %node.name,
                "latency_stddev is NaN, treating stability_score as 0"
            );
            0.0
        } else {
            (100.0 - node.latency_stddev * 0.5).max(0.0)
        };

        // 时间衰减（EMA）
        // 将负值 clamp 到 0，防止时钟偏移（last_test 为未来时间）导致
        // 负指数 → time_weight > 1 → 未来数据反而获得更高权重
        // 避免 num_milliseconds() 在超大时间跨度（> i32::MAX 毫秒 ≈ 24.8 天）时溢出。
        let hours_since_last_test = (now - node.last_test).num_seconds() as f64 / 3600.0;
        let hours_since_last_test = hours_since_last_test.max(0.0);
        let time_weight = (-self.decay_coefficient * hours_since_last_test).exp();

        // 负权重检测：clamp 到 0.0 并记录警告
        let w_latency = if self.weights.latency_p90 < 0.0 {
            tracing::warn!(
                "SmartScorer: latency_p90 权重为负数 ({})，已 clamp 到 0.0",
                self.weights.latency_p90
            );
            0.0
        } else {
            self.weights.latency_p90
        };
        let w_success = if self.weights.success_rate < 0.0 {
            tracing::warn!(
                "SmartScorer: success_rate 权重为负数 ({})，已 clamp 到 0.0",
                self.weights.success_rate
            );
            0.0
        } else {
            self.weights.success_rate
        };
        let w_stability = if self.weights.stability < 0.0 {
            tracing::warn!(
                "SmartScorer: stability 权重为负数 ({})，已 clamp 到 0.0",
                self.weights.stability
            );
            0.0
        } else {
            self.weights.stability
        };

        let weight_sum = w_latency + w_success + w_stability;

        let base = if weight_sum <= 0.0 {
            tracing::warn!(
                "SmartScorer: 权重总和为 {} (<= 0)，返回默认安全分数 0.0",
                weight_sum
            );
            0.0
        } else {
            // 加权综合评分（归一化）
            (latency_score * w_latency
                + success_rate_score * w_success
                + stability_score * w_stability)
                / weight_sum
        };

        (base * time_weight * confidence).clamp(0.0, 100.0)
    }

    /// 对一组节点排序并返回排名
    ///
    /// Returns: Vec<(node_name, score, rank)>
    pub fn rank(&self, nodes: &[NodeHistory]) -> Vec<(String, f64, usize)> {
        // 在方法开头获取一次 now，传递给所有 score_at() 调用，确保时间一致性
        let now = chrono::Utc::now();
        // 先计算分数，使用索引排序避免对每个节点名称克隆
        let mut scored: Vec<(usize, f64)> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (i, self.score_at(n, now)))
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        scored
            .into_iter()
            .enumerate()
            .map(|(rank, (idx, score))| (nodes[idx].name.clone(), score, rank + 1))
            .collect()
    }

    /// 选择最佳节点（排名第一的）
    ///
    /// 确保同一批次中所有节点的评分使用相同的时间基准，避免因时间差异导致排序不一致。
    pub fn select_best<'a>(&self, nodes: &'a [NodeHistory]) -> Option<&'a NodeHistory> {
        if nodes.is_empty() {
            return None;
        }

        // 在入口处获取一次 now，确保时间一致性（与 rank 方法行为一致）
        let now = chrono::Utc::now();

        // 先计算所有分数，避免生命周期冲突
        let scored: Vec<(usize, f64)> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (i, self.score_at(node, now)))
            .collect();

        // 找到分数最高的索引
        scored
            .into_iter()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(idx, _)| &nodes[idx])
    }
}

impl Default for SmartScorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::LatencyRecord;

    fn make_test_node(name: &str, p90: f64, success_rate: f64, stddev: f64) -> NodeHistory {
        let mut node = NodeHistory {
            name: name.to_string(),
            latency_records: std::collections::VecDeque::new(),
            success_rate,
            p90_latency: p90,
            latency_stddev: stddev,
            last_test: chrono::Utc::now(),
            max_records: 1000,
        };
        // 填充一条记录以确保 confidence = 1.0（非空 latency_records）
        // 直接 push 而非调用 add_record，避免触发 recalculate() 覆盖手动设置的统计量
        node.latency_records.push_back(LatencyRecord {
            timestamp: node.last_test,
            latency_ms: p90,
            success: true,
        });
        node
    }

    #[test]
    fn test_basic_scoring() {
        let scorer = SmartScorer::new();
        let good = make_test_node("good", 50.0, 0.99, 10.0);
        let bad = make_test_node("bad", 500.0, 0.5, 200.0);

        let score_good = scorer.score(&good);
        let score_bad = scorer.score(&bad);

        assert!(
            score_good > score_bad,
            "Good node should score higher than bad node"
        );
    }

    #[test]
    fn test_ranking_order() {
        let scorer = SmartScorer::new();
        let nodes = vec![
            make_test_node("medium", 150.0, 0.9, 50.0),
            make_test_node("best", 30.0, 0.99, 5.0),
            make_test_node("worst", 800.0, 0.3, 300.0),
        ];

        let ranking = scorer.rank(&nodes);

        assert_eq!(ranking[0].0, "best"); // 排名第 1
        assert_eq!(ranking[1].0, "medium"); // 排名第 2
        assert_eq!(ranking[2].0, "worst"); // 排名第 3
    }

    #[test]
    fn test_time_decay() {
        let scorer = SmartScorer::with_config(
            ScoreWeights::default(),
            DecayConfig {
                half_life_hours: 1.0,
            },
        );

        let recent = make_test_node("recent", 100.0, 0.95, 20.0);
        let mut stale = make_test_node("stale", 100.0, 0.95, 20.0);
        stale.last_test = chrono::Utc::now() - chrono::Duration::hours(5); // 5 小时前

        let score_recent = scorer.score(&recent);
        let score_stale = scorer.score(&stale);

        assert!(
            score_recent > score_stale,
            "Recent data should score higher due to time decay"
        );
    }

    // ═══ Adversarial / edge-case tests ═══

    #[test]
    fn score_all_zero_history_is_very_low() {
        let scorer = SmartScorer::new();
        // Node with no data: p90=0, success_rate=1.0 (default), stddev=0
        let node = NodeHistory::new("empty-node");
        let score = scorer.score(&node);

        // With p90=0 → latency_score=100, success_rate=1.0 → 100, stddev=0 → stability=100
        // base = 100*0.4 + 100*0.4 + 100*0.2 = 100
        // time_weight ≈ 1.0 (just created)
        // confidence = 0.5 (no latency_records → confidence penalty)
        // So score should be near 50 (100 * 1.0 * 0.5 = 50)
        assert!(
            score > 25.0 && score < 75.0,
            "Empty-history node should be penalized by confidence factor, got {}",
            score
        );
    }

    #[test]
    fn score_perfect_node_near_100() {
        let scorer = SmartScorer::new();
        let perfect = make_test_node("perfect", 0.0, 1.0, 0.0);
        let score = scorer.score(&perfect);

        // latency_score: p90=0 < 50 → 100
        // success_rate_score: 1.0 * 100 = 100
        // stability_score: 100 - 0*0.5 = 100
        // base = 100*0.4 + 100*0.4 + 100*0.2 = 100
        // time_weight ≈ 1.0
        assert!(
            (score - 100.0).abs() < 0.1,
            "Perfect node should score near 100, got {}",
            score
        );
    }

    #[test]
    fn score_terrible_node_near_0() {
        let scorer = SmartScorer::new();
        let terrible = make_test_node("terrible", 1000.0, 0.0, 200.0);
        let score = scorer.score(&terrible);

        // latency_score: p90=1000 → (40 - (1000-200)*0.05).max(0) = (40-40).max(0) = 0
        // success_rate_score: 0.0 * 100 = 0
        // stability_score: (100 - 200*0.5).max(0) = 0
        // base = 0*0.4 + 0*0.4 + 0*0.2 = 0
        assert!(
            score < 1.0,
            "Terrible node should score near 0, got {}",
            score
        );
    }

    #[test]
    fn time_decay_recent_vs_old_same_stats() {
        let scorer = SmartScorer::with_config(
            ScoreWeights::default(),
            DecayConfig {
                half_life_hours: 1.0,
            },
        );

        let now = chrono::Utc::now();
        let mut recent = make_test_node("recent", 100.0, 0.95, 20.0);
        recent.last_test = now;

        let mut old = make_test_node("old", 100.0, 0.95, 20.0);
        old.last_test = now - chrono::Duration::hours(1);

        let score_recent = scorer.score(&recent);
        let score_old = scorer.score(&old);

        assert!(
            score_recent > score_old,
            "Recent record (now) should score higher than 1-hour-old record: recent={}, old={}",
            score_recent,
            score_old
        );

        // After 1 half-life, score should be approximately halved
        let ratio = score_old / score_recent;
        assert!(
            ratio < 0.6,
            "After 1 half-life, old score should be significantly lower. ratio={}",
            ratio
        );
    }

    #[test]
    fn select_best_empty_list_returns_none() {
        let scorer = SmartScorer::new();
        let nodes: Vec<NodeHistory> = vec![];
        assert!(scorer.select_best(&nodes).is_none());
    }

    #[test]
    fn select_best_single_node_returns_that_node() {
        let scorer = SmartScorer::new();
        let node = make_test_node("only", 50.0, 0.9, 10.0);
        let nodes = vec![node];
        let best = scorer.select_best(&nodes).unwrap();
        assert_eq!(best.name, "only");
    }

    #[test]
    fn select_best_multiple_nodes_returns_highest() {
        let scorer = SmartScorer::new();
        let nodes = vec![
            make_test_node("low", 500.0, 0.3, 200.0),
            make_test_node("mid", 150.0, 0.8, 50.0),
            make_test_node("high", 30.0, 0.99, 5.0),
        ];
        let best = scorer.select_best(&nodes).unwrap();
        assert_eq!(best.name, "high");
    }

    #[test]
    fn rank_with_tied_scores_stable_ordering() {
        let scorer = SmartScorer::new();
        let now = chrono::Utc::now();

        let mut n1 = make_test_node("node-a", 50.0, 1.0, 0.0);
        n1.last_test = now;

        let mut n2 = make_test_node("node-b", 50.0, 1.0, 0.0);
        n2.last_test = now;

        let nodes = vec![n1, n2];
        let ranking = scorer.rank(&nodes);

        // Both should have rank 1 (tied) — sort_by is stable so original order preserved
        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].0, "node-a");
        assert_eq!(ranking[1].0, "node-b");
        // Both should have rank 1 due to tie
        assert_eq!(ranking[0].2, 1);
        assert_eq!(ranking[1].2, 2); // Second in sorted order gets rank 2
    }

    #[test]
    fn score_with_high_latency_but_good_success() {
        let scorer = SmartScorer::new();
        // High latency but 100% success and low jitter
        let node = make_test_node("slow-but-reliable", 300.0, 1.0, 5.0);
        let score = scorer.score(&node);

        // latency_score: 300 → (40 - (300-200)*0.05).max(0) = (40-5).max(0) = 35
        // success_rate_score: 100
        // stability_score: 100 - 5*0.5 = 97.5
        // base = 35*0.4 + 100*0.4 + 97.5*0.2 = 14 + 40 + 19.5 = 73.5
        assert!(
            score > 65.0 && score < 80.0,
            "Slow but reliable node should score moderately, got {}",
            score
        );
    }

    #[test]
    fn score_with_low_latency_but_poor_success() {
        let scorer = SmartScorer::new();
        // Low latency but 50% success rate
        let node = make_test_node("fast-but-unreliable", 10.0, 0.5, 5.0);
        let score = scorer.score(&node);

        // latency_score: 10 < 50 → 100
        // success_rate_score: 0.5 * 100 = 50
        // stability_score: 100 - 5*0.5 = 97.5
        // base = 100*0.4 + 50*0.4 + 97.5*0.2 = 40 + 20 + 19.5 = 79.5
        assert!(
            score > 70.0 && score < 90.0,
            "Fast but unreliable node should score moderately-high, got {}",
            score
        );
    }

    #[test]
    fn rank_returns_correct_rank_numbers() {
        let scorer = SmartScorer::new();
        let nodes = vec![
            make_test_node("worst", 800.0, 0.3, 300.0),
            make_test_node("best", 30.0, 0.99, 5.0),
            make_test_node("mid", 150.0, 0.9, 50.0),
        ];
        let ranking = scorer.rank(&nodes);

        assert_eq!(ranking[0].0, "best");
        assert_eq!(ranking[0].2, 1); // rank 1
        assert_eq!(ranking[1].0, "mid");
        assert_eq!(ranking[1].2, 2); // rank 2
        assert_eq!(ranking[2].0, "worst");
        assert_eq!(ranking[2].2, 3); // rank 3
    }

    #[test]
    fn scorer_default_weights_sum_to_one() {
        let weights = ScoreWeights::default();
        let sum = weights.latency_p90 + weights.success_rate + weights.stability;
        assert!(
            (sum - 1.0).abs() < 1e-10,
            "Weights should sum to 1.0, got {}",
            sum
        );
    }

    #[test]
    fn scorer_with_zero_half_life_uses_default() {
        // half_life_hours <= 0 should be handled gracefully
        let scorer = SmartScorer::with_config(
            ScoreWeights::default(),
            DecayConfig {
                half_life_hours: 0.0,
            },
        );
        // Should not panic, should use default 1.0
        let node = make_test_node("test", 50.0, 1.0, 0.0);
        let score = scorer.score(&node);
        assert!(score.is_finite(), "Score should be finite, got {}", score);
    }

    #[test]
    fn scorer_with_all_zero_weights_returns_safe_default() {
        let scorer = SmartScorer::with_config(
            ScoreWeights {
                latency_p90: 0.0,
                success_rate: 0.0,
                stability: 0.0,
            },
            DecayConfig::default(),
        );
        let node = make_test_node("test", 50.0, 1.0, 0.0);
        let score = scorer.score(&node);
        assert!(score.is_finite(), "零权重评分应返回有限值，got {}", score);
        assert_eq!(score, 0.0, "零权重评分应返回安全默认值 0.0");
    }

    #[test]
    fn scorer_with_negative_weights_no_panic() {
        let scorer = SmartScorer::with_config(
            ScoreWeights {
                latency_p90: -0.5,
                success_rate: -0.3,
                stability: -0.2,
            },
            DecayConfig::default(),
        );
        let node = make_test_node("test", 50.0, 1.0, 0.0);
        let score = scorer.score(&node);
        assert!(score.is_finite(), "负权重评分应返回有限值，got {}", score);
    }
}
