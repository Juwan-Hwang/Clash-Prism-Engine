//! # Patch Compiler — 将各种输入编译为 Patch IR
//!
//! ## Responsibilities
//!
//! - **Prism DSL → Patch**: 解析 `.prism.yaml` 文件，提取 `__when__` / `__after__`
//! - **静态字段 AST 校验**: 拒绝 `$filter` / `$transform` 中的运行时字段 (§2.6)
//! - **依赖解析**: 基于 `__after__` 声明进行拓扑排序
//! - **确定性排序**: 同级 Patch 按文件名字典序排列
//!
//! ## Pipeline
//!
//! ```text
//! .prism.yaml files
//!       ↓
//!       ▼
//!  DslParser::parse_file()  →  Vec<Patch>
//!       ↓
//!       ▼
//!  PatchCompiler::register_patches()  →  内部存储
//!       ↓
//!       ▼
//!  PatchCompiler::resolve_dependencies()  →  Vec<PatchId> (已排序)
//! ```

use std::collections::{BTreeMap, HashMap};

use crate::error::{CompileError, PrismError, Result};
use crate::ir::{DependencyRef, Patch, PatchId};
use crate::scope::{Platform, Scope, ScopedBuilder};

/// Patch 编译器 — 管理 Patch 注册和依赖解析。
pub struct PatchCompiler {
    /// Registered Patches grouped by source file name
    patches: HashMap<String, Vec<Patch>>,

    /// 文件名 → PatchId 映射（用于依赖解析）
    /// 通过 `get_patch_ids_for_file()` 方法访问，不直接暴露为 pub。
    file_to_ids: HashMap<String, Vec<PatchId>>,
}

impl PatchCompiler {
    /// Create a new empty compiler.
    pub fn new() -> Self {
        Self {
            patches: HashMap::new(),
            file_to_ids: HashMap::new(),
        }
    }

    /// Register Patches from a source file.
    /// The compiler stores them grouped by file name for later dependency resolution.
    pub fn register_patches(
        &mut self,
        file_name: impl Into<String>,
        patches: Vec<Patch>,
    ) -> Result<()> {
        let file_name = file_name.into();
        let ids: Vec<PatchId> = patches.iter().map(|p| p.id.clone()).collect();
        self.file_to_ids.insert(file_name.clone(), ids);
        self.patches.insert(file_name, patches);
        Ok(())
    }

    /// Get the PatchIds registered from a given file name.
    ///
    /// Provides a query method for `file_to_ids` instead of
    /// exposing the HashMap directly as `pub`.
    ///
    /// Also supports short-name matching (without `.prism.yaml` /
    /// `.prism.yml` suffix), consistent with `resolve_dependencies()`.
    pub fn get_patch_ids_for_file(&self, file_name: &str) -> Option<&[PatchId]> {
        // Exact match first
        if let Some(ids) = self.file_to_ids.get(file_name) {
            return Some(ids.as_slice());
        }
        // Short-name fallback: strip known suffixes
        let short = file_name
            .strip_suffix(".prism.yaml")
            .or_else(|| file_name.strip_suffix(".prism.yml"))
            .unwrap_or(file_name);
        if short != file_name {
            self.file_to_ids.get(short).map(|v| v.as_slice())
        } else {
            None
        }
    }

    /// Resolve dependencies and return topologically-sorted PatchIds.
    ///
    /// Matching rules for `__after__` references:
    /// - Full filename match (e.g., `"00-base-dns.prism.yaml"`)
    /// - Short name without `.prism.yaml` suffix (e.g., `"base-dns"` or `"00-base-dns"`)
    pub fn resolve_dependencies(&self) -> Result<Vec<PatchId>> {
        // 收集所有 patch
        let all_patches: Vec<&Patch> = self.patches.values().flatten().collect();

        // 收集所有文件名 → patch ids 的映射
        let mut name_to_ids: HashMap<String, &Vec<PatchId>> = HashMap::new();
        for (file_name, ids) in &self.file_to_ids {
            // 注册完整文件名
            name_to_ids.insert(file_name.clone(), ids);
            // 注册去掉 .prism.yaml 后缀的名称
            let short_name = file_name
                .strip_suffix(".prism.yaml")
                .or_else(|| file_name.strip_suffix(".prism.yml"))
                .unwrap_or(file_name)
                .to_string();
            if let Some(existing_ids) = name_to_ids.get(&short_name) {
                // 短名称已存在，检查是否为同一文件（完整名 == 短名称的情况）
                if !std::ptr::eq(*existing_ids, ids) {
                    // 短名称冲突：两个不同文件去掉后缀后产生相同短名称
                    // 例如 `foo.prism.yaml` 和 `foo.prism.yml` 的短名称都是 `foo`
                    // 升级为错误：歧义短名称可能导致依赖解析到错误的文件
                    return Err(PrismError::DslParse {
                        message: format!(
                            "短名称 '{}' 存在歧义：文件 '{}' 与已注册的文件共享同一短名称。\
                             依赖引用可能匹配到非预期的文件。请使用完整文件名（如 '{}.prism.yaml'）避免歧义。",
                            short_name, file_name, short_name
                        ),
                        file: None,
                        line: None,
                    });
                }
                // 同一文件（完整名 == 短名称），跳过重复注册
            } else {
                name_to_ids.insert(short_name.clone(), ids);
            }
        }

        // 拓扑排序
        let sorted = self
            .topological_sort(&all_patches, &name_to_ids)
            .map_err(|e| match e {
                CompileError::CircularDependency(cycle) => PrismError::CircularDependency { cycle },
                CompileError::DependencyNotFound(msg) => PrismError::DslParse {
                    message: msg,
                    file: None,
                    line: None,
                },
                other => PrismError::DslParse {
                    message: other.to_string(),
                    file: None,
                    line: None,
                },
            })?;

        Ok(sorted.into_iter().map(|p| p.id.clone()).collect())
    }

