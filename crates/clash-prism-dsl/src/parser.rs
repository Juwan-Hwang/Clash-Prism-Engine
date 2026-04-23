//! DSL 解析器 — 将 .prism.yaml 文件解析为 Patch IR
//!
//! ## 解析流程
//!
//! 1. 使用 serde_yml 读取 YAML 文件为 Value
//! 2. 提取元数据（__when__、__after__）
//! 3. 遍历每个顶层键，识别操作类型（$ 前缀键 vs 普通键）
//! 4. 按 §2.4 固定执行顺序编译操作
//! 5. 静态字段白名单校验

use std::path::{Path, PathBuf};

use clash_prism_core::compiler::ConditionPrecompiler;
use clash_prism_core::error::{PrismError, Result};
use clash_prism_core::ir::{CompiledPredicate, DependencyRef, Patch, PatchOp, SubOp};
use clash_prism_core::scope::Scope;
use clash_prism_core::source::PatchSource;

/// DSL 操作符前缀
const OP_PREFIX: char = '$';

/// 元数据键名
const META_WHEN: &str = "__when__";
const META_AFTER: &str = "__after__";

/// All valid DSL operations (enforced at parse time).
///
/// Note: `DeepMerge` is the **implicit default operation** — it does NOT use a `$` prefix.
/// When a YAML key has no `$`-prefixed operations (e.g., plain `dns: { enable: true }`),
/// the parser treats it as a DeepMerge. Therefore, DeepMerge is intentionally absent
/// from this list. The 7 explicit operations below all require the `$` prefix.
///
/// **Single source of truth**: `schema.rs` auto-generates the JSON Schema from this constant.
/// When adding/removing operators, update this list and `compile_path_group()`.
pub const VALID_OPS: &[&str] = &[
    "$override",
    "$prepend",
    "$append",
    "$filter",
    "$transform",
    "$remove",
    "$default",
];

/// Prism DSL 解析器
pub struct DslParser;

impl DslParser {
    /// 解析单个 .prism.yaml 文件，返回一组 Patches
    pub fn parse_file(file_path: impl AsRef<Path>) -> Result<Vec<Patch>> {
        let file_path = file_path.as_ref();
        let content = std::fs::read_to_string(file_path).map_err(|e| PrismError::DslParse {
            message: format!("Failed to read file: {}", e),
            file: Some(file_path.to_path_buf()),
            line: None,
        })?;

        Self::parse_str(&content, Some(file_path.to_path_buf()))
    }

    /// 从字符串解析 .prism.yaml 内容
    pub fn parse_str(content: &str, file_path: Option<PathBuf>) -> Result<Vec<Patch>> {
        // 1. 解析 YAML
        let value: serde_yml::Value = serde_yml::from_str(content)?;

        // 2. 提取为 serde_yml::Mapping
        let mapping = match value {
            serde_yml::Value::Mapping(m) => m,
            _ => {
                return Err(PrismError::DslParse {
                    message: "Prism DSL file must be a YAML mapping (dict)".into(),
                    file: file_path.clone(),
                    line: None,
                });
            }
        };

        // 3. 提取元数据
        Self::check_when_uniqueness(&mapping, &file_path)?;
        let scope = Self::extract_scope(&mapping, &file_path)?;
        let after_deps = Self::extract_after_dependencies(&mapping, &file_path)?;

        // 4. 收集所有非元数据的键并按路径分组
        //    同一路径下的多个 $ 操作合并为一个 Patch（内部按固定顺序处理）
        let mut patches = Vec::new();

        // 分组：普通键路径 → 该路径下的所有操作
        let mut path_groups: std::collections::BTreeMap<String, serde_yml::Mapping> =
            std::collections::BTreeMap::new();

        for (key, val) in &mapping {
            let key_str = match key.as_str() {
                Some(s) => s,
                None => {
                    // YAML 非字符串键静默回退时添加 warn 日志
                    tracing::warn!(
                        target = "clash_prism_dsl",
                        key_type = ?key,
                        file_path = ?file_path,
                        "YAML 键为非字符串类型，已跳过该条目"
                    );
                    continue;
                }
            };
            if key_str.starts_with("__") && key_str.ends_with("__") {
                continue; // 跳过元数据
            }

            if let Some(path) = Self::resolve_target_path(key_str) {
                // 检查值是否为包含 $ 操作符的映射
                // 例如: tun: { $override: { enable: true } }
                // 需要将内层的 $ 操作符键提取到 ops_mapping 中
                if let Some(inner_mapping) = val.as_mapping() {
                    let has_dsl_op = inner_mapping
                        .keys()
                        .any(|k| k.as_str().map(|s| s.starts_with('$')).unwrap_or(false));
                    if has_dsl_op {
                        // 将内层映射的所有键值对直接作为该路径的操作
                        let entry = path_groups.entry(path).or_default();
                        for (inner_key, inner_val) in inner_mapping {
                            entry.insert(inner_key.clone(), inner_val.clone());
                        }
                        continue;
                    }
                }

                path_groups
                    .entry(path)
                    .or_default()
                    .insert(key.clone(), val.clone());
            }
        }

        // 5. 对每个路径组编译 Patch
        for (path, ops_mapping) in &path_groups {
            let patch =
                Self::compile_path_group(path, ops_mapping, &scope, &after_deps, &file_path)?;
            if let Some(p) = patch {
                patches.push(p);
            }
        }

        Ok(patches)
    }

    /// 检查 `__when__` 唯一性（§2.3：一个文件只能有一个 `__when__`）
    fn check_when_uniqueness(
        mapping: &serde_yml::Mapping,
        file_path: &Option<PathBuf>,
    ) -> Result<()> {
        let when_count = mapping
            .keys()
            .filter(|k| k.as_str() == Some(META_WHEN))
            .count();
        if when_count > 1 {
            return Err(PrismError::DslParse {
                message: format!(
                    "found {} `__when__` declarations in one file (only one is allowed)",
                    when_count
                ),
                file: file_path.clone(),
                line: None,
            });
        }
        Ok(())
    }

    /// 提取作用域（从 __when__ 元数据）
    fn extract_scope(mapping: &serde_yml::Mapping, file_path: &Option<PathBuf>) -> Result<Scope> {
        match mapping.get(serde_yml::Value::String(META_WHEN.into())) {
            Some(when_val) => {
                let when_map = when_val.as_mapping().ok_or_else(|| PrismError::DslParse {
                    message: "__when__ must be a mapping (dict)".into(),
                    file: file_path.clone(),
                    line: None,
                })?;
                ConditionPrecompiler::compile_when(when_map).map_err(|e| PrismError::DslParse {
                    message: e.to_string(),
                    file: file_path.clone(),
                    line: None,
                })
            }
            None => Ok(Scope::Global),
        }
    }

    /// 提取依赖声明（从 __after__ 元数据）
    fn extract_after_dependencies(
        mapping: &serde_yml::Mapping,
        file_path: &Option<PathBuf>,
    ) -> Result<Vec<DependencyRef>> {
        match mapping.get(serde_yml::Value::String(META_AFTER.into())) {
            Some(after_val) => match after_val {
                serde_yml::Value::String(s) => {
                    Ok(vec![DependencyRef::FileName(s.as_str().to_string())])
                }
                serde_yml::Value::Sequence(seq) => {
                    let mut deps = vec![];
                    for item in seq {
                        if let Some(s) = item.as_str() {
                            deps.push(DependencyRef::FileName(s.to_string()));
                        }
                    }
                    Ok(deps)
                }
                _ => Err(PrismError::DslParse {
                    message: "__after__ 必须是字符串或字符串数组".into(),
                    file: file_path.clone(),
                    line: None,
                }),
            },
            None => Ok(vec![]),
        }
    }

    /// 解析目标路径：$ 操作符的父级键即为目标路径
    ///
    /// 例如：
    /// - `dns:` → 路径 "dns"
    /// - `rules:` → 路径 "rules"
    /// - `proxies:` → 路径 "proxies"
    fn resolve_target_path(key: &str) -> Option<String> {
        if key.starts_with(OP_PREFIX) || key.starts_with("__") {
            None // 操作符和元数据不是目标路径
        } else {
            Some(key.to_string())
        }
    }

