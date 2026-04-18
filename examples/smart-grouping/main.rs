//! # 智能分组脚本示例
//!
//! 对应架构文档 §5.3 — 按地区自动分组代理节点
//!
//! ```typescript
//! // smart-grouping.js
//! // @prism-scope: subscribe
//! // @prism-permissions: store
//!
//! function main(ctx: PrismContext) {
//!     const { proxies, groups, log } = ctx.utils;
//!
//!     // 过滤无效节点
//!     proxies.remove(p => !p.server || p.port <= 0 || p.port > 65535);
//!
//!     // 重命名（添加国旗前缀）
//!     proxies.rename(/^港/, "🇭🇰 香港");
//!     proxies.rename(/^日/, "🇯🇵 日本");
//!     proxies.rename(/^美/, "🇺🇸 美国");
//!     proxies.rename(/^新/, "🇸🇬 新加坡");
//!
//!     // 按国旗分组
//!     const regions = proxies.groupBy(/^(🇭🇰|🇯🇵|🇺🇸|🇸🇬)/);
//!
//!     for (const [region, nodes] of regions) {
//!         const groupName = `${region} Auto`;
//!
//!         groups.create({
//!             name: groupName,
//!             type: "url-test",
//!             proxies: nodes.map(p => p.name),
//!             url: "http://www.gstatic.com/generate_204",
//!             interval: 300,
//!             tolerance: 50,
//!         });
//!
//!         groups.addProxy("PROXY", groupName);
//!         log.info(`Created group: ${groupName} (${nodes.length} nodes)`);
//!     }
//! }
//! ```

// Rust 端等价实现（使用 clash-prism-script 完整 API）
use clash_prism_script::{ScriptRuntime, ScriptContext};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 模拟一个包含原始节点的配置
    let mut config = serde_json::json!({
        "proxies": [
            {"name": "香港01", "type": "ss", "server": "hk1.example.com", "port": 8388},
            {"name": "香港02", "type": "ss", "server": "hk2.example.com", "port": 8388},
            {"name": "日本01", "type": "vmess", "server": "jp1.example.com", "port": 443},
            {"name": "美国01", "type": "trojan", "server": "us1.example.com", "port": 443},
            {"name": "新加坡01", "type": "ss", "server": "sg1.example.com", "port": 8388},
        ],
        "proxy-groups": [],
        "rules": []
    });

    println!("Original node count: {}", config["proxies"].as_array().unwrap().len());

    // 使用脚本运行时执行智能分组 JS 脚本
    let script_ctx = ScriptContext {
        core_type: "mihomo".to_string(),
        core_version: "1.0.0".to_string(),
        platform: std::env::consts::OS.to_string(),
        profile_name: "default".to_string(),
    };

    let runtime = ScriptRuntime::with_context(script_ctx);

    // 智能分组脚本（等价于架构 §5.3 的 TypeScript 示例）
    let script = r#"
        const { proxies, groups, log } = utils;

        // Rename: add flag prefix
        proxies.rename(/^港/, "HK Hong Kong");
        proxies.rename(/^日/, "JP Japan");
        proxies.rename(/^美/, "US America");
        proxies.rename(/^新/, "SG Singapore");

        // Group by region flag
        const regions = proxies.groupBy(/^(HK|JP|US|SG)/);

        for (const [region, nodes] of regions) {
            if (nodes.length === 0) continue;
            const groupName = region + " Auto";

            groups.create({
                name: groupName,
                type: "url-test",
                proxies: nodes.map(p => p.name),
                url: "http://www.gstatic.com/generate_204",
                interval: 300,
                tolerance: 50,
            });

            groups.addProxy("PROXY", groupName);
            log.info("Created group: " + groupName + " (" + nodes.length + " nodes)");
        }

        log.info("Smart grouping done!");
    "#;

    let result = runtime.execute(script, "smart-grouping.js", &config);

    match result.success {
        true => {
            println!("Script executed successfully in {}us", result.duration_us);
            for log_entry in &result.logs {
                println!("[{}] {}", log_entry.level, log_entry.message);
            }
            // 输出脚本生成的 Patch
            if !result.patches.is_empty() {
                println!("\nGenerated {} patches:", result.patches.len());
                for patch in &result.patches {
                    println!("  - [{:?}] {} {:?}", patch.op, patch.path, patch.value);
                }
            }
        }
        false => {
            println!("Script failed: {}", result.error.unwrap_or_default());
        }
    }

    println!("\nFinal config:");
    println!("{}", serde_json::to_string_pretty(&config).unwrap_or_default());

    Ok(())
}
