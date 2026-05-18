//! # execute_with_write 功能测试套件
//!
//! 本测试套件覆盖 `ScriptRuntime::execute_with_write()` 的完整功能，包括：
//! - 正常执行流程（返回修改后的配置）
//! - 错误处理（脚本错误、语法错误、返回类型错误）
//! - 边界情况（空配置、大配置、特殊字符）
//! - 安全限制（执行时间、内存限制）
//!
//! ## 测试设计原则
//!
//! 1. **单元测试**：测试纯函数逻辑（如日志提取）
//! 2. **集成测试**：测试完整的 JS 执行流程（需要 rquickjs 运行时）
//! 3. **边界值测试**：覆盖极端输入情况
//! 4. **错误路径测试**：确保错误处理正确

use clash_prism_script::{LogLevel, ScriptContext, ScriptRuntime};
use serde_json::json;

// ═══════════════════════════════════════════════════════════
// 辅助函数
// ═══════════════════════════════════════════════════════════

/// 创建测试用的 ScriptRuntime
fn create_test_runtime() -> ScriptRuntime {
    let ctx = ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "test-profile".to_owned(),
    };
    ScriptRuntime::with_context(ctx)
}

/// 创建基础测试配置
fn create_test_config() -> serde_json::Value {
    json!({
        "port": 7890,
        "mixed-port": 7891,
        "proxies": [
            {"name": "Proxy1", "type": "ss", "server": "1.1.1.1", "port": 443},
            {"name": "Proxy2", "type": "vmess", "server": "2.2.2.2", "port": 443}
        ],
        "proxy-groups": [
            {"name": "Auto", "type": "url-test", "proxies": ["Proxy1", "Proxy2"]}
        ],
        "rules": [
            "DOMAIN,google.com,Proxy1",
            "MATCH,Auto"
        ]
    })
}

/// 基础测试：验证普通 execute 可以工作
#[test]
fn test_basic_execute() {
    let runtime = create_test_runtime();
    let script = r#"
        // 简单的脚本，不做任何操作
        var x = 1 + 1;
    "#;
    let config = create_test_config();

    let result = runtime.execute(script, "test-basic", &config);

    if !result.success {
        eprintln!("Error: {:?}", result.error);
        eprintln!("Logs: {:?}", result.logs);
    }
    assert!(result.success, "Script should execute successfully");
}

/// 测试 log API
#[test]
fn test_log_api() {
    let runtime = create_test_runtime();
    let script = r#"
        log.info('Test message');
    "#;
    let config = create_test_config();

    let result = runtime.execute(script, "test-log", &config);

    if !result.success {
        eprintln!("Error: {:?}", result.error);
        eprintln!("Logs: {:?}", result.logs);
    }
    assert!(result.success, "Script should execute successfully");

    let has_msg = result
        .logs
        .iter()
        .any(|log| log.message.contains("Test message"));
    assert!(has_msg, "Should have test message in logs");
}

// ═══════════════════════════════════════════════════════════
// Part 1: 单元测试 - 纯函数逻辑
// ═══════════════════════════════════════════════════════════

/// 测试从日志中提取修改后的配置 - 正常情况
#[test]
fn test_extract_modified_config_success() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.port = 9999;
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "test-extract-success", &config);

    if !result.success {
        eprintln!("Script error: {:?}", result.error);
        eprintln!("Logs: {:?}", result.logs);
    }
    assert!(result.success, "Script should execute successfully");
    assert!(
        result.config_modified,
        "Config should be marked as modified"
    );
    assert!(
        result.modified_config.is_some(),
        "Modified config should be present"
    );

    let modified = result.modified_config.unwrap();
    assert_eq!(modified["port"], 9999, "Port should be modified to 9999");
}

/// 测试脚本未定义 main 函数 - 应该执行失败
#[test]
fn test_extract_modified_config_no_main() {
    let runtime = create_test_runtime();
    let script = r#"
        // No main function defined - just some code
        var x = 1 + 1;
        log.info('No main function here');
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "test-no-main", &config);

    // 新实现：缺少 main 函数会导致执行失败
    assert!(
        !result.success,
        "Script without main() should fail: {:?}",
        result.error
    );
    assert!(
        result
            .error
            .as_ref()
            .map(|e| e.contains("main"))
            .unwrap_or(false),
        "Error should mention main: {:?}",
        result.error
    );
}