    /// 编译单个路径下的所有操作为一个或多个 Patch
    ///
    /// 核心逻辑：按照固定执行顺序处理同一键下的多个操作
    ///
    /// ## DeepMerge 隐式操作
    ///
    /// DeepMerge 是**隐式默认操作**，不使用 `$` 前缀。当 `ops_mapping` 中存在
    /// 非 `$` 前缀的普通键（如 `dns: { enable: true }` 中的 `enable`）时，
    /// 这些键值对会被收集为 DeepMerge 的合并源。DeepMerge 与其他显式操作
    /// （`$filter`、`$prepend` 等）可以共存于同一个路径组中。
    fn compile_path_group(
        path: &str,
        ops_mapping: &serde_yml::Mapping,
        scope: &Scope,
        after_deps: &[DependencyRef],
        file_path: &Option<PathBuf>,
    ) -> Result<Option<Patch>> {
        use serde_yml::Value;

        // 检查是否有 $override（独占操作）
        if ops_mapping.contains_key(Value::String("$override".into())) {
            if ops_mapping.len() > 1 {
                // $override 不能与其他操作混用
                return Err(PrismError::OverrideConflict {
                    field: path.to_string(),
                });
            }

            let override_val = ops_mapping.get(Value::String("$override".into())).unwrap();
            let json_val = yaml_value_to_json(override_val).map_err(|e| PrismError::DslParse {
                message: e,
                file: file_path.clone(),
                line: None,
            })?;

            let source = PatchSource::yaml_file(
                file_path
                    .as_ref()
                    .and_then(|p| p.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
                None,
            );

            let mut patch = Patch::new(source, scope.clone(), path, PatchOp::Override, json_val);
            for dep in after_deps {
                patch = patch.with_after(dep.clone());
            }

            return Ok(Some(patch));
        }

        // 非独占模式：收集所有操作，按固定顺序编译
        //
        // 固定执行顺序: $filter → $remove → $transform → $prepend → $append
        // 普通键（无 $ 前缀）作为 DeepMerge 处理
        // $default 作为 SetDefault 处理

        let mut has_ops = false;
        let mut filter_expr = None;
        let mut remove_expr = None;
        let mut transform_expr = None;
        let mut prepend_val = None;
        let mut append_val = None;
        let mut default_val = None;
        let mut merge_val: Option<serde_json::Value> = None;

        for (key, val) in ops_mapping {
            let key_str = match key.as_str() {
                Some(s) if !s.is_empty() => s,
                _ => {
                    // 而非静默映射为空字符串进入 match（可能导致意外的 DeepMerge 行为）。
                    let key_repr = key.as_str().unwrap_or("(non-string key)");
                    return Err(PrismError::DslParse {
                        message: format!(
                            "路径 '{}' 的操作映射包含无效键 '{}'：键必须是非空字符串",
                            path, key_repr
                        ),
                        file: file_path.clone(),
                        line: None,
                    });
                }
            };

            match key_str {
                "$filter" => {
                    has_ops = true;
                    let expr_str = val.as_str().ok_or_else(|| PrismError::DslParse {
                        message: format!("{} 的值必须是字符串表达式", key_str),
                        file: file_path.clone(),
                        line: None,
                    })?;
                    // 静态字段校验
                    let referenced = Self::validate_static_fields(expr_str, path, file_path)?;
                    filter_expr = Some(CompiledPredicate::new(expr_str, referenced));
                }
                "$remove" => {
                    has_ops = true;
                    let expr_str = val.as_str().ok_or_else(|| PrismError::DslParse {
                        message: format!("{} 的值必须是字符串表达式", key_str),
                        file: file_path.clone(),
                        line: None,
                    })?;
                    let referenced = Self::validate_static_fields(expr_str, path, file_path)?;
                    remove_expr = Some(CompiledPredicate::new(expr_str, referenced));
                }
                "$transform" => {
                    has_ops = true;
                    let expr_str = val.as_str().ok_or_else(|| PrismError::DslParse {
                        message: format!("{} 的值必须是字符串表达式", key_str),
                        file: file_path.clone(),
                        line: None,
                    })?;
                    let referenced = Self::validate_static_fields(expr_str, path, file_path)?;
                    transform_expr = Some(CompiledPredicate::new(expr_str, referenced));
                }
                "$prepend" => {
                    has_ops = true;
                    let raw_val = yaml_value_to_json(val).map_err(|e| PrismError::DslParse {
                        message: e,
                        file: file_path.clone(),
                        line: None,
                    })?;
                    prepend_val = Some(process_conditional_array_items(&raw_val));
                }
                "$append" => {
                    has_ops = true;
                    let raw_val = yaml_value_to_json(val).map_err(|e| PrismError::DslParse {
                        message: e,
                        file: file_path.clone(),
                        line: None,
                    })?;
                    append_val = Some(process_conditional_array_items(&raw_val));
                }
                "$default" => {
                    has_ops = true;
                    default_val =
                        Some(yaml_value_to_json(val).map_err(|e| PrismError::DslParse {
                            message: e,
                            file: file_path.clone(),
                            line: None,
                        })?);
                }
                _ => {
                    // Reject unknown `$`-prefixed keys
                    if key_str.starts_with('$') {
                        Self::validate_operation_name(key_str, file_path)?;
                    }
                    // Normal key -> DeepMerge
                    // 多个普通键时合并到 merge_val，而非覆盖
                    let val = yaml_value_to_json(val).map_err(|e| PrismError::DslParse {
                        message: e,
                        file: file_path.clone(),
                        line: None,
                    })?;
                    match &mut merge_val {
                        None => {
                            merge_val = Some(val);
                        }
                        Some(serde_json::Value::Object(target)) => {
                            if let serde_json::Value::Object(source) = val {
                                target.extend(source);
                            } else {
                                return Err(PrismError::DslParse {
                                    message: format!(
                                        "路径 '{}' 已有普通键的值，额外的普通键 '{}' 的值必须是对象类型，用于深度合并",
                                        path, key_str
                                    ),
                                    file: file_path.clone(),
                                    line: None,
                                });
                            }
                        }
                        Some(_) => {
                            return Err(PrismError::DslParse {
                                message: format!(
                                    "路径 '{}' 的首个普通键值不是对象类型，无法与额外的普通键 '{}' 合并",
                                    path, key_str
                                ),
                                file: file_path.clone(),
                                line: None,
                            });
                        }
                    }
                }
            }
        }

        // 如果没有任何操作且没有合并值，跳过
        if !has_ops && merge_val.is_none() {
            return Ok(None);
        }

        let source = PatchSource::yaml_file(
            file_path
                .as_ref()
                .and_then(|p| p.to_str())
                .unwrap_or("unknown")
                .to_string(),
            None,
        );

        // Determine primary operation type + collect all sub-operations (composite execution)
        //
        // Fixed execution order: $filter -> $remove -> $transform -> $prepend -> $append -> $default
        // Executor retrieves all operations via patch.all_ops() in correct sorted order.
        // SubOp refactoring: each sub-operation now carries its own value alongside the op.
        let mut sub_ops: Vec<SubOp> = vec![];

        let (op, value) =
            if filter_expr.is_some() || remove_expr.is_some() || transform_expr.is_some() {
                // Collect all array operations into sub_ops
                if let Some(expr) = &filter_expr {
                    sub_ops.push(SubOp {
                        op: PatchOp::Filter { expr: expr.clone() },
                        value: serde_json::Value::Null,
                    });
                }
                if let Some(expr) = &remove_expr {
                    sub_ops.push(SubOp {
                        op: PatchOp::Remove { expr: expr.clone() },
                        value: serde_json::Value::Null,
                    });
                }
                if let Some(expr) = &transform_expr {
                    sub_ops.push(SubOp {
                        op: PatchOp::Transform { expr: expr.clone() },
                        value: serde_json::Value::Null,
                    });
                }
                // Also collect prepend/append when filter/transform/remove are present
                if let Some(pv) = &prepend_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::Prepend,
                        value: pv.clone(),
                    });
                }
                if let Some(av) = &append_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::Append,
                        value: av.clone(),
                    });
                }
                if let Some(dv) = &default_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::SetDefault,
                        value: dv.clone(),
                    });
                }
                // Primary operation is the first one
                if let Some(expr) = filter_expr.take() {
                    (PatchOp::Filter { expr }, serde_json::Value::Null)
                } else if let Some(expr) = remove_expr.take() {
                    (PatchOp::Remove { expr }, serde_json::Value::Null)
                } else if let Some(expr) = transform_expr.take() {
                    (PatchOp::Transform { expr }, serde_json::Value::Null)
                } else {
                    // 替换 unreachable!() 为安全的降级处理
                    // 理论上不应到达此处（filter/remove/transform 均已处理），
                    // 但如果因未来代码变更导致到达，记录警告并跳过
                    tracing::warn!("compile_path_group: 未预期的操作组合，跳过该路径: {}", path);
                    return Ok(None);
                }
            } else if prepend_val.is_some() || append_val.is_some() {
                // Array insert operations
                if let Some(pv) = &prepend_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::Prepend,
                        value: pv.clone(),
                    });
                }
                if let Some(av) = &append_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::Append,
                        value: av.clone(),
                    });
                }
                if let Some(dv) = &default_val {
                    sub_ops.push(SubOp {
                        op: PatchOp::SetDefault,
                        value: dv.clone(),
                    });
                }
                if let Some(pv) = prepend_val {
                    (PatchOp::Prepend, pv)
                } else {
                    (PatchOp::Append, append_val.unwrap())
                }
            } else if let Some(default) = default_val {
                sub_ops.push(SubOp {
                    op: PatchOp::SetDefault,
                    value: default.clone(),
                });
                (PatchOp::SetDefault, default)
            } else if let Some(merge) = merge_val {
                (PatchOp::DeepMerge, merge)
            } else {
                tracing::warn!("compile_path_group: 未预期的操作组合，跳过该路径: {}", path);
                return Ok(None);
            };

        let mut patch = Patch::new(source, scope.clone(), path, op, value);

        // 注册子操作（如果有多个操作）
        if !sub_ops.is_empty() {
            patch = patch.with_sub_ops(sub_ops)?;
        }

        for dep in after_deps {
            patch = patch.with_after(dep.clone());
        }

        Ok(Some(patch))
    }

    /// 静态字段白名单校验（AST 级别 — 基于标识符节点提取，非字符串匹配）
    ///
    /// ## 设计决策（§2.6）
    ///
    /// 使用精确的词法分析提取 `p.xxx` 成员访问中的字段标识符，
    /// 而非简单的 `expr.contains("p.delay")` 字符串匹配。
    ///
    /// 这避免了误杀合法写法，例如：
    /// - `p.name.includes('delayed')` — 字符串 'delayed' 不应触发 delay 检测 ✅
    /// - `p.type === 'latency_test'` — 字符串不应触发 type 以外的检测 ✅
    /// - `p.delay < 200` — 真正引用了运行时字段 ❌ 应报错
    fn validate_static_fields(
        expr: &str,
        _path: &str,
        _file_path: &Option<PathBuf>,
    ) -> Result<Vec<String>> {
        // 提取表达式中所有 p.xxx 形式的字段引用（AST 级别的标识符提取）
        let referenced_fields = extract_member_access_fields(expr);

        for field in &referenced_fields {
            if clash_prism_core::ir::is_runtime_field(field) {
                return Err(PrismError::RuntimeFieldInStaticFilter {
                    field: field.clone(),
                    hint: format!(
                        "`{}` is a runtime field (only available after speed testing). \
                         Use Smart Selector for latency-based selection.",
                        field
                    ),
                });
            }
        }
        Ok(referenced_fields)
    }

    /// Validate that a `$`-prefixed key is a known DSL operation.
    fn validate_operation_name(key: &str, file_path: &Option<PathBuf>) -> Result<()> {
        if !VALID_OPS.contains(&key) {
            let suggestion = VALID_OPS
                .iter()
                .filter(|op| levenshtein_distance(key, op) <= 3)
                .min_by_key(|op| levenshtein_distance(key, op))
                .map(|s| s.to_string());

            return Err(PrismError::DslParse {
                message: if let Some(sugg) = suggestion {
                    format!("Unknown DSL operation `{}`. Did you mean `{}`?", key, sugg)
                } else {
                    format!(
                        "Unknown DSL operation `{}`. Valid operations are: {}",
                        key,
                        VALID_OPS.join(", ")
                    )
                },
                file: file_path.clone(),
                line: None,
            });
        }
        Ok(())
    }
}

