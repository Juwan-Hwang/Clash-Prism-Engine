//! `clash-prism-ext` — Prism Engine 适配模板脚手架
//!
//! 用法：
//!   clash-prism-ext init [--output <dir>]
//!
//! ## 设计决策：手动参数解析
//!
//! 本工具使用手动参数解析（`std::env::args()`）而非 `clap` 等第三方库，
//! 这是刻意的最小化依赖选择：
//! - `clash-prism-ext` 是一次性脚手架工具，仅需解析 `init` + `--output` 两个参数
//! - 避免为如此简单的接口引入 `clap` 及其传递依赖（约 20+ crate）
//! - 与 `prism-cli`（功能丰富的交互式 CLI）不同，本工具不需要子命令补全、帮助生成等高级特性

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const HOST_TEMPLATE: &str = include_str!("../templates/prism_host.rs");
const README_TEMPLATE: &str = include_str!("../templates/README.md");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 || args[1] == "--help" || args[1] == "-h" {
        print_usage();
        return ExitCode::SUCCESS;
    }

    match args[1].as_str() {
        "init" => cmd_init(&args[2..]),
        _ => {
            eprintln!("未知命令: {}\n", args[1]);
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!("clash-prism-ext — Prism Engine 适配模板脚手架\n");
    println!("用法:");
    println!("  clash-prism-ext init [--output <dir>]\n");
    println!("命令:");
    println!("  init    生成通用适配模板到指定目录（默认 ./prism-adapter）");
    println!();
    println!("示例:");
    println!("  clash-prism-ext init");
    println!("  clash-prism-ext init --output ./src-tauri/src/prism");
}

fn cmd_init(args: &[String]) -> ExitCode {
    let output = match parse_output_arg(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("错误: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = fs::create_dir_all(&output) {
        eprintln!("错误: 创建输出目录失败: {}", e);
        return ExitCode::FAILURE;
    }

    let host_path = output.join("prism_host.rs");
    let readme_path = output.join("README.md");

    // 检查文件是否已存在，存在时提示用户确认
    for path in [&host_path, &readme_path] {
        if path.exists() {
            eprint!("文件 {} 已存在，是否覆盖？[y/N] ", path.display());
            io::stderr().flush().ok();
            let mut input = String::new();
            if let Err(e) = io::stdin().read_line(&mut input) {
                eprintln!("错误: 读取输入失败: {}", e);
                return ExitCode::FAILURE;
            }
            let answer = input.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("已取消。");
                return ExitCode::SUCCESS;
            }
        }
    }

    // 先写入临时文件，全部成功后再 rename 到目标路径，
    // 避免第一个文件写入成功但第二个失败导致部分覆盖的不一致状态
    let host_tmp = output.join("prism_host.rs.tmp");
    let readme_tmp = output.join("README.md.tmp");

    if let Err(e) = fs::write(&host_tmp, HOST_TEMPLATE) {
        eprintln!("错误: 写入 prism_host.rs 临时文件失败: {}", e);
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(&readme_tmp, README_TEMPLATE) {
        eprintln!("错误: 写入 README.md 临时文件失败: {}", e);
        // 清理已写入的临时文件
        let _ = fs::remove_file(&host_tmp);
        return ExitCode::FAILURE;
    }

    // 所有临时文件写入成功，原子 rename 到目标路径
    // copy + delete（非原子）。对于脚手架工具而言，此风险可接受：
    // 1. 失败时已有临时文件清理逻辑
    // 2. 脚手架生成的文件可随时重新生成
    // 3. 用户可通过 --output 指定输出目录确保在同一文件系统
    if let Err(e) = fs::rename(&host_tmp, &host_path) {
        eprintln!("错误: 重命名 prism_host.rs 失败: {}", e);
        let _ = fs::remove_file(&host_tmp);
        let _ = fs::remove_file(&readme_tmp);
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::rename(&readme_tmp, &readme_path) {
        eprintln!("错误: 重命名 README.md 失败: {}", e);
        let _ = fs::remove_file(&readme_tmp);
        return ExitCode::FAILURE;
    }

    println!("✓ 模板已生成:");
    println!("  {}", host_path.display());
    println!("  {}", readme_path.display());
    println!();
    println!("下一步:");
    println!("  1. 打开 prism_host.rs，按 TODO 注释填充你的项目配置读写逻辑");
    println!("  2. 在 Cargo.toml 中添加 clash-prism-extension 依赖");
    println!("  3. 在 lib.rs 中注册 init_prism() 和 Tauri Commands");
    println!("  4. 在前端调用 prism:apply 等 API");

    ExitCode::SUCCESS
}

/// 使错误处理流程更清晰、可测试。
fn parse_output_arg(args: &[String]) -> Result<PathBuf, String> {
    for i in 0..args.len() {
        // 支持 --output <dir> 和 --output=<dir> 两种形式
        if args[i] == "--output" {
            if i + 1 >= args.len() {
                return Err("--output 需要一个参数值".to_string());
            }
            if args[i + 1].starts_with('-') {
                return Err(format!(
                    "--output 的值不能以 '-' 开头（得到: {}）",
                    args[i + 1]
                ));
            }
            return Ok(PathBuf::from(&args[i + 1]));
        }
        if let Some(value) = args[i].strip_prefix("--output=") {
            if value.is_empty() {
                return Err("--output 需要一个参数值".to_string());
            }
            return Ok(PathBuf::from(value));
        }
    }
    Ok(PathBuf::from("./prism-adapter"))
}
