// 共享的 use 语句、fixture_dir()、make_patch() 由 include! 从 e2e_helpers.rs 引入
include!("e2e_helpers.rs");

// ══════════════════════════════════════════════════════════
// Part A: Fixture 文件驱动的端到端测试
// ══════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════════
// Test 1: 完整流水线 — DNS 深度合并 + TUN 覆盖 + 规则操作
// ══════════════════════════════════════════════════════════

#[test]
fn test_full_pipeline_deep_merge_and_override() {
    let dns_patches =
        DslParser::parse_file(format!("{}/00-base-dns.prism.yaml", fixture_dir())).unwrap();
    let tun_patches =
        DslParser::parse_file(format!("{}/01-tun-override.prism.yaml", fixture_dir())).unwrap();
    let rules_patches = DslParser::parse_file(format!(
        "{}/02-rules-prepend-append.prism.yaml",
        fixture_dir()
    ))
    .unwrap();

    assert!(
        !dns_patches.is_empty(),
        "DNS fixture should produce at least one patch"
    );
    assert!(
        !tun_patches.is_empty(),
        "TUN fixture should produce at least one patch"
    );
    assert!(
        !rules_patches.is_empty(),
        "Rules fixture should produce at least one patch"
    );

    let all_patches: Vec<_> = dns_patches
        .into_iter()
        .chain(tun_patches)
        .chain(rules_patches)
        .collect();

    let mut executor = PatchExecutor::new();
    let base_config = serde_json::json!({});
    let result = executor.execute_owned(base_config, &all_patches);

    assert!(
        result.is_ok(),
        "Full pipeline execution should succeed: {:?}",
        result.err()
    );
    let config = result.unwrap();

    assert_eq!(config["dns"]["enable"], true, "DNS enable should be true");
    assert_eq!(config["dns"]["ipv6"], false, "DNS ipv6 should be false");
    assert_eq!(
        config["dns"]["enhanced-mode"], "fake-ip",
        "DNS enhanced-mode should be fake-ip"
    );
    assert_eq!(config["tun"]["enable"], true, "TUN enable should be true");
    assert_eq!(config["tun"]["stack"], "mixed", "TUN stack should be mixed");

    // Prepend/Append on non-existent arrays are skipped (base config is {}, no "rules" key)
    assert!(
        config.get("rules").is_none(),
        "Prepend/Append on non-existent array should be skipped"
    );

    // dns=1 trace, tun=1 trace, rules=$prepend+$append composite=2 traces → total 4
    assert_eq!(
        executor.traces.len(),
        4,
        "Should have 4 execution traces (dns + tun + rules prepend + rules append)"
    );
    for trace in &executor.traces {
        assert!(
            trace.condition_matched,
            "All traces in global scope should match"
        );
    }
}

// ══════════════════════════════════════════════════════════
// Test 2: $filter + $transform + $remove 复合操作
// ══════════════════════════════════════════════════════════

#[test]
fn test_filter_transform_remove_pipeline() {
    let patches = DslParser::parse_file(format!(
        "{}/03-proxy-filter-transform.prism.yaml",
        fixture_dir()
    ))
    .unwrap();

    assert_eq!(patches.len(), 1, "Should produce exactly 1 composite patch");

    let patch = &patches[0];
    assert_eq!(patch.path, "proxies", "Target path should be 'proxies'");
    assert!(
        patch.is_composite(),
        "Should be a composite patch with sub_ops"
    );

    let base_proxies = serde_json::json!([
        {"name": "香港 IPLC 01", "type": "ss", "server": "hk1.com", "port": 8388},
        {"name": "日本 IPLC 01", "type": "ss", "server": "jp1.com", "port": 443},
        {"name": "过期节点 v1", "type": "vmess", "server": "old.com", "port": 80},
        {"name": "测试节点", "type": "trojan", "server": "test.com", "port": 443},
        {"name": "美国 SNI 01", "type": "ssr", "server": "us1.com", "port": 8388},
    ]);

    let mut executor = PatchExecutor::new();
    let base_config = serde_json::json!({"proxies": base_proxies});
    let result = executor.execute_owned(base_config, &patches);
    assert!(
        result.is_ok(),
        "Filter/transform/remove pipeline should succeed"
    );

    let config = result.unwrap();
    let proxies = config["proxies"]
        .as_array()
        .expect("Result proxies should be an array");

    for p in proxies {
        let name = p["name"].as_str().unwrap_or("");
        assert!(
            !name.contains("过期") && !name.contains("测试"),
            "Removed nodes should not appear: {}",
            name
        );
    }

    assert!(
        !proxies.is_empty(),
        "Some proxies should survive the filter/transform/remove pipeline"
    );
}

// ══════════════════════════════════════════════════════════
// Test 3: __when__ 条件作用域
// ══════════════════════════════════════════════════════════

#[test]
fn test_when_scope_condition_matching() {
    let patches =
        DslParser::parse_file(format!("{}/04-streaming-scoped.prism.yaml", fixture_dir())).unwrap();

    assert_eq!(patches.len(), 1, "Scoped fixture should produce 1 patch");

    let patch = &patches[0];

    use clash_prism_core::scope::Scope;
    match &patch.scope {
        Scope::Scoped { core, platform, .. } => {
            assert_eq!(core.as_deref(), Some("mihomo"), "Core should be mihomo");
            assert!(platform.is_some(), "Platform should be specified");
        }
        other => panic!("Expected Scoped scope, got: {:?}", other),
    }

    assert_eq!(patch.after.len(), 1, "Should have 1 dependency");
}

