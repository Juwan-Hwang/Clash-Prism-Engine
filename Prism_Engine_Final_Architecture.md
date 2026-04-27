# Prism Engine (棱镜引擎) — 最终架构规范 v0.1.0 (全量同步版)

> **Prism**：多源输入经统一中间层折射，输出精确、可追溯的最终配置。
>
> **定位声明**：Prism 的 DSL 关键字（`$prepend`、`$filter` 等）以 `$` 为前缀，作为标准 YAML 映射键使用。`$` 是合法的 YAML 键名字符，与主流工具链原则上兼容；最终兼容性以项目 CI 实测矩阵为准。

---

## 0. 设计哲学

三条不可妥协的原则：

1. **一个引擎** — rquickjs（基于 quickjs-ng），预生成标准平台 bindings，无需用户端 C 编译工具链，ES2023+ 完整支持。不要 Boa，不要 mlua，不要 Deno Core，不要 WASM（v1）。
2. **一套 IR** — 所有输入（Prism DSL / 可视化拖拽 / JS 脚本 / 插件）编译为统一的 Patch IR。没有 IR 就没有 Explain View，没有 Explain View 就是黑盒。
3. **静态/运行时严格隔离** — 配置生成阶段只能引用静态字段（name、type、server）。延迟、成功率等运行时数据由独立的 Smart Selector 处理。编译器会在错误阶段引用运行时字段时直接报错。

---

## 1. 整体架构

> **Prism Engine v0.1.0** — 纯后端配置增强引擎
>
> **定位**：Prism Engine 是一个**纯 Rust 后端库**，不包含任何前端 UI 代码。所有 GUI 客户端（Tauri / Electron / Web）通过实现 `PrismHost` trait 接入，前端 UI 完全由接入方自行负责。
>
> **输入源**（均编译为 Patch）：
> - **Prism DSL**（8 个操作）— YAML 声明式
> - **Config Plugin** — rquickjs 沙箱内执行
>
> **处理管线**：
>
> 1. **Patch Compiler** — DSL/脚本/插件 → `Vec<Patch>`（静态字段校验 + 条件预编译）
> 2. **Patch IR** — 统一中间表示（可序列化 · 可追溯）
> 3. **Patch Executor** — Profile 级并发执行（记录 ExecutionTrace）
> 4. **Validator** — 合法性校验 + 智能建议
> 5. **Target Compiler** — 输出 mihomo / clash-rs 格式
>
> **独立模块**：
>
> | 模块 | Crate | 说明 |
> |------|-------|------|
> | Smart Selector | `clash-prism-smart` | EMA + P90 延迟 + 时间衰减 · 配置: `smart.toml` · <100KB |
> | UI Extension | `clash-prism-extension` | GUI 接入接口 · PrismHost trait · 规则注解 · JSON API 规范 |
> | 基础设施 | — | rquickjs · redb (KV) · tokio · serde · chrono |

---

## 2. Prism DSL 语法规范

### 2.1 设计原则

- 操作符以 `$` 开头，作为标准 YAML 映射键名使用（如 `$prepend:`）。`$` 是合法的 YAML 键名字符，所有工具链（yamllint / prettier / serde_yml / yq）均兼容。
- **Patch Compiler** 使用 `serde_yml` 的 `Value` 解析后提取 `$` 前缀键。由于 `$` 是标准字符串键名，无需自定义 YAML 解析层，`serde_yml` 直接解析即可。
- 通过 JSON Schema（配合 `# yaml-language-server: $schema=...` 头部注释）提供**自动补全和语法校验**
- 多个操作合并到**同一个键**下，避免 YAML 重复键问题
- `$filter` / `$transform` 表达式只能引用**静态字段**
- 同一键下的多个操作按**固定执行顺序**执行，不依赖 YAML 键的书写顺序（详见 2.4）

### 2.2 完整语法（8 个操作）

> **注意**：以下各操作示例彼此独立，展示的是不同 `.prism.yaml` 文件的语法片段。不可将多个示例直接合并到同一个文件中，否则会产生 YAML 重复键错误（如 `dns:` 出现两次）。

```yaml
# ═══════════════════════════════════════════════════════════
# 文件元数据（可选）
# ═══════════════════════════════════════════════════════════
__when__:                   # 条件作用域（仅当条件满足时生效）
  core: mihomo              # 内核类型
  platform: [macos, windows] # 操作系统
  profile: /流媒体|解锁/     # Profile 名称（正则匹配）
  time: "08:00-23:00"       # 时间段（可选）

__after__: ["base-dns"]     # 依赖声明（确保该文件在 base-dns 之后执行）
                           # 不再提供 __priority__，只用 __after__ 声明依赖
                           # 同级无依赖的文件按文件名字典序排列（确定性）

# ═══════════════════════════════════════════════════════════
# 操作 1：深度合并（无标签，默认行为）
# 适用场景：修改字典字段的部分子项
# ═══════════════════════════════════════════════════════════
dns:
  enable: true
  ipv6: false
  nameserver:
    - https://dns.alidns.com/dns-query

# ═══════════════════════════════════════════════════════════
# 操作 2：$override — 强制覆盖（独占键，不可与其他操作混用）
# 适用场景：完全替换一个字典字段，不递归合并
# ═══════════════════════════════════════════════════════════
tun:
  $override:
    enable: true
    stack: mixed
    auto-route: true
    auto-detect-interface: true

# ═══════════════════════════════════════════════════════════
# 操作 3：$prepend — 数组前置插入
# 操作 4：$append  — 数组末尾追加
# 多个操作合并到同一个键下（解决 YAML 重复键问题）
# ═══════════════════════════════════════════════════════════
rules:
  $prepend:
    - RULE-SET,my-direct,DIRECT
    - DOMAIN,internal.corp,DIRECT
  $append:
    - GEOIP,CN,DIRECT
    - MATCH,PROXY

# ═══════════════════════════════════════════════════════════
# 操作 5：$filter — 条件过滤（仅限静态字段）
# 操作 6：$transform — 映射变换（批量重命名等）
# 操作 7：$remove — 条件删除
# 多个操作可组合在同一个键下
# ═══════════════════════════════════════════════════════════
proxies:
  $filter: "p.name.includes('香港') || p.name.includes('HK')"
  $remove: "p.name.includes('过期')"
  $transform: "({...p, name: '🇭🇰 ' + p.name})"

# ═══════════════════════════════════════════════════════════
# 操作 8：$default — 默认值注入（仅当字段不存在时设置）
# 适用场景：为缺失的配置项提供兜底值，不覆盖用户已有的设置
# ═══════════════════════════════════════════════════════════
dns:
  $default:
    enhanced-mode: fake-ip
    fake-ip-filter:
      - "+.lan"
      - "+.local"

# ═══════════════════════════════════════════════════════════
# 条件作用域示例（声明式，免写代码）
# ⚠️ 一个 .prism.yaml 文件只能有一个 __when__ 声明
#    需要多个条件作用域时，使用多个文件（见 §2.3）
# ═══════════════════════════════════════════════════════════

# 正确：每个条件作用域用独立文件
# 文件: 01-telegram.prism.yaml
__when__:
  core: mihomo
  platform: macos
rules:
  $prepend:
    - PROCESS-NAME,Telegram,PROXY

# 文件: 02-streaming.prism.yaml
__when__:
  profile: /流媒体|解锁/
rules:
  $prepend:
    - DOMAIN-SUFFIX,netflix.com,PROXY
    - DOMAIN-SUFFIX,youtube.com,PROXY
```

### 2.3 文件级约束

> **一个 `.prism.yaml` 文件只能有一个 `__when__` 声明。需要多个条件作用域时，使用多个文件。**

**原因**：YAML 规范对重复键的行为未作定义，不同解析器处理方式不同——yamllint 和 serde_yml 会直接报错，部分解析器会静默覆盖后者。无论哪种结果都不是用户期望的行为，因此 Prism 在编译阶段会主动检测并拒绝含有重复 `__when__` 的文件。

**推荐实践**：按功能拆分文件，文件名编码执行顺序：

```
config/
├── 00-base-dns.prism.yaml          # 无 __when__，全局生效
├── 01-telegram.prism.yaml          # __when__: core=mihomo, platform=macos
├── 02-streaming.prism.yaml         # __when__: profile=/流媒体/
├── 03-ad-filter.prism.yaml         # 无 __when__，全局生效
└── __after__ 声明控制跨文件依赖
```

文件按名字典序排列（确定性），`__after__` 声明显式覆盖默认顺序。

### 2.4 固定执行顺序

当同一个键下声明了多个操作时，**不依赖 YAML 键的书写顺序**，而是按以下固定顺序执行：

```
$filter → $remove → $transform → $default → $prepend → $append → DeepMerge → Override
```

**理由**：

1. **先过滤**（`$filter`）— 保留匹配的元素，缩小后续操作的数据集
2. **再删除**（`$remove`）— 明确移除不需要的元素
3. **再变换**（`$transform`）— 对留下来的元素做批量修改（如重命名）
4. **再注入默认值**（`$default`）— 为缺失字段提供兜底值
5. **最后插入**（`$prepend` / `$append`）— 增加新内容
6. **深度合并**（`DeepMerge`）— 与已有字典递归合并
7. **强制覆盖**（`$override`）— 独占操作，不参与复合排序

> **注意**：`$override` 是独占操作，不可与其他操作混用（编译期报错）。`DeepMerge` 是无标签的默认行为，当同一键下同时存在 `$` 操作和普通键时，普通键作为 DeepMerge 参与排序。实际场景中，复合操作通常只涉及 `$filter` / `$remove` / `$transform` / `$prepend` / `$append` 这五个数组操作。

这个顺序符合直觉：**先减少，再修改，再增加**。

```yaml
# 用户写成这样（书写顺序不影响执行顺序）：
proxies:
  $transform: "({...p, name: '🇭🇰 ' + p.name})"   # 第 3 步执行
  $filter: "p.name.includes('香港')"               # 第 1 步执行
  $remove: "p.name.includes('过期')"                  # 第 2 步执行
  $append: [{ name: "手动节点", type: "ss", ... }]   # 第 4 步执行

# 实际执行顺序：
#   ① $filter: 保留包含"香港"的节点
#   ② $remove: 从中删除包含"过期"的节点
#   ③ $transform: 对最终保留的节点加国旗前缀
#   ④ $append: 追加手动节点
```

