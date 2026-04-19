# Prism Engine (棱镜引擎)

[![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml/badge.svg)](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-773%20passed-success.svg)]()

> **Prism**：多源输入经统一中间层折射，输出精确、可追溯的最终配置。

## 它是什么？

**Prism Engine** 是一个纯 Rust 编写的 **mihomo / Clash 配置增强引擎**。

它解决的核心问题是：**如何用声明式的方式管理和变换代理配置，而不是手动编辑庞大的 YAML 文件。**

### 典型场景

- 你有多个 `.prism.yaml` 文件，分别管理规则、代理、DNS 等不同模块
- 你想根据条件（平台、时间、WiFi）自动切换配置
- 你在开发一个 mihomo GUI 客户端，需要一个可嵌入的配置引擎
- 你需要配置变更的完整追踪（谁改了什么、改了多少条规则）

### 它能做什么？

| 能力 | 说明 |
|------|------|
| 📝 **声明式配置** | 用 `.prism.yaml` DSL 描述"想要什么"，引擎自动计算最终配置 |
| 🔀 **8 种操作** | `$filter` / `$remove` / `$transform` / `$default` / `$prepend` / `$append` / 深度合并 / `$override` |
| 📊 **变更追踪** | 每次编译输出精确统计：+N 条规则、-M 条规则、~K 条修改 |
| 🔌 **插件扩展** | JavaScript 脚本 + 插件系统，支持自定义逻辑 |
| 🖥️ **GUI 集成** | 提供 `PrismHost` trait，Tauri / Electron 客户端 ~100 行代码接入 |
| 🌐 **HTTP API** | 内置 REST 服务器，GUI 客户端通过 API 调用引擎 |
| 👁️ **文件监听** | 自动监听 `.prism.yaml` 变更，实时重新编译 |

## 谁在使用它？

Prism Engine 有两种使用方式：

### 1️⃣ GUI 开发者（主要用户）

如果你在开发 mihomo / Clash 的 GUI 客户端（如 Tauri 应用），可以将 Prism Engine 作为库嵌入：

```toml
[dependencies]
clash-prism-extension = "0.1.1"
```

实现 `PrismHost` trait（4 个方法），即可获得完整的配置管理能力：

```rust
pub trait PrismHost: Send + Sync {
    fn read_running_config(&self) -> Result<String>;       // 读取当前配置
    fn apply_config(&self, config: &str, status: &ApplyStatus) -> Result<()>;  // 写入配置
    fn get_prism_workspace(&self) -> Result<PathBuf>;      // 获取工作目录
    fn notify(&self, event: PrismEvent);                   // 接收事件通知
}
```

脚手架工具一键生成适配代码：

```bash
prism-ext init --output src-tauri/src/
```

生成的代码包含 16 个 Tauri Command（`prism_apply`、`prism_list_rules`、`prism_toggle_group` 等），前端直接调用。

获取完整执行追踪报告：

```rust
let report = ext.trace_report()?;  // 返回文本格式的逐条变更详情
```

### 2️⃣ 终端用户 / 调试

`prism-cli` 提供命令行工具，适合调试和独立使用：

```bash
# 一次性编译：读取 config.yaml + prism/ 目录下的所有 .prism.yaml，输出最终配置
prism-cli apply --config ./config.yaml --prism-dir ./prism

# 编译并输出完整执行追踪报告（逐条变更详情）
prism-cli apply --config ./config.yaml --prism-dir ./prism --verbose

# 启动 HTTP 服务（GUI 客户端通过 API 调用）
prism-cli serve --port 9097 --config ./config.yaml --prism-dir ./prism

# 验证 DSL 语法
prism-cli check rules.prism.yaml

# 解析并预览 Patch IR
prism-cli parse rules.prism.yaml

# 执行 JavaScript 脚本
prism-cli run script.js --config ./config.yaml

# 监听文件变化，自动重新编译
prism-cli watch ./prism --output ./config.yaml

# 查看引擎状态
prism-cli status --prism-dir ./prism
```

## Prism DSL 示例

假设你的 mihomo `config.yaml` 中有 100 个代理节点，你想：

1. 只保留 SS 类型的美国节点
2. 在最前面插入自己的代理
3. 在规则列表最前面添加公司直连规则

创建 `prism/rules.prism.yaml`：

```yaml
proxies:
  # 第 1 步：过滤 — 只保留 SS 且服务器名包含 "US" 的节点
  $filter: "type == 'ss' && server.contains('US')"

  # 第 2 步：前置插入 — 在剩余节点最前面添加自己的代理
  $prepend:
    - name: "my-ss-proxy"
      type: ss
      server: us1.example.com
      port: 8388
      cipher: aes-256-gcm
      password: "your-password"

rules:
  # 第 3 步：前置插入 — 公司域名直连
  $prepend:
    - "DOMAIN-SUFFIX,company.com,DIRECT"

  # 第 4 步：末尾追加 — 兜底规则
  $append:
    - "MATCH,Proxy"
```

