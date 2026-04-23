//! Smart Selector 配置（smart.toml 解析）
//!
//! ## 配置结构
//!
//! ```toml
//! [score]
//! type = "ema"
//!
//! [score.weights]
//! latency_p90 = 0.4
//! success_rate = 0.4
//! stability = 0.2
//!
//! [score.decay]
//! half_life_hours = 1.0
//!
//! [scheduler]
//! base_interval_secs = 300
//! adaptive = true
//!
//! [proxy-groups.auto]
//! filter = "name.includes('香港')"
//! url = "http://www.gstatic.com/generate_204"
//! interval = 300
//! tolerance = 50
//! ```

use serde::{Deserialize, Serialize};

use crate::scorer::{DecayConfig, ScoreWeights};

/// smart.toml 完整配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SmartConfig {
    /// 评分配置
    #[serde(default)]
    pub score: ScoreConfig,

    /// 调度器配置
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// 自动代理组配置
    #[serde(default)]
    pub proxy_groups: std::collections::BTreeMap<String, ProxyGroupSmartConfig>,
}

/// 评分配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreConfig {
    /// 评分算法类型（v1 仅支持 ema）
    #[serde(default = "default_score_type")]
    pub r#type: String,

    /// 权重配置
    #[serde(default)]
    pub weights: ScoreWeights,

    /// 时间衰减配置
    #[serde(default)]
    pub decay: DecayConfig,
}

fn default_score_type() -> String {
    "ema".into()
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            r#type: default_score_type(),
            weights: ScoreWeights::default(),
            decay: DecayConfig::default(),
        }
    }
}

/// 调度器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// 基础测速间隔（秒）
    #[serde(default = "default_base_interval")]
    pub base_interval_secs: u64,

    /// 是否启用自适应调度
    #[serde(default = "default_adaptive")]
    pub adaptive: bool,

    /// 自适应参数
    /// 当 adaptive=true 时此字段必须存在，serde 反序列化时自动填充默认值
    #[serde(default = "default_adaptive_params")]
    pub adaptive_params: Option<AdaptiveParams>,

    /// 最大测速间隔（秒），防止自适应调度在网络好时间隔过长
    /// 默认 3600 秒（1 小时）
    #[serde(default = "default_max_interval")]
    pub max_interval_secs: Option<u64>,

    /// 最小测速间隔（秒），防止自适应调度在网络差时间隔过短
    /// 默认 10 秒
    #[serde(default = "default_min_interval")]
    pub min_interval_secs: Option<u64>,
}

fn default_max_interval() -> Option<u64> {
    Some(3600)
}

fn default_min_interval() -> Option<u64> {
    Some(10)
}

fn default_base_interval() -> u64 {
    300 // 5 分钟
}

fn default_adaptive() -> bool {
    true
}

/// serde 默认值：adaptive=true 时 adaptive_params 默认为 Some(AdaptiveParams::default())
/// 这与 SchedulerConfig 的 Default impl 行为一致，避免反序列化时出现 None 导致 panic
fn default_adaptive_params() -> Option<AdaptiveParams> {
    Some(AdaptiveParams::default())
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            base_interval_secs: default_base_interval(),
            adaptive: default_adaptive(),
            adaptive_params: Some(AdaptiveParams::default()),
            max_interval_secs: default_max_interval(),
            min_interval_secs: default_min_interval(),
        }
    }
}

/// 自适应调度参数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveParams {
    /// 网络质量好时的阈值
    #[serde(default = "default_good_threshold")]
    pub good_quality_threshold: f64,

    /// 网络质量差时的阈值
    #[serde(default = "default_bad_threshold")]
    pub bad_quality_threshold: f64,
}

fn default_good_threshold() -> f64 {
    0.9
}
fn default_bad_threshold() -> f64 {
    0.3
}