**关键语义：`$prepend` / `$append` 插入的元素不受 `$filter` 约束。**

`$filter` 只作用于执行前数组中已存在的元素。`$prepend` / `$append` 在 `$filter` 之后执行，新插入的元素直接进入最终数组，不经过过滤。

```yaml
proxies:
  $filter: "p.type === 'ss'"
  $prepend:
    - { name: "手动VMess", type: "vmess", server: "1.2.3.4", port: 443 }

# 执行过程：
#   ① $filter: 只保留 type === 'ss' 的节点（vmess 节点被过滤）
#   ② $prepend: 将手动VMess 节点插入到数组开头
#
# 最终结果：手动VMess 节点存在，ss 节点被保留，原有的 vmess 节点被过滤
# 手动VMess 不受 $filter 影响，因为它是在 $filter 之后插入的
```

### 2.5 操作一览表

| 操作 | 标签 | 目标类型 | 频率 | 说明 |
|------|------|---------|------|------|
| 深度合并 | (无标签) | 字典/数组 | 40% | 默认行为，递归合并子项 |
| 强制覆盖 | `$override` | 字典 | 15% | 独占该字段的唯一键，值即为覆盖内容。不可与其他操作混用 |
| 数组前置 | `$prepend` | 数组 | 15% | 插入到数组开头 |
| 数组追加 | `$append` | 数组 | 10% | 插入到数组末尾 |
| 条件过滤 | `$filter` | 数组 | 8% | 保留匹配元素，**仅限静态字段** |
| 映射变换 | `$transform` | 数组 | 5% | 对每个元素应用变换（重命名等） |
| 条件删除 | `$remove` | 数组 | 4% | 删除匹配元素，**仅限静态字段**（与 $filter / $transform 同等约束） |
| 默认值 | `$default` | 字典 | 3% | 仅当字段不存在或为 null 时写入。空数组 `[]` 和空字典 `{}` 不触发（它们是有效值）。可与 `$filter` 等数组操作混用 |

### 2.6 静态字段白名单

`$filter`、`$transform` 和 `$remove` 中的表达式**只能引用以下字段**。引用运行时字段会在编译期报错。

```rust
/// 配置生成阶段（$filter / $transform）允许引用的静态字段
const STATIC_PROXY_FIELDS: &[&str] = &[
    // 基础标识
    "name", "type", "server", "port",
    // 认证参数
    "uuid", "password", "cipher",
    // TLS 参数
    "tls", "sni", "skip-cert-verify", "fingerprint", "alpn",
    // 传输参数
    "network", "ws-opts", "grpc-opts", "h2-opts", "reality-opts",
    // 协议参数
    "flow", "username", "alterId", "protocol",
    // 插件相关
    "plugin", "plugin-opts",
    // UDP 相关
    "udp", "udp-over-tcp",
    // 多路复用
    "smux",
    // 其他常见字段
    "servername", "client-fingerprint",
    "shadow-tls", "hy2-opts",
];

/// 运行时字段 — 编译器会拒绝在 $filter / $transform 中引用
/// 延迟、速度等数据由 Smart Selector 在运行时处理
const RUNTIME_PROXY_FIELDS: &[&str] = &[
    "delay", "latency", "speed", "loss_rate", "success_rate",
    "history", "alive", "last_test",
];
```

**编译期校验实现（正则 + 字符串剥离 + 模板字符串嵌套跟踪）**：

> **设计决策**：rquickjs（当前 0.11，基于 quickjs-ng）不暴露 AST 解析 API，因此采用正则 + 字符串/注释剥离的方案。
> 先通过 `strip_strings_and_comments()` 移除字符串字面量和注释（保留模板字符串 `${expr}` 内的表达式），
> 再用正则提取 `p.xxx`、`p['xxx']`、`p["xxx"]` 三种形式的字段引用。
> 这保证了 `p.name.includes('delayed')` 中的 `delayed` 不会被误杀。

**模板字符串嵌套深度跟踪**：

在处理反引号模板字符串（`` ` ``）时，`${...}` 内的表达式可能包含嵌套的 `{}`（如对象字面量）或字符串字面量（其中 `}` 不应被视为闭合括号）。解析器引入 `depth` 计数器跟踪 `${}` 嵌套深度，同时维护 `in_expr_string` 状态标记是否在字符串字面量中。遇到 `"` 或 `'` 时进入字符串模式，在字符串模式下 `}` 不减少 depth；仅当 depth 归零时才视为 `${}` 闭合。

**模板字符串内转义字符处理**：

模板字符串内的反斜杠转义序列（如 `` \` ``、`\$`、`\n`、`\t`、`\uXXXX`）被正确处理。遇到 `\` 时跳过反斜杠和下一个字符（各用空格占位），防止转义字符后的字符被误解析（例如 `` \` `` 被当作模板字符串结束符）。该处理覆盖三个上下文：模板字符串的非 `${}` 部分、`${}` 内部的字符串上下文、`${}` 内部的非字符串上下文。

```rust
/// 从表达式中提取所有 p.xxx / p['xxx'] / p["xxx"] 形式的字段引用
/// 先剥离字符串字面量和注释，再匹配纯代码部分
fn extract_member_access_fields(expr: &str) -> Vec<String> {
    let cleaned = strip_strings_and_comments(expr);
    // 匹配 p.xxx（点号）、p['xxx']（单引号括号）、p["xxx"]（双引号括号）
    let re = Regex::new(r#"(?x)
        \bp\.([a-zA-Z_$][a-zA-Z0-9_$]*)
        | \bp\[\s*'([^']+)'\s*\]
        | \bp\[\s*"([^"]+)"\s*\]
    "#).unwrap();
    // ... 提取并去重
}

fn validate_field_references(expr: &str) -> Result<(), CompileError> {
    let fields = extract_member_access_fields(expr);
    for field in &fields {
        if RUNTIME_PROXY_FIELDS.contains(&field.as_str()) {
            return Err(CompileError::RuntimeFieldInStaticFilter {
                field: field.clone(),
                hint: format!(
                    "`{}` is a runtime field. Use Smart Selector for latency-based selection.",
                    field
                ),
            });
        }
    }
    Ok(())
}
```

**编译期错误示例**：

```
Error: Runtime field in static filter
  → proxies: $filter "p.name.includes('香港') && p.delay < 200"
                                                        ^^^^^
  `delay` is only available after speed testing.
  It cannot be used during config generation.

  For latency-based node selection, use Smart Selector:
    → smart.toml: type = "ema", filter = "name.includes('香港')"

  Note: p.name.includes('delayed') would NOT trigger this error —
  we strip string literals before matching, so 'delayed' inside a string
  is not treated as a field reference.
```

### 2.7 `$default` 触发条件（边界定义）

| 字段状态 | 是否触发 `$default` | 理由 |
|---------|:------------------:|------|
| 字段不存在 | ✅ 触发 | 最常见的场景 — 为缺失配置提供兜底值 |
| 字段值为 `null` | ✅ 触发 | null 等同于不存在 |
| 字段为空数组 `[]` | ❌ 不触发 | 空数组是有效值，可能代表用户有意"清空该列表" |
| 字段为空字典 `{}` | ❌ 不触发 | 空字典是有效值，同上 |

> **混用规则**：`$default` 可与 `$filter`、`$transform`、`$prepend`、`$append` 等数组操作在同一键下混用。此时 `$default` 作为复合操作的一部分，按固定执行顺序参与排序（位于 `$append` 之后、`DeepMerge` 之前）。

```yaml
# 示例：原始配置中 rules 为空数组（用户故意清空）
rules: []

# 以下 $default 不会生效，因为 [] 是有效值
rules:
  $default:
    - MATCH,PROXY
# 结果：rules 仍然是 []
```

### 2.8 `$transform` 运行时校验（防误伤机制）

用户可能漏写 `...p`（展开原始对象），导致变换结果丢失必要字段。引擎在执行 `$transform` 后进行**运行时校验**，对前 N 个变换结果做字段完整性检查，发出 Warning 而非阻断执行。

**为什么检查前 N 个而非仅检查第一个？**

条件分支可能导致不同节点走不同路径——第一个节点正常不代表后续节点安全：

```javascript
// 对香港节点正常（展开 ...p），对其他节点异常（只保留 name）
p => p.name.startsWith("香港") ? {...p, name: "🇭🇰 " + p.name} : {name: p.name}
```

如果只检查第一个节点，恰好是香港节点则校验通过，但其他节点的 `type`、`server`、`port` 全丢失了。

```rust
/// 运行时校验 $transform 结果的字段完整性
/// 对前 min(节点数, 5) 个节点的变换结果逐一检查，覆盖更多分支路径
const TRANSFORM_VALIDATE_SAMPLE_SIZE: usize = 5;

fn validate_transform_results(
    original: &[Proxy],
    results: &[serde_json::Value],
) -> Vec<TransformWarning> {
    let sample_size = original.len().min(TRANSFORM_VALIDATE_SAMPLE_SIZE);
    let mut warnings = vec![];

    for i in 0..sample_size {
        if let Some(result) = results.get(i) {
            for required_field in &["name", "type", "server", "port"] {
                if result.get(required_field).is_none() {
                    warnings.push(TransformWarning::MissingRequiredField {
                        node_index: i,
                        node_name: original[i].name.clone(),
                        field: required_field.to_string(),
                        hint: "Did you forget to spread the original proxy? \
                               Use ({...p, name: ...}) instead of ({name: ...})."
                            .to_string(),
                    });
                }
            }
        }
    }

    warnings
}
```

**Warning 示例**：

```
Warning: Missing required field in $transform result
  → proxies: $transform "(p => ({ name: '🇭🇰 ' + p.name }))"
                                              ^^^^^^^^
  The transform result is missing required field: type
  Original fields: name, type, server, port, uuid, ...
  Result fields: name

  Did you forget to spread the original proxy?
  Fix: use ({...p, name: '🇭🇰 ' + p.name}) instead of ({name: ...})
```

---

## 3. Patch IR — 统一中间表示

### 3.1 核心数据结构

```rust
/// 依赖引用类型（区分文件名引用和运行时 ID 引用）
/// DSL 层用户写的是文件名，IR 层需要解析为运行时 ID
#[derive(Debug, Clone)]
pub enum DependencyRef {
    /// 文件名引用（DSL 层用户写的，如 "base-dns"）
    /// Patch Compiler 负责将其解析为对应文件生成的 PatchId
    /// 匹配规则：完整文件名或去掉 .prism.yaml 后缀的名称
    /// 如 "00-base-dns" 或 "base-dns" 都能匹配 "00-base-dns.prism.yaml"
    FileName(String),

