//! Demonstrates schema default injection and root-level variable binding.
//!
//! Run with: `cargo run --example defaults_and_context --features validation`

use kube_cel::defaults::apply_defaults;
use kube_cel::validation::{RootContext, Validator};
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
            {"rule": "apiGroup == 'apps'", "message": "only apps group allowed"},
            {"rule": "kind == 'Deployment'", "message": "must be a Deployment"}
        ]
    });

    // Object with missing fields (would have defaults in K8s)
    let object = json!({"replicas": 3});

    // 1. Apply defaults
    let defaulted = apply_defaults(&schema, &object);
    println!("=== Default Injection ===");
    println!("Before: {object}");
    println!("After:  {defaulted}\n");

    // 2. Validate with root context
    let root_ctx = RootContext {
        api_version: "apps/v1".into(),
        api_group: "apps".into(),
        kind: "Deployment".into(),
    };

    let validator = Validator::new();
    let errors = validator.validate_with_context(&schema, &defaulted, None, Some(&root_ctx));

    println!("=== Validation Results ===");
    if errors.is_empty() {
        println!("All rules passed!");
    } else {
        for e in &errors {
            println!("[FAIL] {}: {}", e.field_path, e.message);
        }
    }

    // 3. Demonstrate validate_with_defaults convenience method
    // Uses a simpler schema without root-context variables (apiGroup/kind),
    // since validate_with_defaults does not accept a RootContext.
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
    let errors = validator.validate_with_defaults(&simple_schema, &sparse_object, None);
    println!("Validating empty object with defaults applied:");
    if errors.is_empty() {
        println!("All rules passed (replicas defaulted to 1, strategy to RollingUpdate)");
    } else {
        for e in &errors {
            println!("[FAIL] {}", e.message);
        }
    }
}