    /// Topological sort based on `__after__` dependency declarations.
    ///
    /// Sibling patches with no mutual dependency are ordered by filename lexicographically
    /// (deterministic output). Uses Kahn's algorithm with BTreeMap for O(1) sort key lookup .
    ///
    /// # Errors
    /// Returns [`CompileError::DslParse`] with [`PrismError::CircularDependency`] if a cycle is detected.
    fn topological_sort<'a>(
        &self,
        all_patches: &[&'a Patch],
        name_to_ids: &HashMap<String, &'a Vec<PatchId>>,
    ) -> std::result::Result<Vec<&'a Patch>, CompileError> {
        let mut in_degree: HashMap<PatchId, usize> = HashMap::new();
        let mut adj: HashMap<PatchId, Vec<PatchId>> = HashMap::new();
        let mut id_to_patch: HashMap<PatchId, &Patch> = HashMap::new();
        let mut id_to_sort_key: HashMap<PatchId, String> = HashMap::new();

        // 初始化
        for patch in all_patches {
            in_degree.entry(patch.id.clone()).or_insert(0);
            adj.entry(patch.id.clone()).or_default();
            id_to_patch.insert(patch.id.clone(), *patch);
            id_to_sort_key.insert(
                patch.id.clone(),
                patch
                    .source
                    .file
                    .clone()
                    .unwrap_or_else(|| patch.id.to_string()),
            );
        }

        // 构建图
        for patch in all_patches {
            for dep in &patch.after {
                let dep_ids: Vec<PatchId> = match dep {
                    DependencyRef::FileName(name) => {
                        let ids = name_to_ids.get(name.as_str()).ok_or_else(|| {
                            CompileError::DependencyNotFound(format!(
                                "`{dep}` 在文件 `{file}` 中未找到",
                                dep = name,
                                file = patch
                                    .source
                                    .file
                                    .clone()
                                    .unwrap_or_else(|| "unknown".into())
                            ))
                        })?;
                        (*ids).clone()
                    }
                    DependencyRef::PatchId(pid) => {
                        // linear scan through all_patches.
                        if id_to_patch.contains_key(pid) {
                            vec![pid.clone()]
                        } else {
                            return Err(CompileError::DependencyNotFound(format!(
                                "`{pid}` 在文件 `{file}` 中未找到",
                                pid = pid.as_str(),
                                file = patch
                                    .source
                                    .file
                                    .clone()
                                    .unwrap_or_else(|| "unknown".into())
                            )));
                        }
                    }
                };

                for dep_id in &dep_ids {
                    adj.entry(dep_id.clone())
                        .or_default()
                        .push(patch.id.clone());
                    *in_degree.entry(patch.id.clone()).or_insert(0) += 1;
                }
            }
        }

        // Kahn 算法拓扑排序
        // 同级时无依赖的 Patch 仍然按文件名字典序排列（确定性）
        let mut queue: BTreeMap<(String, PatchId), ()> = BTreeMap::new();
        for (id, &degree) in &in_degree {
            if degree == 0
                && let Some(sort_key) = id_to_sort_key.get(id)
            {
                queue.insert((sort_key.clone(), id.clone()), ());
            }
        }

        let mut result = vec![];
        while let Some(((sort_key, id), ())) = queue.pop_first() {
            let _ = sort_key; // sort_key 仅用于排序，不参与逻辑
            if let Some(&patch) = id_to_patch.get(&id) {
                result.push(patch);
            }

            if let Some(neighbors) = adj.get(&id) {
                for neighbor in neighbors {
                    if let Some(degree) = in_degree.get_mut(neighbor) {
                        *degree -= 1;
                        if *degree == 0
                            && let Some(sort_key) = id_to_sort_key.get(neighbor)
                        {
                            queue.insert((sort_key.clone(), neighbor.clone()), ());
                        }
                    }
                }
            }
        }

        // 检测环 — 回溯依赖边构建完整环路径
        if result.len() != all_patches.len() {
            let result_ids: std::collections::HashSet<&PatchId> =
                result.iter().map(|p| &p.id).collect();

            // 构建反向邻接表: node → nodes it depends on (its "after" dependencies)
            // Only for nodes NOT in result_ids (i.e., nodes in the cycle)
            let mut depends_on: HashMap<&PatchId, Vec<&PatchId>> = HashMap::new();
            for patch in all_patches {
                if !result_ids.contains(&patch.id) {
                    for dep in &patch.after {
                        let dep_ids: Vec<&PatchId> = match dep {
                            DependencyRef::FileName(name) => name_to_ids
                                .get(name.as_str())
                                .map(|ids| {
                                    ids.iter().filter(|id| !result_ids.contains(id)).collect()
                                })
                                .unwrap_or_default(),
                            DependencyRef::PatchId(pid) => {
                                if !result_ids.contains(pid) {
                                    vec![pid]
                                } else {
                                    vec![]
                                }
                            }
                        };
                        depends_on.entry(&patch.id).or_default().extend(dep_ids);
                    }
                }
            }

            // 对所有未处理节点逐一尝试 DFS，直到找到真正的环路径。
            // 仅从单个未处理节点开始 DFS 可能失败——若该节点不在环中
            // （而是依赖环中的节点），DFS 不会回到起点，返回空路径。
            let cycle_nodes: Vec<&PatchId> = all_patches
                .iter()
                .filter(|p| !result_ids.contains(&p.id))
                .map(|p| &p.id)
                .collect();

            let cycle_path = Self::find_cycle_path(&cycle_nodes, &depends_on);
            let cycle_str = if cycle_path.is_empty() {
                // Fallback: 列出所有未处理节点
                cycle_nodes
                    .iter()
                    .map(|id| (*id).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                cycle_path.join(" → ")
            };

            return Err(CompileError::CircularDependency(cycle_str));
        }

        Ok(result)
    }