    /// 运行时 ID 引用（脚本/插件动态生成的 Patch 之间的依赖）
    PatchId(PatchId),
}

/// Prism Engine 的核心抽象 — 统一配置变换
/// 所有输入（Prism DSL / 可视化 / 脚本 / 插件）都编译为此类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Patch {
    /// 唯一标识（自动生成）
    pub id: PatchId,

    /// 来源追踪（哪个文件、哪一行、哪个插件生成的）
    pub source: PatchSource,

    /// 作用域
    pub scope: Scope,

    /// 目标配置路径（如 "dns", "rules", "proxy-groups"）
    pub path: String,

    /// 操作类型
    pub op: PatchOp,

    /// 附加值（因 op 不同含义不同）
    pub value: serde_json::Value,

    /// 子操作列表（同一键下多个 $ 操作合并为子操作）
    /// 例如 $filter + $remove + $transform 在同一键下时，
    /// 会被编译为一个 Patch，其中包含多个 SubOp
    pub sub_ops: Vec<SubOp>,

    /// 执行条件（可选，仅当条件为 true 时执行此 Patch）
    pub condition: Option<CompiledPredicate>,

    /// 依赖（此 Patch 必须在指定目标之后执行）
    /// 不再提供 priority 字段，只用 after 声明依赖
    /// 同级无依赖的 Patch 按文件名字典序排列（确定性）
    ///
    /// 使用 DependencyRef 而非直接使用 PatchId，
    /// 因为 DSL 层用户写的是文件名引用（如 "base-dns"），
    /// 需要由 Patch Compiler 解析为运行时 PatchId。
    pub after: Vec<DependencyRef>,
}

/// 来源信息（用于 Trace View 和 Explain View）
#[derive(Debug, Clone)]
pub struct PatchSource {
    pub kind: SourceKind,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub plugin_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SourceKind {
    YamlFile,                     // 用户手写的 Prism DSL 文件
    VisualEditor { source: String }, // 可视化编辑器自动生成，source 为 GUI 接入方自定义名称
    Script { name: String },      // JS 脚本
    Plugin { id: String },        // 已安装的插件
    Builtin,                      // 引擎内建
}

/// 作用域（4 层）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Scope {
    /// 全局 — 对所有配置生效
    Global,

    /// 配置级 — 对指定 Profile 生效
    Profile(String),

    /// 条件作用域 — 仅当条件满足时生效
    Scoped {
        profile: Option<String>,          // Profile 名称或正则
        platform: Option<Vec<Platform>>,  // 操作系统
        core: Option<String>,             // 内核类型
        time_range: Option<TimeRange>,    // 时间段（结构化类型，如 "08:00-23:00"）
        enabled: Option<bool>,            // 是否启用（None 表示不限制，false 表示跳过）
        ssid: Option<String>,             // WiFi SSID 过滤（可选）
    },

    /// 运行时 — UI 设置（TUN 开关、DNS 模式等）
    Runtime,
}

/// 操作类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PatchOp {
    /// 递归深度合并（默认）
    DeepMerge,

    /// 强制替换（不递归）
    Override,

    /// 数组前置插入
    Prepend,

    /// 数组末尾追加
    Append,

    /// 条件过滤（保留匹配元素）
    Filter {
        expr: CompiledPredicate,
        // referenced_fields 已移入 CompiledPredicate 内部
    },

    /// 映射变换（批量修改）
    Transform {
        expr: CompiledPredicate,
        // referenced_fields 已移入 CompiledPredicate 内部
    },

    /// 条件删除
    Remove {
        expr: CompiledPredicate,
        // referenced_fields 已移入 CompiledPredicate 内部
    },

    /// 仅当字段不存在时设置（默认值注入）
    SetDefault,
}

/// 子操作类型（同一键下多个 $ 操作的原子单元）
/// 当同一 YAML 键下声明了 $filter + $remove + $transform 等多个操作时，
/// Patch Compiler 会将它们编译为单个 Patch，其中 sub_ops 包含各操作的有序列表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubOp {
    /// 操作类型（如 Prepend、Append、SetDefault、Filter 等）
    pub op: PatchOp,
    /// 操作关联的值。
    /// - Prepend/Append: 要插入的元素数组
    /// - SetDefault: 要注入的默认值
    /// - Filter/Remove/Transform: `Value::Null`（表达式在 PatchOp 变体内）
    pub value: serde_json::Value,
}
```

### 3.2 执行追踪（Explain View 的数据基础）

**设计决策：只存差分，不存全量快照。**

原因：一个代理列表 500 节点 ≈ 500KB，存 before + after 快照 = 1MB/patch。10 个 patch 就是 10MB，100 个就是 100MB——不可接受。有了 Patch IR，Explain View 需要时随时可以 replay 到任意步骤，不需要全量快照。

**递归差分统计**：`count_merge_diff` 函数递归统计 DeepMerge 操作后 JSON 相对于原始值的差异。对于嵌套对象递归进入子键计算差异；对于数组和标量值直接比较；返回 `(modified_count, total_keys)` 元组，用于生成详细的执行 trace。

```rust
/// 每个 Patch 执行后的追踪记录（轻量级，无全量快照）
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionTrace {
    /// 对应的 Patch
    pub patch_id: PatchId,

    /// 来源信息（从对应 Patch 复制过来，方便查询时无需回查 Patch）
    pub source: PatchSource,

    /// 执行的操作
    pub op: PatchOp,

    /// 执行耗时（微秒）
    pub duration_us: u64,

    /// 是否匹配了条件（针对 Scoped Patch）
    pub condition_matched: bool,

    /// 执行摘要（替代 before/after 全量快照）
    pub summary: TraceSummary,

    /// 受影响的元素列表（只记录被增删改的，不记录未受影响的）
    pub affected_items: Vec<AffectedItem>,

    /// 大批量操作时的完整 item 描述列表（仅 append/prepend ≥ 100 条时填充）
    /// 使用 `Arc<[String]>` 避免 `last_traces` clone 时的深拷贝开销
    #[serde(skip)]
    pub bulk_items: Option<Arc<[String]>>,
}

/// 执行摘要
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub added: usize,      // 新增的元素数
    pub removed: usize,    // 删除的元素数
    pub modified: usize,   // 修改的元素数
    pub kept: usize,       // 未受影响的元素数
    pub total_before: usize, // 操作前总元素数
    pub total_after: usize,  // 操作后总元素数
}

/// 受影响的单个元素（只记录变化）
#[derive(Debug, Clone, Serialize)]
pub struct AffectedItem {
    pub index: usize,
    pub before: Option<String>,  // 简化的描述（如代理名称），非全量数据
    pub after: Option<String>,
    pub action: TraceAction,
}

#[derive(Debug, Clone, Serialize)]
pub enum TraceAction {
    Added,
    Removed,
    Modified,
}

/// 溯源查询 — "这条规则为什么在这里？"
/// 注意：这是 TraceManager 的方法，不是独立函数
/// source 直接在 Trace 上，不需要通过 patch_id 回查 Patch
impl TraceManager {
    pub fn explain_field(
        &self,
        field_path: &str,
        item_key: Option<&str>,  // 如 "DOMAIN-SUFFIX,youtube.com,PROXY"
    ) -> Vec<ExplainEntry> {
        self.traces
            .iter()
            .filter(|t| {
                // 只看条件匹配且成功的 trace
                t.condition_matched && t.source.file.is_some()
            })
            .filter(|t| {
                // 通过 TraceManager 的 trace_affects_path 方法检查路径影响
                self.trace_affects_path(t, field_path)
            })
            .filter(|t| {
                // 如果指定了 item_key，进一步过滤 affected_items
                if let Some(key) = item_key {
                    t.affected_items.iter().any(|item| {
                        let before_match = item.before.as_ref().is_some_and(|b| b.contains(key));
                        let after_match = item.after.as_ref().is_some_and(|a| a.contains(key));
                        before_match || after_match
                    })
                } else {
                    true
                }
            })
            .map(|t| ExplainEntry {
                source: t.source.clone(),            // 直接从 trace 取
                op_name: t.op.display_name(),
                detail: t.describe_change(),
            })
            .collect()
    }
}

pub struct ExplainEntry {
    pub source: PatchSource,
    pub op_name: String,
    pub detail: String,
}

/// 需要查看完整 before/after 数据时，通过 IR replay 实现
/// （不会为每个 trace 预存快照，只在用户主动查看时按需计算）
/// 注意：这也是 TraceManager 的方法
impl TraceManager {
    pub fn replay_at_step(
        &self,
        patches: &[Patch],
        target_step: usize,
        base_config: &serde_json::Value,
    ) -> serde_json::Value {
        let mut config = base_config.clone();
        for patch in &patches[..=target_step.min(patches.len() - 1)] {
            apply_patch(&mut config, patch);
        }
        config
    }
}
```

---

## 4. 处理管线

### 4.1 完整流程

```
[1] 输入收集
    ├── 并发下载多个订阅 (tokio::join_all)
    ├── 读取所有增强文件 (.prism.yaml)
    ├── 加载所有脚本 (.js)
    └── 加载所有插件 (manifest.json)

    ↓

[2] 编译
    ├── Prism DSL → Patch (解析 __when__ / __after__)
    ├── 可视化操作 → Patch (自动生成)
    ├── 脚本执行 → Patch (rquickjs 沙箱内执行，脚本返回 Patch 数组)
    ├── 静态字段 AST 校验 ($filter / $transform，基于标识符节点，非字符串匹配)
    ├── 跨阶段依赖验证：Profile Patch 不得依赖 Shared Patch（Phase 1 先于 Phase 2 执行）
    ├── 统一 Guarded 字段检查：所有操作类型统一检查受保护字段，非 DeepMerge/Override 操作命中时跳过并 warn
    ├── 依赖排序 (拓扑排序 __after__ 声明，同级按文件名字典序)
    └── 环检测增强：从所有未处理节点逐一尝试 DFS，确保找到真正的环路径

    ↓

[3] 执行（Profile 级并发）
    ├── 订阅 A: 下载 → 解析 → 应用 Profile 级 Patches
    ├── 订阅 B: 下载 → 解析 → 应用 Profile 级 Patches    ← 并发
    └── 订阅 C: 下载 → 解析 → 应用 Profile 级 Patches
    │
    └─→ 每个 Patch 执行时记录 ExecutionTrace
    │
    ↓ 合并

