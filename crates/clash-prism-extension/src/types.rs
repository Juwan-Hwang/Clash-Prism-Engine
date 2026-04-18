//! # 支持类型 — Extension API 的数据结构定义
//!
//! 包含 Layer 3 JSON API 所需的所有请求/响应类型。
//!
//! ## 类型分类
//!
//! | 类别 | 类型 | 说明 |
//! |------|------|------|
//! | 编译控制 | [`ApplyOptions`] | 编译选项 |
//! | 编译结果 | [`ApplyResult`] | 编译结果（含输出配置、统计、追踪） |
//! | 编译统计 | [`CompileStats`] | Patch 执行统计 |
//! | 执行追踪 | [`TraceView`], [`TraceSummaryView`], [`TraceDiffView`] | 面向前端的追踪视图 |
//! | 规则注解 | [`RuleAnnotation`], [`RuleGroup`], [`RuleEntry`] | 规则归属标记 |
//! | 规则变更 | [`RuleDiff`], [`PositionChange`] | 规则差异比较 |
//! | 规则插入 | [`RuleInsertPosition`] | 用户规则插入位置策略 |
//! | 运行状态 | [`PrismStatus`] | Extension 运行状态 |

/// Prism 编译选项
///
/// 控制 Prism 编译管道的行为，包括是否跳过禁用的 Patch、是否验证输出等。
/// 传递给 [`PrismExtension::apply`](crate::PrismExtension::apply) 方法。
///
/// # 默认值
///
/// - `skip_disabled_patches`: `true`
/// - `validate_output`: `false`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyOptions {
    /// 是否跳过禁用的 Patch
    ///
    /// 禁用的 Patch 包括：
    /// - 文件名以 `.disabled` 结尾的文件
    /// - 包含 `__when__.enabled = false` 条件的 Patch
    pub skip_disabled_patches: bool,

    /// 是否在写回前验证输出配置
    ///
    /// 启用后会调用 [`PrismHost::validate_config`] 进行 `mihomo -t` 验证。
    /// 如果验证失败，配置不会被写回。
    pub validate_output: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            skip_disabled_patches: true,
            validate_output: false,
        }
    }
}

/// Prism 编译结果
///
/// 包含一次完整编译管道执行后的所有输出信息：
/// 处理后的配置、编译统计、执行追踪和规则注解。
/// 由 [`PrismExtension::apply`](crate::PrismExtension::apply) 返回。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyResult {
    /// 处理后的完整配置（YAML 字符串）
    ///
    /// 这是经过所有 Patch 处理后的最终配置，可直接写入 mihomo。
    pub output_config: String,

    /// 编译统计信息
    ///
    /// 包含 Patch 执行数量、变更统计和耗时信息。
    pub stats: CompileStats,

    /// 执行追踪列表
    ///
    /// 每个 Patch 的执行记录，包含操作名、耗时、条件和变更详情。
    pub trace: Vec<TraceView>,

    /// 规则注解列表
    ///
    /// 标记最终配置中哪些规则由 Prism 管理，供 GUI 规则编辑器使用。
    pub rule_annotations: Vec<RuleAnnotation>,
}

/// 编译统计
///
/// 汇总一次编译过程中所有 Patch 的执行情况。
/// 包含总数、成功/跳过数量、变更统计和耗时信息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompileStats {
    /// 总 Patch 数（包括成功和跳过的）
    pub total_patches: usize,

    /// 成功执行的 Patch 数（条件匹配且执行完成）
    pub succeeded: usize,

    /// 跳过的 Patch 数（条件不匹配或已禁用）
    pub skipped: usize,

    /// 新增的元素总数（跨所有 Patch）
    pub total_added: usize,

    /// 删除的元素总数（跨所有 Patch）
    pub total_removed: usize,

    /// 修改的元素总数（跨所有 Patch）
    pub total_modified: usize,

    /// 总执行耗时（微秒）
    pub total_duration_us: u64,

    /// 平均每个 Patch 的执行耗时（微秒）
    pub avg_duration_us: u64,
}

/// 执行追踪视图（面向前端的精简版）
///
/// 将 [`clash_prism_core::trace::ExecutionTrace`] 转换为前端友好的格式，
/// 隐藏内部实现细节，只保留展示所需的信息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceView {
    /// Patch 标识（UUID）
    pub patch_id: String,

    /// 来源文件路径（如 "ad-filter.prism.yaml"）
    pub source_file: Option<String>,

    /// 执行的操作名（如 "prepend"、"deep_merge"、"filter"）
    pub op_name: String,

    /// 执行耗时（微秒）
    pub duration_us: u64,

    /// 条件是否匹配（`false` 表示 Patch 被跳过）
    pub condition_matched: bool,

    /// 操作摘要统计
    pub summary: TraceSummaryView,

    /// Diff 视图（新增/删除的元素描述列表）
    pub diff: TraceDiffView,
}

/// 追踪摘要视图
///
/// 单个 Patch 操作的变更统计，是 [`TraceView`] 的子结构。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceSummaryView {
    /// 新增的元素数量
    pub added: usize,

    /// 删除的元素数量
    pub removed: usize,

    /// 修改的元素数量
    pub modified: usize,

    /// 未变更的元素数量
    pub kept: usize,

    /// 操作前的元素总数
    pub total_before: usize,

    /// 操作后的元素总数
    pub total_after: usize,
}

