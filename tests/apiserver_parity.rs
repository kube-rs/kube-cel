//! Live apiserver parity tests — GATED, require a running kind cluster.
//!
//! Drives the shared [`common::cases`] table against the real kube-apiserver (via
//! `kubectl apply --dry-run=server` over a CRD wrapping each schema; server
//! dry-run runs CEL admission but persists nothing) and asserts kube-cel's
//! [`Validator`] agrees on every case. This pins kube-cel to ground truth and
//! catches divergences a hand-written expectation would miss.
//!
//! The tests are `#[ignore]`, so `cargo test` skips them (no cluster in CI).
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
    transition_cases,
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

/// Apply a CRD wrapping `schema` under a unique `kind` and wait until it serves.
fn register_crd(kind: &str, plural: &str, schema: &Value) {
    let crd = crd_for(kind, plural, schema);
    let (ok, out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    assert!(ok, "CRD apply failed for {kind}: {out}");
    let (ok, out) = kubectl(
        &[
            "wait",
            "--for=condition=Established",
            &format!("crd/{plural}.x.test"),
            "--timeout=30s",
        ],
        None,
    );
    assert!(ok, "CRD {plural}.x.test never became Established: {out}");
}

/// `object` as a `probe` CR of `kind`, with `metadata` extended by `meta`.
fn cr_for(kind: &str, object: &Value, meta: Value) -> Value {
    let mut cr = object.clone();
    cr["apiVersion"] = json!("x.test/v1");
    cr["kind"] = json!(kind);
    cr["metadata"] = json!({ "name": "probe" });
    if let (Some(md), Some(extra)) = (cr["metadata"].as_object_mut(), meta.as_object()) {
        md.extend(extra.clone());
    }
    cr
}

/// Read a server-dry-run outcome as a CEL-admission verdict.
fn dry_run_verdict(kind: &str, (ok, out): (bool, String)) -> Verdict {
    if ok {
        Verdict::Accept
    } else if out.contains("is invalid") || out.contains("Invalid value") {
        Verdict::Reject
    } else {
        panic!("unexpected apiserver error (not a CEL rejection) for {kind}: {out}");
    }
}

/// Apply a CRD wrapping `schema` under a unique `kind`, then server-dry-run the
/// `object` as a CR and return the apiserver's CEL-admission verdict.
fn apiserver_verdict(kind: &str, plural: &str, schema: &Value, object: &Value) -> Verdict {
    register_crd(kind, plural, schema);
    let cr = cr_for(kind, object, json!({}));
    dry_run_verdict(
        kind,
        kubectl(&["apply", "--dry-run=server", "-f", "-"], Some(&cr.to_string())),
    )
}

/// Store `old` as a real CR, then server-dry-run `object` as a full replace of
/// it (PUT, so nulls survive as written) and return the verdict on the update.
fn apiserver_update_verdict(
    kind: &str,
    plural: &str,
    schema: &Value,
    old: &Value,
    object: &Value,
) -> Verdict {
    register_crd(kind, plural, schema);
    let resource = format!("{plural}.x.test");
    // A rerun against the same cluster finds the previous run's object.
    let (ok, out) = kubectl(
        &["delete", &resource, "probe", "--ignore-not-found", "--wait"],
        None,
    );
    assert!(ok, "deleting a stale {kind} failed: {out}");
    let (ok, out) = kubectl(
        &["create", "-f", "-"],
        Some(&cr_for(kind, old, json!({})).to_string()),
    );
    assert!(ok, "{kind}: the old object must pass validation on create: {out}");
    let (ok, rv) = kubectl(
        &[
            "get",
            &resource,
            "probe",
            "-o",
            "jsonpath={.metadata.resourceVersion}",
        ],
        None,
    );
    assert!(ok, "reading {kind}'s resourceVersion failed: {rv}");
    let cr = cr_for(kind, object, json!({ "resourceVersion": rv.trim() }));
    dry_run_verdict(
        kind,
        kubectl(&["replace", "--dry-run=server", "-f", "-"], Some(&cr.to_string())),
    )
}

/// Apply only the CRD (no CR) and return the apiserver's error if it REJECTED
/// it at registration — e.g. the rule fails to compile against the schema's
/// types — or `None` if it registered.
fn apiserver_registration_error(kind: &str, plural: &str, schema: &Value) -> Option<String> {
    let crd = crd_for(kind, plural, schema);
    let applied = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    crd_rejection(kind, plural, applied)
}

/// Read a CRD apply's outcome: the apiserver's error if it REJECTED the CRD,
/// `None` if it accepted it. Any other kubectl failure (no such context,
/// connection refused, unauthorized) panics: it says nothing about the rules,
/// and reading it as a rejection would pass every registration-rejected row.
fn crd_rejection(kind: &str, plural: &str, (ok, out): (bool, String)) -> Option<String> {
    if ok {
        return None;
    }
    // The apiserver's 422 for an invalid CRD, as kubectl prints it.
    let rejected = format!("CustomResourceDefinition \"{plural}.x.test\" is invalid");
    assert!(
        out.contains(&rejected),
        "CRD apply for {kind} failed without the apiserver rejecting it: {out}"
    );
    Some(out)
}

/// kube-cel's verdict on every entry point (`validate`,
/// `validate_with_defaults`, `validate_compiled`), which must agree.
fn kubecel_verdicts(schema: &Value, object: &Value, old: Option<&Value>) -> [Verdict; 3] {
    [
        kubecel_verdict(schema, object, old),
        kubecel_verdict_with_defaults(schema, object, old),
        kubecel_compiled_verdict(schema, object, old),
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
                    kubecel_verdicts(&c.schema, &c.object, None),
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
                    kubecel_verdicts(&c.schema, &c.object, None),
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

/// The CEL source lines in a function-name list: one name per line, `#`
/// comments.
fn listed_names(list: &str) -> Vec<&str> {
    list.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// kube-cel's compile-time name check accepts the functions the apiserver
/// declares (`src/validation/apiserver_functions.txt`). A unit test in
/// `src/validation/references.rs` keeps every function kube-cel registers in
/// that list; this checks the list against the apiserver.
///
/// Each name is called with seven `dyn` arguments, which no overload takes, in
/// a branch never evaluated: globally (`true || f(dyn(1), ..)`) and, for an
/// undotted name, as a member (`true || dyn(0).f(dyn(1), ..)`). The apiserver
/// type-checks every rule when the CRD is registered and refuses a declared
/// function with "found no matching overload", an undeclared one with
/// "undeclared reference". A name passes when some form gets the first and
/// none the second (`namespace` is a reserved identifier, so its global form
/// fails to parse). One CRD carries every probe; it is server-side dry-run, so
/// nothing is registered.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn function_names_match_the_apiserver() {
    let declared = listed_names(include_str!("../src/validation/apiserver_functions.txt"));
    let args = (1..=7)
        .map(|i| format!("dyn({i})"))
        .collect::<Vec<_>>()
        .join(", ");

    // (name, rule indices of its probes)
    let mut probes: Vec<(&str, Vec<usize>)> = Vec::new();
    let mut rules: Vec<Value> = Vec::new();
    for name in &declared {
        let mut forms = vec![format!("true || {name}({args})")];
        if !name.contains('.') {
            forms.push(format!("true || dyn(0).{name}({args})"));
        }
        let first = rules.len();
        rules.extend(forms.into_iter().map(|rule| json!({ "rule": rule })));
        probes.push((name, (first..rules.len()).collect()));
    }
    let crd = crd_for(
        "Pfunctions",
        "pfunctions",
        &json!({ "type": "object", "x-kubernetes-validations": rules }),
    );
    let dry_run = kubectl(&["apply", "--dry-run=server", "-f", "-"], Some(&crd.to_string()));
    let Some(out) = crd_rejection("Pfunctions", "pfunctions", dry_run) else {
        panic!("a CRD of uncompilable probes registered");
    };

    // The apiserver reports each rule's error after its field path.
    let error_of = |i: usize| -> &str {
        let marker = format!("x-kubernetes-validations[{i}].rule:");
        let Some(start) = out.find(&marker) else {
            return "";
        };
        let rest = &out[start + marker.len()..];
        &rest[..rest.find("x-kubernetes-validations[").unwrap_or(rest.len())]
    };
    let mut mismatches = Vec::new();
    for (name, indices) in &probes {
        let errors: Vec<&str> = indices.iter().map(|&i| error_of(i)).collect();
        let ok = errors.iter().any(|e| e.contains("found no matching overload"))
            && !errors.iter().any(|e| e.contains("undeclared reference"));
        if !ok {
            mismatches.push(format!("{name}: {errors:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "functions listed as declared that the apiserver does not declare:\n{}",
        mismatches.join("\n")
    );
    println!(
        "function_names_match_the_apiserver: {} declared names agree ({} probe rules)",
        declared.len(),
        rules.len()
    );
}

/// Drive the update table: the apiserver stores each case's `old`, then judges
/// `object` as an update of it, so transition rules run against a real
/// `oldSelf`. kube-cel gets `(object, Some(old))` on every entry point.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn transition_parity_against_kind() {
    let cases = transition_cases();
    let total = cases.len();
    for (i, c) in cases.iter().enumerate() {
        let n = i + 1;
        let want = c.expect.verdict();
        let api = apiserver_update_verdict(c.kind, c.plural, &c.schema, &c.old, &c.object);
        assert_eq!(
            api, want,
            "[{n}/{total}] {}: apiserver verdict drifted from expected ({})\n  schema={}\n  old={}\n  object={}",
            c.kind, c.note, c.schema, c.old, c.object
        );
        assert_eq!(
            kubecel_verdicts(&c.schema, &c.object, Some(&c.old)),
            [api; 3],
            "[{n}/{total}] {} DIVERGENCE: apiserver={api:?} vs kube-cel (validate, validate_with_defaults, \
             validate_compiled) ({})\n  schema={}\n  old={}\n  object={}",
            c.kind,
            c.note,
            c.schema,
            c.old,
            c.object
        );
        println!("ok {n}/{total} {} — {}", c.kind, c.note);
    }
    println!("transition_parity_against_kind: {total}/{total} update cases as expected");
}
