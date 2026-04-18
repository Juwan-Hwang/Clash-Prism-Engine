//! clash-prism-smart 集成测试 — SmartConfig / SmartScorer / AdaptiveScheduler

use clash_prism_smart::config::SmartConfig;
use clash_prism_smart::history::NodeHistory;
use clash_prism_smart::scorer::{DecayConfig, ScoreWeights, SmartScorer};
use clash_prism_smart::scheduler::{AdaptiveScheduler, SchedulerConfig};

// ══════════════════════════════════════════════════════════
// SmartConfig::from_toml() + validate()
// ══════════════════════════════════════════════════════════

#[test]
fn test_smart_config_from_toml_valid() {
    let toml = r#"
[score]
type = "ema"

[score.weights]
latency_p90 = 0.4
success_rate = 0.4
stability = 0.2

[score.decay]
half_life_hours = 1.0

[scheduler]
base_interval_secs = 300
adaptive = true

[scheduler.adaptive]
good_quality_threshold = 0.9
bad_quality_threshold = 0.3

[proxy-groups.auto]
filter = "name.includes('HK')"
url = "http://www.gstatic.com/generate_204"
interval = 300
tolerance = 50
"#;
    let config = SmartConfig::from_toml(toml).expect("valid TOML should parse");
    assert_eq!(config.score.r#type, "ema");
    assert_eq!(config.score.weights.latency_p90, 0.4);
    assert_eq!(config.scheduler.base_interval_secs, 300);
    assert!(config.scheduler.adaptive);
    assert!(config.proxy_groups.contains_key("auto"));
    // validate() should pass
    config.validate().expect("valid config should pass validation");
}

#[test]
fn test_smart_config_validate_weight_sum() {
    // 权重和 = 1.0 → 通过
    let toml = r#"
[score.weights]
latency_p90 = 0.5
success_rate = 0.3
stability = 0.2
"#;
    let config = SmartConfig::from_toml(toml).unwrap();
    config.validate().expect("weight sum = 1.0 should pass");

    // 权重和 != 1.0 → 失败
    let toml_bad = r#"
[score.weights]
latency_p90 = 0.8
success_rate = 0.8
stability = 0.0
"#;
    let config_bad = SmartConfig::from_toml(toml_bad).unwrap();
    let err = config_bad.validate().unwrap_err();
    assert!(
        err.iter().any(|e| e.contains("approximately 1.0")),
        "weight sum error should mention 1.0: {:?}",
        err
    );
}

#[test]
fn test_smart_config_validate_zero_interval() {
    let toml = r#"
[scheduler]
base_interval_secs = 0
"#;
    let config = SmartConfig::from_toml(toml).unwrap();
    let err = config.validate().unwrap_err();
    assert!(
        err.iter().any(|e| e.contains("base_interval_secs must be > 0")),
        "zero interval error: {:?}",
        err
    );
}

#[test]
fn test_smart_config_validate_default_weights() {
    // 默认权重 0.4 + 0.4 + 0.2 = 1.0 → 通过
    let config = SmartConfig::default();
    config.validate().expect("default weights should sum to 1.0");
}

// ══════════════════════════════════════════════════════════
// SmartScorer::score()
// ══════════════════════════════════════════════════════════

#[test]
fn test_smart_scorer_basic() {
    let scorer = SmartScorer::new();

    let good = NodeHistory {
        name: "good".into(),
        latency_records: std::collections::VecDeque::new(),
        success_rate: 0.99,
        p90_latency: 50.0,
        latency_stddev: 10.0,
        last_test: chrono::Utc::now(),
        max_records: 1000,
    };
    let bad = NodeHistory {
        name: "bad".into(),
        latency_records: std::collections::VecDeque::new(),
        success_rate: 0.3,
        p90_latency: 800.0,
        latency_stddev: 300.0,
        last_test: chrono::Utc::now(),
        max_records: 1000,
    };

    let score_good = scorer.score(&good);
    let score_bad = scorer.score(&bad);

    assert!(
        score_good > score_bad,
        "good node ({:.1}) should score higher than bad node ({:.1})",
        score_good,
        score_bad
    );
}

#[test]
fn test_smart_scorer_custom_weights() {
    let weights = ScoreWeights {
        latency_p90: 0.5,
        success_rate: 0.5,
        stability: 0.0,
    };
    let scorer = SmartScorer::with_config(weights, DecayConfig::default());

    let node = NodeHistory {
        name: "test".into(),
        latency_records: std::collections::VecDeque::new(),
        success_rate: 1.0,
        p90_latency: 100.0,
        latency_stddev: 0.0,
        last_test: chrono::Utc::now(),
        max_records: 1000,
    };

    let score = scorer.score(&node);
    assert!(
        score > 0.0,
        "score should be positive with custom weights, got {:.1}",
        score
    );
}

// ══════════════════════════════════════════════════════════
// AdaptiveScheduler::next_interval()
// ══════════════════════════════════════════════════════════

#[test]
fn test_scheduler_non_adaptive() {
    let config = SchedulerConfig {
        base_interval_secs: 300,
        adaptive: false,
        adaptive_params: None,
        max_interval_secs: None,
    };
    let scheduler = AdaptiveScheduler::new(config);
    assert_eq!(scheduler.next_interval(0.99), 300);
    assert_eq!(scheduler.next_interval(0.1), 300);
}

#[test]
fn test_scheduler_adaptive_good_network() {
    let config = SchedulerConfig::default();
    let scheduler = AdaptiveScheduler::new(config);

    // 网络质量 > 0.9 → 间隔 × 3
    let interval = scheduler.next_interval(0.95);
    assert_eq!(interval, 900, "good network should triple interval");
}

#[test]
fn test_scheduler_adaptive_bad_network() {
    let config = SchedulerConfig::default();
    let scheduler = AdaptiveScheduler::new(config);

    // 网络质量 < 0.3 → 间隔 × 0.25
    let interval = scheduler.next_interval(0.1);
    assert_eq!(interval, 75, "bad network should quarter interval");
}

#[test]
fn test_scheduler_max_interval_clamp() {
    let config = SchedulerConfig {
        base_interval_secs: 300,
        adaptive: true,
        adaptive_params: Some(clash_prism_smart::config::AdaptiveParams::default()),
        max_interval_secs: Some(600), // 最大 10 分钟
    };
    let scheduler = AdaptiveScheduler::new(config);

    // 网络好时 300 * 3 = 900，但 max_interval = 600，应被 clamp
    let interval = scheduler.next_interval(0.95);
    assert_eq!(
        interval, 600,
        "interval should be clamped to max_interval_secs"
    );
}

#[test]
fn test_scheduler_min_interval_clamp() {
    let config = SchedulerConfig {
        base_interval_secs: 10,
        adaptive: true,
        adaptive_params: Some(clash_prism_smart::config::AdaptiveParams::default()),
        max_interval_secs: None,
    };
    let scheduler = AdaptiveScheduler::new(config);

    // 网络差时 10 * 0.25 = 2.5 → floor to 2 → max(10) = 10
    let interval = scheduler.next_interval(0.1);
    assert_eq!(interval, 10, "interval should not go below minimum");
}
