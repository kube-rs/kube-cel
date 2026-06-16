//! Validate Kubernetes objects against CRD schema CEL rules.
//!
//! Run with: `cargo run --example validate_crd --features validation`

use kube_cel::Validator;
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {
                    "replicas": {
                        "type": "integer",
                        "x-kubernetes-validations": [
                            {"rule": "self >= 0", "message": "replicas must be non-negative"}
                        ]
                    }
                },
                "x-kubernetes-validations": [
                    {"rule": "self.replicas >= 1", "message": "at least one replica required"}
                ]
            }
        }
    });

    let validator = Validator::new();

    // Valid object
    let valid = json!({"spec": {"replicas": 3}});
    match validator.validate(&schema, &valid, None) {
        Ok(()) => println!("Valid object: OK"),
        Err(errors) => println!("Valid object: {} errors", errors.len()),
    }

    // Invalid object
    let invalid = json!({"spec": {"replicas": -1}});
    if let Err(errors) = validator.validate(&schema, &invalid, None) {
        println!("\nInvalid object: {} errors", errors.len());
        for err in &errors {
            println!("  [{path}] {msg}", path = err.field_path, msg = err.message);
        }
    }

    // Transition rule (update check)
    let transition_schema = json!({
        "type": "object",
        "x-kubernetes-validations": [{
            "rule": "self.replicas >= oldSelf.replicas",
            "message": "cannot scale down"
        }],
        "properties": {
            "replicas": {"type": "integer"}
        }
    });

    let new_obj = json!({"replicas": 2});
    let old_obj = json!({"replicas": 5});

    // Create (no old object): transition rule skipped
    match validator.validate(&transition_schema, &new_obj, None) {
        Ok(()) => println!("\nCreate (no old): OK"),
        Err(errors) => println!("\nCreate (no old): {} errors", errors.len()),
    }

    // Update (scale down): transition rule fires
    if let Err(errors) = validator.validate(&transition_schema, &new_obj, Some(&old_obj)) {
        println!("Update (scale down): {} errors", errors.len());
        for err in &errors {
            println!("  {}", err);
        }
    }
}
