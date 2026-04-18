//! # Patch IR — Unified Intermediate Representation
//!
//! All inputs (Prism DSL / JS Scripts / Plugins) compile to this unified IR.
//! **With IR comes Explain View; without IR, it's a black box.**
//!
//! ## Core Abstraction
//!
//! [`Patch`] is the central abstraction of Prism Engine. Every configuration transformation —
//! whether from a hand-written `.prism.yaml` file,
//! a JavaScript script, or a plugin — is represented as one or more `Patch` objects.
//!
//! ## Operation Types ([`PatchOp`])
//!
//! | Operation | DSL Syntax | Description |
//! |-----------|-----------|-------------|
//! | [`DeepMerge`](PatchOp::DeepMerge) | (default key) | Recursive deep merge |
//! | [`Override`](PatchOp::Override) | `$override` | Force replace (exclusive key) |
//! | [`Prepend`](PatchOp::Prepend) | `$prepend` | Array prepend insert |
//! | [`Append`](PatchOp::Append) | `$append` | Array append insert |
//! | [`Filter`](PatchOp::Filter) | `$filter` | Conditional filter (keep matching) |
//! | [`Transform`](PatchOp::Transform) | `$transform` | Map transform (batch modify) |
//! | [`Remove`](PatchOp::Remove) | `$remove` | Conditional remove (delete matching) |
//! | [`SetDefault`](PatchOp::SetDefault) | `$default` | Default value injection |
//!
//! ## Composite Operations
//!
//! When multiple operations target the same config key (e.g., `$filter` + `$prepend`),
//! they are collected into a single [`Patch`] with [`Patch::sub_ops`]. The executor
//! applies them in fixed order: `$filter` → `$remove` → `$transform` → `$default` → `$prepend` → `$append`.
//!
//! ## Static Field Whitelist
//!
//! `$filter` and `$transform` expressions can only reference **static fields**
//! (see [`STATIC_PROXY_FIELDS`]). Runtime fields like `delay`, `latency`, etc.
//! (see [`RUNTIME_PROXY_FIELDS`]) are rejected at compile time.
//! Use [`is_runtime_field()`] and [`is_static_field()`] for field classification.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::PrismError;
use crate::scope::Scope;
use crate::source::PatchSource;

/// Unique identifier for each Patch (auto-generated UUID v4).
///
/// Used for:
/// - Dependency resolution (`__after__` declarations)
/// - Trace-to-Patch correlation in Explain View
/// - Replay step identification
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct PatchId(String);

impl PatchId {
    /// Generate a new random PatchId (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create a PatchId from an existing string (for testing or deserialization).
    ///
    /// Validates the format: only UUID v4 (8-4-4-4-12 hex) or simple identifiers
    /// (no `.` characters) are allowed. Strings containing `.` will trigger a warning.
    ///
    /// Note
    ///
    /// IDs containing `.` are **warned but not rejected** for backward compatibility
    /// with existing configurations. The dot character can cause issues with prefix
    /// matching in [`TraceManager::trace_affects_path()`], where a PatchId like
    /// `"dns.nameserver"` might incorrectly match the path `"dns"`.
    ///
    /// **Recommendation**: Use UUID v4 (via [`PatchId::new()`]) or simple identifiers
    /// without dots for new code.
    pub fn from_identifier(s: impl Into<String>) -> Self {
        let s = s.into();
        match Self::validate_format(&s) {
            Ok(()) => {}
            Err(warning) => {
                // DESIGN COMPROMISE (intentional): IDs containing `.` are warned but
                // not rejected for backward compatibility with existing configurations.
                // The dot character can cause ambiguous prefix matching in
                // `TraceManager::trace_affects_path()`, where `"dns.nameserver"` might
                // incorrectly match the path `"dns"`. New code should use UUID v4 or
                // dot-free identifiers via `PatchId::new()`.
                tracing::warn!(id = s, "{}", warning);
                // In debug builds, catch accidental dot-containing IDs early during
                // development. Release builds continue to accept them silently (warn only).
                debug_assert!(
                    !s.contains('.'),
                    "PatchId '{}' contains '.' — use PatchId::new() or a dot-free identifier",
                    s
                );
            }
        }
        Self(s)
    }