    /// Find a cycle path by trying iterative DFS from each candidate node.
    ///
    /// 接受所有未处理节点的集合，依次尝试每个节点作为 DFS 起点。
    /// 若某个节点不在环中（而是依赖环中的节点），从该节点出发的 DFS 不会
    /// 回到起点，因此需要逐一尝试直到找到真正的环路径。
    ///
    /// Returns the cycle as a list of node IDs (including the repeated start node
    /// at the end to close the cycle), or an empty vec if no cycle is found.
    ///
    /// Uses an iterative DFS with an explicit stack to avoid stack overflow on
    /// deeply nested dependency graphs from adversarial input.
    fn find_cycle_path(
        candidates: &[&PatchId],
        depends_on: &HashMap<&PatchId, Vec<&PatchId>>,
    ) -> Vec<String> {
        // 逐一尝试每个候选节点作为 DFS 起点，确保找到真正的环
        for start in candidates {
            // path: 当前从 start 出发的 DFS 路径
            let mut path: Vec<&PatchId> = vec![start];
            let mut path_set: std::collections::HashSet<&PatchId> =
                std::collections::HashSet::new();
            path_set.insert(start);

            // 显式栈：每个元素是 (当前节点, 邻居迭代起始索引)
            let mut stack: Vec<(&PatchId, usize)> = vec![(start, 0)];

            'outer: loop {
                let (current, neighbor_idx) = *stack.last().unwrap();

                if let Some(deps) = depends_on.get(current) {
                    let mut found_next = false;
                    for (i, dep) in deps.iter().enumerate().skip(neighbor_idx) {
                        if **dep == *path[0] {
                            // 找到回到起点的环
                            path.push(*dep);
                            break 'outer;
                        }
                        if path_set.contains(dep) {
                            // 跳过已在当前路径上的节点
                            continue;
                        }
                        // 推入栈，继续深入
                        path.push(*dep);
                        path_set.insert(*dep);
                        *stack.last_mut().unwrap() = (current, i + 1);
                        stack.push((*dep, 0));
                        found_next = true;
                        break;
                    }
                    if !found_next {
                        // 所有邻居已遍历完，回溯
                        stack.pop();
                        if let Some(popped) = path.pop() {
                            path_set.remove(popped);
                        }
                    }
                } else {
                    // 当前节点无依赖，回溯
                    stack.pop();
                    if let Some(popped) = path.pop() {
                        path_set.remove(popped);
                    }
                }

                if stack.is_empty() {
                    break;
                }
            }

            if path.len() > 1 && path.last().unwrap() == path.first().unwrap() {
                return path.iter().map(|id| id.to_string()).collect();
            }
        }
        vec![]
    }

    /// Get all registered Patches (unsorted).
    pub fn get_all_patches(&self) -> Vec<&Patch> {
        self.patches.values().flatten().collect()
    }

    /// Get Patches from a specific source file.
    pub fn get_file_patches(&self, file_name: &str) -> Option<&[Patch]> {
        self.patches.get(file_name).map(|v| v.as_slice())
    }
}