/// 测试 main 返回非对象 - 应该执行失败
#[test]
fn test_extract_modified_config_invalid_return() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            return "invalid string";
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "test-invalid-return", &config);

    // 新实现：返回非对象会导致执行失败
    assert!(
        !result.success,
        "Script returning non-object should fail: {:?}",
        result.error
    );
    assert!(
        result
            .error
            .as_ref()
            .map(|e| e.contains("config object"))
            .unwrap_or(false),
        "Error should mention config object: {:?}",
        result.error
    );
}

/// 测试 main 返回 null - 应该执行失败
#[test]
fn test_extract_modified_config_return_null() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            return null;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "test-return-null", &config);

    // 新实现：返回 null 会导致执行失败
    assert!(
        !result.success,
        "Script returning null should fail: {:?}",
        result.error
    );
}

/// 测试日志过滤 - 内部前缀不应出现在最终日志中
#[test]
fn test_internal_logs_filtered() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            log.info('User log message');
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "test-log-filter", &config);

    // 检查内部前缀不出现在日志中
    let has_internal_prefix = result
        .logs
        .iter()
        .any(|log| log.message.contains("__PRISM_MODIFIED_CONFIG__"));
    assert!(
        !has_internal_prefix,
        "Internal prefix should be filtered from logs"
    );

    // 用户日志应该保留
    let has_user_log = result
        .logs
        .iter()
        .any(|log| log.message.contains("User log message"));
    assert!(has_user_log, "User log should be preserved");
}

// ═══════════════════════════════════════════════════════════
// Part 2: 集成测试 - 完整 JS 执行流程
// ═══════════════════════════════════════════════════════════

/// 集成测试：基本配置修改 - 修改端口
#[test]
fn integration_basic_port_modification() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.port = 8080;
            config['mixed-port'] = 8081;
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "integration-port-mod", &config);

    assert!(result.success, "Script should succeed");
    assert!(result.config_modified);

    let modified = result.modified_config.expect("Should have modified config");
    assert_eq!(modified["port"], 8080);
    assert_eq!(modified["mixed-port"], 8081);

    // 其他字段应保持不变
    assert_eq!(modified["proxies"].as_array().unwrap().len(), 2);
}

/// 集成测试：代理组动态创建 - 按地区分组
#[test]
fn integration_dynamic_group_by_region() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            // 创建按地区分组的代理组
            var asiaProxies = [];
            var euProxies = [];
            
            for (var i = 0; i < config.proxies.length; i++) {
                var proxy = config.proxies[i];
                if (proxy.server.includes('1.1')) {
                    asiaProxies.push(proxy.name);
                } else {
                    euProxies.push(proxy.name);
                }
            }
            
            // 添加新的代理组
            config['proxy-groups'].push({
                name: 'Asia-Select',
                type: 'select',
                proxies: asiaProxies
            });
            
            config['proxy-groups'].push({
                name: 'EU-Select', 
                type: 'select',
                proxies: euProxies
            });
            
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "integration-region-groups", &config);

    assert!(result.success);
    let modified = result.modified_config.expect("Should have modified config");

    let groups = modified["proxy-groups"]
        .as_array()
        .expect("proxy-groups should be array");
    assert_eq!(
        groups.len(),
        3,
        "Should have 3 proxy groups (original + 2 new)"
    );

    // 验证新组
    let asia_group = groups.iter().find(|g| g["name"] == "Asia-Select");
    assert!(asia_group.is_some(), "Asia-Select group should exist");
    assert_eq!(asia_group.unwrap()["proxies"].as_array().unwrap().len(), 1);
}

