//! # 规则注解 — 追踪哪些规则由 Prism 管理
//!
//! 在 Patch 执行过程中，为 `$prepend` / `$append` 注入的规则生成注解，
//! 供 GUI 规则编辑器判断"这条规则是谁管的"。
//!
//! ## 核心函数
//!
//! | 函数 | 说明 |
//! |------|------|
//! | [`extract_rule_annotations`] | 从执行追踪中提取规则注解 |
//! | [`group_annotations`] | 将规则注解按来源文件分组 |
//!
//! ## 工作流程
//!
//! 1. Prism Engine 执行所有 Patch，产生 [`ExecutionTrace`] 列表
//! 2. [`extract_rule_annotations`] 扫描 `prepend` / `append` 操作的 `affected_items`，
//!    为每条注入的规则生成 [`RuleAnnotation`]
//! 3. [`group_annotations`] 将注解按 `source_file` 归组，生成 [`RuleGroup`] 列表
//! 4. GUI 前端展示规则组，用户可启用/禁用整个组

use std::path::Path;

use crate::types::RuleAnnotation;

/// 从执行追踪中提取规则注解
///
/// 扫描所有 `$prepend` / `$append` 操作的 `affected_items`，
/// 为每条注入的规则生成 [`RuleAnnotation`]。
///
/// # 参数
///
/// - `traces` — Patch 执行追踪列表（由 [`PatchExecutor`] 产生）
/// - `output_config` — 最终输出配置（JSON 格式），用于查找规则在 `rules` 数组中的索引
///
/// # 返回
///
/// 按索引排序的规则注解列表。如果输出配置中没有 `rules` 字段或没有
/// `prepend`/`append` 操作，返回空列表。
///
/// # 标签推导规则
///
/// `source_label` 从文件名推导：
/// 1. 去掉 `.prism.yaml` / `.prism.yml` 后缀
/// 2. 去掉前导数字和连字符（如 `"01-ad-filter"` → `"ad-filter"`）
///
/// # 示例
///
/// ```rust,ignore
/// let annotations = extract_rule_annotations(&traces, &output_config);
/// for ann in &annotations {
///     println!("规则 {} 由 {} 管理", ann.rule_text, ann.source_file);
/// }
/// ```
pub fn extract_rule_annotations(
    traces: &[clash_prism_core::trace::ExecutionTrace],
    output_config: &serde_json::Value,
) -> Vec<RuleAnnotation> {
    let mut annotations = Vec::new();

    // 收集最终配置中 rules 数组的所有规则文本及其索引
    let rules_array = match output_config.get("rules").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return annotations,
    };

    // 为每个 prepend/append 操作生成注解
    // NOTE: 当前仅处理 prepend 和 append 操作。未来如需支持其他操作类型
    // （如 insert、replace 等），应将此判断提取为可配置的策略方法，
    // 例如 `fn is_annotation_target(op_name: &str) -> bool`，
    // 以便调用方根据需要扩展注解范围。
    for trace in traces {
        let op_name = trace.op.display_name();
        if op_name != "Prepend" && op_name != "Append" {
            continue;
        }

        // 提取来源信息
        // group_id 使用文件名（而非 UUID），与 toggle_group() 的文件操作保持一致
        let source_file = trace.source.file.clone().unwrap_or_default();
        let source_label = {
            let mut label = source_file
                .replace(".prism.yaml", "")
                .replace(".prism.yml", "");
            // 循环去除所有前导 "数字-" 模式（如 "01-02-ad-filter" → "ad-filter"）
            loop {
                let trimmed = label.trim_start_matches(|c: char| c.is_numeric());
                if trimmed.starts_with('-') && trimmed.len() > 1 {
                    label = trimmed[1..].to_string();
                } else {
                    break;
                }
            }
            label
        };
        let source_patch = trace.patch_id.as_str().to_string();

        // 遍历 affected_items 中的 Added 条目
        // 当 bulk_items 存在时（大批量摘要模式），从 bulk_items 获取完整描述列表
        if let Some(bulk) = &trace.bulk_items {
            for rule_text in bulk.iter() {
                let pre_parsed = if rule_text.starts_with('{') {
                    serde_json::from_str::<serde_json::Value>(rule_text).ok()
                } else {
                    None
                };
                if let Some(index) = find_rule_index(rules_array, rule_text, &pre_parsed) {
                    annotations.push(RuleAnnotation {
                        rule_text: rule_text.clone(),
                        index_in_output: index,
                        source_file: source_file.clone(),
                        source_patch: source_patch.clone(),
                        source_label: source_label.clone(),
                        immutable: false,
                    });
                }
            }
        } else {
            for item in &trace.affected_items {
                if let Some(rule_text) = &item.after {
                    let pre_parsed = if rule_text.starts_with('{') {
                        serde_json::from_str::<serde_json::Value>(rule_text).ok()
                    } else {
                        None
                    };
                    if let Some(index) = find_rule_index(rules_array, rule_text, &pre_parsed) {
                        annotations.push(RuleAnnotation {
                            rule_text: rule_text.clone(),
                            index_in_output: index,
                            source_file: source_file.clone(),
                            source_patch: source_patch.clone(),
                            source_label: source_label.clone(),
                            immutable: false,
                        });
                    }
                }
            }
        }
    }

    // 按索引排序
    annotations.sort_by_key(|a| a.index_in_output);
    annotations
}

