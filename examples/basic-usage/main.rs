//! # 基础用法示例
//!
//! 演示 Prism Engine 的核心流程：
//!
//! 1. 解析 .prism.yaml 文件
//! 2. 编译为 Patch IR
//! 3. 执行 Patch 生成最终配置
//! 4. 校验结果

use clash_prism_core::executor::PatchExecutor;
use clash_prism_dsl::DslParser;

fn main() {
    // 1. 解析 DSL 文件
    let dsl_content = r#"
dns:
  enable: true
  ipv6: false
  nameserver:
    - https://dns.alidns.com/dns-query

rules:
  $prepend:
    - DOMAIN-SUFFIX,ads.com,REJECT
  $append:
    - MATCH,PROXY
"#;

    // 2. 编译为 Patch IR
    let patches = match DslParser::parse_str(dsl_content, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("DSL 解析失败: {}", e);
            return;
        }
    };

    println!("✅ 成功编译 {} 个 Patch:", patches.len());
    for (i, patch) in patches.iter().enumerate() {
        println!(
            "  [{}] path={} op={}",
            i + 1,
            patch.path,
            patch.op.display_name()
        );
    }

    // 3. 执行 Patch（从基础配置骨架开始）
    let mut executor = PatchExecutor::new();
    let base_config = serde_json::json!({
        "proxies": [],
        "proxy-groups": [],
        "rules": []
    });

    let final_config = match executor.execute(base_config, &patches) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Patch 执行失败: {}", e);
            return;
        }
    };

    // 4. 输出最终配置
    println!("\n📄 最终配置:");
    println!(
        "{}",
        serde_json::to_string_pretty(&final_config).unwrap_or_default()
    );

    // 5. 输出执行追踪
    println!("\n🔍 执行追踪:");
    for (i, trace) in executor.traces.iter().enumerate() {
        println!(
            "  [{}] {} — {}μs",
            i + 1,
            trace.describe_change(),
            trace.duration_us
        );
    }
}
