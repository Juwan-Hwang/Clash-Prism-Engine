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
            let t = (network_quality - bad) / (good - bad); // 0.0 (bad) → 1.0 (good)
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
