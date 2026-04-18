# Contributing to Prism Engine

Thank you for your interest in contributing to Prism Engine! This document provides guidelines for contributing.

## Development Setup

### Prerequisites

- **Rust**: 1.85+ (see `rust-version` in workspace `Cargo.toml`)
- **Cargo**: Bundled with Rust toolchain
- **Git**: Any recent version

### Building

```bash
# Clone the repository
git clone https://github.com/prism-engine/prism-engine.git
cd prism-engine

# Build all crates (debug mode)
cargo build --workspace

# Build with release optimizations
cargo build --workspace --release

# Build the CLI binary
cargo build -p prism-cli --release
```

### Running Tests

```bash
# Run all unit tests
cargo test --workspace

# Run integration tests only
cargo test --workspace --test integration

# Run a specific test
cargo test --workspace test_full_pipeline

# Run with output
cargo test --workspace -- --nocapture
```

### Code Quality

```bash
# Format code
cargo fmt --all

# Check formatting without modifying
cargo fmt --all -- --check

# Run Clippy lints
cargo clippy --workspace --all-targets --all-features

# Fix Clippy suggestions
cargo clippy --workspace --fix --allow-dirty
```

## Project Structure

```
prism-engine/
├── crates/
│   ├── clash-prism-core/      # Core abstractions: IR, compiler, executor, validator
│   ├── clash-prism-dsl/       # DSL parser (.prism.yaml → Patch IR)
│   ├── clash-prism-script/    # JS scripting runtime (rquickjs-based sandbox)
│   ├── clash-prism-plugin/    # Plugin system (manifest, lifecycle)
│   └── clash-prism-smart/     # Smart grouping / selection logic
├── prism-cli/           # Command-line interface
├── examples/            # Usage examples
└── tests/
    ├── fixtures/        # Integration test fixture files
    └── integration/     # End-to-end integration tests
```

## Coding Conventions

1. **Error Handling**: Use `PrismError` from `clash_prism_core::error` for public APIs; use `ExprError` for expression evaluation.
2. **Documentation**: All public items must have `///` doc comments.
3. **Language**: Error messages and user-facing strings must be in English (ENG-006).
4. **Performance**: Avoid unnecessary clones in hot paths (see PERF-001). Use the regex cache for repeated pattern matching (PERF-002).
5. **Testing**: Write unit tests alongside source code (`#[cfg(test)] mod tests`). Add integration tests for cross-crate workflows.

## Submitting Changes

1. Fork the repository and create a feature branch.
2. Make sure all tests pass: `cargo test --workspace`.
3. Ensure Clippy reports no warnings: `cargo clippy --workspace -- -D warnings`.
4. Ensure formatting is correct: `cargo fmt --all -- --check`.
5. Commit with clear, descriptive messages following conventional commits.
6. Open a pull request targeting `main`.

## Security

If you discover a security vulnerability, please do **not** open a public issue. Contact the maintainers privately.

## License

By contributing, you agree that your contributions will be licensed under the Apache-2.0 License.