/// 查找规则在输出数组中的索引。
///
/// ## 设计决策：rposition 策略
///
/// 对于重复规则文本，使用 `rposition` 返回**最后一个**匹配项（即数组中最靠后的）。
/// 这是刻意的选择：Prism 注入的规则通常位于数组末尾（通过 `$append`），
/// 而 `rposition` 优先匹配末尾的规则，确保注解标记的是 Prism 注入的副本
/// 而非用户手动添加的同名规则。
///
/// 如果未来需要标记所有重复规则，可改为 `position` + 循环收集所有匹配索引。
///
/// 支持两种规则格式：
/// 1. 字符串格式：`"DOMAIN-SUFFIX,example.com,PROXY"`
/// 2. 对象格式：`{"type": "RULE-SET", "payload": "ruleset.yaml"}`
fn find_rule_index(
    rules: &[serde_json::Value],
    rule_text: &str,
    pre_parsed: &Option<serde_json::Value>,
) -> Option<usize> {
    rules.iter().rposition(|r| {
        if let Some(s) = r.as_str() {
            return s == rule_text;
        }
        if let Some(parsed) = pre_parsed {
            return *parsed == *r;
        }
        false
    })
}

/// 从规则注解列表生成规则分组
///
/// 将属于同一个 `source_file` 的规则归为一组，生成 [`RuleGroup`] 列表。
/// 分组按文件名字典序排列，每组内的规则按索引排序。
///
/// # 参数
///
/// - `annotations` — 规则注解列表（通常由 [`extract_rule_annotations`] 生成）
/// - `workspace` — Prism 工作目录，用于检查 `.disabled` 标记文件
///
/// # 返回
///
/// 规则分组列表。每个分组包含：
/// - `group_id`: 来源文件名
/// - `label`: 来源标签（取自注解中的 `source_label`）
/// - `patch_id`: 来源文件名
/// - `enabled`: 根据文件系统判断（`file_name + ".disabled"` 不存在则为 `true`）
/// - `rules`: 该组管理的规则列表
///
/// # 示例
///
/// ```rust,ignore
/// let annotations = extract_rule_annotations(&traces, &output_config);
/// let groups = group_annotations(&annotations, &workspace_path);
/// for group in &groups {
///     println!("规则组: {} ({} 条规则, 启用: {})", group.label, group.rules.len(), group.enabled);
/// }
/// ```
pub fn group_annotations(
    annotations: &[RuleAnnotation],
    workspace: &Path,
) -> Vec<crate::types::RuleGroup> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<String, Vec<&RuleAnnotation>> = BTreeMap::new();
    for ann in annotations {
        groups.entry(ann.source_file.clone()).or_default().push(ann);
    }

    groups
        .into_iter()
        .filter(|(file_name, _)| {
            // 拒绝包含 ..、\0、绝对路径前缀的文件名
            if file_name.contains('\0') || file_name.contains("..") {
                tracing::warn!(
                    target = "clash_prism_extension",
                    file_name = %file_name,
                    "group_annotations: file_name 包含危险字符，已跳过"
                );
                return false;
            }
            if file_name.starts_with('/') || file_name.starts_with('\\') {
                tracing::warn!(
                    target = "clash_prism_extension",
                    file_name = %file_name,
                    "group_annotations: file_name 为绝对路径，已跳过"
                );
                return false;
            }
            true
        })
        .map(|(file_name, anns)| {
            let label = anns
                .first()
                .map(|a| a.source_label.clone())
                .unwrap_or_else(|| file_name.clone());

            // 检查是否存在 .disabled 标记文件（如 "ad-filter.prism.yaml.disabled"）
            let disabled_marker = workspace.join(format!("{}.disabled", file_name));
            let enabled = !disabled_marker.exists();

            crate::types::RuleGroup {
                group_id: file_name.clone(),
                label,
                patch_id: file_name,
                enabled,
                immutable: false,
                rules: anns
                    .iter()
                    .map(|a| crate::types::RuleEntry {
                        raw: a.rule_text.clone(),
                        index: a.index_in_output,
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clash_prism_core::ir::PatchOp;
    use clash_prism_core::source::{PatchSource, SourceKind};
    use clash_prism_core::trace::{AffectedItem, ExecutionTrace, TraceSummary};

    /// Helper: create a minimal ExecutionTrace with an Append op and given affected_items.
    fn make_append_trace(file: Option<&str>, affected_items: Vec<AffectedItem>) -> ExecutionTrace {
        ExecutionTrace::new(
            clash_prism_core::ir::PatchId::new(),
            PatchSource {
                kind: SourceKind::YamlFile,
                file: file.map(|s| s.to_string()),
                line: None,
                plugin_id: None,
            },
            PatchOp::Append,
            10,
            true,
            TraceSummary::new(affected_items.len(), 0, 0, 0, 0, affected_items.len()),
            affected_items,
        )
    }

    #[test]
    fn test_find_rule_index_string_match() {
        let rules = vec![
            serde_json::json!("DOMAIN-SUFFIX,google.com,PROXY"),
            serde_json::json!("DOMAIN-KEYWORD,github,PROXY"),
            serde_json::json!("MATCH,DIRECT"),
        ];
        let idx = find_rule_index(&rules, "DOMAIN-KEYWORD,github,PROXY", &None);
        assert_eq!(idx, Some(1), "应精确匹配到索引 1");
    }

    #[test]
    fn test_find_rule_index_no_match() {
        let rules = vec![
            serde_json::json!("DOMAIN-SUFFIX,google.com,PROXY"),
            serde_json::json!("MATCH,DIRECT"),
        ];
        let idx = find_rule_index(&rules, "DOMAIN-KEYWORD,github,PROXY", &None);
        assert_eq!(idx, None, "无匹配应返回 None");
    }

    #[test]
    fn test_find_rule_index_rposition_strategy() {
        // 重复规则：rposition 应返回最后一个匹配
        let rules = vec![
            serde_json::json!("DOMAIN-SUFFIX,ad.com,REJECT"),
            serde_json::json!("MATCH,DIRECT"),
            serde_json::json!("DOMAIN-SUFFIX,ad.com,REJECT"),
        ];
        let idx = find_rule_index(&rules, "DOMAIN-SUFFIX,ad.com,REJECT", &None);
        assert_eq!(
            idx,
            Some(2),
            "重复规则时 rposition 应返回最后一个匹配（索引 2）"
        );
    }

    #[test]
    fn test_extract_rule_annotations_empty_traces() {
        let output_config = serde_json::json!({
            "rules": ["DOMAIN-SUFFIX,google.com,PROXY"]
        });
        let annotations = extract_rule_annotations(&[], &output_config);
        assert!(annotations.is_empty(), "空 traces 应返回空注解");
    }

    #[test]
    fn test_extract_rule_annotations_no_rules_field() {
        let trace = make_append_trace(
            Some("ad-filter.prism.yaml"),
            vec![AffectedItem::added(0, "DOMAIN-SUFFIX,ad.com,REJECT")],
        );
        let output_config = serde_json::json!({
            "dns": { "enable": true }
        });
        let annotations = extract_rule_annotations(&[trace], &output_config);
        assert!(annotations.is_empty(), "输出配置无 rules 字段应返回空注解");
    }

    #[test]
    fn test_source_label_strip_prefix() {
        // 验证 "01-ad-filter.prism.yaml" → "ad-filter"
        let label = {
            let source_file = "01-ad-filter.prism.yaml";
            let mut label = source_file
                .replace(".prism.yaml", "")
                .replace(".prism.yml", "");
            loop {
                let trimmed = label.trim_start_matches(|c: char| c.is_numeric());
                if trimmed.starts_with('-') && trimmed.len() > 1 {
                    label = trimmed[1..].to_string();
                } else {
                    break;
                }
            }
            label
        };
        assert_eq!(
            label, "ad-filter",
            "\"01-ad-filter.prism.yaml\" 的 source_label 应为 \"ad-filter\""
        );
    }

    #[test]
    fn test_source_label_no_prefix() {
        // 验证 "rules.prism.yaml" → "rules"
        let label = {
            let source_file = "rules.prism.yaml";
            let mut label = source_file
                .replace(".prism.yaml", "")
                .replace(".prism.yml", "");
            loop {
                let trimmed = label.trim_start_matches(|c: char| c.is_numeric());
                if trimmed.starts_with('-') && trimmed.len() > 1 {
                    label = trimmed[1..].to_string();
                } else {
                    break;
                }
            }
            label
        };
        assert_eq!(
            label, "rules",
            "\"rules.prism.yaml\" 的 source_label 应为 \"rules\""
        );
    }
}
