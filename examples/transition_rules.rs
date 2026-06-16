//! Transition rules (`oldSelf`): validating an UPDATE against the previous state.
//!
//! A CRD rule that references `oldSelf` is a *transition rule*: it compares the
//! incoming object to the stored one. The apiserver only runs transition rules on
//! UPDATE (there is no previous state on CREATE), and so does kube-cel — pass the
//! old object as the third argument to [`Validator::validate`]. On CREATE
//! (`old = None`) transition rules are skipped; ordinary rules still run.
//!
//! This focuses on the UPDATE-only patterns (immutability, monotonic change). For
//! a broad mixed-rule walkthrough see the `validate_crd` example.
//!
//! Run with: `cargo run --example transition_rules --features validation`

use kube_cel::{ValidationErrors, Validator};
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "storageClass": {"type": "string"},
            "replicas": {"type": "integer"}
        },
        "x-kubernetes-validations": [
            // Immutable field: may never change once set.
            {
                "rule": "self.storageClass == oldSelf.storageClass",
                "message": "storageClass is immutable"
            },
            // Monotonic: replicas may only grow (no scale-down).
            {
                "rule": "self.replicas >= oldSelf.replicas",
                "message": "replicas cannot be decreased"
            }
        ]
    });

    let validator = Validator::new();
    let stored = json!({"storageClass": "fast-ssd", "replicas": 3});

    // CREATE: no previous state, so transition rules are skipped entirely.
    let created = json!({"storageClass": "fast-ssd", "replicas": 3});
    report("CREATE (old = None)", validator.validate(&schema, &created, None));

    // UPDATE that respects both rules: same storageClass, scaling up.
    let scale_up = json!({"storageClass": "fast-ssd", "replicas": 5});
    report(
        "UPDATE scale up 3->5",
        validator.validate(&schema, &scale_up, Some(&stored)),
    );

    // UPDATE that violates the monotonic rule: scaling down.
    let scale_down = json!({"storageClass": "fast-ssd", "replicas": 1});
    report(
        "UPDATE scale down 3->1",
        validator.validate(&schema, &scale_down, Some(&stored)),
    );

    // UPDATE that mutates an immutable field.
    let restorage = json!({"storageClass": "cheap-hdd", "replicas": 3});
    report(
        "UPDATE change storageClass",
        validator.validate(&schema, &restorage, Some(&stored)),
    );
}

fn report(label: &str, result: Result<(), ValidationErrors>) {
    match result {
        Ok(()) => println!("{label}: OK"),
        Err(errors) => {
            println!("{label}: {} error(s)", errors.len());
            for err in &errors {
                println!("  - {}", err.message);
            }
        }
    }
}
