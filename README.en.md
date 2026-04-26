# Prism Engine

[![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml/badge.svg)](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-825%20passed-success.svg)]()

> **Prism**: Multiple input sources refracted through a unified intermediate layer, producing precise, traceable final configurations.

## What is it?

**Prism Engine** is a pure Rust **configuration enhancement engine for mihomo / Clash**.

It solves a core problem: **how to manage and transform proxy configurations declaratively, instead of manually editing large YAML files.**

### Typical Use Cases

- You have multiple `.prism.yaml` files managing rules, proxies, DNS, etc. separately
- You want to automatically switch configurations based on conditions (platform, time, WiFi)
- You're developing a mihomo GUI client and need an embeddable configuration engine
- You need full traceability of configuration changes (what changed, how many rules affected)

### What can it do?

| Capability | Description |
|------------|-------------|
| 📝 **Declarative Config** | Describe "what you want" in `.prism.yaml` DSL, the engine computes the final config |
| 🔀 **8 Operations** | `$filter` / `$remove` / `$transform` / `$default` / `$prepend` / `$append` / Deep Merge / `$override` |
| 📊 **Change Tracking** | Precise stats per compilation: +N rules, -M rules, ~K modifications |
| 🔌 **Plugin System** | JavaScript scripts + plugin system for custom logic |
| 🖥️ **GUI Integration** | `PrismHost` trait — ~100 lines to integrate into Tauri / Electron clients |
| 🌐 **HTTP API** | Built-in REST server for GUI clients to call the engine via API |
| 👁️ **File Watching** | Auto-watch `.prism.yaml` changes and recompile in real-time |

## Who uses it?

Prism Engine has two usage patterns:

### 1️⃣ GUI Developers (Primary Users)

If you're building a mihomo / Clash GUI client (e.g., a Tauri app), embed Prism Engine as a library:

```toml
[dependencies]
clash-prism-extension = "0.1.2"
```

Implement the `PrismHost` trait (4 required methods) to get full configuration management:

```rust
pub trait PrismHost: Send + Sync {
    fn read_running_config(&self) -> Result<String>;       // Read current config
    fn apply_config(&self, config: &str, status: &ApplyStatus) -> Result<()>;  // Write config
    fn get_prism_workspace(&self) -> Result<PathBuf>;      // Get workspace directory
    fn notify(&self, event: PrismEvent);                   // Receive event notifications

    // Optional: override to enable __when__.profile condition matching
    fn get_current_profile(&self) -> Option<String> { None }
}
```

Use the scaffolding tool to generate adapter code in one command:

```bash
prism-ext init --output src-tauri/src/
```

The generated code includes 16 Tauri Commands (`prism_apply`, `prism_list_rules`, `prism_toggle_group`, etc.) ready for your frontend.

Get full execution trace report:

```rust
let report = ext.trace_report()?;  // Returns text-formatted per-patch change details
```

### 2️⃣ Terminal Users / Debugging

`prism-cli` provides a command-line tool for debugging and standalone use:

```bash
# One-shot compile: read config.yaml + all .prism.yaml files in prism/ dir
prism-cli apply --config ./config.yaml --prism-dir ./prism

# Compile and output full execution trace report (per-patch change details)
prism-cli apply --config ./config.yaml --prism-dir ./prism --verbose

# Start HTTP server (GUI clients call via API)
prism-cli serve --port 9097 --config ./config.yaml --prism-dir ./prism

# Validate DSL syntax
prism-cli check rules.prism.yaml

# Parse and preview Patch IR
prism-cli parse rules.prism.yaml

# Execute JavaScript script
prism-cli run script.js --config ./config.yaml

# Watch file changes and auto-recompile
prism-cli watch ./prism --output ./config.yaml

# View engine status
prism-cli status --prism-dir ./prism
```

## DSL Example

Say your mihomo `config.yaml` has 100 proxy nodes and you want to:

1. Keep only SS-type US nodes
2. Insert your own proxy at the top
3. Add company direct-connect rules at the beginning

Create `prism/rules.prism.yaml`:

