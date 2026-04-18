//! # 确定性序列化工具
//!
//! 确保相同语义的 JSON 值永远产生相同的字节输出。
//! 用于缓存键计算、配置差异对比和内容寻址。
//!
//! 核心策略：
//! - **递归排序**：所有 JSON Object 的 key 按 Unicode 码点升序排列
//! - **紧凑输出**：无多余空白，确保输出唯一性
//! - **稳定哈希**：基于排序后的紧凑字符串计算哈希值

use serde_json::Value;
use std::collections::BTreeMap;

/// 确定性 JSON 序列化 — key 按字典序排列（美化格式）
///
/// 递归排序所有 Object 的 key，然后使用 `to_string_pretty` 序列化。
/// 适用于需要人类可读的确定性输出的场景。
///
/// # 示例
/// ```
/// use clash_prism_core::serial::deterministic_serialize;
/// let a = serde_json::json!({"z": 1, "a": 2});
/// let b = serde_json::json!({"a": 2, "z": 1});
/// assert_eq!(deterministic_serialize(&a), deterministic_serialize(&b));
/// ```
pub fn deterministic_serialize(value: &Value) -> String {
    let mut sorted = value.clone();
    sort_keys_recursive(&mut sorted);
    serde_json::to_string_pretty(&sorted)
        .expect("deterministic_serialize: sorted JSON Value serialization should never fail")
}

/// 确定性 JSON 序列化 — key 按字典序排列（紧凑格式）
///
/// 同 `deterministic_serialize`，但使用紧凑格式（无缩进和换行）。
/// 适用于缓存键计算和内容寻址等不需要人类可读的场景。
pub fn deterministic_serialize_compact(value: &Value) -> String {
    let mut sorted = value.clone();
    sort_keys_recursive(&mut sorted);
    serde_json::to_string(&sorted).expect(
        "deterministic_serialize_compact: sorted JSON Value serialization should never fail",
    )
}

/// 计算配置内容的确定性哈希（完整 SHA-256）
///
/// 将 JSON 值排序后紧凑序列化，然后使用 SHA-256 计算完整 32 字节哈希。
/// 相同语义的 JSON 值永远产生相同的哈希值。
///
/// # 示例
/// ```
/// use clash_prism_core::serial::config_content_hash;
/// let a = serde_json::json!({"z": 1, "a": 2});
/// let b = serde_json::json!({"a": 2, "z": 1});
/// assert_eq!(config_content_hash(&a), config_content_hash(&b));
/// ```
pub fn config_content_hash(value: &Value) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let serialized = deterministic_serialize_compact(value);
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    hasher.finalize().into()
}

/// 递归排序 JSON Value 中所有 Object 的 key
///
/// 使用 BTreeMap 保证字典序。
///
///
/// BTreeMap 的排序规则为 **Unicode 码点顺序**（Unicode codepoint order），
/// 即按 `char` 的 `u32` 值升序排列。这与 Rust 默认的 `String` 排序一致。
///
/// 注意：Unicode 码点顺序与 Unicode 排序规则（Unicode Collation Algorithm）
/// 不同。例如：
/// - 大写字母排在小写字母之前（`"Z" < "a"`）
/// - 数字字符按 ASCII 值排序（`"0" < "9"`）
/// - 非 ASCII 字符（如中文）按码点值排序
///
/// 对于 Prism 的使用场景（配置键名通常为 ASCII 字母、数字和连字符），
/// 码点顺序与人类预期的字典序一致，无需使用 locale-aware 排序。
///
/// 对 Array 中的每个元素也递归处理。
fn sort_keys_recursive(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // 收集所有 key-value 对，按 key 排序后重建
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| {
                    let mut v_sorted = v.clone();
                    sort_keys_recursive(&mut v_sorted);
                    (k.clone(), v_sorted)
                })
                .collect();
            *map = sorted.into_iter().collect();
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sort_keys_recursive(item);
            }
        }
        // Number, String, Bool, Null 无需排序
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_serialize_stable() {
        let a = serde_json::json!({"z": 1, "a": 2, "m": {"x": 3, "w": 4}});
        let b = serde_json::json!({"a": 2, "m": {"w": 4, "x": 3}, "z": 1});
        assert_eq!(deterministic_serialize(&a), deterministic_serialize(&b));
    }

    #[test]
    fn test_deterministic_serialize_compact_stable() {
        let a = serde_json::json!({"z": 1, "a": 2, "m": {"x": 3, "w": 4}});
        let b = serde_json::json!({"a": 2, "m": {"w": 4, "x": 3}, "z": 1});
        assert_eq!(
            deterministic_serialize_compact(&a),
            deterministic_serialize_compact(&b)
        );
    }

    #[test]
    fn test_deterministic_serialize_key_order() {
        let value = serde_json::json!({"z": 1, "a": 2, "m": 3});
        let result = deterministic_serialize_compact(&value);
        // key 应按 a, m, z 排序
        assert!(result.starts_with(r#"{"a":2,"m":3,"z":1}"#));
    }

    #[test]
    fn test_config_content_hash_deterministic() {
        let a = serde_json::json!({"z": 1, "a": 2});
        let b = serde_json::json!({"a": 2, "z": 1});
        assert_eq!(config_content_hash(&a), config_content_hash(&b));
    }

    #[test]
    fn test_config_content_hash_different_values() {
        let a = serde_json::json!({"a": 1});
        let b = serde_json::json!({"a": 2});
        assert_ne!(config_content_hash(&a), config_content_hash(&b));
    }

    #[test]
    fn test_sort_keys_recursive_nested() {
        let mut value = serde_json::json!({
            "outer_c": {"inner_z": 1, "inner_a": 2},
            "outer_a": [{"z": 3, "a": 4}]
        });
        sort_keys_recursive(&mut value);

        let result = deterministic_serialize_compact(&value);
        // 外层 key: outer_a, outer_c
        // inner key: inner_a, inner_z
        // array element key: a, z
        assert_eq!(
            result,
            r#"{"outer_a":[{"a":4,"z":3}],"outer_c":{"inner_a":2,"inner_z":1}}"#
        );
    }

    #[test]
    fn test_sort_keys_recursive_primitive_types() {
        // 非对象/数组类型不应被修改
        let mut num = Value::Number(42.into());
        sort_keys_recursive(&mut num);
        assert_eq!(num, Value::Number(42.into()));

        let mut s = Value::String("hello".into());
        sort_keys_recursive(&mut s);
        assert_eq!(s, Value::String("hello".into()));

        let mut b = Value::Bool(true);
        sort_keys_recursive(&mut b);
        assert_eq!(b, Value::Bool(true));

        let mut n = Value::Null;
        sort_keys_recursive(&mut n);
        assert_eq!(n, Value::Null);
    }

    #[test]
    fn test_deterministic_serialize_empty() {
        let value = serde_json::json!({});
        let result = deterministic_serialize_compact(&value);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_deterministic_serialize_array() {
        let value = serde_json::json!([3, 1, 2]);
        let result = deterministic_serialize_compact(&value);
        assert_eq!(result, "[3,1,2]");
    }
}