[4] 合并
    ├── 多 Profile 结果合并（去重、排序）
    ├── 应用 Global 级 Patches
    └── 应用 Scoped 级 Patches（条件判断）

    ↓

[5] 验证
    ├── 字段合法性校验（JSON Schema）
    ├── 代理名称唯一性检查
    ├── 代理组引用完整性检查（组内引用的代理必须存在）
    ├── DNS 配置完整性检查
    └── 智能建议（如"开了 TUN 但未开 fake-ip"）

    ↓

[6] 输出
    ├── 原子写入配置文件（先写临时文件再 rename）
    └── 通知内核热重载
```

### 4.2 并发策略

```
简单规则：Profile 级并发，Profile 内串行。

为什么不用 DAG？
  实际场景中增强配置的依赖关系极其简单：
    全局 → Profile → 内建
  95% 是线性执行，5% 有简单的前后依赖（用 __after__ 声明即可）。
  为 5% 的场景引入 DAG 调度器，增加拓扑排序 + 环检测 + 并发调度的复杂度，不划算。
  如果 v2 真有需求，Patch IR 已经预留了 after 字段，升级到 DAG 很简单。
```

### 4.3 Global 与 Profile 的 prepend 语义

**管线中的实际执行顺序**：Global Patches 在所有 Profile 合并**之后**才应用（见 §4.1 步骤 [4]）。这意味着 Global 的 `$prepend` 会插入到 Profile 规则的**前面**。

```yaml
# Profile A (profile-a.prism.yaml)
rules:
  $prepend:
    - DOMAIN,special.com,DIRECT

# Global (00-base.prism.yaml)
rules:
  $prepend:
    - DOMAIN,ads.com,REJECT
```

**执行过程**：

```
[3] Profile 级执行 → rules: [DOMAIN,special.com,DIRECT, ...原始规则...]
[4] 合并后应用 Global → rules: [DOMAIN,ads.com,REJECT, DOMAIN,special.com,DIRECT, ...原始规则...]
```

**结果**：Global 的 `$prepend` 排在 Profile 的 `$prepend` 前面。

**设计理由**：Global 代表"所有场景都应遵守的基础规则"（如广告拦截），应排在最前面被内核优先匹配。Profile 级规则是特定订阅的定制，放在后面允许被更具体的规则覆盖。

> 如果用户需要 Profile 规则优先于 Global 规则，可以在 Profile 中使用 `$append`（追加到末尾），避免与 Global 的 `$prepend` 冲突。
```

---

## 5. 脚本引擎

### 5.1 引擎选型：rquickjs（唯一）

| 备选方案 | 是否选用 | 理由 |
|---------|---------|------|
| **rquickjs** | ✅ 选用 | 基于 quickjs-ng，预生成标准平台 bindings（无需用户端 C 编译），ES2023+ 完整支持，交叉编译友好 |
| Boa | ❌ | ES6 兼容性差，`?.` `??` 可能不工作 |
| mlua | ❌ | Lua C 库依赖，交叉编译噩梦，用户多学一门语言 |
| Deno Core | ❌ | V8 引擎 +40MB 体积，C++ 依赖 |
| WASM (Wasmtime) | ❌ v1 不需要 | rquickjs 已够用，WASM 增加复杂度但 v1 无实际需求 |

### 5.2 脚本 API

```typescript
// prism.d.ts — 脚本类型定义

interface PrismContext {
    // ─── 配置读写 ───
    config: ClashConfig;

    // ─── 结构化工具（推荐使用，而非直接操作 config）───
    utils: {
        // 基础工具函数
        match(pattern: string, text: string): boolean;  // glob 模式匹配（与 Clash 规则语法一致，非 RegExp）
        includes(text: string, search: string): boolean;
        now(): number;            // 当前时间戳（毫秒）
        random(min: number, max: number): number;  // [min, max] 范围内的随机整数
        hash(input: string): string;  // 简单哈希（用于去重键生成）

        proxies: {
            filter(pred: (p: Proxy) => boolean): Proxy[];
            rename(pattern: RegExp, replacement: string): void;
            remove(pred: (p: Proxy) => boolean): void;
            sort(by: keyof Proxy, order?: "asc" | "desc"): void;
            deduplicate(by: string | string[]): void;
            groupBy(pattern: RegExp): Map<string, Proxy[]>;
        };
        rules: {
            prepend(...rules: string[]): void;
            append(...rules: string[]): void;
            insertAt(index: number, ...rules: string[]): void;
            remove(pred: (rule: string) => boolean): void;
            deduplicate(): void;
        };
        groups: {
            get(name: string): ProxyGroup | undefined;
            addProxy(groupName: string, ...proxyNames: string[]): void;
            removeProxy(groupName: string, ...proxyNames: string[]): void;
            create(group: ProxyGroup): void;
            remove(name: string): void;
        };
    };

    // ─── 生成 Patch（高级用法，用于条件化配置变换）───
    patch: {
        add(patch: PrismPatch): void;
    };

    // ─── 日志（输出到 Prism 日志面板）───
    log: {
        debug(msg: string, ...args: unknown[]): void;
        info(msg: string, ...args: unknown[]): void;
        warn(msg: string, ...args: unknown[]): void;
        error(msg: string, ...args: unknown[]): void;
    };

    // ─── KV 存储（跨脚本持久化）───
    store: {
        get<T>(key: string): T | undefined;
        set<T>(key: string, value: T): void;
        delete(key: string): void;
        keys(): string[];           // 返回所有键名
    };

    // ─── 环境信息（只读）───
    env: {
        coreType: "mihomo" | "clash-rs";
        coreVersion: string;
        platform: "windows" | "macos" | "linux";
        profileName: string;
    };
}

// 脚本入口
function main(ctx: PrismContext): void | Promise<void>;
```

### 5.3 脚本示例

```typescript
// smart-grouping.js — 按地区自动分组
// @prism-scope: subscribe
// @prism-permissions: store

function main(ctx: PrismContext) {
    const { proxies, groups, log } = ctx.utils;

    // 过滤无效节点
    proxies.remove(p => !p.server || p.port <= 0 || p.port > 65535);

    // 重命名
    proxies.rename(/^港/, "🇭🇰 香港");
    proxies.rename(/^日/, "🇯🇵 日本");
    proxies.rename(/^美/, "🇺🇸 美国");
    proxies.rename(/^新/, "🇸🇬 新加坡");

    // 按国旗分组
    const regions = proxies.groupBy(/^(🇭🇰|🇯🇵|🇺🇸|🇸🇬)/);

    for (const [region, nodes] of regions) {
        const groupName = `${region} Auto`;

        groups.create({
            name: groupName,
            type: "url-test",
            proxies: nodes.map(p => p.name),
            url: "http://www.gstatic.com/generate_204",
            interval: 300,
            tolerance: 50,
        });

        groups.addProxy("PROXY", groupName);
        log.info(`Created group: ${groupName} (${nodes.length} nodes)`);
    }
}
```

### 5.4 脚本安全限制

```rust
pub struct ScriptLimits {
    pub max_execution_time_ms: u64,   // 最大执行时间（毫秒，默认 5000）
    pub max_memory_bytes: usize,      // 最大内存（默认 50MB）
    pub max_output_size_bytes: usize, // 最大输出大小（默认 1MB）
    pub max_log_entries: usize,       // 最大日志条数（默认 500）
    pub max_script_size_bytes: usize, // 最大脚本大小（默认 10MB）
    pub max_config_bytes: usize,      // 最大配置大小（默认 10MB）
    pub max_string_length: usize,     // 单字符串最大长度（默认 1MB，防内存炸弹）
    pub max_loop_iterations: u64,     // 最大循环迭代次数（默认 100_000，防死循环）
    pub max_recursion_depth: u32,     // 最大递归深度（默认 32，防栈溢出）
}
```

**沙箱加固（Unicode 预处理 + 五层检测 + 运行时加固）**：

**预处理**：将脚本中的 `\uXXXX` 和 `\xXX` 转义序列转换为实际 Unicode 字符，防止通过转义绕过检测（如 `\u0065val` → `eval`）。

1. **Layer 1 — 原始子串检查**：拒绝包含 `eval(`、`require(`、`Function(`、`import(` 的脚本
2. **Layer 2 — 词法分析**：剥离字符串/注释后检测危险标识符（含括号访问 `globalThis['eval']` 等绕过方式）
3. **Layer 3 — 方括号访问检测**：检测 `this["eval"]`、`globalThis["Function"]` 等变体
4. **Layer 4 — 模板字面量检测**：检测 `` globalThis[`eval`] `` 等模板构造
5. **Layer 4.5 — 模板字面量拼接检测**：检测 `` `ev` + `al` `` 等通过拼接构造危险标识符的模式

**运行时加固（5 层纵深防御，兼容 quickjs-ng）**：

注意：不使用 `Object.freeze(globalThis)`，因为 quickjs-ng（rquickjs 0.9+）中冻结 globalThis 会导致 `var` 声明抛异常。替代方案：

1. **保存 Function.prototype 引用**：在删除 `Function` 前，将 `Function.prototype`、`Function.prototype.call`、`Function.prototype.apply` 保存到临时全局变量（因为 quickjs-ng 中 `delete globalThis.Function` 后 `Function` 变为 `undefined`，导致 `Function.prototype` 不可访问）
2. **删除危险属性**：`delete globalThis.eval/Function/require`
3. **Per-property 不可配置属性描述符**：对 19 个危险属性（`eval`、`Function`、`require`、`process`、`module`、`exports`、`__dirname`、`__filename`、`global`、`Buffer`、`child_process`、`fs`、`net`、`http`、`https`、`dlopen`、`WebAssembly`、`Proxy`、`Symbol`）逐一设置 `Object.defineProperty(globalThis, name, { get: throw, set: throw, configurable: false })`。列表与 builtins 数组保持语义对齐
4. **原型链 constructor 访问阻断**：对所有内置构造器的 `prototype.constructor` 设置不可配置的 getter（抛异常）
5. **strict mode**：用户脚本在 `'use strict'` 下执行，禁止 `arguments.callee`（阻止匿名函数递归绕过深度计数器）、禁止未声明全局变量赋值、禁止 `with` 语句、`this` 默认为 `undefined`（非 globalThis）

