//! NDJSON (Newline Delimited JSON) 输出支持
//!
//! 参考 Claude Code `ndjsonSafeStringify.ts`:
//!
//! - 转义 U+2028 (LINE SEPARATOR) 和 U+2029 (PARAGRAPH SEPARATOR)
//! - 这两个字符在 ECMA-262 中被视为行终止符，会截断 NDJSON 流
//!
//! 用途：为 CLI 提供 `--output-format ndjson` 机器可读输出，
//! 每行一个 JSON 对象，便于前端 / 管道工具逐行解析。

use serde_json::Value;
use std::io::{self, Write};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// NDJSON 安全序列化
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// NDJSON 安全序列化 — 转义 JavaScript 行终止符
///
/// ECMA-262 规定 U+2028 和 U+2029 属于行终止符（LineTerminator），
/// 但 JSON 规范（RFC 8259）允许它们出现在字符串中。
/// 当 NDJSON 流被 JavaScript 解析器逐行读取时，这两个字符
/// 会导致意外的行分割，从而破坏 JSON 结构。
///
/// 本函数在 `serde_json::to_string` 的基础上额外转义这两个字符，
/// 确保输出对 JavaScript NDJSON 解析器安全。
#[inline]
pub fn ndjson_safe_stringify(value: &Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_default();
    // 转义 U+2028 → \u2028 和 U+2029 → \u2029
    // serde_json 不会自动转义这两个字符（它们在 JSON 中合法）
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// NdjsonWriter — 流式 NDJSON 输出器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// NDJSON 输出器
///
/// 将 JSON 值逐行写入底层 Writer，每行以 `\n` 结尾。
/// 所有输出均经过 [ndjson_safe_stringify] 处理，
/// 确保对 JavaScript NDJSON 解析器安全。
pub struct NdjsonWriter<W: Write> {
    writer: W,
}

impl<W: Write> NdjsonWriter<W> {
    /// 创建新的 NDJSON 输出器
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// 写入一个 JSON 值（自动追加换行符）
    ///
    /// 值经过 [ndjson_safe_stringify] 安全序列化后写入，
    /// 末尾追加 `\n` 作为行分隔符。
    pub fn write_value(&mut self, value: &Value) -> io::Result<()> {
        let line = ndjson_safe_stringify(value);
        writeln!(self.writer, "{}", line)
    }

    /// 写入一个事件（自动包装为 `{"event": name, ...data}`）
    ///
    /// 将 `data` 中的所有字段合并到事件对象中，
    /// 并添加 `"event"` 字段标识事件类型。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// writer.write_event("patch_applied", &json!({"patch_id": "p1", "success": true}))?;
    /// // 输出: {"event":"patch_applied","patch_id":"p1","success":true}\n
    /// ```
    pub fn write_event(&mut self, name: &str, data: &Value) -> io::Result<()> {
        let mut event = match data {
            Value::Object(map) => map.clone(),
            _ => {
                // 如果 data 不是对象，包装为 {"data": ...}
                let mut map = serde_json::Map::new();
                map.insert("data".into(), data.clone());
                map
            }
        };
        event.insert("event".into(), Value::String(name.to_string()));
        self.write_value(&Value::Object(event))
    }

    /// 消费输出器，返回底层 Writer
    #[allow(dead_code)] // 公开 API，供外部消费者使用
    pub fn into_inner(self) -> W {
        self.writer
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// OutputFormat — 输出格式枚举
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 输出格式
///
/// 控制 CLI 的输出方式，支持人类可读和机器可读两种模式。
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// 人类可读（默认）— 带颜色、图标、格式化文本
    #[default]
    Human,
    /// NDJSON 机器可读 — 每行一个 JSON 对象，适合管道 / 前端解析
    Ndjson,
    /// 单次 JSON 输出 — 整体输出一个 JSON 数组或对象
    Json,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "human" | "text" | "pretty" => Ok(Self::Human),
            "ndjson" | "jsonl" | "stream" => Ok(Self::Ndjson),
            "json" => Ok(Self::Json),
            _ => Err(format!(
                "未知的输出格式: '{}'，可选值: human, ndjson, json",
                s
            )),
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ndjson_safe_stringify_basic() {
        let value = json!({"msg": "hello", "count": 42});
        let result = ndjson_safe_stringify(&value);
        // serde_json 使用 BTreeMap，key 按字典序排列: count < msg
        assert_eq!(result, r#"{"count":42,"msg":"hello"}"#);
    }

    #[test]
    fn test_ndjson_safe_stringify_escapes_line_separators() {
        // U+2028 LINE SEPARATOR
        let value = json!({"text": "line1\u{2028}line2"});
        let result = ndjson_safe_stringify(&value);
        assert!(!result.contains('\u{2028}'), "U+2028 应被转义");
        assert!(result.contains(r#"\u2028"#), "U+2028 应转义为 \\u2028");

        // U+2029 PARAGRAPH SEPARATOR
        let value = json!({"text": "para1\u{2029}para2"});
        let result = ndjson_safe_stringify(&value);
        assert!(!result.contains('\u{2029}'), "U+2029 应被转义");
        assert!(result.contains(r#"\u2029"#), "U+2029 应转义为 \\u2029");

        // 同时包含两个字符
        let value = json!({"text": "a\u{2028}b\u{2029}c"});
        let result = ndjson_safe_stringify(&value);
        assert_eq!(result, r#"{"text":"a\u2028b\u2029c"}"#,);
    }

    #[test]
    fn test_ndjson_safe_stringify_empty() {
        let value = json!({});
        let result = ndjson_safe_stringify(&value);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_ndjson_safe_stringify_nested() {
        let value = json!({"outer": {"inner": "test\u{2028}data"}});
        let result = ndjson_safe_stringify(&value);
        assert!(
            !result.contains('\u{2028}'),
            "嵌套结构中的 U+2028 也应被转义"
        );
    }

    #[test]
    fn test_ndjson_writer_write_value() {
        let mut buf = Vec::new();
        let mut writer = NdjsonWriter::new(&mut buf);

        writer.write_value(&json!({"event": "start"})).unwrap();
        writer.write_value(&json!({"event": "end"})).unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"{"event":"start"}"#);
        assert_eq!(lines[1], r#"{"event":"end"}"#);
    }

    #[test]
    fn test_ndjson_writer_write_event() {
        let mut buf = Vec::new();
        let mut writer = NdjsonWriter::new(&mut buf);

        writer
            .write_event("patch_applied", &json!({"patch_id": "p1", "success": true}))
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["event"], "patch_applied");
        assert_eq!(parsed["patch_id"], "p1");
        assert_eq!(parsed["success"], true);
    }

    #[test]
    fn test_ndjson_writer_write_event_non_object() {
        // 非 object 数据应被包装为 {"data": ..., "event": ...}
        let mut buf = Vec::new();
        let mut writer = NdjsonWriter::new(&mut buf);

        writer.write_event("log", &json!("hello world")).unwrap();

        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["event"], "log");
        assert_eq!(parsed["data"], "hello world");
    }

    #[test]
    fn test_output_format_from_str() {
        assert_eq!(
            "human".parse::<OutputFormat>().unwrap(),
            OutputFormat::Human
        );
        assert_eq!("text".parse::<OutputFormat>().unwrap(), OutputFormat::Human);
        assert_eq!(
            "ndjson".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ndjson
        );
        assert_eq!(
            "jsonl".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ndjson
        );
        assert_eq!("json".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert!("invalid".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn test_output_format_default() {
        assert_eq!(OutputFormat::default(), OutputFormat::Human);
    }
}