// ══════════════════════════════════════════════════════════
// AST 级别字段引用提取（§2.6 静态字段白名单校验）
// ══════════════════════════════════════════════════════════

/// From expression, extract all `p.xxx` member access field names.
///
/// `p['ws-opts'].path`. Template string `${p.delay}` expressions are now
/// preserved by `strip_strings_and_comments` and thus also checked.
fn extract_member_access_fields(expr: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::OnceLock;

    // ── Phase 1: Extract bracket-access fields BEFORE stripping strings ──
    // p['field'] and p["field"] contain quotes that look like string literals,
    // so we must capture them on the raw expression first.
    static BRACKET_RE: OnceLock<Regex> = OnceLock::new();
    let bracket_re = BRACKET_RE
        .get_or_init(|| Regex::new(r#"\bp\[\s*'([^']+)'\s*\]|\bp\[\s*"([^"]+)"\s*\]"#).unwrap());

    let mut fields = Vec::new();
    for cap in bracket_re.captures_iter(expr) {
        let field = cap.get(1).or_else(|| cap.get(2));
        if let Some(m) = field {
            let f = m.as_str().to_string();
            if !fields.contains(&f) {
                fields.push(f);
            }
        }
    }

    // ── Phase 2: Extract dot-access fields from stripped expression ──
    static DOT_RE: OnceLock<Regex> = OnceLock::new();
    let dot_re = DOT_RE.get_or_init(|| Regex::new(r#"\bp\.([a-zA-Z_$][a-zA-Z0-9_$]*)"#).unwrap());

    // Strip string literals and comments before matching (AST-level accuracy)
    let cleaned = strip_strings_and_comments(expr);

    for cap in dot_re.captures_iter(&cleaned) {
        if let Some(m) = cap.get(1) {
            let f = m.as_str().to_string();
            if !fields.contains(&f) {
                fields.push(f);
            }
        }
    }

    fields
}

/// 移除表达式中的字符串字面量和注释，返回"纯代码"版本
///
///
/// 这是实现 AST 级别校验的关键：先剥离字符串和注释，
/// 再做字段匹配，就不会误杀字符串内容中包含运行时字段名的合法写法。
///
/// ## Note on duplication
///
/// A similar function exists in `clash-prism-script/src/runtime.rs` (`strip_code_strings_and_comments`).
/// The two implementations have intentional differences:
/// - **This version** (parser.rs): Uses `match` chains, no regex literal handling
///   (DSL expressions don't contain regex literals).
/// - **Script version** (runtime.rs): Handles **regex literals** (`/pattern/flags`) because
///   script validation needs to detect dangerous content inside regex patterns too.
fn strip_strings_and_comments(expr: &str) -> String {
    let mut result = String::with_capacity(expr.len());
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        match ch {
            // 单行注释 //
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                i = skip_line_comment(&chars, i);
            }
            // 多行注释 /* */
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i = skip_block_comment(&chars, i);
            }
            // 单引号字符串 '
            '\'' => {
                i = skip_string_literal(&chars, i, '\'', &mut result);
            }
            // 双引号字符串 "
            '"' => {
                i = skip_string_literal(&chars, i, '"', &mut result);
            }
            // 反引号模板字符串 `
            '`' => {
                i = skip_template_literal(&chars, i, &mut result);
            }
            _ => {
                result.push(ch);
                i += 1;
            }
        }
    }

    result
}

/// 跳过单行注释 `// ...`，返回跳过后的位置（指向 `\n` 或末尾）
fn skip_line_comment(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

/// 跳过多行注释 `/* ... */`，返回跳过后的位置（`*/` 之后）
fn skip_block_comment(chars: &[char], mut i: usize) -> usize {
    i += 2; // 跳过 /*
    while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
        i += 1;
    }
    i += 2; // 跳过 */
    i
}

/// 跳过字符串字面量（单引号或双引号），将内容替换为空格
/// 返回跳过后的位置（闭合引号之后）
fn skip_string_literal(chars: &[char], mut i: usize, quote: char, result: &mut String) -> usize {
    result.push(' '); // 起始引号用空格占位
    i += 1;
    while i < chars.len() && chars[i] != quote {
        if chars[i] == '\\' && i + 1 < chars.len() {
            result.push(' '); // 转义字符占位
            i += 2;
        } else {
            result.push(' ');
            i += 1;
        }
    }
    if i < chars.len() {
        result.push(' '); // 闭合引号用空格占位
        i += 1;
    }
    i
}

/// 跳过模板字面量 `` `...` ``，保留 `${...}` 内的表达式代码
/// 返回跳过后的位置（闭合反引号之后）
fn skip_template_literal(chars: &[char], mut i: usize, result: &mut String) -> usize {
    result.push(' '); // 起始反引号用空格占位
    i += 1;
    while i < chars.len() && chars[i] != '`' {
        if chars[i] == '\\' && i + 1 < chars.len() {
            // 转义字符处理 — 跳过反斜杠和下一个字符
            result.push(' ');
            result.push(' ');
            i += 2;
        } else if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // 模板表达式 ${...}：保留内部代码用于字段提取
            result.push('$');
            result.push('{');
            i += 2;
            i = skip_template_expr(chars, i, result);
        } else {
            result.push(' ');
            i += 1;
        }
    }
    if i < chars.len() {
        result.push(' '); // 闭合反引号用空格占位
        i += 1;
    }
    i
}

/// 跳过模板表达式 `${...}` 内部内容，保留代码到 result
/// 返回 `}` 之后的位置
fn skip_template_expr(chars: &[char], mut i: usize, result: &mut String) -> usize {
    let mut depth = 1usize;
    let mut in_expr_string = false;
    let mut expr_string_char = ' ';

    while i < chars.len() && depth > 0 {
        let c = chars[i];
        if in_expr_string {
            if c == '\\' && i + 1 < chars.len() {
                result.push(c);
                i += 1;
                result.push(chars[i]);
                i += 1;
                continue;
            }
            if c == expr_string_char {
                in_expr_string = false;
            }
            result.push(c);
            i += 1;
        } else if c == '"' || c == '\'' {
            in_expr_string = true;
            expr_string_char = c;
            result.push(c);
            i += 1;
        } else if c == '\\' && i + 1 < chars.len() {
            result.push(c);
            i += 1;
            result.push(chars[i]);
            i += 1;
        } else if c == '{' {
            depth += 1;
            result.push(c);
            i += 1;
        } else if c == '}' {
            depth -= 1;
            if depth > 0 {
                result.push(c);
            } else {
                result.push('}'); // 闭合 }
            }
            i += 1;
        } else {
            result.push(c);
            i += 1;
        }
    }
    i
}

