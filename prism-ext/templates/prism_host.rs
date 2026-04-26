//! 通用 Prism Extension 适配模板
//!
//! 将此文件复制到你的 GUI 项目中，按注释提示填充 TODO 即可接入 Prism Engine。
//! 适用于所有 Tauri 2 + Rust 的 Mihomo GUI 客户端。

use clash_prism_extension::{
    ApplyStatus, CoreInfo, PrismEvent, PrismExtension, PrismHost, ProfileInfo,
};
use std::path::PathBuf;

// ═══════════════════════════════════════════════════════════
// 第一步：实现 PrismHost trait
// 只需实现 4 个必须方法，其余 8 个可选方法有默认实现
// ═══════════════════════════════════════════════════════════

pub struct MyHost {
    pub app: tauri::AppHandle,
}

impl PrismHost for MyHost {
    /// 读取当前运行中的配置（YAML 字符串）
    fn read_running_config(&self) -> Result<String, String> {
        // TODO: 替换为你项目的配置读取方式
        // 例如: Config::runtime().latest().0.config
        let config_path = self.app.path().app_data_dir()
            .map_err(|e| format!("{}", e))?
            .join("config.yaml");
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取配置失败: {}", e))
    }

    /// 将处理后的配置写回并触发热重载
    fn apply_config(&self, config: &str) -> Result<ApplyStatus, String> {
        // TODO: 替换为你项目的配置写入和重载方式
        // 1. 原子写入配置文件（使用 OpenOptions + sync_all 确保数据落盘）
        let config_path = self.app.path().app_data_dir()
            .map_err(|e| format!("{}", e))?
            .join("config.yaml");
        let tmp = config_path.with_extension(format!("yaml.tmp.{}", uuid::Uuid::new_v4()));
        {
            // 使用 OpenOptions 创建文件，写入后调用 sync_all() 再 rename，
            // 确保数据在 rename 之前已持久化到磁盘，防止操作系统崩溃导致数据丢失
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| format!("创建临时文件失败: {}", e))?;
            file.write_all(config.as_bytes())
                .map_err(|e| format!("写入临时文件失败: {}", e))?;
            file.sync_all()
                .map_err(|e| format!("sync_all 失败: {}", e))?;
        }
        if let Err(e) = std::fs::rename(&tmp, &config_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("重命名失败: {}", e));
        }

        // 2. 触发热重载（PATCH /configs?force=true 或你项目的方式）
        //
        // ⚠️ TODO: 取消下面的注释以启用热重载！
        // Prism Engine 已完成配置编译和写入，但需要通知内核重新加载配置。
        // 根据你的 GUI 客户端实现，选择以下方式之一：
        //
        // 方式 A — Mihomo RESTful API（推荐）:
        //   let reload_url = "http://127.0.0.1:9090/configs?force=true";
        //   let client = reqwest::blocking::Client::new();
        //   client.patch(reload_url).send().ok();
        //
        // 方式 B — 内核进程信号:
        //   // 发送 SIGHUP 或 SIGUSR1 给 mihomo 进程
        //   let _ = std::process::Command::new("kill")
        //       .args(["-HUP", &pid.to_string()])
        //       .status();
        //
        // 方式 C — Tauri 事件通知前端触发重载:
        //   let _ = self.app.emit("core:reload-config", ());
        //
        // self.reload_core()?;

        // 热重载：取消下方注释并实现 reload_core() 方法即可启用
        Ok(ApplyStatus {
            files_saved: true,
            hot_reload_success: false,
            message: "配置已写入，但热重载未实现（请取消上方注释并选择重载方式）".into(),
            restarted: false,
        })
    }

    /// 获取 Prism 工作目录（存放 .prism.yaml 文件的地方）
    fn get_prism_workspace(&self) -> Result<PathBuf, String> {
        let dir = self.app.path().app_data_dir()
            .map_err(|e| format!("{}", e))?
            .join("prism");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {}", e))?;
        Ok(dir)
    }

    /// 向前端发送事件通知
    fn notify(&self, event: PrismEvent) {
        if let Some(window) = self.app.get_webview_window("main") {
            let _ = window.emit("prism:event", event);
        }
    }

    // ── 以下为可选方法，有默认实现，按需覆盖 ──

    // fn read_raw_profile(&self, profile_id: &str) -> Result<String, String> { ... }
    // fn list_profiles(&self) -> Result<Vec<ProfileInfo>, String> { ... }
    // fn get_core_info(&self) -> Result<CoreInfo, String> { ... }
    // fn validate_config(&self, config: &str) -> Result<bool, String> { ... }
    // fn script_count(&self) -> Result<usize, String> { ... }
    // fn plugin_count(&self) -> Result<usize, String> { ... }
    // fn get_current_profile(&self) -> Option<String> { ... }  // __when__.profile 条件匹配
    // fn get_variables(&self) -> std::collections::HashMap<String, String> { ... }  // {{var}} 模板替换
}

