# Changelog

All notable changes to Prism Engine will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **profile_name 未注入 ExecutionContext** — `compile_pipeline` 中创建 `PatchExecutor` 时未设置 `context.profile_name`，导致 `__when__.profile` 条件匹配始终失败（所有带 profile 条件的 Patch 被静默跳过）；改为通过 `PatchExecutor::with_context()` 从 `PrismHost::get_current_profile()` 获取并注入

### Added

- **PrismHost::get_current_profile()** — 新增默认方法，返回当前激活的 Profile 名称（默认 `None`），GUI 接入方可覆盖此方法以启用 `__when__.profile` 条件匹配

- **变量模板系统 (`__vars__` + `{{var}}`)** — DSL 文件支持 `__vars__` 声明文件级变量默认值，Patch 中使用 `{{var_name}}` 或 `{{var_name|default}}` 模板占位符，运行时由 `PrismHost::get_variables()` 注入实际值。优先级：Host 变量 > `__vars__` 文件默认值 > 内联默认值。解决了规则文件跨订阅复用时 proxy group 名称硬编码的问题

## [0.1.2] - 2026-04-21

### Performance

大规模规则场景（几万条规则注入）下的全面性能优化：

- **Annotation 缓存** — `get_current_annotations()` 从每次重算 O(A×R) 改为 O(1) 缓存读取，`list_rules()` / `toggle_group()` / `get_stats()` 响应从秒级降到毫秒级
- **文件级解析缓存** — 基于 SHA-256 内容哈希，未变更的 `.prism.yaml` 文件跳过 DSL 解析，重复编译速度提升 50-70%
- **output_config Arc 共享** — `ExtensionState` 和 `WatchResult` 之间通过 `Arc<Value>` 共享配置，省掉一次数 MB 的 JSON 深拷贝
- **is_prism_rule HashMap 索引** — 从 O(N) 线性扫描改为 O(1) HashMap 查询，GUI 渲染规则列表时大幅加速
- **find_rule_index 预解析** — 对象格式规则的 JSON 解析从 A×R 次降为 A 次
- **filter/remove retain 模式** — `$filter` 和 `$remove` 操作从全量克隆改为 `retain` 原地过滤，仅对被移除元素做克隆，保留 trace 完整性
- **Patch 引用传递** — `execute()` / `execute_pipeline()` 签名改为 `&[&Patch]`，编译管线中避免所有 Patch 的深拷贝；新增 `execute_owned()` 便捷方法保持向后兼容

### Changed

- `ExtensionState.last_output` 和 `WatchResult.output` 类型从 `serde_json::Value` 改为 `Arc<serde_json::Value>`
- `PatchExecutor::execute()` 签名从 `&[Patch]` 改为 `&[&Patch]`（新增 `execute_owned()` 保持兼容）
- `PatchExecutor::execute_pipeline()` 和 `execute_profile_patches()` 签名改为引用传递
- `compile_and_execute_pipeline()` 中 Patch 按 scope 分类时不再 clone，改为引用
- `clash-prism-extension` 新增 `sha2` 依赖（文件内容哈希）

### Fixed

