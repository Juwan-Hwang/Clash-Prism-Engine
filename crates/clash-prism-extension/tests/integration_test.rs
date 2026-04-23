//! clash-prism-extension 集成测试
//!
//! 测试 PrismExtension 的核心功能，包括：
//! - 空 workspace 的 apply 行为
//! - 单个 prepend patch 的 apply 和验证
//! - toggle_group 启用/禁用规则组
//! - apply 后的 status 状态检查
//! - list_rules 规则组列表验证

use std::path::PathBuf;
use std::sync::Mutex;

use clash_prism_extension::{ApplyOptions, ApplyStatus, PrismEvent, PrismExtension, PrismHost};
use tempfile::tempdir;

/// 测试用宿主实现
///
/// 使用内存中的配置字符串和临时目录来模拟 GUI 宿主。
/// `validate_config` 直接返回 `Ok(true)`，不实际运行 mihomo。
struct TestHost {
    /// 当前运行中的配置（YAML 字符串）
    running_config: Mutex<String>,
    /// Prism 工作目录
    workspace: PathBuf,
}

impl TestHost {
    /// 创建测试宿主
    ///
    /// # 参数
    ///
    /// - `config` — 初始运行配置（YAML 字符串）
    /// - `workspace` — Prism 工作目录路径
    fn new(config: &str, workspace: PathBuf) -> Self {
        Self {
            running_config: Mutex::new(config.to_string()),
            workspace,
        }
    }
}

impl PrismHost for TestHost {
    fn read_running_config(&self) -> Result<String, String> {
        Ok(self.running_config.lock().unwrap().clone())
    }

    fn apply_config(&self, config: &str) -> Result<ApplyStatus, String> {
        // 同时更新 running_config，使后续 read_running_config 返回最新配置
        *self.running_config.lock().unwrap() = config.to_string();
        Ok(ApplyStatus {
            files_saved: true,
            hot_reload_success: true,
            message: "配置已更新".to_string(),
            restarted: false,
        })
    }

    fn get_prism_workspace(&self) -> Result<PathBuf, String> {
        Ok(self.workspace.clone())
    }

    fn notify(&self, _event: PrismEvent) {
        // 测试中不需要处理事件通知
    }

    fn validate_config(&self, _config: &str) -> Result<bool, String> {
        Ok(true)
    }
}

/// 只读测试宿主
///
/// `apply_config` 不更新 `running_config`，使多次 apply 的 base config 始终相同。
/// 用于测试解析缓存一致性等需要稳定 base config 的场景。
struct ReadOnlyTestHost {
    running_config: Mutex<String>,
    workspace: PathBuf,
}

impl ReadOnlyTestHost {
    fn new(config: &str, workspace: PathBuf) -> Self {
        Self {
            running_config: Mutex::new(config.to_string()),
            workspace,
        }
    }
}

impl PrismHost for ReadOnlyTestHost {
    fn read_running_config(&self) -> Result<String, String> {
        Ok(self.running_config.lock().unwrap().clone())
    }

    fn apply_config(&self, _config: &str) -> Result<ApplyStatus, String> {
        // 不更新 running_config，保持 base config 不变
        Ok(ApplyStatus {
            files_saved: true,
            hot_reload_success: true,
            message: "配置已更新".to_string(),
            restarted: false,
        })
    }

    fn get_prism_workspace(&self) -> Result<PathBuf, String> {
        Ok(self.workspace.clone())
    }

    fn notify(&self, _event: PrismEvent) {
        // 测试中不需要处理事件通知
    }

    fn validate_config(&self, _config: &str) -> Result<bool, String> {
        Ok(true)
    }
}

/// 构造一个基础 mihomo 配置（JSON -> YAML）
fn base_config_yaml() -> String {
    let config = serde_json::json!({
        "mixed-port": 7890,
        "allow-lan": false,
        "mode": "rule",
        "log-level": "info",
        "dns": {
            "enable": true,
            "nameserver": ["8.8.8.8", "1.1.1.1"]
        },
        "rules": [
            "MATCH,DIRECT"
        ]
    });
    serde_yml::to_string(&config).unwrap()
}

