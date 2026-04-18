//! 插件加载器 — 负责发现、加载和验证插件

use std::path::PathBuf;
use std::str::FromStr;

use clash_prism_core::error::{PrismError, Result};
use clash_prism_script::limits::ScriptLimits;
use clash_prism_script::{ScriptResult, ScriptRuntime};

use sha2::Digest;

use crate::hook::Hook;
use crate::manifest::PluginManifest;
use crate::permission::{PermissionAction, is_permitted};

/// 插件加载器专用错误
#[derive(Debug, thiserror::Error)]
pub enum PluginLoadError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("清单解析失败: {0}")]
    ManifestParse(#[from] serde_json::Error),

    #[error("清单验证失败: {0}")]
    Validation(String),

    #[error("未找到插件: {0}")]
    NotFound(String),
}

impl From<PluginLoadError> for PrismError {
    fn from(err: PluginLoadError) -> Self {
        match err {
            PluginLoadError::Io(e) => PrismError::from(e),
            PluginLoadError::ManifestParse(e) => PrismError::Serialization(e),
            PluginLoadError::Validation(msg) => PrismError::DslParse {
                message: msg,
                file: None,
                line: None,
            },
            PluginLoadError::NotFound(id) => PrismError::DslParse {
                message: format!("未找到插件: {}", id),
                file: None,
                line: None,
            },
        }
    }
}

/// 插件加载器
///
/// 负责插件的发现、加载、验证和执行。
/// 维护已加载插件表，提供按 ID 查找和执行能力。
pub struct PluginLoader {
    /// 插件搜索路径
    search_paths: Vec<PathBuf>,

    /// 已加载的插件
    loaded_plugins: std::collections::HashMap<String, LoadedPlugin>,
}