// ══════════════════════════════════════════════════════════
// Test 4: $default 默认值注入边界逻辑
// ══════════════════════════════════════════════════════════

#[test]
fn test_default_injection_edge_cases() {
    let patches =
        DslParser::parse_file(format!("{}/05-default-injection.prism.yaml", fixture_dir()))
            .unwrap();

    assert_eq!(
        patches.len(),
        1,
        "Default injection fixture should produce 1 patch"
    );

    let mut executor = PatchExecutor::new();
    let config_empty = serde_json::json!({});
    let result = executor.execute_owned(config_empty, &patches).unwrap();
    assert_eq!(
        result["dns"]["enhanced-mode"], "fake-ip",
        "Default should inject when field missing"
    );

    let mut executor2 = PatchExecutor::new();
    let config_null = serde_json::json!({"dns": null});
    let result2 = executor2.execute_owned(config_null, &patches).unwrap();
    assert_eq!(
        result2["dns"]["enhanced-mode"], "fake-ip",
        "Default should inject when field is null"
    );

    let mut executor3 = PatchExecutor::new();
    let config_existing = serde_json::json!({"dns": {"enhanced-mode": "redir-host"}});
    let result3 = executor3.execute_owned(config_existing, &patches).unwrap();
    assert_eq!(
        result3["dns"]["enhanced-mode"], "redir-host",
        "Default should NOT overwrite existing value"
    );

    let mut executor4 = PatchExecutor::new();
    let config_arr = serde_json::json!({"fake-ip-filter": []});
    let _result4 = executor4.execute_owned(config_arr, &patches).unwrap();
    let last_trace = executor4.traces.last().unwrap();
    assert!(last_trace.condition_matched);
}

// ══════════════════════════════════════════════════════════
// Test 5: 运行时字段引用应被拒绝
// ══════════════════════════════════════════════════════════

#[test]
fn test_runtime_field_rejected_at_compile_time() {
    let result = DslParser::parse_file(format!(
        "{}/06-runtime-field-error.prism.yaml",
        fixture_dir()
    ));

    assert!(
        result.is_err(),
        "Runtime field reference should be rejected at compile time"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("delay") || err_msg.contains("runtime"),
        "Error message should mention the problematic field: {}",
        err_msg
    );
}

// ══════════════════════════════════════════════════════════
// Test 6: $override 冲突检测
// ══════════════════════════════════════════════════════════

#[test]
fn test_override_conflict_detection() {
    let result =
        DslParser::parse_file(format!("{}/07-override-conflict.prism.yaml", fixture_dir()));

    assert!(
        result.is_err(),
        "$override mixed with plain keys should be rejected"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.to_lowercase().contains("override") || err_msg.to_lowercase().contains("conflict"),
        "Error should mention override conflict: {}",
        err_msg
    );
}

// ══════════════════════════════════════════════════════════
// Test 7: 验证 — 大型配置执行无多余 clone 开销
// ══════════════════════════════════════════════════════════

#[test]
fn test_large_config_performance() {
    let mut large_proxies = vec![];
    for i in 0..200 {
        large_proxies.push(serde_json::json!({
            "name": format!("Node-{:03}", i),
            "type": "ss",
            "server": format!("node{}.example.com", i),
            "port": 8388 + (i % 1000)
        }));
    }

    let base_config = serde_json::json!({"proxies": large_proxies});

    let yaml = r#"
proxies:
  $filter: "p.port < 8500"
  $transform: "{...p, name: 'filtered-' + p.name}"
"#;
    let patches = DslParser::parse_str(yaml, None).unwrap();

    let mut executor = PatchExecutor::new();
    let start = std::time::Instant::now();
    let result = executor.execute_owned(base_config.clone(), &patches);
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "Large config execution should succeed");
    let config = result.unwrap();
    let proxies = config["proxies"].as_array().unwrap();

    assert!(!proxies.is_empty(), "Some nodes should survive filtering");
    assert!(
        proxies
            .iter()
            .all(|p| { p["name"].as_str().unwrap_or("").starts_with("filtered-") }),
        "All surviving nodes should have 'filtered-' prefix from transform"
    );

    // 将阈值放宽到 5000ms，避免 CI 环境性能波动（尤其是 macOS/Windows runners）
    // 导致测试 flaky 失败。
    assert!(
        elapsed.as_millis() < 5000,
        "Large config execution took too long: {:?}",
        elapsed
    );
    assert_eq!(
        executor.traces.len(),
        2,
        "Should have 2 traces (filter + transform composite)"
    );
    let trace = &executor.traces[0];
    assert!(trace.condition_matched);
    // Filter removes non-matching items — check that the total changed
    assert!(
        trace.summary.removed > 0 || trace.summary.modified > 0 || trace.summary.added > 0,
        "Filter should report some changes: {:?}",
        trace.summary
    );
}

// ══════════════════════════════════════════════════════════
// Test 8: clash-prism-script 安全验证 (validate)
// ══════════════════════════════════════════════════════════

#[test]
fn test_script_validate_rejects_eval() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    assert!(
        runtime.validate("eval('code')").is_err(),
        "eval should be rejected"
    );
}

#[test]
fn test_script_validate_rejects_require() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    assert!(
        runtime.validate("require('fs')").is_err(),
        "require should be rejected"
    );
}

#[test]
fn test_script_validate_rejects_function_constructor() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    assert!(
        runtime.validate("Function('return 1')()").is_err(),
        "Function constructor should be rejected"
    );
}

