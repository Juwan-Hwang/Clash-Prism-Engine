//! # execute_with_write 集成测试
//!
//! 真正的集成测试：测试 script 模块与 core 模块的协作
//! 验证脚本生成的修改配置可以被 Patch 系统正确处理

use clash_prism_core::executor::PatchExecutor;
use clash_prism_core::ir::{Patch, PatchOp, PatchSource, Scope};
use clash_prism_script::{ScriptContext, ScriptRuntime};
use serde_json::json;

/// 集成测试：脚本修改配置后，通过 Patch 系统应用
#[test]
fn integration_script_patch_pipeline() {
    // Step 1: 使用 script 生成修改后的配置
    let runtime = ScriptRuntime::with_context(ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "integration-test".to_owned(),
    });

    let script = r#"
        function main(config) {
            // 添加 DNS 配置
            config.dns = {
                enable: true,
                ipv6: false,
                'enhanced-mode': 'fake-ip',
                nameserver: ['8.8.8.8', '1.1.1.1']
            };
            
            // 修改端口
            config.port = 9090;
            
            // 添加代理
            config.proxies = [
                {name: 'TestProxy', type: 'ss', server: 'test.com', port: 443}
            ];
            
            return config;
        }
    "#;

    let base_config = json!({
        "port": 7890,
        "mixed-port": 7891
    });

    let script_result = runtime.execute_with_write(script, "integration-pipeline", &base_config);
    
    assert!(script_result.success, "Script should execute successfully");
    assert!(script_result.config_modified, "Config should be modified");
    
    let modified_config = script_result.modified_config.expect("Should have modified config");
    
    // Step 2: 验证修改后的配置可以被序列化和反序列化
    let config_yaml = serde_yaml::to_string(&modified_config).expect("Should serialize to YAML");
    let reparsed: serde_json::Value = serde_yaml::from_str(&config_yaml).expect("Should parse back from YAML");
    
    assert_eq!(reparsed["port"], 9090);
    assert_eq!(reparsed["dns"]["enable"], true);
    assert_eq!(reparsed["proxies"].as_array().unwrap().len(), 1);
    
    // Step 3: 使用 Patch 系统进一步修改
    let mut executor = PatchExecutor::new();
    
    // 创建一个 Patch 来添加更多配置
    let patch = Patch::new(
        PatchSource::builtin(),
        Scope::Global,
        "tun",
        PatchOp::Override,
        json!({
            "enable": true,
            "stack": "mixed"
        }),
    );
    
    let final_result = executor.execute_owned(reparsed, &[patch]);
    assert!(final_result.is_ok(), "Patch execution should succeed");
    
    let final_config = final_result.unwrap();
    assert_eq!(final_config["port"], 9090);  // 脚本修改的值
    assert_eq!(final_config["dns"]["enable"], true);  // 脚本添加的 DNS
    assert_eq!(final_config["tun"]["enable"], true);  // Patch 添加的 TUN
}

/// 集成测试：多脚本顺序执行
#[test]
fn integration_multiple_scripts_sequential() {
    let runtime = ScriptRuntime::with_context(ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "multi-script-test".to_owned(),
    });

    let base_config = json!({"port": 7890});

    // 脚本 1: 添加 DNS
    let script1 = r#"
        function main(config) {
            config.dns = {enable: true, nameserver: ['8.8.8.8']};
            return config;
        }
    "#;
    
    let result1 = runtime.execute_with_write(script1, "script-1", &base_config);
    assert!(result1.success);
    let config1 = result1.modified_config.unwrap();
    assert!(config1.get("dns").is_some());
    
    // 脚本 2: 在脚本 1 的基础上添加 TUN
    let script2 = r#"
        function main(config) {
            config.tun = {enable: true};
            // 修改 DNS
            config.dns['enhanced-mode'] = 'fake-ip';
            return config;
        }
    "#;
    
    let result2 = runtime.execute_with_write(script2, "script-2", &config1);
    assert!(result2.success);
    let config2 = result2.modified_config.unwrap();
    
    // 验证两个脚本的修改都生效
    assert!(config2.get("dns").is_some());
    assert_eq!(config2["dns"]["nameserver"][0], "8.8.8.8");  // 脚本 1 的修改
    assert_eq!(config2["dns"]["enhanced-mode"], "fake-ip");  // 脚本 2 的修改
    assert!(config2.get("tun").is_some());  // 脚本 2 的添加
}