/// test_apply_empty_workspace -- 空 prism 目录，apply 应返回空结果
///
/// 当工作目录为空（没有 .prism.yaml 文件）时，apply 应直接返回原始配置，
/// stats 中所有计数为 0，trace 和 rule_annotations 为空。
#[test]
fn test_apply_empty_workspace() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    let result = ext.apply(ApplyOptions::default()).unwrap();

    // 空 workspace：无 patch 执行
    assert_eq!(result.stats.total_patches, 0);
    assert_eq!(result.stats.succeeded, 0);
    assert_eq!(result.stats.skipped, 0);
    assert_eq!(result.stats.total_added, 0);
    assert_eq!(result.stats.total_removed, 0);
    assert_eq!(result.stats.total_modified, 0);
    assert!(result.trace.is_empty());
    assert!(result.rule_annotations.is_empty());

    // 输出配置应与输入完全一致
    assert_eq!(
        result.output_config.trim(),
        base_config_yaml().trim(),
        "空 workspace 输出应与输入完全一致"
    );
}

/// test_apply_single_prepend -- 创建一个包含 $prepend 的 .prism.yaml 文件
///
/// apply 后验证 output_config 包含新增规则，stats 统计正确。
#[test]
fn test_apply_single_prepend() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 创建 .prism.yaml 文件，向 rules 数组头部插入广告过滤规则
    let prism_file = workspace.join("ad-filter.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,ad.com,REJECT"
    - "DOMAIN-SUFFIX,ads.example.com,REJECT"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    let result = ext.apply(ApplyOptions::default()).unwrap();

    // 验证 patch 被执行
    assert_eq!(result.stats.total_patches, 1);
    assert_eq!(result.stats.succeeded, 1);
    assert_eq!(result.stats.total_added, 2);

    // 验证输出配置包含新增的规则
    assert!(
        result.output_config.contains("DOMAIN-SUFFIX,ad.com,REJECT"),
        "输出配置应包含新增的广告过滤规则"
    );
    assert!(
        result
            .output_config
            .contains("DOMAIN-SUFFIX,ads.example.com,REJECT"),
        "输出配置应包含新增的广告过滤规则"
    );

    // 验证原始规则仍然存在
    assert!(
        result.output_config.contains("MATCH,DIRECT"),
        "输出配置应保留原始的 MATCH 规则"
    );

    // 验证 trace 中记录了 prepend 操作
    assert_eq!(result.trace.len(), 1);
    assert_eq!(result.trace[0].op_name, "Prepend");
    assert!(result.trace[0].condition_matched);
    assert_eq!(result.trace[0].summary.added, 2);

    // 验证 diff 视图记录了新增项
    assert_eq!(result.trace[0].diff.added.len(), 2);

    // 验证规则顺序和数量
    // $prepend 保持声明顺序
    let output: serde_json::Value = serde_yml::from_str(&result.output_config).unwrap();
    let rules = output["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 3, "应有 2 条 prepend + 1 条原始规则");
    assert_eq!(rules[0].as_str().unwrap(), "DOMAIN-SUFFIX,ad.com,REJECT");
    assert_eq!(
        rules[1].as_str().unwrap(),
        "DOMAIN-SUFFIX,ads.example.com,REJECT"
    );
    assert_eq!(rules[2].as_str().unwrap(), "MATCH,DIRECT");
}

/// test_toggle_group -- 创建 .prism.yaml 文件，toggle_group(false) 后验证文件被重命名为 .disabled
///
/// toggle_group(true) 后恢复原始文件名。
#[test]
fn test_toggle_group() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 创建 .prism.yaml 文件
    let prism_file = workspace.join("test-group.prism.yaml");
    let prism_content = r#"
dns:
  nameserver:
    $prepend:
      - "223.5.5.5"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace.clone());
    let ext = PrismExtension::new(host);

    // 禁用规则组
    let result = ext.toggle_group("test-group.prism.yaml", false).unwrap();
    assert!(result, "toggle_group(false) 应返回 true");

    // 验证原文件已不存在
    assert!(!prism_file.exists(), "禁用后原文件应不存在");

    // 验证 .disabled 文件存在
    let disabled_file = workspace.join("test-group.prism.yaml.disabled");
    assert!(disabled_file.exists(), "禁用后应有 .disabled 文件");

    // 重新启用规则组
    let result = ext.toggle_group("test-group.prism.yaml", true).unwrap();
    assert!(result, "toggle_group(true) 应返回 true");

    // 验证原文件恢复
    assert!(prism_file.exists(), "启用后原文件应恢复");

    // 验证 .disabled 文件已不存在
    assert!(!disabled_file.exists(), "启用后 .disabled 文件应不存在");
}

