//! Inspecting validation errors: the `ErrorKind` classification and the
//! `std::error::Error::source()` cause chain.
//!
//! Every [`ValidationError`] carries a machine-readable [`ErrorKind`] so callers
//! can branch on *why* a rule failed — the object was invalid, the rule hit a
//! runtime error, or the rule used a CEL feature this build cannot evaluate. For
//! the runtime cases the underlying `cel` execution error is preserved and
//! reachable via `source()`, so you can walk the cause chain like any other
//! `std::error::Error`.
//!
//! Run with: `cargo run --example error_chain --features validation`

use std::error::Error;

use kube_cel::{ErrorKind, Validator};
use serde_json::json;

fn main() {
    // Three rules, each failing a different way against the same object.
    let schema = json!({
        "type": "object",
        "x-kubernetes-validations": [
            // (1) the object simply violates the rule -> ValidationFailure
            {"rule": "self.replicas >= 1", "message": "replicas must be at least 1"},
            // (2) the rule references a field that isn't there -> EvaluationError
            //     (compiles fine, errors at runtime; the cel error is the cause)
            {"rule": "self.maxSurge > 0", "message": "maxSurge must be positive"},
            // (3) the rule uses a CEL macro cel 0.13 can't evaluate -> UnsupportedReference
            {"rule": "self.zones.sortBy(z, z) == self.zones", "message": "zones must be sorted"}
        ],
        "properties": {
            "replicas": {"type": "integer"},
            "zones": {"type": "array", "items": {"type": "string"}}
        }
    });

    let object = json!({"replicas": 0, "zones": ["b", "a"]});

    let errors = Validator::new()
        .validate(&schema, &object, None)
        .expect_err("object violates every rule, so validation must fail");

    println!("{} validation error(s)\n", errors.len());
    for err in &errors {
        println!("rule: {}", err.rule);
        println!("  kind: {:?}", err.kind);
        println!("  message: {err}");

        // Branch on the classification. `ErrorKind` is `#[non_exhaustive]`, so a
        // wildcard arm is required.
        match err.kind {
            ErrorKind::ValidationFailure => {
                println!("  -> the object violated the rule; reject it");
            }
            ErrorKind::EvaluationError => {
                println!("  -> the rule failed at runtime; likely a malformed object");
            }
            ErrorKind::UnsupportedReference => {
                println!("  -> coverage gap: this kube-cel build can't evaluate the rule");
            }
            _ => println!("  -> other ({:?})", err.kind),
        }

        // Walk the `source()` cause chain. Runtime failures carry the owned `cel`
        // execution error as their cause; pure ValidationFailure has none.
        let mut cause = err.source();
        if cause.is_none() {
            println!("  (no underlying cause)");
        }
        let mut depth = 1;
        while let Some(e) = cause {
            println!("  {:indent$}caused by: {e}", "", indent = depth * 2);
            cause = e.source();
            depth += 1;
        }
        println!();
    }
}