**不平衡括号防护**：`split_function_args` 在解析函数参数时，检测 `depth < 0` 的情况（多余的闭合括号 `)`），输出 warn 日志（包含具体字符、位置和原始输入）并终止解析，避免产生错误的参数分割结果。

**模板字符串转义处理**：沙箱的 Layer 4 模板字面量检测与 DSL 解析器的模板字符串转义处理（§2.6）协同工作，确保 `` globalThis[`eval`] `` 等通过模板构造的恶意访问被正确检测和拦截。

---

## 6. 插件体系

### 6.1 两类插件（最小权限原则）

**Config Plugin（配置插件）**

| 属性 | 说明 |
|------|------|
| 运行环境 | 后端 rquickjs 沙箱 |
| 能力 | 读写配置、注册生命周期钩子、使用 KV 存储、请求测速（proxy:test，由引擎代为发起） |
| 禁止 | 访问文件系统、直接访问网络、访问前端 DOM |
| 用途 | 配置变换、节点重命名、规则注入、智能分组 |

> **UI Extension**（界面扩展）的类型定义和权限系统已在代码中预留（`PluginType::Ui`、`Permission::Ui*`），实际 iframe 沙箱运行时由 GUI 接入方实现，不在本引擎范围内。

### 6.2 插件清单（manifest.json）

```json
{
    "id": "smart-grouping",
    "name": "智能分组",
    "version": "1.2.0",
    "type": "config",

    "permissions": [
        "config:read",
        "config:write",
        "store:readwrite"
    ],

    "hooks": ["onSubscribeParsed"],

    "entry": "main.js",

    "scope": "subscribe",
    "timeout": 5000,

    "author": "user",
    "description": "按地区自动分组代理节点"
}
```

### 6.3 权限清单

| 权限 | 说明 | Config Plugin | UI Extension |
|------|------|:------------:|:------------:|
| `config:read` | 读取配置 | ✅ | ✅ (受限) |
| `config:write` | 修改配置 | ✅ | ❌ |
| `proxy:test` | 请求测速 | ✅ (由引擎代为发起) | ❌ |
| `proxy:select` | 切换代理 | ✅ | ✅ |
| `store:readwrite` | KV 持久存储 | ✅ | ✅ |
| `network:outbound` | 外部网络请求 | ❌ (v1 不可申请) | ✅ |
| `ui:notify` | 显示通知 | ✅ | ✅ |
| `ui:dialog` | 弹出对话框 | ❌ | ✅ |
| `ui:page` | 注册自定义页面 | ❌ | ✅ |
| `ui:tray` | 托盘图标/菜单 | ❌ | ✅ |

**权限设计说明**：
- `network:outbound` v1 仅限 UI Extension 申请。Config Plugin 需要下载外部资源时，应通过 `onSubscribeFetch` 钩子由引擎代为请求，而非 Plugin 直接建连。
- `proxy:test` Config Plugin 可声明，但测速连接由引擎执行（返回结果），Plugin 不直接建立网络连接。

### 6.4 多组件插件架构

插件可包含多种组件类型，通过 `ComponentManifest` 声明，实现"一个插件包，多种能力"：

```rust
/// 插件组件清单（嵌入 manifest.json 的 "components" 字段）
#[derive(Debug, Serialize, Deserialize)]
pub struct ComponentManifest {
    /// DSL 增强文件列表（.prism.yaml）
    #[serde(default)]
    pub patches: Vec<String>,
    /// JS 脚本文件列表（.js）
    #[serde(default)]
    pub scripts: Vec<String>,
    /// 生命周期钩子映射（事件名 → 脚本文件）
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
    /// 配置模板文件列表
    #[serde(default)]
    pub templates: Vec<String>,
    /// 评分算法文件列表
    #[serde(default)]
    pub scorers: Vec<String>,
    /// 校验规则文件列表
    #[serde(default)]
    pub validators: Vec<String>,
}
```

**加载策略（软失败）**：单个组件文件不存在时记录警告并跳过，不中断整体加载。`LoadedComponents` 保留完整的 `load_warnings` 诊断信息。

```json
{
    "id": "ultimate-proxy",
    "name": "终极代理增强",
    "version": "2.0.0",
    "type": "config",
    "components": {
        "patches": ["rules/prism-enhance.yaml"],
        "scripts": ["smart-grouping.js"],
        "hooks": { "onSubscribeParsed": "hooks/filter.js" },
        "templates": ["templates/base.yaml"],
        "scorers": ["scorers/custom-ema.js"],
        "validators": ["validators/check-fields.js"]
    }
}
```

---

## 7. 生命周期钩子

### 7.1 8 + 1 个钩子 + 1 个 Rust 原生策略

```rust
pub enum Hook {
    // ─── 配置生命周期（6 个）───
    OnSubscribeFetch,      // 订阅下载时（可替换 URL、解密内容、预处理响应）
    OnSubscribeParsed,     // 单个订阅解析完成后（可过滤/重命名节点）
    OnMerged,              // 多订阅合并完成后（可跨订阅去重/统一分组）
    OnBeforeWrite,         // 写入配置文件前（最后修改机会）
    OnBeforeCoreStart,     // 内核启动前（端口清理、环境检测）
    OnCoreStopped,         // 内核停止后（清理资源）

    // ─── 应用生命周期（2 个）───
    OnAppReady,            // 应用启动完成（初始化插件）
    OnShutdown,            // 应用关闭前（保存状态、释放资源）

    // ─── 事件驱动（1 个）───
    OnSchedule(String),    // Cron 表达式定时触发（5 字段标准格式）
}

// 注意：OnNodeFail 不在此枚举中，由 Rust 原生实现（见 §7.2）
```

### 7.2 Rust 原生：OnNodeFail（不走 JS）

```rust
/// 节点连续失败自动切换 — 由 Rust 原生实现，不暴露给 JS 插件
///
/// 为什么不走 JS 回调？
///   连接建立/断开是毫秒级高频事件
///   JS 回调会有 ~1ms 的额外延迟
///   在高并发连接场景下会显著影响性能
///   而且节点切换逻辑很简单，不需要脚本灵活性
pub struct NodeFailPolicy {
    /// 是否启用节点失败自动切换（默认 true）
    pub enabled: bool,

    /// 连续失败多少次后触发切换
    pub threshold: u32,       // 默认 3

    /// 切换到哪个组/节点
    pub fallback_group: String, // 默认当前组的下一个节点

    /// 冷却时间（避免频繁切换）
    pub cooldown: Duration,   // 默认 30 秒
}
```

**v1 钩子设计原则**：只保留"低频、高价值、需要灵活性"的钩子。高频事件（连接建立/断开、节点测速）由 Rust 原生处理，不暴露给 JS。

### 7.3 Hook 结果聚合与条件过滤

当多个插件注册同一钩子事件时，引擎通过 `AggregatedHookResult` 聚合所有钩子的执行结果：

```rust
/// 单个钩子执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub hook_name: String,
    pub success: bool,
    pub messages: Vec<String>,
    pub blocking_errors: Vec<String>,
    pub prevent_continuation: bool,      // 阻止后续钩子执行
    pub modified_config: Option<Value>,  // 修改后的配置（链式传递）
}

/// 聚合结果（合并所有钩子的执行记录）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AggregatedHookResult {
    pub messages: Vec<String>,
    pub blocking_errors: Vec<String>,
    pub prevent_continuation: bool,
    pub modified_config: Option<Value>,  // 链式配置传递
    pub hook_results: Vec<HookResult>,   // 完整审计链
}
```

**核心机制**：

- **链式配置传递**：`modified_config` 在钩子链中逐级传递，后续钩子基于前一钩子的修改结果工作
- **阻止传播**：任一钩子设置 `prevent_continuation` 后，后续钩子不再执行，但结果仍被收集用于审计
- **完整审计链**：`hook_results` 保留每个钩子的完整执行记录，`report()` 生成详细报告

**条件过滤（HookCondition）**：

钩子可通过 `HookCondition` 声明执行条件，仅在条件满足时触发：

```rust
/// 钩子条件过滤器
pub struct HookCondition {
    pub expression: String,  // 条件表达式
}

/// 钩子执行上下文
pub struct HookContext {
    pub event: String,                    // 事件名称
    pub modified_paths: Vec<String>,      // 被修改的配置路径
    pub patch_count: usize,               // Patch 数量
    pub extra: BTreeMap<String, String>,  // 扩展键值对
}
```

支持四种条件模式：
- **精确事件名**：`dns_changed` — 仅当事件名完全匹配时触发
- **数值比较**：`patch_count > 5` — Patch 数量超过阈值时触发
- **事件名比较**：`event == OnSubscribeParsed` — 事件名等值判断
- **路径包含**：`modified_paths contains proxies` — 被修改的路径包含关键词时触发

> 无法解析的条件表达式默认返回 `false`（安全失败），不会导致 panic。

---

## 8. Smart Selector — 独立运行时模块

### 8.1 为什么独立

Smart Selector 与配置增强管线**完全解耦**：

- 配置增强运行在**配置生成阶段**（静态数据）
- Smart Selector 运行在**内核运行阶段**（动态数据）

两者不共享数据通道。配置增强只负责生成 `type: smart` 的代理组声明，实际的节点评分和切换由 Smart Selector 在运行时完成。

### 8.2 配置文件：smart.toml

```toml
[score]
type = "ema"                    # 评分算法：ema（唯一选项，v1 无 ML）

[score.weights]
latency_p90 = 0.4               # P90 延迟权重（非平均延迟）
success_rate = 0.4              # 成功率权重
stability = 0.2                 # 稳定性（延迟标准差）权重

[score.decay]
# 时间衰减系数：1 小时前的数据权重衰减到 80%
half_life_hours = 1.0

[scheduler]
# 自适应测速：网络好时降低频率，网络差时提高频率
base_interval_secs = 300       # 基础测速间隔（秒）
adaptive = true                 # 启用自适应

[scheduler.adaptive]
# 网络质量 > 0.9 时：间隔 × 3
# 网络质量 < 0.3 时：间隔 × 0.25
good_quality_threshold = 0.9
bad_quality_threshold = 0.3

[proxy-groups.auto]
# 自动生成 smart 类型的代理组
filter = "name.includes('香港')"   # 基于**静态字段**预筛选
url = "http://www.gstatic.com/generate_204"
interval = 300
tolerance = 50
```