/// test_status_after_apply -- apply 后验证 status() 返回正确的编译时间和成功状态
#[test]
fn test_status_after_apply() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 创建一个简单的 .prism.yaml 文件
    let prism_file = workspace.join("simple.prism.yaml");
    let prism_content = r#"
dns:
  enable: true
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // apply 前检查初始状态
    let status = ext.status();
    assert!(!status.last_compile_success, "apply 前编译状态应为失败");
    assert!(
        status.last_compile_time.is_none(),
        "apply 前编译时间应为 None"
    );

    // 执行 apply
    ext.apply(ApplyOptions::default()).unwrap();

    // apply 后检查状态
    let status = ext.status();
    assert!(status.last_compile_success, "apply 后编译状态应为成功");
    assert!(status.last_compile_time.is_some(), "apply 后编译时间应有值");
    assert_eq!(status.patch_count, 1, "apply 后 patch_count 应为 1");

    // 验证编译时间是有效的 ISO 8601 格式
    let time_str = status.last_compile_time.unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&time_str).is_ok(),
        "编译时间应为有效的 RFC 3339 格式"
    );
}

/// test_list_rules -- apply 后验证 list_rules() 返回正确的规则组
///
/// 验证 extract_rule_annotations 正确从 affected_items 中提取规则文本，
/// 并按来源文件分组返回 RuleGroup 列表。
#[test]
fn test_list_rules() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 创建一个包含 prepend 规则的 .prism.yaml 文件
    let prism_file = workspace.join("my-rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,example.com,DIRECT"
    - "DOMAIN-KEYWORD,test,REJECT"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // apply 前无规则组
    let groups = ext.list_rules().unwrap();
    assert!(groups.is_empty(), "apply 前规则组应为空");

    // 执行 apply
    let result = ext.apply(ApplyOptions::default()).unwrap();

    // 验证 patch 执行成功且规则已注入到输出配置
    assert_eq!(result.stats.total_patches, 1);
    assert_eq!(result.stats.succeeded, 1);
    assert_eq!(result.stats.total_added, 2);
    assert!(
        result
            .output_config
            .contains("DOMAIN-SUFFIX,example.com,DIRECT"),
        "输出配置应包含新增的第一条规则"
    );
    assert!(
        result.output_config.contains("DOMAIN-KEYWORD,test,REJECT"),
        "输出配置应包含新增的第二条规则"
    );

    // 验证 trace 记录了 prepend 操作来源
    assert_eq!(result.trace.len(), 1);
    assert_eq!(
        result.trace[0].source_file.as_deref(),
        Some("my-rules.prism.yaml"),
        "trace 应记录来源文件"
    );

    // list_rules 依赖 extract_rule_annotations，后者依赖 affected_items
    // 中的实际规则文本。executor 已正确填充 affected_items。
    let groups = ext.list_rules().unwrap();
    assert_eq!(
        groups.len(),
        1,
        "应有 1 个规则组（来自 my-rules.prism.yaml）"
    );
    assert_eq!(groups[0].group_id, "my-rules.prism.yaml");
    assert_eq!(groups[0].rules.len(), 2, "规则组应包含 2 条规则");
    // $prepend 保持声明顺序
    assert_eq!(groups[0].rules[0].raw, "DOMAIN-SUFFIX,example.com,DIRECT");
    assert_eq!(groups[0].rules[0].index, 0);
    assert_eq!(groups[0].rules[1].raw, "DOMAIN-KEYWORD,test,REJECT");
    assert_eq!(groups[0].rules[1].index, 1);
}