// ──────────────────────────────────────────────────────
// YAML ↔ JSON 转换工具
// ──────────────────────────────────────────────────────

/// 将 serde_yml::Value 转换为 serde_json::Value
///
/// 如果遇到 NaN 或 Infinity 浮点值，返回错误而非静默替换为 0。
fn yaml_value_to_json(
    yaml_val: &serde_yml::Value,
) -> std::result::Result<serde_json::Value, String> {
    yaml_value_to_json_inner(yaml_val, 0)
}

/// 递归深度限制（防止恶意构造的深层嵌套 YAML 或 Tagged 值导致栈溢出）
const MAX_YAML_TO_JSON_DEPTH: usize = 10;

/// `yaml_value_to_json` 的内部递归实现，带深度跟踪。
fn yaml_value_to_json_inner(
    yaml_val: &serde_yml::Value,
    depth: usize,
) -> std::result::Result<serde_json::Value, String> {
    if depth > MAX_YAML_TO_JSON_DEPTH {
        tracing::warn!(
            target = "clash_prism_dsl",
            depth = depth,
            max_depth = MAX_YAML_TO_JSON_DEPTH,
            "yaml_value_to_json: 递归深度超过限制 ({} > {})，返回 Null。\
             输入可能包含恶意构造的深层嵌套 YAML 结构。",
            depth,
            MAX_YAML_TO_JSON_DEPTH
        );
        return Ok(serde_json::Value::Null);
    }

    match yaml_val {
        serde_yml::Value::Null => Ok(serde_json::Value::Null),
        serde_yml::Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        serde_yml::Value::Number(n) => {
            // 尝试转为 i64 或 f64
            if let Some(i) = n.as_i64() {
                Ok(serde_json::Value::Number(serde_json::Number::from(i)))
            } else if let Some(f) = n.as_f64() {
                if f.is_nan() || f.is_infinite() {
                    return Err(format!("YAML 中包含非法浮点值: {}", f));
                }
                // f64 拥有 53 位尾数，当 |f| > 2^53 时整数部分无法精确表示。
                if f.abs() > 9_007_199_254_740_992.0 && f.fract() == 0.0 {
                    tracing::warn!(
                        target = "clash_prism_dsl",
                        value = f,
                        "yaml_value_to_json: 浮点值超出 i64 精确表示范围 (|f| > 2^53)，\
                         转换为 JSON 时可能丢失精度"
                    );
                }
                // from_f64 对无法精确表示的值返回 None，
                // 此时使用 f64 的整数近似值（截断）而非静默替换为 0
                Ok(serde_json::Value::Number(
                    serde_json::Number::from_f64(f)
                        .unwrap_or_else(|| serde_json::Number::from(f as i64)),
                ))
            } else if let Some(u) = n.as_u64() {
                Ok(serde_json::Value::Number(serde_json::Number::from(u)))
            } else {
                Ok(serde_json::Value::Null)
            }
        }
        serde_yml::Value::String(s) => Ok(serde_json::Value::String(s.clone())),
        serde_yml::Value::Sequence(arr) => {
            let json_arr: std::result::Result<Vec<_>, String> = arr
                .iter()
                .map(|v| yaml_value_to_json_inner(v, depth + 1))
                .collect();
            Ok(serde_json::Value::Array(json_arr?))
        }
        serde_yml::Value::Mapping(map) => {
            let mut json_map = serde_json::Map::new();
            for (k, v) in map {
                if let Some(key_str) = k.as_str() {
                    json_map.insert(key_str.to_string(), yaml_value_to_json_inner(v, depth + 1)?);
                }
            }
            Ok(serde_json::Value::Object(json_map))
        }
        // serde_yml 0.9+ 引入了 Tagged 变体，尝试转换内部值
        // 注意：Tagged 值的转换可能导致递归（例如自引用的 Tagged 结构），
        // 因此使用深度限制防止无限循环
        other => match serde_yml::to_value(other) {
            Ok(v) => yaml_value_to_json_inner(&v, depth + 1),
            Err(e) => {
                tracing::warn!(
                    "YAML Tagged value conversion failed: {}, returning Null. \
                         This may indicate an unsupported YAML feature in the input.",
                    e
                );
                Ok(serde_json::Value::Null)
            }
        },
    }
}

/// Process conditional rule objects in $prepend/$append arrays.
///
/// Supports the `__when__` + `__rule__` pattern for per-element conditions:
/// - `{ __when__: { enabled: false }, __rule__: "DOMAIN-SUFFIX,example.com,PROXY" }` -> skipped
/// - `{ __when__: { enabled: true }, __rule__: "DOMAIN-SUFFIX,example.com,PROXY" }` -> included as string
/// - `{ __when__: { platform: macos }, __rule__: "DOMAIN-SUFFIX,example.com,PROXY" }` -> kept as-is for runtime eval
/// - Plain strings -> kept as-is
fn process_conditional_array_items(arr: &serde_json::Value) -> serde_json::Value {
    let items = match arr.as_array() {
        Some(a) => a,
        None => return arr.clone(),
    };

    let mut result = Vec::with_capacity(items.len());

    for item in items {
        if let Some(obj) = item.as_object() {
            // Check if this is a conditional rule object (has __when__ key)
            if obj.contains_key("__when__")
                && let Some(when_obj) = obj.get("__when__").and_then(|v| v.as_object())
            {
                // Check for `enabled` condition — can be resolved at parse time
                if let Some(enabled_val) = when_obj.get("enabled") {
                    if let Some(false) = enabled_val.as_bool() {
                        // enabled: false -> skip this rule entirely
                        continue;
                    }
                    // enabled: true -> extract __rule__ if present
                    if let Some(rule_str) = obj.get("__rule__").and_then(|v| v.as_str()) {
                        result.push(serde_json::Value::String(rule_str.to_string()));
                        continue;
                    }
                }

                // Has __when__ but no `enabled` key (e.g., platform, core, time conditions)
                // -> keep as conditional object for runtime evaluation
                // Extract __rule__ if present, otherwise keep the whole object
                if let Some(rule_str) = obj.get("__rule__").and_then(|v| v.as_str()) {
                    // Wrap in a conditional object that the executor can evaluate later
                    let mut cond_obj = serde_json::Map::new();
                    cond_obj.insert(
                        "__when__".to_string(),
                        obj.get("__when__").cloned().unwrap_or_default(),
                    );
                    cond_obj.insert(
                        "__rule__".to_string(),
                        serde_json::Value::String(rule_str.to_string()),
                    );
                    result.push(serde_json::Value::Object(cond_obj));
                    continue;
                }
            }
        }
        // Plain string or unrecognized format -> keep as-is
        result.push(item.clone());
    }

    serde_json::Value::Array(result)
}

