//! JSON Path 辅助工具 — 基于 dot-notation 的 `serde_json::Value` 导航
//!
//! 提供一组统一的函数，通过点分隔路径（如 `"proxies.auto-group"` 或 `"items.0.name"`）
//! 遍历和修改 JSON 值。
//!
//! ## 路径语义
//!
//! - **对象键**：通过字段名导航（`"server.host"` → `obj["server"]["host"]`）
//! - **数组索引**：通过数字字符串导航（`"items.0"` → `arr[0]`）
//! - **空路径**：返回根值本身
//!
//! ## 函数一览
//!
//! | 函数 | 方向 | 可变性 | 数组支持 | 自动创建缺失节点 |
//! |---|---|---|---|---|
//! | `get_json_path` | 读取 | 不可变 | 支持 | 否 |
//! | `get_json_path_mut` | 写入 | 可变 | 支持 | 否 |
//! | `get_or_create_json_path_mut` | 写入 | 可变 | 否（仅对象） | 是 |
//! | `get_array_at_path_mut` | 写入 | 可变 | 支持 | 否 |
//! | `get_array_len` | 读取 | 不可变 | 支持 | 否 |
//! | `set_json_path` | 写入 | 可变 | 否（仅对象） | 是 |

use serde_json::Value;

/// 沿点分隔路径遍历，返回不可变引用。
///
/// 同时支持对象键和数组索引（解析为 `usize`）。
/// 如果任一路径段无法解析，返回 `None`。
pub fn get_json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for part in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(arr) => {
                // 数组索引歧义说明
                // 当路径段可以解析为 usize 时，视为数组索引。
                // 这意味着如果对象键恰好是纯数字（如 {"0": "foo"}），
                // 在数组上下文中会被误解析为数组索引。
                //
                // 在 Prism 的使用场景中，JSON 配置的数组元素不使用纯数字键，
                // 因此此歧义在实际使用中不会造成问题。
                //
                // 上下文感知说明：路径解析是逐段进行的，每段根据当前节点的
                // 实际类型（Object 或 Array）决定解析方式。只有当当前节点
                // 是数组时才会尝试 usize 解析，因此纯数字对象键在对象上下文
                // 中不会被误解析。歧义仅在路径同时包含对象和数组且对象键
                // 为纯数字时才可能发生，这在 Prism 配置格式中不存在。
                let idx: usize = part.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// 沿点分隔路径遍历，返回可变引用。
///
/// 同时支持对象键和数组索引（解析为 `usize`）。
/// 如果任一路径段无法解析，返回 `None`。
pub fn get_json_path_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for part in path.split('.') {
        current = match current {
            Value::Object(map) => map.get_mut(part)?,
            Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                arr.get_mut(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// 沿点分隔路径遍历，自动创建缺失的中间对象节点。
///
/// **仅支持对象路径** — 如果中间节点不是对象（如遇到数组或标量），返回 `None`。
pub fn get_or_create_json_path_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for part in path.split('.') {
        current = match current {
            Value::Object(map) => map
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new())),
            _ => return None,
        };
    }
    Some(current)
}

/// 获取指定路径处数组的可变引用。
///
/// 如果路径未解析到数组值，返回 `None`。
pub fn get_array_at_path_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    get_json_path_mut(value, path).filter(|v| v.is_array())
}