    /// Validate the format of a PatchId string.
    ///
    /// Allowed formats:
    /// - UUID v4: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12 hex digits)
    /// - Simple identifiers: strings without `.` characters
    ///
    /// Returns `Ok(())` for valid formats, `Err(warning_message)` for questionable formats.
    /// The PatchId is still created for backward compatibility even when validation fails.
    pub fn validate_format(s: &str) -> Result<(), String> {
        // UUID v4 format: 8-4-4-4-12 hex digits (compiled once via LazyLock)
        static UUID_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
            )
            .expect("UUID_RE: hardcoded regex pattern must be valid")
        });
        if UUID_RE.is_match(s) {
            return Ok(()); // Valid UUID v4 format
        }
        // Simple identifier: no dots allowed
        if !s.contains('.') {
            return Ok(()); // Valid simple identifier
        }
        // Contains dots — return warning
        Err(
            "PatchId contains '.' characters, which may cause prefix matching issues. \
             Use UUID v4 or simple identifiers without dots."
                .to_string(),
        )
    }

    /// Get the underlying string representation of this PatchId.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for PatchId {
    fn default() -> Self {
        Self::new()
    }
}

/// Dependency reference type.
///
/// Distinguishes between file-level references (DSL user-written, e.g., `"base-dns"`)
/// and runtime ID references (dynamically generated by scripts/plugins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DependencyRef {
    /// 文件名引用（DSL 层用户写的，如 "base-dns"）
    FileName(String),

    /// 运行时 ID 引用（脚本/插件动态生成的 Patch 之间的依赖）
    PatchId(PatchId),
}

/// Compiled predicate expression (placeholder; actual compilation by clash-prism-script).
///
/// ## 设计说明
///
/// 架构文档 §2.1 声明: "Patch Compiler 使用 serde_yml 的 Value 解析后提取 `$` 前缀键"
/// 表达式编译（`$filter`/`$transform`/`$remove` 的 expr 字段）由 `clash-prism-script` crate 中的
/// `executor/expr.rs` 模块在**运行时**完成，而非编译时。此结构体仅存储原始表达式字符串
/// 和编译期提取的字段引用列表。
///
/// ### 为什么不在 IR 编译阶段做表达式解析？
/// 1. DSL 编译器 (`clash-prism-dsl`) 不依赖 JS 引擎 — 保持 crate 边界清晰
/// 2. 表达式语法可能扩展（如未来支持管道、函数调用），运行时解析更灵活
/// 3. 静态字段白名单校验在编译期完成（`referenced_fields`），运行时只做值计算
///
/// 运行时求值入口: `clash_prism_core::executor::expr::evaluate_predicate()` / `evaluate_transform_expr()`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledPredicate {
    pub expr: String,
    /// 编译时校验通过的字段引用列表（用于静态字段白名单校验）
    #[serde(default)]
    pub referenced_fields: Vec<String>,
}

impl CompiledPredicate {
    /// Create a new compiled predicate with the given expression and field references.
    ///
    /// Validates that `referenced_fields` does not contain any runtime fields
    /// (e.g., `delay`, `latency`, `speed`). Runtime fields are only meaningful
    /// after speed testing and should never appear in static filter/transform expressions.
    ///
    /// Both debug and release builds **warn** (not reject) runtime fields with a
    /// tracing::warn log. This ensures consistent behavior across build profiles while
    /// maintaining forward compatibility with existing configurations.
    ///
    /// - 原注释错误地描述为 "reject"，实际行为是 "warn"（记录警告但不拒绝）。
    /// - Parser 层（clash-prism-dsl）已经在编译期硬性拒绝了运行时字段，
    ///   此处的 warn 是纵深防御的第二层，捕获 parser 遗漏的情况。
    ///   两层防护互补：parser 层硬性拒绝（编译错误），IR 层 warn（运行时警告）。
    pub fn new(expr: impl Into<String>, referenced_fields: Vec<String>) -> Self {
        for field in &referenced_fields {
            if is_runtime_field(field) {
                tracing::warn!(
                    field = field,
                    "CompiledPredicate::new(): runtime field '{}' detected in static predicate. \
                     This field is only meaningful after speed testing and may produce \
                     incorrect results. Use Smart Selector for latency-based selection logic.",
                    field
                );
            }
        }
        Self {
            expr: expr.into(),
            referenced_fields,
        }
    }

    /// Create a constant predicate (true/false) with no field references.
    /// Used for always-true or always-false conditions.
    pub fn constant(value: bool) -> Self {
        Self {
            expr: value.to_string(),
            referenced_fields: vec![],
        }
    }
}