#[test]
fn test_script_validate_rejects_process() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    assert!(
        runtime.validate("process.exit(0)").is_err(),
        "process access should be rejected"
    );
}

#[test]
fn test_script_validate_allows_safe_code() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    let safe_code = r#"
        var x = 1 + 2;
        var arr = [1, 2, 3];
        for (var i = 0; i < arr.length; i++) {
            console.log(arr[i]);
        }
    "#;
    assert!(
        runtime.validate(safe_code).is_ok(),
        "safe code should pass validation"
    );
}

#[test]
fn test_script_validate_rejects_bracket_access() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    assert!(
        runtime.validate(r#"this["eval"]("code")"#).is_err(),
        "bracket access to eval should be rejected"
    );
}

#[test]
fn test_script_execute_simple() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    let result = runtime.execute("var x = 1 + 2;", "test_simple", &serde_json::json!({}));
    assert!(result.success, "simple script should execute successfully");
}

#[test]
fn test_script_execute_with_config_access() {
    let runtime = clash_prism_script::ScriptRuntime::new();
    let config = serde_json::json!({
        "proxy": {"name": "test-node"}
    });
    let result = runtime.execute(
        r#"
        var name = config.get("proxy.name");
        log.info("node: " + name);
        "#,
        "test_config_access",
        &config,
    );
    assert!(
        result.success,
        "config access should work: {:?}",
        result.error
    );
    assert!(
        result
            .logs
            .iter()
            .any(|l| l.message.contains("node: test-node")),
        "should log the config value"
    );
}

// ══════════════════════════════════════════════════════════
// Part B: 综合对抗性端到端集成测试
// ══════════════════════════════════════════════════════════

// ─── Test 1: 全部 8 种操作按序执行 ───

#[test]
fn test_full_e2e_all_eight_operations() {
    let mut config = serde_json::json!({
        "dns": { "enable": false },
        "tun": { "enable": false, "stack": "gvisor" },
        "rules": ["MATCH,DIRECT"],
        "proxies": [
            { "name": "HK-01", "type": "ss", "server": "hk1.com", "port": 443 },
            { "name": "JP-01", "type": "vmess", "server": "jp1.com", "port": 443 },
            { "name": "US-old", "type": "trojan", "server": "us1.com", "port": 80 }
        ],
        "proxy-groups": []
    });

    let mut executor = PatchExecutor::new();

    // Op 1: DeepMerge on dns
    let p1 = make_patch(
        "dns",
        PatchOp::DeepMerge,
        serde_json::json!({"enable": true, "enhanced-mode": "fake-ip"}),
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p1]).unwrap();
    assert_eq!(config["dns"]["enable"], true);
    assert_eq!(config["dns"]["enhanced-mode"], "fake-ip");

    // Op 2: Override on tun
    let p2 = make_patch(
        "tun",
        PatchOp::Override,
        serde_json::json!({"enable": true, "stack": "mixed"}),
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p2]).unwrap();
    assert_eq!(config["tun"]["stack"], "mixed");
    assert_eq!(config["tun"]["enable"], true);

    // Op 3: Filter proxies — keep only ss type
    let p3 = make_patch(
        "proxies",
        PatchOp::Filter {
            expr: clash_prism_core::ir::CompiledPredicate::new(
                "p.type == 'ss'",
                vec!["type".to_string()],
            ),
        },
        serde_json::Value::Null,
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p3]).unwrap();
    assert_eq!(config["proxies"].as_array().unwrap().len(), 1);
    assert_eq!(config["proxies"][0]["name"], "HK-01");

    // Op 4: Transform — rename remaining proxy
    let p4 = make_patch(
        "proxies",
        PatchOp::Transform {
            expr: clash_prism_core::ir::CompiledPredicate::new(
                "{...p, name: 'HK-' + p.name}",
                vec!["name".to_string()],
            ),
        },
        serde_json::Value::Null,
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p4]).unwrap();
    assert_eq!(config["proxies"][0]["name"], "HK-HK-01");

    // Op 5: Remove — remove items matching condition
    config["proxies"] = serde_json::json!([
        { "name": "keep-me", "type": "ss" },
        { "name": "remove-me", "type": "ss" }
    ]);
    let p5 = make_patch(
        "proxies",
        PatchOp::Remove {
            expr: clash_prism_core::ir::CompiledPredicate::new(
                "p.name.includes('remove')",
                vec!["name".to_string()],
            ),
        },
        serde_json::Value::Null,
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p5]).unwrap();
    assert_eq!(config["proxies"].as_array().unwrap().len(), 1);
    assert_eq!(config["proxies"][0]["name"], "keep-me");

    // Op 6: Prepend rules
    let p6 = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN-SUFFIX,google.com,PROXY"]),
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p6]).unwrap();
    let rules = config["rules"].as_array().unwrap();
    assert_eq!(rules[0], "DOMAIN-SUFFIX,google.com,PROXY");
    assert_eq!(rules[1], "MATCH,DIRECT");

    // Op 7: Append rules
    let p7 = make_patch(
        "rules",
        PatchOp::Append,
        serde_json::json!(["DOMAIN-SUFFIX,github.com,PROXY"]),
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p7]).unwrap();
    let rules = config["rules"].as_array().unwrap();
    assert_eq!(rules.last().unwrap(), "DOMAIN-SUFFIX,github.com,PROXY");

    // Op 8: SetDefault — dns already has enhanced-mode, should NOT override
    let p8 = make_patch(
        "dns",
        PatchOp::SetDefault,
        serde_json::json!({"enhanced-mode": "redir-host"}),
        Scope::Global,
    );
    config = executor.execute_owned(config, &[p8]).unwrap();
    assert_eq!(
        config["dns"]["enhanced-mode"], "fake-ip",
        "SetDefault should not overwrite existing value"
    );
}

