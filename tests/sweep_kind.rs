//! Phase-2 fidelity SWEEP runner — GATED, requires a live kind cluster.
//!
//! Reads every candidate case authored under `target/sweep/*.json` (one file per
//! extension library + the validation engine), then for EACH case measures the
//! ACTUAL bucket by feeding the identical (schema, object) to the real
//! kube-apiserver (kind, via kubectl) and to kube-cel's [`Validator`]:
//!
//! - apiserver refuses the CRD at registration → `kube_cel_only` (or `neither`
//!   if kube-cel also can't compile the rule).
//! - both register, verdicts agree → `faithful_accept` / `faithful_reject`.
//! - apiserver ACCEPTs, kube-cel REJECTs/can't-eval → `apiserver_only`
//!   (over-rejection; safe direction) or `divergent_fail_closed`.
//! - apiserver REJECTs, kube-cel ACCEPTs → `divergent_fail_open` (DANGEROUS).
//!
//! This is a MEASUREMENT, not an assertion suite: it never panics on a single
//! case, records every outcome to `target/sweep/_results.json`, and prints a
//! matrix. The human promotes divergences to issues / curated `cases()` rows.
//!
//! Run via `just sweep` (spins a throwaway kind cluster, exports
//! `KUBE_CEL_PARITY_CTX`). `#[ignore]` so `cargo test` skips it.
#![cfg(feature = "validation")]

use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

use kube_cel::{ErrorKind, Validator};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct LibFile {
    library: String,
    cases: Vec<RawCase>,
}

#[derive(Deserialize)]
struct RawCase {
    #[serde(default)]
    function: String,
    #[serde(default)]
    rule: String,
    schema: Value,
    #[serde(default)]
    object: Value,
    #[serde(default)]
    predicted_bucket: String,
    #[serde(default)]
    reasoning: String,
}

#[derive(Serialize)]
struct ResultRow {
    library: String,
    kind: String,
    function: String,
    rule: String,
    predicted_bucket: String,
    actual_bucket: String,
    apiserver: String,
    kube_cel: String,
    matched: bool,
    detail: String,
    reasoning: String,
}

fn ctx() -> String {
    std::env::var("KUBE_CEL_PARITY_CTX")
        .expect("set KUBE_CEL_PARITY_CTX to a kubectl context (use `just sweep`)")
}

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

/// What the apiserver did with this case.
enum Api {
    Accept,
    Reject,
    RegRejected(String),
    Error(String),
}

fn apiserver(kind: &str, plural: &str, schema: &Value, object: &Value) -> Api {
    let crd = crd_for(kind, plural, schema);
    let (ok, out) = kubectl(&["apply", "-f", "-"], Some(&crd.to_string()));
    if !ok {
        return Api::RegRejected(snippet(&out));
    }
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
    if !cr.is_object() {
        cr = json!({});
    }
    cr["apiVersion"] = json!("x.test/v1");
    cr["kind"] = json!(kind);
    cr["metadata"] = json!({ "name": "probe" });
    let (ok, out) = kubectl(&["apply", "--dry-run=server", "-f", "-"], Some(&cr.to_string()));
    if ok {
        Api::Accept
    } else if out.contains("is invalid") || out.contains("Invalid value") {
        Api::Reject
    } else {
        Api::Error(snippet(&out))
    }
}

/// kube-cel's classification of this case.
#[derive(PartialEq, Eq)]
enum Kc {
    Accept,
    Reject,
    CantCompile,
    Unsupported,
}

fn kube_cel(schema: &Value, object: &Value) -> Kc {
    match Validator::new().validate(schema, object, None) {
        Ok(()) => Kc::Accept,
        Err(e) => {
            let kinds: Vec<&ErrorKind> = e.as_slice().iter().map(|x| &x.kind).collect();
            if kinds
                .iter()
                .any(|k| matches!(k, ErrorKind::CompilationFailure | ErrorKind::InvalidRule))
            {
                Kc::CantCompile
            } else if kinds
                .iter()
                .any(|k| matches!(k, ErrorKind::UnsupportedReference | ErrorKind::SchemaTooDeep))
            {
                Kc::Unsupported
            } else {
                Kc::Reject
            }
        }
    }
}

fn snippet(s: &str) -> String {
    let one = s.replace('\n', " ");
    one.chars().take(200).collect()
}

fn api_label(a: &Api) -> &'static str {
    match a {
        Api::Accept => "accept",
        Api::Reject => "reject",
        Api::RegRejected(_) => "reg_rejected",
        Api::Error(_) => "error",
    }
}

fn kc_label(k: &Kc) -> &'static str {
    match k {
        Kc::Accept => "accept",
        Kc::Reject => "reject",
        Kc::CantCompile => "cant_compile",
        Kc::Unsupported => "unsupported",
    }
}