/// Operation type — defines 8 Prism DSL operations as IR operations.
///
/// Each variant corresponds to a `$`-prefixed operator in Prism DSL syntax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PatchOp {
    /// 递归深度合并（默认行为）
    DeepMerge,

    /// 强制替换（不递归），独占键
    Override,

    /// 数组前置插入
    Prepend,

    /// 数组末尾追加
    Append,

    /// 条件过滤（保留匹配元素），仅限静态字段
    /// Field references are stored in `expr.referenced_fields` (single source of truth).
    Filter { expr: CompiledPredicate },

    /// 映射变换（批量修改）
    /// Field references are stored in `expr.referenced_fields` (single source of truth).
    Transform { expr: CompiledPredicate },

    /// 条件删除，仅限静态字段
    /// Field references are stored in `expr.referenced_fields` (single source of truth).
    Remove { expr: CompiledPredicate },

    /// 仅当字段不存在时设置（默认值注入）
    SetDefault,
}

impl PatchOp {
    /// Get the human-readable display name for this operation.
    /// Used in Trace View / Explain View UI.
    pub fn display_name(&self) -> &str {
        match self {
            PatchOp::DeepMerge => "DeepMerge",
            PatchOp::Override => "Override",
            PatchOp::Prepend => "Prepend",
            PatchOp::Append => "Append",
            PatchOp::Filter { .. } => "Filter",
            PatchOp::Transform { .. } => "Transform",
            PatchOp::Remove { .. } => "Remove",
            PatchOp::SetDefault => "SetDefault",
        }
    }

    /// Check if this is an array operation (`$prepend` / `$append` / `$filter` / `$transform` / `$remove`).
    /// Array operations require the target path to resolve to a JSON array.
    pub fn is_array_op(&self) -> bool {
        matches!(
            self,
            PatchOp::Prepend
                | PatchOp::Append
                | PatchOp::Filter { .. }
                | PatchOp::Transform { .. }
                | PatchOp::Remove { .. }
        )
    }

    /// Check if this is a map/object operation (`DeepMerge` / `Override` / `SetDefault`).
    /// Map operations require the target path to resolve to a JSON object.
    pub fn is_map_op(&self) -> bool {
        matches!(
            self,
            PatchOp::DeepMerge | PatchOp::Override | PatchOp::SetDefault
        )
    }
}

/// A sub-operation within a composite patch, carrying both the operation type
/// and its associated value (if any).
///
/// When multiple DSL operations target the same config key (e.g., `$filter` + `$prepend`),
/// each operation is stored as a `SubOp` so the executor can access both the operation
/// type and its value independently.
///
/// For operations that carry their data inside the enum variant (e.g., `Filter { expr }`),
/// the `value` field is `Value::Null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubOp {
    /// The operation type (e.g., Prepend, Append, SetDefault, Filter, etc.)
    pub op: PatchOp,
    /// The associated value for this operation.
    /// - Prepend/Append: the array of items to insert
    /// - SetDefault: the default value to inject
    /// - Filter/Remove/Transform: `Value::Null` (expr is inside the PatchOp variant)
    pub value: serde_json::Value,
}

/// Core abstraction of Prism Engine — unified configuration transformation.
///
/// All inputs (Prism DSL / Scripts / Plugins) compile to this type.
/// A `Patch` represents a single atomic transformation on the target configuration.
///
/// ## Lifecycle
///
/// ```text
/// DSL File → DslParser → Patch → PatchCompiler (sort) → PatchExecutor → Final Config
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    /// 唯一标识（自动生成）
    pub id: PatchId,

    /// 来源追踪（哪个文件、哪一行、哪个插件生成的）
    pub source: PatchSource,

    /// 作用域
    pub scope: Scope,

    /// 目标配置路径（如 "dns", "rules", "proxy-groups"）
    pub path: String,

    /// 主操作类型
    ///
    /// 对于单一操作 Patch，这就是唯一的操作。
    /// 对于复合操作 Patch（同键多操作），这是第一个/主要的操作，
    /// 完整操作列表见 `sub_ops`。
    pub op: PatchOp,

    /// 附加值（因 op 不同含义不同）
    pub value: serde_json::Value,

    /// 执行条件（可选，仅当条件为 true 时执行此 Patch）
    pub condition: Option<CompiledPredicate>,

    /// 依赖（此 Patch 必须在指定目标之后执行）
    /// 不再提供 priority 字段，只用 after 声明依赖
    /// 同级无依赖的 Patch 按文件名字典序排列（确定性）
    pub after: Vec<DependencyRef>,

    /// 子操作列表（同键多操作的复合执行）
    ///
    /// 当同一个配置键下有多个操作时（如 `$filter` + `$prepend`），
    /// parser 将它们收集到此列表中。executor 会按固定顺序依次执行：
    ///   $filter → $remove → $transform → $default → $prepend → $append
    ///
    /// 如果为空，表示这是一个单一操作 Patch，只需执行 `op` 即可。
    #[serde(default)]
    pub sub_ops: Vec<SubOp>,
}

