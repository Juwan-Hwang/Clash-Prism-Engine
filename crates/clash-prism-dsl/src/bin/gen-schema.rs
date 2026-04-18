//! Schema Consistency Checker
//!
//! Validates that the hand-curated static `prism-schema.json` stays in sync
//! with the code-defined `VALID_OPS` in `parser.rs`.
//!
//! **Design principle**: `prism-schema.json` is the **authoritative schema file**
//! used by IDEs. It contains hand-curated metadata (descriptions, defaults,
//! constraints) that go beyond what `prism_schema()` generates. This tool
//! does NOT overwrite the static file — it only checks that the operator
//! lists are consistent.
//!
//! ## What it checks
//!
//! 1. Every op in `VALID_OPS` exists as a property in the static schema's
//!    `array_field` or `config_field` definitions.
//! 2. No stale ops exist in the static schema that are missing from `VALID_OPS`.
//!
//! ## Usage
//!
//! ```bash
//! # Check consistency (used in CI):
//! cargo run --package clash-prism-dsl --bin gen-schema -- --check
//!
//! # The static file is never modified by this tool.
//! ```

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let check_mode = args.iter().any(|a| a == "--check");

    if !check_mode {
        eprintln!("ℹ️  This tool only supports --check mode.");
        eprintln!("   The static prism-schema.json is the authoritative schema.");
        eprintln!("   Edit it directly — this tool verifies operator consistency.");
        eprintln!("   Usage: cargo run --package clash-prism-dsl --bin gen-schema -- --check");
        std::process::exit(0);
    }

    // Locate workspace root
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set — run via `cargo run`");
    let manifest_path = PathBuf::from(&manifest_dir);
    let workspace_root = manifest_path
        .parent()
        .expect("clash-prism-dsl/ parent")
        .parent()
        .expect("crates/ parent")
        .to_path_buf();

    let schema_path = workspace_root.join("prism-schema.json");
    let schema_str = match std::fs::read_to_string(&schema_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "❌ prism-schema.json not found at {}",
                schema_path.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("❌ Failed to read {}: {}", schema_path.display(), e);
            std::process::exit(1);
        }
    };

    let schema: serde_json::Value = match serde_json::from_str(&schema_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ Failed to parse prism-schema.json: {}", e);
            std::process::exit(1);
        }
    };

    // Collect ops from VALID_OPS (code source of truth)
    let code_ops: Vec<&str> = clash_prism_dsl::parser::VALID_OPS.to_vec();

    // Collect ops referenced in the static schema's array_field definition
    let mut schema_ops = Vec::new();
    if let Some(defs) = schema.get("definitions").and_then(|v| v.as_object()) {
        if let Some(af) = defs.get("array_field").and_then(|v| v.as_object()) {
            if let Some(one_of) = af.get("oneOf").and_then(|v| v.as_array()) {
                // Find the object variant (second element) that contains $-prefixed ops
                for variant in one_of {
                    if let Some(props) = variant.get("properties").and_then(|v| v.as_object()) {
                        for key in props.keys() {
                            if key.starts_with('$') {
                                schema_ops.push(key.as_str());
                            }
                        }
                    }
                }
            }
        }
        // Also check config_field for $override and $default
        if let Some(cf) = defs.get("config_field").and_then(|v| v.as_object()) {
            if let Some(one_of) = cf.get("oneOf").and_then(|v| v.as_array()) {
                for variant in one_of {
                    if let Some(props) = variant.get("properties").and_then(|v| v.as_object()) {
                        for key in props.keys() {
                            if key.starts_with('$') && !schema_ops.contains(&key.as_str()) {
                                schema_ops.push(key.as_str());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut errors = 0;

    // Check 1: Every code op exists in the static schema
    for op in &code_ops {
        if !schema_ops.contains(op) {
            eprintln!("❌ MISSING in prism-schema.json: {}", op);
            eprintln!("   Add it to the array_field or config_field definitions.");
            errors += 1;
        }
    }

    // Check 2: No stale ops in the static schema
    for op in &schema_ops {
        if !code_ops.contains(op) {
            eprintln!("⚠️  STALE in prism-schema.json: {}", op);
            eprintln!("   This op no longer exists in VALID_OPS (parser.rs). Remove it.");
            errors += 1;
        }
    }

    if errors == 0 {
        eprintln!(
            "✅ prism-schema.json operators are in sync with VALID_OPS ({} ops verified)",
            code_ops.len()
        );
    } else {
        eprintln!(
            "\n{} issue(s) found. Please update prism-schema.json manually.",
            errors
        );
        std::process::exit(1);
    }
}