执行编译：

```bash
prism-cli apply --config ./config.yaml --prism-dir ./prism
```

输出：

```
⚡ Prism Engine Apply

📊 编译统计:
  总 Patch 数: 3
  成功: 3
  跳过: 0
  新增规则: 2
  删除规则: 0
  修改规则: 0
  总耗时: 142μs (平均 47μs/patch)
```

最终 `config.yaml` 被自动更新：过滤后的代理列表 + 你的自定义代理 + 新的规则顺序。

## 条件作用域

根据平台、时间、WiFi 等条件自动应用不同配置：

```yaml
__when__:
  platform: windows
  ssid: "Office-WiFi"
  time: "09:00-18:00"

proxies:
  $prepend:
    - name: "work-proxy"
      type: ss
      server: proxy.company.com
      port: 8388
```

## 特性

- **8 个 DSL 操作** — 固定执行顺序：`$filter` → `$remove` → `$transform` → `$default` → `$prepend` → `$append` → 深度合并 → `$override`
- **统一 Patch IR** — 所有输入编译为统一的中间表示，支持字段溯源和步骤回放
- **JavaScript 脚本引擎** — rquickjs (QuickJS-NG) 沙箱，ES2023+，5 层静态验证 + 5 层运行时加固
- **插件系统** — 6 种组件（patches / scripts / hooks / templates / scorers / validators），8+1 生命周期钩子
- **Smart Selector** — EMA 评分（P90 延迟 + 成功率 + 稳定性），自适应测速调度
- **4 层作用域** — Global → Profile → Scoped → Runtime，Profile 级并发执行
- **安全纵深** — 路径遍历防护、ReDoS 防护、原型链污染防护、常量时间 API key 比较、原子文件写入

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
| `clash-prism-core` | 核心引擎：Patch IR、编译器、执行器、校验器、缓存、文件监听 | [![cr](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core) |
| `clash-prism-dsl` | DSL 解析器：`.prism.yaml` 解析、静态字段校验、JSON Schema | [![cr](https://img.shields.io/crates/v/clash-prism-dsl.svg)](https://crates.io/crates/clash-prism-dsl) |
| `clash-prism-script` | 脚本引擎：rquickjs 沙箱、结构化 API、KV 存储 | [![cr](https://img.shields.io/crates/v/clash-prism-script.svg)](https://crates.io/crates/clash-prism-script) |
| `clash-prism-smart` | 智能选择器：EMA 评分、时间衰减、自适应测速 | [![cr](https://img.shields.io/crates/v/clash-prism-smart.svg)](https://crates.io/crates/clash-prism-smart) |
| `clash-prism-plugin` | 插件系统：生命周期钩子、多组件架构、Cron 调度 | [![cr](https://img.shields.io/crates/v/clash-prism-plugin.svg)](https://crates.io/crates/clash-prism-plugin) |
| `clash-prism-extension` | GUI 接入层：PrismHost trait、规则注解、JSON API | [![cr](https://img.shields.io/crates/v/clash-prism-extension.svg)](https://crates.io/crates/clash-prism-extension) |
| `prism-cli` | CLI 工具：apply / serve / watch / parse / check / run | [二进制下载](https://github.com/Juwan-Hwang/Clash-Prism-Engine/releases) |
| `prism-ext` | 脚手架工具：`prism-ext init` 生成 GUI 适配模板 | [二进制下载](https://github.com/Juwan-Hwang/Clash-Prism-Engine/releases) |

## DSL 操作参考

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

## 文档

| 文档 | 说明 |
|------|------|
| [Prism_Engine_Final_Architecture.md](Prism_Engine_Final_Architecture.md) | 完整架构设计规范（14 章，83 项需求） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 开发环境搭建与贡献流程 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |
| [SECURITY.md](SECURITY.md) | 安全漏洞报告策略 |
| [examples/](examples/) | 使用示例（基础用法、插件、Smart Selector） |
| [prism-schema.json](prism-schema.json) | DSL JSON Schema（VS Code 高亮） |

## 许可证

Apache License, Version 2.0 — 详见 [LICENSE](LICENSE)

## 致谢

- [mihomo](https://github.com/MetaCubeX/mihomo) — Clash Meta 内核
- [QuickJS-NG](https://github.com/nicknisi/quickjs-ng) — JavaScript 引擎
- [rquickjs](https://github.com/DelSkayn/rquickjs) — QuickJS Rust 绑定

---

[English](README.en.md) | 简体中文
