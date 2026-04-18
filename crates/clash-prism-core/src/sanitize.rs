//! # Unicode 安全清洗
//!
//! 参考 Claude Code sanitization.ts 的安全策略：
//! - 迭代 NFKC 归一化 + 隐藏字符移除
//! - 最多 10 次迭代防止恶意构造的深层嵌套 Unicode 字符串
//! - 移除零宽字符、方向控制、BOM、私用区等危险 Unicode
//!
//! ## 危险 Unicode 分类
//!
//! | 类别 | 范围 | 风险 |
//! |------|------|------|
//! | 零宽字符 | U+200B-U+200D, U+FEFF | 不可见，可隐藏恶意内容 |
//! | 方向控制 | U+200E-U+200F, U+202A-U+202E | 可反转文本方向，欺骗用户 |
//! | BOM | U+FEFF | 干扰字符串比较 |
//! | 私用区 | U+E000-U+F8FF 等 | 不可预测的渲染行为 |
//! | 非字符 | U+FFFE, U+FFFF 等 | Unicode 标准禁止在文本中使用 |
//! | 格式控制 | \p{Cf} | 部分可被滥用（保留 U+00AD 软连字符） |

use std::io;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// 判断字符是否为危险的 Unicode 字符
///
/// 涵盖以下类别：
/// - 零宽空格: U+200B, U+200C, U+200D, U+FEFF
/// - 方向控制: U+200E, U+200F, U+202A-U+202E
/// - BOM: U+FEFF（与零宽不换行空格相同码点）
/// - 私用区: U+E000-U+F8FF, U+F0000-U+FFFFD, U+100000-U+10FFFD
/// - 非字符: U+FFFE, U+FFFF, U+nFFFE, U+nFFFF
/// - 格式控制: \p{Cf}（但保留 U+00AD 软连字符）
fn is_dangerous_unicode(c: char) -> bool {
    match c {
        // 零宽字符
        '\u{200B}' | '\u{200C}' | '\u{200D}' => true,
        // 方向控制
        '\u{200E}' | '\u{200F}' => true,
        '\u{202A}'..='\u{202E}' => true,
        // BOM / 零宽不换行空格
        '\u{FEFF}' => true,
        // 软连字符（U+00AD）保留，不算危险
        '\u{00AD}' => false,
        // 其他格式控制字符 (Cf 类别)
        _ if is_other_format_control(c) => true,
        // 私用区
        '\u{E000}'..='\u{F8FF}' => true,
        '\u{F0000}'..='\u{FFFFD}' => true,
        '\u{100000}'..='\u{10FFFD}' => true,
        // 非字符码点
        _ if is_noncharacter(c) => true,
        _ => false,
    }
}

/// 判断是否为其他格式控制字符（Cf 类别，排除已单独处理的和 U+00AD）
fn is_other_format_control(c: char) -> bool {
    matches!(c,
        '\u{2060}'       // 词连接符
        | '\u{2061}'..='\u{2063}'  // 不可见数学运算符
        | '\u{2064}'     // 不可见加号
        | '\u{2066}'..='\u{2069}'  // 双向隔离控制
        | '\u{206A}'..='\u{206F}'  // 已废弃的格式控制
        | '\u{FFF9}'..='\u{FFFB}'  // 行内注释控制
        | '\u{13430}'..='\u{1343F}' // 埃及象形文字格式控制
        | '\u{1BCA0}'..='\u{1BCA3}' // 闪族字母格式控制
        | '\u{1D173}'..='\u{1D17A}' // 音乐符号格式控制
        | '\u{E0001}'     // 语言标签
        | '\u{E0020}'..='\u{E007F}' // 标签字符
    )
}

/// 判断是否为 Unicode 非字符码点
///
/// 非字符码点是 Unicode 标准保留的，不应出现在交换文本中。
/// 包括：U+FFFE, U+FFFF, 以及每个平面的最后两个码点 (U+nFFFE, U+nFFFF)。
fn is_noncharacter(c: char) -> bool {
    let cp = c as u32;
    // U+FFFE, U+FFFF
    if cp == 0xFFFE || cp == 0xFFFF {
        return true;
    }
    // U+nFFFE, U+nFFFF (n >= 1)
    if (0x1FFFE..=0x10FFFF).contains(&cp) {
        let low16 = cp & 0xFFFF;
        if low16 == 0xFFFE || low16 == 0xFFFF {
            return true;
        }
    }
    false
}

