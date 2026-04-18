//! # Validator — Configuration Legality Validation + Smart Suggestions
//!
//! ## Responsibilities
//!
//! - Field legality validation (JSON Schema level)
//! - Proxy name uniqueness check
//! - Proxy group reference integrity check
//! - DNS configuration completeness check
//! - Smart suggestions (heuristic-based)
//!
//! ## Validation Flow
//!
//! ```text
//! Final Config → Validator::validate() → ValidationResult
//!                                              ├── errors[] (blocking)
//!                                              └── warnings[] (non-blocking, with suggestions)
//! ```

/// Special proxy names that are built-in to proxy cores and should be
/// skipped during group reference integrity checks.
const SPECIAL_PROXY_NAMES: &[&str] = &["DIRECT", "REJECT", "PASS", "COMPATIBLE"];

/// 校验结果
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationWarning>,
    /// 是否通过校验（无 error 即为通过）
    pub is_valid: bool,
}

impl ValidationResult {
    pub fn new() -> Self {
        Self {
            errors: vec![],
            warnings: vec![],
            is_valid: true,
        }
    }

    pub fn with_error(mut self, err: ValidationError) -> Self {
        self.is_valid = false;
        self.errors.push(err);
        self
    }

    pub fn with_warning(mut self, warn: ValidationWarning) -> Self {
        self.warnings.push(warn);
        self
    }
}

impl Default for ValidationResult {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationResult {
    /// 合并两个校验结果
    pub fn merge(self, other: ValidationResult) -> ValidationResult {
        let is_valid = self.is_valid && other.is_valid;
        let mut errors = self.errors;
        errors.extend(other.errors);
        let mut warnings = self.warnings;
        warnings.extend(other.warnings);
        ValidationResult {
            errors,
            warnings,
            is_valid,
        }
    }
}

/// 校验错误（阻断执行）
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.path, self.message)
    }
}

/// 校验警告（不阻断，仅提示）
#[derive(Debug, Clone)]
pub struct ValidationWarning {
    pub path: String,
    pub message: String,
    pub suggestion: Option<String>,
}

impl std::fmt::Display for ValidationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(suggestion) = &self.suggestion {
            write!(f, "[{}] {} (建议: {})", self.path, self.message, suggestion)
        } else {
            write!(f, "[{}] {}", self.path, self.message)
        }
    }
}

/// 配置校验器
pub struct Validator;

impl Validator {
    /// 对完整配置执行所有校验
    pub fn validate(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        result = result.merge(Self::check_proxy_names_unique(config));
        result = result.merge(Self::check_proxy_group_names_unique(config));
        result = result.merge(Self::check_proxy_group_references(config));
        result = result.merge(Self::check_proxy_group_use_references(config));
        result = result.merge(Self::check_dns_config(config));
        result = result.merge(Self::check_proxy_required_fields(config));
        result = result.merge(Self::check_smart_suggestions(config));

        result
    }

    // ─── 具体校验规则 ───

    /// 检查代理名称唯一性
    fn check_proxy_names_unique(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        if let Some(proxies) = config.get("proxies").and_then(|v| v.as_array()) {
            let mut name_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();

            for proxy in proxies {
                if let Some(name) = proxy.get("name").and_then(|v| v.as_str()) {
                    *name_counts.entry(name.to_string()).or_insert(0) += 1;
                }
            }

            for (name, &count) in &name_counts {
                if count > 1 {
                    result = result.with_error(ValidationError {
                        path: "proxies".into(),
                        message: format!("代理名称重复: 「{}」出现 {} 次", name, count),
                    });
                }
            }
        }

        result
    }

    fn check_proxy_group_names_unique(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        if let Some(groups) = config.get("proxy-groups").and_then(|v| v.as_array()) {
            let mut name_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();

            for group in groups {
                if let Some(name) = group.get("name").and_then(|v| v.as_str()) {
                    *name_counts.entry(name.to_string()).or_insert(0) += 1;
                }
            }

            for (name, &count) in &name_counts {
                if count > 1 {
                    result = result.with_error(ValidationError {
                        path: "proxy-groups".into(),
                        message: format!("代理组名称重复: 「{}」出现 {} 次", name, count),
                    });
                }
            }
        }

        result
    }

