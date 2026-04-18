//! Fuzz target: DSL Parser
//!
//! Fuzzes `DslParser::parse_str()` with arbitrary YAML-like input.
//! The parser should never panic — any invalid input should produce
//! a graceful `Err(PrismError)` result.
//!
//! ## Run
//!
//! ```bash
//! cargo +nightly fuzz run fuzz_dsl_parser
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use clash_prism_dsl::DslParser;

fuzz_target!(|data: &[u8]| {
    // Convert arbitrary bytes to a UTF-8 string, replacing invalid sequences.
    // This ensures we test with valid Unicode (as YAML requires) while still
    // exercising edge cases like null bytes, control characters, etc.
    let input = String::from_utf8_lossy(data);

    // The parser should NEVER panic on any input.
    // Invalid YAML → Err, valid YAML with bad ops → Err, valid YAML → Ok.
    let _ = DslParser::parse_str(&input, None);
});