### 8.3 在 YAML 中使用

```yaml
# 只需声明 type: smart，其他由 smart.toml 控制
proxy-groups:
  - name: 智能优选
    type: smart          # 触发 Smart Selector
    # 可选：覆盖全局 filter
    filter: "name.includes('IPLC')"
```

### 8.4 评分公式

```rust
impl SmartScorer {
    pub fn score(&self, node: &NodeHistory) -> f64 {
        // P90 延迟分数（而非平均延迟，对抗偶发高延迟假象）
        let latency_score = match node.p90_latency {
            d if d < 50.0 => 100.0,
            d if d < 200.0 => 100.0 - (d - 50.0) * 0.4,
            d => (40.0 - (d - 200.0) * 0.05).max(0.0),
        };

        // 稳定性分数（标准差越小越稳定）
        let stability_score = (100.0 - node.latency_stddev * 0.5).max(0.0);

        // 时间衰减（EMA）
        let time_weight = (-self.decay_coefficient
            * (now - node.last_test) / 3600000.0).exp();

        // 加权综合
        let base = latency_score * self.weights.latency_p90
            + node.success_rate * 100.0 * self.weights.success_rate
            + stability_score * self.weights.stability;

        base * time_weight
    }
}
```

**除零保护**：评分公式计算 `weight_sum` 后先检查是否 `<= 0.0`。若所有权重均为零或负数，输出 warn 日志并返回安全默认值 `0.0`，避免除零异常。

**安全算术运算**：历史记录中方差累加使用 `f64::saturating_add` 防止溢出，`trim` 方法使用 `saturating_sub` 防止整数下溢，成功率计算使用 `checked_div` 纵深防御。

**体积：纯 Rust，< 100 行核心代码，零外部依赖。**

---

## 9. 作用域系统

### 9.1 四层作用域

| 层级 | 作用域 | 条件声明 | 说明 |
|------|--------|---------|------|
| 0 | **Global**（全局） | 不写 `__when__` 即默认 | 对所有配置生效 |
| 1 | **Profile**（配置级） | `__when__: { profile: "机场A" }` | 对指定 Profile 生效 |
| 2 | **Scoped**（条件作用域） | `__when__: { platform: macos, core: mihomo }` | 仅当条件满足时生效 |
| 3 | **Runtime**（运行时） | 不在增强文件中 | UI 设置层的快速开关（TUN/DNS 模式等），由 UI 状态直接驱动 |

### 9.2 物理执行顺序与覆盖语义

**物理执行顺序**（管线中的实际处理顺序，对应 §4.1 步骤 [3]-[4]）：

```
Phase 1 — Profile 级并发执行（§4.1 步骤 [3]）
  Profile Patches → 处理各订阅的原始节点（过滤、重命名、分组）

Phase 2 — 合并后叠加（§4.1 步骤 [4]）
  多 Profile 结果合并 → Global Patches → Scoped Patches → Runtime Patches
```

**关键理解**：Profile Patches 先执行（处理原始订阅数据），Global/Scoped/Runtime Patches 在合并后才叠加。这保证了：

- Profile 级的节点过滤/重命名在原始数据上操作，不受 Global 干扰
- Global 规则（如广告拦截）最终 prepend 到所有规则最前面，被内核优先匹配
- Runtime（UI 当前开关）拥有最终决定权

**覆盖语义**（同一字段被多次修改时，谁说了算）：

```
后执行的覆盖先执行的。
Runtime 最后执行 → 它的值覆盖所有之前的设置。
```

| 作用域层 | 物理执行阶段 | 覆盖关系 | 说明 |
|---------|------------|---------|------|
| Profile | Phase 1 — 先执行 | 被 Global/Scoped/Runtime 覆盖 | 处理原始订阅数据 |
| Global | Phase 2 — 合并后第 1 个 | 被 Scoped/Runtime 覆盖 | 基础规则兜底（如广告拦截） |
| Scoped | Phase 2 — 合并后第 2 个 | 被 Runtime 覆盖 | 条件化定制 |
| Runtime | Phase 2 — 最后执行 | **覆盖所有** | UI 当前状态，用户选择 > 预配置 |

> **注意**：Phase 2 内部 Global → Scoped → Runtime 的顺序意味着 Global 的 `$prepend` 会出现在 Profile 规则**前面**（详见 §4.3 示例）。这是设计意图：Global 规则代表"所有场景都应遵守的基础规则"，应被内核优先匹配。

---

## 10. 调试系统

### 10.1 数据层（已实现）

调试系统的数据层已在 `clash-prism-core` 中完整实现，CLI 通过 `full_report()` 输出文本格式报告，GUI 接入方可基于 `ExecutionTrace` 数据自行渲染可视化界面。

| 组件 | 说明 | 代码位置 |
|------|------|---------|
| `ExecutionTrace` | 单个 Patch 的执行记录（patch_id, source, op, duration, condition_matched, summary, affected_items, bulk_items） | `trace.rs` |
| `TraceSummary` | 操作统计（added, removed, modified, kept, total_before, total_after） | `trace.rs` |
| `AffectedItem` | 受影响的单个元素（index, before, after, action） | `trace.rs` |
| `TraceManager::explain_field()` | 溯源查询：哪些 Patch 修改了指定字段 | `trace.rs` |
| `TraceManager::replay_at_step()` | 按步回放：重放到第 N 步时的配置状态 | `trace.rs` |
| `TraceManager::diff_view_report()` | Diff View：配置变更总览报告 | `trace.rs` |
| `TraceManager::full_report()` | 完整报告：包含所有 Trace 的详细文本输出 | `trace.rs` |

### 10.2 三种视图（概念说明）

以下视图由 GUI 接入方基于数据层自行渲染：

**Diff View** — 配置变更总览（哪些字段被修改/新增/删除）

**Trace View** — 变更来源追踪（指定字段/规则经历了哪些 Patch 操作）

**Explain View** — 字段溯源（指定代理/规则的完整生命周期）

### 10.3 性能追踪器（PerfTracker）

引擎内置性能追踪模块，用于管线各阶段的耗时测量和报告：

```rust
/// 单阶段性能指标
pub struct PhaseMetric {
    pub name: String,
    pub duration_us: u64,
}

/// 性能追踪器
pub struct PerfTracker {
    metrics: Vec<PhaseMetric>,
}

impl PerfTracker {
    /// 执行闭包并自动计时（高阶函数模式，调用方零侵入）
    pub fn measure<F, R>(&mut self, name: &str, f: F) -> R { ... }

    /// 手动记录阶段耗时
    pub fn record(&mut self, name: &str, duration: Duration) { ... }

    /// 生成对齐的人类可读报告（千分位分隔、右对齐数值）
    pub fn report(&self) -> String { ... }

    /// 总耗时
    pub fn total_duration(&self) -> Duration { ... }
}
```

**设计特点**：
- **高阶函数模式**：`measure()` 接受 `FnOnce` 闭包，自动包裹计时逻辑
- **Display trait**：可直接 `println!("{}", tracker)` 输出报告
- **对齐格式化**：自动计算阶段名称最大宽度，右对齐耗时数值，千分位分隔

**使用示例**：

```rust
let mut tracker = PerfTracker::new();

let config = tracker.measure("download", || download_subscriptions(&urls));
let patches = tracker.measure("compile", || compile_pipeline(&config));
let result = tracker.measure("execute", || execute_patches(&patches));

println!("{}", tracker.report());
// 输出：
//   download    1,234,567 µs
//   compile       456,789 µs
//   execute        12,345 µs
//   ─────────────────────
//   total       1,703,701 µs
```

---

## 11. 技术栈总结

| 组件 | 技术 | 理由 |
|------|------|------|
| 桌面框架 | Tauri 2 | 轻量、安全、跨平台 |
| 后端语言 | Rust | 性能、安全、类型安全 |
| 前端框架 | Vue 3 + TypeScript | 轻量、TypeScript 类型安全 |
| 脚本引擎 | rquickjs (quickjs-ng) | 预生成 bindings、ES2023+、无需用户端 C 编译 |
| KV 存储 | redb | 纯 Rust、ACID、嵌入式 |
| 异步运行时 | tokio | Rust 标准异步框架 |
| 序列化 | serde + serde_yml | YAML/JSON 处理 |
| 定时任务 | tokio-cron-scheduler | Cron 表达式调度 |
| 构建工具 | Vite (前端) + Cargo (后端) | 快速、成熟 |

### 11.1 基础设施模块

以下模块位于 `clash-prism-core` crate 中，为引擎提供底层支撑能力：

#### 配置迁移系统（migration.rs）

```rust
/// 迁移接口（Send + Sync，支持异步上下文）
pub trait Migration: Send + Sync {
    fn name(&self) -> &'static str;
    fn target_version(&self) -> u32;
    fn apply(&mut self, config: &mut Value) -> Result<bool, String>;
}

/// 按版本号升序执行迁移，返回审计报告
pub fn run_migrations(config: &mut Value, migrations: &[Box<dyn Migration>]) -> Vec<MigrationReport>;
```

**设计原则**：
- **幂等性守卫**：每个 `apply` 自行检查是否已执行，返回 `Ok(false)` 表示跳过
- **版本守卫**：仅执行 `target_version > current_version` 的迁移，失败时停止链式推进
- **有序确定性**：按 `target_version` 升序排序后执行，不依赖注册顺序
- **审计追踪**：每次迁移返回 `MigrationReport`（名称、耗时、是否实际执行）

#### 三级缓存架构（cache.rs）

```
L1 内存缓存 (MemoryCache<V>)
  ├── mtime 自动失效（关联文件路径，get 时比对修改时间）
  ├── FIFO 淘汰（超出 max_size 时移除最早条目）
  └── 适用于：编译结果、解析缓存

L2 磁盘缓存 (DiskCache)
  ├── 原子写入（temp + sync_all + rename，崩溃安全）
  ├── 内容寻址（DefaultHasher 哈希文件名）
  ├── 过期清理（cleanup 按文件年龄清理）
  └── 适用于：编译缓存持久化、大文件缓存

L3 分布式缓存（接口预留）
  └── 基于 redb KV，接口已规划但未实现
```

#### Extension 层缓存（extension.rs）

针对 GUI 集成场景的大规模规则优化（v0.1.2 新增）：

