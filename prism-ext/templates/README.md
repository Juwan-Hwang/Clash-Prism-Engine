# Prism Engine 适配指南

## 快速开始

只需 3 步即可在你的 Mihomo GUI 中接入 Prism Engine。

### 第 1 步：添加依赖

在 `Cargo.toml` 中添加：

```toml
[dependencies]
clash-prism-extension = { path = "path/to/prism-engine/crates/clash-prism-extension" }
```

### 第 2 步：复制模板

将 `prism_host.rs` 复制到你的 `src-tauri/src/` 目录，按 TODO 注释填充你的项目配置读写逻辑。

### 第 3 步：注册

在 `lib.rs` 中：

```rust
mod prism_host;

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            prism_host::init_prism(&app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // ... 你现有的 commands ...
            prism_host::prism_apply,
            prism_host::prism_status,
            prism_host::prism_list_rules,
            prism_host::prism_preview_rules,
            prism_host::prism_is_prism_rule,
            prism_host::prism_toggle_group,
            prism_host::prism_get_trace,
            prism_host::prism_get_stats,
            prism_host::prism_list_profiles,
            prism_host::prism_get_core_info,
            prism_host::prism_validate_config,
            prism_host::prism_insert_rule,
            prism_host::prism_insert_rule_str,
            prism_host::prism_start_watching,
            prism_host::prism_stop_watching,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

## 前端调用

```typescript
import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";

// 执行 Prism 编译
const result = await invoke("prism_apply", { options: {} });

// 监听事件
await listen("prism:event", (event) => {
    console.log(event.payload);
});

// 查询规则
const rules = await invoke("prism_list_rules");
```

## API 列表

| 命令 | 说明 |
|------|------|
| `prism_apply` | 执行 Prism 编译管道 |
| `prism_status` | 获取运行状态 |
| `prism_list_rules` | 列出 Prism 管理的规则组 |
| `prism_preview_rules` | 预览指定 patch 的规则变更 |
| `prism_is_prism_rule` | 判断指定规则是否由 Prism 管理 |
| `prism_toggle_group` | 启用/禁用规则组 |
| `prism_get_trace` | 获取执行追踪 |
| `prism_get_stats` | 获取编译统计 |
| `prism_list_profiles` | 列出所有 profile（可选） |
| `prism_get_core_info` | 获取核心信息（可选） |
| `prism_validate_config` | 验证配置（可选） |
| `prism_insert_rule` | 插入规则（对象格式） |
| `prism_insert_rule_str` | 插入规则（字符串格式） |
| `prism_start_watching` | 启动文件监听 |
| `prism_stop_watching` | 停止文件监听 |

## 热重载示例

`apply_config` 中被注释掉的 `self.reload_core()` 是核心热重载入口。
取消注释并实现 `reload_core()` 方法即可启用配置热重载：

```rust
impl MyHost {
    /// 通过 mihomo RESTful API 触发核心热重载
    fn reload_core(&self) -> Result<(), String> {
        let core_info = self.get_core_info()?;
        let url = format!(
            "http://127.0.0.1:{}/configs?force=true",
            core_info.api_port
        );
        let client = reqwest::blocking::Client::new();
        let resp = client
            .put(&url)
            .header("Authorization", format!("Bearer {}", core_info.api_secret))
            .send()
            .map_err(|e| format!("热重载请求失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("热重载失败: HTTP {}", resp.status()));
        }
        Ok(())
    }
}
```

然后在 `apply_config` 中取消注释 `self.reload_core()?;` 即可。

> **注意**：某些配置变更（如 `external-controller` 端口修改）无法通过热重载生效，
> 需要重启 mihomo 核心。此时 `ApplyStatus.restarted` 应设为 `true`。