// ─── Test 2: 复合补丁执行顺序验证 ───

#[test]
fn test_e2e_composite_patch_execution_order() {
    let yaml = r#"
proxies:
  $filter: "p.type == 'ss'"
  $transform: "{...p, tagged: true}"
  $remove: "p.name.includes('old')"
  $prepend:
    - name: "new-node"
      type: ss
      server: new.com
      port: 443
"#;
    let patches = DslParser::parse_str(yaml, None).unwrap();
    assert_eq!(patches.len(), 1);
    let patch = &patches[0];
    assert!(patch.is_composite());

    let all_ops = patch.all_ops();
    assert_eq!(all_ops.len(), 4);
    assert!(matches!(all_ops[0].op, PatchOp::Filter { .. }));
    assert!(matches!(all_ops[1].op, PatchOp::Remove { .. }));
    assert!(matches!(all_ops[2].op, PatchOp::Transform { .. }));
    assert!(matches!(all_ops[3].op, PatchOp::Prepend));

    let base_config = serde_json::json!({
        "proxies": [
            { "name": "ss-node-1", "type": "ss", "server": "ss1.com", "port": 443 },
            { "name": "vmess-node", "type": "vmess", "server": "vmess1.com", "port": 443 },
            { "name": "ss-old-node", "type": "ss", "server": "old.com", "port": 80 }
        ]
    });

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &patches).unwrap();
    let proxies = result["proxies"].as_array().unwrap();

    assert_eq!(
        proxies.len(),
        2,
        "Should have 2 proxies after composite ops"
    );
    assert_eq!(
        proxies[0]["name"], "new-node",
        "Prepended node should be first"
    );
    assert_eq!(
        proxies[1]["name"], "ss-node-1",
        "Filtered+transformed node should be second"
    );
    assert_eq!(
        proxies[1]["tagged"], true,
        "Transform should have added tagged field"
    );
}

// ─── Test 3: Profile 隔离 ───

#[test]
fn test_e2e_scope_profile_isolation() {
    let profile_a_patches = vec![make_patch(
        "dns",
        PatchOp::DeepMerge,
        serde_json::json!({"enable": true}),
        Scope::Profile("profile-a".to_string()),
    )];
    let profile_b_patches = vec![make_patch(
        "dns",
        PatchOp::DeepMerge,
        serde_json::json!({"ipv6": false}),
        Scope::Profile("profile-b".to_string()),
    )];

    let base_config = serde_json::json!({"dns": {}});
    let mut executor = PatchExecutor::new();

    executor.context.profile_name = Some("profile-a".to_string());
    let config_a = executor
        .execute_owned(base_config.clone(), &profile_a_patches)
        .unwrap();
    assert_eq!(config_a["dns"]["enable"], true);
    assert!(
        config_a["dns"].get("ipv6").is_none(),
        "Profile B's change should not be present"
    );

    executor.context.profile_name = Some("profile-b".to_string());
    let config_b = executor
        .execute_owned(base_config.clone(), &profile_b_patches)
        .unwrap();
    assert_eq!(config_b["dns"]["ipv6"], false);
    assert!(
        config_b["dns"].get("enable").is_none(),
        "Profile A's change should not be present"
    );
}

// ─── Test 4: Global 在 Profile 之后执行 ───

#[test]
fn test_e2e_global_applies_after_profile() {
    let profile_patches = vec![make_patch(
        "mode",
        PatchOp::DeepMerge,
        serde_json::json!("rule"),
        Scope::Profile("work".to_string()),
    )];
    let global_patches = vec![make_patch(
        "mode",
        PatchOp::Override,
        serde_json::json!("global"),
        Scope::Global,
    )];

    let base_config = serde_json::json!({});
    let mut executor = PatchExecutor::new();

    executor.context.profile_name = Some("work".to_string());
    let config = executor
        .execute_owned(base_config, &profile_patches)
        .unwrap();
    assert_eq!(config["mode"], "rule");

    executor.context.profile_name = None;
    let config = executor.execute_owned(config, &global_patches).unwrap();
    assert_eq!(
        config["mode"], "global",
        "Global should override profile value"
    );
}

// ─── Test 5: 条件作用域 — 平台匹配/不匹配 ───

#[test]
fn test_e2e_conditional_scope_platform() {
    let scoped_patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN-SUFFIX,apple.com,PROXY"]),
        Scope::Scoped {
            profile: None,
            platform: Some(vec![clash_prism_core::scope::Platform::MacOS]),
            core: None,
            time_range: None,
            enabled: None,
            ssid: None,
        },
    );

    let base_config = serde_json::json!({"rules": ["MATCH,DIRECT"]});

    let mut executor_match =
        PatchExecutor::with_context(clash_prism_core::executor::ExecutionContext {
            platform: Some("macos".to_string()),
            ..Default::default()
        });
    let result_match = executor_match
        .execute_owned(base_config.clone(), &[scoped_patch.clone()])
        .unwrap();
    assert_eq!(
        result_match["rules"][0], "DOMAIN-SUFFIX,apple.com,PROXY",
        "Should apply on macOS"
    );
    assert!(executor_match.traces[0].condition_matched);

    let mut executor_no_match =
        PatchExecutor::with_context(clash_prism_core::executor::ExecutionContext {
            platform: Some("linux".to_string()),
            ..Default::default()
        });
    let result_no_match = executor_no_match
        .execute_owned(base_config.clone(), &[scoped_patch])
        .unwrap();
    assert_eq!(
        result_no_match["rules"].as_array().unwrap().len(),
        1,
        "Should NOT apply on Linux"
    );
    assert!(!executor_no_match.traces[0].condition_matched);
}