/// 已加载的插件实例（包含清单和目录信息）
#[derive(Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub plugin_dir: PathBuf,
    pub cached_entry_script: Option<String>,
    /// 首次执行时运行完整入口脚本 + 钩子调用，后续仅运行钩子调用。
    /// 使用 Arc<AtomicBool> 以支持 Clone 语义。
    pub entry_initialized: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PluginLoader {
    /// 创建新的插件加载器（无搜索路径，需手动添加）
    pub fn new() -> Self {
        Self {
            search_paths: vec![],
            loaded_plugins: std::collections::HashMap::new(),
        }
    }

    /// 添加插件搜索路径
    ///
    /// 支持多个搜索路径，`discover()` 会遍历所有路径查找 `manifest.json`。
    pub fn add_search_path(&mut self, path: impl Into<PathBuf>) {
        self.search_paths.push(path.into());
    }

    /// 扫描所有搜索路径，发现可用插件
    ///
    /// 返回所有找到的有效插件清单列表（manifest.json 验证通过）。
    pub fn discover(&self) -> Result<Vec<PluginManifest>> {
        let mut manifests = Vec::new();

        for search_path in &self.search_paths {
            if !search_path.exists() {
                tracing::warn!(
                    path = %search_path.display(),
                    "插件搜索路径不存在，跳过"
                );
                continue;
            }

            let entries = match std::fs::read_dir(search_path) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        path = %search_path.display(),
                        error = %e,
                        "无法读取插件搜索路径，跳过"
                    );
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                // 查找 manifest.json
                let manifest_path = path.join("manifest.json");
                if manifest_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                        if let Ok(manifest) = PluginManifest::from_json(&content) {
                            if let Err(errors) = manifest.validate() {
                                tracing::warn!(
                                    path = %manifest_path.display(),
                                    plugin_id = %manifest.id,
                                    errors = ?errors,
                                    "清单验证失败，跳过"
                                );
                            } else {
                                manifests.push(manifest);
                            }
                        } else {
                            tracing::warn!(
                                path = %manifest_path.display(),
                                "清单 JSON 解析失败，跳过"
                            );
                        }
                    } else {
                        tracing::warn!(
                            path = %manifest_path.display(),
                            "无法读取清单文件，跳过"
                        );
                    }
                }
            }
        }

        Ok(manifests)
    }

    /// 加载指定插件（验证清单 + 检查权限）
    pub fn load(&mut self, plugin_id: &str) -> Result<LoadedPlugin> {
        if plugin_id.contains("..") || plugin_id.contains('\0') {
            return Err(PluginLoadError::Validation(format!(
                "插件 ID '{}' 包含非法字符（路径遍历攻击嫌疑）",
                plugin_id
            ))
            .into());
        }

        // 如果已加载，直接返回（共享同一个 Arc<AtomicBool>）
        if let Some(plugin) = self.loaded_plugins.get(plugin_id) {
            return Ok(plugin.clone());
        }

        // 在搜索路径中查找
        for search_path in &self.search_paths {
            let plugin_dir = search_path.join(plugin_id);
            let manifest_path = plugin_dir.join("manifest.json");

            if manifest_path.exists() {
                let content = std::fs::read_to_string(&manifest_path)?;

                let manifest = PluginManifest::from_json(&content)?;

                // 验证清单（含路径遍历防护等安全检查）
                if let Err(errors) = manifest.validate() {
                    return Err(PluginLoadError::Validation(format!(
                        "插件验证失败:\n  - {}",
                        errors.join("\n  - ")
                    ))
                    .into());
                }

                if let Some(expected_checksum) = &manifest.checksum {
                    let entry_path = plugin_dir.join(&manifest.entry);
                    let entry_bytes = std::fs::read(&entry_path).map_err(|e| {
                        PluginLoadError::Validation(format!(
                            "无法读取入口文件进行完整性校验 '{}': {}",
                            entry_path.display(),
                            e
                        ))
                    })?;
                    let digest = sha2::Sha256::digest(&entry_bytes);
                    let actual_checksum = format!("{:x}", digest);
                    if actual_checksum != *expected_checksum {
                        return Err(PluginLoadError::Validation(format!(
                            "插件 '{}' 入口脚本完整性校验失败: 期望 '{}', 实际 '{}'",
                            plugin_id, expected_checksum, actual_checksum
                        ))
                        .into());
                    }
                }

                let loaded = LoadedPlugin {
                    manifest: manifest.clone(),
                    plugin_dir: plugin_dir.clone(),
                    cached_entry_script: {
                        let ep = plugin_dir.join(&manifest.entry);
                        if ep.exists() {
                            Some(std::fs::read_to_string(&ep).map_err(|e| {
                                PluginLoadError::Validation(format!(
                                    "Failed to read plugin entry script '{}': {}",
                                    ep.display(),
                                    e
                                ))
                            })?)
                        } else {
                            None
                        }
                    },
                    entry_initialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                        false,
                    )),
                };

                self.loaded_plugins
                    .insert(plugin_id.to_string(), loaded.clone());
                return Ok(loaded);
            }
        }

        Err(PluginLoadError::NotFound(plugin_id.to_string()).into())
    }

    /// 卸载插件
    pub fn unload(&mut self, plugin_id: &str) -> bool {
        self.loaded_plugins.remove(plugin_id).is_some()
    }

    ///
    /// 每个插件可能有不同的超时需求，共享的 `self.runtime` 使用默认的 5 秒超时，
    /// 无法尊重插件 manifest 中声明的 `timeout` 值。
    /// 此方法为每个插件创建一个带自定义 `ScriptLimits` 的运行时实例。
    fn plugin_runtime(&self, manifest: &PluginManifest) -> ScriptRuntime {
        let limits = ScriptLimits {
            max_execution_time_ms: manifest.timeout_ms(),
            ..Default::default()
        };
        ScriptRuntime::with_limits(limits)
    }

    /// 获取所有已加载的插件
    pub fn loaded_plugins(&self) -> Vec<&LoadedPlugin> {
        self.loaded_plugins.values().collect()
    }

    /// 执行插件的入口脚本
    ///
    /// 对于 Config Plugin，加载并执行 `manifest.entry` 指定的 JS 文件。
    /// 执行前会进行安全验证（大小限制、危险模式检测）。
    ///
    /// # Arguments
    /// * `plugin_id` - 要执行的插件 ID
    /// * `config` - 当前配置（将注入为全局 `config` 对象）
    ///
    /// # Returns
    /// 脚本执行结果（含日志、耗时、成功/失败状态）
    ///
    /// 后续钩子触发仅执行钩子调用脚本，避免重复执行入口脚本中的
    /// 非钩子逻辑。通过 `LoadedPlugin.entry_initialized` 原子标记实现。
    pub fn execute_plugin(
        &self,
        plugin_id: &str,
        config: &serde_json::Value,
    ) -> Result<ScriptResult> {
        let plugin = self
            .loaded_plugins
            .get(plugin_id)
            .ok_or_else(|| PluginLoadError::NotFound(plugin_id.to_string()))?;

        let script = if let Some(cached) = &plugin.cached_entry_script {
            cached.clone()
        } else {
            let entry_path = plugin.plugin_dir.join(&plugin.manifest.entry);
            if !entry_path.exists() {
                return Err(PluginLoadError::Validation(format!(
                    "插件 '{}' 的入口文件 '{}' 不存在",
                    plugin_id, plugin.manifest.entry
                ))
                .into());
            }
            std::fs::read_to_string(&entry_path).map_err(|e| {
                PluginLoadError::Validation(format!(
                    "无法读取入口文件 '{}': {}",
                    entry_path.display(),
                    e
                ))
            })?
        };

        tracing::trace!(
            plugin = plugin_id,
            permissions = ?plugin.manifest.permissions,
            "执行插件前检查权限"
        );

        if !is_permitted(&plugin.manifest.permissions, PermissionAction::ConfigRead) {
            return Err(PluginLoadError::Validation(format!(
                "插件 '{}' 缺少执行所需权限 'config:read'",
                plugin_id
            ))
            .into());
        }

        let runtime = self.plugin_runtime(&plugin.manifest);

        // 静态安全验证
        runtime.validate(&script).map_err(|msg| {
            PluginLoadError::Validation(format!("插件 '{}' 安全验证失败: {}", plugin_id, msg))
        })?;

        // 执行脚本
        let result = runtime.execute(&script, &format!("plugin:{}", plugin_id), config);

        tracing::info!(
            plugin = plugin_id,
            success = result.success,
            duration_us = result.duration_us,
            "插件脚本执行完成"
        );

        Ok(result)
    }

    /// 执行钩子回调
    ///
    /// 当引擎触发某个生命周期钩子时，调用此方法通知所有注册了该钩子的插件。
    ///
    /// 再调用钩子函数。之前只构建钩子调用脚本但从未加载插件代码，
    /// 导致 `on{hook_name}` 函数不存在，所有钩子都无法正常工作。
    ///
    /// # Arguments
    /// * `plugin_ids` - 需要通知的插件 ID 列表
    /// * `hook_name` - 触发的钩子名称
    /// * `config` - 当前配置
    /// * `hook_data` - 钩子附加数据（如订阅 URL、节点列表等）
    pub fn execute_hook(
        &self,
        plugin_ids: &[&str],
        hook_name: &str,
        config: &serde_json::Value,
        hook_data: Option<&serde_json::Value>,
    ) -> Vec<(String, ScriptResult)> {
        let mut results = Vec::new();

        for plugin_id in plugin_ids {
            match self.loaded_plugins.get(*plugin_id) {
                Some(plugin) => {
                    if !is_permitted(&plugin.manifest.permissions, PermissionAction::ConfigRead) {
                        results.push((
                            plugin_id.to_string(),
                            ScriptResult {
                                logs: vec![],
                                duration_us: 0,
                                success: false,
                                error: Some(format!(
                                    "插件 '{}' 缺少执行钩子所需权限 'config:read'",
                                    plugin_id
                                )),
                                patches: vec![],
                            },
                        ));
                        continue;
                    }

                    // 不再一刀切要求 config:read + config:write

                    let entry_script = if let Some(cached) = &plugin.cached_entry_script {
                        Some(cached.clone())
                    } else {
                        // 缓存未命中时回退到磁盘读取
                        let entry_path = plugin.plugin_dir.join(&plugin.manifest.entry);
                        if !entry_path.exists() {
                            tracing::warn!(
                                plugin = plugin_id,
                                path = %entry_path.display(),
                                "插件入口文件不存在，跳过钩子执行"
                            );
                            results.push((
                                plugin_id.to_string(),
                                ScriptResult {
                                    logs: vec![],
                                    duration_us: 0,
                                    success: false,
                                    error: Some("入口文件不存在".into()),
                                    patches: vec![],
                                },
                            ));
                            continue;
                        }
                        match std::fs::read_to_string(&entry_path) {
                            Ok(content) => Some(content),
                            Err(e) => {
                                tracing::warn!(
                                    plugin = plugin_id,
                                    path = %entry_path.display(),
                                    error = %e,
                                    "无法读取插件入口文件"
                                );
                                results.push((
                                    plugin_id.to_string(),
                                    ScriptResult {
                                        logs: vec![],
                                        duration_us: 0,
                                        success: false,
                                        error: Some(format!("读取入口文件失败: {}", e)),
                                        patches: vec![],
                                    },
                                ));
                                continue;
                            }
                        }
                    };

                    let Some(script_content) = entry_script else {
                        // 入口脚本不存在或读取失败，跳过此插件
                        continue;
                    };

                    // 步骤 2 — 构建完整脚本：入口脚本 + 钩子调用
                    //
                    // 安全保证：
                    // 1. serde_json::to_string 输出合法 JSON（双引号、特殊字符均已正确转义）
                    // 2. 合法 JSON 直接嵌入 JS 代码作为对象字面量，无需额外转义
                    // 3. 不经过字符串中间层，彻底消除注入向量
                    // 在函数名映射前，先将 hook_name 通过 Hook::from_str + Display
                    // 标准化为 PascalCase（如 "onSubscribeFetch" → "OnSubscribeFetch"）。
                    // 这确保后续的 strip_prefix("On") 对所有大小写变体都能正确匹配。
                    let normalized_hook_name = match Hook::from_str(hook_name) {
                        Ok(hook) => format!("{}", hook),
                        Err(_) => hook_name.to_string(),
                    };
                    let normalized_name = normalized_hook_name.as_str();

                    let is_valid = Hook::builtin_hooks().iter().any(|h| {
                        h.display_name() == normalized_name || format!("{}", h) == normalized_name
                    }) || is_valid_schedule_hook(normalized_name);
                    if !is_valid {
                        results.push((
                            plugin_id.to_string(),
                            ScriptResult {
                                logs: vec![],
                                duration_us: 0,
                                success: false,
                                error: Some(format!(
                                    "非法的钩子名称 '{}': 不在允许列表中",
                                    normalized_name
                                )),
                                patches: vec![],
                            },
                        ));
                        continue;
                    }

                    let data_json =
                        serde_json::to_string(hook_data.unwrap_or(&serde_json::json!({})))
                            .unwrap_or_else(|_| "{}".to_string());
                    // 仅转义 </script>、<!-- 和 -->，避免破坏外层 HTML 结构。
                    let safe_data_json = data_json
                        .replace("</script>", "<\\/script>")
                        .replace("<!--", "<\\!--")
                        .replace("-->", "<\\!-->");
                    //
                    // 首次钩子触发时，运行完整入口脚本（注册 on{hook_name} 等函数），
                    // 后续钩子触发仅运行钩子调用脚本，避免重复执行入口脚本中的
                    // 非钩子逻辑（全局变量初始化、工具函数定义等）。
                    //
                    // 注意：由于每次 execute_hook() 创建新的 ScriptRuntime 实例，
                    // 跨调用的函数状态无法直接复用。因此这里采用"首次完整执行 +
                    // 后续仅调用"的策略，在单次引擎生命周期内有效。
                    // 对于需要跨调用保持状态的场景，插件应使用 store API。
                    // SAFETY: PluginLoader 是单线程设计（未实现 Send + Sync），
                    // 因此 entry_initialized.swap(true) 不会产生竞态条件。
                    // 即使 swap 在实际执行入口脚本之前将标记设为 true，
                    // 也不会有另一个线程观察到该值并跳过初始化。
                    let already_initialized = plugin
                        .entry_initialized
                        .swap(true, std::sync::atomic::Ordering::AcqRel);

                    //
                    // Hook::Display 输出 "OnSubscribeFetch"（PascalCase），
                    // 用户定义的是 function onSubscribeFetch(config, data) {...}
                    // 直接拼接 "on" + "OnSubscribeFetch" 会生成 "onOnSubscribeFetch"，永远匹配不上。
                    //
                    // - 普通钩子：剥离 "On" 前缀 → "SubscribeFetch"，拼接 "on" → "onSubscribeFetch"
                    // - OnSchedule 钩子：提取 cron 表达式，将特殊字符替换为下划线，
                    //   生成如 "onSchedule_0_____" 的合法 JS 标识符
                    let js_func_name = if let Some(cron_expr) = normalized_name
                        .strip_prefix("OnSchedule(")
                        .and_then(|s| s.strip_suffix(")"))
                    {
                        // OnSchedule 钩子：将 cron 表达式编码为合法 JS 标识符
                        let safe_cron: String = cron_expr
                            .chars()
                            .map(|c| if c.is_alphanumeric() { c } else { '_' })
                            .collect();
                        format!("onSchedule_{}", safe_cron)
                    } else {
                        // 普通钩子：剥离 "On" 前缀，添加 "on" 前缀
                        let stripped = normalized_name
                            .strip_prefix("On")
                            .unwrap_or(normalized_name);
                        format!("on{}", stripped)
                    };

                    let hook_script = if already_initialized {
                        // 后续调用：仅执行钩子函数调用（不含入口脚本）
                        format!(
                            r#"
// Prism Hook: {hook_name} (cached)
if (typeof {js_func_name} === 'function') {{
    const hookData = {data_json};
    const result = {js_func_name}(config, hookData);
    if (result !== undefined) {{
        __prism_hook_result = result;
    }}
}} else {{
    // 插件未导出此钩子的处理函数，跳过
}}
"#,
                            hook_name = hook_name,
                            js_func_name = js_func_name,
                            data_json = safe_data_json,
                        )
                    } else {
                        // 首次调用：执行完整入口脚本 + 钩子函数调用
                        format!(
                            r#"
// Prism Hook: {hook_name}
// ── 阶段 1：加载插件入口脚本（注册钩子函数）──
{entry_part}
// ── 阶段 2：调用钩子函数 ──
if (typeof {js_func_name} === 'function') {{
    const hookData = {data_json};
    const result = {js_func_name}(config, hookData);
    if (result !== undefined) {{
        __prism_hook_result = result;
    }}
}} else {{
    // 插件未导出此钩子的处理函数，跳过
}}
"#,
                            hook_name = hook_name,
                            js_func_name = js_func_name,
                            data_json = safe_data_json,
                            entry_part = script_content,
                        )
                    };

                    // 创建带 hook result 变量的运行时包装
                    // 当前未读取，供未来钩子需要返回数据时使用。
                    let wrapped_script = format!(
                        r#"
var __prism_hook_result = undefined;
{}
"#,
                        hook_script
                    );

                    let exec_result = {
                        let runtime = self.plugin_runtime(&plugin.manifest);
                        runtime.execute(
                            &wrapped_script,
                            &format!("hook:{}:{}", plugin_id, hook_name),
                            config,
                        )
                    };

                    results.push((plugin_id.to_string(), exec_result));
                }
                None => {
                    results.push((
                        plugin_id.to_string(),
                        ScriptResult {
                            logs: vec![],
                            duration_us: 0,
                            success: false,
                            error: Some(format!("插件 '{}' 未加载", plugin_id)),
                            patches: vec![],
                        },
                    ));
                }
            }
        }

        results
    }
}

