//! 自适应测速调度器
//!
//! ## 调度策略
//!
//! 网络质量 > 0.9 时：间隔 × 3（降低频率）
//! 网络质量 < 0.3 时：间隔 × 0.25（提高频率）
//!
//! 这样在网络好的时候减少不必要的测速，
//! 在网络差的时候增加测速频率以更快发现恢复。

use std::sync::RwLock;

use crate::config::SchedulerConfig;

/// 自适应调度器
///
/// 使用 RwLock 替代 Mutex，因为 next_interval() 和 config()
/// 都是只读操作（占绝大多数场景），RwLock 允许多个读者并发访问。
pub struct AdaptiveScheduler {
    /// 调度器配置（受 RwLock 保护）
    config: RwLock<SchedulerConfig>,
}

impl AdaptiveScheduler {
    /// 创建新的自适应调度器
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config: RwLock::new(config),
        }
    }

    /// 根据当前网络质量计算下一次测速的间隔
    ///
    /// # Arguments
    /// * `network_quality` - 当前网络质量评分 (0.0 ~ 1.0)
    ///
    /// # Returns
    /// 下次测速间隔（秒），被 clamp 到 `[min_interval, max_interval]` 范围
    pub fn next_interval(&self, network_quality: f64) -> u64 {
        // NaN 输入检查：NaN 时返回 base_interval_secs 作为安全默认值
        if network_quality.is_nan() {
            tracing::warn!("next_interval: network_quality 为 NaN，返回 base_interval_secs");
            let config = match self.config.read() {
                Ok(guard) => guard,
                Err(_) => {
                    // 中毒意味着持有锁的线程 panic，数据可能处于不一致状态。
                    tracing::error!("AdaptiveScheduler RwLock 已中毒，使用默认配置重建");
                    return SchedulerConfig::default().base_interval_secs;
                }
            };
            return config.base_interval_secs;
        }

        let config = match self.config.read() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::error!("AdaptiveScheduler RwLock 已中毒，使用默认配置重建");
                return SchedulerConfig::default().base_interval_secs;
            }
        };

        if !config.adaptive {
            return config.base_interval_secs;
        }

        // 防御性解包：即使 serde default 已保证 adaptive=true 时
        // adaptive_params 不为 None，这里仍做安全降级
        let params = match &config.adaptive_params {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "adaptive=true but adaptive_params is missing, falling back to base interval"
                );
                return config.base_interval_secs;
            }
        };

        // 线性插值：在 good 和 bad 阈值之间平滑映射 multiplier，
        // good 以上为 3.0x（降低频率），bad 以下为 0.25x（提高频率）。
        let multiplier = if network_quality > params.good_quality_threshold {
            3.0 // 网络好，降低频率
        } else if network_quality < params.bad_quality_threshold {
            0.25 // 网络差，提高频率
        } else {
            // good 与 bad 之间线性插值
            let good = params.good_quality_threshold;
            let bad = params.bad_quality_threshold;
            let denom = good - bad;
            let t = if denom.abs() < f64::EPSILON {
                0.5 // good == bad 时取中间值
            } else {
                (network_quality - bad) / denom // 0.0 (bad) → 1.0 (good)
            };
            0.25 + t * (3.0 - 0.25) // 0.25 → 3.0
        };

        // 超出 u64::MAX 导致截断为不正确的值。
        let interval = (config.base_interval_secs as f64 * multiplier)
            .min(u64::MAX as f64)
            .max(0.0) as u64;

        let min_interval = config.min_interval_secs.unwrap_or(10);
        let max_interval = config.max_interval_secs.unwrap_or(3600);
        interval.clamp(min_interval, max_interval)
    }

    /// 获取基础配置的克隆副本
    ///
    pub fn config(&self) -> SchedulerConfig {
        self.config.read().map(|g| g.clone()).unwrap_or_else(|_| {
            tracing::error!("AdaptiveScheduler RwLock 已中毒，返回默认配置");
            SchedulerConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdaptiveParams;

    /// 辅助：构建一个启用了自适应的 SchedulerConfig
    fn adaptive_config() -> SchedulerConfig {
        SchedulerConfig {
            base_interval_secs: 300,
            adaptive: true,
            adaptive_params: Some(AdaptiveParams {
                good_quality_threshold: 0.9,
                bad_quality_threshold: 0.3,
            }),
            min_interval_secs: Some(10),
            max_interval_secs: Some(3600),
        }
    }

    // -----------------------------------------------------------------------
    // test_next_interval_good_network
    // quality=0.95 > good(0.9) → multiplier=3.0 → 300*3=900
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_good_network() {
        let scheduler = AdaptiveScheduler::new(adaptive_config());
        let interval = scheduler.next_interval(0.95);
        // 300 * 3.0 = 900，在 [10, 3600] 范围内
        assert_eq!(interval, 900, "网络好时应返回 3x 间隔（加速）");
        assert!(
            interval < adaptive_config().base_interval_secs * 4,
            "网络好时不应过于频繁测速"
        );
    }

    // -----------------------------------------------------------------------
    // test_next_interval_bad_network
    // quality=0.1 < bad(0.3) → multiplier=0.25 → 300*0.25=75
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_bad_network() {
        let scheduler = AdaptiveScheduler::new(adaptive_config());
        let interval = scheduler.next_interval(0.1);
        // 300 * 0.25 = 75
        assert_eq!(interval, 75, "网络差时应返回 0.25x 间隔（减速）");
        assert!(
            interval < adaptive_config().base_interval_secs,
            "网络差时应更频繁测速"
        );
    }

    // -----------------------------------------------------------------------
    // test_next_interval_medium_network
    // quality=0.6 ∈ [0.3, 0.9] → 线性插值
    // t = (0.6 - 0.3) / (0.9 - 0.3) = 0.5
    // multiplier = 0.25 + 0.5 * 2.75 = 1.625
    // interval = 300 * 1.625 = 487.5 → 487
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_medium_network() {
        let scheduler = AdaptiveScheduler::new(adaptive_config());
        let interval = scheduler.next_interval(0.6);
        // 300 * 1.625 = 487.5 → 487 (truncated)
        assert_eq!(interval, 487, "中等网络质量应返回插值间隔");
        assert!(
            interval >= adaptive_config().min_interval_secs.unwrap()
                && interval <= adaptive_config().max_interval_secs.unwrap(),
            "中等质量间隔应在合理范围内"
        );
    }

    // -----------------------------------------------------------------------
    // test_next_interval_nan_returns_base
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_nan_returns_base() {
        let scheduler = AdaptiveScheduler::new(adaptive_config());
        let interval = scheduler.next_interval(f64::NAN);
        assert_eq!(interval, 300, "NaN 输入应返回 base_interval_secs (300)");
    }

    // -----------------------------------------------------------------------
    // test_next_interval_non_adaptive_returns_base
    // adaptive=false → 忽略 quality，始终返回 base
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_non_adaptive_returns_base() {
        let mut config = adaptive_config();
        config.adaptive = false;
        let scheduler = AdaptiveScheduler::new(config);
        // 即使网络质量极好，非自适应模式也应返回 base
        assert_eq!(scheduler.next_interval(0.99), 300);
        // 即使网络质量极差，非自适应模式也应返回 base
        assert_eq!(scheduler.next_interval(0.01), 300);
    }

    // -----------------------------------------------------------------------
    // test_next_interval_clamped_to_min_max
    // 构造极端配置：base=100, min=50, max=200
    // quality=0.99 → multiplier=3.0 → 300, clamp → 200
    // quality=0.01 → multiplier=0.25 → 25, clamp → 50
    // -----------------------------------------------------------------------
    #[test]
    fn test_next_interval_clamped_to_min_max() {
        let config = SchedulerConfig {
            base_interval_secs: 100,
            adaptive: true,
            adaptive_params: Some(AdaptiveParams {
                good_quality_threshold: 0.9,
                bad_quality_threshold: 0.3,
            }),
            min_interval_secs: Some(50),
            max_interval_secs: Some(200),
        };
        let scheduler = AdaptiveScheduler::new(config);

        // quality 极好 → 100 * 3.0 = 300 → clamp to 200
        let high = scheduler.next_interval(0.99);
        assert_eq!(high, 200, "应被 clamp 到 max_interval");

        // quality 极差 → 100 * 0.25 = 25 → clamp to 50
        let low = scheduler.next_interval(0.01);
        assert_eq!(low, 50, "应被 clamp 到 min_interval");

        // quality 中等 → 100 * 1.625 = 162.5 → 162, 在 [50, 200] 内
        let mid = scheduler.next_interval(0.6);
        assert!(
            mid >= 50 && mid <= 200,
            "中等质量间隔应在 [min, max] 范围内，实际: {mid}"
        );
    }
}