- **$prepend 规则顺序错误** — `splice(0..0, iter)` 是一次性插入，`.rev()` 反而将声明顺序反转，导致 `$prepend: [A, B]` 产生 `[B, A, ...original...]` 而非预期的 `[A, B, ...original...]`
- **append/prepend trace 不准确** — 目标路径不存在时操作为 no-op，但 trace 仍报告 `added=N`；改为实际添加数为 0
- **DeepMerge 多普通键覆盖** — 多个非 `$` 前缀普通键（如 `dns` + `rules`）时后者覆盖前者，改为 `extend` 合并
- **profile 条件 fall through** — 未指定 `--profile` 时，带 profile 条件的 Scoped patch 被错误执行；添加显式 `false` 返回
- **Reflect 绕过沙箱** — `_dangerous` 列表和 bracket-access 正则添加 `Reflect`，堵住沙箱逃逸路径
- **eval_replace_call 过度裁剪** — `trim_end_matches(')')` 改为 `strip_suffix(')')`，避免裁剪 replacement 中的括号
- **is_prism_rule 双重加锁** — 从两次独立 Mutex 获取改为单次加锁，消除 TOCTOU 窗口
- **scheduler 除零防御** — `next_interval` 线性插值添加 `good == bad` 防御性检查
- **validate_path_within_base 路径分隔符** — 添加 `MAIN_SEPARATOR` 检查，防止 `/data/plugins-evil/` 匹配 `/data/plugins`
- **沙箱安全检查管线一致性** — `execute()` 中沙箱检查使用 `preprocess_unicode_escapes` 预处理后的脚本
- **JSON Schema additionalProperties 位置** — 从 `properties` 内移到顶层，修复 IDE 自动补全
- **max_config_bytes 未检查** — `execute()` 入口添加配置大小检查，防内存炸弹
- **PluginLoadError 映射语义** — `Validation` 和 `NotFound` 从 `PrismError::DslParse` 改为 `PrismError::Validation`
- **lib.rs 注释** — "rquickjs, zero C dependency" 改为 "Pure Rust expression evaluator"
- **schema $default null 一致性** — `array_field` 的 `$default` 类型添加 `"null"`，与 `config_field` 保持一致
- **parse_cache Arc 共享** — `parse_cache` 从 `ExtensionState` 提取为 `Arc<Mutex<ParseCache>>`，通过 `new_with_shared` 传递给 watcher 线程
- `insert_rule()` 后未清空注解缓存和 HashMap 索引，导致规则索引偏移后 `is_prism_rule()` 和 `list_rules()` 返回错误数据
- `get_current_annotations()` 使用 `!is_empty()` 判断缓存有效性，无法区分"从未编译"和"编译结果为空"，改为基于 `compile_success` 标志

### Added

- **68 个新测试**，覆盖之前缺失的场景：
  - `error.rs` — PrismError Display、From 转换链、TransformWarning（8 个）
  - `trace.rs` — replay 边界、import 长度不一致、statistics 精确计算（5 个）
  - `annotation.rs` — find_rule_index、extract_rule_annotations、source_label（7 个）
  - `smart/config.rs` — SmartConfig::validate 全分支（7 个）
  - `smart/scheduler.rs` — AdaptiveScheduler::next_interval 全场景（6 个）
  - `executor.rs` — filter/remove affected_items 内容验证、trace summary 验证（6 个）
  - `loader.rs` — PluginLoadError 映射测试（4 个）
  - `parser.rs` — $default null 解析（2 个）
  - `integration_test.rs` — 缓存一致性/失效、is_prism_rule 正向测试、对象格式规则注解、insert_rule 注解清空（6 个）
  - `hook.rs` — 24 个内置钩子全格式覆盖、固定时间精确断言（4 个）
  - `api.rs` — DFA 限制测试双分支断言（1 个）
- **ReadOnlyTestHost** — `apply_config` 不更新 `running_config` 的测试辅助结构，用于缓存一致性测试
- 加强 21 处已有测试断言（精确匹配替代 contains/len/is_some 模糊断言）

## [0.1.1] - 2026-04-19

### Added

- `PrismExtension::trace_report()` — 生成完整执行追踪文本报告（GUI 和 CLI 均可调用）
- `prism-cli apply --verbose` — 输出逐条变更详情的 Trace Report
- `rustfmt.toml` / `.clippy.toml` — 锁定格式化和 lint 配置
- `SECURITY.md` — 安全漏洞报告策略
- `README.en.md` — 英文版 README
- Release CI workflow — 4 平台自动构建（Linux / macOS x64 / macOS ARM / Windows）

### Changed

- Trace Report 品牌行简化为 `Powered by Prism Engine`
- CLI `apply` / `parse` / `run` 命令输出末尾添加 `Powered by Prism Engine`
- CONTRIBUTING.md Rust 版本修正为 1.88+
- CONTRIBUTING.md clone URL 修正为 `Juwan-Hwang/Clash-Prism-Engine`
- `.gitignore` 添加 `*.pdb`

### Fixed

- `cron_scheduler.rs` sleep(0) 无限循环 bug（`.max(0)` → `.max(1)`）
- CI flaky test（用不存在的日期替代 `* * * * *`）
- CI cross-check job 禁用（rquickjs-sys 含 C 代码无法交叉编译）

## [0.1.0] - 2026-04-18

### Added

