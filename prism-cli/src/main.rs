//! # Prism CLI — Prism Engine 命令行工具
//!
//! 用法：
//!
//! ```bash
//! # 执行 Prism 编译管道
//! prism-cli apply --config ./config.yaml --prism-dir ./prism
//!
//! # 查看引擎状态
//! prism-cli status --prism-dir ./prism
//!
//! # 启动 HTTP Server
//! prism-cli serve --port 9097 --config ./config.yaml --prism-dir ./prism
//!
//! # 解析 DSL 文件
//! prism-cli parse config.prism.yaml
//!
//! # 验证 DSL 语法
//! prism-cli check config.prism.yaml
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod ndjson;
mod pid_lock;

/// Prism Engine CLI — 配置变换引擎命令行工具
#[derive(Parser, Debug)]
#[command(name = "prism-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// 执行 Prism 编译管道（读取配置 → 编译 → 写回）
    Apply {
        /// 运行中的配置文件路径（YAML）
        #[arg(long, default_value = "./config.yaml")]
        config: PathBuf,
        /// Prism 工作目录（存放 .prism.yaml 文件）
        #[arg(long, default_value = "./prism")]
        prism_dir: PathBuf,
        /// 跳过已禁用的 patch
        #[arg(long, default_value_t = true)]
        skip_disabled: bool,
        /// 编译后验证配置
        #[arg(long, default_value_t = false)]
        validate: bool,
        /// 输出格式：human（默认）、ndjson、json
        #[arg(long, default_value = "human")]
        output: String,
        /// 输出完整执行追踪报告（逐条变更详情）
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },

    /// 查看 Prism Engine 运行状态
    Status {
        /// Prism 工作目录
        #[arg(long, default_value = "./prism")]
        prism_dir: PathBuf,
    },

    /// 启动 HTTP Server，暴露 REST API
    Serve {
        /// 监听端口
        #[arg(long, default_value_t = 9097)]
        port: u16,
        /// 监听地址
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// 配置文件路径
        #[arg(long, default_value = "./config.yaml")]
        config: PathBuf,
        /// Prism 工作目录
        #[arg(long, default_value = "./prism")]
        prism_dir: PathBuf,
        /// 文件监听防抖时间（毫秒）
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
        /// API 认证密钥（Bearer Token）。未指定时尝试从 PRISM_API_KEY 环境变量读取；均未指定时不启用认证（适用于本地开发）
        #[arg(long)]
        api_key: Option<String>,
        /// 禁用文件监听（不自动启动 watcher）
        #[arg(long, default_value_t = false)]
        no_watch: bool,
        /// 允许的 CORS 来源列表（逗号分隔）。仅非 localhost 绑定时生效。留空表示允许所有来源
        #[arg(long)]
        allowed_origins: Option<String>,
        /// 强制获取 PID 锁（覆盖已运行的实例）
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// 解析 .prism.yaml DSL 文件并输出 Patch IR
    Parse {
        /// DSL 文件路径
        file: PathBuf,
    },

    /// 验证 DSL 语法（不执行）
    Check {
        /// DSL 文件路径
        file: PathBuf,
    },

    /// 执行 JS 脚本
    Run {
        /// 脚本文件路径
        script: PathBuf,
        /// 配置文件路径（可选）
        config: Option<PathBuf>,
    },

    /// 插件管理
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },

    /// 监听文件变化并自动编译（热重载）
    Watch {
        /// 监听目录
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// 输出配置文件路径
        #[arg(default_value = "./config.yaml")]
        output: PathBuf,
        /// 热重载方式：signal <pid> / http <url>
        #[arg(long, conflicts_with = "reload_http")]
        reload_signal: Option<u32>,
        #[arg(long, conflicts_with = "reload_signal")]
        reload_http: Option<String>,
        /// 文件变化防抖时间（毫秒）
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },
}

