//! Hook 结果聚合
//!
//! 参考 Claude Code AggregatedHookResult: 合并多个钩子的结果
//! （消息、错误、修改决策）。
//!
//! ## 设计原则
//!
//! - 多个钩子按顺序执行
//! - 每个钩子可以看到前一个钩子的修改结果
//! - 如果某个钩子设置了 `prevent_continuation`，后续钩子不再执行
//! - 阻塞性错误（blocking_errors）会阻止管道继续推进

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ══════════════════════════════════════════════════════════
// 单个钩子执行结果
// ══════════════════════════════════════════════════════════

/// 单个钩子的执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    /// 钩子名称（如 "OnBeforeWrite"）
    pub hook_name: String,

    /// 是否执行成功
    pub success: bool,

    /// 信息消息（非阻塞，仅记录）
    #[serde(default)]
    pub messages: Vec<String>,

    /// 阻塞性错误（阻止后续执行）
    #[serde(default)]
    pub blocking_errors: Vec<String>,

    /// 是否阻止后续钩子执行
    #[serde(default)]
    pub prevent_continuation: bool,

    /// 钩子修改后的配置（`None` 表示未修改）
    pub modified_config: Option<Value>,
}

impl HookResult {
    /// 创建成功的钩子结果
    pub fn ok(hook_name: impl Into<String>) -> Self {
        Self {
            hook_name: hook_name.into(),
            success: true,
            messages: Vec::new(),
            blocking_errors: Vec::new(),
            prevent_continuation: false,
            modified_config: None,
        }
    }

    /// 创建失败的钩子结果
    pub fn err(hook_name: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            hook_name: hook_name.into(),
            success: false,
            messages: Vec::new(),
            blocking_errors: vec![error.into()],
            prevent_continuation: true,
            modified_config: None,
        }
    }

    /// 添加信息消息
    pub fn with_message(mut self, msg: impl Into<String>) -> Self {
        self.messages.push(msg.into());
        self
    }

    /// 添加阻塞性错误
    pub fn with_blocking_error(mut self, err: impl Into<String>) -> Self {
        self.blocking_errors.push(err.into());
        self.prevent_continuation = true;
        self.success = false;
        self
    }

    /// 设置修改后的配置
    pub fn with_modified_config(mut self, config: Value) -> Self {
        self.modified_config = Some(config);
        self
    }

    /// 设置阻止后续执行标志
    pub fn with_prevent_continuation(mut self) -> Self {
        self.prevent_continuation = true;
        self
    }
}

// ══════════════════════════════════════════════════════════
// 聚合结果
// ══════════════════════════════════════════════════════════

/// 聚合的钩子结果
///
/// 按顺序合并多个钩子的执行结果，维护最终状态：
/// - `modified_config`：最后一个设置了修改的钩子的输出
/// - `prevent_continuation`：任一钩子设置后为 `true`
/// - `blocking_errors`：所有钩子的阻塞性错误汇总
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedHookResult {
    /// 所有钩子的信息消息
    pub messages: Vec<String>,

    /// 所有阻塞性错误
    pub blocking_errors: Vec<String>,

    /// 是否有钩子阻止了后续执行
    pub prevent_continuation: bool,

    /// 最终修改后的配置（链式传递）
    pub modified_config: Option<Value>,

    /// 各钩子的执行记录（保留完整审计链）
    pub hook_results: Vec<HookResult>,
}

impl AggregatedHookResult {
    /// 创建空结果
    pub fn new() -> Self {
        Self::default()
    }

    /// 合并单个钩子的结果
    ///
    /// 合并规则：
    /// 1. 如果 `prevent_continuation` 已为 `true`，仍收集结果但标记跳过
    /// 2. 如果 `modified_config` 有值，更新（后续钩子基于此修改）
    /// 3. 收集所有 `messages` 和 `blocking_errors`
    /// 4. 如果新结果的 `prevent_continuation` 为 `true`，设置标志
    pub fn merge(&mut self, mut result: HookResult) {
        // 收集消息（保留原始记录用于审计）
        self.messages.extend(result.messages.iter().cloned());

        // 收集阻塞性错误（保留原始记录用于审计）
        self.blocking_errors
            .extend(result.blocking_errors.iter().cloned());

        // 传递修改后的配置（链式更新，take 避免部分移动）
        if result.modified_config.is_some() {
            self.modified_config = result.modified_config.take();
        }

        // 传播阻止标志
        if result.prevent_continuation {
            self.prevent_continuation = true;
        }

        // 记录完整审计链
        self.hook_results.push(result);
    }