**Core Engine**
- Patch IR — unified intermediate representation for all configuration transformations
- 8 DSL operations: DeepMerge, Override, Prepend, Append, Filter, Transform, Remove, SetDefault
- Fixed execution order: $filter → $remove → $transform → $default → $prepend → $append → DeepMerge → Override
- Pure Rust expression evaluator (no JS dependency for predicates/transforms)
- Two-phase pipeline: Profile-level concurrency + Shared-level merge
- 4-layer scope system: Global, Profile, Scoped, Runtime
- Execution tracing with Explain View, Diff View, and step replay
- Configuration validator with 7 checks and smart suggestions
- Target compiler for mihomo / clash-rs / JSON output
- Atomic file writes (temp + sync_all + rename) with cross-device fallback

**DSL Parser**
- `.prism.yaml` file parsing with `__when__` conditional scopes and `__after__` dependency declarations
- Static field whitelist enforcement (compile-time rejection of runtime field references)
- Template string escape handling with nested `${}` depth tracking
- Conditional rule objects for $prepend/$append (`__when__` + `__rule__`)
- JSON Schema for IDE autocompletion and syntax validation

**Script Engine**
- rquickjs 0.11 sandbox with 4-layer static validation + runtime hardening
- Structured script API: config, utils (proxies/rules/groups), patch, store, env, log
- KV storage with optional redb persistence
- 9 configurable security limits (execution time, memory, loops, recursion, etc.)
- Unbalanced bracket detection and prototype chain pollution prevention

**Plugin System**
- Config Plugin and UI Extension types with minimum-privilege principle
- manifest.json validation (ID format, path traversal, permission checks)
- Multi-component architecture (patches, scripts, hooks, templates, scorers, validators)
- 8+1 lifecycle hooks with Cron scheduling (5-field standard format)
- Hook result aggregation with chain-passing and condition filtering
- NodeFailPolicy — Rust-native automatic failover (no JS overhead)

**Smart Selector**
- EMA scoring with P90 latency, success rate, and stability weights
- Time decay with configurable half-life
- Adaptive speed test scheduling (network quality-based interval adjustment)
- smart.toml configuration with validation

**Extension Interface**
- `PrismHost` trait (4 required + 4 optional methods)
- `PrismExtension<H>` API (apply, status, list_rules, preview_rules, toggle_group, etc.)
- Rule annotation system (RuleAnnotation + group_annotations)
- Guarded fields protection (external-controller, secret, mixed-port, etc.)
- Path traversal protection with canonicalize + prefix verification

**CLI**
- `prism-cli apply` — execute full compilation pipeline
- `prism-cli status` — show engine status
- `prism-cli serve` — HTTP server mode (axum, port 9097)
- NDJSON output with ECMA-262 safe serialization (U+2028/U+2029 escaping)
- PID file lock with cross-process mutual exclusion and RAII auto-release

**Infrastructure**
- Three-level cache (L1 memory with mtime + L2 disk with SHA-256 + L3 reserved)
- Configuration migration system (idempotent, version-tracked, ordered execution)
- Deterministic serialization (BTreeMap recursive key sorting + content hashing)
- Unicode security sanitization (NFKC normalization + dangerous character removal + BOM-aware reading)
- User-friendly error formatting (9 categories + actionable fix suggestions)
- Performance tracker (PerfTracker) with high-order function pattern

**Tooling**
- VS Code extension (syntax highlighting + JSON Schema validation)
- `clash-prism-ext init` scaffolding tool for GUI integration
- Fuzzing targets (DSL parser, expression evaluator, YAML round-trip)
- Criterion benchmarks (parser, predicate, transform, full pipeline)
- GitHub Actions CI (lint, test, MSRV, release build, cross-compile)
- Schema consistency checker (VALID_OPS ↔ prism-schema.json sync verification)

### Security

- Path traversal prevention (null byte, `..`, canonicalize, prefix check)
- Unicode injection prevention (zero-width chars, direction control, BOM, private use areas)
- ReDoS protection (DFA size limit 1MB + thread-local LRU regex cache)
- Script sandbox (Unicode escape preprocessing + 5-layer static validation + runtime per-property hardening, quickjs-ng compatible)
- Prototype chain pollution prevention (`__proto__`, `constructor`, `prototype`)
- Constant-time API key comparison
- HTTP rate limiting (sliding window, 60 req/60s per IP)
- Guarded fields (external-controller, secret, mixed-port, etc.)
