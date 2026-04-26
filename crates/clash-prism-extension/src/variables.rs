//! 变量模板替换
//!
//! 将 Patch IR 中 `serde_json::Value` 树里的 `{{var_name}}` 和 `{{var_name|default}}`
//! 模板占位符替换为实际值。
//!
//! ## 优先级
//!
//! ```text
//! Host get_variables()  >  __vars__ 文件声明  >  {{var|inline_default}}
//!    (最高优先级)            (中等优先级)           (最低优先级)
//! ```
//!
//! ## 语法
//!
//! ```yaml
//! __vars__:
//!   proxy: VPN07              # 文件级默认值
//!
//! rules:
//!   $append:
//!     - DOMAIN,example.com,{{proxy}}           # 无内联默认值
//!     - DOMAIN,test.com,{{proxy|DIRECT}}       # 内联默认值 DIRECT
//! ```

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// 匹配 `{{var_name}}` 或 `{{var_name|default_value}}` 的正则
static TEMPLATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{(\w+)(?:\|([^}]*))?\}\}")
        .expect("TEMPLATE_RE regex compilation should not fail")
});

/// 未定义变量信息
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UndefinedVar {
    /// 变量名
    pub name: String,
    /// 所在的原始字符串（用于错误定位）
    pub context: String,
}

/// 替换字符串中的 `{{var_name}}` 和 `{{var_name|default}}` 模板
///
/// ## 优先级
///
/// 1. `variables` 中有该变量 → 使用变量值
/// 2. 模板有内联默认值 `{{var|default}}` → 使用默认值
/// 3. 都没有 → 收集到 `UndefinedVar`，调用方决定是否报错
///
/// ## 返回
///
/// `Ok(replaced_string)` — 所有变量都已解析
/// `Err(undefined_vars)` — 存在未定义且无默认值的变量
pub fn replace_templates(
    input: &str,
    variables: &HashMap<String, String>,
) -> Result<String, Vec<UndefinedVar>> {
    let mut result = input.to_string();
    let mut undefined = Vec::new();

    // 从后往前替换，避免索引偏移
    let matches: Vec<_> = TEMPLATE_RE.find_iter(input).collect();

    for mat in matches.into_iter().rev() {
        let full = mat.as_str();
        let caps = TEMPLATE_RE
            .captures(full)
            .expect("match always has captures");

        let var_name = &caps[1];
        let inline_default = caps.get(2).map(|m| m.as_str());

        let replacement = if let Some(value) = variables.get(var_name) {
            value.clone()
        } else if let Some(default) = inline_default {
            default.to_string()
        } else {
            undefined.push(UndefinedVar {
                name: var_name.to_string(),
                context: input.to_string(),
            });
            continue; // 不替换，保留原始占位符
        };

        result.replace_range(mat.range(), &replacement);
    }

    if undefined.is_empty() {
        Ok(result)
    } else {
        Err(undefined)
    }
}

/// 递归遍历 `serde_json::Value` 树，替换所有字符串中的模板
///
/// - `Value::String` → 执行模板替换
/// - `Value::Object` / `Value::Array` → 递归处理子节点
/// - `Value::Number` / `Value::Bool` / `Value::Null` → 跳过
pub fn substitute_in_value(
    value: &mut serde_json::Value,
    variables: &HashMap<String, String>,
) -> Result<(), Vec<UndefinedVar>> {
    let mut all_undefined = Vec::new();

    substitute_in_value_recursive(value, variables, &mut all_undefined);

    if all_undefined.is_empty() {
        Ok(())
    } else {
        Err(all_undefined)
    }
}