    /// 是否有错误（阻塞性错误或执行失败）
    pub fn has_errors(&self) -> bool {
        !self.blocking_errors.is_empty() || self.hook_results.iter().any(|r| !r.success)
    }

    /// 是否所有钩子都执行成功
    pub fn is_success(&self) -> bool {
        !self.has_errors() && !self.prevent_continuation
    }

    /// 获取人类可读的报告
    pub fn report(&self) -> String {
        if self.hook_results.is_empty() {
            return "无钩子执行记录".to_string();
        }

        let mut lines = Vec::new();

        // 头部摘要
        let success_count = self.hook_results.iter().filter(|r| r.success).count();
        let fail_count = self.hook_results.len() - success_count;
        lines.push(format!(
            "钩子执行报告: {} 个钩子, {} 成功, {} 失败",
            self.hook_results.len(),
            success_count,
            fail_count
        ));

        // 各钩子详情
        for result in &self.hook_results {
            let status = if result.success { "OK" } else { "FAIL" };
            lines.push(format!("  [{}] {}", status, result.hook_name));

            for msg in &result.messages {
                lines.push(format!("    消息: {}", msg));
            }
            for err in &result.blocking_errors {
                lines.push(format!("    错误: {}", err));
            }
            if result.prevent_continuation {
                lines.push("    已阻止后续执行".to_string());
            }
            if result.modified_config.is_some() {
                lines.push("    已修改配置".to_string());
            }
        }

        // 尾部汇总
        if self.prevent_continuation {
            lines.push("执行被阻止: 后续钩子未执行".to_string());
        }
        if !self.blocking_errors.is_empty() {
            lines.push(format!("阻塞性错误总计: {}", self.blocking_errors.len()));
        }

        lines.join("\n")
    }
}

// ══════════════════════════════════════════════════════════
// 钩子条件过滤器
// ══════════════════════════════════════════════════════════

/// 钩子条件过滤器
///
/// 参考 Claude Code hook 的 `if` 字段：使用模式匹配语法过滤触发条件。
///
/// 支持的表达式语法：
/// - `"dns_changed"` — 精确匹配事件名
/// - `"patch_count > 0"` — 数值比较
/// - `"event == OnBeforeWrite"` — 事件名比较
/// - `"modified_paths contains dns"` — 路径包含检查
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCondition {
    /// 条件表达式
    pub expression: String,
}

impl HookCondition {
    /// 评估条件是否满足
    ///
    /// 解析表达式并对照上下文数据进行求值。
    /// 对于无法解析的表达式，默认返回 `false`（安全失败）。
    pub fn evaluate(&self, context: &HookContext) -> bool {
        let expr = self.expression.trim();

        // ── 精确事件名匹配 ──
        // 例如: "dns_changed", "OnBeforeWrite"
        // 条件：不含空格、不含比较运算符、不含独立关键字 "contains"
        // 使用结构化解析：仅当 "contains" 作为独立单词出现时才分类为路径包含检查
        let has_comparison_op =
            expr.contains('>') || expr.contains('<') || expr.contains('=') || expr.contains('!');
        let has_space = expr.contains(' ');
        // 检查 "contains" 是否作为独立单词出现（前后有空白或位于开头/结尾）
        let has_contains_keyword = expr.split_whitespace().any(|word| word == "contains");
        if !has_space && !has_comparison_op && !has_contains_keyword {
            return context.event == expr;
        }

        // ── 数值比较: patch_count > N ──
        if let Some(rest) = expr.strip_prefix("patch_count") {
            let rest = rest.trim();
            if let Some(result) = Self::parse_comparison(rest, context.patch_count) {
                return result;
            }
        }

        // ── 事件名比较: event == Name ──
        if let Some(rest) = expr.strip_prefix("event") {
            let rest = rest.trim();
            if let Some(target) = rest.strip_prefix("==") {
                return context.event == target.trim();
            }
            if let Some(target) = rest.strip_prefix("!=") {
                return context.event != target.trim();
            }
        }

        // ── 路径包含检查: modified_paths contains keyword ──
        if let Some(rest) = expr.strip_prefix("modified_paths") {
            let rest = rest.trim();
            if let Some(keyword) = rest.strip_prefix("contains") {
                let keyword = keyword.trim();
                return context.modified_paths.iter().any(|p| p.contains(keyword));
            }
        }

        // 无法解析的表达式 → 安全失败
        tracing::warn!(
            expression = %self.expression,
            "无法解析的钩子条件表达式，默认返回 false"
        );
        false
    }