const LEVENSHTEIN_MAX_LEN: usize = 256;

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (a_len, b_len) = (a_chars.len(), b_chars.len());

    // 超过长度限制时直接返回最大距离（保守估计）
    if a_len > LEVENSHTEIN_MAX_LEN || b_len > LEVENSHTEIN_MAX_LEN {
        return a_len.max(b_len);
    }

    let mut prev_row: Vec<usize> = (0..=b_len).collect();
    for i in 1..=a_len {
        let mut curr_row = vec![i];
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            let min_val = *prev_row
                .get(j - 1)
                .unwrap_or(&0)
                .min(curr_row.get(j - 1).unwrap_or(&0))
                .min(&(*prev_row.get(j).unwrap_or(&0) + cost));
            curr_row.push(min_val);
        }
        prev_row = curr_row;
    }
    prev_row[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_deep_merge() {
        let yaml = r#"
dns:
  enable: true
  ipv6: false
  nameserver:
    - https://dns.alidns.com/dns-query
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "dns");
        assert!(matches!(patches[0].op, PatchOp::DeepMerge));
        assert_eq!(patches[0].value["enable"], true);
        assert_eq!(patches[0].value["ipv6"], false);
    }

    #[test]
    fn test_parse_prepend_and_append() {
        let yaml = r#"
rules:
  $prepend:
    - RULE-SET,my-direct,DIRECT
  $append:
    - GEOIP,CN,DIRECT
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "rules");
        // $prepend 在固定顺序中优先于 $append，但两者都存在时以 $prepend 为主表示
        assert!(matches!(patches[0].op, PatchOp::Prepend));
    }

    #[test]
    fn test_parse_override() {
        let yaml = r#"
tun:
  $override:
    enable: true
    stack: mixed
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Override));
        assert_eq!(patches[0].value["enable"], true);
        assert_eq!(patches[0].value["stack"], "mixed");
    }

    #[test]
    fn test_parse_when_scope() {
        let yaml = r#"
__when__:
  core: mihomo
  platform: macos
rules:
  $prepend:
    - PROCESS-NAME,Telegram,PROXY
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        match &patches[0].scope {
            Scope::Scoped { core, platform, .. } => {
                assert_eq!(core.as_deref(), Some("mihomo"));
                assert!(platform.is_some());
            }
            other => panic!("Expected Scoped, got: {:?}", other),
        }
    }

    #[test]
    fn test_parse_after_dependency() {
        let yaml = r#"
__after__: ["base-dns"]
dns:
  enable: true
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].after.len(), 1);
        match &patches[0].after[0] {
            DependencyRef::FileName(name) => assert_eq!(name, "base-dns"),
            _ => panic!("Expected FileName dependency"),
        }
    }

    #[test]
    fn test_filter_runtime_field_rejected() {
        let yaml = r#"
proxies:
  $filter: "p.delay < 200"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err = result.err().unwrap();
        let err_str = err.to_string();
        assert!(err_str.contains("delay"));
        assert!(err_str.contains("runtime field"));
    }

    #[test]
    fn test_override_conflict_detection() {
        let yaml = r#"
tun:
  $override:
    enable: true
  enable: false
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err_str = result.err().unwrap().to_string();
        assert!(err_str.contains("override") || err_str.contains("conflict"));
    }

    #[test]
    fn test_default_injection() {
        let yaml = r#"
dns:
  $default:
    enhanced-mode: fake-ip
    fake-ip-filter:
      - "+.lan"
      - "+.local"
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::SetDefault));
        assert_eq!(patches[0].value["enhanced-mode"], "fake-ip");
    }

    // ──────────────────────────────────────────────────────
    // AST 级别静态字段校验测试
    // ──────────────────────────────────────────────────────

    #[test]
    fn test_ast_level_field_extraction() {
        // 真正引用运行时字段 → 应报错
        let expr = "p.delay < 200";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"delay".to_string()));

        // 字符串中的 delay 不应被提取 → 不应误报
        let expr = "p.name.includes('delayed')";
        let fields = extract_member_access_fields(expr);
        assert!(!fields.contains(&"delay".to_string()));
        assert!(fields.contains(&"name".to_string()));
    }

    #[test]
    fn test_ast_level_no_false_positive() {
        // p.type === 'latency_test' — 'latency' 在字符串中，不应触发
        let expr = "p.type === 'latency_test'";
        let fields = extract_member_access_fields(expr);
        assert!(!fields.contains(&"latency".to_string()));
        assert!(fields.contains(&"type".to_string()));
    }

    #[test]
    fn test_ast_level_complex_expression() {
        // 复合表达式：只提取 p.xxx 中的标识符
        let expr = "p.name.includes('HK') && p.type === 'ss' || p.delay > 100";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"name".to_string()));
        assert!(fields.contains(&"type".to_string()));
        assert!(fields.contains(&"delay".to_string()));
        // 不应包含字符串字面量内容
        assert!(!fields.contains(&"HK".to_string()));
        assert!(!fields.contains(&"ss".to_string()));
    }

    #[test]
    fn test_ast_level_transform_expr() {
        // $transform 表达式中的字段提取
        let expr = "{...p, name: 'prefix ' + p.name}";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"name".to_string()));
    }

    // ──────────────────────────────────────────────────────
    // 模板字符串 ${} 内字段检测 + 括号访问 p['xxx']
    // ──────────────────────────────────────────────────────

    #[test]
    fn test_template_string_runtime_field_detected() {
        // 模板字符串 ${p.delay} 中的运行时字段应被检测到
        let expr = "`节点延迟: ${p.delay}ms`";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"delay".to_string()));
    }

    #[test]
    fn test_template_string_static_field_ok() {
        // 模板字符串 ${p.name} 中的静态字段应被提取但不报错
        let expr = "`节点名: ${p.name}`";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"name".to_string()));
        assert!(!fields.contains(&"delay".to_string()));
    }

    #[test]
    fn test_template_string_plain_text_not_extracted() {
        // 模板字符串纯文本部分（非 ${}）不应被提取
        let expr = "`delay is high`";
        let fields = extract_member_access_fields(expr);
        assert!(!fields.contains(&"delay".to_string()));
    }

    #[test]
    fn test_bracket_access_runtime_field() {
        // p['delay'] 括号访问应被检测到
        let expr = "p['delay'] < 200";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"delay".to_string()));
    }

    #[test]
    fn test_bracket_access_double_quote() {
        // p["delay"] 双引号括号访问也应被检测到
        let expr = "p[\"delay\"] < 200";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"delay".to_string()));
    }

    #[test]
    fn test_bracket_access_with_dash() {
        // p['ws-opts'] 含连字符的字段名
        let expr = "p['ws-opts'] !== undefined";
        let fields = extract_member_access_fields(expr);
        assert!(fields.contains(&"ws-opts".to_string()));
    }

    #[test]
    fn test_filter_template_runtime_field_rejected() {
        // 端到端 — $filter 中模板字符串引用运行时字段应报错
        let yaml = r#"
proxies:
  $filter: "`${p.delay}` < 200"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err_str = result.err().unwrap().to_string();
        assert!(err_str.contains("delay"));
        assert!(err_str.contains("runtime field"));
    }

    #[test]
    fn test_filter_bracket_runtime_field_rejected() {
        // 端到端 — $filter 中 p['delay'] 应报错
        let yaml = r#"
proxies:
  $filter: "p['delay'] < 200"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err_str = result.err().unwrap().to_string();
        assert!(err_str.contains("delay"));
    }

    #[test]
    fn test_parse_rule_level_when_disabled() {
        let yaml = r#"
rules:
  $prepend:
    - DOMAIN-SUFFIX,google.com,PROXY
    - __when__:
        enabled: false
      __rule__: DOMAIN-SUFFIX,youtube.com,PROXY
    - DOMAIN-SUFFIX,facebook.com,PROXY
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        // The disabled rule should be filtered out, leaving 2 rules
        if let Some(arr) = patches[0].value.as_array() {
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0].as_str().unwrap(), "DOMAIN-SUFFIX,google.com,PROXY");
            assert_eq!(arr[1].as_str().unwrap(), "DOMAIN-SUFFIX,facebook.com,PROXY");
        } else {
            panic!("Expected array value");
        }
    }

    #[test]
    fn test_parse_rule_level_when_enabled() {
        let yaml = r#"
rules:
  $prepend:
    - DOMAIN-SUFFIX,google.com,PROXY
    - __when__:
        enabled: true
      __rule__: DOMAIN-SUFFIX,youtube.com,PROXY
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        if let Some(arr) = patches[0].value.as_array() {
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[1].as_str().unwrap(), "DOMAIN-SUFFIX,youtube.com,PROXY");
        } else {
            panic!("Expected array value");
        }
    }

    #[test]
    fn test_parse_rule_level_when_runtime_condition() {
        let yaml = r#"
rules:
  $prepend:
    - __when__:
        platform: macos
      __rule__: DOMAIN-SUFFIX,apple.com,PROXY
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        // Runtime conditions should be preserved as conditional objects
        if let Some(arr) = patches[0].value.as_array() {
            assert_eq!(arr.len(), 1);
            let item = &arr[0];
            assert!(item.is_object());
            assert!(item.get("__when__").is_some());
            assert!(item.get("__rule__").is_some());
        } else {
            panic!("Expected array value");
        }
    }

    // ══════════════════════════════════════════════════════════
    // 对抗性测试 — 边界条件、错误路径、恶意输入
    // ══════════════════════════════════════════════════════════

    #[test]
    fn test_after_nonexistent_file_parsed_but_dep_resolution_fails() {
        // __after__ 引用不存在的文件 — 解析阶段不报错（仅记录依赖声明），
        // 但编译器 resolve_dependencies() 时应报 DependencyNotFound。
        // 这里验证解析阶段本身成功。
        let yaml = r#"
__after__: "nonexistent-file-that-does-not-exist"
dns:
  enable: true
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "__after__ referencing non-existent file should parse OK at DSL level"
        );
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].after.len(), 1);
    }

    #[test]
    fn test_when_time_range_valid() {
        let yaml = r#"
__when__:
  time: "09:00-17:00"
rules:
  $prepend:
    - DOMAIN-SUFFIX,work.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Valid time range should parse correctly");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        match &patches[0].scope {
            Scope::Scoped {
                time_range: Some(tr),
                ..
            } => {
                assert_eq!(tr.start, (9, 0));
                assert_eq!(tr.end, (17, 0));
            }
            other => panic!("Expected Scoped with time_range, got: {:?}", other),
        }
    }

    #[test]
    fn test_when_time_range_invalid() {
        let yaml = r#"
__when__:
  time: "invalid-time"
rules:
  $prepend:
    - DOMAIN-SUFFIX,work.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "Invalid time range should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("时间") || err_msg.contains("time") || err_msg.contains("无效"),
            "Error should mention time format issue: {}",
            err_msg
        );
    }

    #[test]
    fn test_when_ssid_condition() {
        let yaml = r#"
__when__:
  ssid: "MyWiFi"
rules:
  $prepend:
    - DOMAIN-SUFFIX,local.com,DIRECT
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "SSID condition should parse correctly");
        let patches = result.unwrap();
        match &patches[0].scope {
            Scope::Scoped { ssid: Some(s), .. } => {
                assert_eq!(s, "MyWiFi");
            }
            other => panic!("Expected Scoped with ssid, got: {:?}", other),
        }
    }

    #[test]
    fn test_when_enabled_false_marks_disabled() {
        let yaml = r#"
__when__:
  enabled: false
rules:
  $prepend:
    - DOMAIN-SUFFIX,disabled.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "enabled: false should parse but mark scope as disabled"
        );
        let patches = result.unwrap();
        match &patches[0].scope {
            Scope::Scoped {
                enabled: Some(false),
                ..
            } => {}
            other => panic!("Expected Scoped with enabled=false, got: {:?}", other),
        }
    }

    #[test]
    fn test_when_duplicate_declaration_errors() {
        let yaml = r#"
__when__:
  core: mihomo
__when__:
  platform: macos
rules:
  $prepend:
    - DOMAIN-SUFFIX,test.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "重复 __when__ 声明应返回错误");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("__when__"), "错误信息应提及 __when__");
    }

    #[test]
    fn test_unknown_dollar_operator_with_levenshtein_suggestion() {
        let yaml = r#"
proxies:
  $foo: "some expression"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "Unknown $ operator should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unknown DSL operation") || err_msg.contains("$foo"),
            "Error should mention unknown operation: {}",
            err_msg
        );
    }

    #[test]
    fn test_empty_yaml_mapping_produces_no_patches() {
        let yaml = "{}";
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert!(
            patches.is_empty(),
            "Empty YAML mapping should produce no patches"
        );
    }

    #[test]
    fn test_non_mapping_root_errors() {
        let yaml = "[1, 2, 3]";
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "Non-mapping root (array) should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("mapping") || err_msg.contains("dict"),
            "Error should mention mapping requirement: {}",
            err_msg
        );
    }

    #[test]
    fn test_filter_complex_expression() {
        let yaml = r#"
proxies:
  $filter: "p.type == 'SS' && p.name.includes('HK')"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Complex filter expression should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Filter { .. }));
    }

    #[test]
    fn test_transform_with_regex_replace() {
        let yaml = r#"
proxies:
  $transform: "{name: p.name.replace(/old/new/)}"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Transform with regex replace should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Transform { .. }));
    }

    #[test]
    fn test_remove_with_negation() {
        let yaml = r#"
proxies:
  $remove: "!(p.name.includes('test'))"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Remove with negation should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Remove { .. }));
    }

    #[test]
    fn test_composite_patch_filter_transform_append() {
        let yaml = r#"
proxies:
  $filter: "p.type == 'ss'"
  $transform: "{...p, name: 'prefix-' + p.name}"
  $append:
    - name: "manual-node"
      type: ss
      server: manual.com
      port: 443
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "Composite patch with filter+transform+append should parse"
        );
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(patches[0].is_composite(), "Should be composite");
        // Verify all_ops returns 3 operations in correct order
        let all_ops = patches[0].all_ops();
        assert_eq!(all_ops.len(), 3, "Should have 3 sub-operations");
        assert!(
            matches!(all_ops[0].op, PatchOp::Filter { .. }),
            "First should be Filter"
        );
        assert!(
            matches!(all_ops[1].op, PatchOp::Transform { .. }),
            "Second should be Transform"
        );
        assert!(
            matches!(all_ops[2].op, PatchOp::Append),
            "Third should be Append"
        );
    }

    #[test]
    fn test_default_with_nested_path() {
        let yaml = r#"
dns.nameservers.0:
  $default: "1.1.1.1"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Default with nested path should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "dns.nameservers.0");
        assert!(matches!(patches[0].op, PatchOp::SetDefault));
    }

    #[test]
    fn test_deep_merge_deeply_nested_object() {
        let yaml = r#"
dns:
  enable: true
  nameservers:
    primary:
      address: "1.1.1.1"
      port: 53
    secondary:
      address: "8.8.8.8"
      port: 53
  fallback:
    enabled: true
    servers:
      - "1.0.0.1"
      - "8.8.4.4"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Deeply nested object should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::DeepMerge));
        // Verify the nested structure is preserved
        let val = &patches[0].value;
        assert_eq!(val["nameservers"]["primary"]["address"], "1.1.1.1");
        assert_eq!(val["fallback"]["servers"][0], "1.0.0.1");
    }

    #[test]
    fn test_override_with_complex_nested_value() {
        let yaml = r#"
tun:
  $override:
    enable: true
    stack: mixed
    dns-hijack:
      - "any:53"
    auto-route: true
    auto-detect-interface: true
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "Override with complex nested value should parse"
        );
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Override));
        let val = &patches[0].value;
        assert_eq!(val["dns-hijack"][0], "any:53");
        assert_eq!(val["auto-route"], true);
    }

    #[test]
    fn test_very_long_expression_string() {
        // 1000+ 字符的表达式字符串 — 应该能处理而不崩溃
        let long_field = "a".repeat(100);
        let long_expr = format!("p.name == '{}'", long_field);
        let yaml = format!(
            r#"
proxies:
  $filter: "{}"
"#,
            long_expr
        );
        let result = DslParser::parse_str(&yaml, None);
        assert!(
            result.is_ok(),
            "Very long expression should parse without crashing"
        );
    }

    #[test]
    fn test_unicode_in_proxy_names_and_expressions() {
        let yaml = r#"
proxies:
  $filter: "p.name.includes('日本')"
  $transform: "{...p, name: '🇯🇵 ' + p.name}"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "Unicode in expressions should parse");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(patches[0].is_composite());
    }

    #[test]
    fn test_prepend_with_conditional_rule_objects() {
        let yaml = r#"
rules:
  $prepend:
    - DOMAIN-SUFFIX,google.com,PROXY
    - __when__:
        enabled: false
      __rule__: DOMAIN-SUFFIX,disabled.com,PROXY
    - __when__:
        enabled: true
      __rule__: DOMAIN-SUFFIX,enabled.com,PROXY
    - __when__:
        platform: macos
      __rule__: DOMAIN-SUFFIX,apple.com,PROXY
    - DOMAIN-SUFFIX,github.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "Prepend with mixed conditional rule objects should parse"
        );
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        if let Some(arr) = patches[0].value.as_array() {
            // enabled:false should be filtered out → 4 items remain
            assert_eq!(arr.len(), 4, "enabled:false rule should be filtered out");
            assert_eq!(arr[0].as_str().unwrap(), "DOMAIN-SUFFIX,google.com,PROXY");
            // enabled:true should be resolved to plain string
            assert_eq!(arr[1].as_str().unwrap(), "DOMAIN-SUFFIX,enabled.com,PROXY");
            // platform condition should be preserved as conditional object
            assert!(
                arr[2].is_object(),
                "Runtime condition should be preserved as object"
            );
            assert_eq!(arr[3].as_str().unwrap(), "DOMAIN-SUFFIX,github.com,PROXY");
        } else {
            panic!("Expected array value");
        }
    }

    #[test]
    fn test_override_conflict_with_filter() {
        // $override 不能与任何其他操作混用
        let yaml = r#"
proxies:
  $override:
    - name: test
  $filter: "p.type == 'ss'"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "$override mixed with $filter should error");
    }

    #[test]
    fn test_string_literal_root_errors() {
        let yaml = r#""just a string""#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "String root should error (not a mapping)");
    }

    #[test]
    fn test_null_root_errors() {
        let yaml = "null";
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "Null root should error (not a mapping)");
    }

    #[test]
    fn test_numeric_root_errors() {
        let yaml = "42";
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "Numeric root should error (not a mapping)");
    }

    #[test]
    fn test_when_with_multiple_conditions() {
        let yaml = r#"
__when__:
  core: mihomo
  platform: macos
  time: "08:00-22:00"
  ssid: "OfficeWiFi"
rules:
  $prepend:
    - DOMAIN-SUFFIX,company.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "Multiple conditions in __when__ should parse"
        );
        let patches = result.unwrap();
        match &patches[0].scope {
            Scope::Scoped {
                core,
                platform,
                time_range,
                ssid,
                ..
            } => {
                assert_eq!(core.as_deref(), Some("mihomo"));
                assert!(platform.is_some());
                assert!(time_range.is_some());
                assert_eq!(ssid.as_deref(), Some("OfficeWiFi"));
            }
            other => panic!("Expected Scoped with multiple conditions, got: {:?}", other),
        }
    }

    #[test]
    fn test_after_as_array() {
        let yaml = r#"
__after__: ["file-a", "file-b", "file-c"]
dns:
  enable: true
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "__after__ as array should parse");
        let patches = result.unwrap();
        assert_eq!(patches[0].after.len(), 3);
    }

    #[test]
    fn test_after_invalid_type_errors() {
        let yaml = r#"
__after__: 42
dns:
  enable: true
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_err(),
            "__after__ with non-string/non-array should error"
        );
    }

    #[test]
    fn test_when_not_mapping_errors() {
        let yaml = r#"
__when__: "invalid"
rules:
  $prepend:
    - DOMAIN,test,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_err(),
            "__when__ as string (not mapping) should error"
        );
    }

    #[test]
    fn test_filter_and_remove_and_transform_composite_order() {
        let yaml = r#"
proxies:
  $remove: "p.name.includes('old')"
  $filter: "p.type == 'ss'"
  $transform: "{...p, tagged: true}"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok());
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(patches[0].is_composite());
        let all_ops = patches[0].all_ops();
        assert_eq!(all_ops.len(), 3);
        // Execution order: Filter(0) → Remove(1) → Transform(2)
        assert!(matches!(all_ops[0].op, PatchOp::Filter { .. }));
        assert!(matches!(all_ops[1].op, PatchOp::Remove { .. }));
        assert!(matches!(all_ops[2].op, PatchOp::Transform { .. }));
    }

    #[test]
    fn test_default_and_prepend_composite() {
        let yaml = r#"
rules:
  $default:
    - MATCH,DIRECT
  $prepend:
    - DOMAIN-SUFFIX,example.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok());
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(patches[0].is_composite());
        let all_ops = patches[0].all_ops();
        assert_eq!(all_ops.len(), 2);
        // Execution order: SetDefault(3) → Prepend(4)
        assert!(matches!(all_ops[0].op, PatchOp::SetDefault));
        assert!(matches!(all_ops[1].op, PatchOp::Prepend));
    }

    #[test]
    fn test_deep_merge_with_scalar_value() {
        let yaml = r#"
mode: rule
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok());
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "mode");
        assert!(matches!(patches[0].op, PatchOp::DeepMerge));
        assert_eq!(
            patches[0].value,
            serde_json::Value::String("rule".to_string())
        );
    }

    #[test]
    fn test_deep_merge_with_array_value() {
        let yaml = r#"
dns-nameserver:
  - 1.1.1.1
  - 8.8.8.8
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok());
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "dns-nameserver");
        assert!(patches[0].value.is_array());
        assert_eq!(patches[0].value.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_filter_expression_with_runtime_field_in_string() {
        // p.name.includes('delayed') — 'delayed' 在字符串中，不应触发运行时字段检测
        let yaml = r#"
proxies:
  $filter: "p.name.includes('delayed')"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "Runtime field name inside string literal should NOT be rejected"
        );
    }

    #[test]
    fn test_filter_expression_with_runtime_field_in_template_string() {
        // p.name == 'test' && p.delay < 200 — delay 是真正的运行时字段引用
        let yaml = r#"
proxies:
  $filter: "p.name == 'test' && p.delay < 200"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_err(),
            "Actual runtime field reference should be rejected"
        );
    }

    #[test]
    fn test_unknown_operator_close_to_valid_gets_suggestion() {
        // "$filtre" is close to "$filter" (Levenshtein distance 1)
        let yaml = r#"
proxies:
  $filtre: "p.type == 'ss'"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Did you mean"),
            "Should suggest closest valid operator: {}",
            err_msg
        );
    }

    #[test]
    fn test_unknown_operator_far_from_valid_lists_all() {
        // "$zzzzz" is far from any valid operator — should still get a suggestion
        // (Levenshtein always finds the closest match)
        let yaml = r#"
proxies:
  $zzzzz: "something"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unknown DSL operation") || err_msg.contains("Did you mean"),
            "Should report unknown operation with suggestion: {}",
            err_msg
        );
    }

    // ══════════════════════════════════════════════════════════
    // 边界测试 — 刁难、临界、对抗性情况
    // ══════════════════════════════════════════════════════════

    /// 1. 空文件解析
    #[test]
    fn test_parse_empty_file() {
        let yaml = "";
        let result = DslParser::parse_str(yaml, None);
        // Empty string is not a valid YAML mapping — should error
        assert!(result.is_err(), "空字符串不是有效的 YAML mapping");
    }

    /// 2. 只有 __when__ 无操作
    #[test]
    fn test_parse_only_when() {
        let yaml = r#"
__when__:
  core: mihomo
  platform: macos
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "只有 __when__ 无操作应成功解析");
        let patches = result.unwrap();
        assert!(patches.is_empty(), "无操作键时应产生 0 个 patch");
    }

    /// 3. 无效 YAML
    #[test]
    fn test_parse_invalid_yaml() {
        let yaml = r#"
dns:
  enable: true
  - invalid: list inside mapping
    broken: [unclosed
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "无效 YAML 应返回错误");
    }

    /// 4. 非映射 YAML（如数组）
    #[test]
    fn test_parse_non_mapping_yaml() {
        let yaml = r#"
- item1
- item2
- item3
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "数组根节点应返回错误");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("mapping") || err_msg.contains("dict"),
            "错误信息应提及 mapping: {}",
            err_msg
        );
    }

    /// 5. 重复 __when__ 应报错
    #[test]
    fn test_parse_duplicate_when() {
        let yaml = r#"
__when__:
  core: mihomo
__when__:
  platform: linux
dns:
  enable: true
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "重复 __when__ 应报错");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("__when__"),
            "错误信息应提及 __when__: {}",
            err_msg
        );
    }

    /// 6. override 与其他操作混用应报错
    #[test]
    fn test_parse_override_with_other_ops() {
        let yaml = r#"
proxies:
  $override:
    - name: test
  $append:
    - name: another
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "$override 与 $append 混用应报错");
    }

    /// 7. 未知的 $ 前缀键应报错
    #[test]
    fn test_parse_unknown_dollar_key() {
        let yaml = r#"
proxies:
  $explode: "p.type == 'ss'"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "未知 $ 前缀键应报错");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unknown DSL operation") || err_msg.contains("$explode"),
            "错误信息应提及未知操作: {}",
            err_msg
        );
    }

    /// 8. 空 filter 表达式
    #[test]
    fn test_parse_empty_filter_expression() {
        let yaml = r#"
proxies:
  $filter: ""
"#;
        let result = DslParser::parse_str(yaml, None);
        // Empty expression is technically valid syntax but semantically questionable.
        // The parser should accept it (validation happens at runtime).
        assert!(result.is_ok(), "空 filter 表达式在语法层面应被接受");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::Filter { .. }));
    }

    /// 9. 复杂 __when__ 条件（多平台+时间范围）
    #[test]
    fn test_parse_complex_condition() {
        let yaml = r#"
__when__:
  core: mihomo
  platform:
    - macos
    - linux
  time: "08:00-23:00"
  ssid: "OfficeWiFi"
  enabled: true
rules:
  $prepend:
    - DOMAIN-SUFFIX,company.com,PROXY
    - DOMAIN-SUFFIX,internal.net,DIRECT
dns:
  enable: true
  enhanced-mode: fake-ip
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "复杂 __when__ 条件应成功解析");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 2, "应有 2 个 patch (rules + dns)");

        // 验证 scope 条件
        match &patches[0].scope {
            Scope::Scoped {
                core,
                platform,
                time_range,
                ssid,
                enabled,
                ..
            } => {
                assert_eq!(core.as_deref(), Some("mihomo"));
                assert_eq!(platform.as_ref().map(|v| v.len()), Some(2));
                assert!(time_range.is_some());
                assert_eq!(time_range.as_ref().unwrap().start, (8, 0));
                assert_eq!(time_range.as_ref().unwrap().end, (23, 0));
                assert_eq!(ssid.as_deref(), Some("OfficeWiFi"));
                assert_eq!(*enabled, Some(true));
            }
            other => panic!("Expected Scoped variant, got: {:?}", other),
        }
    }

    /// 10. __after__ 依赖声明
    #[test]
    fn test_parse_after_dependency_boundary() {
        let yaml = r#"
__after__: ["base-network", "base-dns", "base-rules"]
tun:
  $override:
    enable: true
    stack: mixed
proxies:
  $filter: "p.type == 'ss'"
rules:
  $prepend:
    - DOMAIN-SUFFIX,example.com,PROXY
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_ok(), "__after__ 依赖声明应成功解析");
        let patches = result.unwrap();
        assert_eq!(patches.len(), 3, "应有 3 个 patch");

        // 所有 patch 都应携带相同的 3 个依赖
        for patch in &patches {
            assert_eq!(patch.after.len(), 3, "每个 patch 应有 3 个依赖");
            let dep_names: Vec<&str> = patch
                .after
                .iter()
                .filter_map(|d| match d {
                    DependencyRef::FileName(name) => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            assert!(dep_names.contains(&"base-network"));
            assert!(dep_names.contains(&"base-dns"));
            assert!(dep_names.contains(&"base-rules"));
        }
    }

    // ─── DSL 解析边界扩展测试 ───

    /// 验证：同一顶层键下多个子键被正确合并为 DeepMerge。
    /// 例如 dns: { enable: true, ipv6: false } 应产生包含两个字段的 merge_val。
    #[test]
    fn test_parse_deep_merge_multiple_keys_under_same_top_level() {
        let yaml = r#"
dns:
  enable: true
  ipv6: false
  enhanced-mode: fake-ip
  nameserver:
    - https://dns.alidns.com/dns-query
    - https://doh.pub/dns-query
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1, "应产生 1 个 patch");
        assert_eq!(patches[0].path, "dns");
        assert!(matches!(patches[0].op, PatchOp::DeepMerge));

        // 验证 value 包含所有子键
        let val = &patches[0].value;
        assert_eq!(val.get("enable").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(val.get("ipv6").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            val.get("enhanced-mode").and_then(|v| v.as_str()),
            Some("fake-ip")
        );
        let ns = val.get("nameserver").and_then(|v| v.as_array()).unwrap();
        assert_eq!(ns.len(), 2);
    }

    /// 验证：单独 $default 不与 DeepMerge 冲突。
    /// $default 应被解析为 SetDefault 操作。
    #[test]
    fn test_parse_default_alone() {
        let yaml = r#"
dns:
  $default:
    enable: true
    nameserver:
      - https://dns.alidns.com/dns-query
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::SetDefault));

        // 验证默认值内容
        let val = &patches[0].value;
        assert_eq!(val.get("enable").and_then(|v| v.as_bool()), Some(true));
    }

    /// 验证：$default 与 $filter/$prepend 等操作共存时正确处理。
    /// 同键多操作应合并为复合 Patch，sub_ops 按固定顺序排列。
    #[test]
    fn test_parse_default_with_other_ops() {
        let yaml = r#"
proxies:
  $filter: "p.type == 'ss'"
  $default:
    - {"name": "default-proxy", "type": "ss"}
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);

        // 应有 sub_ops（复合操作）
        assert!(!patches[0].sub_ops.is_empty(), "同键多操作应产生 sub_ops");

        // 验证 sub_ops 中包含 Filter 和 SetDefault
        let op_names: Vec<&str> = patches[0]
            .sub_ops
            .iter()
            .map(|s| s.op.display_name())
            .collect();
        assert!(
            op_names.contains(&"Filter"),
            "sub_ops 应包含 Filter，实际: {:?}",
            op_names
        );
        assert!(
            op_names.contains(&"SetDefault"),
            "sub_ops 应包含 SetDefault，实际: {:?}",
            op_names
        );
    }

    /// 验证：$override 与其他操作混用时正确报错。
    /// $override 是独占操作，不允许与其他操作共存。
    #[test]
    fn test_parse_override_conflict() {
        let yaml = r#"
tun:
  $override:
    enable: true
  $filter: "p.name == 'test'"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "$override 与其他操作混用应报错");
    }

    /// 验证：复合操作按固定顺序执行。
    /// 固定顺序: Filter → Remove → Transform → SetDefault → Prepend → Append → DeepMerge → Override
    #[test]
    fn test_parse_execution_order() {
        let yaml = r#"
proxies:
  $append:
    - {"name": "appended", "type": "ss"}
  $filter: "p.type == 'ss'"
  $prepend:
    - {"name": "prepended", "type": "ss"}
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);

        let sub_ops = &patches[0].sub_ops;
        assert_eq!(sub_ops.len(), 3, "应有 3 个子操作");

        // 验证执行顺序：Filter(0) → Prepend(4) → Append(5)
        assert_eq!(sub_ops[0].op.display_name(), "Filter");
        assert_eq!(sub_ops[1].op.display_name(), "Prepend");
        assert_eq!(sub_ops[2].op.display_name(), "Append");
    }

    /// 验证：$filter 中引用运行时字段（delay, speed 等）被拒绝。
    /// 运行时字段仅在执行时可用，不能在 DSL 解析时静态引用。
    #[test]
    fn test_parse_filter_runtime_field_rejected() {
        // delay 是运行时字段
        let yaml = r#"
proxies:
  $filter: "p.delay < 200"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "引用运行时字段 delay 应被拒绝");
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("delay"));
    }

    /// 验证：字符串中包含运行时字段名不触发误报。
    /// 例如 p.name.includes('delayed') 中的 'delayed' 不应报错。
    #[test]
    fn test_parse_filter_static_field_in_string() {
        let yaml = r#"
proxies:
  $filter: "p.name.includes('delayed')"
"#;
        let result = DslParser::parse_str(yaml, None);
        assert!(
            result.is_ok(),
            "字符串中的 'delayed' 不应触发运行时字段误报"
        );
    }

    /// 验证：Unicode 字段名正确处理。
    /// 中文、emoji 等非 ASCII 字符在字段名和值中应被正确解析。
    #[test]
    fn test_parse_unicode_field_names() {
        let yaml = r#"
proxies:
  $filter: "p.name.includes('香港')"
  $transform: "{...p, name: '🇭🇰 ' + p.name}"
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(!patches[0].sub_ops.is_empty());
    }

    /// 边界：空 YAML 文件。
    /// 空字符串不是有效的 YAML mapping，应返回错误。
    #[test]
    fn test_parse_empty_string_errors() {
        let yaml = "";
        let result = DslParser::parse_str(yaml, None);
        assert!(result.is_err(), "空字符串不是有效的 YAML mapping");
    }

    /// 边界：只有 __when__ 没有操作。
    /// 纯元数据文件应产生 0 个 patch。
    #[test]
    fn test_parse_only_when_no_operations() {
        let yaml = r#"
__when__:
  core: mihomo
  platform: macos
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 0, "仅有 __when__ 应产生 0 个 patch");
    }

    /// 边界：深层嵌套 YAML 结构。
    /// 验证深层嵌套的配置值被正确解析为 DeepMerge。
    #[test]
    fn test_parse_nested_yaml_structure() {
        let yaml = r#"
dns:
  enable: true
  fake-ip-filter:
    - "*.lan"
    - "localhost.ptlogin2.qq.com"
  nameserver-policy:
    "geosite:cn":
      - https://dns.alidns.com/dns-query
    "geosite:geolocation-!cn":
      - https://dns.google/dns-query
      - https://cloudflare-dns.com/dns-query
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(patches[0].op, PatchOp::DeepMerge));

        // 验证深层嵌套结构
        let val = &patches[0].value;
        let policy = val
            .get("nameserver-policy")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(policy.contains_key("geosite:cn"));
        assert!(policy.contains_key("geosite:geolocation-!cn"));
    }

    /// 验证：__when__ platform 条件解析。
    /// platform 可以是字符串或字符串数组。
    #[test]
    fn test_parse_when_condition_platform() {
        use clash_prism_core::scope::Platform;

        // 单平台
        let yaml = r#"
__when__:
  platform: macos
rules:
  $prepend:
    - DOMAIN-SUFFIX,apple.com,DIRECT
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        match &patches[0].scope {
            Scope::Scoped { platform, .. } => {
                assert_eq!(platform.as_ref().map(|v| v.len()), Some(1));
                assert_eq!(platform.as_ref().unwrap()[0], Platform::MacOS);
            }
            other => panic!("Expected Scoped, got: {:?}", other),
        }

        // 多平台
        let yaml2 = r#"
__when__:
  platform:
    - macos
    - linux
    - windows
rules:
  $prepend:
    - MATCH,DIRECT
"#;
        let patches2 = DslParser::parse_str(yaml2, None).unwrap();
        match &patches2[0].scope {
            Scope::Scoped { platform, .. } => {
                assert_eq!(platform.as_ref().map(|v| v.len()), Some(3));
                assert!(platform.as_ref().unwrap().contains(&Platform::MacOS));
                assert!(platform.as_ref().unwrap().contains(&Platform::Linux));
                assert!(platform.as_ref().unwrap().contains(&Platform::Windows));
            }
            other => panic!("Expected Scoped, got: {:?}", other),
        }
    }

    /// 验证：__when__ time 条件解析。
    /// time 范围格式为 "HH:MM-HH:MM"。
    #[test]
    fn test_parse_when_condition_time_range() {
        let yaml = r#"
__when__:
  time: "09:00-18:00"
rules:
  $prepend:
    - DOMAIN-SUFFIX,work.com,PROXY
"#;
        let patches = DslParser::parse_str(yaml, None).unwrap();
        assert_eq!(patches.len(), 1);
        match &patches[0].scope {
            Scope::Scoped { time_range, .. } => {
                let tr = time_range.as_ref().unwrap();
                assert_eq!(tr.start, (9, 0));
                assert_eq!(tr.end, (18, 0));
            }
            other => panic!("Expected Scoped with time_range, got: {:?}", other),
        }
    }

    /// test_parse_default_null_value -- $default: null 能被正确解析
    ///
    /// 验证 $default 的值为 YAML null 时，解析器能正确生成 PatchOp::SetDefault，
    /// 且 Patch 的 value 字段为 Value::Null。
    #[test]
    fn test_parse_default_null_value() {
        let input = r#"
dns:
  $default: null
"#;
        let patches = DslParser::parse_str(input, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "dns");
        assert!(matches!(patches[0].op, PatchOp::SetDefault));
        assert!(
            patches[0].value.is_null(),
            "$default: null 的值应为 Value::Null"
        );
    }

    /// test_parse_default_null_in_array_field -- 数组字段的 $default: null
    ///
    /// 验证 rules 等数组字段的 $default: null 也能被正确解析。
    #[test]
    fn test_parse_default_null_in_array_field() {
        let input = r#"
rules:
  $default: null
"#;
        let patches = DslParser::parse_str(input, None).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "rules");
        assert!(matches!(patches[0].op, PatchOp::SetDefault));
        assert!(patches[0].value.is_null());
    }
}