```
Annotation 缓存
  ├── apply() 成功后缓存 rule_annotations 到 ExtensionState 和 WatchResult
  ├── get_current_annotations() 优先从缓存读取，O(A×R) → O(1)
  └── 基于 compile_success 标志判断缓存有效性

文件级解析缓存
  ├── SHA-256 内容哈希比对，未变更文件跳过 DSL 解析
  ├── 缓存存储：HashMap<String, (hash, Vec<Patch>)>
  └── 重复编译速度提升 50-70%

output_config Arc 共享
  ├── ExtensionState.last_output 和 WatchResult.output 共享同一 Arc<Value>
  └── 省掉一次数 MB 的 JSON 深拷贝

is_prism_rule HashMap 索引
  ├── HashMap<usize, usize>：index_in_output → annotations Vec 索引
  └── O(N) 线性扫描 → O(1) 查询

Patch 引用传递
  ├── execute() / execute_pipeline() 签名改为 &[&Patch]
  ├── compile_and_execute_pipeline() 中按 scope 分类时使用引用
  └── execute_owned() 便捷方法保持向后兼容
```

**设计原则**：所有缓存都有明确的失效时机（apply 成功更新、insert_rule 清空），不会出现缓存与实际数据不一致的情况。

```rust
/// 基于文件内容哈希的缓存键（内容寻址）
pub fn compile_cache_key(file_path: &Path) -> Result<String, String>;
```

#### 确定性序列化（serial.rs）

```rust
/// 美化格式，key 按字典序排列（人类可读）
pub fn deterministic_serialize(value: &Value) -> String;

/// 紧凑格式，key 按字典序排列（缓存键/内容寻址）
pub fn deterministic_serialize_compact(value: &Value) -> String;

/// 排序后紧凑序列化 + DefaultHasher 哈希
pub fn config_content_hash(value: &Value) -> u64;
```

**设计原则**：递归全深度排序（BTreeMap 保证字典序），相同语义的 JSON 永远产生相同的哈希值，确保缓存稳定性。

#### Unicode 安全清洗（sanitize.rs）

```rust
/// 迭代 NFKC 归一化 + 危险字符移除（最多 10 次迭代）
pub fn sanitize_config_string(input: &str) -> String;

/// BOM 感知的配置文件读取
pub fn read_config_file(path: &Path) -> Result<String, io::Error>;
```

**危险字符覆盖范围**：
- 零宽字符（U+200B-200D）
- 方向控制（U+200E-200F, U+202A-202E）
- BOM（U+FEFF）
- 私用区（3 个范围）
- 非字符码点（U+FFFE/U+FFFF 及各平面末尾）
- 格式控制（Cf 类别，排除 U+00AD 软连字符）

**BOM 感知读取**：自动检测 UTF-8 BOM、UTF-16 LE BOM，无 BOM 时使用 lossy 转换。

#### 用户友好错误格式化（error_format.rs）

```rust
/// 9 种错误类别
pub enum ErrorCategory {
    FileSystem, ConfigStructure, Security, Runtime,
    Network, Script, Plugin, Validation, Unknown,
}

/// 用户友好错误（含修复建议）
pub struct UserError {
    pub category: ErrorCategory,
    pub title: String,
    pub detail: String,
    pub suggestion: Option<String>,
}

/// 将 PrismError 转换为用户友好格式
pub fn format_user_facing_error(err: &PrismError) -> UserError;
```

**设计原则**：基于语义含义分类（而非类型匹配），为所有 16 种 `PrismError` 变体提供具体可操作的修复建议。

---

## 12. 实现优先级

### Phase 1: 核心（2-3 周）

- [x] Patch IR 数据结构 + Patch Compiler
- [x] Prism DSL 解析器（8 个操作 + 固定执行顺序）
- [x] 静态字段 AST 校验（基于标识符节点，非字符串匹配）
- [x] Patch Executor（含 ExecutionTrace）
- [x] 4 层作用域 + `__when__` 条件判断
- [x] 跨阶段依赖验证（Profile Patch 不得依赖 Shared Patch）
- [x] 统一 Guarded 字段检查（所有操作类型）
- [x] 环检测增强（多候选 DFS 回溯）

### Phase 2: 调试（1-2 周）

- [x] Diff View（配置变更总览）
- [x] Trace View（变更来源追踪）
- [x] Explain View（字段溯源查询）
- [x] 性能追踪器（PerfTracker，高阶函数 + 人类可读报告）
- [x] 大规模规则性能优化（Annotation 缓存、文件级解析缓存、Arc 共享、HashMap 索引、retain 原地过滤、Patch 引用传递）

### Phase 3: 脚本与插件（2-3 周）

- [x] rquickjs 脚本运行时 + 安全沙箱
- [x] 结构化脚本 API（proxies/rules/groups 工具）
- [x] Config Plugin 加载 + manifest 权限系统
- [x] 8 个生命周期钩子
- [x] 多组件插件架构（ComponentManifest，6 种组件类型）
- [x] Hook 结果聚合与条件过滤（AggregatedHookResult + HookCondition）

### Phase 4: 智能（1-2 周）

- [x] Smart Selector（EMA 评分 + P90 + 时间衰减）
- [x] smart.toml 配置解析
- [x] 自适应测速调度
- [x] 除零保护 + 安全算术运算

### Phase 5: Extension 接口层（已完成）

- [x] `clash-prism-extension` crate 骨架
- [x] `PrismHost` trait（4 必须 + 8 可选方法）
- [x] `PrismExtension<H>` 结构（apply / list_rules / preview_rules / toggle_group / status / start_watching / stop_watching / get_trace / get_stats）
- [x] 支持类型（ApplyResult / ApplyOptions / PrismEvent / RuleAnnotation / RuleGroup / RuleDiff）
- [x] 规则注解系统（RuleAnnotation + group_annotations）
- [x] 适配模板 / CLI 脚手架（`clash-prism-ext init`）

> **说明**：Layer 3 JSON API（Tauri Commands / Electron IPC / HTTP）的具体实现由 GUI 接入方自行完成。Prism Engine 仅提供 Rust 层 API（`PrismExtension<H>`），接入方根据自身技术栈选择通信方式。

### Phase 6: 生态

- [x] HTTP Server 模式（`prism-cli serve --port 9097`，axum REST API）
- [x] prism-cli 完善（apply / status / serve 子命令 + clap 参数解析）
- [x] 集成测试（5 个端到端测试用例）
- [x] rustdoc 文档注释（所有公开类型和方法）
- [x] 配置迁移系统（幂等、版本追踪、有序执行）
- [x] 三级缓存架构（L1 内存 mtime + L2 磁盘原子 + L3 分布式预留）
- [x] 确定性序列化（BTreeMap 递归 key 排序 + 内容哈希）
- [x] Unicode 安全清洗（迭代 NFKC + 危险字符移除 + BOM 感知读取）
- [x] 用户友好错误格式化（9 类错误 + 修复建议）
- [x] NDJSON 输出（U+2028/U+2029 转义 + 流式 Writer）
- [x] PID 文件锁（跨进程互斥 + 过期检测 + RAII 自动释放）
- [x] 发布 `clash-prism-extension` crate 到 crates.io
- [x] CHANGELOG + SemVer 版本管理
- [x] Fuzzing targets（DSL parser / expression evaluator / YAML round-trip）
- [x] Criterion benchmarks（parser / predicate / transform / full pipeline）
- [x] GitHub Actions CI（lint / test / MSRV / release build / cross-compile）
- [x] Schema 一致性检查工具（gen-schema.rs，VALID_OPS 与 prism-schema.json 同步验证）

---

## 13. Extension 接口层（clash-prism-extension）

### 13.1 定位

Prism Engine 是**纯后端 Rust 库**，不包含任何前端代码。

- **Prism Engine 负责**：配置编译、规则注解、执行追踪、文件监听
- **GUI 接入方负责**：实现 `PrismHost` trait、选择 IPC 方式（Tauri / Electron / HTTP）、构建前端 UI

接入流程：

```text
┌─────────────────────────────────────────────────┐
│ Prism Engine (纯 Rust 库)                        │
│ clash-prism-core / clash-prism-dsl / clash-prism-extension        │
│                                                  │
│ 提供: PrismHost trait + PrismExtension<H> API   │
└──────────────────────┬──────────────────────────┘
                       │ GUI 接入方实现 PrismHost
┌──────────────────────▼──────────────────────────┐
│ GUI 客户端 (Tauri / Electron / Web / ...)       │
│                                                  │
│ 负责: Host Bridge + IPC + 前端 UI               │
└─────────────────────────────────────────────────┘
```

### 13.2 PrismHost Trait

GUI 客户端需要实现的唯一 trait（~50-100 行 Rust 代码）：

```rust
pub trait PrismHost: Send + Sync {
    // ── 必须实现 (4 个) ──
    fn read_running_config(&self) -> Result<String, String>;
    fn apply_config(&self, config: &str) -> Result<ApplyStatus, String>;
    fn get_prism_workspace(&self) -> Result<PathBuf, String>;
    fn notify(&self, event: PrismEvent);

    // ── 可选实现 (8 个，有默认实现) ──
    fn read_raw_profile(&self, profile_id: &str) -> Result<String, String>;
    fn list_profiles(&self) -> Result<Vec<ProfileInfo>, String>;
    fn get_core_info(&self) -> Result<CoreInfo, String>;
    fn validate_config(&self, config: &str) -> Result<bool, String>;
    fn script_count(&self) -> Result<usize, String>;
    fn plugin_count(&self) -> Result<usize, String>;
    fn get_current_profile(&self) -> Option<String>;  // __when__.profile 条件匹配依赖此方法
    fn get_variables(&self) -> HashMap<String, String>;  // {{var}} 模板替换依赖此方法
}
```

使用 `clash-prism-ext init` 可生成通用适配模板，按 TODO 注释填充即可。

### 13.3 PrismExtension\<H\> API 参考

```rust
let ext = PrismExtension::new(my_host);

// ── 核心操作 ──
let result = ext.apply(ApplyOptions::default())?;     // 执行完整编译管道
let status = ext.status();                              // 获取引擎运行状态

// ── 文件监听 ──
ext.start_watching(500)?;  // 启动监听，500ms 防抖
ext.stop_watching();       // 停止监听

// ── 规则查询 ──
let groups = ext.list_rules()?;                        // 列出 Prism 管理的规则组
let diff = ext.preview_rules("ad-filter")?;            // 预览指定 patch 的规则变更
let is_prism = ext.is_prism_rule(5)?;                  // 判断指定规则是否由 Prism 管理
ext.toggle_group("ad-filter.prism.yaml", false)?;      // 启用/禁用规则组

// ── 调试与追踪 ──
let trace = ext.get_trace("patch-id")?;                // 获取执行追踪
let stats = ext.get_stats()?;                          // 获取编译统计

// ── 可选方法（代理到 Host） ──
let raw_profile = ext.read_raw_profile("profile-id")?;
let profiles = ext.list_profiles()?;
let core_info = ext.get_core_info()?;
let is_valid = ext.validate_config(&config_str)?;
```

