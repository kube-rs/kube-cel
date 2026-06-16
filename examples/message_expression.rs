//! Dynamic validation messages with `messageExpression`.
//!
//! A CRD validation rule can carry a `messageExpression` — a CEL expression that
//! produces the failure message at evaluation time, so the message can embed the
//! offending value (mirroring the apiserver's `messageExpression` support). When
//! present and it evaluates to a string, kube-cel uses it; otherwise it falls back
//! to the static `message`.
//!
//! Run with: `cargo run --example message_expression --features validation`

use kube_cel::Validator;
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {
                    "replicas": {"type": "integer"}
                },
                "x-kubernetes-validations": [{
                    "rule": "self.replicas <= 5",
                    // Static fallback, used if messageExpression is absent/non-string.
                    "message": "too many replicas",
                    // Dynamic message: embeds the actual value and the limit.
                    "messageExpression":
                        "'requested ' + string(self.replicas) + ' replicas, max allowed is 5'"
                }]
            }
        }
    });

    let validator = Validator::new();

    // Two invalid objects: the dynamic message differs per object.
    for replicas in [8, 20] {
        let object = json!({"spec": {"replicas": replicas}});
        if let Err(errors) = validator.validate(&schema, &object, None) {
            for err in &errors {
                // Note the message reflects this object's value, not a fixed string.
                println!("replicas={replicas}: {}", err.message);
            }
        }
    }

    // A valid object produces no errors (and so no message at all).
    let ok = json!({"spec": {"replicas": 3}});
    match validator.validate(&schema, &ok, None) {
        Ok(()) => println!("replicas=3: 0 error(s)"),
        Err(errors) => println!("replicas=3: {} error(s)", errors.len()),
    }
}
