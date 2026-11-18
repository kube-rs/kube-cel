//! Expression-level VALUE probe — GATED, requires a live kind cluster.
//!
//! The unit tests in `src/functions/` and `tests/k8s_conformance.rs` assert what
//! *we believe* cel-go and the apiserver produce. This runner checks that belief
//! against ground truth: it harvests every CEL expression literal out of those
//! tests, evaluates each one through kube-cel, renders the result as a CEL
//! literal, and asks a real kube-apiserver whether `<expr> == <our value>` holds
//! (a CRD rule + `kubectl apply --dry-run=server`). A rejected rule means the
//! apiserver disagrees with the value kube-cel computed.
//!
//! Unlike a verdict-only comparison, this pins the *value*, so it also covers the
//! standard-library layer kube-cel inherits from the `cel` crate.
//!
//! Rules are packed in batches per CRD (one round-trip per batch instead of per
//! expression); a batch the apiserver refuses to register is bisected so a single
//! uncompilable rule cannot hide the rest.
//!
//! Like `tests/sweep_kind.rs` this is a MEASUREMENT, not an assertion suite: it
//! never fails on a divergence, it reports them. The human promotes findings to
//! `tests/common/mod.rs` parity rows.
//!
//! It lives inside the crate, as a unit test, so it evaluates with the
//! validation engine's own (private) compile and base context, the environment
//! `Validator` uses, without widening the public API.
//!
//! Run via `just probe` (which provisions the cluster and exports
//! `KUBE_CEL_PARITY_CTX`). `#[ignore]` so `cargo test` skips it.

use std::{
    collections::BTreeSet,
    io::Write,
    process::{Command, Stdio},
};

use cel::{Value, objects::Key};
use serde_json::{Value as Json, json};

/// Test helpers whose first argument is a CEL expression string literal.
const HELPERS: &[&str] = &[
    "eval_str(",
    "eval_bool(",
    "eval(",
    "assert_true(",
    "assert_false(",
    "assert_int(",
    "assert_string(",
    "assert_string_on_kube_env(",
];

/// Rules per CRD. Large enough to keep round-trips down, small enough that the
/// apiserver's per-schema estimated-cost budget is not the binding constraint.
const BATCH: usize = 40;

// ── harvest ─────────────────────────────────────────────────────────────────

/// Read a Rust string literal starting at `bytes[i]` (which must be `"`),
/// returning the unescaped contents and the index just past the closing quote.
/// Bytes are collected as bytes, so UTF-8 text survives; an escape this does
/// not know gives `None` rather than a mangled expression.
fn read_rust_string(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8(out).ok()?, i + 1)),
            b'\\' => {
                i += 1;
                match *bytes.get(i)? {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'0' => out.push(b'\0'),
                    c @ (b'\\' | b'"' | b'\'') => out.push(c),
                    b'u' => {
                        // `\u{e9}`
                        let rest = bytes.get(i + 1..)?;
                        let close = rest.iter().position(|&b| b == b'}')?;
                        let hex = std::str::from_utf8(rest.get(1..close)?).ok()?;
                        let c = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
                        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                        i += close + 1;
                    }
                    b'\n' => {
                        // A line continuation drops the newline and the next
                        // line's leading whitespace.
                        while bytes.get(i + 1).is_some_and(u8::is_ascii_whitespace) {
                            i += 1;
                        }
                    }
                    _ => return None,
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// Every distinct CEL expression literal passed to a test helper in `src`.
fn harvest() -> Vec<String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut files: Vec<String> = vec![format!("{root}/tests/k8s_conformance.rs")];
    if let Ok(dir) = std::fs::read_dir(format!("{root}/src/functions")) {
        let mut fns: Vec<String> = dir
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().into_owned())
            .filter(|p| p.ends_with(".rs"))
            .collect();
        fns.sort();
        files.extend(fns);
    }

    let mut found = BTreeSet::new();
    for path in files {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let bytes = src.as_bytes();
        for helper in HELPERS {
            let mut from = 0;
            while let Some(rel) = src[from..].find(helper) {
                let mut at = from + rel + helper.len();
                from = at;
                // A call may put its argument on the next line.
                while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                    at += 1;
                }
                // `eval(` also appears inside `.eval(`/`fn eval(`; only a string
                // literal right after the paren is a harvestable expression.
                if let Some((expr, _)) = read_rust_string(bytes, at)
                    && !expr.is_empty()
                {
                    found.insert(expr);
                }
            }
        }
    }
    found.into_iter().collect()
}