/// test_insert_rule -- 测试 insert_rule 功能
///
/// 在 apply 后通过 insert_rule_str 插入一条用户自定义规则，
/// 验证规则被正确插入到配置中。
#[test]
fn test_insert_rule() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 创建一个包含 prepend 规则的 .prism.yaml 文件
    let prism_file = workspace.join("rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,ad.com,REJECT"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // 先 apply 使配置就绪
    ext.apply(ApplyOptions::default()).unwrap();

    // 在末尾插入一条用户规则
    let idx = ext
        .insert_rule_str(
            "DOMAIN-KEYWORD,custom,DIRECT",
            &clash_prism_extension::RuleInsertPosition::Append,
        )
        .unwrap();

    // 验证插入位置合理（Append 到 2 条规则之后，索引应为 2）
    assert_eq!(idx, 2, "Append 到 2 条规则之后，索引应为 2");

    // insert_rule 内部调用 host.apply_config，TestHost 会更新 running_config
    // 验证 insert_rule 后内部状态被正确失效（last_patches/last_traces 已清空）
    // 通过 is_prism_rule 检查返回值：insert_rule 插入的是用户自定义规则，
    // 不属于 Prism 管理的规则，因此 is_prism 应为 false
    let is_prism = ext.is_prism_rule(idx).unwrap();
    assert!(
        !is_prism.is_prism,
        "insert_rule 插入的用户规则不应被标记为 Prism 管理的规则 (index={})",
        idx
    );
    assert!(is_prism.group.is_none(), "用户规则不应有 group 关联");
    assert!(is_prism.label.is_none(), "用户规则不应有 label 关联");
}

/// test_preview_rules -- 测试 preview_rules 功能
///
/// apply 后通过 preview_rules 查看指定 patch 的规则变更，
/// 验证返回的 diff 信息正确。
#[test]
fn test_preview_rules() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("preview-test.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,preview.example.com,PROXY"
    - "DOMAIN-KEYWORD,preview-test,REJECT"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    let result = ext.apply(ApplyOptions::default()).unwrap();
    assert_eq!(result.stats.total_added, 2);

    // preview_rules 需要有效的 patch_id
    // patch_id 来自 trace，验证 trace 非空
    assert!(!result.trace.is_empty());
    let patch_id = &result.trace[0].patch_id;

    // 尝试 preview — 验证 diff 内容
    let diff = ext.preview_rules(patch_id).unwrap();
    assert_eq!(diff.added.len(), 2, "preview_rules 应返回 2 条新增规则");
    assert!(
        diff.added
            .contains(&"DOMAIN-SUFFIX,preview.example.com,PROXY".to_string()),
        "diff 应包含新增的第一条规则"
    );
    assert!(
        diff.added
            .contains(&"DOMAIN-KEYWORD,preview-test,REJECT".to_string()),
        "diff 应包含新增的第二条规则"
    );
}

