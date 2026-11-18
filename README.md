# kube-cel

[![Crates.io](https://img.shields.io/crates/v/kube-cel.svg)](https://crates.io/crates/kube-cel)
[![Docs.rs](https://docs.rs/kube-cel/badge.svg)](https://docs.rs/kube-cel)
[![CI](https://github.com/kube-rs/kube-cel/actions/workflows/ci.yml/badge.svg)](https://github.com/kube-rs/kube-cel/actions)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Kubernetes [CEL](https://kubernetes.io/docs/reference/using-api/cel/) extension functions for Rust, built on top of [`cel`](https://crates.io/crates/cel).

Implements the Kubernetes-specific CEL libraries defined in [`k8s.io/apiserver/pkg/cel/library`](https://pkg.go.dev/k8s.io/apiserver/pkg/cel/library) and [`cel-go/ext`](https://pkg.go.dev/github.com/google/cel-go/ext), enabling client-side evaluation of CRD validation rules.

## Installation

```toml
[dependencies]
kube-cel = "0.8"
```

`kube-cel` re-exports the `cel` crate it was built against as `kube_cel::cel`. Import `cel` types through that re-export rather than declaring a separate `cel` dependency, otherwise a version mismatch surfaces as a cryptic `Context` type error.

## Usage

```rust
use std::sync::Arc;

// kube-cel's Env (the apiserver's base CEL environment, as far as kube-cel
// models it) and a context on it with every compiled-in function registered.
let env = Arc::new(kube_cel::new_env());
let ctx = kube_cel::new_context(Arc::clone(&env)).unwrap();

// Compile with env.compile(..), which expands the Env's macros.
let result = env.compile("'hello'.upperAscii()")
    .unwrap().execute(&ctx).unwrap();

// Quantity comparison
let result = env.compile("quantity('1Gi').isGreaterThan(quantity('500Mi'))")
    .unwrap().execute(&ctx).unwrap();

// cel's lists extension and its sortBy macro
let result = env.compile("[3, 1, 2].sortBy(x, -x)")
    .unwrap().execute(&ctx).unwrap();

// IP to string (an overload on kube-cel's Env)
let result = env.compile("string(ip('192.168.0.1'))")
    .unwrap().execute(&ctx).unwrap();
```

`new_env()` is the environment the validation pipeline below uses too. Add
`cel`'s extensions or overloads of your own to it before wrapping it in an
`Arc`; `new_context` refuses an environment that does not start from
`new_env()` (`RegisterError::IncompleteEnv`). The `KubeCelExt` trait
(`Context::default().with_all()`) is deprecated: it registers only the
functions that live on a context, not the lists extension, the macros or
`string(<IP>)`, and will be removed once kube-cel's functions move to `Env`
overloads ([#17](https://github.com/kube-rs/kube-cel/issues/17)).

## CRD Validation Pipeline

With the `validation` feature, you can compile and evaluate `x-kubernetes-validations` CEL rules client-side — no API server required.

```toml
[dependencies]
kube-cel = { version = "0.8", features = ["validation"] }
```

```rust
use kube_cel::Validator;
use serde_json::json;

let schema = json!({
    "type": "object",
    "properties": {
        "spec": {
            "type": "object",
            "properties": {
                "replicas": {
                    "type": "integer",
                    "x-kubernetes-validations": [
                        {"rule": "self >= 0", "message": "replicas must be non-negative"}
                    ]
                }
            },
            "x-kubernetes-validations": [
                {"rule": "self.replicas >= 1", "message": "at least one replica"}
            ]
        }
    }
});

let object = json!({"spec": {"replicas": -1}});

let validator = Validator::new();
let errors = validator.validate(&schema, &object, None).unwrap_err();

assert_eq!(errors.len(), 2);
assert_eq!(errors[0].field_path, "spec");
assert_eq!(errors[1].field_path, "spec.replicas");
```

Validation entry points return `Result<(), ValidationErrors>`: `Ok(())` when every rule passes (so a caller can `?`-propagate failure), and `Err(ValidationErrors)` otherwise. `ValidationErrors` derefs to `[ValidationError]` and is iterable, so every individual failure stays inspectable; `into_vec()` recovers the owned `Vec`.

The validator walks the schema tree, compiles rules at each node, and evaluates them with `self` bound to the corresponding object value. Transition rules (referencing `oldSelf`) are supported by passing `old_object`.

### Schema-aware `format` support

Fields with `format: "date-time"` or `format: "duration"` in the schema are automatically converted to CEL `Timestamp` / `Duration` values, matching K8s API server behavior:

```rust
let schema = json!({
    "type": "object",
    "properties": {
        "expiresAt": { "type": "string", "format": "date-time" },
        "timeout":   { "type": "string", "format": "duration" }
    },
    "x-kubernetes-validations": [
        {"rule": "self.expiresAt > timestamp('2024-01-01T00:00:00Z')", "message": "expired"},
        {"rule": "self.timeout <= duration('1h')", "message": "too long"}
    ]
});
```

A `format: date` or `format: duration` string the apiserver cannot parse is an
`EvaluationError` at its field, as the apiserver refuses it (durations follow
its `strfmt.ParseDuration`, so `1d` is a day); other invalid strings fall back
to `Value::String`.

### Field name escaping

JSON field names that are CEL reserved words or contain special characters are automatically escaped when converting to CEL map keys, matching K8s API server behavior:

| JSON field name | CEL access |
|----------------|------------|
| `namespace` | `self.__namespace__` |
| `foo-bar` | `self.foo__dash__bar` |
| `a.b` | `self.a__dot__b` |
| `x/y` | `self.x__slash__y` |
| `my_field` | `self.my__field` |

### Schema defaults

Apply schema `default` values before validation, matching K8s API server behavior:

```rust
use kube_cel::apply_defaults;

let schema = json!({
    "type": "object",
    "properties": {
        "replicas": {"type": "integer", "default": 1},
        "strategy": {"type": "string", "default": "RollingUpdate"}
    }
});

let object = json!({"replicas": 3});
let defaulted = apply_defaults(&schema, &object);
// defaulted = {"replicas": 3, "strategy": "RollingUpdate"}
```

Or use the convenience method:

```rust
let result = Validator::new().validate_with_defaults(&schema, &object, None);
```

### Root-level variables

Bind CRD-level `apiVersion`, `apiGroup`, `kind` variables for root-level rules:

```rust
use kube_cel::{Validator, RootContext};

let root_ctx = RootContext {
    api_version: "apps/v1".into(),
    api_group: "apps".into(),
    kind: "Deployment".into(),
};
let result = Validator::new().validate_with_context(&schema, &object, None, Some(&root_ctx));
```

## ValidatingAdmissionPolicy (VAP)

Evaluate [ValidatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/) CEL expressions client-side — no API server required. Supports all VAP variables except `authorizer`.

```rust
use kube_cel::{VapEvaluator, VapExpression, AdmissionRequest};

let evaluator = VapEvaluator::builder()
    .object(json!({"spec": {"replicas": 3}}))
    .request(AdmissionRequest {
        operation: "CREATE".into(),
        namespace: "production".into(),
        ..Default::default()
    })
    .params(json!({"maxReplicas": 5}))
    .build();

let results = evaluator.evaluate(&[VapExpression {
    expression: "object.spec.replicas <= params.maxReplicas".into(),
    message: Some("too many replicas".into()),
    message_expression: None,
}]);

assert!(results[0].passed);
```

For repeated evaluation, pre-compile expressions:

```rust
let compiled = evaluator.compile_expressions(&expressions);
let results = evaluator.evaluate_compiled(&compiled); // no re-parsing
```

## Static Analysis

Catch CEL rule issues before deployment — variable scope violations and cost budget warnings:

```rust
use kube_cel::{analyze_rule, ScopeContext};
use kube_cel::compile_schema;

let compiled = compile_schema(&schema);

let warnings = analyze_rule(
    "self.items.all(item, item.size() > 0)",
    &compiled,
    ScopeContext::CrdValidation,
);
// Warns: "list field has no maxItems bound" (may exceed K8s 1M cost budget)
```

`ScopeContext::CrdValidation` catches admission-only variables (`request`, `object`, etc.) used in CRD rules. `ScopeContext::AdmissionPolicy` allows the full VAP variable set.

## Supported Functions

### Strings
`charAt`, `indexOf`, `lastIndexOf`, `lowerAscii`, `upperAscii`, `replace`, `split`, `substring`, `trim`, `join`, `strings.quote`

### Lists
`isSorted`, `sum`, `min`, `max`, `indexOf`, `lastIndexOf`, `first`, `last`, and
`cel`'s upstream `lists` extension (cel-go's `ext.Lists`) on the `Env`:
`slice`, `sort`, `sortBy`, `flatten`, `reverse`, `distinct`, `lists.range`
(`sortBy` is a macro, so compile with `env.compile(..)`)

### Comprehensions
The two-variable macros (cel-go's `ext.TwoVarComprehensions`), on the `Env`
whatever the features: `all(i, v, ..)`, `exists(i, v, ..)`, `existsOne(i, v, ..)`,
`transformList`, `transformMap`, `transformMapEntry`

### Sets
`sets.contains`, `sets.equivalent`, `sets.intersects`

### Regex
`find`, `findAll`

### URLs
`url`, `isURL`, `getScheme`, `getHost`, `getHostname`, `getPort`, `getEscapedPath`, `getQuery`

### IP / CIDR
`ip`, `isIP`, `ip.isCanonical`, `family`, `isLoopback`, `isUnspecified`, `isLinkLocalMulticast`, `isLinkLocalUnicast`, `isGlobalUnicast`, `string(<IP>)`, `cidr`, `isCIDR`, `containsIP`, `containsCIDR`, `prefixLength`, `masked`, `<CIDR>.ip()`, `string(<CIDR>)` (the `string` overloads need `new_env()`; the member form `<IP>.string()` is an error, as in the apiserver)

### Semver
`semver`, `isSemver`, `major`, `minor`, `patch`, `isGreaterThan`, `isLessThan`, `compareTo`

### Quantity
`quantity`, `isQuantity`, `isInteger`, `asInteger`, `asApproximateFloat`, `sign`, `add`, `sub`, `isGreaterThan`, `isLessThan`, `compareTo`

### Format
`<string>.format(<list>)` with verbs: `%s`, `%d`, `%f`, `%e`, `%b`, `%o`, `%x`, `%X`

### Named Format Validation
`format.dns1123Label`, `format.dns1123Subdomain`, `format.dns1035Label`, `format.dns1035LabelPrefix`, `format.dns1123LabelPrefix`, `format.dns1123SubdomainPrefix`, `format.qualifiedName`, `format.labelValue`, `format.uri`, `format.uuid`, `format.byte`, `format.date`, `format.datetime`, `format.named`, `validate`

```rust
// Returns optional: none = valid, of([...errors]) = invalid
// K8s pattern: !format.<name>().validate(value).hasValue()
let result = env.compile("!format.dns1123Label().validate('my-name').hasValue()")
    .unwrap().execute(&ctx).unwrap();
// Value::Bool(true)

// Dynamic format lookup — format.named() returns optional(Format); unwrap with .value()
let result = env.compile("!format.named('uuid').value().validate('550e8400-e29b-41d4-a716-446655440000').hasValue()")
    .unwrap().execute(&ctx).unwrap();
// Value::Bool(true)
```

### Not included
kube-cel ships only the functions the Kubernetes apiserver declares. The
`math.*` and `base64.*` functions of cel-go's `ext.Math` and `ext.Encoders`
are `cel`'s `math` and `encoders` extensions: add them to an `Env` of your own
(they never reach the validation pipeline, as the apiserver has neither).

```rust
let mut env = kube_cel::new_env();
env.add_extension(kube_cel::cel::extensions::math)?;
env.add_extension(kube_cel::cel::extensions::encoders)?;
let env = Arc::new(env);
let ctx = kube_cel::new_context(Arc::clone(&env))?;
// math.greatest / math.least are macros: compile with env.compile(..).
let result = env.compile("math.greatest(1, 3, 2)")?.execute(&ctx)?;
```

## Feature Flags

Cargo features are the **only** granularity axis — there is no runtime per-library registration. All extension-function features are enabled by default. To narrow the set you must disable the defaults first:

```toml
# Correct: only string + list helpers.
kube-cel = { version = "0.8", default-features = false, features = ["strings", "lists"] }

# No-op narrowing: without `default-features = false` the list is ADDED to the
# already-complete default set, so you still get everything.
kube-cel = { version = "0.8", features = ["strings", "lists"] }

# Restore the whole surface (all 10 function groups + the validation engine)
# in one flag, e.g. after narrowing for a downstream build profile.
kube-cel = { version = "0.8", default-features = false, features = ["full"] }
```


| Feature | Dependencies | Description |
|---------|-------------|-------------|
| `strings` | - | String extension functions |
| `lists` | - | List extension functions, and `cel`'s upstream `lists` extension on the `Env` |
| `sets` | - | Set operations |
| `regex_funcs` | `regex` | Regex find/findAll |
| `urls` | `url` | URL parsing and accessors |
| `ip` | `ipnet` | IP/CIDR parsing and operations |
| `semver_funcs` | `semver` | Semantic versioning |
| `format` | - | String formatting |
| `quantity` | - | Kubernetes resource quantities |
| `named_format` | - | Named format validation (`format.dns1123Label()`, etc.) |
| `validation` | `serde_json`, `serde`, `chrono`, `base64` | CRD validation pipeline, VAP evaluation, static analysis, schema defaults |
| `full` | *(all of the above)* | Umbrella: every extension-function group **plus** `validation`. Not in `default` (which is functions-only) |

## Versioning and stability

kube-cel is pre-1.0 and **cannot reach 1.0 until the `cel` crate does.** Its
public signatures expose `cel::Context` and `cel::Value`, and a crate cannot be
stable while its public dependencies are not ([Rust API Guidelines,
C-STABLE](https://rust-lang.github.io/api-guidelines/necessities.html#c-stable)).
Once `cel` reaches 1.0, kube-cel 1.x will track `cel` 1.y; a `cel` major bump
forces a kube-cel major bump. While below 1.0, minor releases may contain
breaking changes (the Cargo convention for `0.x`).

Two stability tiers, by surface:

- **Tier 1 — entry points (committed).** The `new_env`/`new_context`
  constructors and their `RegisterError`, the `pub use cel` re-export, and the
  set of Kubernetes CEL functions they provide. This is the crate's core
  identity and changes only deliberately. The deprecated `KubeCelExt` trait is
  not part of it: it stays until kube-cel's functions move to `Env` overloads
  ([#17](https://github.com/kube-rs/kube-cel/issues/17)).
- **Tier 2 — validation engine (evolving, `validation` feature).** `Validator`,
  the VAP evaluator, static analysis, schema defaults, and the
  `compilation`/`validation`/`vap`/`analysis` types. Still maturing; expect its
  surface to change across pre-1.0 minors as the validation pipeline hardens.

## apiserver divergence

The validation pipeline aims to return the same verdict the Kubernetes API
server would, client-side. Where it cannot today, it diverges **fail-closed** —
it reports an error rather than silently accepting — so a passing result is as
strict as the API server, with two exception classes (the last two rows below),
which stay until `cel` ships the missing piece. `cel` has no type checker yet
([cel-rust#350](https://github.com/cel-rust/cel-rust/issues/350)), so a rule the
apiserver rejects at registration for a type, arity or call-shape error can
still pass client-side. And `cel` has no cost model yet (on its roadmap,
[cel-rust#230](https://github.com/cel-rust/cel-rust/discussions/230)), so a rule
the apiserver rejects for its estimated cost at registration, or an object it
rejects for a rule's runtime cost, can still pass client-side. These
divergences are pinned by
`tests/apiserver_divergence.rs`.

| Feature | API server | kube-cel | Direction |
|---------|-----------|----------|-----------|
| `cel.bind(var, init, expr)` | rejects the CRD at registration (no `ext.Bindings` in the base environment) | `CompilationFailure` | fail-closed |
| `math.*` (cel-go `ext.Math`) | rejects the CRD at registration (not in the base environment) | `CompilationFailure` | fail-closed |
| `isIPv4`, `isIPv6`, `isCIDRv4`, `isCIDRv6` (kube-cel functions before 0.9) | rejects the CRD at registration (not in the base environment) | `CompilationFailure` | fail-closed |
| `base64.*` (cel-go `ext.Encoders`), `jsonpatch.escapeKey` | rejects the CRD at registration (not in the base environment; `jsonpatch` is a MutatingAdmissionPolicy library) | `CompilationFailure` | fail-closed |
| Global `min(..)` / `max(..)` | rejects the CRD at registration (only `<list>.min()` / `<list>.max()`) | `CompilationFailure` | fail-closed |
| String `reverse()` (`'abc'.reverse()`) | rejects the CRD at registration (no matching overload) | `EvaluationError` | fail-closed |
| `lists.range(-1)`; `sort()` on a list holding a NaN (`[double('NaN')].sort()`) | evaluates: `lists.range(-1)` is `[]`, and the list sorts | `EvaluationError` | fail-closed |
| `<IP>.string()`, `<CIDR>.string()` | rejects the CRD at registration (no matching overload; only `string(<IP>)`) | `UnsupportedReference` | fail-closed |
| A rule reading `oldSelf` in the items of a list that is not `x-kubernetes-list-type: map`, or `optionalOldSelf` on a rule that does not read `oldSelf` | rejects the CRD at registration | `CompilationFailure` | fail-closed |
| Quantity `add` / `sub` past `i128` (`quantity('2e38').add(quantity('1'))`) | exact (`inf.Dec`), so `….isGreaterThan(quantity('2e38'))` holds | `EvaluationError` (quantity overflow) | fail-closed |
| An object reaching more than 64 levels of schema nesting | no nesting limit; evaluates rules at any depth | `SchemaTooDeep` error | fail-closed |
| Rule whose `messageExpression` does not compile (a syntax error, or a call refused above) | rejects the CRD at registration | `CompilationFailure` (the rule is rejected, not just its message) | fail-closed |
| Functions the apiserver declares that kube-cel does not implement: `optional.unwrap(..)`, `<list>.unwrapOpt()`, the authz library (`authorizer.*` in a VAP), or a function whose kube-cel feature is disabled | evaluates (the authz library against the cluster) | compiles, and passes where evaluation does not reach the call; `UnsupportedReference` where it does | fail-closed (the authz library is out of scope) |
| No type checker: type errors (`[1, 2] == [1.0, 2.0]`, `[1, 2.0].isSorted()`), arity errors (`'a'.lowerAscii(42)`), call-shape errors (`family(ip(..))`, and the `reverse()` / `.string()` rows above), a non-string `messageExpression`, and an undeclared identifier (such as `authorizer` in a CRD rule), where evaluation does not reach the error | rejects the CRD at registration (type-checks every expression) | passes; `cel` has no type checker yet ([cel-rust#350](https://github.com/cel-rust/cel-rust/issues/350)) | **fail-open** (exception class) |
| No cost model: a rule whose estimated cost exceeds the budget (`self.l.all(x, self.l.exists(y, x == y))` on a list with no `maxItems`), or whose evaluation exceeds the per-expression cost limit (`lists.range(1000000).size() > 0`, `lists.range(600000).map(x, x).size() > 0`) | rejects the CRD at registration ("estimated rule cost exceeds budget"), or the object at evaluation ("actual cost limit exceeded") | passes; `cel` has no cost model yet ([cel-rust#230](https://github.com/cel-rust/cel-rust/discussions/230)) | **fail-open** (exception class) |

The validation pipeline compiles and runs rules in the environment `new_env()`
builds, modelled on the apiserver's base CEL environment (`k8s.io/apiserver`
`environment.MustBaseEnvSet(..).NewExpressionsEnv()`): it enables the
two-variable comprehensions (`all(i, v, …)`, `transformList`, `transformMap`, …)
and, with the `lists` feature, `cel`'s `lists` extension (`sortBy`, `slice`,
`flatten`, `distinct`, `reverse`, `sort`, `lists.range`), which evaluate as in
the apiserver but for the `lists.range(-1)` / NaN row above, and it leaves out
what the base environment lacks: `cel.bind` and the `math` and `base64.*`
libraries. (`cel` has them as its `bindings`, `math` and `encoders` extensions,
for an `Env` of your own.) `cel` only parses,
so kube-cel walks the AST of every rule and `messageExpression` at compile time
and refuses a call to a function the apiserver does not declare as
`ErrorKind::CompilationFailure`, even in a branch evaluation never reaches
(`self.s == '' || isIPv4(self.s)`), as the apiserver does at registration. The
declared names
(`src/validation/apiserver_functions.txt`, from the `k8s.io/apiserver` v0.36.1
base environment) are checked against a live apiserver by `just parity`. CRD
rules, their `messageExpression`s and VAP expressions share them, since the
apiserver declares the same functions for all three. A function the apiserver declares but
kube-cel does not implement (`optional.unwrap`, `<list>.unwrapOpt()`, the authz
library) therefore compiles, and fails only when evaluation reaches it. Every
rule in the schema is compiled before the object is walked, including those on
nodes the object does not reach (an omitted property, the items of an empty
list), and if any fails to compile the object is rejected with those errors
alone. A
reference evaluation cannot resolve (such a function, an undeclared identifier,
or a function called in a form none of its overloads takes) surfaces as
`ErrorKind::UnsupportedReference`, distinct from
`EvaluationError` (reserved for genuine runtime errors in a supported rule), so a
consumer can tell a coverage gap apart from an actual rule failure. Either way the
object is rejected. The authz library requires a live API server and is out of
scope for a client library.

## Related

- [kube-rs](https://github.com/kube-rs/kube) - Rust Kubernetes client and controller runtime
- [cel](https://crates.io/crates/cel) - Rust CEL interpreter
- [Kubernetes CEL docs](https://kubernetes.io/docs/reference/using-api/cel/)

## License

Apache-2.0