impl Default for PatchCompiler {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷方法：编译 + 两阶段管线执行（§4.1 / §9）
///
/// 将已注册的 Patches 按 scope 分类，分为 Profile 级和 Shared 级，
/// 然后调用 [`PatchExecutor::execute_pipeline`] 完成两阶段管线执行。
///
/// # Arguments
/// * `base_config` — 基础配置（将被修改为最终结果）
/// * `executor` — Patch 执行器实例
///
/// # Returns
/// 所有阶段的执行追踪记录
///
/// # Patch 分类规则
/// - **Profile 级**: `Scope::Profile(name)` — 按 profile name 分组，Phase 1 并发执行
/// - **Shared 级**: `Scope::Global` / `Scope::Scoped { .. }` / `Scope::Runtime` — Phase 2 顺序执行
pub fn compile_and_execute_pipeline(
    compiler: &PatchCompiler,
    base_config: &mut serde_json::Value,
    executor: &mut crate::executor::PatchExecutor,
) -> crate::error::Result<Vec<crate::trace::ExecutionTrace>> {
    let all_patches: Vec<&Patch> = compiler.get_all_patches();

    // 按 scope 分类
    let mut profile_groups: std::collections::HashMap<String, Vec<Patch>> =
        std::collections::HashMap::new();
    let mut shared_patches: Vec<Patch> = Vec::new();

    for patch in all_patches {
        match &patch.scope {
            Scope::Profile(name) => {
                profile_groups
                    .entry(name.clone())
                    .or_default()
                    .push(patch.clone());
            }
            _ => {
                shared_patches.push(patch.clone());
            }
        }
    }

    // 转为 Vec<(String, Vec<Patch>)>，按 profile name 排序确保确定性
    let mut profile_groups: Vec<(String, Vec<Patch>)> = profile_groups.into_iter().collect();
    profile_groups.sort_by(|a, b| a.0.cmp(&b.0));

    // Topological sort by __after__ dependencies for deterministic execution order.
    // Both profile_groups and shared_patches need proper ordering.
    let sorted_ids = compiler.resolve_dependencies()?;
    let id_order: std::collections::HashMap<String, usize> = sorted_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str().to_string(), i))
        .collect();

    // Sort patches within each profile group by dependency order
    for (_, patches) in &mut profile_groups {
        patches.sort_by_key(|p| id_order.get(p.id.as_str()).copied().unwrap_or(usize::MAX));
    }

    // Sort shared_patches by dependency order
    shared_patches.sort_by_key(|p| id_order.get(p.id.as_str()).copied().unwrap_or(usize::MAX));

    // Detect cross-group profile dependencies that would be violated
    // by concurrent Phase 1 execution. Profile groups run in parallel via
    // std::thread::scope, so any __after__ dependency between patches in
    // different profile groups is a correctness hazard.
    {
        // Build patch_id → profile_group_name mapping
        let mut patch_group: HashMap<String, &str> = HashMap::new();
        for (group_name, patches) in &profile_groups {
            for p in patches {
                patch_group.insert(p.id.to_string(), group_name.as_str());
            }
        }

        // Check each profile patch's dependencies
        for (group_name, patches) in &profile_groups {
            for p in patches {
                for dep in &p.after {
                    let dep_target = match dep {
                        DependencyRef::PatchId(id) => id.as_str(),
                        DependencyRef::FileName(name) => {
                            // Resolve file-level dependency to patch IDs
                            // via compiler's public query method.
                            // Check ALL IDs in the file, not just the first.
                            let ids = compiler.get_patch_ids_for_file(name).unwrap_or(&[]);

                            if ids.is_empty() {
                                continue;
                            }

                            // Check if ANY of the dependency's patch IDs belongs to a different group
                            let mut cross_dep_group: Option<&str> = None;
                            for dep_id in ids {
                                if let Some(&dep_group) = patch_group.get(dep_id.as_str())
                                    && dep_group != group_name.as_str()
                                {
                                    cross_dep_group = Some(dep_group);
                                    break;
                                }
                            }
                            if let Some(dep_group) = cross_dep_group {
                                return Err(crate::error::PrismError::DslParse {
                                    message: format!(
                                        "跨 Profile 组依赖不允许: Patch '{}' (Profile '{}') 依赖 Profile '{}' 中的 Patch。\
                                         Profile 组在 Phase 1 中并发执行，跨组依赖无法保证顺序。\
                                         请将相关 Patch 移至同一 Profile 组或改用 Shared 级 Scope。",
                                        p.id, group_name, dep_group
                                    ),
                                    file: None,
                                    line: None,
                                });
                            }
                            // 所有 ID 都在同一组内，无需报错
                            continue;
                        }
                    };

                    if let Some(&dep_group) = patch_group.get(dep_target)
                        && dep_group != group_name.as_str()
                    {
                        return Err(crate::error::PrismError::DslParse {
                            message: format!(
                                "跨 Profile 组依赖不允许: Patch '{}' (Profile '{}') 依赖 Profile '{}' 中的 Patch。\
                                     Profile 组在 Phase 1 中并发执行，跨组依赖无法保证顺序。\
                                     请将相关 Patch 移至同一 Profile 组或改用 Shared 级 Scope。",
                                p.id, group_name, dep_group
                            ),
                            file: None,
                            line: None,
                        });
                    }
                }
            }
        }
    }

    // 检测 Profile Patch 依赖 Shared Patch 的跨阶段依赖违规。
    // Profile 在 Phase 1 先于 Phase 2 的 Shared 执行，因此 Profile Patch
    // 通过 __after__ 声明依赖 Shared Patch 是时序上不可能满足的。
    {
        // 构建 shared patch 的 ID 集合，用于快速查找
        let shared_patch_ids: std::collections::HashSet<&str> =
            shared_patches.iter().map(|p| p.id.as_str()).collect();

        for (group_name, patches) in &profile_groups {
            for p in patches {
                for dep in &p.after {
                    let dep_targets: Vec<&str> = match dep {
                        DependencyRef::PatchId(id) => vec![id.as_str()],
                        DependencyRef::FileName(name) => {
                            // 解析文件级依赖到具体 patch ID
                            let ids = compiler.get_patch_ids_for_file(name).unwrap_or(&[]);
                            ids.iter().map(|id| id.as_str()).collect()
                        }
                    };

                    for dep_id in dep_targets {
                        if shared_patch_ids.contains(dep_id) {
                            return Err(crate::error::PrismError::DslParse {
                                message: format!(
                                    "跨阶段依赖不允许: Profile Patch '{}' (Profile '{}') 依赖 Shared Patch '{}'。\
                                     Profile 在 Phase 1 先于 Phase 2 的 Shared 执行，此依赖无法满足。\
                                     请将依赖的 Patch 改为 Profile 级 Scope，或将依赖方移至 Shared 级。",
                                    p.id, group_name, dep_id
                                ),
                                file: None,
                                line: None,
                            });
                        }
                    }
                }
            }
        }
    }

    let (traces, merged_config) =
        executor.execute_pipeline(base_config, profile_groups, shared_patches)?;
    *base_config = merged_config;
    Ok(traces)
}