// ── value → CEL literal ─────────────────────────────────────────────────────

fn quote_string(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Render `v` as a CEL literal that evaluates back to it, or `None` when the
/// value has no literal form (opaque library types, non-finite doubles, …).
fn literal(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => format!("{u}u"),
        Value::Float(f) if f.is_finite() => {
            let s = format!("{f:?}");
            // `{:?}` prints 1e20 without a fraction; CEL needs no fixup there,
            // but an integral value like `1` must stay a double literal.
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::String(s) => quote_string(s),
        Value::Bytes(b) => {
            let mut out = String::from("b'");
            for byte in b.iter() {
                out.push_str(&format!("\\x{byte:02x}"));
            }
            out.push('\'');
            out
        }
        Value::Timestamp(ts) => format!("timestamp({})", quote_string(&ts.to_rfc3339())),
        Value::Duration(d) => {
            // `duration('1500ms')`-style; nanosecond precision keeps sub-second
            // values exact.
            let ns = d.num_nanoseconds()?;
            format!("duration('{ns}ns')")
        }
        Value::List(items) => {
            let parts: Option<Vec<String>> = items.iter().map(literal).collect();
            format!("[{}]", parts?.join(", "))
        }
        Value::Map(m) => {
            let mut parts = Vec::with_capacity(m.map.len());
            for (k, val) in m.map.iter() {
                let key = match k {
                    Key::String(s) => quote_string(s),
                    Key::Int(i) => i.to_string(),
                    Key::Uint(u) => format!("{u}u"),
                    Key::Bool(b) => b.to_string(),
                };
                parts.push(format!("{key}: {}", literal(val)?));
            }
            // Map literals compare by content, so emission order is irrelevant.
            format!("{{{}}}", parts.join(", "))
        }
        _ => return None,
    })
}

/// Evaluate with kube-cel, turning a panic into a reported outcome rather than
/// killing the run. A panic here is a finding in its own right: a validation
/// library fed an untrusted CRD must return an error, never unwind (and in
/// release builds the same overflow wraps silently, which is worse).
fn kube_cel_eval(expr: &str) -> Result<Value, String> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    // The validation engine's compile path (with the name check) on its
    // environment, the apiserver's base environment as far as kube-cel models
    // it: the probe measures what `Validator` evaluates.
    let caught = std::panic::catch_unwind(|| {
        let program = super::compile(expr, super::Scope::Crd).map_err(|e| format!("compile: {e}"))?;
        program
            .execute(&super::base_context())
            .map_err(|e| format!("eval: {e}"))
    });
    std::panic::set_hook(hook);
    match caught {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            Err(format!("PANIC: {msg}"))
        }
    }
}

// ── apiserver ───────────────────────────────────────────────────────────────

fn ctx() -> String {
    std::env::var("KUBE_CEL_PARITY_CTX")
        .expect("set KUBE_CEL_PARITY_CTX to a kubectl context (use `just probe`)")
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
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A probe: the harvested expression and the rule asserting kube-cel's value.
#[derive(Clone)]
struct Probe {
    idx: usize,
    expr: String,
    rule: String,
}

/// What the apiserver said about one probe.
enum Outcome {
    /// The rule held: the apiserver computes the same value kube-cel did.
    Agree,
    /// The rule was false or errored at runtime — with the apiserver's text.
    Disagree(String),
    /// The rule did not survive CRD registration (type error, unknown function,
    /// cost budget) — kube-cel evaluated something the apiserver would not.
    NotRegisterable(String),
    /// Run alone, the rule ran the apiserver out of cost budget, so the object
    /// was refused without a verdict on the rule (kube-cel has no cost model).
    CostHalted(String),
}

fn crd_for(plural: &str, rules: Vec<Json>) -> Json {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": { "name": format!("{plural}.x.test") },
        "spec": {
            "group": "x.test",
            "names": { "kind": plural, "plural": plural, "singular": plural },
            "scope": "Namespaced",
            "versions": [{
                "name": "v1", "served": true, "storage": true,
                "schema": { "openAPIV3Schema": {
                    "type": "object",
                    "properties": {},
                    "x-kubernetes-validations": rules
                }}
            }]
        }
    })
}