impl Patch {
    /// Create a new Patch with the given parameters.
    ///
    /// # Arguments
    /// * `source` — Origin information (file, plugin, script, etc.)
    /// * `scope` — Execution scope (Global / Profile / Scoped / Runtime)
    /// * `path` — Target config path (e.g., "dns", "rules", "proxies")
    /// * `op` — Primary operation type
    /// * `value` — Operation payload (meaning depends on `op`)
    pub fn new(
        source: PatchSource,
        scope: Scope,
        path: impl Into<String>,
        op: PatchOp,
        value: serde_json::Value,
    ) -> Self {
        Self {
            id: PatchId::new(),
            source,
            scope,
            path: path.into(),
            op,
            value,
            condition: None,
            after: vec![],
            sub_ops: vec![],
        }
    }

    /// Set the execution condition for this Patch.
    /// The Patch will only execute when the condition evaluates to true.
    pub fn with_condition(mut self, condition: CompiledPredicate) -> Self {
        self.condition = Some(condition);
        self
    }

    /// Add a dependency — this Patch must execute after the specified target.
    pub fn with_after(mut self, dep: DependencyRef) -> Self {
        self.after.push(dep);
        self
    }

    /// Set all dependencies at once (replaces any existing dependencies).
    pub fn with_deps(mut self, deps: Vec<DependencyRef>) -> Self {
        self.after = deps;
        self
    }

    /// Set sub-operations for composite execution (multiple ops on same key).
    /// See [`Patch::all_ops()`] for execution order details.
    ///
    /// # Errors
    ///
    /// Returns [`PrismError::OverrideConflict`] if `$override` is mixed with any
    /// other operation on the same key. `$override` is an exclusive operation — it
    /// force-replaces the entire value, so combining it with `$filter`, `$prepend`,
    /// etc. is semantically meaningless.
    pub fn with_sub_ops(mut self, sub_ops: Vec<SubOp>) -> std::result::Result<Self, PrismError> {
        // $override 独占检查 — 不允许与其他操作混用
        let has_override = sub_ops.iter().any(|op| matches!(op.op, PatchOp::Override));
        if has_override && sub_ops.len() > 1 {
            return Err(PrismError::OverrideConflict {
                field: self.path.clone(),
            });
        }
        self.sub_ops = sub_ops;
        Ok(self)
    }

    /// Check if this Patch is a composite operation (contains sub-operations).
    /// Composite patches have multiple operations targeting the same config key.
    pub fn is_composite(&self) -> bool {
        !self.sub_ops.is_empty()
    }

    /// Get all operations to execute (primary + sub-operations), in fixed order.
    ///
    /// Fixed execution order: `$filter`(0) → `$remove`(1) → `$transform`(2) → `$default`(3) → `$prepend`(4) → `$append`(5)
    ///
    /// For single-operation patches, returns `vec![SubOp { op: self.op, value: self.value }]`.
    /// For composite patches, sorts all operations by [`execution_order()`].
    pub fn all_ops(&self) -> Vec<SubOp> {
        if self.sub_ops.is_empty() {
            // Single operation: return only the primary op with its value
            vec![SubOp {
                op: self.op.clone(),
                value: self.value.clone(),
            }]
        } else {
            // Composite: sub_ops already contains ALL operations (including primary).
            // Sort by execution priority and return.
            let mut ops: Vec<SubOp> = self.sub_ops.to_vec();
            ops.sort_by_key(|sub_op| execution_order(&sub_op.op));
            ops
        }
    }
}

/// Fixed execution priority for operations (lower value = executes first).
///
/// Order: Filter(0) -> Remove(1) -> Transform(2) -> SetDefault(3) -> Prepend(4) -> Append(5) -> DeepMerge(6) -> Override(7)
///
/// This is the single source of truth for execution ordering.
/// `clash_prism_dsl::ops` delegates to this function via `op_priority()`.
pub fn execution_order(op: &PatchOp) -> u8 {
    match op {
        PatchOp::Filter { .. } => 0,
        PatchOp::Remove { .. } => 1,
        PatchOp::Transform { .. } => 2,
        PatchOp::SetDefault => 3,
        PatchOp::Prepend => 4,
        PatchOp::Append => 5,
        PatchOp::DeepMerge => 6,
        PatchOp::Override => 7,
    }
}