### 13.4 规则注解系统（Rule Provenance）

Prism 在执行 `$prepend` / `$append` 时，为每条注入的规则生成注解。GUI 规则编辑器据此判断"这条规则是谁管的"：

```rust
pub struct RuleAnnotation {
    pub rule_text: String,        // "DOMAIN-SUFFIX,ad.com,REJECT"
    pub index_in_output: usize,   // 在最终配置中的位置
    pub source_file: String,      // "ad-filter.prism.yaml"
    pub source_patch: String,     // Patch ID
    pub source_label: String,     // "广告过滤"
    pub immutable: bool,          // 是否不可编辑
}
```

### 13.5 事件通知

所有事件通过 `PrismHost::notify()` 发送给 GUI，由 GUI 决定如何转发给前端：

```rust
pub enum PrismEvent {
    PatchApplied { patch_id: String, stats: PatchStats },
    PatchFailed { patch_id: String, error: String },
    ConfigReloaded { success: bool, message: String },
    RulesChanged { added: usize, removed: usize, modified: usize },
    WatcherEvent { file: String, change_type: String },
    WatcherStatus { running: bool, watching_count: usize },
}
```

### 13.6 安全考量

- **Guarded Fields（统一检查）**：`external-controller`、`secret`、`mixed-port` 等字段不得被 Prism 覆盖。所有操作类型（DeepMerge / Override / Prepend / Append / Filter / Transform / Remove / SetDefault）统一在分发前检查，命中时跳过并输出 warn 日志
- **Path Traversal**：`toggle_group()` 包含 `..` / `\0` 检查 + `canonicalize` + `starts_with` 验证
- **Script Sandbox**：JS 脚本在 rquickjs（quickjs-ng）沙箱中执行，Unicode 转义预处理 + 5 层安全验证 + 不平衡括号防护 + per-property 运行时加固（兼容 quickjs-ng）
- **Mutex Safety**：所有内部锁使用 `lock_or_err()` 处理 poison，不会 panic。`AdaptiveScheduler` 的 `config` 字段使用 `Mutex<SchedulerConfig>` 保护，返回克隆副本避免跨线程生命周期问题
- **Config Validation**：处理后配置应通过 `validate_config()` 验证后再应用。新增 `check_proxy_required_fields` 检查代理节点必要字段（name / type / server）
- **Unicode 安全纵深**：配置字符串经迭代 NFKC 归一化 + 危险字符移除（零宽字符、方向控制、BOM、私用区、非字符码点）。配置文件读取支持 BOM 感知（UTF-8 / UTF-16 LE）
- **原子写入**：所有文件写入采用 temp + `sync_all` + rename 模式（磁盘缓存、配置输出、模板生成、PID 文件），确保崩溃安全
- **Cron 调度器安全**：`register` 方法中 `running` 检查和 `spawn_task` 在同一锁保护下执行，防止竞态条件。`shutdown` 使用常量超时时间（30s）
- **常量时间比较**：敏感比较操作使用常量时间算法，防止时序攻击

### 13.7 接入指南

1. **Rust GUI（Tauri）**：`clash-prism-ext init` → 复制模板 → 实现 `PrismHost` → 注册 Tauri Commands
2. **Electron GUI**：通过 napi-rs 编译原生模块，或通过 `prism serve` HTTP API 接入
3. **其他语言**：通过 `prism serve` HTTP API 接入（Phase 6 规划）

### 13.8 Layer 3 JSON API 参考（供接入方设计 IPC 时参考）

> 以下 API 名称和参数格式**仅供参考**，接入方可根据自身技术栈自由设计。
> Prism Engine 仅提供 Rust 层 `PrismExtension<H>` API，不强制任何特定的 JSON 协议。

#### 核心 API

| API | 对应 Rust 方法 | 参数 | 返回值 |
|-----|---------------|------|--------|
| `prism_apply` | `ext.apply(opts)` | `ApplyOptions` | `ApplyResult` |
| `prism_status` | `ext.status()` | `{}` | `PrismStatus` |

#### 规则 API

| API | 对应 Rust 方法 | 参数 | 返回值 |
|-----|---------------|------|--------|
| `prism_list_rules` | `ext.list_rules()` | `{}` | `Vec<RuleGroup>` |
| `prism_preview_rules` | `ext.preview_rules(id)` | `{ patch_id }` | `RuleDiff` |
| `prism_is_prism_rule` | `ext.is_prism_rule(i)` | `{ index }` | `IsPrismRule` |
| `prism_toggle_group` | `ext.toggle_group(id, on)` | `{ group_id, enabled }` | `bool` |

#### 调试 API

| API | 对应 Rust 方法 | 参数 | 返回值 |
|-----|---------------|------|--------|
| `prism_get_trace` | `ext.get_trace(id)` | `{ patch_id }` | `TraceView` |
| `prism_get_stats` | `ext.get_stats()` | `{}` | `CompileStats` |

#### 可选 API（依赖 Host 实现）

| API | 对应 Rust 方法 | 参数 | 返回值 |
|-----|---------------|------|--------|
| `prism_read_profile` | `ext.read_raw_profile(id)` | `{ profile_id }` | `String` |
| `prism_list_profiles` | `ext.list_profiles()` | `{}` | `Vec<ProfileInfo>` |
| `prism_get_core_info` | `ext.get_core_info()` | `{}` | `CoreInfo` |
| `prism_validate_config` | `ext.validate_config(s)` | `{ config }` | `bool` |

### 13.9 GUI 适配工作量估算

| GUI | Host Bridge | IPC Commands | 前端改动 | 总计 |
|-----|------------|--------------|---------|------|
| Zephyr (Tauri + Rust) | ~80 行 Rust | ~30 行 | ~50 行 TSX | ~160 行 |
| Clash Verge Rev (Tauri + Rust) | ~100 行 Rust | ~30 行 | ~50 行 TSX | ~180 行 |
| Clash Nyanpasu (Tauri + Rust) | ~90 行 Rust | ~30 行 | ~50 行 TSX | ~170 行 |
| mihomo-party (Electron + TS) | ~120 行 TS | ~20 行 IPC | ~50 行 TSX | ~190 行 |

### 13.10 GUI 编辑器集成示例

以下伪代码展示前端如何利用规则注解区分 Prism 管理的规则和用户手动编辑的规则：

```typescript
// 前端获取规则列表后，根据注解渲染不同样式
async function renderRules(allRules: string[]) {
    // 调用 Prism Extension 获取规则注解
    const groups = await invoke("prism_list_rules");
    const prismIndices = new Map(
        groups.flatMap(g => g.rules.map(r => [r.index, g]))
    );

    for (const [i, rule] of allRules.entries()) {
        if (prismIndices.has(i)) {
            // Prism 管理的规则：显示来源标签，禁止编辑（如果 immutable）
            const group = prismIndices.get(i);
            renderPrismManagedRule(rule, group);
        } else {
            // 用户手动编辑的规则：正常显示，可自由编辑
            renderUserRule(rule);
        }
    }
}
```

---

## 14. CLI 增强功能

### 14.1 NDJSON 输出

CLI 支持 NDJSON（Newline-Delimited JSON）输出格式，便于机器解析和日志聚合：

```rust
/// 输出格式
pub enum OutputFormat {
    Human,   // 默认，人类可读文本
    Ndjson,  // NDJSON 流式输出
    Json,    // 单次 JSON 输出
}

/// ECMA-262 安全的 JSON 序列化（转义 U+2028/U+2029）
pub fn ndjson_safe_stringify(value: &Value) -> String;

/// 流式 NDJSON 写入器
pub struct NdjsonWriter<W: Write> {
    writer: W,
}

impl<W: Write> NdjsonWriter<W> {
    /// 写入单行 JSON
    pub fn write_value(&mut self, value: &Value) -> io::Result<()>;

    /// 写入事件对象（自动添加 "event" 字段）
    pub fn write_event(&mut self, name: &str, data: &Value) -> io::Result<()>;
}
```

**ECMA-262 安全性**：RFC 8259 允许 U+2028（行分隔符）和 U+2029（段落分隔符）出现在 JSON 字符串中，但 ECMA-262 将其视为行终止符，会截断 NDJSON 流。`ndjson_safe_stringify` 额外转义这两个字符，确保 JavaScript 解析器安全。

**命令行用法**：

```bash
prism apply --output ndjson    # NDJSON 流式输出
prism apply --output json      # 单次 JSON 输出
prism apply --output human     # 人类可读（默认）
# 别名支持：text/pretty → human, jsonl/stream → ndjson
```

### 14.2 PID 文件锁

CLI 使用 PID 文件锁实现跨进程互斥，防止多个 Prism 实例同时操作同一工作目录：

```rust
pub struct PidLock {
    lock_file: PathBuf,
    acquired: bool,
}

impl PidLock {
    /// 获取锁（原子写入 PID 文件）
    pub fn acquire(lock_dir: &Path, force: bool) -> Result<Self, String>;

    /// 手动释放（幂等）
    pub fn release(&mut self);
}

impl Drop for PidLock {
    /// RAII 自动释放：离开作用域时删除锁文件
    fn drop(&mut self);
}
```

**核心机制**：
- **原子写入 PID**：temp + rename 模式，与磁盘缓存策略一致
- **跨平台进程检测**：Unix 使用 `libc::kill(pid, 0)` 信号 0 检测；Windows 使用 `CreateToolhelp32Snapshot` 遍历进程列表
- **过期锁自动覆盖**：PID 文件存在但对应进程已不存在时，自动安全覆盖
- **强制模式**：`--force` 标志即使进程仍在运行也强制获取锁，用于异常恢复
- **RAII 自动释放**：实现 `Drop` trait，防止资源泄漏

---

*Prism Engine v0.1.0 — 2026-04-16（纯后端配置增强引擎，前端由接入方自行负责）*