    /// 解析比较表达式 `> N`, `>= N`, `< N`, `<= N`, `== N`
    ///
    /// 返回比较结果，如果无法解析则返回 `None`。
    fn parse_comparison(expr: &str, value: usize) -> Option<bool> {
        let expr = expr.trim();

        if let Some(rest) = expr.strip_prefix(">=") {
            let n: usize = rest.trim().parse().ok()?;
            Some(value >= n)
        } else if let Some(rest) = expr.strip_prefix("<=") {
            let n: usize = rest.trim().parse().ok()?;
            Some(value <= n)
        } else if let Some(rest) = expr.strip_prefix(">") {
            let n: usize = rest.trim().parse().ok()?;
            Some(value > n)
        } else if let Some(rest) = expr.strip_prefix("<") {
            let n: usize = rest.trim().parse().ok()?;
            Some(value < n)
        } else if let Some(rest) = expr.strip_prefix("==") {
            let n: usize = rest.trim().parse().ok()?;
            Some(value == n)
        } else {
            None
        }
    }
}

// ══════════════════════════════════════════════════════════
// 钩子执行上下文
// ══════════════════════════════════════════════════════════

/// 钩子执行上下文
///
/// 提供给钩子条件过滤器和钩子脚本的环境信息，
/// 描述当前正在处理的事件和状态。
#[derive(Debug, Clone)]
pub struct HookContext {
    /// 当前事件名称（如 "OnBeforeWrite", "OnMerged"）
    pub event: String,

    /// 被修改的配置路径列表
    pub modified_paths: Vec<String>,

    /// 执行的 patch 数量
    pub patch_count: usize,

    /// 额外上下文数据（键值对）
    pub extra: BTreeMap<String, String>,
}

impl HookContext {
    /// 创建新的钩子上下文
    pub fn new(event: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            modified_paths: Vec::new(),
            patch_count: 0,
            extra: BTreeMap::new(),
        }
    }

    /// 添加修改路径
    pub fn with_modified_path(mut self, path: impl Into<String>) -> Self {
        self.modified_paths.push(path.into());
        self
    }

    /// 设置 patch 数量
    pub fn with_patch_count(mut self, count: usize) -> Self {
        self.patch_count = count;
        self
    }

    /// 添加额外上下文数据
    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