// ══════════════════════════════════════════════════════════
// Static Field Whitelist (§2.6)
// ══════════════════════════════════════════════════════════

/// Static proxy fields allowed in `$filter` / `$transform` expressions.
///
/// These fields are available at **compile time** (they exist in the YAML source),
/// so they can be safely used in predicate/transform expressions without runtime dependency.
pub const STATIC_PROXY_FIELDS: &[&str] = &[
    // 基础标识
    "name",
    "type",
    "server",
    "port",
    // 认证参数
    "uuid",
    "password",
    "cipher",
    // TLS 参数
    "tls",
    "sni",
    "skip-cert-verify",
    "fingerprint",
    "alpn",
    // 传输参数
    "network",
    "ws-opts",
    "grpc-opts",
    "h2-opts",
    "reality-opts",
    // 协议参数
    "flow",
    "username",
    "alterId",
    "protocol",
    // 插件相关
    "plugin",
    "plugin-opts",
    // UDP 相关
    "udp",
    "udp-over-tcp",
    // 多路复用
    "smux",
    // 其他常见字段
    "servername",
    "client-fingerprint",
    "shadow-tls",
    "hy2-opts",
];

/// Runtime proxy fields — **rejected** at compile time in `$filter` / `$transform`.
///
/// These fields are only available after speed testing (runtime), so referencing them
/// in static expressions would produce meaningless results. Use Smart Selector instead
/// for latency-based selection logic.
pub const RUNTIME_PROXY_FIELDS: &[&str] = &[
    "delay",
    "latency",
    "speed",
    "loss_rate",
    "success_rate",
    "history",
    "alive",
    "last_test",
];

/// Check if a field name is a runtime field (not allowed in `$filter`/`$transform`).
pub fn is_runtime_field(field: &str) -> bool {
    RUNTIME_PROXY_FIELDS.contains(&field)
}

