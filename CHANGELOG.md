# Changelog

All notable changes to Prism Engine will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