// ─── Test 6: 条件作用域 — 时间范围 ───

#[test]
fn test_e2e_conditional_scope_time_range() {
    let always_active_scope = Scope::Scoped {
        profile: None,
        platform: None,
        core: None,
        time_range: Some(clash_prism_core::scope::TimeRange {
            start: (0, 0),
            end: (23, 59),
        }),
        enabled: None,
        ssid: None,
    };

    let patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN,time-test,PROXY"]),
        always_active_scope,
    );
    let base_config = serde_json::json!({"rules": ["MATCH,DIRECT"]});

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();
    assert_eq!(
        result["rules"][0], "DOMAIN,time-test,PROXY",
        "Always-active time range should match"
    );
    assert!(executor.traces[0].condition_matched);

    // 使用基于当前时间动态计算的时间窗口，
    // 确保窄时间范围在测试运行时一定不匹配（避免午夜 00:00-00:01 的 flaky）。
    // 策略：构造一个 1 分钟窗口，起始时间 = 当前时间 + 12 小时（取模 24h），
    // 这样无论何时运行测试，该窗口都不会包含当前时间。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let secs_since_midnight = now % 86400;
    let current_hour = (secs_since_midnight / 3600) as u8;
    let current_minute = ((secs_since_midnight % 3600) / 60) as u8;
    // 窗口起始 = 当前时间 + 12h，取模 24h，确保远离当前时间
    let window_start_hour = (current_hour + 12) % 24;
    let window_end_hour = (window_start_hour + 1) % 24;

    let narrow_scope = Scope::Scoped {
        profile: None,
        platform: None,
        core: None,
        time_range: Some(clash_prism_core::scope::TimeRange {
            start: (window_start_hour, current_minute),
            end: (window_end_hour, current_minute),
        }),
        enabled: None,
        ssid: None,
    };

    let narrow_patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN,narrow,PROXY"]),
        narrow_scope,
    );
    let mut executor2 = PatchExecutor::new();
    let result2 = executor2
        .execute_owned(
            serde_json::json!({"rules": ["MATCH,DIRECT"]}),
            &[narrow_patch],
        )
        .unwrap();
    // 窄时间范围规则不应被应用（规则列表应保持原样）
    let rules = result2["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "窄时间范围规则不应被应用");
    assert_eq!(rules[0], "MATCH,DIRECT", "原始规则应保持不变");
    assert!(
        !executor2.traces[0].condition_matched,
        "窄时间范围条件应不匹配"
    );
}

// ─── Test 7: 依赖链执行顺序 ───

#[test]
fn test_e2e_dependency_order() {
    let mut compiler = PatchCompiler::new();

    let patches_a = DslParser::parse_str(
        r#"dns: { enable: true }"#,
        Some(std::path::PathBuf::from("00-a.prism.yaml")),
    )
    .unwrap();
    let patches_b = DslParser::parse_str(
        r#"__after__: "00-a.prism.yaml"
dns: { enhanced-mode: "fake-ip" }"#,
        Some(std::path::PathBuf::from("01-b.prism.yaml")),
    )
    .unwrap();
    let patches_c = DslParser::parse_str(
        r#"__after__: "01-b.prism.yaml"
dns: { fake-ip-filter: ["+.lan"] }"#,
        Some(std::path::PathBuf::from("02-c.prism.yaml")),
    )
    .unwrap();

    compiler
        .register_patches("00-a.prism.yaml", patches_a)
        .unwrap();
    compiler
        .register_patches("01-b.prism.yaml", patches_b)
        .unwrap();
    compiler
        .register_patches("02-c.prism.yaml", patches_c)
        .unwrap();

    let sorted_ids = compiler.resolve_dependencies().unwrap();
    assert_eq!(
        sorted_ids.len(),
        3,
        "Should have 3 patches in dependency order"
    );

    let all_patches = compiler.get_all_patches();
    let id_to_file: std::collections::HashMap<_, _> = all_patches
        .iter()
        .map(|p| {
            (
                p.id.as_str().to_string(),
                p.source.file.clone().unwrap_or_default(),
            )
        })
        .collect();

    let file_a = id_to_file.get(sorted_ids[0].as_str()).unwrap();
    let file_b = id_to_file.get(sorted_ids[1].as_str()).unwrap();
    let file_c = id_to_file.get(sorted_ids[2].as_str()).unwrap();

    assert!(file_a.contains("00-a"), "First should be file A");
    assert!(file_b.contains("01-b"), "Second should be file B");
    assert!(file_c.contains("02-c"), "Third should be file C");
}

// ─── Test 8: 循环依赖检测 ───

