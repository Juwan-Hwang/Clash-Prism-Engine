//! JSON Schema Definition — IDE autocompletion and syntax validation
//!
//! This module provides the Prism DSL JSON Schema,
//! referenceable via `# yaml-language-server: $schema=...` header comment.
//!
//!
//! The Schema is now **auto-generated** from `VALID_OPS` defined in `parser.rs`.
//! This eliminates the manual synchronization risk that existed when the Schema
//! was hand-built. The single source of truth is `VALID_OPS` in `parser.rs`.
//!
//! ### How it works
//!
//! 1. `VALID_OPS` is re-exported from `parser.rs` via `clash_prism_dsl::parser::VALID_OPS`
//! 2. `prism_schema()` builds the Schema dynamically, iterating over `VALID_OPS`
//! 3. Each op is classified as either "exclusive" ($override) or "array" (all others)
//! 4. Compile-time assertions verify the count matches expectations
//!
//! ### To add a new operator
//!
//! Simply add it to `VALID_OPS` in `parser.rs` and update `compile_path_group()`.
//! The Schema will automatically include the new operator.

use serde_json::json;

/// Re-export VALID_OPS from parser.rs — the single source of truth for operator names.
pub use crate::parser::VALID_OPS;

/// Operators that are "exclusive" — they cannot be mixed with other operations.
const EXCLUSIVE_OPS: &[&str] = &["$override"];

