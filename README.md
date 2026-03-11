# kube-cel

[![Crates.io](https://img.shields.io/crates/v/kube-cel.svg)](https://crates.io/crates/kube-cel)
[![Docs.rs](https://docs.rs/kube-cel/badge.svg)](https://docs.rs/kube-cel)
[![CI](https://github.com/kube-rs/kube-cel/actions/workflows/ci.yml/badge.svg)](https://github.com/kube-rs/kube-cel/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Kubernetes CEL extension functions for the [`cel`](https://crates.io/crates/cel) crate.

Provides the Kubernetes-specific [CEL extension libraries](https://kubernetes.io/docs/reference/using-api/cel/#kubernetes-cel-libraries) so you can evaluate CRD validation rules client-side, without an API server.

## Features

- **Extension functions** — strings, lists, sets, regex, URLs, IP/CIDR, semver, quantity, math, base64, named formats, jsonpatch escaping
- **CRD validation pipeline** — compile and evaluate `x-kubernetes-validations` rules against JSON objects
- **Schema-aware type coercion** — `format: date-time` and `format: duration` fields are automatically converted to CEL timestamp/duration values
- **Pre-compilation** — compile schemas once, validate many objects

## Quick start

```rust
use cel::Context;
use kube_cel::register_all;

let mut ctx = Context::default();
register_all(&mut ctx);
// ctx is now ready to evaluate K8s CEL expressions
```

### CRD validation (feature = `validation`)

```toml
kube-cel = { version = "1", features = ["validation"] }
```

```rust,ignore
use kube_cel::validation::Validator;
use serde_json::json;

let schema = json!({
    "type": "object",
    "x-kubernetes-validations": [
        {"rule": "self.replicas >= 0", "message": "must be non-negative"}
    ],
    "properties": { "replicas": {"type": "integer"} }
});

let object = json!({"replicas": -1});
let errors = Validator::new().validate(&schema, &object, None);
assert_eq!(errors.len(), 1);
```

For repeated validation, pre-compile with `compile_schema` and use `validate_compiled`:

```rust,ignore
use kube_cel::compilation::compile_schema;
use kube_cel::validation::Validator;
use serde_json::json;

let schema = json!({ /* ... */ });
let compiled = compile_schema(&schema);
let validator = Validator::new();

for object in objects {
    let errors = validator.validate_compiled(&compiled, &object, None);
}
```

## Feature flags

All extension function features are enabled by default. The `validation` feature is opt-in.

| Feature | Description | Dependencies |
|---------|-------------|-------------|
| `strings` | `charAt`, `indexOf`, `join`, `split`, `trim`, `replace`, `reverse`, … | — |
| `lists` | `flatten`, `sort`, `first`, `last`, `range`, … | — |
| `sets` | `sets.contains`, `sets.intersects`, `sets.equivalent` | — |
| `regex_funcs` | `matches`, `find`, `findAll` | `regex` |
| `urls` | `url`, `getScheme`, `getHost`, `getHostname`, … | `url` |
| `ip` | `ip`, `isIP`, `cidr`, `isCIDR`, `isIPv4`, `isIPv6`, `isCanonical`, … | `ipnet` |
| `semver_funcs` | `semver.isValid`, `semver.compare`, `semver.major`, … | `semver` |
| `format` | `format.named`, `format.dns1123Label`, `format.uri`, `format.uuid`, … | — |
| `quantity` | `quantity`, `isQuantity`, `isGreaterThan`, `isLessThan`, `compareTo` | — |
| `math` | `math.abs`, `math.ceil`, `math.floor`, `math.greatest`, bitwise ops, … | — |
| `encoders` | `base64.encode`, `base64.decode` | `base64` |
| `named_format` | Named format validators (`dns1123Label`, `uri`, `uuid`, …) | — |
| `jsonpatch` | `jsonpatch.escapeKey` | — |
| `validation` | CRD validation pipeline (`Validator`, `compile_schema`) | `serde_json`, `serde`, `chrono` |

## Known limitations

| Feature | Reason |
|---------|--------|
| `cel.bind(var, init, expr)` | CEL compiler macro — requires `cel` crate support |
| `<list>.sortBy(var, expr)` | Lambda evaluation — requires `cel` crate support |
| TwoVarComprehensions | CEL compiler macro — K8s 1.33+ |
| Authz library | Requires API server connection — outside client library scope |

## License

Apache-2.0