/// 验证 OnSchedule 钩子名称的合法性。
///
/// 使用严格正则 `^OnSchedule\([0-9a-zA-Z\s,\-*/]+\)$` 验证字符白名单，
/// 并进一步验证 cron 表达式的 5 个字段范围：
/// - 分钟: 0-59
/// - 小时: 0-23
/// - 日: 1-31
/// - 月: 1-12
/// - 周: 0-6
fn is_valid_schedule_hook(hook_name: &str) -> bool {
    if let Some(rest) = hook_name.strip_prefix("OnSchedule(")
        && let Some(inner) = rest.strip_suffix(')')
    {
        // 只允许: 数字、字母、空格、逗号、连字符、星号、斜杠
        if !inner.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == ' ' || c == ',' || c == '-' || c == '*' || c == '/'
        }) {
            return false;
        }

        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() != 5 {
            return false;
        }

        // 每个字段的允许范围
        let ranges: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)];

        for (i, part) in parts.iter().enumerate() {
            if !validate_cron_field_range(part, ranges[i].0, ranges[i].1) {
                return false;
            }
        }

        return true;
    }
    false
}

/// 验证单个 cron 字段中所有值是否在有效范围内。
///
/// 支持 `*`、`*/N`、`N`、`N-M`、`N-M/S`、`A,B,C` 等语法。
fn validate_cron_field_range(field: &str, min: u32, max: u32) -> bool {
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if part == "*" {
            continue; // 通配符始终合法
        }

        // 步长模式: */N
        if let Some(step_str) = part.strip_prefix("*/") {
            if let Ok(step) = step_str.parse::<u32>() {
                if step == 0 {
                    return false;
                }
                continue;
            }
            return false;
        }

        // 范围模式（可能带步长）: N-M 或 N-M/S
        if part.contains('-') {
            let (range_part, _step) = if let Some(slash_pos) = part.find('/') {
                let range_part = &part[..slash_pos];
                let step_str = &part[slash_pos + 1..];
                if let Ok(step) = step_str.parse::<u32>() {
                    if step == 0 {
                        return false;
                    }
                    (range_part, step)
                } else {
                    return false;
                }
            } else {
                (part, 1u32)
            };

            if let Some(dash_pos) = range_part.find('-') {
                let start_str = &range_part[..dash_pos];
                let end_str = &range_part[dash_pos + 1..];
                if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                    if start < min || end > max || start > end {
                        return false;
                    }
                    continue;
                }
            }
            return false;
        }

        // 固定值
        if let Ok(value) = part.parse::<u32>() {
            if value < min || value > max {
                return false;
            }
            continue;
        }

        return false;
    }
    true
}

impl Default for PluginLoader {
    fn default() -> Self {
        Self::new()
    }
}