fn substitute_in_value_recursive(
    value: &mut serde_json::Value,
    variables: &HashMap<String, String>,
    undefined: &mut Vec<UndefinedVar>,
) {
    match value {
        serde_json::Value::String(s) if s.contains("{{") => match replace_templates(s, variables) {
            Ok(replaced) => *s = replaced,
            Err(mut undefs) => undefined.append(&mut undefs),
        },
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_in_value_recursive(v, variables, undefined);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                substitute_in_value_recursive(v, variables, undefined);
            }
        }
        _ => {} // Number, Bool, Null — 不处理
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_exact_match() {
        let mut vars = HashMap::new();
        vars.insert("proxy".to_string(), "一分机场".to_string());
        assert_eq!(
            replace_templates("DOMAIN,example.com,{{proxy}}", &vars).unwrap(),
            "DOMAIN,example.com,一分机场"
        );
    }

    #[test]
    fn test_replace_inline_default() {
        let vars = HashMap::new();
        assert_eq!(
            replace_templates("DOMAIN,example.com,{{proxy|DIRECT}}", &vars).unwrap(),
            "DOMAIN,example.com,DIRECT"
        );
    }

    #[test]
    fn test_replace_host_overrides_file_default() {
        let mut vars = HashMap::new();
        vars.insert("proxy".to_string(), "一分机场".to_string());
        // Host 提供的值优先于内联默认值
        assert_eq!(
            replace_templates("DOMAIN,example.com,{{proxy|DIRECT}}", &vars).unwrap(),
            "DOMAIN,example.com,一分机场"
        );
    }

    #[test]
    fn test_replace_undefined_no_default() {
        let vars = HashMap::new();
        let result = replace_templates("DOMAIN,example.com,{{proxy}}", &vars);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].name, "proxy");
    }

    #[test]
    fn test_replace_multiple_variables() {
        let mut vars = HashMap::new();
        vars.insert("proxy".to_string(), "一分机场".to_string());
        vars.insert("dns".to_string(), "8.8.8.8".to_string());
        assert_eq!(
            replace_templates("{{dns}},{{proxy}}", &vars).unwrap(),
            "8.8.8.8,一分机场"
        );
    }

    #[test]
    fn test_replace_no_templates() {
        let vars = HashMap::new();
        assert_eq!(
            replace_templates("DOMAIN,example.com,DIRECT", &vars).unwrap(),
            "DOMAIN,example.com,DIRECT"
        );
    }

    #[test]
    fn test_replace_empty_string() {
        let vars = HashMap::new();
        assert_eq!(replace_templates("", &vars).unwrap(), "");
    }

    #[test]
    fn test_substitute_in_json_object() {
        let mut vars = HashMap::new();
        vars.insert("server".to_string(), "1.2.3.4".to_string());

        let mut value = serde_json::json!({
            "dns": {
                "nameserver": ["https://{{server}}/dns-query"]
            }
        });

        assert!(substitute_in_value(&mut value, &vars).is_ok());
        assert_eq!(value["dns"]["nameserver"][0], "https://1.2.3.4/dns-query");
    }

    #[test]
    fn test_substitute_in_json_array() {
        let mut vars = HashMap::new();
        vars.insert("proxy".to_string(), "ProxyGroup".to_string());

        let mut value = serde_json::json!([
            "DOMAIN,example.com,{{proxy}}",
            "DOMAIN-SUFFIX,test.com,{{proxy|DIRECT}}"
        ]);

        assert!(substitute_in_value(&mut value, &vars).is_ok());
        assert_eq!(value[0], "DOMAIN,example.com,ProxyGroup");
        assert_eq!(value[1], "DOMAIN-SUFFIX,test.com,ProxyGroup");
    }

    #[test]
    fn test_substitute_skips_numbers_and_bools() {
        let mut vars = HashMap::new();
        vars.insert("port".to_string(), "53".to_string());

        let mut value = serde_json::json!({
            "enable": true,
            "port": 7890,
            "name": "{{port}}"
        });

        assert!(substitute_in_value(&mut value, &vars).is_ok());
        assert_eq!(value["enable"], true);
        assert_eq!(value["port"], 7890);
        assert_eq!(value["name"], "53");
    }

    #[test]
    fn test_default_with_special_chars() {
        let vars = HashMap::new();
        assert_eq!(
            replace_templates("{{proxy|REJECT}}", &vars).unwrap(),
            "REJECT"
        );
    }
}