/// 条件预编译器 — 将 `__when__` 条件编译为可执行谓词。
///
/// Note: Actual condition evaluation happens at execution time;
/// this only does syntax-level validation and preprocessing.
pub struct ConditionPrecompiler;

impl ConditionPrecompiler {
    /// 预编译条件作用域
    ///
    /// 校验 __when__ 中声明的字段是否合法，
    /// 并返回一个结构化的 Scope 表示
    pub fn compile_when(when: &serde_yml::Mapping) -> std::result::Result<Scope, CompileError> {
        let mut builder = ScopedBuilder::new();

        if let Some(core) = when.get(serde_yml::Value::String("core".into()))
            && let Some(core_str) = core.as_str()
        {
            // Semantic validation: core field must be a known kernel name.
            // "clash" and "meta" are accepted as legacy aliases for "mihomo"
            // (the project was formerly known as Clash Meta). They are mapped
            // to "mihomo" so that existing .prism.yaml files continue to work.
            // Only "mihomo" and "clash-rs" have corresponding TargetCore variants.
            let normalized_core = match core_str {
                "clash" | "meta" => {
                    tracing::warn!(
                        core = core_str,
                        "'{core_str}' is a legacy alias for 'mihomo' (formerly Clash Meta). \
                             Please update to 'mihomo' in your .prism.yaml files. \
                             This alias will be normalized to 'mihomo'."
                    );
                    "mihomo"
                }
                _ => core_str,
            };
            if !matches!(normalized_core, "mihomo" | "clash-rs") {
                tracing::warn!(
                    core = core_str,
                    "core 字段值 '{}' 不是已知的内核名称 (mihomo / clash-rs)，将继续但可能不会匹配",
                    core_str
                );
            }
            builder = builder.core(normalized_core);
        }

        if let Some(platform_val) = when.get(serde_yml::Value::String("platform".into())) {
            let platforms = match platform_val {
                serde_yml::Value::String(s) => {
                    vec![parse_platform(s.as_str())?]
                }
                serde_yml::Value::Sequence(seq) => {
                    let mut result = vec![];
                    for item in seq {
                        if let Some(s) = item.as_str() {
                            result.push(parse_platform(s)?);
                        }
                    }
                    result
                }
                _ => {
                    return Err(CompileError::ConditionPrecompile(
                        "platform 字段类型错误，应为字符串或字符串数组".into(),
                    ));
                }
            };
            builder = builder.platform(platforms);
        }

        if let Some(profile) = when.get(serde_yml::Value::String("profile".into()))
            && let Some(profile_str) = profile.as_str()
        {
            builder = builder.profile(profile_str);
        }

        // Parse time condition
        if let Some(time_val) = when.get(serde_yml::Value::String("time".into()))
            && let Some(time_str) = time_val.as_str()
        {
            let time_range = crate::scope::TimeRange::parse(time_str)
                .map_err(CompileError::ConditionPrecompile)?;
            builder = builder.time(time_range);
        }

        // File-level enabled condition
        if let Some(enabled_val) = when.get(serde_yml::Value::String("enabled".into())) {
            if let Some(enabled_bool) = enabled_val.as_bool() {
                builder = builder.enabled(enabled_bool);
            } else {
                tracing::warn!(
                    value = ?enabled_val,
                    "'enabled' field is not a boolean, ignoring (expected true/false)"
                );
            }
        }

        // WiFi SSID condition
        if let Some(ssid_val) = when.get(serde_yml::Value::String("ssid".into()))
            && let Some(ssid_str) = ssid_val.as_str()
        {
            builder = builder.ssid(ssid_str);
        }

        Ok(builder.build())
    }
}