impl Default for AdaptiveParams {
    fn default() -> Self {
        Self {
            good_quality_threshold: default_good_threshold(),
            bad_quality_threshold: default_bad_threshold(),
        }
    }
}

/// 单个智能代理组配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyGroupSmartConfig {
    /// 基于**静态字段**的预筛选表达式
    pub filter: Option<String>,

    /// 测速 URL
    #[serde(default = "default_test_url")]
    pub url: String,

    /// 测速间隔（秒）
    #[serde(default = "default_interval")]
    pub interval: u64,

    /// 容差（毫秒）
    #[serde(default = "default_tolerance")]
    pub tolerance: u64,
}

fn default_test_url() -> String {
    "http://www.gstatic.com/generate_204".into()
}
fn default_interval() -> u64 {
    300
}
fn default_tolerance() -> u64 {
    50
}

impl Default for ProxyGroupSmartConfig {
    fn default() -> Self {
        Self {
            filter: None,
            url: default_test_url(),
            interval: default_interval(),
            tolerance: default_tolerance(),
        }
    }
}

impl SmartConfig {
    /// 从 TOML 字符串解析配置
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// 验证配置的合法性
    ///
    /// 检查关键字段是否满足基本约束，收集所有错误一次性返回。
    ///
    /// 返回 `Result<(), Vec<String>>`，与 `PluginManifest::validate()` 风格一致，
    /// 支持一次性报告所有问题，避免反复修改-验证循环。
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = vec![];

        // score.type 校验：v1 仅支持 "ema" 算法
        if !matches!(self.score.r#type.as_str(), "ema") {
            errors.push(format!(
                "score.type '{}' is not supported. Only 'ema' is available in v1",
                self.score.r#type
            ));
        }

        if self.scheduler.base_interval_secs == 0 {
            errors.push("base_interval_secs must be > 0".into());
        }
        if let Some(adaptive) = &self.scheduler.adaptive_params {
            if adaptive.good_quality_threshold <= 0.0 || adaptive.good_quality_threshold > 1.0 {
                errors.push("adaptive_params.good_quality_threshold must be in (0, 1]".into());
            }
            if adaptive.bad_quality_threshold < 0.0 || adaptive.bad_quality_threshold >= 1.0 {
                errors.push("adaptive_params.bad_quality_threshold must be in [0, 1)".into());
            }
            // good > bad 检查
            if adaptive.good_quality_threshold <= adaptive.bad_quality_threshold {
                errors.push(format!(
                    "adaptive_params.good_quality_threshold ({}) must be > bad_quality_threshold ({})",
                    adaptive.good_quality_threshold, adaptive.bad_quality_threshold
                ));
            }
        }
        if self.score.decay.half_life_hours <= 0.0 {
            errors.push("score.decay.half_life_hours must be > 0".into());
        }
        // 权重非负检查
        if self.score.weights.latency_p90 < 0.0 {
            errors.push("score.weights.latency_p90 must be >= 0".into());
        }
        if self.score.weights.success_rate < 0.0 {
            errors.push("score.weights.success_rate must be >= 0".into());
        }
        if self.score.weights.stability < 0.0 {
            errors.push("score.weights.stability must be >= 0".into());
        }
        let weight_sum = self.score.weights.latency_p90
            + self.score.weights.success_rate
            + self.score.weights.stability;
        if (weight_sum - 1.0).abs() > 0.01 {
            errors.push(format!(
                "score.weights sum ({:.4}) must be approximately 1.0 (tolerance 0.01)",
                weight_sum
            ));
        }