// ═══════════════════════════════════════════════════════════
// 第二步：初始化 Extension（在 Tauri Builder::setup 中调用）
// ═══════════════════════════════════════════════════════════

pub static PRISM: std::sync::LazyLock<std::sync::Mutex<Option<PrismExtension<MyHost>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

pub fn init_prism(app: &tauri::AppHandle) {
    let ext = PrismExtension::new(MyHost { app: app.clone() });
    let mut guard = PRISM.lock().unwrap_or_else(|e| {
        tracing::error!("PRISM Mutex poisoned, recovering poisoned data");
        e.into_inner()
    });
    *guard = Some(ext);
}

// ═══════════════════════════════════════════════════════════
// 第三步：注册 Tauri Commands（添加到 invoke_handler）
// ═══════════════════════════════════════════════════════════

macro_rules! with_prism {
    ($ext:expr, $body:expr) => {
        match $ext.lock().unwrap_or_else(|e| {
            tracing::error!("PRISM Mutex poisoned in with_prism!, recovering poisoned data");
            e.into_inner()
        }).as_ref() {
            Some(ext) => $body,
            None => Err("Prism Extension 未初始化，请先调用 init_prism()".into()),
        }
    };
}

fn to_json<T: serde::Serialize>(val: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(val).map_err(|e| format!("{}", e))
}

#[tauri::command]
pub fn prism_apply(options: serde_json::Value) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, {
        let opts = clash_prism_extension::ApplyOptions {
            skip_disabled_patches: options.get("skip_disabled_patches").and_then(|v| v.as_bool()).unwrap_or(true),
            validate_output: options.get("validate_output").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        to_json(ext.apply(opts)?)
    })
}

#[tauri::command]
pub fn prism_status() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.status()) })
}

#[tauri::command]
pub fn prism_list_rules() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.list_rules()?) })
}

#[tauri::command]
pub fn prism_preview_rules(patch_id: String) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.preview_rules(&patch_id)?) })
}

#[tauri::command]
pub fn prism_is_prism_rule(index: usize) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.is_prism_rule(index)?) })
}

#[tauri::command]
pub fn prism_toggle_group(group_id: String, enabled: bool) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.toggle_group(&group_id, enabled)?) })
}

#[tauri::command]
pub fn prism_get_trace(patch_id: String) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.get_trace(&patch_id)?) })
}

#[tauri::command]
pub fn prism_get_stats() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.get_stats()?) })
}

#[tauri::command]
pub fn prism_list_profiles() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.list_profiles()?) })
}

#[tauri::command]
pub fn prism_get_core_info() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.get_core_info()?) })
}

#[tauri::command]
pub fn prism_validate_config(config: String) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, { to_json(ext.validate_config(&config)?) })
}

#[tauri::command]
pub fn prism_insert_rule(rule: serde_json::Value, position: String) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, {
        let pos = match position.as_str() {
            "before_prism" => clash_prism_extension::RuleInsertPosition::BeforePrism,
            "after_prism" => clash_prism_extension::RuleInsertPosition::AfterPrism,
            "append" => clash_prism_extension::RuleInsertPosition::Append,
            s if s.starts_with("after_group:") => {
                // strip_prefix 后检查空字符串，防止无意义的组名
                let group_name = s.strip_prefix("after_group:").unwrap_or("");
                if group_name.is_empty() {
                    return Err(format!("after_group: 后的组名不能为空"));
                }
                clash_prism_extension::RuleInsertPosition::AfterGroup(group_name.to_string())
            }
            _ => clash_prism_extension::RuleInsertPosition::Append,
        };
        to_json(ext.insert_rule(rule, &pos)?)
    })
}

#[tauri::command]
pub fn prism_insert_rule_str(rule_text: String, position: String) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, {
        let pos = match position.as_str() {
            "before_prism" => clash_prism_extension::RuleInsertPosition::BeforePrism,
            "after_prism" => clash_prism_extension::RuleInsertPosition::AfterPrism,
            "append" => clash_prism_extension::RuleInsertPosition::Append,
            s if s.starts_with("after_group:") => {
                // strip_prefix 后检查空字符串，防止无意义的组名
                let group_name = s.strip_prefix("after_group:").unwrap_or("");
                if group_name.is_empty() {
                    return Err(format!("after_group: 后的组名不能为空"));
                }
                clash_prism_extension::RuleInsertPosition::AfterGroup(group_name.to_string())
            }
            _ => clash_prism_extension::RuleInsertPosition::Append,
        };
        to_json(ext.insert_rule_str(&rule_text, &pos)?)
    })
}

#[tauri::command]
pub fn prism_start_watching(debounce_ms: Option<u64>) -> Result<serde_json::Value, String> {
    with_prism!(PRISM, {
        ext.start_watching(debounce_ms.unwrap_or(500))?;
        to_json(serde_json::json!({ "success": true }))
    })
}

#[tauri::command]
pub fn prism_stop_watching() -> Result<serde_json::Value, String> {
    with_prism!(PRISM, {
        ext.stop_watching();
        to_json(serde_json::json!({ "success": true }))
    })
}
