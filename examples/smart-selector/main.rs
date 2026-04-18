//! # Smart Selector 使用示例
//!
//! 演示如何使用 Smart Selector 进行节点评分和选择

use clash_prism_smart::{SmartScorer, SmartConfig, AdaptiveScheduler, history::NodeHistory};

fn main() {
    // 1. 加载配置
    let config = SmartConfig::from_toml(SmartConfig::example_toml())
        .expect("示例 TOML 应该能正确解析");

    println!("📋 Smart Config:");
    println!("   评分算法: {}", config.score.r#type);
    println!(
        "   权重: latency_p90={}, success_rate={}, stability={}",
        config.score.weights.latency_p90,
        config.score.weights.success_rate,
        config.score.weights.stability
    );
    println!(
        "   衰减半衰期: {} 小时",
        config.score.decay.half_life_hours
    );

    // 2. 创建评分器
    let scorer = SmartScorer::with_config(config.score.weights, config.score.decay);

    // 3. 模拟节点数据
    let nodes = vec![
        {
            let mut n = NodeHistory::new("🇭🇰 香港 IPLC 01");
            n.add_record(30.0, true); // 30ms 延迟，成功
            n.add_record(28.0, true);
            n.add_record(35.0, true);
            n.add_record(32.0, true);
            n.add_record(29.0, true);
            n
        },
        {
            let mut n = NodeHistory::new("🇭🇰 香港 IPLC 02");
            n.add_record(45.0, true);
            n.add_record(50.0, true);
            n.add_record(42.0, true);
            n.add_record(48.0, true);
            n.add_record(55.0, true);
            n
        },
        {
            let mut n = NodeHistory::new("🇯🇵 日本普通 01");
            n.add_record(150.0, true);
            n.add_record(200.0, true);
            n.add_record(180.0, false); // 偶尔失败
            n.add_record(160.0, true);
            n.add_record(300.0, true); // 偶发高延迟
            n
        },
        {
            let mut n = NodeHistory::new("🇺🇸 美国 01");
            n.add_record(250.0, true);
            n.add_record(280.0, true);
            n.add_record(260.0, true);
            n.add_record(270.0, true);
            n.add_record(255.0, true);
            n
        },
    ];

    // 4. 排名
    println!("\n🏆 节点排名:");
    let ranking = scorer.rank(&nodes);
    for (name, score, rank) in &ranking {
        println!("   #{:2} {} — 评分 {:.1}", rank, name, score);
    }

    // 5. 选择最佳节点
    if let Some(best) = scorer.select_best(&nodes) {
        println!("\n🥇 最佳节点: 「{}」 (评分 {:.1})", best.name, scorer.score(best));
    }

    // 6. 自适应调度演示
    println!("\n⏱️ 自适应测速调度:");
    let scheduler = AdaptiveScheduler::new(config.scheduler);

    for quality in [0.95, 0.8, 0.4, 0.15] {
        let interval = scheduler.next_interval(quality);
        println!(
            "   网络质量 {:.0%} → 下次测速间隔: {} 秒 ({:.1} 分钟)",
            quality,
            interval,
            interval as f64 / 60.0
        );
    }
}