```yaml
proxies:
  # Step 1: Filter — keep only SS nodes with "US" in server name
  $filter: "type == 'ss' && server.contains('US')"

  # Step 2: Prepend — add your proxy at the top
  $prepend:
    - name: "my-ss-proxy"
      type: ss
      server: us1.example.com
      port: 8388
      cipher: aes-256-gcm
      password: "your-password"

rules:
  # Step 3: Prepend — company domain direct connect
  $prepend:
    - "DOMAIN-SUFFIX,company.com,DIRECT"

  # Step 4: Append — catch-all rule
  $append:
    - "MATCH,Proxy"
```

Run the compilation:

```bash
prism-cli apply --config ./config.yaml --prism-dir ./prism
```

Output:

```
⚡ Prism Engine Apply

📊 Compile stats:
  Total patches: 3
  Succeeded: 3
  Skipped: 0
  Rules added: 2
  Rules removed: 0
  Rules modified: 0
  Total time: 142μs (avg 47μs/patch)
```

The final `config.yaml` is automatically updated: filtered proxy list + your custom proxy + new rule order.

## Conditional Scopes

Automatically apply different configurations based on platform, time, WiFi, etc.:

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

## Features

- **8 DSL Operations** — Fixed execution order: `$filter` → `$remove` → `$transform` → `$default` → `$prepend` → `$append` → Deep Merge → `$override`
- **Unified Patch IR** — All inputs compiled into a unified intermediate representation with field tracing and step replay
- **JavaScript Script Engine** — rquickjs (QuickJS-NG) sandbox, ES2023+, 5-layer static validation + 5-layer runtime hardening
- **Plugin System** — 6 component types (patches / scripts / hooks / templates / scorers / validators), 8+1 lifecycle hooks
- **Smart Selector** — EMA scoring (P90 latency + success rate + stability), adaptive speed testing scheduler
- **4-Layer Scope** — Global → Profile → Scoped → Runtime, Profile-level concurrent execution
- **Security in Depth** — Path traversal protection, ReDoS protection, prototype pollution prevention, constant-time API key comparison, atomic file writes
- **Large-Scale Rule Optimization** — Annotation caching, file-level parse caching, Arc sharing, HashMap indexing, retain in-place filtering, Patch reference passing; response drops from seconds to milliseconds with tens of thousands of rules

## Architecture

```
Input Sources (DSL / Script / Plugin)
        │
        ▼
  ┌─ Patch Compiler ─┐
  │  Static field validation │
  │  Condition precompilation │
  │  Dependency topological sort │
  └────────┬─────────┘
           │
           ▼
    ┌─ Patch IR ─┐     ← Unified Intermediate Representation
    │  Serializable │
    │  Traceable   │
    │  Replayable  │
    └─────┬──────┘
          │
          ▼
  ┌─ Patch Executor ─┐
  │  Profile-level concurrency │
  │  Fixed operation order     │
  │  ExecutionTrace            │
  └────────┬─────────┘
           │
           ▼
  ┌─ Validator ─┐
  │  7 checks    │
  │  Smart suggestions │
  └─────┬───────┘
        │
        ▼
  ┌─ Target Compiler ─┐
  │  mihomo / clash-rs │
  └───────────────────┘
```

For the complete architecture design, see [Prism_Engine_Final_Architecture.md](Prism_Engine_Final_Architecture.md).

## Crate Overview

