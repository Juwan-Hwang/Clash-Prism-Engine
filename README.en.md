# Prism Engine

[![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml/badge.svg)](https://github.com/Juwan-Hwang/Clash-Prism-Engine/actions/workflows/ci.yml)
[![Tests](https://img.shields.io/badge/tests-773%20passed-success.svg)]()

> **Prism**: Multiple input sources refracted through a unified intermediate layer, producing precise, traceable final configurations.

**Prism Engine** is a pure Rust configuration enhancement engine designed for [mihomo](https://github.com/MetaCubeX/mihomo) / Clash kernels. Through a unified **Patch IR** intermediate representation, it compiles DSL declarations, JavaScript scripts, plugin systems, and other input sources into deterministic configuration transformations, enabling traceable, debuggable, and reproducible proxy configuration management.

## Features

- **8 DSL Operations** — `$override` / `$prepend` / `$append` / `$filter` / `$transform` / `$remove` / `$default` / Deep Merge, with fixed execution order
- **Unified Patch IR** — All inputs compiled into a unified intermediate representation with Explain View field tracing and Step Replay
- **JavaScript Script Engine** — rquickjs (QuickJS-NG) sandbox, ES2023+ support, 5-layer static validation + 5-layer runtime hardening
- **Plugin System** — 6 component types (patches / scripts / hooks / templates / scorers / validators), 8+1 lifecycle hooks
- **Smart Selector** — Standalone runtime module with EMA scoring (P90 latency + success rate + stability), adaptive speed testing scheduler
- **4-Layer Scope** — Global → Profile → Scoped → Runtime, Profile-level concurrent execution
- **GUI Integration** — `PrismHost` trait, ~100 lines of code to integrate with Tauri / Electron clients
- **Debug System** — Diff View / Trace View / Explain View / Step Replay / PerfTracker
- **Security in Depth** — Path traversal protection, ReDoS protection, prototype pollution prevention, constant-time API key comparison, atomic file writes

## Quick Start

### Add Dependencies

```toml
[dependencies]
clash-prism-core = "0.1.0"
clash-prism-dsl = "0.1.0"
```

### Basic Usage

```rust
use clash_prism_core::{PatchCompiler, PatchExecutor, TargetCompiler};
use clash_prism_dsl::DslParser;

// 1. Parse DSL file
let dsl = DslParser::parse_file("rules.prism.yaml")?;

// 2. Compile to Patch IR
let mut compiler = PatchCompiler::new();
compiler.register_dsl_patches(&dsl)?;
let patches = compiler.compile_and_execute(base_config.clone())?;

// 3. Output target format
let output = TargetCompiler::to_mihomo_yaml(&patches.output)?;
```

### Prism DSL Example

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
| `clash-prism-core` | Core engine: Patch IR, compiler, executor, validator, cache, file watcher | [![crates.io](https://img.shields.io/crates/v/clash-prism-core.svg)](https://crates.io/crates/clash-prism-core) |
| `clash-prism-dsl` | DSL parser: `.prism.yaml` parsing, static field validation, JSON Schema | [![crates.io](https://img.shields.io/crates/v/clash-prism-dsl.svg)](https://crates.io/crates/clash-prism-dsl) |
| `clash-prism-script` | Script engine: rquickjs sandbox, structured API, KV storage | [![crates.io](https://img.shields.io/crates/v/clash-prism-script.svg)](https://crates.io/crates/clash-prism-script) |
| `clash-prism-smart` | Smart selector: EMA scoring, time decay, adaptive speed testing | [![crates.io](https://img.shields.io/crates/v/clash-prism-smart.svg)](https://crates.io/crates/clash-prism-smart) |
| `clash-prism-plugin` | Plugin system: lifecycle hooks, multi-component architecture, Cron scheduling | [![crates.io](https://img.shields.io/crates/v/clash-prism-plugin.svg)](https://crates.io/crates/clash-prism-plugin) |
| `clash-prism-extension` | GUI integration: PrismHost trait, rule annotations, JSON API | [![crates.io](https://img.shields.io/crates/v/clash-prism-extension.svg)](https://crates.io/crates/clash-prism-extension) |
| `prism-cli` | CLI tool: apply / status / serve / watch | Binary distribution |
| `prism-ext` | Scaffolding tool: `prism-ext init` generates adapter templates | Binary distribution |

## DSL Operations

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

**Fixed execution order**: `$filter` → `$remove` → `$transform` → `$default` → `$prepend` → `$append` → Deep Merge → `$override`

## Conditional Scopes

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

## GUI Integration

Prism Engine integrates with GUI clients via the `PrismHost` trait. Only 4 required methods:

```rust
pub trait PrismHost: Send + Sync {
    fn read_running_config(&self) -> Result<String>;
    fn apply_config(&self, config: &str, status: &ApplyStatus) -> Result<()>;
    fn get_prism_workspace(&self) -> Result<PathBuf>;
    fn notify(&self, event: PrismEvent);
}
```

Scaffolding tool for quick adapter generation:

```bash
prism-ext init --output src-tauri/src/
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

## Build

```bash
# Debug
cargo build --workspace

# Release
cargo build -p prism-cli --release

# Test
cargo test --workspace

# Lint
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features
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