// ══════════════════════════════════════════════════════════
// 测试
// ══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── HookResult 测试 ──

    #[test]
    fn test_hook_result_ok() {
        let result = HookResult::ok("OnBeforeWrite");
        assert!(result.success);
        assert!(result.messages.is_empty());
        assert!(result.blocking_errors.is_empty());
        assert!(!result.prevent_continuation);
    }

    #[test]
    fn test_hook_result_err() {
        let result = HookResult::err("OnBeforeWrite", "配置校验失败");
        assert!(!result.success);
        assert_eq!(result.blocking_errors.len(), 1);
        assert!(result.prevent_continuation);
    }

    #[test]
    fn test_hook_result_builder() {
        let result = HookResult::ok("OnMerged")
            .with_message("合并完成")
            .with_modified_config(serde_json::json!({"key": "value"}));

        assert!(result.success);
        assert_eq!(result.messages.len(), 1);
        assert!(result.modified_config.is_some());
    }

    #[test]
    fn test_hook_result_with_blocking_error() {
        let result = HookResult::ok("OnBeforeWrite").with_blocking_error("严重错误");

        assert!(!result.success);
        assert!(result.prevent_continuation);
    }

    // ── AggregatedHookResult 测试 ──

    #[test]
    fn test_aggregated_result_new() {
        let agg = AggregatedHookResult::new();
        assert!(agg.messages.is_empty());
        assert!(agg.blocking_errors.is_empty());
        assert!(!agg.prevent_continuation);
        assert!(agg.modified_config.is_none());
        assert!(agg.hook_results.is_empty());
        assert!(!agg.has_errors());
        assert!(agg.is_success());
    }

    #[test]
    fn test_aggregated_result_merge() {
        let mut agg = AggregatedHookResult::new();

        // 合并第一个成功结果
        let r1 = HookResult::ok("OnMerged")
            .with_message("节点去重完成")
            .with_modified_config(serde_json::json!({"merged": true}));
        agg.merge(r1);

        // 合并第二个成功结果
        let r2 = HookResult::ok("OnBeforeWrite").with_message("配置校验通过");
        agg.merge(r2);

        assert_eq!(agg.hook_results.len(), 2);
        assert_eq!(agg.messages.len(), 2);
        assert!(!agg.has_errors());
        assert!(agg.is_success());
        // modified_config 应为第一个钩子的输出
        assert_eq!(agg.modified_config.unwrap()["merged"], true);
    }

    #[test]
    fn test_aggregated_result_prevent_continuation() {
        let mut agg = AggregatedHookResult::new();

        // 第一个钩子阻止后续执行
        let r1 = HookResult::ok("OnMerged")
            .with_message("检测到异常配置")
            .with_prevent_continuation();
        agg.merge(r1);

        // 第二个钩子仍然会被收集（但实际执行时会被跳过）
        let r2 = HookResult::ok("OnBeforeWrite").with_message("此钩子不应执行");
        agg.merge(r2);

        assert!(agg.prevent_continuation);
        assert_eq!(agg.hook_results.len(), 2);
        assert_eq!(agg.messages.len(), 2);
    }

    #[test]
    fn test_aggregated_result_blocking_errors() {
        let mut agg = AggregatedHookResult::new();

        let r1 = HookResult::err("OnBeforeWrite", "DNS 配置无效");
        agg.merge(r1);

        let r2 = HookResult::ok("OnMerged").with_blocking_error("端口冲突");
        agg.merge(r2);

        assert!(agg.has_errors());
        assert!(!agg.is_success());
        assert_eq!(agg.blocking_errors.len(), 2);
        assert!(agg.prevent_continuation);
    }

    #[test]
    fn test_aggregated_result_config_chain() {
        let mut agg = AggregatedHookResult::new();

        // 钩子 A 修改配置
        agg.merge(HookResult::ok("A").with_modified_config(serde_json::json!({"step": "a"})));

        // 钩子 B 在 A 的基础上修改
        agg.merge(HookResult::ok("B").with_modified_config(serde_json::json!({"step": "b"})));

        // 钩子 C 不修改配置
        agg.merge(HookResult::ok("C"));

        // 最终配置应为 B 的输出（链式传递）
        assert_eq!(agg.modified_config.unwrap()["step"], "b");
    }

    #[test]
    fn test_aggregated_result_report() {
        let mut agg = AggregatedHookResult::new();
        agg.merge(HookResult::ok("OnMerged").with_message("合并完成"));
        agg.merge(HookResult::err("OnBeforeWrite", "校验失败"));

        let report = agg.report();
        assert!(report.contains("2 个钩子"));
        assert!(report.contains("1 成功"));
        assert!(report.contains("1 失败"));
        assert!(report.contains("合并完成"));
        assert!(report.contains("校验失败"));
    }

    #[test]
    fn test_aggregated_result_report_empty() {
        let agg = AggregatedHookResult::new();
        let report = agg.report();
        assert!(report.contains("无钩子执行记录"));
    }

    // ── HookCondition 测试 ──

    #[test]
    fn test_hook_condition_exact_event() {
        let cond = HookCondition {
            expression: "OnBeforeWrite".to_string(),
        };
        let ctx = HookContext::new("OnBeforeWrite");
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnMerged");
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_dns_changed() {
        let cond = HookCondition {
            expression: "dns_changed".to_string(),
        };
        let ctx = HookContext::new("dns_changed");
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("other_event");
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_patch_count_greater() {
        let cond = HookCondition {
            expression: "patch_count > 0".to_string(),
        };
        let ctx = HookContext::new("OnMerged").with_patch_count(5);
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnMerged").with_patch_count(0);
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_patch_count_equals() {
        let cond = HookCondition {
            expression: "patch_count == 3".to_string(),
        };
        let ctx = HookContext::new("OnMerged").with_patch_count(3);
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnMerged").with_patch_count(4);
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_event_equals() {
        let cond = HookCondition {
            expression: "event == OnBeforeWrite".to_string(),
        };
        let ctx = HookContext::new("OnBeforeWrite");
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnMerged");
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_event_not_equals() {
        let cond = HookCondition {
            expression: "event != OnShutdown".to_string(),
        };
        let ctx = HookContext::new("OnMerged");
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnShutdown");
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_modified_paths_contains() {
        let cond = HookCondition {
            expression: "modified_paths contains dns".to_string(),
        };
        let ctx = HookContext::new("OnMerged")
            .with_modified_path("config.dns.servers")
            .with_modified_path("config.routes");
        assert!(cond.evaluate(&ctx));

        let ctx2 = HookContext::new("OnMerged").with_modified_path("config.routes");
        assert!(!cond.evaluate(&ctx2));
    }

    #[test]
    fn test_hook_condition_unparseable() {
        let cond = HookCondition {
            expression: "some gibberish !!!".to_string(),
        };
        let ctx = HookContext::new("OnMerged");
        // 无法解析的表达式 → 安全失败 → false
        assert!(!cond.evaluate(&ctx));
    }

    // ── HookContext 测试 ──

    #[test]
    fn test_hook_context_builder() {
        let ctx = HookContext::new("OnBeforeWrite")
            .with_modified_path("dns.servers")
            .with_modified_path("routes")
            .with_patch_count(3)
            .with_extra("source", "subscription");

        assert_eq!(ctx.event, "OnBeforeWrite");
        assert_eq!(ctx.modified_paths.len(), 2);
        assert_eq!(ctx.patch_count, 3);
        assert_eq!(ctx.extra.get("source").unwrap(), "subscription");
    }

    // ── Serde 兼容性测试 ──

    #[test]
    fn test_hook_result_serde_roundtrip() {
        let result = HookResult::ok("OnMerged")
            .with_message("测试消息")
            .with_modified_config(serde_json::json!({"test": true}));

        let json = serde_json::to_string(&result).unwrap();
        let deserialized: HookResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.hook_name, "OnMerged");
        assert!(deserialized.success);
        assert_eq!(deserialized.messages.len(), 1);
        assert!(deserialized.modified_config.is_some());
    }

    #[test]
    fn test_aggregated_result_serde_roundtrip() {
        let mut agg = AggregatedHookResult::new();
        agg.merge(HookResult::ok("A").with_message("msg"));
        agg.merge(HookResult::err("B", "err"));

        let json = serde_json::to_string(&agg).unwrap();
        let deserialized: AggregatedHookResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.hook_results.len(), 2);
        assert_eq!(deserialized.messages.len(), 1);
        assert_eq!(deserialized.blocking_errors.len(), 1);
    }

    #[test]
    fn test_hook_condition_serde_roundtrip() {
        let cond = HookCondition {
            expression: "patch_count > 0".to_string(),
        };
        let json = serde_json::to_string(&cond).unwrap();
        let deserialized: HookCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.expression, "patch_count > 0");
    }

    // ─── HookCondition 条件求值 ───

    #[test]
    fn test_hook_condition_event_name_containing_contains() {
        // 事件名本身包含 "contains" 不应被误分类
        let condition = HookCondition {
            expression: "event_contains_data".to_string(),
        };
        let ctx = HookContext::new("event_contains_data");
        // "event_contains_data" 不含空格，应被分类为精确事件名匹配
        assert!(condition.evaluate(&ctx));
    }

    #[test]
    fn test_hook_condition_path_contains_keyword() {
        // "modified_paths contains proxies" 应被分类为路径包含检查
        let condition = HookCondition {
            expression: "modified_paths contains proxies".to_string(),
        };
        let ctx = HookContext::new("some_event")
            .with_modified_path("proxies")
            .with_modified_path("rules");
        assert!(condition.evaluate(&ctx));
    }

    #[test]
    fn test_hook_condition_path_contains_no_match() {
        let condition = HookCondition {
            expression: "modified_paths contains dns".to_string(),
        };
        let ctx = HookContext::new("some_event").with_modified_path("proxies");
        assert!(!condition.evaluate(&ctx));
    }

    #[test]
    fn test_hook_condition_numeric_comparison() {
        let condition = HookCondition {
            expression: "patch_count > 5".to_string(),
        };
        let ctx = HookContext::new("some_event").with_patch_count(10);
        assert!(condition.evaluate(&ctx));

        let ctx2 = HookContext::new("some_event").with_patch_count(3);
        assert!(!condition.evaluate(&ctx2));
    }
}