#[derive(clap::Subcommand, Debug)]
enum PluginAction {
    /// 加载插件目录
    Load { dir: PathBuf },
    /// 列出已加载的插件
    List,
    /// 执行插件
    Execute {
        id: String,
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CliHost — PrismHost 的文件系统实现
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// CLI 模式下的 Host 实现，直接操作文件系统
struct CliHost {
    config_path: PathBuf,
    prism_dir: PathBuf,
}

impl clash_prism_extension::PrismHost for CliHost {
    fn read_running_config(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("读取配置失败 [{}]: {}", self.config_path.display(), e))
    }

    fn apply_config(&self, config: &str) -> Result<clash_prism_extension::ApplyStatus, String> {
        use std::io::Write;
        // 原子写入：先写临时文件（UUID 后缀避免冲突），sync_all 确保落盘，再 rename
        let tmp = self
            .config_path
            .with_extension(format!("yaml.tmp.{}", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("创建临时文件失败: {}", e))?;
        file.write_all(config.as_bytes())
            .map_err(|e| format!("写入临时文件失败: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("sync 临时文件失败: {}", e))?;
        drop(file);
        if let Err(e) = std::fs::rename(&tmp, &self.config_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("重命名失败: {}", e));
        }
        Ok(clash_prism_extension::ApplyStatus {
            files_saved: true,
            hot_reload_success: true,
            message: "配置已写入".into(),
            restarted: false,
        })
    }

    fn get_prism_workspace(&self) -> Result<PathBuf, String> {
        let dir = &self.prism_dir;
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 Prism 目录失败: {}", e))?;
        Ok(dir.clone())
    }

    fn notify(&self, event: clash_prism_extension::PrismEvent) {
        match &event {
            clash_prism_extension::PrismEvent::PatchApplied { patch_id, stats } => {
                println!(
                    "  ✓ Patch [{}] applied: +{} -{} ~{} ({}μs)",
                    patch_id, stats.added, stats.removed, stats.modified, stats.duration_us
                );
            }
            clash_prism_extension::PrismEvent::PatchFailed { patch_id, error } => {
                eprintln!("  ✗ Patch [{}] failed: {}", patch_id, error);
            }
            clash_prism_extension::PrismEvent::ConfigReloaded { success, message } => {
                if *success {
                    println!("  ✓ Config reloaded: {}", message);
                } else {
                    eprintln!("  ✗ Config reload failed: {}", message);
                }
            }
            clash_prism_extension::PrismEvent::RulesChanged {
                added,
                removed,
                modified,
            } => {
                println!("  → Rules changed: +{} -{} ~{}", added, removed, modified);
            }
            clash_prism_extension::PrismEvent::WatcherEvent { file, change_type } => {
                println!("  ⟳ File changed [{}]: {}", change_type, file);
            }
            clash_prism_extension::PrismEvent::WatcherStatus {
                running,
                watching_count,
            } => {
                if *running {
                    println!("  ◉ Watcher started ({} dir)", watching_count);
                } else {
                    println!("  ◉ Watcher stopped");
                }
            }
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 全局已加载插件注册表
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 全局插件注册表，记录已加载的插件信息
static LOADED_PLUGINS: std::sync::LazyLock<std::sync::Mutex<Vec<PluginEntry>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// 已加载插件条目
struct PluginEntry {
    id: String,
    name: String,
    version: String,
    path: String,
}

fn register_plugin(id: String, name: String, version: String, path: String) {
    let mut plugins = LOADED_PLUGINS.lock().unwrap_or_else(|e| e.into_inner());
    // 避免重复注册
    if !plugins.iter().any(|p| p.id == id) {
        plugins.push(PluginEntry {
            id,
            name,
            version,
            path,
        });
    }
}

fn list_plugins() -> Vec<(String, String, String, String)> {
    let plugins = LOADED_PLUGINS.lock().unwrap_or_else(|e| e.into_inner());
    plugins
        .iter()
        .map(|p| {
            (
                p.id.clone(),
                p.name.clone(),
                p.version.clone(),
                p.path.clone(),
            )
        })
        .collect()
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 子命令实现
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn cmd_apply(
    config: PathBuf,
    prism_dir: PathBuf,
    skip_disabled: bool,
    validate: bool,
    output_format: &str,
    verbose: bool,
) -> Result<(), String> {
    let fmt: ndjson::OutputFormat = output_format.parse().map_err(|e: String| e)?;

    let host = CliHost {
        config_path: config,
        prism_dir,
    };
    let ext = clash_prism_extension::PrismExtension::new(host);

    let opts = clash_prism_extension::ApplyOptions {
        skip_disabled_patches: skip_disabled,
        validate_output: validate,
    };

    let result = ext.apply(opts)?;

    match fmt {
        ndjson::OutputFormat::Human => {
            if verbose {
                let report = ext
                    .trace_report()
                    .unwrap_or_else(|e| format!("追踪报告生成失败: {}", e));
                println!("{}", report);
            } else {
                println!("📊 编译统计:");
                println!("  总 Patch 数: {}", result.stats.total_patches);
                println!("  成功: {}", result.stats.succeeded);
                println!("  跳过: {}", result.stats.skipped);
                println!("  新增规则: {}", result.stats.total_added);
                println!("  删除规则: {}", result.stats.total_removed);
                println!("  修改规则: {}", result.stats.total_modified);
                println!(
                    "  总耗时: {}μs (平均 {}μs/patch)",
                    result.stats.total_duration_us, result.stats.avg_duration_us
                );

                if !result.rule_annotations.is_empty() {
                    println!(
                        "\n🏷️  规则注解 ({} 条 Prism 管理的规则):",
                        result.rule_annotations.len()
                    );
                }

                println!("\nPowered by Prism Engine");
            }
        }
        ndjson::OutputFormat::Ndjson => {
            let mut writer = ndjson::NdjsonWriter::new(std::io::stdout());
            let result_value =
                serde_json::to_value(&result).map_err(|e| format!("序列化结果失败: {}", e))?;
            writer
                .write_event("apply_result", &result_value)
                .map_err(|e| format!("写入 NDJSON 输出失败: {}", e))?;
        }
        ndjson::OutputFormat::Json => {
            let json_output = serde_json::to_string_pretty(&result)
                .map_err(|e| format!("序列化结果失败: {}", e))?;
            println!("{}", json_output);
        }
    }

    Ok(())
}

fn cmd_status(prism_dir: PathBuf) -> Result<(), String> {
    // 注意：config_path 此处仅用于构造 CliHost 结构体，status() 方法不读取配置文件
    let host = CliHost {
        config_path: PathBuf::from("./config.yaml"),
        prism_dir,
    };
    let ext = clash_prism_extension::PrismExtension::new(host);
    let status = ext.status();

    println!("📊 Prism Engine Status\n");
    println!("  监听中: {}", if status.watching { "是" } else { "否" });
    println!(
        "  上次编译: {}",
        status.last_compile_time.as_deref().unwrap_or("从未")
    );
    println!(
        "  编译成功: {}",
        if status.last_compile_success {
            "是"
        } else {
            "否"
        }
    );
    println!("  Patch 文件数: {}", status.patch_count);

    Ok(())
}

fn cmd_parse(file: PathBuf) -> Result<(), String> {
    println!("📂 解析文件: {}\n", file.display());

    let content = std::fs::read_to_string(&file).map_err(|e| format!("读取文件失败: {}", e))?;

    let patches = clash_prism_dsl::DslParser::parse_str(&content, Some(file.clone()))
        .map_err(|e| format!("DSL 解析失败: {}", e))?;

    println!("✅ 成功解析 {} 个 Patch:\n", patches.len());
    for (i, patch) in patches.iter().enumerate() {
        println!(
            "  {}. {:20} → {:10} [scope: {}]",
            i + 1,
            patch.path,
            patch.op.display_name(),
            patch.scope,
        );
    }

    if !patches.is_empty() {
        println!("\n🔧 尝试执行（基于空配置）...");
        let mut executor = clash_prism_core::executor::PatchExecutor::new();
        match executor.execute(serde_json::json!({}), &patches) {
            Ok(cfg) => {
                println!(
                    "\n📄 输出:\n{}",
                    serde_json::to_string_pretty(&cfg).unwrap_or_default()
                );
                if !executor.traces.is_empty() {
                    println!("\n🔍 追踪:");
                    for (i, t) in executor.traces.iter().enumerate() {
                        println!(
                            "  [{}] {} — {}μs",
                            i + 1,
                            t.describe_change(),
                            t.duration_us
                        );
                    }
                }
            }
            Err(e) => println!("\n⚠️  执行失败: {}", e),
        }
    }

    println!("\nPowered by Prism Engine");

    Ok(())
}

fn cmd_check(file: PathBuf) -> Result<(), String> {
    println!("🔍 验证: {}\n", file.display());

    let content = std::fs::read_to_string(&file).map_err(|e| format!("读取失败: {}", e))?;

    match clash_prism_dsl::DslParser::parse_str(&content, Some(file.clone())) {
        Ok(patches) => {
            println!("✅ 语法正确，{} 个 Patch", patches.len());
            for (i, p) in patches.iter().enumerate() {
                println!("  [{}] {} → {}", i + 1, p.path, p.op.display_name());
            }
            Ok(())
        }
        Err(e) => Err(format!("❌ 语法错误: {}", e)),
    }
}

fn cmd_run(script: PathBuf, config_file: Option<PathBuf>) -> Result<(), String> {
    println!("▶️  执行脚本: {}\n", script.display());

    let script_content =
        std::fs::read_to_string(&script).map_err(|e| format!("读取脚本失败: {}", e))?;

    let config = match config_file {
        Some(p) => {
            let s = std::fs::read_to_string(&p).map_err(|e| format!("读取配置失败: {}", e))?;
            if p.extension().is_some_and(|e| e == "json") {
                serde_json::from_str(&s).map_err(|e| format!("JSON 解析失败: {}", e))?
            } else {
                serde_yml::from_str(&s).map_err(|e| format!("YAML 解析失败: {}", e))?
            }
        }
        None => serde_json::json!({}),
    };

    let rt = clash_prism_script::ScriptRuntime::new();
    rt.validate(&script_content)?;
    let result = rt.execute(&script_content, script.to_string_lossy().as_ref(), &config);

    println!("  状态: {}", if result.success { "✅" } else { "❌" });
    println!("  耗时: {}μs", result.duration_us);
    if let Some(e) = &result.error {
        println!("  错误: {}", e);
    }
    for log in &result.logs {
        println!("  [{}] {}", log.level, log.message);
    }

    println!("\nPowered by Prism Engine");

    Ok(())
}

fn cmd_plugin(action: PluginAction) -> Result<(), String> {
    match action {
        PluginAction::Load { dir } => {
            let manifest_path = dir.join("manifest.json");
            let content = std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("读取 manifest.json 失败: {}", e))?;
            let manifest = clash_prism_plugin::PluginManifest::from_json(&content)
                .map_err(|e| format!("解析失败: {}", e))?;
            if let Err(errs) = manifest.validate() {
                return Err(format!("验证失败:\n  - {}", errs.join("\n  - ")));
            }
            register_plugin(
                manifest.id.clone(),
                manifest.name.clone(),
                manifest.version.clone(),
                dir.display().to_string(),
            );
            println!(
                "✅ 插件: {} v{} ({})",
                manifest.name, manifest.version, manifest.id
            );
            Ok(())
        }
        PluginAction::List => {
            let plugins = list_plugins();
            if plugins.is_empty() {
                println!("📦 已加载插件: (空)");
            } else {
                println!("📦 已加载插件 ({} 个):", plugins.len());
                for (id, name, version, path) in &plugins {
                    println!("  - {} v{} [{}] ({})", name, version, id, path);
                }
            }
            Ok(())
        }
        PluginAction::Execute { id, dir } => {
            let mut loader = clash_prism_plugin::PluginLoader::new();
            loader.add_search_path(&dir);
            let _ = loader.load(&id).map_err(|e| e.to_string())?;
            let config = serde_json::json!({"proxy":{"groups":[]},"rules":[]});
            let result = loader
                .execute_plugin(&id, &config)
                .map_err(|e| e.to_string())?;
            println!("  状态: {}", if result.success { "✅" } else { "❌" });
            println!("  耗时: {}μs", result.duration_us);
            Ok(())
        }
    }
}

fn cmd_watch(
    dir: PathBuf,
    output: PathBuf,
    reload_signal: Option<u32>,
    reload_http: Option<String>,
    debounce_ms: u64,
) -> Result<(), String> {
    use clash_prism_core::target::HotReloadStrategy;
    use clash_prism_core::watcher::{FileWatcher, WatchConfig};

    // 防抖时间上限校验（60 秒）
    let debounce_ms = if debounce_ms > 60_000 {
        eprintln!(
            "⚠️  --debounce-ms {} 超过上限 60000，已自动调整为 60000",
            debounce_ms
        );
        60_000
    } else {
        debounce_ms
    };

    let hot_reload = match (reload_signal, reload_http) {
        (Some(pid), _) => HotReloadStrategy::Signal { pid },
        (_, Some(url)) => HotReloadStrategy::HttpApi { url },
        _ => HotReloadStrategy::None,
    };

    let config = WatchConfig {
        watch_dir: dir.clone(),
        output_path: output.clone(),
        hot_reload,
        debounce_ms,
        compile_on_start: true,
        skip_dirs: vec!["node_modules".to_string(), "target".to_string()],
    };

    println!("👀 Watch Mode");
    println!("   目录: {}", dir.display());
    println!("   输出: {}", output.display());
    println!("   按 Ctrl+C 退出\n");

    let watcher = FileWatcher::new(config)?;
    watcher.run(|content, path| {
        clash_prism_dsl::DslParser::parse_str(content, path).map_err(|e| e.to_string())
    })?;

    Ok(())
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// HTTP Server
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

mod server;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
/// 恒定时间字符串比较，防止时序攻击
///
/// 对两个完整字节序列进行恒定时间比较，不截断、不设上限。
/// 长度不同的两个密钥始终返回 false（通过将差异编码到结果中，
/// 避免通过长度信息泄露时序侧信道）。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();

    // 长度不同时，将差异编码到比较结果中。
    let len_diff = if a_bytes.len() == b_bytes.len() {
        0u8
    } else {
        // 非零值，确保长度不同时结果一定为 false
        1u8
    };

    // 固定迭代长度为 max(a.len(), b.len())，对超出较短长度的部分使用 0xFF 填充。
    // 这确保无论输入长度如何，迭代次数相同，消除基于长度的时序侧信道。
    let max_len = a_bytes.len().max(b_bytes.len());
    let byte_diff: u8 = (0..max_len).fold(0u8, |acc, i| {
        let a_byte = a_bytes.get(i).copied().unwrap_or(0xFF);
        let b_byte = b_bytes.get(i).copied().unwrap_or(0xFF);
        std::hint::black_box(acc | (a_byte ^ b_byte))
    });

    // 长度不同或内容不同均返回 false
    byte_diff == 0 && len_diff == 0
}

// Main
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clash_prism_cli=info,clash_prism_core=info".into()),
        )
        .init();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    init_logging();

    let cli = Cli::parse();

    let result = match cli.command {
        Command::Apply {
            config,
            prism_dir,
            skip_disabled,
            validate,
            output,
            verbose,
        } => cmd_apply(config, prism_dir, skip_disabled, validate, &output, verbose),
        Command::Status { prism_dir } => cmd_status(prism_dir).map_err(|e| e.to_string()),
        Command::Serve {
            port,
            bind,
            config,
            prism_dir,
            debounce_ms,
            mut api_key,
            no_watch,
            allowed_origins,
            force,
        } => {
            // 命令行未指定时，尝试从环境变量 PRISM_API_KEY 读取
            if api_key.is_none() {
                api_key = std::env::var("PRISM_API_KEY").ok();
            }
            let origins = allowed_origins.as_ref().map(|s| {
                s.split(',')
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .collect()
            });

            server::run(server::ServeConfig {
                bind,
                port,
                config,
                prism_dir,
                debounce_ms,
                api_key,
                no_watch,
                allowed_origins: origins,
                force,
            })
            .await
        }
        Command::Parse { file } => cmd_parse(file),
        Command::Check { file } => cmd_check(file),
        Command::Run { script, config } => cmd_run(script, config),
        Command::Plugin { action } => cmd_plugin(action),
        Command::Watch {
            dir,
            output,
            reload_signal,
            reload_http,
            debounce_ms,
        } => cmd_watch(dir, output, reload_signal, reload_http, debounce_ms),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("❌ {}", e);
            ExitCode::FAILURE
        }
    }
}