/// Check if a field name is a static field (allowed in `$filter`/`$transform`).
pub fn is_static_field(field: &str) -> bool {
    STATIC_PROXY_FIELDS.contains(&field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Scope;
    use crate::source::PatchSource;

    fn test_source() -> PatchSource {
        PatchSource::builtin()
    }

    // ─── PatchId ───

    #[test]
    fn test_patch_id_new_generates_unique() {
        let id1 = PatchId::new();
        let id2 = PatchId::new();
        assert_ne!(id1, id2);
        // UUID v4 format: 8-4-4-4-12
        assert_eq!(id1.as_str().len(), 36);
        assert_eq!(id2.as_str().len(), 36);
    }

    #[test]
    fn test_patch_id_from_identifier_simple() {
        let id = PatchId::from_identifier("my-patch");
        assert_eq!(id.as_str(), "my-patch");
    }

    #[test]
    fn test_patch_id_from_identifier_uuid_format() {
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000";
        let id = PatchId::from_identifier(uuid_str);
        assert_eq!(id.as_str(), uuid_str);
    }

    #[test]
    fn test_patch_id_from_identifier_with_dots_warns() {
        // Contains dots — validate_format returns a warning but does not reject.
        // The debug_assert in from_identifier will panic in debug builds, so we
        // test validate_format directly here to verify the warning behavior.
        let result = PatchId::validate_format("some.path.here");
        assert!(result.is_err(), "dots should produce a validation warning");
    }

    #[test]
    fn test_patch_id_display() {
        let id = PatchId::from_identifier("test-id");
        assert_eq!(format!("{}", id), "test-id");
    }

    #[test]
    fn test_patch_id_default() {
        let id = PatchId::default();
        assert_eq!(id.as_str().len(), 36); // UUID v4
    }

    #[test]
    fn test_patch_id_equality() {
        let id1 = PatchId::from_identifier("same");
        let id2 = PatchId::from_identifier("same");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_patch_id_ordering() {
        let id_a = PatchId::from_identifier("aaa");
        let id_b = PatchId::from_identifier("bbb");
        assert!(id_a < id_b);
    }

    // ─── Patch::new ───

    #[test]
    fn test_patch_new_basic() {
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({"enable": true}),
        );
        assert_eq!(patch.path, "dns");
        assert!(patch.condition.is_none());
        assert!(patch.after.is_empty());
        assert!(patch.sub_ops.is_empty());
        assert!(!patch.is_composite());
    }

    #[test]
    fn test_patch_new_generates_unique_id() {
        let p1 = Patch::new(
            test_source(),
            Scope::Global,
            "a",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        );
        let p2 = Patch::new(
            test_source(),
            Scope::Global,
            "a",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        );
        assert_ne!(p1.id, p2.id);
    }

    // ─── Builder chain ───

    #[test]
    fn test_patch_with_condition() {
        let cond = CompiledPredicate::new("true", vec![]);
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_condition(cond.clone());
        assert!(patch.condition.is_some());
        assert_eq!(patch.condition.as_ref().unwrap().expr, "true");
    }

    #[test]
    fn test_patch_with_after() {
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_after(DependencyRef::FileName("base.yaml".into()));
        assert_eq!(patch.after.len(), 1);
    }

    #[test]
    fn test_patch_with_deps() {
        let deps = vec![
            DependencyRef::FileName("a.yaml".into()),
            DependencyRef::FileName("b.yaml".into()),
        ];
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_deps(deps);
        assert_eq!(patch.after.len(), 2);
    }

    #[test]
    fn test_patch_with_sub_ops() {
        let sub_ops = vec![
            SubOp {
                op: PatchOp::Filter {
                    expr: CompiledPredicate::constant(true),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Prepend,
                value: serde_json::json!([1, 2]),
            },
        ];
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "proxies",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_sub_ops(sub_ops)
        .unwrap();
        assert!(patch.is_composite());
        assert_eq!(patch.sub_ops.len(), 2);
    }

    // ─── is_composite ───

    #[test]
    fn test_is_composite_true() {
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "p",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_sub_ops(vec![SubOp {
            op: PatchOp::Prepend,
            value: serde_json::json!([1]),
        }])
        .unwrap();
        assert!(patch.is_composite());
    }

    #[test]
    fn test_is_composite_false() {
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "p",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        );
        assert!(!patch.is_composite());
    }

    // ─── all_ops ───

    #[test]
    fn test_all_ops_single_returns_primary() {
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "dns",
            PatchOp::DeepMerge,
            serde_json::json!({"a": 1}),
        );
        let ops = patch.all_ops();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, PatchOp::DeepMerge);
        assert_eq!(ops[0].value, serde_json::json!({"a": 1}));
    }

    #[test]
    fn test_all_ops_composite_sorted_order() {
        // Sub ops should be sorted by execution_order:
        // Filter(0) → Remove(1) → Transform(2) → SetDefault(3) → Prepend(4) → Append(5) → DeepMerge(6) → Override(7)
        let sub_ops = vec![
            SubOp {
                op: PatchOp::Append,
                value: serde_json::json!([3]),
            },
            SubOp {
                op: PatchOp::Filter {
                    expr: CompiledPredicate::constant(true),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Prepend,
                value: serde_json::json!([1]),
            },
        ];
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "proxies",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_sub_ops(sub_ops)
        .unwrap();
        let ops = patch.all_ops();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].op.display_name(), "Filter");
        assert_eq!(ops[1].op.display_name(), "Prepend");
        assert_eq!(ops[2].op.display_name(), "Append");
    }

    #[test]
    fn test_all_ops_full_execution_order() {
        // Override is exclusive — test non-override ops together
        let sub_ops = vec![
            SubOp {
                op: PatchOp::DeepMerge,
                value: serde_json::json!({}),
            },
            SubOp {
                op: PatchOp::Append,
                value: serde_json::json!([]),
            },
            SubOp {
                op: PatchOp::Prepend,
                value: serde_json::json!([]),
            },
            SubOp {
                op: PatchOp::SetDefault,
                value: serde_json::json!(null),
            },
            SubOp {
                op: PatchOp::Transform {
                    expr: CompiledPredicate::constant(true),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Remove {
                    expr: CompiledPredicate::constant(true),
                },
                value: serde_json::Value::Null,
            },
            SubOp {
                op: PatchOp::Filter {
                    expr: CompiledPredicate::constant(true),
                },
                value: serde_json::Value::Null,
            },
        ];
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "p",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_sub_ops(sub_ops)
        .unwrap();
        let ops = patch.all_ops();
        let names: Vec<&str> = ops.iter().map(|o| o.op.display_name()).collect();
        assert_eq!(
            names,
            vec![
                "Filter",
                "Remove",
                "Transform",
                "SetDefault",
                "Prepend",
                "Append",
                "DeepMerge"
            ]
        );
    }

    #[test]
    fn test_all_ops_override_execution_order() {
        // Override alone should work (single sub-op)
        let sub_ops = vec![SubOp {
            op: PatchOp::Override,
            value: serde_json::json!({}),
        }];
        let patch = Patch::new(
            test_source(),
            Scope::Global,
            "p",
            PatchOp::Override,
            serde_json::json!({}),
        )
        .with_sub_ops(sub_ops)
        .unwrap();
        let ops = patch.all_ops();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op.display_name(), "Override");
    }

    #[test]
    fn test_with_sub_ops_override_conflict_returns_err() {
        // $override mixed with other ops should return Err
        let sub_ops = vec![
            SubOp {
                op: PatchOp::Override,
                value: serde_json::json!({}),
            },
            SubOp {
                op: PatchOp::DeepMerge,
                value: serde_json::json!({}),
            },
        ];
        let result = Patch::new(
            test_source(),
            Scope::Global,
            "p",
            PatchOp::DeepMerge,
            serde_json::json!({}),
        )
        .with_sub_ops(sub_ops);
        assert!(result.is_err(), "should return Err for override conflict");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("override") || err.contains("Override"),
            "error should mention override: {}",
            err
        );
    }

    // ─── PatchOp::display_name ───

    #[test]
    fn test_display_name_all_variants() {
        assert_eq!(PatchOp::DeepMerge.display_name(), "DeepMerge");
        assert_eq!(PatchOp::Override.display_name(), "Override");
        assert_eq!(PatchOp::Prepend.display_name(), "Prepend");
        assert_eq!(PatchOp::Append.display_name(), "Append");
        assert_eq!(
            PatchOp::Filter {
                expr: CompiledPredicate::constant(true)
            }
            .display_name(),
            "Filter"
        );
        assert_eq!(
            PatchOp::Transform {
                expr: CompiledPredicate::constant(true)
            }
            .display_name(),
            "Transform"
        );
        assert_eq!(
            PatchOp::Remove {
                expr: CompiledPredicate::constant(true)
            }
            .display_name(),
            "Remove"
        );
        assert_eq!(PatchOp::SetDefault.display_name(), "SetDefault");
    }

    // ─── PatchOp::is_array_op / is_map_op ───

    #[test]
    fn test_is_array_op() {
        assert!(PatchOp::Prepend.is_array_op());
        assert!(PatchOp::Append.is_array_op());
        assert!(
            PatchOp::Filter {
                expr: CompiledPredicate::constant(true)
            }
            .is_array_op()
        );
        assert!(
            PatchOp::Transform {
                expr: CompiledPredicate::constant(true)
            }
            .is_array_op()
        );
        assert!(
            PatchOp::Remove {
                expr: CompiledPredicate::constant(true)
            }
            .is_array_op()
        );
        assert!(!PatchOp::DeepMerge.is_array_op());
        assert!(!PatchOp::Override.is_array_op());
        assert!(!PatchOp::SetDefault.is_array_op());
    }

    #[test]
    fn test_is_map_op() {
        assert!(PatchOp::DeepMerge.is_map_op());
        assert!(PatchOp::Override.is_map_op());
        assert!(PatchOp::SetDefault.is_map_op());
        assert!(!PatchOp::Prepend.is_map_op());
        assert!(!PatchOp::Append.is_map_op());
        assert!(
            !PatchOp::Filter {
                expr: CompiledPredicate::constant(true)
            }
            .is_map_op()
        );
        assert!(
            !PatchOp::Transform {
                expr: CompiledPredicate::constant(true)
            }
            .is_map_op()
        );
        assert!(
            !PatchOp::Remove {
                expr: CompiledPredicate::constant(true)
            }
            .is_map_op()
        );
    }

    // ─── is_runtime_field / is_static_field ───

    #[test]
    fn test_is_runtime_field() {
        assert!(is_runtime_field("delay"));
        assert!(is_runtime_field("latency"));
        assert!(is_runtime_field("speed"));
        assert!(is_runtime_field("loss_rate"));
        assert!(is_runtime_field("success_rate"));
        assert!(is_runtime_field("history"));
        assert!(is_runtime_field("alive"));
        assert!(is_runtime_field("last_test"));
        assert!(!is_runtime_field("name"));
        assert!(!is_runtime_field("type"));
        assert!(!is_runtime_field("server"));
    }

    #[test]
    fn test_is_static_field() {
        assert!(is_static_field("name"));
        assert!(is_static_field("type"));
        assert!(is_static_field("server"));
        assert!(is_static_field("port"));
        assert!(is_static_field("uuid"));
        assert!(is_static_field("tls"));
        // "delay" is a runtime field, NOT a static field
        assert!(!is_static_field("delay"));
        assert!(!is_static_field("latency"));
        assert!(!is_static_field("speed"));
    }

    #[test]
    fn test_static_and_runtime_are_disjoint() {
        // Verify no field appears in both lists
        for &field in STATIC_PROXY_FIELDS {
            assert!(
                !RUNTIME_PROXY_FIELDS.contains(&field),
                "Field '{}' appears in both static and runtime lists",
                field
            );
        }
    }

    // ─── execution_order ───

    #[test]
    fn test_execution_order_priorities() {
        assert_eq!(
            execution_order(&PatchOp::Filter {
                expr: CompiledPredicate::constant(true)
            }),
            0
        );
        assert_eq!(
            execution_order(&PatchOp::Remove {
                expr: CompiledPredicate::constant(true)
            }),
            1
        );
        assert_eq!(
            execution_order(&PatchOp::Transform {
                expr: CompiledPredicate::constant(true)
            }),
            2
        );
        assert_eq!(execution_order(&PatchOp::SetDefault), 3);
        assert_eq!(execution_order(&PatchOp::Prepend), 4);
        assert_eq!(execution_order(&PatchOp::Append), 5);
        assert_eq!(execution_order(&PatchOp::DeepMerge), 6);
        assert_eq!(execution_order(&PatchOp::Override), 7);
    }

    // ─── SubOp ───

    #[test]
    fn test_sub_op_construction() {
        let sub = SubOp {
            op: PatchOp::Prepend,
            value: serde_json::json!([{"name": "test"}]),
        };
        assert_eq!(sub.op.display_name(), "Prepend");
        assert_eq!(sub.value.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_sub_op_filter_value_is_null() {
        let sub = SubOp {
            op: PatchOp::Filter {
                expr: CompiledPredicate::new("type == 'ss'", vec!["type".into()]),
            },
            value: serde_json::Value::Null,
        };
        assert!(sub.value.is_null());
        match sub.op {
            PatchOp::Filter { ref expr } => {
                assert_eq!(expr.expr, "type == 'ss'");
                assert_eq!(expr.referenced_fields, vec!["type"]);
            }
            _ => panic!("Expected Filter"),
        }
    }

    // ─── CompiledPredicate ───

    #[test]
    fn test_compiled_predicate_new() {
        let pred = CompiledPredicate::new("name == 'test'", vec!["name".into()]);
        assert_eq!(pred.expr, "name == 'test'");
        assert_eq!(pred.referenced_fields, vec!["name"]);
    }

    #[test]
    fn test_compiled_predicate_constant_true() {
        let pred = CompiledPredicate::constant(true);
        assert_eq!(pred.expr, "true");
        assert!(pred.referenced_fields.is_empty());
    }

    #[test]
    fn test_compiled_predicate_constant_false() {
        let pred = CompiledPredicate::constant(false);
        assert_eq!(pred.expr, "false");
    }

    // ─── DependencyRef ───

    #[test]
    fn test_dependency_ref_file_name() {
        let dep = DependencyRef::FileName("base.yaml".into());
        match dep {
            DependencyRef::FileName(name) => assert_eq!(name, "base.yaml"),
            DependencyRef::PatchId(_) => panic!("Expected FileName"),
        }
    }

    #[test]
    fn test_dependency_ref_patch_id() {
        let pid = PatchId::from_identifier("test-id");
        let dep = DependencyRef::PatchId(pid.clone());
        match dep {
            DependencyRef::PatchId(id) => assert_eq!(id, pid),
            DependencyRef::FileName(_) => panic!("Expected PatchId"),
        }
    }

    // ─── PatchId validate_format ───

    #[test]
    fn test_validate_format_valid_uuid() {
        // Should not panic or warn
        assert!(PatchId::validate_format("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn test_validate_format_valid_identifier() {
        assert!(PatchId::validate_format("my-simple-patch").is_ok());
    }

    #[test]
    fn test_validate_format_dots_warn() {
        // Contains dots — should return Err with warning message
        let result = PatchId::validate_format("path.with.dots");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains('.'));
    }

    // ─── PatchId hash ───

    #[test]
    fn test_patch_id_hashable() {
        use std::collections::HashSet;
        let id1 = PatchId::from_identifier("same");
        let id2 = PatchId::from_identifier("same");
        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2);
        assert_eq!(set.len(), 1); // Same value → same hash
    }
}
