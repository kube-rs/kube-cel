//! Demonstrates client-side evaluation of ValidatingAdmissionPolicy CEL expressions.
//!
//! Run with: `cargo run --example vap_evaluation --features validation`

use kube_cel::vap::{AdmissionRequest, GroupVersionKind, VapEvaluator, VapExpression};
use serde_json::json;

fn main() {
    // Simulate an incoming Deployment object
    let object = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": "my-app",
            "namespace": "production",
            "labels": {"app": "my-app"}
        },
        "spec": {
            "replicas": 3,
            "template": {
                "spec": {
                    "containers": [{
                        "name": "app",
                        "image": "my-app:latest"
                    }]
                }
            }
        }
    });

    // Build evaluator with admission context
    let evaluator = VapEvaluator::builder()
        .object(object)
        .request(AdmissionRequest {
            operation: "CREATE".into(),
            username: "developer@example.com".into(),
            namespace: "production".into(),
            name: "my-app".into(),
            kind: GroupVersionKind {
                group: "apps".into(),
                version: "v1".into(),
                kind: "Deployment".into(),
            },
            ..Default::default()
        })
        .params(json!({"maxReplicas": 5, "requiredLabel": "app"}))
        .build();

    // Define policy expressions (from a ValidatingAdmissionPolicy)
    let expressions = vec![
        VapExpression {
            expression: "object.spec.replicas <= params.maxReplicas".into(),
            message: Some("replicas exceeds maximum allowed".into()),
            message_expression: Some(
                "'replicas ' + string(object.spec.replicas) + ' exceeds max ' + string(params.maxReplicas)"
                    .into(),
            ),
        },
        VapExpression {
            expression: "has(object.metadata.labels) && params.requiredLabel in object.metadata.labels"
                .into(),
            message: Some("required label missing".into()),
            message_expression: None,
        },
        VapExpression {
            expression: "!object.spec.template.spec.containers.exists(c, c.image.endsWith(':latest'))".into(),
            message: Some("latest tag is not allowed in production".into()),
            message_expression: None,
        },
    ];

    let results = evaluator.evaluate(&expressions);

    println!("=== VAP Evaluation Results ===\n");
    for result in &results {
        let status = if result.passed { "PASS" } else { "FAIL" };
        println!("[{status}] {}", result.expression);
        if let Some(msg) = &result.message {
            println!("       {msg}");
        }
    }

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    println!(
        "\n{} passed, {} failed",
        results.len() - failures.len(),
        failures.len()
    );
}
