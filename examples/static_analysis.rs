//! Demonstrates static analysis of CEL validation rules.
//!
//! Run with: `cargo run --example static_analysis --features validation`

use kube_cel::{
    analysis::{self, ScopeContext},
    compilation::compile_schema,
};
use serde_json::json;

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "maxLength": 253},
            "tags": {
                "type": "array",
                "items": {"type": "string"}
                // Note: no maxItems — cost estimator will warn
            },
            "replicas": {"type": "integer"}
        }
    });
    let compiled = compile_schema(&schema);

    let rules = [
        // Good rule: simple comparison, low cost
        "self.replicas >= 0",
        // Good rule: bounded string operation
        "self.name.size() > 0",
        // Potentially expensive: unbounded list comprehension
        "self.tags.all(tag, tag.size() > 0)",
        // Wrong scope: uses admission policy variable in CRD context
        "request.userInfo.username != 'admin'",
    ];

    println!("=== CEL Rule Analysis ===\n");

    for rule in &rules {
        println!("Rule: {rule}");

        // Combined analysis: scope + cost in one compilation
        let warnings = analysis::analyze_rule(rule, &compiled, ScopeContext::CrdValidation);

        if warnings.is_empty() {
            println!("  OK — no issues found\n");
        } else {
            for w in &warnings {
                println!("  [{:?}] {}", w.kind, w.message);
            }
            println!();
        }
    }
}