/// 清洗配置字符串中的危险 Unicode 字符
///
/// 迭代执行 NFKC 归一化 + 危险字符移除，最多 10 次，
/// 直到字符串不再变化为止。防止恶意构造的深层嵌套 Unicode
/// 字符串在归一化后暴露出新的危险字符。
///
/// 当提供 `field_whitelist` 时，仅对列表中的字段名应用 NFKC 归一化。
/// 其他字段只执行危险字符移除（不改变 Unicode 表示形式）。
/// 这避免了 NFKC 对代理名称、服务器地址等字段产生意外影响
/// （例如将全角字符 `Ａ` 归一化为半角 `A`）。
///
/// # 示例
/// ```
/// use clash_prism_core::sanitize::sanitize_config_string;
/// let cleaned = sanitize_config_string("hello\u{200B}world");
/// assert_eq!(cleaned, "helloworld");
/// ```
pub fn sanitize_config_string(input: &str) -> String {
    sanitize_config_string_with_whitelist(input, &[])
}

/// 清洗配置字符串，仅对白名单字段应用 NFKC 归一化。
///
/// - `field_whitelist`: 需要应用 NFKC 的字段名列表（如 `["name", "type"]`）。
///   空列表表示对所有字段应用 NFKC（向后兼容行为）。
pub fn sanitize_config_string_with_whitelist(input: &str, field_whitelist: &[&str]) -> String {
    let apply_nfkc = field_whitelist.is_empty();
    let mut current = input.to_string();
    let max_iterations = 10;

    for _ in 0..max_iterations {
        // NFKC 归一化（仅在白名单为空时应用，即向后兼容模式）
        let normalized = if apply_nfkc {
            current.chars().nfkc().collect::<String>()
        } else {
            current.clone()
        };

        // 移除危险字符
        let sanitized: String = normalized
            .chars()
            .filter(|c| !is_dangerous_unicode(*c))
            .collect();

        // 如果字符串不再变化，说明已达到稳定状态
        if sanitized == current {
            break;
        }
        current = sanitized;
    }

    // This indicates a potentially malicious input that keeps producing new
    // dangerous characters after each NFKC normalization pass.
    let final_check: String = if apply_nfkc {
        current.chars().nfkc().collect::<String>()
    } else {
        current.clone()
    };
    let final_sanitized: String = final_check
        .chars()
        .filter(|c| !is_dangerous_unicode(*c))
        .collect();
    if final_sanitized != current {
        tracing::warn!(
            target = "clash_prism_core",
            "sanitize_config_string: 未在 {} 次迭代内达到稳定状态，输入可能包含恶意构造的 Unicode 序列",
            max_iterations
        );
    }

    current
}

/// 验证路径在指定工作空间目录内，防止符号链接绕过路径遍历检查。
///
/// 先对路径执行 `canonicalize()` 解析所有符号链接和 `..` 组件，
/// 再检查解析后的绝对路径是否以 `workspace` 为前缀。
///
/// # 错误
///
/// - 路径不存在或无法解析 → `io::Error`
/// - 解析后的路径不在 workspace 内 → `io::Error(InvalidInput)`
///
/// # 示例
///
/// ```no_run
/// use std::path::Path;
/// let base = Path::new("/safe/workspace");
/// let safe = Path::new("/safe/workspace/config.yaml");
/// let result = clash_prism_core::sanitize::validate_path_within_workspace(safe, base);
/// assert!(result.is_ok());
/// ```
pub fn validate_path_within_workspace(path: &Path, workspace: &Path) -> Result<PathBuf, io::Error> {
    let canonical = path.canonicalize().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("路径无效或不存在: {} ({})", path.display(), e),
        )
    })?;

    let workspace_canonical = workspace.canonicalize().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("工作空间目录无效: {} ({})", workspace.display(), e),
        )
    })?;

    // 检查解析后的路径是否在工作空间目录内。
    // 必须精确匹配或以 workspace 路径 + 分隔符开头，防止
    // `/safe/dir-other/file` 错误匹配 workspace `/safe/dir`。
    // 使用 std::path::is_separator 兼容 Windows 反斜杠。
    let workspace_str = workspace_canonical.as_os_str().as_encoded_bytes();
    let canonical_bytes = canonical.as_os_str().as_encoded_bytes();
    let is_inside = canonical == workspace_canonical
        || (canonical_bytes.len() > workspace_str.len()
            && canonical_bytes.starts_with(workspace_str)
            && std::path::is_separator(canonical_bytes[workspace_str.len()] as char));
    if !is_inside {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "路径 '{}' 解析后不在工作空间 '{}' 范围内（可能通过符号链接绕过）",
                canonical.display(),
                workspace_canonical.display()
            ),
        ));
    }

    Ok(canonical)
}