/// Diff 视图
///
/// 描述单个 Patch 操作中新增和删除的元素。
/// 是 [`TraceView`] 的子结构，用于前端展示变更详情。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceDiffView {
    /// 新增的元素描述列表
    ///
    /// 每个元素是一条描述字符串（如规则文本、配置键路径等）。
    pub added: Vec<String>,

    /// 删除的元素描述列表
    ///
    /// 每个元素是一条描述字符串。
    pub removed: Vec<String>,
}

/// 规则注解 — 标记哪些规则由 Prism 管理
///
/// 在 Patch 执行过程中，为 `$prepend` / `$append` 注入的规则生成注解。
/// GUI 规则编辑器可根据注解判断"这条规则是谁管的"，
/// 从而决定是否允许用户编辑（`immutable = true` 时应拒绝编辑）。
///
/// 由 [`crate::annotation::extract_rule_annotations`] 函数生成。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleAnnotation {
    /// 规则文本（如 `"DOMAIN-SUFFIX,ad.com,REJECT"`）
    pub rule_text: String,

    /// 在最终配置 `rules` 数组中的位置索引（从 0 开始）
    pub index_in_output: usize,

    /// 来源 Patch 文件名（如 `"ad-filter.prism.yaml"`）
    pub source_file: String,

    /// 来源 Patch ID（UUID 或自定义标识符）
    pub source_patch: String,

    /// 来源标签（如 `"广告过滤"`，从文件名推导而来）
    pub source_label: String,

    /// 是否不可编辑
    ///
    /// `true` 时 GUI 编辑器应拒绝用户修改此规则。
    /// 目前始终为 `false`，预留给未来使用。
    pub immutable: bool,
}

/// 规则组 — 一组由同一个 Patch 管理的规则
///
/// 将属于同一个来源文件的规则归为一组，供 GUI 展示和管理。
/// 用户可通过 [`PrismExtension::toggle_group`](crate::PrismExtension::toggle_group)
/// 启用或禁用整个规则组。
///
/// 由 [`crate::annotation::group_annotations`] 函数生成。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleGroup {
    /// 分组标识（通常与文件名相同，如 `"ad-filter.prism.yaml"`）
    pub group_id: String,

    /// 显示标签（如 `"广告过滤"`，用于 GUI 展示）
    pub label: String,

    /// 来源 Patch 文件名（如 `"ad-filter.prism.yaml"`）
    pub patch_id: String,

    /// 是否启用
    ///
    /// `false` 表示该组对应的 `.prism.yaml` 文件已被重命名为 `.disabled`。
    pub enabled: bool,

    /// 是否不可编辑
    ///
    /// `true` 时 GUI 应禁止编辑该组下的所有规则。
    pub immutable: bool,

    /// 该组管理的规则列表
    pub rules: Vec<RuleEntry>,
}

/// 规则条目
///
/// 描述规则组中的单条规则，包含规则原始文本和在最终配置中的位置。
/// 是 [`RuleGroup`] 的子结构。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleEntry {
    /// 规则原始文本（如 `"DOMAIN-SUFFIX,ad.com,REJECT"`）
    pub raw: String,

    /// 在最终配置 `rules` 数组中的位置索引（从 0 开始）
    pub index: usize,
}

/// 规则变更差异
///
/// 描述两个配置版本之间规则的变更情况。
/// 由 [`PrismExtension::preview_rules`](crate::PrismExtension::preview_rules) 返回。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleDiff {
    /// 新增的规则列表
    pub added: Vec<String>,

    /// 删除的规则列表
    pub removed: Vec<String>,

    /// 修改的规则列表（修改后的文本）
    pub modified: Vec<String>,

    /// 位置变更列表（规则在数组中移动了位置）
    pub position_changes: Vec<PositionChange>,
}

/// 位置变更
///
/// 描述一条规则在配置数组中的位置移动。
/// 是 [`RuleDiff`] 的子结构。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PositionChange {
    /// 变更前的位置索引
    pub from: usize,

    /// 变更后的位置索引
    pub to: usize,
}

/// 规则插入位置
///
/// 定义用户自定义规则相对于 Prism 管理规则的插入位置策略。
/// 用于 GUI 规则编辑器中"用户规则应该放在哪里"的决策。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RuleInsertPosition {
    /// 用户规则在所有 Prism 规则之前
    BeforePrism,

    /// 在指定 Prism 分组之后插入
    ///
    /// 参数为分组标识（如 `"ad-filter.prism.yaml"`）。
    AfterGroup(String),

    /// 用户规则在所有 Prism 规则之后
    AfterPrism,

    /// 追加到 rules 数组末尾
    Append,
}

/// Extension 运行状态
///
/// 描述 Prism Extension 的当前运行状态，包括文件监听状态、
/// 上次编译时间和结果、已注册的 Patch/脚本/插件数量。
/// 由 [`PrismExtension::status`](crate::PrismExtension::status) 返回。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrismStatus {
    /// 是否正在监听文件变更
    pub watching: bool,

    /// 监听的文件/目录数量
    pub watching_count: usize,

    /// 上次编译时间（ISO 8601 / RFC 3339 格式）
    ///
    /// `None` 表示尚未执行过编译。
    pub last_compile_time: Option<String>,

    /// 上次编译是否成功
    pub last_compile_success: bool,

    /// 已注册的 Patch 文件数量（去重后）
    pub patch_count: usize,

    /// 已注册的脚本数量
    pub script_count: usize,

    /// 已注册的插件数量
    pub plugin_count: usize,
}