/// test_toggle_group_path_traversal -- 测试路径遍历防护
///
/// 验证 toggle_group 拒绝包含 ".." 的 group_id，
/// 防止路径遍历攻击（如 "../../etc/passwd"）。
#[test]
fn test_toggle_group_path_traversal() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // 尝试使用路径遍历的 group_id
    let result = ext.toggle_group("../../etc/passwd", false);
    assert!(result.is_err(), "路径遍历 group_id 应被拒绝");

    // 尝试使用包含 null 字节的 group_id
    let result = ext.toggle_group("foo\0bar", false);
    assert!(result.is_err(), "包含 null 字节的 group_id 应被拒绝");

    // 尝试使用包含正斜杠的 group_id
    let result = ext.toggle_group("sub/dir/file.prism.yaml", false);
    assert!(result.is_err(), "包含正斜杠的 group_id 应被拒绝");

    // 尝试使用包含反斜杠的 group_id
    let result = ext.toggle_group("sub\\dir\\file.prism.yaml", false);
    assert!(result.is_err(), "包含反斜杠的 group_id 应被拒绝");
}

/// test_is_prism_rule_positive -- is_prism_rule 对已知 Prism 规则返回正确信息
///
/// 验证 HashMap 索引命中路径：apply 后对 Prism 注入的规则调用 is_prism_rule()，
/// 应返回 is_prism: true 且 group/label 正确。
#[test]
fn test_is_prism_rule_positive() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,managed.example.com,PROXY"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    let result = ext.apply(ApplyOptions::default()).unwrap();
    assert_eq!(result.stats.total_added, 1);

    // Prism 注入的规则在索引 0（prepend 到 rules 数组头部）
    let is_prism = ext.is_prism_rule(0).unwrap();
    assert!(is_prism.is_prism, "索引 0 应为 Prism 管理的规则");
    assert_eq!(
        is_prism.group.as_deref(),
        Some("rules.prism.yaml"),
        "group 应为来源文件名"
    );
    assert!(is_prism.label.is_some(), "label 应有值（来源标签）");
    assert_eq!(
        is_prism.label.as_deref(),
        Some("rules"),
        "label 应为来源文件名（去掉 .prism.yaml 后缀）"
    );
}

/// test_is_prism_rule_non_prism -- is_prism_rule 对原始配置中的非 Prism 规则返回 false
#[test]
fn test_is_prism_rule_non_prism() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,managed.example.com,PROXY"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    ext.apply(ApplyOptions::default()).unwrap();

    // 原始配置中的 MATCH,DIRECT 在 prepend 之后，索引为 1
    let is_prism = ext.is_prism_rule(1).unwrap();
    assert!(
        !is_prism.is_prism,
        "原始 MATCH,DIRECT 不应被标记为 Prism 规则"
    );
    assert!(is_prism.group.is_none());
    assert!(is_prism.label.is_none());
}

/// test_insert_rule_clears_annotations -- insert_rule 后 list_rules 应返回空
///
/// 验证 insert_rule() 正确清空了 last_annotations 和 annotation_index，
/// 避免索引偏移后返回错误数据。
#[test]
fn test_insert_rule_clears_annotations() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,ad.com,REJECT"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = TestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // apply 后应有 1 个规则组
    ext.apply(ApplyOptions::default()).unwrap();
    let groups = ext.list_rules().unwrap();
    assert_eq!(groups.len(), 1, "apply 后应有 1 个规则组");

    // insert_rule 后注解应被清空
    ext.insert_rule_str(
        "DOMAIN-KEYWORD,custom,DIRECT",
        &clash_prism_extension::RuleInsertPosition::Append,
    )
    .unwrap();

    let groups = ext.list_rules().unwrap();
    assert!(
        groups.is_empty(),
        "insert_rule 后注解应被清空，list_rules 应返回空"
    );
}

