//! The headline use case: client-side CRD validation inside a kube-rs workflow.
//!
//! A controller (or an admission webhook) often builds an object and is about to
//! `apply`/`patch` it to the cluster. Running the CRD's `x-kubernetes-validations`
//! rules *before* the round-trip catches invalid objects locally, surfaces a clear
//! reason on the resource status, and avoids a guaranteed apiserver rejection.
//!
//! This example is intentionally dependency-light: it mocks the two things a real
//! controller would *fetch* — the CRD's OpenAPI schema and the object under
//! reconciliation — with `serde_json`, and marks each spot where a `kube::Api`
//! call would slot in with a comment. That keeps `cargo run` fast and free of the
//! heavy `kube`/`k8s-openapi` build, while showing exactly where validation fits.
//!
//! Run with: `cargo run --example kube_workflow --features validation`

use kube_cel::Validator;
use serde_json::{Value, json};

fn main() {
    // In a real controller this comes from the cluster, e.g.:
    //     use kube::{Api, Client};
    //     use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    //     let crds: Api<CustomResourceDefinition> = Api::all(client);
    //     let crd = crds.get("widgets.example.com").await?;
    //     let schema = crd.spec.versions[0].schema.unwrap().open_api_v3_schema.unwrap();
    // Here we mock that fetched OpenAPI v3 schema directly.
    let schema = fetch_crd_schema();

    let validator = Validator::new();

    // A controller reconcile loop typically builds a *desired* object and applies
    // it. We show two desired states: one valid, one that violates the CRD rules.
    let desired_ok = json!({
        "spec": { "replicas": 3, "image": "registry.example.com/widget:1.4.2" }
    });
    let desired_bad = json!({
        "spec": { "replicas": 0, "image": "widget:latest" }
    });

    for (label, desired) in [("valid desired", &desired_ok), ("invalid desired", &desired_bad)] {
        println!("=== reconcile: {label} ===");

        // Client-side gate, run BEFORE touching the apiserver.
        match validator.validate(&schema, desired, None) {
            Ok(()) => {
                println!("  validation passed -> apply to cluster");
                // The real apply happens only on the happy path:
                //     let widgets: Api<Widget> = Api::namespaced(client, "default");
                //     widgets.patch("my-widget", &PatchParams::apply("my-controller"),
                //                   &Patch::Apply(desired)).await?;
            }
            Err(errors) => {
                println!("  validation failed -> skip apply, record on status:");
                for err in &errors {
                    println!("    [{}] {}", err.field_path, err.message);
                }
                // Instead of a doomed apply, the controller would surface the reason:
                //     widgets.patch_status("my-widget", &pp,
                //         &Patch::Merge(json!({"status": {"validationError": err.message}}))).await?;
            }
        }
        println!();
    }
}

/// Stand-in for the CRD schema a controller fetches from the apiserver.
fn fetch_crd_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "x-kubernetes-validations": [
                    {"rule": "self.replicas >= 1", "message": "replicas must be at least 1"},
                    {
                        "rule": "!self.image.endsWith(':latest')",
                        "message": "pin the image to a concrete tag, not ':latest'"
                    }
                ],
                "properties": {
                    "replicas": {"type": "integer"},
                    "image": {"type": "string"}
                }
            }
        }
    })
}