/// 集成测试：规则操作 - 添加和修改规则
#[test]
fn integration_rule_manipulation() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            // 在规则开头添加广告拦截
            config.rules.unshift('DOMAIN-SUFFIX,ads.com,REJECT');
            config.rules.unshift('DOMAIN-KEYWORD,telemetry,REJECT');
            
            // 在末尾添加最终规则
            config.rules.push('GEOIP,CN,DIRECT');
            
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "integration-rules", &config);

    assert!(result.success);
    let modified = result.modified_config.expect("Should have modified config");

    let rules = modified["rules"].as_array().expect("rules should be array");
    assert_eq!(rules.len(), 5, "Should have 5 rules (2 + original 2 + 1)");

    // 验证规则顺序
    assert!(rules[0].as_str().unwrap().contains("telemetry"));
    assert!(rules[1].as_str().unwrap().contains("ads.com"));
    assert!(rules[4].as_str().unwrap().contains("GEOIP"));
}

/// 集成测试：复杂嵌套对象修改
#[test]
fn integration_nested_object_modification() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            // 添加 DNS 配置
            config.dns = {
                enable: true,
                ipv6: false,
                'enhanced-mode': 'fake-ip',
                'fake-ip-range': '198.18.0.1/16',
                nameserver: ['8.8.8.8', '1.1.1.1'],
                fallback: ['tls://1.0.0.1:853']
            };
            
            // 添加 TUN 配置
            config.tun = {
                enable: true,
                stack: 'mixed',
                'dns-hijack': ['8.8.8.8:53']
            };
            
            return config;
        }
    "#;
    let config = json!({"port": 7890}); // 最小配置

    let result = runtime.execute_with_write(script, "integration-nested", &config);

    assert!(result.success);
    let modified = result.modified_config.expect("Should have modified config");

    assert!(modified.get("dns").is_some(), "DNS config should exist");
    assert_eq!(modified["dns"]["enable"], true);
    assert_eq!(modified["dns"]["nameserver"].as_array().unwrap().len(), 2);

    assert!(modified.get("tun").is_some(), "TUN config should exist");
    assert_eq!(modified["tun"]["enable"], true);
}

/// 集成测试：脚本抛出异常
#[test]
fn integration_script_throws_exception() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            throw new Error('Intentional test error');
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "integration-exception", &config);

    // 新实现：脚本抛出异常会导致执行失败
    assert!(
        !result.success,
        "Script throwing exception should fail: {:?}",
        result.error
    );
    assert!(!result.config_modified);
    assert!(result.modified_config.is_none());
}

/// 集成测试：访问未定义变量
#[test]
fn integration_undefined_variable_access() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.port = undefinedVariable;  // This will throw
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "integration-undefined-var", &config);

    // 新实现：访问未定义变量会导致执行失败
    assert!(
        !result.success,
        "Script with undefined variable should fail: {:?}",
        result.error
    );
    assert!(!result.config_modified);
}

// ═══════════════════════════════════════════════════════════
// Part 3: 边界情况测试
// ═══════════════════════════════════════════════════════════

/// 测试空配置对象
#[test]
fn boundary_empty_config() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.port = 7890;
            config.proxies = [];
            return config;
        }
    "#;
    let config = json!({});

    let result = runtime.execute_with_write(script, "boundary-empty", &config);

    assert!(result.success);
    assert!(result.config_modified);

    let modified = result.modified_config.unwrap();
    assert_eq!(modified["port"], 7890);
    assert!(modified["proxies"].as_array().unwrap().is_empty());
}

/// 测试大数组处理
#[test]
fn boundary_large_proxy_array() {
    let runtime = create_test_runtime();

    // 创建包含 100 个代理的配置
    let mut proxies = vec![];
    for i in 0..100 {
        proxies.push(json!({
            "name": format!("Proxy{}", i),
            "type": "ss",
            "server": format!("192.168.1.{}", i),
            "port": 443
        }));
    }
    let config = json!({ "proxies": proxies });

    let script = r#"
        function main(config) {
            // 手动过滤偶数索引的代理
            var filtered = [];
            for (var i = 0; i < config.proxies.length; i++) {
                if (i % 2 === 0) {
                    filtered.push(config.proxies[i]);
                }
            }
            config.proxies = filtered;
            return config;
        }
    "#;

    let result = runtime.execute_with_write(script, "boundary-large-array", &config);

    assert!(result.success, "Script failed: {:?}", result.error);
    let modified = result.modified_config.expect("Should have modified config");
    assert_eq!(modified["proxies"].as_array().unwrap().len(), 50);
}