/// Fold (apiserver, kube-cel) outcomes into the fidelity bucket.
fn bucket(api: &Api, kc: &Kc) -> &'static str {
    match api {
        Api::RegRejected(_) => match kc {
            Kc::CantCompile => "neither",
            _ => "kube_cel_only",
        },
        Api::Error(_) => "apiserver_error",
        Api::Accept => match kc {
            Kc::Accept => "faithful_accept",
            Kc::Reject => "divergent_fail_closed",
            Kc::CantCompile | Kc::Unsupported => "apiserver_only",
        },
        Api::Reject => match kc {
            Kc::Reject => "faithful_reject",
            // both reject the object, but kube-cel via an eval/compile error
            // rather than a clean rule-false: agreement on outcome, noted.
            Kc::CantCompile | Kc::Unsupported => "faithful_reject_via_error",
            Kc::Accept => "divergent_fail_open",
        },
    }
}

/// A predicted bucket "matches" the actual one if they describe the same
/// observable outcome (predictions use a coarser vocabulary than measurement).
fn prediction_matches(predicted: &str, actual: &str) -> bool {
    match predicted {
        "faithful_accept" => actual == "faithful_accept",
        "faithful_reject" => actual == "faithful_reject" || actual == "faithful_reject_via_error",
        "kube_cel_only" => actual == "kube_cel_only" || actual == "neither",
        "apiserver_only" => actual == "apiserver_only",
        "divergent" => actual == "divergent_fail_open" || actual == "divergent_fail_closed",
        _ => false,
    }
}

fn sanitize(lib: &str) -> String {
    let mut out: String = lib.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

#[test]
#[ignore = "needs a live kind cluster; run via `just sweep`"]
fn sweep_against_kind() {
    // Candidate cases are authored by the agent fan-out into this (gitignored)
    // directory; a fresh checkout has none, so skip cleanly rather than panic.
    let dir = "target/sweep";
    let Ok(entries) = fs::read_dir(dir) else {
        println!("no {dir}/ — author candidate cases (agent fan-out) first; skipping sweep.");
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('_'))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        println!("no candidate JSON under {dir}/; skipping sweep.");
        return;
    }

    let mut rows: Vec<ResultRow> = Vec::new();
    for path in &files {
        let txt = fs::read_to_string(path).unwrap();
        let lib: LibFile = match serde_json::from_str(&txt) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("SKIP {path:?}: bad JSON: {e}");
                continue;
            }
        };
        let base = sanitize(&lib.library);
        for (i, c) in lib.cases.iter().enumerate() {
            let kind = format!("Sw{base}{i}");
            let plural = kind.to_ascii_lowercase();
            let api = apiserver(&kind, &plural, &c.schema, &c.object);
            let kc = kube_cel(&c.schema, &c.object);
            let actual = bucket(&api, &kc).to_string();
            let matched = prediction_matches(&c.predicted_bucket, &actual);
            let detail = match &api {
                Api::RegRejected(s) | Api::Error(s) => s.clone(),
                _ => String::new(),
            };
            let flag = if matched { "  " } else { "!!" };
            println!(
                "{flag} [{}] {} :: pred={} actual={} (api={}, kc={})",
                lib.library,
                c.function,
                c.predicted_bucket,
                actual,
                api_label(&api),
                kc_label(&kc),
            );
            rows.push(ResultRow {
                library: lib.library.clone(),
                kind,
                function: c.function.clone(),
                rule: c.rule.clone(),
                predicted_bucket: c.predicted_bucket.clone(),
                actual_bucket: actual,
                apiserver: api_label(&api).to_string(),
                kube_cel: kc_label(&kc).to_string(),
                matched,
                detail,
                reasoning: c.reasoning.clone(),
            });
        }
    }

    // Persist full results for the matrix builder.
    let out_path = format!("{dir}/_results.json");
    fs::write(&out_path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();

    // ── Summary ──────────────────────────────────────────────────────────
    let total = rows.len();
    let mut by_bucket: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in &rows {
        *by_bucket.entry(r.actual_bucket.as_str()).or_default() += 1;
    }
    println!("\n===== SWEEP SUMMARY ({total} cases) =====");
    for (b, n) in &by_bucket {
        println!("  {b:28} {n}");
    }
    let fail_open: Vec<&ResultRow> = rows
        .iter()
        .filter(|r| r.actual_bucket == "divergent_fail_open")
        .collect();
    let fail_closed: Vec<&ResultRow> = rows
        .iter()
        .filter(|r| r.actual_bucket == "divergent_fail_closed")
        .collect();
    let mispredicted: Vec<&ResultRow> = rows.iter().filter(|r| !r.matched).collect();
    println!(
        "\nDIVERGENT fail-open (DANGEROUS): {}   fail-closed: {}   mispredicted: {}",
        fail_open.len(),
        fail_closed.len(),
        mispredicted.len()
    );
    for r in &fail_open {
        println!("  FAIL-OPEN  [{}] {} :: {}", r.library, r.function, r.rule);
    }
    for r in &fail_closed {
        println!("  FAIL-CLOSED[{}] {} :: {}", r.library, r.function, r.rule);
    }
    println!("\nfull results -> {out_path}");
}