/// Register `batch` as one CRD and dry-run a CR through it. Returns one outcome
/// per probe. A batch the apiserver refuses to register is split and retried, so
/// one bad rule costs its neighbours nothing but a round-trip.
fn run_batch(batch: &[Probe], seq: &mut usize) -> Vec<(usize, Outcome)> {
    if batch.is_empty() {
        return Vec::new();
    }
    *seq += 1;
    let plural = format!("probe{seq}");
    let rules: Vec<Json> = batch
        .iter()
        .map(|p| json!({ "rule": p.rule, "message": format!("[[{}]]", p.idx) }))
        .collect();

    let (ok, out) = kubectl(&["apply", "-f", "-"], Some(&crd_for(&plural, rules).to_string()));
    if !ok {
        // Only the apiserver's 422 for the CRD says a rule cannot register; any
        // other failure (no such context, connection refused) is about the
        // cluster, and splitting on it would report every probe unregisterable.
        assert!(
            out.contains(&format!(
                "CustomResourceDefinition \"{plural}.x.test\" is invalid"
            )),
            "CRD apply for {plural} failed without the apiserver rejecting it: {out}"
        );
        if batch.len() == 1 {
            return vec![(batch[0].idx, Outcome::NotRegisterable(snippet(&out)))];
        }
        let (a, b) = batch.split_at(batch.len() / 2);
        let mut res = run_batch(a, seq);
        res.extend(run_batch(b, seq));
        return res;
    }
    let (ok, out) = kubectl(
        &[
            "wait",
            "--for=condition=Established",
            &format!("crd/{plural}.x.test"),
            "--timeout=60s",
        ],
        None,
    );
    assert!(ok, "CRD {plural}.x.test never became Established: {out}");

    let cr = json!({
        "apiVersion": "x.test/v1",
        "kind": plural,
        "metadata": { "name": "probe" }
    });
    let (accepted, out) = kubectl(&["apply", "--dry-run=server", "-f", "-"], Some(&cr.to_string()));
    let token = |p: &Probe| format!("[[{}]]", p.idx);
    let named = |p: &Probe| !accepted && out.contains(&token(p));
    // A rule over the per-call cost limit, or one that exhausts the budget,
    // stops the apiserver from evaluating the rest (validateExpressions returns
    // early, "no further validation rules will be run"). A rule it never
    // reached is neither agreeing nor disagreeing.
    let halted = !accepted && out.contains("no further validation rules will be run");
    // Otherwise a failing rule is reported under its message, evaluation errors
    // included (apiextensions `ruleErrorString`). A refusal naming no probe is
    // not a CEL verdict, and reading it as one would count the batch as agreeing.
    assert!(
        accepted || halted || batch.iter().any(named),
        "dry-run of {plural} failed without naming a probe: {out}"
    );

    let mut res: Vec<(usize, Outcome)> = batch
        .iter()
        .filter(|p| named(p))
        .map(|p| (p.idx, Outcome::Disagree(extract(&out, &token(p)))))
        .collect();
    let unnamed: Vec<Probe> = batch.iter().filter(|p| !named(p)).cloned().collect();
    if !halted {
        res.extend(unnamed.iter().map(|p| (p.idx, Outcome::Agree)));
    } else if unnamed.len() < batch.len() {
        // Rerun what the halt may have kept from running.
        res.extend(run_batch(&unnamed, seq));
    } else if let [only] = batch {
        res.push((only.idx, Outcome::CostHalted(snippet(&out))));
    } else {
        // The budget ran out without naming a rule: bisect to find it.
        let (a, b) = batch.split_at(batch.len() / 2);
        res.extend(run_batch(a, seq));
        res.extend(run_batch(b, seq));
    }
    res
}

fn snippet(s: &str) -> String {
    s.replace('\n', " ").chars().take(220).collect()
}

/// The apiserver's sentence mentioning `token`, for the report.
fn extract(out: &str, token: &str) -> String {
    out.split(", spec")
        .find(|part| part.contains(token))
        .map_or_else(|| snippet(out), snippet)
}

// ── runner ──────────────────────────────────────────────────────────────────

