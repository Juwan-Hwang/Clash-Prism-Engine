//! Performance benchmarks for Prism Engine hot paths.
//!
//! ## Run
//!
//! ```bash
//! cargo bench
//! ```
//!
//! ## Output
//!
//! Results are written to `target/criterion/` and printed to stdout.

use clash_prism_core::executor::{evaluate_predicate, evaluate_transform_expr};
use clash_prism_dsl::DslParser;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Benchmark Data
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Sample proxy items for expression evaluation benchmarks.
fn sample_proxies() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "香港 IPLC 01", "type": "ss",
            "server": "hk1.example.com", "port": 443,
            "tls": true, "udp": true, "network": "ws"
        }),
        serde_json::json!({
            "name": "日本 IPLC 02", "type": "vmess",
            "server": "jp1.example.com", "port": 443,
            "uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "network": "grpc", "tls": true
        }),
        serde_json::json!({
            "name": "美国 Trojan 03", "type": "trojan",
            "server": "us1.example.com", "port": 8443,
            "password": "test123", "udp": true, "sni": "us1.example.com"
        }),
        serde_json::json!({
            "name": "新加坡 SS 04", "type": "ss",
            "server": "sg1.example.com", "port": 8388,
            "cipher": "aes-256-gcm", "plugin": "obfs"
        }),
        serde_json::json!({
            "name": "过期节点 v1", "type": "ss",
            "server": "old.example.com", "port": 80
        }),
    ]
}