/// 获取指定路径处数组的长度。
///
/// 如果路径未解析到数组，返回 `0`。
pub fn get_array_len(config: &Value, path: &str) -> usize {
    get_json_path(config, path)
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

/// 在指定路径设置值，自动创建缺失的中间对象节点。
///
/// **仅支持对象路径。** 如果路径是单个键，直接插入根对象。
/// 对于多段路径，先导航/创建中间对象，再在最后一段插入键值。
///
/// 如果根节点或任何中间节点不是对象，则不执行任何操作。
pub fn set_json_path(config: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return;
    }

    // 快速路径：单键直接插入
    if parts.len() == 1 {
        if let Some(obj) = config.as_object_mut() {
            obj.insert(parts[0].to_string(), value);
        }
        return;
    }

    // 导航/创建到父节点
    let mut current = config;
    for &part in &parts[..parts.len() - 1] {
        if let Some(obj) = current.as_object_mut() {
            current = obj
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        } else {
            return;
        }
    }

    if let Some(obj) = current.as_object_mut() {
        obj.insert(parts[parts.len() - 1].to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── get_json_path ───

    #[test]
    fn test_get_simple_key() {
        let config = serde_json::json!({"dns": {"enable": true}});
        let result = get_json_path(&config, "dns");
        assert!(result.is_some());
        assert_eq!(result.unwrap()["enable"], true);
    }

    #[test]
    fn test_get_nested_path() {
        let config = serde_json::json!({"dns": {"nameservers": ["8.8.8.8", "1.1.1.1"]}});
        let result = get_json_path(&config, "dns.nameservers");
        assert!(result.is_some());
        assert_eq!(result.unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_get_array_index() {
        let config = serde_json::json!({"proxies": [{"name": "p1"}, {"name": "p2"}]});
        let result = get_json_path(&config, "proxies.0");
        assert!(result.is_some());
        assert_eq!(result.unwrap()["name"], "p1");
    }

    #[test]
    fn test_get_array_index_out_of_bounds() {
        let config = serde_json::json!({"proxies": [{"name": "p1"}]});
        let result = get_json_path(&config, "proxies.99");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_nonexistent_path() {
        let config = serde_json::json!({"dns": {}});
        let result = get_json_path(&config, "dns.nonexistent.deep");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_empty_path_returns_root() {
        let config = serde_json::json!({"a": 1});
        let result = get_json_path(&config, "");
        assert!(result.is_some());
        assert_eq!(result.unwrap()["a"], 1);
    }

    #[test]
    fn test_get_through_non_object_intermediate() {
        let config = serde_json::json!({"dns": "not-an-object"});
        let result = get_json_path(&config, "dns.enable");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_deep_array_index() {
        let config = serde_json::json!({"items": [[1, 2], [3, 4]]});
        let result = get_json_path(&config, "items.1.0");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 3);
    }

    // ─── get_json_path_mut ───

    #[test]
    fn test_get_mut_modify_nested_value() {
        let mut config = serde_json::json!({"dns": {"enable": false}});
        if let Some(target) = get_json_path_mut(&mut config, "dns.enable") {
            *target = serde_json::json!(true);
        }
        assert_eq!(config["dns"]["enable"], true);
    }

    #[test]
    fn test_get_mut_nonexistent_returns_none() {
        let mut config = serde_json::json!({"dns": {}});
        assert!(get_json_path_mut(&mut config, "dns.nonexistent").is_none());
    }

    #[test]
    fn test_get_mut_empty_path() {
        let mut config = serde_json::json!({"a": 1});
        let result = get_json_path_mut(&mut config, "");
        assert!(result.is_some());
        *result.unwrap() = serde_json::json!({"b": 2});
        assert_eq!(config["b"], 2);
    }

    // ─── get_or_create_json_path_mut ───

    #[test]
    fn test_get_or_create_creates_intermediate_objects() {
        let mut config = serde_json::json!({});
        let target = get_or_create_json_path_mut(&mut config, "dns.nameservers");
        assert!(target.is_some());
        // Intermediate objects should be created
        assert!(config.get("dns").is_some());
    }

    #[test]
    fn test_get_or_create_existing_path() {
        let mut config = serde_json::json!({"dns": {"enable": true}});
        let target = get_or_create_json_path_mut(&mut config, "dns.enable");
        assert!(target.is_some());
        assert_eq!(target.unwrap(), &serde_json::json!(true));
    }

    #[test]
    fn test_get_or_create_returns_none_when_intermediate_is_scalar() {
        let mut config = serde_json::json!({"dns": "string-value"});
        let result = get_or_create_json_path_mut(&mut config, "dns.enable");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_or_create_empty_path() {
        let mut config = serde_json::json!({"a": 1});
        let result = get_or_create_json_path_mut(&mut config, "");
        assert!(result.is_some());
    }

    #[test]
    fn test_get_or_create_returns_none_when_intermediate_is_array() {
        let mut config = serde_json::json!({"items": [1, 2, 3]});
        let result = get_or_create_json_path_mut(&mut config, "items.key");
        assert!(result.is_none());
    }

    // ─── set_json_path ───

    #[test]
    fn test_set_single_key() {
        let mut config = serde_json::json!({});
        set_json_path(&mut config, "dns", serde_json::json!({"enable": true}));
        assert_eq!(config["dns"]["enable"], true);
    }

    #[test]
    fn test_set_nested_path() {
        let mut config = serde_json::json!({});
        set_json_path(&mut config, "dns.enable", serde_json::json!(true));
        assert_eq!(config["dns"]["enable"], true);
    }

    #[test]
    fn test_set_deep_nested_creates_intermediates() {
        let mut config = serde_json::json!({});
        set_json_path(&mut config, "a.b.c.d", serde_json::json!(42));
        assert_eq!(config["a"]["b"]["c"]["d"], 42);
    }

    #[test]
    fn test_set_overwrites_existing() {
        let mut config = serde_json::json!({"dns": {"enable": false}});
        set_json_path(&mut config, "dns.enable", serde_json::json!(true));
        assert_eq!(config["dns"]["enable"], true);
    }

    #[test]
    fn test_set_on_non_object_root_noop() {
        let mut config = serde_json::json!("not-an-object");
        set_json_path(&mut config, "key", serde_json::json!(1));
        assert_eq!(config, "not-an-object");
    }

    #[test]
    fn test_set_empty_path_noop() {
        let mut config = serde_json::json!({"a": 1});
        set_json_path(&mut config, "", serde_json::json!(2));
        assert_eq!(config["a"], 1);
    }

    // ─── get_array_at_path_mut ───

    #[test]
    fn test_get_array_mut_on_array_field() {
        let mut config = serde_json::json!({"rules": ["a", "b", "c"]});
        let result = get_array_at_path_mut(&mut config, "rules");
        assert!(result.is_some());
        let arr = result.unwrap();
        assert!(arr.is_array());
        assert_eq!(arr.as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_get_array_mut_on_non_array_returns_none() {
        let mut config = serde_json::json!({"dns": {"enable": true}});
        let result = get_array_at_path_mut(&mut config, "dns.enable");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_array_mut_nonexistent_returns_none() {
        let mut config = serde_json::json!({});
        let result = get_array_at_path_mut(&mut config, "missing");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_array_mut_can_modify() {
        let mut config = serde_json::json!({"items": [1, 2]});
        if let Some(arr) = get_array_at_path_mut(&mut config, "items") {
            arr.as_array_mut().unwrap().push(serde_json::json!(3));
        }
        assert_eq!(config["items"].as_array().unwrap().len(), 3);
    }

    // ─── get_array_len ───

    #[test]
    fn test_get_array_len_correct_count() {
        let config = serde_json::json!({"rules": ["a", "b", "c", "d"]});
        assert_eq!(get_array_len(&config, "rules"), 4);
    }

    #[test]
    fn test_get_array_len_empty_array() {
        let config = serde_json::json!({"rules": []});
        assert_eq!(get_array_len(&config, "rules"), 0);
    }

    #[test]
    fn test_get_array_len_nonexistent_path() {
        let config = serde_json::json!({});
        assert_eq!(get_array_len(&config, "missing"), 0);
    }

    #[test]
    fn test_get_array_len_non_array_returns_zero() {
        let config = serde_json::json!({"dns": "string"});
        assert_eq!(get_array_len(&config, "dns"), 0);
    }

    // ─── 边界情况 ───

    #[test]
    fn test_get_on_null_value() {
        let config = serde_json::Value::Null;
        assert!(get_json_path(&config, "any").is_none());
    }

    #[test]
    fn test_get_mut_on_null_value() {
        let mut config = serde_json::Value::Null;
        assert!(get_json_path_mut(&mut config, "any").is_none());
    }

    #[test]
    fn test_numeric_key_in_object() {
        let config = serde_json::json!({"0": "zero", "1": "one"});
        // "0" is treated as object key, not array index
        let result = get_json_path(&config, "0");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "zero");
    }

    #[test]
    fn test_set_overwrites_array_with_object() {
        let mut config = serde_json::json!({"items": [1, 2]});
        set_json_path(&mut config, "items", serde_json::json!({"key": "value"}));
        assert!(config["items"].is_object());
        assert_eq!(config["items"]["key"], "value");
    }
}
