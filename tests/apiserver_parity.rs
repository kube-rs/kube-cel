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
use common::{
    Expect, Verdict, cases, kubecel_compiled_verdict, kubecel_verdict, kubecel_verdict_with_defaults,
};

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

/// Apply only the CRD (no CR) and return the apiserver's error if it REJECTED
/// it at registration — e.g. the rule fails to compile against the schema's
/// types — or `None` if it registered.
fn apiserver_registration_error(kind: &str, plural: &str, schema: &Value) -> Option<String> {
    let crd = crd_for(kind, plural, schema);
    let (ok, out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    (!ok).then_some(out)
}

/// kube-cel's verdict on every entry point (`validate`,
/// `validate_with_defaults`, `validate_compiled`), which must agree.
fn kubecel_verdicts(schema: &Value, object: &Value) -> [Verdict; 3] {
    [
        kubecel_verdict(schema, object),
        kubecel_verdict_with_defaults(schema, object),
        kubecel_compiled_verdict(schema, object),
    ]
}

/// Drive the entire shared table through the kind/kubectl path. For each case:
/// either pin a CRD-registration rejection (and kube-cel's verdict on a valid
/// object: a rejection, or an acceptance for the documented no-type-checker
/// fail-open class), or feed the identical (schema, object) to the apiserver
/// and every kube-cel entry point and require all of them to match the
/// expected verdict.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn parity_against_kind() {
    let cases = cases();
    let total = cases.len();
    let mut fail_open = 0;
    for (i, c) in cases.iter().enumerate() {
        let n = i + 1;
        let mut registration_error = None;
        match c.expect {
            Expect::RegistrationRejected | Expect::RegistrationRejectedFailOpen => {
                let Some(err) = apiserver_registration_error(c.kind, c.plural, &c.schema) else {
                    panic!(
                        "[{n}/{total}] {}: expected registration rejection — {}",
                        c.kind, c.note
                    );
                };
                let want = if c.expect == Expect::RegistrationRejected {
                    Verdict::Reject
                } else {
                    Verdict::Accept
                };
                assert_eq!(
                    kubecel_verdicts(&c.schema, &c.object),
                    [want; 3],
                    "[{n}/{total}] {}: kube-cel verdicts (validate, validate_with_defaults, validate_compiled) \
                     on a registration-rejected rule ({})\n  schema={}\n  object={}",
                    c.kind,
                    c.note,
                    c.schema,
                    c.object
                );
                fail_open += usize::from(want == Verdict::Accept);
                registration_error = err.lines().find(|l| !l.trim().is_empty()).map(str::to_owned);
            }
            Expect::Accept | Expect::Reject => {
                let want = c.expect.verdict();
                let api = apiserver_verdict(c.kind, c.plural, &c.schema, &c.object);
                assert_eq!(
                    api, want,
                    "[{n}/{total}] {}: apiserver verdict drifted from expected ({})\n  schema={}\n  object={}",
                    c.kind, c.note, c.schema, c.object
                );
                assert_eq!(
                    kubecel_verdicts(&c.schema, &c.object),
                    [api; 3],
                    "[{n}/{total}] {} DIVERGENCE: apiserver={api:?} vs kube-cel (validate, validate_with_defaults, \
                     validate_compiled) ({})\n  schema={}\n  object={}",
                    c.kind,
                    c.note,
                    c.schema,
                    c.object
                );
            }
        }
        println!("ok {n}/{total} {} — {}", c.kind, c.note);
        if let Some(err) = registration_error {
            println!("   apiserver: {err}");
        }
    }
    println!(
        "parity_against_kind: {total}/{total} cases as expected ({fail_open} of them the pinned \
         no-type-checker fail-open, kube-cel accepting what the apiserver refuses at registration)"
    );
}
