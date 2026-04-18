//! Fuzz target: Expression Evaluator
//!
//! Fuzzes `evaluate_predicate()` and `evaluate_transform_expr()` with
//! arbitrary expression strings against a fixed proxy item.
//! The evaluator should never panic — any malformed expression should
//! produce a graceful `Err(ExprError)` result.
//!
//! ## Run
//!
//! ```bash
//! cargo +nightly fuzz run fuzz_expr_evaluator
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use clash_prism_core::executor::expr::{evaluate_predicate, evaluate_transform_expr};

/// A representative proxy item for predicate evaluation.
fn sample_proxy() -> serde_json::Value {
    serde_json::json!({
        "name": "香港 IPLC 01",
        "type": "ss",
        "server": "hk1.example.com",
        "port": 443,
        "uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "tls": true,
        "sni": "hk1.example.com",
        "udp": true,
        "network": "ws",
        "cipher": "aes-256-gcm"
    })
}

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    let proxy = sample_proxy();

    // Predicate evaluation should never panic.
    let _ = evaluate_predicate(&input, &proxy);

    // Transform evaluation should never panic.
    let _ = evaluate_transform_expr(&input, &proxy);
});
