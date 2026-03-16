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
kube-cel = "0.5"
cel = "0.13"
```

## Usage

```rust
use cel::{Context, Program};
use kube_cel::register_all;

let mut ctx = Context::default();
register_all(&mut ctx);

// String functions
let result = Program::compile("'hello'.upperAscii()")
    .unwrap().execute(&ctx).unwrap();

// Quantity comparison
let result = Program::compile("quantity('1Gi').isGreaterThan(quantity('500Mi'))")
    .unwrap().execute(&ctx).unwrap();

// Semver
let result = Program::compile("semver('1.2.3').isLessThan(semver('2.0.0'))")
    .unwrap().execute(&ctx).unwrap();
```

## CRD Validation Pipeline

With the `validation` feature, you can compile and evaluate `x-kubernetes-validations` CEL rules client-side — no API server required.

```toml
[dependencies]
kube-cel = { version = "0.5", features = ["validation"] }
```

```rust
use kube_cel::validation::Validator;
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
let errors = validator.validate(&schema, &object, None);

assert_eq!(errors.len(), 2);
assert_eq!(errors[0].field_path, "spec");
assert_eq!(errors[1].field_path, "spec.replicas");
```

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

Invalid strings gracefully fall back to `Value::String`.

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
use kube_cel::defaults::apply_defaults;

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
let errors = Validator::new().validate_with_defaults(&schema, &object, None);
```

### Root-level variables

Bind CRD-level `apiVersion`, `apiGroup`, `kind` variables for root-level rules:

```rust
use kube_cel::validation::{Validator, RootContext};

let root_ctx = RootContext {
    api_version: "apps/v1".into(),
    api_group: "apps".into(),
    kind: "Deployment".into(),
};
let errors = Validator::new().validate_with_context(&schema, &object, None, Some(&root_ctx));
```

## ValidatingAdmissionPolicy (VAP)

Evaluate [ValidatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/) CEL expressions client-side — no API server required. Supports all VAP variables except `authorizer`.

```rust
use kube_cel::vap::{VapEvaluator, VapExpression, AdmissionRequest};

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
use kube_cel::analysis::{analyze_rule, ScopeContext};
use kube_cel::compilation::compile_schema;

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
`charAt`, `indexOf`, `lastIndexOf`, `lowerAscii`, `upperAscii`, `replace`, `split`, `substring`, `trim`, `join`, `reverse`, `strings.quote`

### Lists
`isSorted`, `sum`, `min`, `max`, `indexOf`, `lastIndexOf`, `slice`, `sort`, `flatten`, `reverse`, `distinct`, `first`, `last`, `lists.range`

### Sets
`sets.contains`, `sets.equivalent`, `sets.intersects`

### Regex
`find`, `findAll`

### URLs
`url`, `isURL`, `getScheme`, `getHost`, `getHostname`, `getPort`, `getEscapedPath`, `getQuery`

### IP / CIDR
`ip`, `isIP`, `isIPv4`, `isIPv6`, `ip.isCanonical`, `family`, `isLoopback`, `isUnspecified`, `isLinkLocalMulticast`, `isLinkLocalUnicast`, `isGlobalUnicast`, `<IP>.string()`, `cidr`, `isCIDR`, `isCIDRv4`, `isCIDRv6`, `containsIP`, `containsCIDR`, `prefixLength`, `masked`, `<CIDR>.ip()`, `<CIDR>.string()`

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
let result = Program::compile("!format.dns1123Label().validate('my-name').hasValue()")
    .unwrap().execute(&ctx).unwrap();
// Value::Bool(true)

// Dynamic format lookup
let result = Program::compile("!format.named('uuid').validate('550e8400-e29b-41d4-a716-446655440000').hasValue()")
    .unwrap().execute(&ctx).unwrap();
// Value::Bool(true)
```

### Math
`math.ceil`, `math.floor`, `math.round`, `math.trunc`, `math.abs`, `math.sign`, `math.sqrt`, `math.isInf`, `math.isNaN`, `math.isFinite`, `math.bitAnd`, `math.bitOr`, `math.bitXor`, `math.bitNot`, `math.bitShiftLeft`, `math.bitShiftRight`, `math.greatest`, `math.least`

### Encoders
`base64.decode`, `base64.encode`

### JSONPatch
`jsonpatch.escapeKey`

```rust
// RFC 6901: ~ → ~0, / → ~1
let result = Program::compile("jsonpatch.escapeKey('k8s.io/my~label')")
    .unwrap().execute(&ctx).unwrap();
// Value::String("k8s.io~1my~0label")
```

## Feature Flags

All features are enabled by default. Disable with `default-features = false` and pick what you need:

| Feature | Dependencies | Description |
|---------|-------------|-------------|
| `strings` | - | String extension functions |
| `lists` | - | List extension functions |
| `sets` | - | Set operations |
| `regex_funcs` | `regex` | Regex find/findAll |
| `urls` | `url` | URL parsing and accessors |
| `ip` | `ipnet` | IP/CIDR parsing and operations |
| `semver_funcs` | `semver` | Semantic versioning |
| `format` | - | String formatting |
| `quantity` | - | Kubernetes resource quantities |
| `jsonpatch` | - | JSONPatch key escaping (RFC 6901) |
| `named_format` | - | Named format validation (`format.dns1123Label()`, etc.) |
| `math` | - | Math functions (`math.ceil`, `math.abs`, bitwise, etc.) |
| `encoders` | `base64` | Base64 encode/decode |
| `validation` | `serde_json`, `serde`, `chrono` | CRD validation pipeline, VAP evaluation, static analysis, schema defaults |

## Known Limitations

| Feature | Reason |
|---------|--------|
| `cel.bind(var, init, expr)` | CEL compiler macro — requires `cel` crate support |
| `<list>.sortBy(var, expr)` | Lambda evaluation — requires `cel` crate support |
| TwoVarComprehensions (`all(i,v,...)`, `transformList`, etc.) | CEL compiler macro — K8s 1.33+ |
| Authz library | Requires API server connection — outside client library scope |

## Related

- [kube-rs](https://github.com/kube-rs/kube) - Rust Kubernetes client and controller runtime
- [cel](https://crates.io/crates/cel) - Rust CEL interpreter
- [Kubernetes CEL docs](https://kubernetes.io/docs/reference/using-api/cel/)

## License

Apache-2.0