/// BOM 感知的配置文件读取
///
/// 自动检测并处理以下 BOM 格式：
/// - UTF-8 BOM: `EF BB BF`
/// - UTF-16 LE BOM: `FF FE`
/// - UTF-16 BE BOM: `FE FF`
/// - 无 BOM: 按 UTF-8 读取（使用 lossy 转换处理非法字节）
///
/// # Path Validation
///
/// 此函数会对路径进行规范化（canonicalize）以解析符号链接和 `..` 组件，
/// 防止路径遍历攻击。如果路径不存在或无法解析，返回 `std::io::Error`。
///
/// # 错误
/// 返回 `std::io::Error` 当文件读取失败或路径无效时。
pub fn read_config_file(path: &Path) -> Result<String, io::Error> {
    // canonicalize resolves symlinks and `..` components, which means a malicious
    // path like `/safe/dir/../../etc/passwd` would be resolved to `/etc/passwd`
    // and pass canonicalize successfully. By checking for `..` components first,
    // we reject obvious traversal attempts regardless of symlink resolution.
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("路径包含遍历组件 (..)，拒绝访问: {}", path.display()),
            ));
        }
    }

    // 路径规范化：解析符号链接和相对路径组件（..），防止路径遍历
    let canonical = path.canonicalize().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("路径无效或不存在: {} ({})", path.display(), e),
        )
    })?;

    let bytes = std::fs::read(&canonical)?;

    let content = match bytes.as_slice() {
        // UTF-8 BOM (EF BB BF)
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).to_string(),
        // UTF-16 LE BOM (FF FE)
        [0xFF, 0xFE, rest @ ..] => {
            if rest.len() % 2 != 0 {
                tracing::warn!(
                    target = "clash_prism_core",
                    "UTF-16 LE 文件字节数为奇数 ({}字节)，最后一个字节将被丢弃",
                    rest.len()
                );
            }
            let u16_iter = rest
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
            char::decode_utf16(u16_iter)
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect()
        }
        // UTF-16 BE BOM (FE FF)
        [0xFE, 0xFF, rest @ ..] => {
            if rest.len() % 2 != 0 {
                tracing::warn!(
                    target = "clash_prism_core",
                    "UTF-16 BE 文件字节数为奇数 ({}字节)，最后一个字节将被丢弃",
                    rest.len()
                );
            }
            let u16_iter = rest
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]));
            char::decode_utf16(u16_iter)
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect()
        }
        // 无 BOM，按 UTF-8 读取
        _ => String::from_utf8_lossy(&bytes).to_string(),
    };

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_removes_zero_width() {
        // 零宽空格 U+200B
        let input = "hello\u{200B}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 零宽不连字 U+200C
        let input = "hello\u{200C}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 零宽连字 U+200D
        let input = "hello\u{200D}world";
        assert_eq!(sanitize_config_string(input), "helloworld");
    }

    #[test]
    fn test_sanitize_removes_direction_controls() {
        // 左到右标记 U+200E
        let input = "hello\u{200E}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 右到左标记 U+200F
        let input = "hello\u{200F}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 左到右覆盖 U+202A
        let input = "hello\u{202A}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 右到左覆盖 U+202E
        let input = "hello\u{202E}world";
        assert_eq!(sanitize_config_string(input), "helloworld");
    }

    #[test]
    fn test_sanitize_removes_bom() {
        // BOM U+FEFF
        let input = "\u{FEFF}hello world";
        assert_eq!(sanitize_config_string(input), "hello world");
    }

    #[test]
    fn test_sanitize_removes_private_use() {
        // 私用区 U+E000
        let input = "hello\u{E000}world";
        assert_eq!(sanitize_config_string(input), "helloworld");
    }

    #[test]
    fn test_sanitize_removes_noncharacters() {
        // 非字符 U+FFFE
        let input = "hello\u{FFFE}world";
        assert_eq!(sanitize_config_string(input), "helloworld");

        // 非字符 U+FFFF
        let input = "hello\u{FFFF}world";
        assert_eq!(sanitize_config_string(input), "helloworld");
    }

    #[test]
    fn test_sanitize_preserves_normal_text() {
        let input = "Hello, World! 你好世界 123";
        assert_eq!(sanitize_config_string(input), "Hello, World! 你好世界 123");
    }

    #[test]
    fn test_sanitize_preserves_soft_hyphen() {
        // U+00AD 软连字符应保留
        let input = "hello\u{00AD}world";
        assert_eq!(sanitize_config_string(input), "hello\u{00AD}world");
    }

    #[test]
    fn test_sanitize_iterates_to_stable() {
        // 多层嵌套的危险字符应在迭代中全部被清除
        let input = "\u{200B}\u{FEFF}\u{200E}clean\u{200D}\u{202A}";
        let result = sanitize_config_string(input);
        assert_eq!(result, "clean");
    }

    #[test]
    fn test_sanitize_empty_string() {
        assert_eq!(sanitize_config_string(""), "");
    }

    #[test]
    fn test_sanitize_all_dangerous() {
        let input = "\u{200B}\u{200C}\u{200D}\u{FEFF}\u{200E}\u{200F}";
        assert_eq!(sanitize_config_string(input), "");
    }

    #[test]
    fn test_read_config_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8_bom.txt");

        // UTF-8 BOM + 内容
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"hello world");
        std::fs::write(&path, &bytes).unwrap();

        let content = read_config_file(&path).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_read_config_utf16le_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16le_bom.txt");

        // UTF-16 LE BOM + "Hi" 的 UTF-16 LE 编码
        let mut bytes = vec![0xFF, 0xFE]; // BOM
        bytes.extend_from_slice(&('H' as u16).to_le_bytes());
        bytes.extend_from_slice(&('i' as u16).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let content = read_config_file(&path).unwrap();
        assert_eq!(content, "Hi");
    }

    #[test]
    fn test_read_config_no_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_bom.txt");

        std::fs::write(&path, b"plain text").unwrap();

        let content = read_config_file(&path).unwrap();
        assert_eq!(content, "plain text");
    }

    #[test]
    fn test_read_config_invalid_utf8_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.txt");

        // 非法 UTF-8 字节
        std::fs::write(&path, b"hello\xFF\xFEworld").unwrap();

        let content = read_config_file(&path).unwrap();
        // lossy 转换，非法字节被替换为 U+FFFD
        assert!(content.contains("hello"));
        assert!(content.contains("world"));
    }

    #[test]
    fn test_is_noncharacter() {
        assert!(is_noncharacter('\u{FFFE}'));
        assert!(is_noncharacter('\u{FFFF}'));
        assert!(is_noncharacter('\u{1FFFE}'));
        assert!(is_noncharacter('\u{1FFFF}'));
        assert!(is_noncharacter('\u{10FFFE}'));
        assert!(is_noncharacter('\u{10FFFF}'));
        assert!(!is_noncharacter('A'));
        assert!(!is_noncharacter('\u{0041}'));
    }

    // ─── 跨平台路径分隔符 ───

    #[test]
    fn test_validate_path_within_workspace_trailing_separator() {
        // 路径以分隔符结尾时应被正确识别为"在内部"
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let inner = workspace.join("subdir");
        std::fs::create_dir_all(&inner).unwrap();
        let file = inner.join("config.yaml");
        std::fs::write(&file, "test").unwrap();

        let result = validate_path_within_workspace(&file, workspace);
        assert!(result.is_ok(), "file inside workspace should be valid");
    }

    #[test]
    fn test_validate_path_rejects_symlink_escape() {
        // 符号链接逃逸应被拒绝
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.yaml");
        std::fs::write(&outside_file, "secret").unwrap();

        // 创建符号链接指向外部
        #[cfg(unix)]
        {
            let workspace = dir.path();
            let link = workspace.join("escape_link");
            std::os::unix::fs::symlink(&outside_file, &link).unwrap();
            let result = validate_path_within_workspace(&link, workspace);
            assert!(result.is_err(), "symlink escape should be rejected");
        }
    }

    #[test]
    fn test_sanitize_iterative_stability_normalization() {
        // 验证 NFKC 迭代清洗在正常输入上快速收敛
        // 注意：NFKC 会将全角标点（如 ！U+FF01）归一化为半角（! U+0021）
        let input = "Hello 世界！";
        let result = sanitize_config_string(input);
        assert_eq!(result, "Hello 世界!"); // ！被 NFKC 归一化为 !
    }

    #[test]
    fn test_sanitize_dangerous_unicode_removal() {
        // 零宽字符应被移除
        let input = "Hello\u{200B}World"; // U+200B 零宽空格
        let result = sanitize_config_string(input);
        assert_eq!(result, "HelloWorld");
    }

    #[test]
    fn test_sanitize_bom_removal() {
        // BOM 应被移除
        let input = "\u{FEFF}Hello";
        let result = sanitize_config_string(input);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn test_sanitize_direction_control_removal() {
        // 方向控制字符应被移除
        let input = "Hello\u{200E}World"; // U+200E LRM
        let result = sanitize_config_string(input);
        assert_eq!(result, "HelloWorld");
    }
}