/// DSL fixture for parser benchmarks.
fn sample_dsl_fixtures() -> Vec<(&'static str, &'static str)> {
    vec![
        // Simple deep merge
        (
            "simple_merge",
            r#"
dns:
  enable: true
  ipv6: false
  nameserver:
    - https://dns.alidns.com/dns-query
"#,
        ),
        // Override
        (
            "override",
            r#"
tun:
  $override:
    enable: true
    stack: mixed
    auto-route: true
"#,
        ),
        // Composite: filter + transform + remove + prepend
        (
            "composite",
            r#"
proxies:
  $filter: "p.type == 'ss'"
  $transform: "{...p, name: '🇭🇰 ' + p.name}"
  $remove: "p.name.includes('过期')"
  $prepend:
    - name: "手动节点"
      type: ss
      server: 1.2.3.4
      port: 443
"#,
        ),
        // Multiple paths
        (
            "multi_path",
            r#"
dns:
  enable: true
  enhanced-mode: fake-ip
  nameserver:
    - https://dns.alidns.com/dns-query

rules:
  $prepend:
    - DOMAIN-SUFFIX,ads.com,REJECT
    - DOMAIN-KEYWORD,telemetry,REJECT
  $append:
    - GEOIP,CN,DIRECT
    - MATCH,PROXY

tun:
  $override:
    enable: true
    stack: mixed
"#,
        ),
        // With __when__ and __after__
        (
            "scoped",
            r#"
__when__:
  core: mihomo
  platform: [macos, windows]
  profile: /流媒体|解锁/

rules:
  $prepend:
    - DOMAIN-SUFFIX,netflix.com,PROXY
    - DOMAIN-SUFFIX,youtube.com,PROXY

__after__: ["base-rules"]
"#,
        ),
    ]
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Benchmark: DSL Parser
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn bench_dsl_parser(c: &mut Criterion) {
    let fixtures = sample_dsl_fixtures();

    let mut group = c.benchmark_group("dsl_parser");

    for (name, content) in &fixtures {
        group.bench_with_input(
            BenchmarkId::new("parse_str", name),
            content,
            |b, content| {
                b.iter(|| {
                    let _ = black_box(DslParser::parse_str(content, None));
                });
            },
        );
    }

    group.finish();
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Benchmark: Expression Evaluator — Predicates
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn bench_predicate_evaluation(c: &mut Criterion) {
    let proxies = sample_proxies();

    let predicates = [
        // Simple field access
        "p.type == 'ss'",
        // String method
        "p.name.includes('香港')",
        // Logical AND
        "p.type == 'ss' && p.port == 443",
        // Logical OR
        "p.type == 'ss' || p.type == 'vmess'",
        // Negation
        "!p.name.includes('过期')",
        // Complex: multiple conditions
        "p.type == 'ss' && p.port > 400 && p.port < 9000 && p.tls == true",
        // Regex match
        "p.name.match(/香港|日本|美国/)",
        // Chained includes
        "p.name.includes('IPLC') && p.server.includes('example')",
    ];

    let mut group = c.benchmark_group("predicate_eval");

    for expr in &predicates {
        group.bench_with_input(BenchmarkId::new("single", expr), &proxies, |b, proxies| {
            b.iter(|| {
                for proxy in proxies.iter() {
                    let _ = black_box(evaluate_predicate(expr, proxy));
                }
            });
        });
    }

    group.finish();
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Benchmark: Expression Evaluator — Transforms
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn bench_transform_evaluation(c: &mut Criterion) {
    let proxies = sample_proxies();

    let transforms = [
        // Simple spread with rename
        "{...p, name: '🇭🇰 ' + p.name}",
        // Spread with new field
        "{...p, tagged: true, region: 'HK'}",
        // Regex replace
        "{...p, name: p.name.replace(/01$/, '-primary')}",
        // Conditional transform
        "p.type == 'ss' ? {...p, priority: 1} : {...p, priority: 0}",
    ];

    let mut group = c.benchmark_group("transform_eval");

    for expr in &transforms {
        group.bench_with_input(BenchmarkId::new("single", expr), &proxies, |b, proxies| {
            b.iter(|| {
                for proxy in proxies.iter() {
                    let _ = black_box(evaluate_transform_expr(expr, proxy));
                }
            });
        });
    }

    group.finish();
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Benchmark: Full Pipeline (Parse + Execute)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn bench_full_pipeline(c: &mut Criterion) {
    use clash_prism_core::executor::PatchExecutor;

    let dsl_content = r#"
proxies:
  $filter: "p.type == 'ss'"
  $transform: "{...p, name: '🇭🇰 ' + p.name}"
  $remove: "p.name.includes('过期')"
  $prepend:
    - name: "手动节点"
      type: ss
      server: 1.2.3.4
      port: 443

rules:
  $prepend:
    - DOMAIN-SUFFIX,ads.com,REJECT
  $append:
    - MATCH,PROXY

dns:
  enable: true
  enhanced-mode: fake-ip
"#;

    // Pre-generate large proxy list
    let mut large_proxies = Vec::with_capacity(500);
    for i in 0..500 {
        large_proxies.push(serde_json::json!({
            "name": format!("Node-{:03}", i),
            "type": if i % 3 == 0 { "ss" } else if i % 3 == 1 { "vmess" } else { "trojan" },
            "server": format!("node{}.example.com", i),
            "port": 443 + (i % 1000)
        }));
    }

    let mut group = c.benchmark_group("full_pipeline");

    group.bench_function("parse_100_proxies", |b| {
        b.iter(|| {
            let _ = black_box(DslParser::parse_str(dsl_content, None));
        });
    });

    group.bench_function("filter_transform_500_proxies", |b| {
        // Pre-parse patches (only measure execution)
        let patches = DslParser::parse_str(dsl_content, None).unwrap();
        b.iter(|| {
            let base_config = serde_json::json!({"proxies": large_proxies.clone()});
            let mut executor = PatchExecutor::new();
            let _ = black_box(executor.execute(base_config, &patches.iter().collect::<Vec<_>>()));
        });
    });

    group.finish();
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Criterion Configuration
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

criterion_group!(
    benches,
    bench_dsl_parser,
    bench_predicate_evaluation,
    bench_transform_evaluation,
    bench_full_pipeline,
);

criterion_main!(benches);