/// test_parse_cache_consistency -- 连续两次 apply 输出完全一致
///
/// 使用 ReadOnlyTestHost（apply_config 不更新 running_config），
/// 确保两次 apply 的 base config 完全相同。
/// 验证文件级解析缓存不会导致第二次 apply 产生不同结果。
#[test]
fn test_parse_cache_consistency() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("cache-test.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - "DOMAIN-SUFFIX,cache.test,PROXY"
    - "DOMAIN-KEYWORD,cached,REJECT"
dns:
  nameserver:
    $prepend:
      - "223.5.5.5"
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    let host = ReadOnlyTestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    // 第一次 apply
    let result1 = ext.apply(ApplyOptions::default()).unwrap();
    assert_eq!(result1.stats.succeeded, 2);

    // 第二次 apply（文件内容未变，应命中缓存）
    let result2 = ext.apply(ApplyOptions::default()).unwrap();
    assert_eq!(result2.stats.succeeded, 2, "缓存命中后 patch 数应一致");

    // 输出配置应完全一致（base config 相同 + 缓存命中 = 结果相同）
    assert_eq!(
        result1.output_config, result2.output_config,
        "连续两次 apply（相同 base config）的输出配置应完全一致"
    );
}

/// test_parse_cache_invalidation -- 修改文件后缓存应失效
///
/// 使用 ReadOnlyTestHost 确保 base config 不变，
/// 验证修改 .prism.yaml 内容后重新 apply，结果反映新内容且旧内容不存在。
#[test]
fn test_parse_cache_invalidation() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let prism_file = workspace.join("mutable.prism.yaml");
    std::fs::write(
        &prism_file,
        "rules:\n  $prepend:\n    - \"DOMAIN-SUFFIX,v1.example.com,PROXY\"\n",
    )
    .unwrap();

    let host = ReadOnlyTestHost::new(&base_config_yaml(), workspace);
    let ext = PrismExtension::new(host);

    let result1 = ext.apply(ApplyOptions::default()).unwrap();
    assert!(
        result1.output_config.contains("v1"),
        "第一次 apply 应包含 v1 规则"
    );

    // 修改文件内容
    std::fs::write(
        &prism_file,
        "rules:\n  $prepend:\n    - \"DOMAIN-SUFFIX,v2.example.com,PROXY\"\n",
    )
    .unwrap();

    let result2 = ext.apply(ApplyOptions::default()).unwrap();
    assert!(
        result2.output_config.contains("v2"),
        "修改文件后 apply 应反映新内容"
    );
    assert!(
        !result2.output_config.contains("v1"),
        "修改文件后旧内容不应存在"
    );
}

/// test_object_rule_annotation -- 对象格式规则的注解正确性
///
/// 验证 find_rule_index 的 pre_parsed 分支能正确处理 JSON 对象规则。
#[test]
fn test_object_rule_annotation() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    // 使用对象格式规则（rule-set 类型）
    let prism_file = workspace.join("obj-rules.prism.yaml");
    let prism_content = r#"
rules:
  $prepend:
    - {"type": "RULE-SET", "payload": "category-ads-all"}
"#;
    std::fs::write(&prism_file, prism_content).unwrap();

    // 基础配置中包含一个已有的对象格式规则
    let config = serde_json::json!({
        "mixed-port": 7890,
        "mode": "rule",
        "rules": [
            {"type": "RULE-SET", "payload": "existing-ruleset"},
            "MATCH,DIRECT"
        ]
    });
    let config_yaml = serde_yml::to_string(&config).unwrap();

    let host = TestHost::new(&config_yaml, workspace);
    let ext = PrismExtension::new(host);

    let result = ext.apply(ApplyOptions::default()).unwrap();
    assert_eq!(result.stats.total_added, 1);

    // list_rules 应正确识别对象格式规则
    let groups = ext.list_rules().unwrap();
    assert_eq!(groups.len(), 1, "应有 1 个规则组");
    assert_eq!(groups[0].rules.len(), 1, "应包含 1 条 Prism 管理的规则");

    // is_prism_rule 应对 Prism 注入的对象规则返回 true
    let is_prism = ext.is_prism_rule(0).unwrap();
    assert!(
        is_prism.is_prism,
        "对象格式规则应被正确标记为 Prism 管理的规则"
    );
}