#[test]
#[ignore = "needs a live kind cluster; run via `just probe`"]
fn probe_expressions_against_kind() {
    // `KUBE_CEL_PROBE_CORPUS=<file>` probes an external newline-delimited corpus
    // (e.g. expressions lifted from cel-spec's conformance testdata) instead of
    // our own tests — that is how the *inherited* standard library gets measured,
    // since kube-cel's tests barely touch it.
    let exprs = match std::env::var("KUBE_CEL_PROBE_CORPUS") {
        Ok(path) => {
            let text = std::fs::read_to_string(&path).expect("corpus file readable");
            let list: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect();
            println!("corpus {path}: {} expressions", list.len());
            list
        }
        Err(_) => {
            let list = harvest();
            println!("harvested {} distinct expressions", list.len());
            list
        }
    };

    let mut probes = Vec::new();
    let mut kube_cel_errors = Vec::new();
    let mut unrenderable = Vec::new();
    for expr in &exprs {
        match kube_cel_eval(expr) {
            // An expression whose own value is the comparison would be circular
            // for booleans, but `<expr> == true` is exactly the claim we want the
            // apiserver to confirm, so no special-casing.
            Ok(v) => match literal(&v) {
                Some(lit) => {
                    let idx = probes.len();
                    probes.push(Probe {
                        idx,
                        rule: format!("({expr}) == ({lit})"),
                        expr: expr.clone(),
                    });
                }
                None => unrenderable.push(expr.clone()),
            },
            Err(e) => kube_cel_errors.push((expr.clone(), e)),
        }
    }
    println!(
        "probing {} (skipped: {} opaque/unrenderable, {} error in kube-cel)",
        probes.len(),
        unrenderable.len(),
        kube_cel_errors.len()
    );

    let mut seq = 0usize;
    let mut outcomes: Vec<(usize, Outcome)> = Vec::new();
    for (n, chunk) in probes.chunks(BATCH).enumerate() {
        println!("  batch {} ({} rules)…", n + 1, chunk.len());
        outcomes.extend(run_batch(chunk, &mut seq));
    }
    outcomes.sort_by_key(|(i, _)| *i);

    let mut agree = 0;
    let mut disagree = Vec::new();
    let mut unregisterable = Vec::new();
    let mut cost_halted = Vec::new();
    for (idx, outcome) in &outcomes {
        let p = &probes[*idx];
        match outcome {
            Outcome::Agree => agree += 1,
            Outcome::Disagree(msg) => disagree.push((p, msg.clone())),
            Outcome::NotRegisterable(msg) => unregisterable.push((p, msg.clone())),
            Outcome::CostHalted(msg) => cost_halted.push((p, msg.clone())),
        }
    }

    println!("\n===== EXPRESSION PROBE =====");
    println!("  agree              {agree}");
    println!("  DISAGREE           {}", disagree.len());
    println!("  not registerable   {}", unregisterable.len());
    println!("  cost-halted        {}", cost_halted.len());
    println!("  skipped (opaque)   {}", unrenderable.len());
    println!("  skipped (kc error) {}", kube_cel_errors.len());

    for (p, msg) in &disagree {
        println!("\nDISAGREE  {}", p.expr);
        println!("  kube-cel says: {}", p.rule);
        println!("  apiserver:     {msg}");
    }
    for (p, msg) in &unregisterable {
        println!("\nNOT-REGISTERABLE  {}", p.expr);
        println!("  {msg}");
    }
    for (p, msg) in &cost_halted {
        println!("\nCOST-HALTED  {}", p.expr);
        println!("  {msg}");
    }
    for (expr, e) in &kube_cel_errors {
        println!("kc-error  {expr}  ({e})");
    }
    for expr in &unrenderable {
        println!("opaque    {expr}");
    }
}

/// The harvest reaches every helper the conformance tests use, including the
/// IP/CIDR `string()` cases that need kube-cel's `Env`.
#[test]
fn harvest_includes_kube_env_string_cases() {
    let exprs = harvest();
    for expr in [
        r#"string(ip("192.168.0.1"))"#,
        r#"string(cidr("192.168.0.0/24"))"#,
    ] {
        assert!(exprs.iter().any(|e| e == expr), "{expr} was not harvested");
    }
}
