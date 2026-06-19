//! Live apiserver parity tests — GATED, require a running kind cluster.
//!
//! Drives the shared [`common::cases`] table against the real kube-apiserver (via
//! `kubectl apply --dry-run=server` over a CRD wrapping each schema; server
//! dry-run runs CEL admission but persists nothing) and asserts kube-cel's
//! [`Validator`] agrees on every case. This pins kube-cel to ground truth and
//! catches divergences a hand-written expectation would miss. The same table is
//! also exercised in-process by `envtest_parity.rs`; any disagreement between the
//! two runners is itself a finding.
//!
//! The single test is `#[ignore]`, so `cargo test` skips it (no cluster in CI).
//! Run via `just parity`, which provisions a throwaway kind cluster and exports
//! `KUBE_CEL_PARITY_CTX`.
#![cfg(feature = "validation")]

use std::{
    io::Write,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

mod common;
use common::{Expect, Verdict, cases, kubecel_verdict, kubecel_verdict_with_defaults};

fn ctx() -> String {
    std::env::var("KUBE_CEL_PARITY_CTX")
        .expect("set KUBE_CEL_PARITY_CTX to a kubectl context (use `just parity`)")
}

/// Run kubectl against the parity cluster, optionally piping `stdin`.
/// Returns `(success, combined stdout+stderr)`.
fn kubectl(args: &[&str], stdin: Option<&str>) -> (bool, String) {
    let c = ctx();
    let mut child = Command::new("kubectl")
        .arg("--context")
        .arg(&c)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kubectl");
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

/// Build the CRD wrapping `schema` under a unique `kind`/`plural` in `x.test`.
fn crd_for(kind: &str, plural: &str, schema: &Value) -> Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": { "name": format!("{plural}.x.test") },
        "spec": {
            "group": "x.test",
            "names": { "kind": kind, "plural": plural, "singular": plural },
            "scope": "Namespaced",
            "versions": [{
                "name": "v1", "served": true, "storage": true,
                "schema": { "openAPIV3Schema": schema }
            }]
        }
    })
}

/// Apply a CRD wrapping `schema` under a unique `kind`, then server-dry-run the
/// `object` as a CR and return the apiserver's CEL-admission verdict.
fn apiserver_verdict(kind: &str, plural: &str, schema: &Value, object: &Value) -> Verdict {
    let crd = crd_for(kind, plural, schema);
    let (ok, out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    assert!(ok, "CRD apply failed for {kind}: {out}");
    kubectl(
        &[
            "wait",
            "--for=condition=Established",
            &format!("crd/{plural}.x.test"),
            "--timeout=30s",
        ],
        None,
    );

    let mut cr = object.clone();
    cr["apiVersion"] = json!("x.test/v1");
    cr["kind"] = json!(kind);
    cr["metadata"] = json!({ "name": "probe" });
    let (ok, out) = kubectl(&["apply", "--dry-run=server", "-f", "-"], Some(&cr.to_string()));
    if ok {
        Verdict::Accept
    } else if out.contains("is invalid") || out.contains("Invalid value") {
        Verdict::Reject
    } else {
        panic!("unexpected apiserver error (not a CEL rejection) for {kind}: {out}");
    }
}

/// Apply only the CRD (no CR) and report whether the apiserver REJECTED it at
/// registration — e.g. the rule fails to compile against the schema's types.
fn apiserver_registration_rejected(kind: &str, plural: &str, schema: &Value) -> bool {
    let crd = crd_for(kind, plural, schema);
    let (ok, _out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    !ok
}

/// Drive the entire shared table through the kind/kubectl path. For each case:
/// either pin a CRD-registration rejection, or feed the identical (schema,
/// object) to the apiserver and both kube-cel entry points and require all three
/// to match the expected verdict.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn parity_against_kind() {
    let cases = cases();
    let total = cases.len();
    for (i, c) in cases.iter().enumerate() {
        let n = i + 1;
        match c.expect {
            Expect::RegistrationRejected => {
                assert!(
                    apiserver_registration_rejected(c.kind, c.plural, &c.schema),
                    "[{n}/{total}] {}: expected registration rejection — {}",
                    c.kind,
                    c.note
                );
            }
            Expect::Accept | Expect::Reject => {
                let want = c.expect.verdict();
                let api = apiserver_verdict(c.kind, c.plural, &c.schema, &c.object);
                let kc = kubecel_verdict(&c.schema, &c.object);
                let kcd = kubecel_verdict_with_defaults(&c.schema, &c.object);
                assert_eq!(
                    api, want,
                    "[{n}/{total}] {}: apiserver verdict drifted from expected ({})\n  schema={}\n  object={}",
                    c.kind, c.note, c.schema, c.object
                );
                assert_eq!(
                    kc, api,
                    "[{n}/{total}] {} DIVERGENCE: apiserver={api:?} but kube-cel validate={kc:?} ({})\n  schema={}\n  object={}",
                    c.kind, c.note, c.schema, c.object
                );
                assert_eq!(
                    kcd, api,
                    "[{n}/{total}] {} DIVERGENCE: apiserver={api:?} but kube-cel validate_with_defaults={kcd:?} ({})\n  schema={}\n  object={}",
                    c.kind, c.note, c.schema, c.object
                );
            }
        }
        println!("ok {n}/{total} {} — {}", c.kind, c.note);
    }
    println!("parity_against_kind: {total}/{total} cases agree with the apiserver");
}