fn parse_platform(s: &str) -> std::result::Result<Platform, CompileError> {
    match s.to_lowercase().as_str() {
        "windows" | "win" => Ok(Platform::Windows),
        "macos" | "mac" => Ok(Platform::MacOS),
        "linux" | "lin" => Ok(Platform::Linux),
        "android" => Ok(Platform::Android),
        "ios" => Ok(Platform::IOS),
        other => Err(CompileError::ConditionPrecompile(format!(
            "Unsupported platform: {}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{DependencyRef, PatchOp};
    use crate::source::PatchSource;

    /// Helper: create a minimal Patch for testing
    fn make_patch(file: &str, path: &str, after: Vec<DependencyRef>) -> Patch {
        let mut patch = Patch::new(
            PatchSource::yaml_file(file.to_string(), None),
            crate::scope::Scope::Global,
            path,
            PatchOp::DeepMerge,
            serde_json::json!({"test": true}),
        );
        patch.after = after;
        patch
    }

    // ─── PatchCompiler 基础构造 ───

    #[test]
    fn test_compiler_new_is_empty() {
        let compiler = PatchCompiler::new();
        assert!(compiler.get_all_patches().is_empty());
        assert!(compiler.get_file_patches("nonexistent").is_none());
    }

    #[test]
    fn test_compiler_default_trait() {
        let compiler = PatchCompiler::default();
        assert!(compiler.get_all_patches().is_empty());
    }

    // ─── register_patches ───

    #[test]
    fn test_register_patches_single_file() {
        let mut compiler = PatchCompiler::new();
        let patches = vec![make_patch("a.yaml", "dns", vec![])];
        compiler.register_patches("a.yaml", patches).unwrap();
        assert_eq!(compiler.get_all_patches().len(), 1);
        assert!(compiler.get_file_patches("a.yaml").is_some());
    }

    #[test]
    fn test_register_patches_multiple_files() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "dns", vec![])])
            .unwrap();
        compiler
            .register_patches("b.yaml", vec![make_patch("b.yaml", "rules", vec![])])
            .unwrap();
        assert_eq!(compiler.get_all_patches().len(), 2);
    }

    #[test]
    fn test_register_patches_overwrites_same_file() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "dns", vec![])])
            .unwrap();
        compiler
            .register_patches(
                "a.yaml",
                vec![
                    make_patch("a.yaml", "dns", vec![]),
                    make_patch("a.yaml", "rules", vec![]),
                ],
            )
            .unwrap();
        // Second registration overwrites the first
        assert_eq!(compiler.get_all_patches().len(), 2);
    }

    // ─── resolve_dependencies 拓扑排序正确性 ───

    #[test]
    fn test_resolve_no_dependencies_sorted_by_filename() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("z-last.yaml", vec![make_patch("z-last.yaml", "z", vec![])])
            .unwrap();
        compiler
            .register_patches(
                "a-first.yaml",
                vec![make_patch("a-first.yaml", "a", vec![])],
            )
            .unwrap();
        compiler
            .register_patches("m-mid.yaml", vec![make_patch("m-mid.yaml", "m", vec![])])
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 3);
        // 验证按文件名字典序排列
        let a_id = &compiler.get_file_patches("a-first.yaml").unwrap()[0].id;
        let m_id = &compiler.get_file_patches("m-mid.yaml").unwrap()[0].id;
        let z_id = &compiler.get_file_patches("z-last.yaml").unwrap()[0].id;
        assert_eq!(&sorted[0], a_id);
        assert_eq!(&sorted[1], m_id);
        assert_eq!(&sorted[2], z_id);
    }

    #[test]
    fn test_resolve_linear_dependency_chain() {
        let mut compiler = PatchCompiler::new();
        // c depends on b, b depends on a → order: a, b, c
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "a", vec![])])
            .unwrap();
        compiler
            .register_patches(
                "b.yaml",
                vec![make_patch(
                    "b.yaml",
                    "b",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "c.yaml",
                vec![make_patch(
                    "c.yaml",
                    "c",
                    vec![DependencyRef::FileName("b.yaml".into())],
                )],
            )
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 3);
        let a_id = compiler.get_file_patches("a.yaml").unwrap()[0].id.clone();
        let b_id = compiler.get_file_patches("b.yaml").unwrap()[0].id.clone();
        let c_id = compiler.get_file_patches("c.yaml").unwrap()[0].id.clone();

        let pos_a = sorted.iter().position(|id| *id == a_id).unwrap();
        let pos_b = sorted.iter().position(|id| *id == b_id).unwrap();
        let pos_c = sorted.iter().position(|id| *id == c_id).unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_resolve_diamond_dependency() {
        // d depends on b and c, b depends on a, c depends on a
        // Valid order: a, b, c, d (or a, c, b, d)
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "a", vec![])])
            .unwrap();
        compiler
            .register_patches(
                "b.yaml",
                vec![make_patch(
                    "b.yaml",
                    "b",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "c.yaml",
                vec![make_patch(
                    "c.yaml",
                    "c",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "d.yaml",
                vec![make_patch(
                    "d.yaml",
                    "d",
                    vec![
                        DependencyRef::FileName("b.yaml".into()),
                        DependencyRef::FileName("c.yaml".into()),
                    ],
                )],
            )
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 4);
        let a_id = compiler.get_file_patches("a.yaml").unwrap()[0].id.clone();
        let d_id = compiler.get_file_patches("d.yaml").unwrap()[0].id.clone();
        let pos_a = sorted.iter().position(|id| *id == a_id).unwrap();
        let pos_d = sorted.iter().position(|id| *id == d_id).unwrap();
        assert!(pos_a < pos_d);
    }

    // ─── 循环依赖检测 ───

    #[test]
    fn test_circular_dependency_two_nodes() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "a.yaml",
                vec![make_patch(
                    "a.yaml",
                    "a",
                    vec![DependencyRef::FileName("b.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "b.yaml",
                vec![make_patch(
                    "b.yaml",
                    "b",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();

        let result = compiler.resolve_dependencies();
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("循环依赖"),
            "错误信息应包含'循环依赖': {}",
            err_msg
        );
    }

    #[test]
    fn test_circular_dependency_three_nodes() {
        let mut compiler = PatchCompiler::new();
        // a → b → c → a
        compiler
            .register_patches(
                "a.yaml",
                vec![make_patch(
                    "a.yaml",
                    "a",
                    vec![DependencyRef::FileName("b.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "b.yaml",
                vec![make_patch(
                    "b.yaml",
                    "b",
                    vec![DependencyRef::FileName("c.yaml".into())],
                )],
            )
            .unwrap();
        compiler
            .register_patches(
                "c.yaml",
                vec![make_patch(
                    "c.yaml",
                    "c",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();

        let result = compiler.resolve_dependencies();
        assert!(result.is_err());
    }

    // ─── 自依赖检测 ───

    #[test]
    fn test_self_dependency() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "a.yaml",
                vec![make_patch(
                    "a.yaml",
                    "a",
                    vec![DependencyRef::FileName("a.yaml".into())],
                )],
            )
            .unwrap();

        let result = compiler.resolve_dependencies();
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("循环依赖"),
            "自依赖应报告为循环依赖: {}",
            err_msg
        );
    }

    // ─── 依赖不存在 ───

    #[test]
    fn test_dependency_not_found() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "a.yaml",
                vec![make_patch(
                    "a.yaml",
                    "a",
                    vec![DependencyRef::FileName("nonexistent.yaml".into())],
                )],
            )
            .unwrap();

        let result = compiler.resolve_dependencies();
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("nonexistent"),
            "错误信息应包含不存在的依赖名: {}",
            err_msg
        );
    }

    // ─── __after__ 短名称（不带 .prism.yaml 后缀）───

    #[test]
    fn test_after_short_name_without_extension() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "00-base-dns.prism.yaml",
                vec![make_patch("00-base-dns.prism.yaml", "dns", vec![])],
            )
            .unwrap();
        compiler
            .register_patches(
                "01-override.prism.yaml",
                vec![make_patch(
                    "01-override.prism.yaml",
                    "dns",
                    vec![
                        DependencyRef::FileName("00-base-dns".into()), // 短名称
                    ],
                )],
            )
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 2);
        let base_id = compiler.get_file_patches("00-base-dns.prism.yaml").unwrap()[0]
            .id
            .clone();
        let override_id = compiler.get_file_patches("01-override.prism.yaml").unwrap()[0]
            .id
            .clone();
        let pos_base = sorted.iter().position(|id| *id == base_id).unwrap();
        let pos_override = sorted.iter().position(|id| *id == override_id).unwrap();
        assert!(pos_base < pos_override);
    }

    #[test]
    fn test_after_short_name_strip_prism_yml() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "base.prism.yml",
                vec![make_patch("base.prism.yml", "dns", vec![])],
            )
            .unwrap();
        compiler
            .register_patches(
                "child.yaml",
                vec![make_patch(
                    "child.yaml",
                    "dns",
                    vec![
                        DependencyRef::FileName("base".into()), // 去掉 .prism.yml 后缀
                    ],
                )],
            )
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 2);
    }

    // ─── __after__ 数组形式 ───

    #[test]
    fn test_after_array_form() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "a", vec![])])
            .unwrap();
        compiler
            .register_patches("b.yaml", vec![make_patch("b.yaml", "b", vec![])])
            .unwrap();
        compiler
            .register_patches(
                "c.yaml",
                vec![make_patch(
                    "c.yaml",
                    "c",
                    vec![
                        DependencyRef::FileName("a.yaml".into()),
                        DependencyRef::FileName("b.yaml".into()),
                    ],
                )],
            )
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        let a_id = compiler.get_file_patches("a.yaml").unwrap()[0].id.clone();
        let b_id = compiler.get_file_patches("b.yaml").unwrap()[0].id.clone();
        let c_id = compiler.get_file_patches("c.yaml").unwrap()[0].id.clone();
        let pos_a = sorted.iter().position(|id| *id == a_id).unwrap();
        let pos_b = sorted.iter().position(|id| *id == b_id).unwrap();
        let pos_c = sorted.iter().position(|id| *id == c_id).unwrap();
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_c);
    }

    // ─── 同优先级按文件名字典序 ───

    #[test]
    fn test_same_priority_sorted_lexicographically() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("zzz.yaml", vec![make_patch("zzz.yaml", "z", vec![])])
            .unwrap();
        compiler
            .register_patches("aaa.yaml", vec![make_patch("aaa.yaml", "a", vec![])])
            .unwrap();
        compiler
            .register_patches("mmm.yaml", vec![make_patch("mmm.yaml", "m", vec![])])
            .unwrap();

        let sorted = compiler.resolve_dependencies().unwrap();
        let aaa_id = &compiler.get_file_patches("aaa.yaml").unwrap()[0].id;
        let mmm_id = &compiler.get_file_patches("mmm.yaml").unwrap()[0].id;
        let zzz_id = &compiler.get_file_patches("zzz.yaml").unwrap()[0].id;
        assert_eq!(&sorted[0], aaa_id);
        assert_eq!(&sorted[1], mmm_id);
        assert_eq!(&sorted[2], zzz_id);
    }

    // ─── ConditionPrecompiler::compile_when ───

    #[test]
    fn test_compile_when_core_field() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("core".into()),
            serde_yml::Value::String("mihomo".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { core, .. } => {
                assert_eq!(core.as_deref(), Some("mihomo"));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_platform_invalid_string() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("platform".into()),
            serde_yml::Value::String("freebsd".into()),
        );
        // "freebsd" is not a valid platform → error
        let result = ConditionPrecompiler::compile_when(&when);
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_when_platform_valid() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("platform".into()),
            serde_yml::Value::String("macos".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { platform, .. } => {
                assert_eq!(platform.as_deref(), Some(&[Platform::MacOS][..]));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_platform_array() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("platform".into()),
            serde_yml::Value::Sequence(vec![
                serde_yml::Value::String("windows".into()),
                serde_yml::Value::String("linux".into()),
            ]),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { platform, .. } => {
                let plats = platform.unwrap();
                assert_eq!(plats.len(), 2);
                assert!(plats.contains(&Platform::Windows));
                assert!(plats.contains(&Platform::Linux));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_profile() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("profile".into()),
            serde_yml::Value::String("work".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { profile, .. } => {
                assert_eq!(profile.as_deref(), Some("work"));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_time_valid() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("time".into()),
            serde_yml::Value::String("09:00-17:00".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { time_range, .. } => {
                let tr = time_range.unwrap();
                assert_eq!(tr.start, (9, 0));
                assert_eq!(tr.end, (17, 0));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_time_invalid_format() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("time".into()),
            serde_yml::Value::String("not-a-time".into()),
        );
        let result = ConditionPrecompiler::compile_when(&when);
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_when_enabled_bool() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("enabled".into()),
            serde_yml::Value::Bool(false),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { enabled, .. } => {
                assert_eq!(enabled, Some(false));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_enabled_string_warns_not_errors() {
        let mut when = serde_yml::Mapping::new();
        // enabled as string "true" — should not error, just warn
        when.insert(
            serde_yml::Value::String("enabled".into()),
            serde_yml::Value::String("true".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when);
        // Should succeed (string enabled is warned, not errored)
        assert!(scope.is_ok());
        match scope.unwrap() {
            crate::scope::Scope::Scoped { enabled, .. } => {
                // enabled should be None since string was not a bool
                assert!(enabled.is_none());
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_ssid() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("ssid".into()),
            serde_yml::Value::String("HomeWiFi".into()),
        );
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped { ssid, .. } => {
                assert_eq!(ssid.as_deref(), Some("HomeWiFi"));
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_empty_mapping() {
        let when = serde_yml::Mapping::new();
        let scope = ConditionPrecompiler::compile_when(&when).unwrap();
        match scope {
            crate::scope::Scope::Scoped {
                profile,
                platform,
                core,
                time_range,
                enabled,
                ssid,
            } => {
                assert!(profile.is_none());
                assert!(platform.is_none());
                assert!(core.is_none());
                assert!(time_range.is_none());
                assert!(enabled.is_none());
                assert!(ssid.is_none());
            }
            _ => panic!("Expected Scoped scope"),
        }
    }

    #[test]
    fn test_compile_when_platform_invalid_value() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("platform".into()),
            serde_yml::Value::String("freebsd".into()),
        );
        let result = ConditionPrecompiler::compile_when(&when);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("Unsupported platform"));
    }

    #[test]
    fn test_compile_when_platform_wrong_type() {
        let mut when = serde_yml::Mapping::new();
        when.insert(
            serde_yml::Value::String("platform".into()),
            serde_yml::Value::Number(42.into()),
        );
        let result = ConditionPrecompiler::compile_when(&when);
        assert!(result.is_err());
    }

    // ─── parse_platform 辅助函数 ───

    #[test]
    fn test_parse_platform_aliases() {
        assert_eq!(parse_platform("windows").unwrap(), Platform::Windows);
        assert_eq!(parse_platform("win").unwrap(), Platform::Windows);
        assert_eq!(parse_platform("macos").unwrap(), Platform::MacOS);
        assert_eq!(parse_platform("mac").unwrap(), Platform::MacOS);
        assert_eq!(parse_platform("linux").unwrap(), Platform::Linux);
        assert_eq!(parse_platform("lin").unwrap(), Platform::Linux);
    }

    #[test]
    fn test_parse_platform_case_insensitive() {
        assert_eq!(parse_platform("Windows").unwrap(), Platform::Windows);
        assert_eq!(parse_platform("MACOS").unwrap(), Platform::MacOS);
        assert_eq!(parse_platform("LiNuX").unwrap(), Platform::Linux);
    }

    // ─── 空编译器 resolve ───

    #[test]
    fn test_resolve_empty_compiler() {
        let compiler = PatchCompiler::new();
        let sorted = compiler.resolve_dependencies().unwrap();
        assert!(sorted.is_empty());
    }

    // ─── 同文件多 patch ───

    #[test]
    fn test_multiple_patches_same_file() {
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "multi.yaml",
                vec![
                    make_patch("multi.yaml", "dns", vec![]),
                    make_patch("multi.yaml", "rules", vec![]),
                ],
            )
            .unwrap();
        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 2);
    }

    // ─── 短名称歧义检测 ───

    #[test]
    fn test_short_name_no_ambiguity_same_file() {
        // 完整文件名 == 短名称（无 .prism.yaml 后缀）不应触发歧义错误
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches("a.yaml", vec![make_patch("a.yaml", "a", vec![])])
            .unwrap();
        compiler
            .register_patches("b.yaml", vec![make_patch("b.yaml", "b", vec![])])
            .unwrap();
        let sorted = compiler.resolve_dependencies().unwrap();
        assert_eq!(sorted.len(), 2);
    }

    #[test]
    fn test_short_name_ambiguity_different_files() {
        // 两个不同文件去掉后缀后产生相同短名称 → 应报错
        // register_patches 本身不检查歧义，歧义在 resolve_dependencies 中检测
        let mut compiler = PatchCompiler::new();
        compiler
            .register_patches(
                "foo.prism.yaml",
                vec![make_patch("foo.prism.yaml", "a", vec![])],
            )
            .unwrap();
        compiler
            .register_patches(
                "foo.prism.yml",
                vec![make_patch("foo.prism.yml", "b", vec![])],
            )
            .unwrap();
        let result = compiler.resolve_dependencies();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("歧义"));
    }
}