    /// 检查代理组引用的代理是否都存在，以及代理组之间是否存在循环引用
    fn check_proxy_group_references(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        // 收集所有已存在的代理名称
        let proxy_names: std::collections::HashSet<String> = config
            .get("proxies")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        p.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 收集所有代理组名称
        let group_names: std::collections::HashSet<String> = config
            .get("proxy-groups")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| {
                        g.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 图的节点是代理组名称，边 A→B 表示组 A 引用了组 B
        let mut group_refs: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if let Some(groups) = config.get("proxy-groups").and_then(|v| v.as_array()) {
            for group in groups {
                if let Some(group_name) = group.get("name").and_then(|v| v.as_str()) {
                    let mut refs = Vec::new();
                    if let Some(proxies) = group.get("proxies").and_then(|v| v.as_array()) {
                        for proxy_ref in proxies {
                            if let Some(ref_name) = proxy_ref.as_str()
                                && !SPECIAL_PROXY_NAMES.contains(&ref_name)
                                && !is_filter_selector(ref_name)
                                && group_names.contains(ref_name)
                            {
                                refs.push(ref_name.to_string());
                            }
                        }
                    }
                    // Also check `use` field for group chaining
                    if let Some(use_groups) = group.get("use").and_then(|v| v.as_array()) {
                        for use_ref in use_groups {
                            if let Some(ref_name) = use_ref.as_str()
                                && group_names.contains(ref_name)
                            {
                                refs.push(ref_name.to_string());
                            }
                        }
                    }
                    group_refs.insert(group_name.to_string(), refs);
                }
            }
        }

        // DFS 环检测
        let mut visiting = std::collections::HashSet::<String>::new();
        let mut visited = std::collections::HashSet::<String>::new();
        for group_name in &group_names {
            if !visited.contains(group_name)
                && let Some(cycle) =
                    detect_cycle(group_name, &group_refs, &mut visiting, &mut visited)
            {
                result = result.with_error(ValidationError {
                    path: "proxy-groups".to_string(),
                    message: format!("代理组存在循环引用: {}", cycle),
                });
            }
        }

        if let Some(groups) = config.get("proxy-groups").and_then(|v| v.as_array()) {
            for group in groups {
                let group_name = match group.get("name").and_then(|v| v.as_str()) {
                    Some(name) => name,
                    None => {
                        result = result.with_error(ValidationError {
                            path: "proxy-groups[].name".to_string(),
                            message: "proxy-group entry is missing required 'name' field"
                                .to_string(),
                        });
                        continue;
                    }
                };

                if let Some(proxies) = group.get("proxies").and_then(|v| v.as_array()) {
                    for proxy_ref in proxies {
                        if let Some(ref_name) = proxy_ref.as_str() {
                            // 特殊名称跳过检查
                            if SPECIAL_PROXY_NAMES.contains(&ref_name) {
                                continue;
                            }

                            // 如果引用的不是已知代理也不是已知组名，则报错
                            // 排除 mihomo filter 选择器语法：
                            //   !!prefix — 排除匹配前缀的节点
                            //   NAME(regexp), TYPE(regexp), etc. — 正则过滤语法
                            if !proxy_names.contains(ref_name)
                                && !group_names.contains(ref_name)
                                && !is_filter_selector(ref_name)
                            {
                                result = result.with_error(ValidationError {
                                    path: format!("proxy-groups.{}", group_name),
                                    message: format!("引用不存在的代理/组: 「{}」", ref_name),
                                });
                            }
                        }
                    }
                }
            }
        }

        result
    }

    /// Check that `use` field references in proxy-groups point to existing groups.
    ///
    /// In mihomo/clash-rs, the `use` field in a proxy-group references other
    /// proxy-groups by name (for group chaining). This validates that all
    /// referenced groups actually exist.
    fn check_proxy_group_use_references(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        let group_names: std::collections::HashSet<String> = config
            .get("proxy-groups")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| {
                        g.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Some(groups) = config.get("proxy-groups").and_then(|v| v.as_array()) {
            for group in groups {
                let group_name = group
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                if let Some(use_groups) = group.get("use").and_then(|v| v.as_array()) {
                    for use_ref in use_groups {
                        if let Some(ref_name) = use_ref.as_str() {
                            if is_filter_selector(ref_name) {
                                continue;
                            }
                            if !group_names.contains(ref_name) {
                                result = result.with_error(ValidationError {
                                    path: format!("proxy-groups.{}.use", group_name),
                                    message: format!("use 引用不存在的代理组: 「{}」", ref_name),
                                });
                            }
                        }
                    }
                }
            }
        }

        result
    }

    /// DNS 配置完整性检查
    fn check_dns_config(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        if let Some(dns) = config.get("dns") {
            // 开了 TUN 但没开 DNS
            if let Some(tun) = config.get("tun") {
                let tun_enabled = tun.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
                let dns_enabled = dns.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);

                if tun_enabled && !dns_enabled {
                    result = result.with_warning(ValidationWarning {
                        path: "dns".into(),
                        message: "TUN 已启用但 DNS 未启用".into(),
                        suggestion: Some(
                            "建议在 dns.enable 中设置 true 以确保 TUN 正常工作".into(),
                        ),
                    });
                }

                // TUN + fake-ip 推荐
                if tun_enabled && dns_enabled {
                    let enhanced_mode = dns
                        .get("enhanced-mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if enhanced_mode != "fake-ip" {
                        result = result.with_warning(ValidationWarning {
                            path: "dns.enhanced-mode".into(),
                            message: "TUN 模式推荐使用 fake-ip 模式".into(),
                            suggestion: Some(
                                "设置 dns.enhanced-mode 为 \"fake-ip\" 可获得更好的兼容性".into(),
                            ),
                        });
                    }
                }
            }
        }

        result
    }

    /// 检查 proxies 数组中每个代理的必要字段
    ///
    /// 必要字段：name, type, server。
    /// 缺少这些字段的代理节点无法正常工作，应报错。
    fn check_proxy_required_fields(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        if let Some(proxies) = config.get("proxies").and_then(|v| v.as_array()) {
            for (idx, proxy) in proxies.iter().enumerate() {
                // 检查 name 字段
                if proxy.get("name").is_none() {
                    result = result.with_error(ValidationError {
                        path: format!("proxies[{}]", idx),
                        message: "代理节点缺少必要字段「name」".into(),
                    });
                } else if !proxy.get("name").unwrap().is_string() {
                    result = result.with_error(ValidationError {
                        path: format!("proxies[{}].name", idx),
                        message: "代理节点字段「name」类型错误，期望字符串".into(),
                    });
                }
                // 检查 type 字段
                if proxy.get("type").is_none() {
                    result = result.with_error(ValidationError {
                        path: format!("proxies[{}]", idx),
                        message: "代理节点缺少必要字段「type」".into(),
                    });
                }
                // 检查 server 字段
                if proxy.get("server").is_none() {
                    result = result.with_error(ValidationError {
                        path: format!("proxies[{}]", idx),
                        message: "代理节点缺少必要字段「server」".into(),
                    });
                }
            }
        }

        result
    }

    /// 智能建议（基于配置模式的启发式建议）
    fn check_smart_suggestions(config: &serde_json::Value) -> ValidationResult {
        let mut result = ValidationResult::new();

        // 建议：rules 末尾应有 MATCH 规则
        if let Some(rules) = config.get("rules").and_then(|v| v.as_array()) {
            let has_match = rules.iter().any(|r| match r {
                serde_json::Value::String(s) => s.split(',').next() == Some("MATCH"),
                _ => false,
            });

            if rules.is_empty() {
                result = result.with_warning(ValidationWarning {
                    path: "rules".into(),
                    message: "规则列表为空，所有流量将无法匹配".into(),
                    suggestion: Some("至少添加一条 MATCH 规则作为兜底".into()),
                });
            } else if !has_match {
                result = result.with_warning(ValidationWarning {
                    path: "rules".into(),
                    message: "规则列表末尾缺少 MATCH 兜底规则".into(),
                    suggestion: Some("建议在 rules 末尾追加 \"MATCH,PROXY\" 作为默认策略".into()),
                });
            }
        }

        result
    }
}

/// DFS-based cycle detection in a directed graph (iterative implementation).
///
/// Returns `Some(cycle_description)` if a cycle is found, `None` otherwise.
/// Uses an explicit stack to avoid stack overflow on deeply nested dependency
/// graphs from adversarial input.
fn detect_cycle(
    start: &str,
    graph: &std::collections::HashMap<String, Vec<String>>,
    visiting: &mut std::collections::HashSet<String>,
    visited: &mut std::collections::HashSet<String>,
) -> Option<String> {
    // Iterative DFS using an explicit stack.
    // Each stack frame is (node, neighbor_iterator_start_index).
    // We also maintain a parallel path stack for cycle reconstruction.
    let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
    let mut path: Vec<String> = vec![start.to_string()];
    let mut path_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    path_set.insert(start.to_string());

    while let Some(&(current, neighbor_idx)) = stack.last() {
        if visited.contains(current) {
            stack.pop();
            if let Some(popped) = path.pop() {
                path_set.remove(&popped);
            }
            continue;
        }

        let neighbors = match graph.get(current) {
            Some(n) => n,
            None => {
                // No outgoing edges — backtrack
                visiting.remove(current);
                visited.insert(current.to_string());
                stack.pop();
                if let Some(popped) = path.pop() {
                    path_set.remove(&popped);
                }
                continue;
            }
        };

        // Try to find an unvisited neighbor starting from neighbor_idx
        let mut found_next = false;
        for (i, neighbor) in neighbors.iter().enumerate().skip(neighbor_idx) {
            if path_set.contains(neighbor) {
                // Cycle detected — reconstruct the cycle path
                let cycle_start = path.iter().position(|p| p == neighbor);
                let cycle_str = if let Some(idx) = cycle_start {
                    let cycle_part: Vec<&str> = path[idx..].iter().map(|s| s.as_str()).collect();
                    format!("{} -> {}", cycle_part.join(" -> "), neighbor)
                } else {
                    format!("{} -> {}", current, neighbor)
                };
                return Some(cycle_str);
            }

            if visited.contains(neighbor) {
                continue; // Already fully processed
            }

            // Push current node with updated index, then descend into neighbor
            *stack.last_mut().unwrap() = (current, i + 1);
            stack.push((neighbor, 0));
            path.push(neighbor.clone());
            path_set.insert(neighbor.clone());
            found_next = true;
            break;
        }

        if !found_next {
            // All neighbors exhausted — mark as visited and backtrack
            visiting.remove(current);
            visited.insert(current.to_string());
            stack.pop();
            if let Some(popped) = path.pop() {
                path_set.remove(&popped);
            }
        }
    }

    None
}

/// Check if a proxy reference name is a mihomo/clash-rs filter selector.
///
/// Filter selectors are special syntax in proxy-group `proxies` arrays that
/// dynamically select nodes by pattern matching, not by explicit name reference.
///
/// Recognized patterns:
/// - `!!prefix` — exclude nodes whose name starts with `prefix`
/// - `NAME(regexp)` — regex match on node name
/// - `TYPE(regexp)` — regex match on node type (e.g., Shadowsocks, VMess)
/// - `REGEXP(regexp)` — generic regex match
/// - Other `KEYWORD(value)` forms — mihomo filter syntax extensions
///
/// The keyword list below is hardcoded to match mihomo's supported filter keywords.
/// If mihomo adds new filter keywords in future releases, this list must be updated accordingly.
/// Reference: https://wiki.metacubex.one/config/proxy-groups/filter/
fn is_filter_selector(ref_name: &str) -> bool {
    // !!exclude prefix
    if ref_name.starts_with("!!") {
        return true;
    }
    // KEYWORD(value) pattern — filter selectors use uppercase keywords
    // followed by parenthesized arguments
    if let Some(paren_pos) = ref_name.find('(')
        && paren_pos > 0
        && ref_name.ends_with(')')
    {
        let keyword = &ref_name[..paren_pos];
        // Known mihomo filter keywords
        // MATCH is a valid filter selector keyword that matches all nodes (equivalent
        // to a catch-all pattern). It is distinct from the MATCH rule keyword in the
        // rules array. Reference: https://wiki.metacubex.one/config/proxy-groups/filter/
        return matches!(
            keyword,
            "NAME"
                | "TYPE"
                | "REGEXP"
                | "KEYWORD"
                | "GEOSITE"
                | "GEOIP"
                | "IP_CIDR"
                | "SRC_IP_CIDR"
                | "DST_IP_CIDR"
                | "PROCESS_NAME"
                | "PROCESS_PATH"
                | "UID"
                | "IN_PORT"
                | "OUT_PORT"
                | "NETWORK"
                | "DST_PORT"
                | "SRC_PORT"
                | "RULE_SET"
                | "SRC_GEOIP"
                | "DST_GEOIP"
                | "IPASN"
                | "SRC_IPASN"
                | "DST_IPASN"
                | "MATCH"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Validator::validate 干净配置 ───

    #[test]
    fn test_validate_clean_config_no_errors() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "p1", "type": "ss", "server": "1.1.1.1", "port": 443},
                {"name": "p2", "type": "vmess", "server": "2.2.2.2", "port": 8080}
            ],
            "proxy-groups": [
                {"name": "auto", "type": "url-test", "proxies": ["p1", "p2"]}
            ],
            "rules": [
                "MATCH,DIRECT"
            ]
        });
        let result = Validator::validate(&config);
        assert!(
            result.is_valid,
            "Clean config should have no errors: {:?}",
            result.errors
        );
    }

    // ─── 代理名称重复检测 ───

    #[test]
    fn test_validate_duplicate_proxy_name() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "dup", "type": "ss", "server": "1.1.1.1", "port": 443},
                {"name": "dup", "type": "vmess", "server": "2.2.2.2", "port": 8080}
            ]
        });
        let result = Validator::validate(&config);
        assert!(!result.is_valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.path == "proxies" && e.message.contains("dup"))
        );
    }

    #[test]
    fn test_validate_triple_duplicate_proxy_name() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "x", "type": "ss", "server": "1.1.1.1", "port": 1},
                {"name": "x", "type": "ss", "server": "1.1.1.1", "port": 2},
                {"name": "x", "type": "ss", "server": "1.1.1.1", "port": 3}
            ]
        });
        let result = Validator::validate(&config);
        assert!(!result.is_valid);
        assert!(result.errors.iter().any(|e| e.message.contains("3 次")));
    }

    // ─── 代理组名称重复检测 ───

    #[test]
    fn test_validate_duplicate_group_name() {
        let config = serde_json::json!({
            "proxies": [],
            "proxy-groups": [
                {"name": "auto", "type": "url-test", "proxies": []},
                {"name": "auto", "type": "select", "proxies": []}
            ]
        });
        let result = Validator::validate(&config);
        assert!(!result.is_valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.path == "proxy-groups" && e.message.contains("auto"))
        );
    }

    // ─── 代理组引用不存在的代理 ───

    #[test]
    fn test_validate_group_references_nonexistent_proxy() {
        let config = serde_json::json!({
            "proxies": [
                {"name": "p1", "type": "ss", "server": "1.1.1.1", "port": 443}
            ],
            "proxy-groups": [
                {"name": "auto", "type": "url-test", "proxies": ["p1", "ghost-proxy"]}
            ]
        });
        let result = Validator::validate(&config);
        assert!(!result.is_valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("ghost-proxy"))
        );
    }

    // ─── 代理组引用不存在的组 ───

    #[test]
    fn test_validate_group_references_nonexistent_group() {
        let config = serde_json::json!({
            "proxies": [],
            "proxy-groups": [
                {"name": "select", "type": "select", "proxies": ["nonexistent-group"]}
            ]
        });
        let result = Validator::validate(&config);
        assert!(!result.is_valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("nonexistent-group"))
        );
    }

    // ─── 特殊名称跳过重复检查 ───

    #[test]
    fn test_special_names_skip_duplicate_check() {
        let config = serde_json::json!({
            "proxies": [],
            "proxy-groups": [
                {"name": "g1", "type": "select", "proxies": ["DIRECT", "REJECT", "PASS", "COMPATIBLE"]}
            ]
        });
        let result = Validator::validate(&config);
        // 特殊名称不应触发引用错误
        let ref_errors: Vec<_> = result
            .errors
            .iter()
            .filter(|e| {
                e.message.contains("DIRECT")
                    || e.message.contains("REJECT")
                    || e.message.contains("PASS")
                    || e.message.contains("COMPATIBLE")
            })
            .collect();
        assert!(
            ref_errors.is_empty(),
            "Special names should not cause errors: {:?}",
            ref_errors
        );
    }

    // ─── TUN 启用但 DNS 禁用 → 警告 ───

    #[test]
    fn test_validate_tun_enabled_dns_disabled_warning() {
        let config = serde_json::json!({
            "tun": {"enable": true},
            "dns": {"enable": false}
        });
        let result = Validator::validate(&config);
        assert!(result.is_valid); // Warning, not error
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.message.contains("TUN") && w.message.contains("DNS"))
        );
    }

    #[test]
    fn test_validate_tun_enabled_dns_enabled_no_warning() {
        let config = serde_json::json!({
            "tun": {"enable": true},
            "dns": {"enable": true, "enhanced-mode": "fake-ip"}
        });
        let result = Validator::validate(&config);
        let tun_dns_warnings: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.message.contains("TUN") && w.message.contains("DNS"))
            .collect();
        assert!(tun_dns_warnings.is_empty());
    }

    // ─── 空规则列表 → 警告 ───

    #[test]
    fn test_validate_empty_rules_warning() {
        let config = serde_json::json!({
            "rules": []
        });
        let result = Validator::validate(&config);
        assert!(result.is_valid);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.path == "rules" && w.message.contains("空"))
        );
    }

    // ─── 规则无 MATCH 兜底 → 警告 ───

    #[test]
    fn test_validate_no_match_rule_warning() {
        let config = serde_json::json!({
            "rules": [
                "DOMAIN,example.com,DIRECT",
                "IP-CIDR,192.168.0.0/16,DIRECT"
            ]
        });
        let result = Validator::validate(&config);
        assert!(result.is_valid);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.path == "rules" && w.message.contains("MATCH"))
        );
    }

    #[test]
    fn test_validate_has_match_no_warning() {
        let config = serde_json::json!({
            "rules": [
                "DOMAIN,example.com,DIRECT",
                "MATCH,PROXY"
            ]
        });
        let result = Validator::validate(&config);
        let match_warnings: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.path == "rules" && w.message.contains("MATCH"))
            .collect();
        assert!(match_warnings.is_empty());
    }

    // ─── ValidationResult ───

    #[test]
    fn test_validation_result_new() {
        let result = ValidationResult::new();
        assert!(result.is_valid);
        assert!(result.errors.is_empty());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_validation_result_default() {
        let result = ValidationResult::default();
        assert!(result.is_valid);
    }

    #[test]
    fn test_validation_result_with_error() {
        let result = ValidationResult::new().with_error(ValidationError {
            path: "test".into(),
            message: "err".into(),
        });
        assert!(!result.is_valid);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_validation_result_with_warning() {
        let result = ValidationResult::new().with_warning(ValidationWarning {
            path: "test".into(),
            message: "warn".into(),
            suggestion: None,
        });
        assert!(result.is_valid); // Warnings don't affect validity
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn test_validation_result_merge() {
        let a = ValidationResult::new()
            .with_error(ValidationError {
                path: "a".into(),
                message: "err-a".into(),
            })
            .with_warning(ValidationWarning {
                path: "a".into(),
                message: "warn-a".into(),
                suggestion: None,
            });
        let b = ValidationResult::new()
            .with_error(ValidationError {
                path: "b".into(),
                message: "err-b".into(),
            })
            .with_warning(ValidationWarning {
                path: "b".into(),
                message: "warn-b".into(),
                suggestion: None,
            });
        let merged = a.merge(b);
        assert!(!merged.is_valid);
        assert_eq!(merged.errors.len(), 2);
        assert_eq!(merged.warnings.len(), 2);
    }

    #[test]
    fn test_validation_result_merge_both_valid() {
        let a = ValidationResult::new();
        let b = ValidationResult::new();
        let merged = a.merge(b);
        assert!(merged.is_valid);
    }

    #[test]
    fn test_validation_result_merge_one_invalid() {
        let a = ValidationResult::new().with_error(ValidationError {
            path: "x".into(),
            message: "err".into(),
        });
        let b = ValidationResult::new();
        let merged = a.merge(b);
        assert!(!merged.is_valid);
    }

    // ─── ValidationError Display ───

    #[test]
    fn test_validation_error_display() {
        let err = ValidationError {
            path: "proxies".into(),
            message: "名称重复".into(),
        };
        let display = format!("{}", err);
        assert_eq!(display, "[proxies] 名称重复");
    }

    // ─── ValidationWarning Display ───

    #[test]
    fn test_validation_warning_display_with_suggestion() {
        let warn = ValidationWarning {
            path: "rules".into(),
            message: "缺少 MATCH".into(),
            suggestion: Some("添加 MATCH 规则".into()),
        };
        let display = format!("{}", warn);
        assert!(display.contains("[rules]"));
        assert!(display.contains("缺少 MATCH"));
        assert!(display.contains("添加 MATCH 规则"));
    }

    #[test]
    fn test_validation_warning_display_without_suggestion() {
        let warn = ValidationWarning {
            path: "dns".into(),
            message: "DNS 未启用".into(),
            suggestion: None,
        };
        let display = format!("{}", warn);
        assert_eq!(display, "[dns] DNS 未启用");
    }

    // ─── 边界情况 ───

    #[test]
    fn test_validate_empty_config() {
        let config = serde_json::json!({});
        let result = Validator::validate(&config);
        assert!(result.is_valid);
    }

    #[test]
    fn test_validate_null_config() {
        let config = serde_json::Value::Null;
        let result = Validator::validate(&config);
        assert!(result.is_valid);
    }

    #[test]
    fn test_validate_proxies_without_name_field() {
        let config = serde_json::json!({
            "proxies": [
                {"type": "ss", "server": "1.1.1.1", "port": 443},
                {"type": "ss", "server": "2.2.2.2", "port": 443}
            ]
        });
        let result = Validator::validate(&config);
        // 缺少 name 字段应报错
        assert!(!result.is_valid, "缺少 name 字段应校验失败");
        assert!(
            result.errors.iter().any(|e| e.message.contains("name")),
            "应报告缺少 name 字段: {:?}",
            result.errors
        );
    }

    #[test]
    fn test_validate_filter_selector_skipped() {
        let config = serde_json::json!({
            "proxies": [],
            "proxy-groups": [
                {"name": "g", "type": "select", "proxies": ["!!//regex"]}
            ]
        });
        let result = Validator::validate(&config);
        // Filter selectors (starting with !!) should be skipped
        let ref_errors: Vec<_> = result
            .errors
            .iter()
            .filter(|e| e.message.contains("regex"))
            .collect();
        assert!(ref_errors.is_empty());
    }

    #[test]
    fn test_validate_tun_fake_ip_recommendation() {
        let config = serde_json::json!({
            "tun": {"enable": true},
            "dns": {"enable": true, "enhanced-mode": "redir-host"}
        });
        let result = Validator::validate(&config);
        assert!(result.is_valid);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.message.contains("fake-ip"))
        );
    }
}
