//! Validate fields with `format: date-time` and `format: duration`.
//!
//! Run with: `cargo run --example timestamp_duration --features validation`

use kube_cel::{
    compilation::compile_schema,
    validation::{validate, validate_compiled},
};
use serde_json::json;

fn main() {
    // Schema with date-time and duration formats
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {
                    "expiresAt": {
                        "type": "string",
                        "format": "date-time"
                    },
                    "timeout": {
                        "type": "string",
                        "format": "duration"
                    }
                },
                "x-kubernetes-validations": [
                    {
                        "rule": "self.expiresAt > timestamp('2024-01-01T00:00:00Z')",
                        "message": "must expire after 2024-01-01"
                    },
                    {
                        "rule": "self.timeout <= duration('1h')",
                        "message": "timeout must be at most 1 hour"
                    }
                ]
            }
        }
    });

    // ── Schema-based validation ──────────────────────────────────────

    println!("=== Schema-based validation ===\n");

    let valid = json!({
        "spec": {
            "expiresAt": "2025-06-15T12:00:00Z",
            "timeout": "30m"
        }
    });
    let errors = validate(&schema, &valid, None);
    println!("Valid object: {} errors", errors.len());

    let invalid = json!({
        "spec": {
            "expiresAt": "2023-06-15T12:00:00Z",
            "timeout": "2h"
        }
    });
    let errors = validate(&schema, &invalid, None);
    println!("Invalid object: {} errors", errors.len());
    for err in &errors {
        println!("  [{path}] {msg}", path = err.field_path, msg = err.message);
    }

    // ── Compiled schema (pre-compile once, validate many) ────────────

    println!("\n=== Compiled schema validation ===\n");

    let compiled = compile_schema(&schema);

    let objects = vec![
        json!({"spec": {"expiresAt": "2025-01-01T00:00:00Z", "timeout": "30s"}}),
        json!({"spec": {"expiresAt": "2023-12-31T23:59:59Z", "timeout": "45m"}}),
        json!({"spec": {"expiresAt": "2025-06-15T00:00:00Z", "timeout": "90m"}}),
    ];

    for (i, obj) in objects.iter().enumerate() {
        let errors = validate_compiled(&compiled, obj, None);
        if errors.is_empty() {
            println!("Object {i}: OK");
        } else {
            println!("Object {i}: {} error(s)", errors.len());
            for err in &errors {
                println!("  [{path}] {msg}", path = err.field_path, msg = err.message);
            }
        }
    }

    // ── Transition rule with timestamps ──────────────────────────────

    println!("\n=== Transition rule (expiration cannot move earlier) ===\n");

    let transition_schema = json!({
        "type": "object",
        "properties": {
            "expiresAt": {
                "type": "string",
                "format": "date-time"
            }
        },
        "x-kubernetes-validations": [{
            "rule": "self.expiresAt >= oldSelf.expiresAt",
            "message": "expiration cannot be moved earlier"
        }]
    });

    let new_obj = json!({"expiresAt": "2025-12-01T00:00:00Z"});
    let old_obj = json!({"expiresAt": "2025-06-01T00:00:00Z"});

    let errors = validate(&transition_schema, &new_obj, Some(&old_obj));
    println!("Extend expiration: {} errors", errors.len());

    let bad_obj = json!({"expiresAt": "2025-01-01T00:00:00Z"});
    let errors = validate(&transition_schema, &bad_obj, Some(&old_obj));
    println!("Shorten expiration: {} errors", errors.len());
    for err in &errors {
        println!("  {err}");
    }
}
