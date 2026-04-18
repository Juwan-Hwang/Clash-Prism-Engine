//! Fuzz target: YAML Round-Trip
//!
//! Fuzzes the YAML parsing pipeline: raw bytes → serde_yml::Value → serde_json::Value.
//! This catches edge cases in YAML deserialization and type conversion.
//!
//! ## Run
//!
//! ```bash
//! cargo +nightly fuzz run fuzz_yaml_roundtrip
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);

    // Step 1: Parse as YAML
    let yaml_value: Result<serde_yml::Value, _> = serde_yml::from_str(&input);
    let yaml_value = match yaml_value {
        Ok(v) => v,
        Err(_) => return, // Invalid YAML is expected for fuzz input
    };

    // Step 2: Convert to JSON Value (yaml_value_to_json equivalent)
    let json_value: serde_json::Value = serde_json::to_value(&yaml_value).unwrap_or_default();

    // Step 3: Round-trip back to JSON string (should never panic)
    let _ = serde_json::to_string(&json_value);

    // Step 4: Round-trip YAML → JSON → YAML (should never panic)
    let _ = serde_yml::to_string(&yaml_value);
});