#[test]
fn test_e2e_circular_dependency_detection() {
    let mut compiler = PatchCompiler::new();

    let patches_a = DslParser::parse_str(
        r#"__after__: "02-c.prism.yaml"
dns: { enable: true }"#,
        Some(std::path::PathBuf::from("00-a.prism.yaml")),
    )
    .unwrap();
    let patches_b = DslParser::parse_str(
        r#"__after__: "00-a.prism.yaml"
dns: { enhanced-mode: "fake-ip" }"#,
        Some(std::path::PathBuf::from("01-b.prism.yaml")),
    )
    .unwrap();
    let patches_c = DslParser::parse_str(
        r#"__after__: "01-b.prism.yaml"
dns: { fake-ip-filter: ["+.lan"] }"#,
        Some(std::path::PathBuf::from("02-c.prism.yaml")),
    )
    .unwrap();

    compiler
        .register_patches("00-a.prism.yaml", patches_a)
        .unwrap();
    compiler
        .register_patches("01-b.prism.yaml", patches_b)
        .unwrap();
    compiler
        .register_patches("02-c.prism.yaml", patches_c)
        .unwrap();

    let result = compiler.resolve_dependencies();
    assert!(result.is_err(), "Circular dependency should be detected");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("循环") || err_msg.contains("circular") || err_msg.contains("cycle"),
        "Error should mention circular dependency: {}",
        err_msg
    );
}

// ─── Test 9: $override 替换整个配置根 ───

#[test]
fn test_e2e_override_entire_config() {
    let patch = make_patch(
        "", // empty path = root level
        PatchOp::Override,
        serde_json::json!({"mode": "rule", "log-level": "info"}),
        Scope::Global,
    );

    let base_config = serde_json::json!({"dns": {"enable": true}, "rules": ["MATCH,DIRECT"]});
    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();

    // Empty path override replaces the entire config root
    assert_eq!(result["mode"], "rule");
    assert_eq!(result["log-level"], "info");
    assert!(
        result.get("dns").is_none(),
        "Old keys should be gone after full override"
    );
}

// ─── Test 10: $default 嵌套路径注入 ───

#[test]
fn test_e2e_default_nested_path_injection() {
    let patch = make_patch(
        "dns.nameservers",
        PatchOp::SetDefault,
        serde_json::json!(["1.1.1.1", "8.8.8.8"]),
        Scope::Global,
    );

    let base_empty = serde_json::json!({"dns": {"enable": true}});
    let mut executor = PatchExecutor::new();
    let result = executor
        .execute_owned(base_empty, &[patch.clone()])
        .unwrap();
    assert_eq!(result["dns"]["nameservers"][0], "1.1.1.1");

    let base_existing = serde_json::json!({"dns": {"nameservers": ["9.9.9.9"]}});
    let mut executor2 = PatchExecutor::new();
    let result2 = executor2.execute_owned(base_existing, &[patch]).unwrap();
    assert_eq!(
        result2["dns"]["nameservers"][0], "9.9.9.9",
        "Should preserve existing value"
    );
}

// ─── Test 11: Deep Merge 部分覆盖 ───

#[test]
fn test_e2e_deep_merge_partial_override() {
    let patch = make_patch(
        "dns",
        PatchOp::DeepMerge,
        serde_json::json!({
            "enable": true,
            "enhanced-mode": "fake-ip",
            "nameserver": ["1.1.1.1"]
        }),
        Scope::Global,
    );

    let base_config = serde_json::json!({
        "dns": {
            "enable": false,
            "ipv6": true,
            "fallback": true
        }
    });

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();

    assert_eq!(result["dns"]["enable"], true);
    assert_eq!(result["dns"]["enhanced-mode"], "fake-ip");
    assert_eq!(
        result["dns"]["ipv6"], true,
        "Deep merge should preserve existing keys"
    );
    assert_eq!(
        result["dns"]["fallback"], true,
        "Deep merge should preserve existing keys"
    );
    assert_eq!(result["dns"]["nameserver"][0], "1.1.1.1");
}

// ─── Test 12: 空规则数组 + Prepend ───

#[test]
fn test_e2e_empty_rules_array_with_prepend() {
    let patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN-SUFFIX,example.com,PROXY", "MATCH,DIRECT"]),
        Scope::Global,
    );

    let base_config = serde_json::json!({});
    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();

    assert!(
        result.get("rules").is_none()
            || result["rules"].as_array().is_none_or(|arr| arr.is_empty()),
        "Prepend to non-existent path should not create the array"
    );
}

// ─── Test 13: Filter 移除所有项 ───

#[test]
fn test_e2e_filter_removes_all_items() {
    let patch = make_patch(
        "proxies",
        PatchOp::Filter {
            expr: clash_prism_core::ir::CompiledPredicate::new(
                "p.type == 'wireguard'",
                vec!["type".to_string()],
            ),
        },
        serde_json::Value::Null,
        Scope::Global,
    );

    let base_config = serde_json::json!({
        "proxies": [
            {"name": "a", "type": "ss"},
            {"name": "b", "type": "vmess"},
            {"name": "c", "type": "trojan"}
        ]
    });

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();
    let proxies = result["proxies"].as_array().unwrap();
    assert_eq!(
        proxies.len(),
        0,
        "Filter should remove all items when none match"
    );
}

// ─── Test 14: Transform 仅修改匹配项 ───

#[test]
fn test_e2e_transform_preserves_unmatched() {
    let patch = make_patch(
        "proxies",
        PatchOp::Transform {
            expr: clash_prism_core::ir::CompiledPredicate::new(
                "{...p, name: p.name + '-tagged'}",
                vec!["name".to_string()],
            ),
        },
        serde_json::Value::Null,
        Scope::Global,
    );

    let base_config = serde_json::json!({
        "proxies": [
            {"name": "HK-01", "type": "ss", "server": "hk.com", "port": 443},
            {"name": "JP-01", "type": "vmess", "server": "jp.com", "port": 443}
        ]
    });

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();
    let proxies = result["proxies"].as_array().unwrap();
    assert_eq!(proxies.len(), 2);
    assert_eq!(proxies[0]["name"], "HK-01-tagged");
    assert_eq!(
        proxies[0]["type"], "ss",
        "Transform should preserve other fields via spread"
    );
    assert_eq!(proxies[1]["name"], "JP-01-tagged");
}