        for (group_name, group_config) in &self.proxy_groups {
            if let Some(filter) = &group_config.filter {
                if filter.trim().is_empty() {
                    errors.push(format!("proxy-groups.{}.filter 不能为空字符串", group_name));
                }
                // 括号匹配检查
                let mut depth = 0i32;
                for ch in filter.chars() {
                    match ch {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    if depth < 0 {
                        errors.push(format!(
                            "proxy-groups.{}.filter 括号不匹配：存在多余的闭合括号",
                            group_name
                        ));
                        break;
                    }
                }
                if depth > 0 {
                    errors.push(format!(
                        "proxy-groups.{}.filter 括号不匹配：存在未闭合的括号",
                        group_name
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 获取默认配置的 TOML 示例
    pub fn example_toml() -> String {
        let default_config = SmartConfig::default();
        // Build a representative example with proxy-groups section
        let mut example = default_config;
        example.proxy_groups.insert(
            "auto".to_string(),
            ProxyGroupSmartConfig {
                filter: Some("name.includes('香港')".to_string()),
                url: "http://www.gstatic.com/generate_204".to_string(),
                interval: 300,
                tolerance: 50,
            },
        );
        toml::to_string_pretty(&example)
            .unwrap_or_else(|_| "# Error generating example TOML\n".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 辅助：构建一个合法的完整配置
    fn valid_config() -> SmartConfig {
        SmartConfig::default()
    }

    // -----------------------------------------------------------------------
    // test_validate_valid_default_config
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_valid_default_config() {
        let config = valid_config();
        assert!(config.validate().is_ok(), "默认配置应通过验证");
    }

    // -----------------------------------------------------------------------
    // test_validate_rejects_invalid_score_type
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rejects_invalid_score_type() {
        let mut config = valid_config();
        config.score.r#type = "invalid".into();
        let errs = config.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("'invalid' is not supported")),
            "应报告 score.type 不合法，实际错误: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // test_validate_rejects_zero_base_interval
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rejects_zero_base_interval() {
        let mut config = valid_config();
        config.scheduler.base_interval_secs = 0;
        let errs = config.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("base_interval_secs must be > 0")),
            "应报告 base_interval_secs 为零，实际错误: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // test_validate_rejects_inverted_thresholds
    // good >= bad 应报错（good 必须大于 bad）
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rejects_inverted_thresholds() {
        let mut config = valid_config();
        // 默认 good=0.9, bad=0.3 → 合法。反转后 good=0.3, bad=0.9 → good <= bad
        config.scheduler.adaptive_params = Some(AdaptiveParams {
            good_quality_threshold: 0.3,
            bad_quality_threshold: 0.9,
        });
        let errs = config.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e
                .contains("good_quality_threshold (0.3) must be > bad_quality_threshold (0.9)")),
            "应报告阈值反转，实际错误: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // test_validate_rejects_weight_sum_not_one
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rejects_weight_sum_not_one() {
        let mut config = valid_config();
        config.score.weights.latency_p90 = 0.5;
        config.score.weights.success_rate = 0.5;
        config.score.weights.stability = 0.5; // sum = 1.5
        let errs = config.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("score.weights sum")),
            "应报告权重和不等于 1.0，实际错误: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // test_validate_rejects_unbalanced_parens_filter
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rejects_unbalanced_parens_filter() {
        let mut config = valid_config();
        let mut groups: BTreeMap<String, ProxyGroupSmartConfig> = BTreeMap::new();
        groups.insert(
            "auto".to_string(),
            ProxyGroupSmartConfig {
                filter: Some("name.includes('香港'".to_string()), // 缺少右括号
                url: default_test_url(),
                interval: 300,
                tolerance: 50,
            },
        );
        config.proxy_groups = groups;
        let errs = config.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("括号不匹配")),
            "应报告括号不匹配，实际错误: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // test_example_toml_is_valid
    // -----------------------------------------------------------------------
    #[test]
    fn test_example_toml_is_valid() {
        let toml_str = SmartConfig::example_toml();
        let parsed = SmartConfig::from_toml(&toml_str).expect("example_toml() 输出应能被成功解析");
        assert!(
            parsed.validate().is_ok(),
            "解析后的配置应通过验证，错误: {:?}",
            parsed.validate().unwrap_err()
        );
    }
}
