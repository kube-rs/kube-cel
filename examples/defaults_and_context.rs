//! Demonstrates schema default injection and root rules reading the object's
//! own `apiVersion` and `kind` (the apiserver declares no `apiVersion`,
//! `apiGroup` or `kind` variable; kube-cel's deprecated `RootContext` bound
//! them before 0.9).
//!
//! Run with: `cargo run --example defaults_and_context --features validation`

use kube_cel::{Validator, apply_defaults};
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "replicas": {
                "type": "integer",
                "default": 1,
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "replicas must be non-negative"}
                ]
            },
            "strategy": {
                "type": "string",
                "default": "RollingUpdate"
            }
        },
        "x-kubernetes-validations": [
            {"rule": "self.apiVersion.startsWith('apps/')", "message": "only apps group allowed"},
            {"rule": "self.kind == 'Deployment'", "message": "must be a Deployment"}
        ]
    });

    // Object with missing fields (would have defaults in K8s)
    let object = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web"},
        "replicas": 3
    });

    // 1. Apply defaults
    let defaulted = apply_defaults(&schema, &object);
    println!("=== Default Injection ===");
    println!("Before: {object}");
    println!("After:  {defaulted}\n");

    // 2. Validate: root rules read the object's own apiVersion and kind
    let validator = Validator::new();
    let result = validator.validate(&schema, &defaulted, None);

    println!("=== Validation Results ===");
    match result {
        Ok(()) => println!("All rules passed!"),
        Err(errors) => {
            for e in &errors {
                println!("[FAIL] {}: {}", e.field_path, e.message);
            }
        }
    }

    // 3. Demonstrate validate_with_defaults convenience method
    // Uses a simpler schema without the root rules, on an empty object.
    println!("\n=== validate_with_defaults ===");
    let simple_schema = json!({
        "type": "object",
        "properties": {
            "replicas": {
                "type": "integer",
                "default": 1,
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "replicas must be non-negative"}
                ]
            },
            "strategy": {
                "type": "string",
                "default": "RollingUpdate"
            }
        }
    });
    let sparse_object = json!({}); // no fields at all
    let result = validator.validate_with_defaults(&simple_schema, &sparse_object, None);
    println!("Validating empty object with defaults applied:");
    match result {
        Ok(()) => println!("All rules passed (replicas defaulted to 1, strategy to RollingUpdate)"),
        Err(errors) => {
            for e in &errors {
                println!("[FAIL] {}", e.message);
            }
        }
    }
}