/// 集成测试：脚本错误不应破坏后续处理
#[test]
fn integration_script_error_isolation() {
    let runtime = ScriptRuntime::with_context(ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "error-isolation-test".to_owned(),
    });

    let base_config = json!({"port": 7890, "dns": {"enable": false}});

    // 脚本 1: 正常执行
    let script1 = r#"
        function main(config) {
            config.dns.enable = true;
            return config;
        }
    "#;
    
    let result1 = runtime.execute_with_write(script1, "ok-script", &base_config);
    assert!(result1.success);
    assert!(result1.config_modified);
    
    // 脚本 2: 抛出错误
    let script2 = r#"
        function main(config) {
            throw new Error('Intentional error');
        }
    "#;
    
    let result2 = runtime.execute_with_write(script2, "error-script", &result1.modified_config.unwrap());
    // 新实现：脚本抛出异常会导致执行失败
    assert!(!result2.success);
    assert!(!result2.config_modified);
    
    // 脚本 3: 应该能继续正常执行
    let script3 = r#"
        function main(config) {
            config.port = 9999;
            return config;
        }
    "#;
    
    let result3 = runtime.execute_with_write(script3, "recovery-script", &result1.modified_config.unwrap());
    assert!(result3.success);
    assert!(result3.config_modified);
    assert_eq!(result3.modified_config.unwrap()["port"], 9999);
}

