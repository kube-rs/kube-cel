//! Pre-compile a schema once, then validate many objects.
//!
//! Run with: `cargo run --example compiled_schema --features validation`

use kube_cel::{compilation::compile_schema, validation::validate_compiled};
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "x-kubernetes-validations": [{
                    "rule": "self.replicas >= self.minReplicas",
                    "message": "replicas must be >= minReplicas",
                    "messageExpression": "'replicas is ' + string(self.replicas) + ' but minReplicas is ' + string(self.minReplicas)"
                }],
                "properties": {
                    "replicas": {
                        "type": "integer",
                        "x-kubernetes-validations": [
                            {"rule": "self >= 0", "message": "must be non-negative"}
                        ]
                    },
                    "minReplicas": {"type": "integer"}
                }
            }
        }
    });

    // Compile once
    let compiled = compile_schema(&schema);
    println!("Schema compiled successfully.\n");

    // Validate many objects
    let objects = vec![
        json!({"spec": {"replicas": 3, "minReplicas": 1}}),
        json!({"spec": {"replicas": -1, "minReplicas": 0}}),
        json!({"spec": {"replicas": 1, "minReplicas": 5}}),
        json!({"spec": {"replicas": 10, "minReplicas": 2}}),
    ];

    for (i, obj) in objects.iter().enumerate() {
        let errors = validate_compiled(&compiled, obj, None);
        let replicas = obj["spec"]["replicas"].as_i64().unwrap();
        let min = obj["spec"]["minReplicas"].as_i64().unwrap();
        if errors.is_empty() {
            println!("Object {i}: replicas={replicas}, min={min} -> OK");
        } else {
            println!(
                "Object {i}: replicas={replicas}, min={min} -> {} error(s)",
                errors.len()
            );
            for err in &errors {
                println!("  [{path}] {msg}", path = err.field_path, msg = err.message);
            }
        }
    }
}