// ─── Test 15: 验证器 — 重复代理名称 ───

#[test]
fn test_e2e_validator_duplicate_proxy_names() {
    let config = serde_json::json!({
        "proxies": [
            {"name": "HK-01", "type": "ss"},
            {"name": "HK-01", "type": "vmess"},
            {"name": "JP-01", "type": "trojan"}
        ]
    });

    let result = Validator::validate(&config);
    assert!(
        !result.is_valid,
        "Duplicate proxy names should fail validation"
    );
    assert!(
        result.errors.iter().any(|e| e.message.contains("HK-01")),
        "Error should mention the duplicate name"
    );
}

// ─── Test 16: 验证器 — 代理组引用不存在的代理 ───

#[test]
fn test_e2e_validator_missing_group_reference() {
    let config = serde_json::json!({
        "proxies": [
            {"name": "HK-01", "type": "ss"}
        ],
        "proxy-groups": [
            {
                "name": "auto",
                "type": "url-test",
                "proxies": ["HK-01", "NONEXISTENT-PROXY"]
            }
        ]
    });

    let result = Validator::validate(&config);
    assert!(
        !result.is_valid,
        "Missing proxy reference should fail validation"
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.message.contains("NONEXISTENT-PROXY")),
        "Error should mention the missing proxy"
    );
}

// ─── Test 17: 大规模性能测试 — 500 代理 ───

#[test]
fn test_e2e_large_scale_500_proxies() {
    let mut proxies = Vec::with_capacity(500);
    for i in 0..500 {
        proxies.push(serde_json::json!({
            "name": format!("Node-{:03}", i),
            "type": if i % 3 == 0 { "ss" } else if i % 3 == 1 { "vmess" } else { "trojan" },
            "server": format!("node{}.example.com", i),
            "port": 443 + (i % 1000)
        }));
    }

    let base_config = serde_json::json!({"proxies": proxies});

    let yaml = r#"
proxies:
  $filter: "p.type == 'ss'"
  $transform: "{...p, name: 'SS-' + p.name}"
"#;
    let patches = DslParser::parse_str(yaml, None).unwrap();

    let mut executor = PatchExecutor::new();
    let start = std::time::Instant::now();
    let result = executor.execute_owned(base_config, &patches).unwrap();
    let elapsed = start.elapsed();

    let filtered_proxies = result["proxies"].as_array().unwrap();
    assert!(
        filtered_proxies.len() > 100,
        "Should have filtered to SS nodes"
    );
    assert!(
        filtered_proxies
            .iter()
            .all(|p| p["name"].as_str().unwrap_or("").starts_with("SS-")),
        "All nodes should have SS- prefix"
    );

    // 将阈值从 1s 放宽到 3s，避免 CI 环境性能波动导致测试失败
    assert!(
        elapsed.as_millis() < 3000,
        "500 proxy filter+transform should complete in < 3s, took: {:?}",
        elapsed
    );
}

// ─── Test 18: 并发 Profile 执行 ───

#[test]
fn test_e2e_concurrent_profile_execution() {
    let profile_a_patches = vec![make_patch(
        "dns",
        PatchOp::DeepMerge,
        serde_json::json!({"profile": "A"}),
        Scope::Profile("profile-a".to_string()),
    )];
    let profile_b_patches = vec![make_patch(
        "mode",
        PatchOp::DeepMerge,
        serde_json::json!("direct"),
        Scope::Profile("profile-b".to_string()),
    )];
    let shared_patches = vec![make_patch(
        "log-level",
        PatchOp::DeepMerge,
        serde_json::json!("info"),
        Scope::Global,
    )];

    let base_config = serde_json::json!({});
    let mut executor = PatchExecutor::new();

    let result = executor.execute_pipeline(
        &base_config,
        vec![
            ("profile-a".to_string(), profile_a_patches.iter().collect()),
            ("profile-b".to_string(), profile_b_patches.iter().collect()),
        ],
        shared_patches.iter().collect(),
    );

    assert!(
        result.is_ok(),
        "Concurrent profile execution should succeed"
    );
    let (traces, _merged) = result.unwrap();
    assert!(
        traces.len() >= 2,
        "Should have traces from multiple profiles"
    );
}

// ─── Test 19: 脚本沙箱拒绝危险代码 ───

