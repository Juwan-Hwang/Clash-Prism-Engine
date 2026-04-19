# Prism Engine (棱镜引擎)

[![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml/badge.svg)](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-773%20passed-success.svg)]()

> **Prism**：多源输入经统一中间层折射，输出精确、可追溯的最终配置。

**Prism Engine** 是一个纯 Rust 编写的配置增强引擎，专为 [mihomo](https://github.com/MetaCubeX/mihomo) / Clash 内核设计。通过统一的 **Patch IR** 中间表示，将 DSL 声明、JavaScript 脚本、插件系统等多种输入源编译为确定性的配置变换，实现可追溯、可调试、可复现的代理配置管理。

## 特性

- **8 个 DSL 操作** — `$override` / `$prepend` / `$append` / `$filter` / `$transform` / `$remove` / `$default` / 深度合并，固定执行顺序
- **统一 Patch IR** — 所有输入编译为统一的中间表示，支持 Explain View 字段溯源和 Step Replay
- **JavaScript 脚本引擎** — 基于 rquickjs (QuickJS-NG) 沙箱，ES2023+ 支持，5 层静态验证 + 5 层运行时加固
- **插件系统** — 6 种组件（patches / scripts / hooks / templates / scorers / validators），8+1 生命周期钩子
- **Smart Selector** — 独立运行时模块，EMA 评分（P90 延迟 + 成功率 + 稳定性），自适应测速调度
- **4 层作用域** — Global → Profile → Scoped → Runtime，Profile 级并发执行
- **GUI 接入接口** — `PrismHost` trait，~100 行代码即可集成到 Tauri / Electron 客户端
- **调试系统** — Diff View / Trace View / Explain View / Step Replay / PerfTracker
- **安全纵深** — 路径遍历防护、ReDoS 防护、原型链污染防护、常量时间 API key 比较、原子文件写入

## 快速开始

### 添加依赖

```toml
[dependencies]
clash-prism-core = "0.1.0"
clash-prism-dsl = "0.1.0"
```

### 基础用法

```rust
use clash_prism_core::{PatchCompiler, PatchExecutor, TargetCompiler};
use clash_prism_dsl::DslParser;

// 1. 解析 DSL 文件
let dsl = DslParser::parse_file("rules.prism.yaml")?;

// 2. 编译为 Patch IR
let mut compiler = PatchCompiler::new();
compiler.register_dsl_patches(&dsl)?;
let patches = compiler.compile_and_execute(base_config.clone())?;

// 3. 输出目标格式
let output = TargetCompiler::to_mihomo_yaml(&patches.output)?;
```

### Prism DSL 示例

```yaml
# rules.prism.yaml
proxies:
  $filter: "type == 'ss' && server.contains('US')"
  $prepend:
    - name: "my-ss-proxy"
      type: ss
      server: us1.example.com
      port: 8388

rules:
  $prepend:
    - "DOMAIN-SUFFIX,example.com,DIRECT"
  $append:
    - "MATCH,Proxy"
```

## 架构

```
输入源 (DSL / Script / Plugin)
        │
        ▼
  ┌─ Patch Compiler ─┐
  │  静态字段校验      │
  │  条件预编译        │
  │  依赖拓扑排序      │
  └────────┬─────────┘
           │
           ▼
    ┌─ Patch IR ─┐     ← 统一中间表示
    │  可序列化    │
    │  可追溯      │
    │  可回放      │
    └─────┬──────┘
          │
          ▼
  ┌─ Patch Executor ─┐
  │  Profile 级并发    │
  │  固定操作顺序      │
  │  ExecutionTrace   │
  └────────┬─────────┘
           │
           ▼
  ┌─ Validator ─┐
  │  7 项校验     │
  │  智能建议     │
  └─────┬───────┘
        │
        ▼
  ┌─ Target Compiler ─┐
  │  mihomo / clash-rs │
  └───────────────────┘
```

完整架构设计请参阅 [Prism_Engine_Final_Architecture.md](Prism_Engine_Final_Architecture.md)。

## Crate 概览

| Crate | 说明 | Crates.io |
|-------|------|-----------|
| `clash-prism-core` | 核心引擎：Patch IR、编译器、执行器、校验器、缓存、文件监听 | [![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core) |
| `clash-prism-dsl` | DSL 解析器：`.prism.yaml` 解析、静态字段校验、JSON Schema | [![crates.io](https://img.shields.io/crates/v/clash-prism-dsl.svg)](https://crates.io/crates/clash-prism-dsl) |
| `clash-prism-script` | 脚本引擎：rquickjs 沙箱、结构化 API、KV 存储 | [![crates.io](https://img.shields.io/crates/v/clash-prism-script.svg)](https://crates.io/crates/clash-prism-script) |
| `clash-prism-smart` | 智能选择器：EMA 评分、时间衰减、自适应测速 | [![crates.io](https://img.shields.io/crates/v/clash-prism-smart.svg)](https://crates.io/crates/clash-prism-smart) |
| `clash-prism-plugin` | 插件系统：生命周期钩子、多组件架构、Cron 调度 | [![crates.io](https://img.shields.io/crates/v/clash-prism-plugin.svg)](https://crates.io/crates/clash-prism-plugin) |
| `clash-prism-extension` | GUI 接入层：PrismHost trait、规则注解、JSON API | [![crates.io](https://img.shields.io/crates/v/clash-prism-extension.svg)](https://crates.io/crates/clash-prism-extension) |
| `prism-cli` | CLI 工具：apply / status / serve / watch | 二进制分发 |
| `prism-ext` | 脚手架工具：`prism-ext init` 生成适配模板 | 二进制分发 |

## DSL 操作

| 操作 | 语法 | 说明 |
|------|------|------|
| 深度合并 | (默认键) | 递归合并子项 |
| `$override` | `$override: {...}` | 强制覆盖（独占键） |
| `$prepend` | `$prepend: [...]` | 数组前置插入 |
| `$append` | `$append: [...]` | 数组末尾追加 |
| `$filter` | `$filter: "expr"` | 条件过滤（仅静态字段） |
| `$transform` | `$transform: {...}` | 映射变换 |
| `$remove` | `$remove: "expr"` | 条件删除 |
| `$default` | `$default: {...}` | 默认值注入 |

**固定执行顺序**：`$filter` → `$remove` → `$transform` → `$default` → `$prepend` → `$append` → 深度合并 → `$override`

## 条件作用域

```yaml
__when__:
  core: mihomo
  platform: windows
  profile: "work*"
  time: "09:00-18:00"
  enabled: true
  ssid: "Office-WiFi"

proxies:
  $prepend:
    - name: "work-proxy"
      ...
```

## 插件开发

```json
{
  "id": "my-plugin",
  "name": "My Plugin",
  "version": "1.0.0",
  "type": "config",
  "permissions": ["config:read", "config:write"],
  "hooks": ["OnMerged", "OnBeforeWrite"],
  "entry": "main.js"
}
```

## GUI 集成

Prism Engine 通过 `PrismHost` trait 接入 GUI 客户端，仅需实现 4 个必须方法：

```rust
pub trait PrismHost: Send + Sync {
    fn read_running_config(&self) -> Result<String>;
    fn apply_config(&self, config: &str, status: &ApplyStatus) -> Result<()>;
    fn get_prism_workspace(&self) -> Result<PathBuf>;
    fn notify(&self, event: PrismEvent);
}
```

提供脚手架工具快速生成适配代码：

```bash
prism-ext init --output src-tauri/src/
```

## 安全

| 措施 | 说明 |
|------|------|
| 脚本沙箱 | 5 层静态验证 + 5 层运行时加固 |
| Unicode 安全 | NFKC 归一化 + 危险字符移除 + BOM 感知 |
| 路径遍历防护 | canonicalize + 前缀检查 |
| ReDoS 防护 | DFA size limit + LRU 正则缓存 |
| 原子文件写入 | tmp + sync_all + rename |
| 常量时间比较 | API key 认证防时序攻击 |
| HTTP 速率限制 | 滑动窗口 60 req/60s per IP |

安全漏洞请参阅 [SECURITY.md](SECURITY.md)。

## 项目结构

```
Clash-Prism-Engine/
├── crates/
│   ├── clash-prism-core/       # 核心引擎
│   ├── clash-prism-dsl/        # DSL 解析器
│   ├── clash-prism-script/     # 脚本引擎
│   ├── clash-prism-smart/      # 智能选择器
│   ├── clash-prism-plugin/     # 插件系统
│   └── clash-prism-extension/  # GUI 接入层
├── prism-cli/                  # CLI 工具
├── prism-ext/                  # 脚手架工具
├── examples/                   # 使用示例
├── tests/                      # 集成测试
├── fuzz/                       # Fuzzing targets
├── .github/workflows/          # CI/CD
├── Prism_Engine_Final_Architecture.md  # 架构规范文档
├── CHANGELOG.md                # 变更日志
├── CONTRIBUTING.md             # 贡献指南
├── SECURITY.md                 # 安全策略
└── prism-schema.json           # DSL JSON Schema
```

## 构建

```bash
# Debug
cargo build --workspace

# Release
cargo build -p prism-cli --release

# 测试
cargo test --workspace

# Lint
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features
```

## 文档

| 文档 | 说明 |
|------|------|
| [Prism_Engine_Final_Architecture.md](Prism_Engine_Final_Architecture.md) | 完整架构设计规范（14 章，83 项需求） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 开发环境搭建与贡献流程 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |
| [SECURITY.md](SECURITY.md) | 安全漏洞报告策略 |
| [examples/](examples/) | 使用示例（基础用法、插件、Smart Selector） |
| [prism-schema.json](prism-schema.json) | DSL 文件 JSON Schema（VS Code 高亮） |

## 许可证

Apache License, Version 2.0 — 详见 [LICENSE](LICENSE)

## 致谢

- [mihomo](https://github.com/MetaCubeX/mihomo) — Clash Meta 内核
- [QuickJS-NG](https://github.com/nicknisi/quickjs-ng) — JavaScript 引擎
- [rquickjs](https://github.com/DelSkayn/rquickjs) — QuickJS Rust 绑定

---

[English](README.en.md) | 简体中文