| Crate | Description | Crates.io |
|-------|-------------|-----------|
| `clash-prism-core` | Core engine: Patch IR, compiler, executor, validator, cache, file watcher | [![cr](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core) |
| `clash-prism-dsl` | DSL parser: `.prism.yaml` parsing, static field validation, JSON Schema | [![cr](https://img.shields.io/crates/v/clash-prism-dsl.svg)](https://crates.io/crates/clash-prism-dsl) |
| `clash-prism-script` | Script engine: rquickjs sandbox, structured API, KV storage | [![cr](https://img.shields.io/crates/v/clash-prism-script.svg)](https://crates.io/crates/clash-prism-script) |
| `clash-prism-smart` | Smart selector: EMA scoring, time decay, adaptive speed testing | [![cr](https://img.shields.io/crates/v/clash-prism-smart.svg)](https://crates.io/crates/clash-prism-smart) |
| `clash-prism-plugin` | Plugin system: lifecycle hooks, multi-component architecture, Cron scheduling | [![cr](https://img.shields.io/crates/v/clash-prism-plugin.svg)](https://crates.io/crates/clash-prism-plugin) |
| `clash-prism-extension` | GUI integration: PrismHost trait, rule annotations, JSON API | [![cr](https://img.shields.io/crates/v/clash-prism-extension.svg)](https://crates.io/crates/clash-prism-extension) |
| `prism-cli` | CLI tool: apply / serve / watch / parse / check / run | [Binaries](https://github.com/Juwan-Hwang/Clash-Prism-Engine/releases) |
| `prism-ext` | Scaffolding tool: `prism-ext init` generates GUI adapter templates | [Binaries](https://github.com/Juwan-Hwang/Clash-Prism-Engine/releases) |

## DSL Operations Reference

| Operation | Syntax | Description |
|-----------|--------|-------------|
| Deep Merge | (default key) | Recursive merge of child items |
| `$override` | `$override: {...}` | Force override (exclusive key) |
| `$prepend` | `$prepend: [...]` | Prepend to array |
| `$append` | `$append: [...]` | Append to array |
| `$filter` | `$filter: "expr"` | Conditional filter (static fields only) |
| `$transform` | `$transform: {...}` | Map transformation |
| `$remove` | `$remove: "expr"` | Conditional removal |
| `$default` | `$default: {...}` | Default value injection |

## Plugin Development

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

## Security

| Measure | Description |
|---------|-------------|
| Script Sandbox | 5-layer static validation + 5-layer runtime hardening |
| Unicode Safety | NFKC normalization + dangerous character removal + BOM awareness |
| Path Traversal Protection | canonicalize + prefix check |
| ReDoS Protection | DFA size limit + LRU regex cache |
| Atomic File Writes | tmp + sync_all + rename |
| Constant-Time Comparison | API key authentication against timing attacks |
| HTTP Rate Limiting | Sliding window 60 req/60s per IP |

See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## Project Structure

```
Clash-Prism-Engine/
├── crates/
│   ├── clash-prism-core/       # Core engine
│   ├── clash-prism-dsl/        # DSL parser
│   ├── clash-prism-script/     # Script engine
│   ├── clash-prism-smart/      # Smart selector
│   ├── clash-prism-plugin/     # Plugin system
│   └── clash-prism-extension/  # GUI integration layer
├── prism-cli/                  # CLI tool
├── prism-ext/                  # Scaffolding tool
├── examples/                   # Usage examples
├── tests/                      # Integration tests
├── fuzz/                       # Fuzzing targets
├── .github/workflows/          # CI/CD
├── Prism_Engine_Final_Architecture.md  # Architecture spec
├── CHANGELOG.md                # Changelog
├── CONTRIBUTING.md             # Contributing guide
├── SECURITY.md                 # Security policy
└── prism-schema.json           # DSL JSON Schema
```

## Documentation

| Document | Description |
|----------|-------------|
| [Prism_Engine_Final_Architecture.md](Prism_Engine_Final_Architecture.md) | Complete architecture spec (14 chapters, 83 requirements) |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup and contribution workflow |
| [CHANGELOG.md](CHANGELOG.md) | Version changelog |
| [SECURITY.md](SECURITY.md) | Vulnerability reporting policy |
| [examples/](examples/) | Usage examples (basic, plugin, Smart Selector) |
| [prism-schema.json](prism-schema.json) | DSL JSON Schema (VS Code highlighting) |

## License

Apache License, Version 2.0 — See [LICENSE](LICENSE)

## Acknowledgements

- [mihomo](https://github.com/MetaCubeX/mihomo) — Clash Meta kernel
- [QuickJS-NG](https://github.com/nicknisi/quickjs-ng) — JavaScript engine
- [rquickjs](https://github.com/DelSkayn/rquickjs) — QuickJS Rust bindings

---

English | [简体中文](README.md)