/// 集成测试：复杂真实场景 - 代理分组和规则处理
#[test]
fn integration_real_world_scenario() {
    let runtime = ScriptRuntime::with_context(ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "real-world-test".to_owned(),
    });

    // 模拟一个真实的订阅配置
    let subscription_config = json!({
        "port": 7890,
        "socks-port": 7891,
        "proxies": [
            {"name": "🇭🇰 HK-1", "type": "ss", "server": "hk1.example.com", "port": 443},
            {"name": "🇭🇰 HK-2", "type": "ss", "server": "hk2.example.com", "port": 443},
            {"name": "🇯🇵 JP-1", "type": "vmess", "server": "jp1.example.com", "port": 443},
            {"name": "🇺🇸 US-1", "type": "ss", "server": "us1.example.com", "port": 443},
            {"name": "🇸🇬 SG-1", "type": "trojan", "server": "sg1.example.com", "port": 443}
        ],
        "proxy-groups": [
            {"name": "🚀 Select", "type": "select", "proxies": ["🇭🇰 HK-1", "🇯🇵 JP-1", "🇺🇸 US-1"]}
        ],
        "rules": [
            "DOMAIN,google.com,🚀 Select",
            "DOMAIN,youtube.com,🚀 Select",
            "MATCH,DIRECT"
        ]
    });

    // 脚本：自动按地区分组
    let auto_group_script = r#"
        function main(config) {
            var hkProxies = [];
            var jpProxies = [];
            var usProxies = [];
            var sgProxies = [];
            var others = [];
            
            for (var i = 0; i < config.proxies.length; i++) {
                var p = config.proxies[i];
                if (p.name.includes('🇭🇰')) hkProxies.push(p.name);
                else if (p.name.includes('🇯🇵')) jpProxies.push(p.name);
                else if (p.name.includes('🇺🇸')) usProxies.push(p.name);
                else if (p.name.includes('🇸🇬')) sgProxies.push(p.name);
                else others.push(p.name);
            }
            
            // 添加地区分组
            if (hkProxies.length > 0) {
                config['proxy-groups'].push({
                    name: '🇭🇰 Hong Kong',
                    type: 'url-test',
                    proxies: hkProxies,
                    url: 'http://www.gstatic.com/generate_204',
                    interval: 300
                });
            }
            
            if (jpProxies.length > 0) {
                config['proxy-groups'].push({
                    name: '🇯🇵 Japan',
                    type: 'url-test',
                    proxies: jpProxies,
                    url: 'http://www.gstatic.com/generate_204',
                    interval: 300
                });
            }
            
            if (usProxies.length > 0) {
                config['proxy-groups'].push({
                    name: '🇺🇸 United States',
                    type: 'url-test',
                    proxies: usProxies,
                    url: 'http://www.gstatic.com/generate_204',
                    interval: 300
                });
            }
            
            // 添加自动选择组
            var allRegions = [];
            if (hkProxies.length > 0) allRegions.push('🇭🇰 Hong Kong');
            if (jpProxies.length > 0) allRegions.push('🇯🇵 Japan');
            if (usProxies.length > 0) allRegions.push('🇺🇸 United States');
            if (sgProxies.length > 0) allRegions.push('🇸🇬 Singapore');
            
            config['proxy-groups'].push({
                name: '🌐 Auto Select',
                type: 'url-test',
                proxies: allRegions,
                url: 'http://www.gstatic.com/generate_204',
                interval: 300
            });
            
            // 更新规则，使用新的自动选择组
            for (var j = 0; j < config.rules.length; j++) {
                if (config.rules[j].includes('🚀 Select')) {
                    config.rules[j] = config.rules[j].replace('🚀 Select', '🌐 Auto Select');
                }
            }
            
            return config;
        }
    "#;

    let result = runtime.execute_with_write(auto_group_script, "auto-group", &subscription_config);
    
    assert!(result.success, "Script should succeed: {:?}", result.error);
    assert!(result.config_modified, "Config should be modified");
    
    let modified = result.modified_config.unwrap();
    
    // 验证地区分组
    let groups = modified["proxy-groups"].as_array().unwrap();
    assert!(groups.iter().any(|g| g["name"] == "🇭🇰 Hong Kong"));
    assert!(groups.iter().any(|g| g["name"] == "🇯🇵 Japan"));
    assert!(groups.iter().any(|g| g["name"] == "🇺🇸 United States"));
    assert!(groups.iter().any(|g| g["name"] == "🌐 Auto Select"));
    
    // 验证规则更新
    let rules = modified["rules"].as_array().unwrap();
    assert!(rules.iter().any(|r| r.as_str().unwrap().contains("🌐 Auto Select")));
    
    // 验证原始分组仍然存在
    assert!(groups.iter().any(|g| g["name"] == "🚀 Select"));
}

/// 集成测试：配置验证 - 确保脚本返回的配置是有效的
#[test]
fn integration_config_validation() {
    let runtime = ScriptRuntime::with_context(ScriptContext {
        core_type: "mihomo".to_owned(),
        core_version: "1.0.0".to_owned(),
        platform: "test".to_owned(),
        profile_name: "validation-test".to_owned(),
    });

    // 测试：脚本返回包含必需字段的配置
    let script = r#"
        function main(config) {
            // 确保 port 存在
            if (!config.port) {
                config.port = 7890;
            }
            
            // 确保至少有一个代理组
            if (!config['proxy-groups'] || config['proxy-groups'].length === 0) {
                config['proxy-groups'] = [
                    {name: 'PROXY', type: 'select', proxies: ['DIRECT']}
                ];
            }
            
            // 确保有规则
            if (!config.rules || config.rules.length === 0) {
                config.rules = ['MATCH,PROXY'];
            }
            
            return config;
        }
    "#;

    let minimal_config = json!({});
    
    let result = runtime.execute_with_write(script, "validation", &minimal_config);
    
    assert!(result.success);
    assert!(result.config_modified);
    
    let modified = result.modified_config.unwrap();
    
    // 验证必需字段
    assert!(modified.get("port").is_some(), "Should have port");
    assert!(modified.get("proxy-groups").is_some(), "Should have proxy-groups");
    assert!(modified.get("rules").is_some(), "Should have rules");
}
