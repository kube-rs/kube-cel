//! Live apiserver parity tests — GATED, require a running kind cluster.
//!
//! Each case feeds ONE `openAPIV3Schema` (carrying an `x-kubernetes-validations`
//! rule) plus an instance to BOTH:
//!   - the real kube-apiserver, via `kubectl apply --dry-run=server` against a
//!     CRD wrapping the schema (server dry-run runs CEL admission but persists
//!     nothing), and
//!   - kube-cel's [`Validator`],
//! then asserts the two verdicts agree. This pins kube-cel to ground truth and
//! catches divergences a hand-written expectation would miss.
//!
//! Every test is `#[ignore]`, so `cargo test` skips them (no cluster in CI).
//! Run via `just parity`, which provisions a throwaway kind cluster and exports
//! `KUBE_CEL_PARITY_CTX`.
#![cfg(feature = "validation")]

use std::{
    io::Write,
    process::{Command, Stdio},
};

use kube_cel::Validator;
use serde_json::{Value, json};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verdict {
    Accept,
    Reject,
}

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

/// Apply a CRD wrapping `schema` under a unique `kind`, then server-dry-run the
/// `object` as a CR and return the apiserver's CEL-admission verdict.
fn apiserver_verdict(kind: &str, plural: &str, schema: &Value, object: &Value) -> Verdict {
    let crd = json!({
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
    });
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
/// Used to pin rules that are not even expressible against the real apiserver.
fn apiserver_registration_rejected(kind: &str, plural: &str, schema: &Value) -> bool {
    let crd = json!({
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
    });
    let (ok, _out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    !ok
}

fn kubecel_verdict(schema: &Value, object: &Value) -> Verdict {
    match Validator::new().validate(schema, object, None) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
}

/// Feed the identical (schema, object) to both engines and require agreement.
/// Returns the (shared) verdict so the caller can also pin its direction.
fn assert_parity(kind: &str, plural: &str, schema: Value, object: Value) -> Verdict {
    let api = apiserver_verdict(kind, plural, &schema, &object);
    let kc = kubecel_verdict(&schema, &object);
    assert_eq!(
        api, kc,
        "DIVERGENCE: apiserver={api:?} but kube-cel={kc:?}\n  schema={schema}\n  object={object}"
    );
    api
}

/// `map[string]string` (additionalProperties object) with the forbidden literal
/// key present. The #8 case: both must REJECT (map keys not escaped).
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn additional_properties_object_forbidden_key() {
    let schema = json!({
        "type": "object",
        "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
        "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
    });
    let v = assert_parity("PmapObj", "pmapobjs", schema, json!({"m": {"a.b/c": "x"}}));
    assert_eq!(v, Verdict::Reject, "forbidden map key must be rejected");
}

/// Free-form map (`additionalProperties: true`) makes `self.m` opaque: the
/// apiserver rejects `'k' in self.m` at REGISTRATION ("no matching overload for
/// '@in'"). So the #8 escaping concern is unreachable through a valid CRD for
/// free-form maps — only TYPED maps (`additionalProperties: {schema}`) support
/// membership. kube-cel's broader fix for the `true` case is thus harmless
/// belt-and-suspenders, not a reachable-divergence fix.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn free_form_map_in_operator_not_expressible() {
    let schema = json!({
        "type": "object",
        "properties": { "m": { "type": "object", "additionalProperties": true } },
        "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
    });
    assert!(
        apiserver_registration_rejected("PmapTrue", "pmaptrues", &schema),
        "apiserver should reject `in` on a free-form map at registration"
    );
}

/// `[]map[string]string` — a forbidden key sits inside a LIST ELEMENT's map.
/// Exercises escaping skip through `items` recursion. Both must REJECT.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn list_of_typed_maps_forbidden_key() {
    let schema = json!({
        "type": "object",
        "properties": {
            "l": { "type": "array", "items": { "type": "object", "additionalProperties": {"type": "string"} } }
        },
        "x-kubernetes-validations": [{ "rule": "self.l.all(e, !('a.b/c' in e))", "message": "forbidden key" }]
    });
    let v = assert_parity("PlistMap", "plistmaps", schema, json!({"l": [{"a.b/c": "x"}]}));
    assert_eq!(
        v,
        Verdict::Reject,
        "forbidden key in a list-element map must be rejected"
    );
}

/// `map[string]map[string]string` — a forbidden key sits inside a NESTED map
/// value. Exercises escaping skip through `additionalProperties` recursion AND
/// that the outer map key is itself literal. Both must REJECT.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn map_of_typed_maps_forbidden_key() {
    let schema = json!({
        "type": "object",
        "properties": {
            "m": {
                "type": "object",
                "additionalProperties": { "type": "object", "additionalProperties": {"type": "string"} }
            }
        },
        "x-kubernetes-validations": [{ "rule": "self.m.all(k, !('a.b/c' in self.m[k]))", "message": "forbidden key" }]
    });
    let v = assert_parity(
        "PmapMap",
        "pmapmaps",
        schema,
        json!({"m": {"grp": {"a.b/c": "x"}}}),
    );
    assert_eq!(
        v,
        Verdict::Reject,
        "forbidden key in a nested map value must be rejected"
    );
}

/// Map key that collides with a CEL reserved word (`in`). The rule REQUIRES the
/// key, so a literal (unescaped) key means both ACCEPT.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn reserved_word_map_key() {
    let schema = json!({
        "type": "object",
        "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
        "x-kubernetes-validations": [{ "rule": "'in' in self.m", "message": "needs in" }]
    });
    let v = assert_parity("PmapResv", "pmapresvs", schema, json!({"m": {"in": "x"}}));
    assert_eq!(
        v,
        Verdict::Accept,
        "literal reserved-word map key must satisfy the rule"
    );
}

/// Control: a harmless map key satisfies the forbidden-key rule, so both ACCEPT.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn harmless_map_key() {
    let schema = json!({
        "type": "object",
        "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
        "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
    });
    let v = assert_parity("PmapOk", "pmapoks", schema, json!({"m": {"harmless": "x"}}));
    assert_eq!(v, Verdict::Accept);
}

/// Declared struct fields keep field-name escaping: a dotted property `foo.bar`
/// is reachable ONLY via its escaped identifier `foo__dot__bar` (the apiserver
/// rejects `self['foo.bar']` indexing on a struct). Proves kube-cel escapes
/// declared properties the same way — the regression-guard direction of #8.
#[test]
#[ignore = "needs a live kind cluster; run via `just parity`"]
fn declared_property_uses_escaped_identifier() {
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": { "foo.bar": { "type": "string" } },
                "x-kubernetes-validations": [{ "rule": "self.foo__dot__bar == 'ok'", "message": "bad" }]
            }
        }
    });
    // Rule lives on spec, so `self` = spec; validate the spec node directly.
    let v = apiserver_verdict(
        "PstructEsc",
        "pstructescs",
        &schema,
        &json!({"spec": {"foo.bar": "ok"}}),
    );
    let kc = kubecel_verdict(&schema["properties"]["spec"], &json!({"foo.bar": "ok"}));
    assert_eq!(v, Verdict::Accept, "apiserver should reach the escaped field");
    assert_eq!(kc, Verdict::Accept, "kube-cel should reach the escaped field too");
}