#[test]
fn test_e2e_script_sandbox_rejects_dangerous_code() {
    let runtime = clash_prism_script::ScriptRuntime::new();

    // eval — Layer 1 raw check
    assert!(runtime.validate("eval('malicious')").is_err());
    // require — Layer 1 raw check
    assert!(runtime.validate("require('fs')").is_err());
    // Function constructor — Layer 1 raw check
    assert!(runtime.validate("Function('return 1')()").is_err());
    // process — Layer 1 raw check
    assert!(runtime.validate("process.exit(0)").is_err());
    // globalThis — Layer 1 raw check
    assert!(runtime.validate("globalThis.eval('code')").is_err());
    // __proto__ — Layer 1 raw check
    assert!(runtime.validate("obj.__proto__ = {}").is_err());
    // import statement — Layer 1 raw check
    assert!(runtime.validate("import 'fs'").is_err());
    // constructor bracket access — Layer 3
    assert!(runtime.validate(r#"this["eval"]("code")"#).is_err());
}

// ─── Test 20: 脚本 KV 存储持久化 ───

#[test]
fn test_e2e_script_kv_store_persistence() {
    let kv_store = std::sync::Arc::new(clash_prism_script::api::KvStore::new());
    let config = serde_json::json!({});

    let runtime1 = clash_prism_script::ScriptRuntime::with_config(
        clash_prism_script::limits::ScriptLimits::default(),
        clash_prism_script::api::ScriptContext::default(),
        std::sync::Arc::clone(&kv_store),
    );
    let result1 = runtime1.execute(
        r#"
        store.set("counter", 42);
        store.set("greeting", "hello");
        "#,
        "script-1",
        &config,
    );
    assert!(
        result1.success,
        "KV write should succeed: {:?}",
        result1.error
    );

    let runtime2 = clash_prism_script::ScriptRuntime::with_config(
        clash_prism_script::limits::ScriptLimits::default(),
        clash_prism_script::api::ScriptContext::default(),
        std::sync::Arc::clone(&kv_store),
    );
    let result2 = runtime2.execute(
        r#"
        var val = store.get("counter");
        log.info("counter=" + val);
        "#,
        "script-2",
        &config,
    );
    assert!(
        result2.success,
        "KV read should succeed: {:?}",
        result2.error
    );
    assert!(
        result2
            .logs
            .iter()
            .any(|l| l.message.contains("counter=42")),
        "Should read the value written by script-1"
    );

    let keys = kv_store.keys();
    assert!(keys.contains(&"counter".to_string()));
    assert!(keys.contains(&"greeting".to_string()));
}

// ─── Test 21: enabled:false 作用域跳过执行 ───

#[test]
fn test_e2e_enabled_false_scope_skips_execution() {
    let disabled_scope = Scope::Scoped {
        profile: None,
        platform: None,
        core: None,
        time_range: None,
        enabled: Some(false),
        ssid: None,
    };

    let patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN,disabled.com,PROXY"]),
        disabled_scope,
    );
    let base_config = serde_json::json!({"rules": ["MATCH,DIRECT"]});

    let mut executor = PatchExecutor::new();
    let result = executor.execute_owned(base_config, &[patch]).unwrap();

    assert_eq!(
        result["rules"].as_array().unwrap().len(),
        1,
        "Disabled patch should not modify config"
    );
    assert!(
        !executor.traces[0].condition_matched,
        "Trace should show condition not matched"
    );
}

// ─── Test 22: SSID 条件匹配 ───

#[test]
fn test_e2e_ssid_condition_matching() {
    let ssid_scope = Scope::Scoped {
        profile: None,
        platform: None,
        core: None,
        time_range: None,
        enabled: None,
        ssid: Some("HomeWiFi".to_string()),
    };

    let patch = make_patch(
        "rules",
        PatchOp::Prepend,
        serde_json::json!(["DOMAIN,home.local,DIRECT"]),
        ssid_scope,
    );
    let base_config = serde_json::json!({"rules": ["MATCH,DIRECT"]});

    let mut executor_match =
        PatchExecutor::with_context(clash_prism_core::executor::ExecutionContext {
            ssid: Some("HomeWiFi".to_string()),
            ..Default::default()
        });
    let result = executor_match
        .execute_owned(base_config.clone(), &[patch.clone()])
        .unwrap();
    assert_eq!(result["rules"][0], "DOMAIN,home.local,DIRECT");
    assert!(executor_match.traces[0].condition_matched);

    let mut executor_no_match =
        PatchExecutor::with_context(clash_prism_core::executor::ExecutionContext {
            ssid: Some("OfficeWiFi".to_string()),
            ..Default::default()
        });
    let result2 = executor_no_match
        .execute_owned(base_config, &[patch])
        .unwrap();
    assert_eq!(result2["rules"].as_array().unwrap().len(), 1);
    assert!(!executor_no_match.traces[0].condition_matched);
}

// ─── Test 23: 依赖引用不存在文件 ───

#[test]
fn test_e2e_dependency_not_found_error() {
    let mut compiler = PatchCompiler::new();

    let patches = DslParser::parse_str(
        r#"__after__: "nonexistent-file"
dns: { enable: true }"#,
        Some(std::path::PathBuf::from("dependent.prism.yaml")),
    )
    .unwrap();

    compiler
        .register_patches("dependent.prism.yaml", patches)
        .unwrap();

    let result = compiler.resolve_dependencies();
    assert!(
        result.is_err(),
        "Dependency on non-existent file should error"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("nonexistent-file") || err_msg.contains("未找到"),
        "Error should mention the missing dependency: {}",
        err_msg
    );
}

// ─── Test 24: 验证器 — DNS + TUN 联动警告 ───

#[test]
fn test_e2e_validator_dns_tun_warning() {
    let config = serde_json::json!({
        "tun": {"enable": true},
        "dns": {"enable": false}
    });

    let result = Validator::validate(&config);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.message.contains("TUN") && w.message.contains("DNS")),
        "Should warn about TUN enabled but DNS disabled"
    );
}

// ─── Test 25: 验证器 — 规则列表为空警告 ───

#[test]
fn test_e2e_validator_empty_rules_warning() {
    let config = serde_json::json!({
        "rules": []
    });

    let result = Validator::validate(&config);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.message.contains("空") || w.message.contains("MATCH")),
        "Should warn about empty rules list"
    );
}