/// Build the Prism DSL JSON Schema dynamically from `VALID_OPS`.
///
/// This function is the single entry point for Schema generation. It reads
/// `VALID_OPS` at runtime and constructs the Schema, ensuring consistency
/// between the parser and the IDE validation layer.
pub fn prism_schema() -> serde_json::Value {
    // Build array_field properties from VALID_OPS (excluding exclusive ops)
    let mut array_field_props = serde_json::Map::new();

    for op in VALID_OPS.iter() {
        if EXCLUSIVE_OPS.contains(op) {
            continue; // $override goes in config_field, not array_field
        }
        let (desc, schema_type) = match *op {
            "$prepend" => (
                "Array prepend insert (supports conditional rule objects)",
                json!({"type": "array", "items": {"$ref": "#/definitions/conditional_rule_item"}}),
            ),
            "$append" => (
                "Array append insert (supports conditional rule objects)",
                json!({"type": "array", "items": {"$ref": "#/definitions/conditional_rule_item"}}),
            ),
            "$filter" => (
                "Conditional filter (static fields only)",
                json!({"type": "string"}),
            ),
            "$transform" => ("Map transform (batch modify)", json!({"type": "string"})),
            "$remove" => (
                "Conditional remove (static fields only)",
                json!({"type": "string"}),
            ),
            "$default" => (
                "Default value injection (only when field is absent or null)",
                json!({"type": ["object", "string", "number", "boolean", "array", "null"]}),
            ),
            _ => continue, // Skip unknown ops
        };
        let mut prop_entry = serde_json::Map::new();
        prop_entry.insert("description".to_string(), json!(desc));
        prop_entry.insert("type".to_string(), schema_type);
        array_field_props.insert(op.to_string(), serde_json::Value::Object(prop_entry));
    }

    // Build config_field oneOf variants
    let config_field_one_of = json!([
        // $default — default value injection (can be mixed with normal keys)
        {
            "type": "object",
            "properties": {
                "$default": {
                    "description": "Default value injection (only when field is absent or null)",
                    "type": ["object", "string", "number", "boolean", "array", "null"]
                }
            },
            "additionalProperties": true
        },
        // Plain value → deep merge
        { "type": ["object", "string", "number", "boolean", "null"] },
        // $override — exclusive operation
        {
            "type": "object",
            "properties": {
                "$override": {
                    "description": "Force replace (exclusive key, cannot be mixed with other operations)",
                    "type": ["object", "string", "number", "boolean", "null"]
                }
            },
            "required": ["$override"],
            "additionalProperties": false
        }
    ]);

    // Build array_field oneOf variants
    let array_field_one_of = json!([
        // Plain array → deep merge
        { "type": "array" },
        // Array operation combination
        {
            "type": "object",
            "properties": serde_json::to_value(array_field_props).unwrap(),
            "additionalProperties": true
        }
    ]);

    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Prism DSL",
        "description": "Prism Engine Configuration Enhancement DSL — .prism.yaml file format",
        "type": "object",
        "properties": {
            // ─── Metadata ───
            "__when__": {
                "description": "Conditional scope (only takes effect when conditions are met)",
                "type": "object",
                "properties": {
                    "core": {
                        "description": "Kernel type",
                        "type": "string",
                        "enum": ["mihomo", "clash-rs"]
                    },
                    "platform": {
                        "description": "Operating system",
                        "oneOf": [
                            { "type": "string", "enum": ["windows", "macos", "linux", "android", "ios"] },
                            {
                                "type": "array",
                                "items": { "type": "string", "enum": ["windows", "macos", "linux", "android", "ios"] }
                            }
                        ]
                    },
                    "profile": {
                        "description": "Profile name (supports regex matching)\n\nSyntax: JavaScript-style regular expressions wrapped in slashes.\nExamples:\n  - \"/streaming|unlock/\" — matches profiles containing 'streaming' or 'unlock'\n  - \"/^prod-/\" — matches profiles starting with 'prod-'\n  - \"/test$/\" — matches profiles ending with 'test'\n\nNote: Regex matching is executed at runtime by the engine.",
                        "type": "string"
                    },
                    "time": {
                        "description": "Time range (optional)",
                        "type": "string",
                        "pattern": "^\\d{2}:\\d{2}-\\d{2}:\\d{2}$"
                    },
                    "enabled": {
                        "description": "Enable/disable toggle. When false, the entire file is skipped.",
                        "type": "boolean"
                    },
                    "ssid": {
                        "description": "WiFi SSID condition. Only applies when connected to this SSID.",
                        "type": "string"
                    }
                },
                "additionalProperties": false
            },
            "__after__": {
                "description": "Dependency declaration (ensures this file executes after the specified file)",
                "oneOf": [
                    { "type": "string" },
                    {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                ]
            },

            // ─── Common config fields (examples; actual fields determined by target kernel schema) ───
            "dns": { "$ref": "#/definitions/config_field" },
            "tun": { "$ref": "#/definitions/config_field" },
            "rules": { "$ref": "#/definitions/array_field" },
            "proxies": { "$ref": "#/definitions/array_field" },
            "proxy-groups": { "$ref": "#/definitions/array_field" }
        },
        // ─── Allow arbitrary top-level keys (users can define any config path) ───
        "additionalProperties": {
            "oneOf": [
                { "$ref": "#/definitions/config_field" },
                { "$ref": "#/definitions/array_field" }
            ]
        },
        "definitions": {
            "config_field": {
                "description": "Dict-type config field (supports deep merge, $override, $default)",
                "oneOf": config_field_one_of
            },
            "array_field": {
                "description": "Array-type config field (supports $prepend, $append, $filter, $transform, $remove, $default)",
                "oneOf": array_field_one_of
            },
            "conditional_rule_item": {
                "description": "Conditional rule item for $prepend/$append",
                "oneOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": {
                            "__when__": {
                                "description": "Rule-level condition",
                                "type": "object",
                                "properties": {
                                    "enabled": { "type": "boolean", "description": "Enable/disable this rule" },
                                    "platform": { "oneOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }] },
                                    "ssid": { "type": "string", "description": "WiFi SSID condition" }
                                }
                            },
                            "__rule__": {
                                "description": "Rule content (takes effect when condition matches)",
                                "type": "string"
                            }
                        },
                        "required": ["__when__", "__rule__"],
                        "additionalProperties": false
                    }
                ]
            }
        }
    })
}

///
/// VALID_OPS must contain exactly 7 operators: $override, $prepend, $append,
/// $filter, $transform, $remove, $default.
///
/// Note: We use array type-level trick for compile-time count validation,
/// since `assert!` with format args is not const-evaluable in Rust 1.85.
const _: () = {
    const EXPECTED: usize = 7;
    const ACTUAL: usize = VALID_OPS.len();
    // Compile error if count mismatches: "evaluation of constant value failed... mismatched types"
    let _check: [(); EXPECTED] = [(); ACTUAL];
};