/// 测试特殊字符处理
#[test]
fn boundary_special_characters() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config['special-key'] = 'value with "quotes" and \\ backslash';
            config.unicode = '中文 🎉 émoji';
            config.multiline = 'line1\nline2\nline3';
            return config;
        }
    "#;
    let config = json!({});

    let result = runtime.execute_with_write(script, "boundary-special-chars", &config);

    assert!(result.success);
    let modified = result.modified_config.unwrap();

    assert!(modified["special-key"].as_str().unwrap().contains("quotes"));
    assert_eq!(modified["unicode"], "中文 🎉 émoji");
    assert!(modified["multiline"].as_str().unwrap().contains("\n"));
}

/// 测试深层嵌套对象
#[test]
fn boundary_deep_nesting() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.level1 = {
                level2: {
                    level3: {
                        level4: {
                            level5: {
                                value: 'deep value'
                            }
                        }
                    }
                }
            };
            return config;
        }
    "#;
    let config = json!({});

    let result = runtime.execute_with_write(script, "boundary-deep-nest", &config);

    assert!(result.success);
    let modified = result.modified_config.unwrap();
    assert_eq!(
        modified["level1"]["level2"]["level3"]["level4"]["level5"]["value"],
        "deep value"
    );
}

// ═══════════════════════════════════════════════════════════
// Part 4: 性能和安全测试
// ═══════════════════════════════════════════════════════════

/// 测试执行时间被记录
#[test]
fn perf_execution_time_recorded() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            // 模拟一些工作
            for (var i = 0; i < 1000; i++) {
                config['key' + i] = i;
            }
            return config;
        }
    "#;
    let config = json!({});

    let result = runtime.execute_with_write(script, "perf-timing", &config);

    assert!(result.success);
    assert!(result.duration_us > 0, "Duration should be recorded");
    // 执行时间应该合理（小于 1 秒 = 1,000,000 微秒）
    assert!(
        result.duration_us < 1_000_000,
        "Execution should complete within 1 second"
    );
}

/// 测试沙箱限制 - eval 在词法层面被阻止
#[test]
fn security_sandbox_restrictions() {
    let runtime = create_test_runtime();

    // 测试 eval 在词法分析阶段就被拒绝
    let script_eval = r#"
        function main(config) {
            eval('config.port = 9999');
            return config;
        }
    "#;

    let config = create_test_config();
    let result = runtime.execute_with_write(script_eval, "security-eval", &config);

    // 脚本应该在执行前就被拒绝（词法安全检查）
    assert!(!result.success, "Script with eval should be rejected");
    assert!(
        result.error.as_ref().unwrap().contains("eval"),
        "Error should mention eval: {:?}",
        result.error
    );
}

/// 测试脚本可以访问 config 和 log API
#[test]
fn security_api_availability() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            // 验证 log API 可用
            log.info('API test');
            
            // 验证 config 是传入的配置
            if (config && config.port === 7890) {
                log.info('Config received correctly');
            }
            
            return config;
        }
    "#;
    let config = create_test_config();

    let result = runtime.execute_with_write(script, "security-api", &config);

    assert!(result.success);
    assert!(result.config_modified);

    let has_api_log = result
        .logs
        .iter()
        .any(|log| log.message.contains("API test"));
    assert!(has_api_log, "API log should be recorded");
}

// ═══════════════════════════════════════════════════════════
// Part 5: 对比测试 - execute vs execute_with_write
// ═══════════════════════════════════════════════════════════

/// 对比测试：execute 返回 patches，execute_with_write 返回修改后的配置
#[test]
fn comparison_execute_vs_execute_with_write() {
    let runtime = create_test_runtime();
    let script = r#"
        function main(config) {
            config.port = 9999;
            return config;
        }
    "#;
    let config = create_test_config();

    // 使用 execute
    let result_normal = runtime.execute(script, "comparison-normal", &config);
    assert!(result_normal.success);
    // execute 不返回修改后的配置，只返回 patches

    // 使用 execute_with_write
    let result_write = runtime.execute_with_write(script, "comparison-write", &config);
    assert!(result_write.success);
    assert!(result_write.config_modified);
    assert!(result_write.modified_config.is_some());
    assert_eq!(result_write.modified_config.unwrap()["port"], 9999);
}
