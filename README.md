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

### Schema-aware lists and objects

As in the apiserver, an `x-kubernetes-list-type: set` list compares as a set and `+` on it is a union, and an `x-kubernetes-list-type: map` list compares by its `x-kubernetes-list-map-keys` and `+` merges by key, at any depth (below `additionalProperties` too). A nullable property that is `null` is absent to `has()`, `.?` and `in`, while `size()` and iteration still count it; so is a key the schema does not declare, which iterates as `null`. An object with neither `properties` nor `additionalProperties` hides all its keys, and `metadata` at the resource root or in an embedded resource declares `name` and `generateName` only.

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

### Root rules

A rule on the root schema reads the object's own `apiVersion`, `kind` and
`metadata.name` through `self`, as on the apiserver, which declares no
`apiVersion`, `apiGroup` or `kind` variable (a rule naming one does not
compile). `RootContext` and the `*_with_context` methods, which bound them
before 0.9, are deprecated and ignore it.

```rust
let schema = json!({
    "type": "object",
    "x-kubernetes-validations": [
        {"rule": "self.apiVersion.startsWith('apps/') && self.kind == 'Deployment'"}
    ]
});
let object = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"}});
let result = Validator::new().validate(&schema, &object, None);
```

## ValidatingAdmissionPolicy (VAP)

Evaluate [ValidatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/) CEL expressions client-side — no API server required. Binds every VAP variable but `authorizer` (it needs an API server) and `variables` (`spec.variables` is not modelled, so an expression naming it does not compile); `namespaceObject` is `null` when not set, and `params` is declared for a policy with a `paramKind` (`has_param_kind`, or a parameter object set) and `null` without an object.

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

For repeated evaluation, pre-compile a policy's expressions once, stating
whether it has a `paramKind`, then evaluate them per request:

```rust
let compiled = VapEvaluator::builder()
    .has_param_kind(true)
    .build()
    .compile_expressions(&expressions);
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
cel-go's `ext.Lists` on the `Env`: `slice`, `sort`, `sortBy`, `flatten`,
`reverse`, `distinct`, `lists.range` (`sortBy` is a macro, so compile with
`env.compile(..)`). `sort()` and `sortBy()` order equal keys as the apiserver
does, through a port of Go's `sort.Slice`.

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
| `lists` | - | List extension functions, and cel-go's `ext.Lists` on the `Env` |
| `sets` | - | Set operations |
| `regex_funcs` | `regex` | Regex find/findAll |
| `urls` | - | URL parsing and accessors (a port of Go's `net/url`) |
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
server would, client-side, for the CEL rules; OpenAPI value validation is out
of scope (a value of the wrong type, a `null` list item, a missing `required`
field, an `enum`, `pattern` or length bound pass client-side unless a rule
catches them). Where it cannot today, it diverges **fail-closed** —
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
| An undeclared identifier: a typo (`slef`), a macro's variable outside the macro (or bound as `.x` and read as `x`), a policy variable in a CRD rule (`authorizer`, `object`), `apiVersion` / `apiGroup` / `kind` in a CRD rule (kube-cel's `RootContext` bound them before 0.9), `self` in a policy, `authorizer` in a policy's `messageExpression`, `params` in a policy without a `paramKind`, `variables` in a policy without `spec.variables`, a type name the apiserver does not declare (`dyn`, `duration`); and `__result__` as a standard macro's variable ("iteration variable overwrites accumulator variable") | rejects the CRD or policy at registration ("undeclared reference") | `CompilationFailure` | fail-closed |
| Global `min(..)` / `max(..)` | rejects the CRD at registration (only `<list>.min()` / `<list>.max()`) | `CompilationFailure` | fail-closed |
| `variables` in a VAP with `spec.variables` (`variables.foo == 1`) | evaluates the policy's variables | `VapError`: kube-cel does not model `spec.variables`, so `variables` is never declared | fail-closed |
| Names `cel` cannot evaluate: type names with no `cel` value (`net.IP`, `net.CIDR`, `google.protobuf.Empty`), `google.protobuf.NullValue.NULL_VALUE`, a message construction (`google.protobuf.Duration{seconds: 1}`) | evaluates (`type(ip('1.2.3.4')) == net.IP` holds) | compiles; `UnsupportedReference` where evaluation reaches it (`EvaluationError` for the construction) | fail-closed |
| `.self` / `.oldSelf` in a CRD rule (`.self.kind == self.kind`) | evaluates: the leading dot names the rule's `self` | compiles; `UnsupportedReference` where evaluation reaches it: kube-cel binds `self` per schema node in an inner scope, and `cel` looks a leading-dot name up at the root only (a policy's `.object` evaluates) | fail-closed |
| `==` / `!=` between a list or object of the schema and a value of another kind, written with `dyn` (`self.o != dyn([1])`, `self.s != dyn({'a': 1})`), or between an object holding fields its schema does not declare (`x-kubernetes-preserve-unknown-fields`) and a map not of its kind | `==` fails (no such overload); `!=` holds, as cel-go's `!=` is `Equal(..) != true` | `EvaluationError` for both: `cel`'s equality answers only true or false | fail-closed |
| A two-variable comprehension over an object holding fields its schema does not declare (`dyn(self.o).all(k, v, true)`) | evaluates: an undeclared key iterates as `null`, its value as an error the body may ignore | `EvaluationError`: `cel` looks the value up as it binds it | fail-closed |
| A set list `+` whose outcome could hang on state the apiserver keeps: a second `+` on one value adding a key the first added, or `==` on it reading one (`[self.s].all(x, (x + [3]).size() == 3 && x == [1, 3])`, or across the rules of a set node, which share its `self`: `(self + [3]).size() == 3` then `self == [1, 3]`), or two `+`s on one set's elements appending different values at one position (`(self.s + [3]).size() == 3 && (self.s + [4]).size() == 3`); a timestamp, which kube-cel does not key, wherever a cached one may match | evaluates: its set `+` adds to the receiver's cached set and appends into its slice, so the outcome hangs on how Go's slice grew | `EvaluationError`: kube-cel computes each `+` afresh and refuses where that state could change the outcome | fail-closed |
| A set read from a list or map that a variable or literal holds whole, `+`ed twice (`[self.l].all(x, (x[0] + [3]).size() == 3 && (x[0] + [3]).size() == 3)`, likewise `[self.d].all(x, .. x['k'] ..)`) | evaluates: each `Get` / `Find` binds the set afresh, so no `+` state carries between the reads | `EvaluationError`: `cel` copies the set when it copies its container, as when it holds the set itself (`[self.l[0]]`, where the state does carry), so kube-cel keeps one wrapper | fail-closed |
| A value kube-cel holds as a plain map or list, such as an `additionalProperties` map: an item `+` adds to a set or map list (`self.l + dyn(self.r)`), or one a map list's `==` / `!=` keys (`self.m == dyn(self.r)`), or one a value under a typeless `x-kubernetes-preserve-unknown-fields` node compares with (`dyn(self).u != dyn({'a': 1})`, `dyn(self).u != dyn(self.a)` with `a` a plain list field) | evaluates: the item binds under the left list's items, keys and compares by its decoded map | `EvaluationError`: kube-cel cannot tell such a map's or list's Go value from a CEL-built one's | fail-closed |
| Iterating, or handing to a function, a set or map list that `+` gave an element its items refuse (`(self.s + dyn(['a'])).all(x, true)` in an integer set) | evaluates: the element is an error value, which the body may not read | `EvaluationError`: `cel` iterates values, not errors (reading the element by index is an error, as on the apiserver) | fail-closed |
| A list or map field of an element `+` added that holds a value its schema refuses (`(self.l + dyn(self.r))[0].x.size() == 2` with `l`'s `x` a list of integers and `r`'s `x` given `['a', 1]`) | evaluates: the field binds as a list or map, and only reading the refused value fails | `EvaluationError` where the field is read (its size, presence or another key): kube-cel binds the field's values at once | fail-closed |
| An operation on a value under a typeless `x-kubernetes-preserve-unknown-fields` node other than `==`, `has()`, `.?` and indexing, under an absorbing `\|\| true` (`type(dyn(self).u) != string \|\| true`, `dyn(self).u.size() == 1 \|\| true`) | evaluates: the operation is an error, which `\|\| true` absorbs | `EvaluationError`: `cel` reads the value's type there (`type()`, overload lookup, its error), which kube-cel refuses as it cannot fail `type()` | fail-closed |
| `has()` or `.?` on a scalar (`!has(dyn(self.s).a)` for a string `s`, `!dyn(self.s).?a.hasValue()`, and in an `optionalOldSelf` rule a typeless preserve-unknown value, which `oldSelf.value()` holds as plain JSON) | `false` / `optional.none()`: cel-go's presence test on a value without fields, as the apiserver configures it | `EvaluationError`: `cel` refuses a presence test on a value without fields, as cel-spec has it (cel-go's `errorOnBadPresenceTest`) | fail-closed |
| A map-list `==` or `+` with four `x-kubernetes-list-map-keys` or more and a number key value | evaluates: the key is Go's `%v` of the key tuple | `EvaluationError`: kube-cel prints a key tuple as Go does except for floats | fail-closed |
| `.x` naming a macro's variable `x` (`[1].all(x, .x > 0)`, `[dyn(1)].all(namespaceObject, .namespaceObject == null)`) | reads the macro's `x` (the second is false) | `CompilationFailure`: `cel` looks a leading-dot name up outside the macros (where a policy's `namespaceObject` is null), so it cannot evaluate it alike | fail-closed |
| String `reverse()` (`'abc'.reverse()`) | rejects the CRD at registration (no matching overload) | `EvaluationError` | fail-closed |
| `<IP>.string()`, `<CIDR>.string()` | rejects the CRD at registration (no matching overload; only `string(<IP>)`) | `UnsupportedReference` | fail-closed |
| `x-kubernetes-validations` under `allOf`, `anyOf`, `oneOf` or `not`, at any depth below one (before 0.9: evaluated under the first three, ignored under `not`) | rejects the CRD at registration ("must be empty to be structural") | `CompilationFailure` at the root, whatever the object | fail-closed |
| A rule field the apiserver refuses beside the expressions: a blank or multi-line static `message` (CRD rule or VAP validation), a multi-line rule without a `message`, an unsupported `reason`, a blank, multi-line or invalid `fieldPath` (`a`, `.nope`) (before 0.9: accepted) | rejects the CRD or policy at registration | `InvalidRule` (a VAP: `VapError`) | fail-closed |
| A rule reading `oldSelf` in the items of a list that is not `x-kubernetes-list-type: map`, or `optionalOldSelf` on a rule that does not read `oldSelf` | rejects the CRD at registration | `CompilationFailure` | fail-closed |
| Quantity `!=` with an `add` / `sub` result on the right (`quantity('2') != quantity('1').add(1)`) | `true` (`==` there is an error: the right side of `Equal` must not be the pointer `add` returns) | `EvaluationError`, as for `==` (`cel`'s `==` and `!=` share one comparison that cannot fail) | fail-closed |
| Quantity `add` / `sub` past `i128` (`quantity('2e38').add(quantity('1'))`) | exact (`inf.Dec`), so `….isGreaterThan(quantity('2e38'))` holds | `EvaluationError` (quantity overflow), even where CEL absorbs the error (`… \|\| true`) | fail-closed |
| A quantity past kube-cel's supported range: an exponent or scale outside ±1000, before or after trailing zeros are moved into it (`1e1001`, `10e1000`, `1e4294967296`, the sum `5e1000 + 5e1000`), or a mantissa past the `i128` range | parses it: narrows the exponent to int32 (`quantity('1e4294967296') == quantity('1')`), and near the int32 bounds computes a power of ten of hundreds of MiB or panics | `EvaluationError` for a rule or VAP expression that reaches it, even where CEL absorbs the error (`… \|\| true`); `quantity()` / `isQuantity()` error | fail-closed |
| The standard `timestamp(string)` / `duration(string)` conversions (from `cel`): an offset of hour 24 or minute 60 (`+24:00`, `+23:60`, `-24:00`, `+00:60`), a `,` before the fraction (`10:30:00,5Z`), a one-digit hour (`T1:30:00Z`), the units `µs` / `μs` (`duration('1µs')`) | parses them (Go's `time.Parse` and `time.ParseDuration`) | `EvaluationError` | fail-closed |
| A URL component that is not UTF-8 once unescaped (`url('http://h%80x/').getHost()`) | returns the bytes as a string | `EvaluationError` (a CEL string here must be UTF-8) | fail-closed |
| An object reaching more than 64 levels of schema nesting | no nesting limit; evaluates rules at any depth | `SchemaTooDeep` error | fail-closed |
| Rule whose `messageExpression` does not compile (a syntax error, or a call refused above) | rejects the CRD at registration | `CompilationFailure` (the rule is rejected, not just its message) | fail-closed |
| Functions the apiserver declares that kube-cel does not implement: `optional.unwrap(..)`, `<list>.unwrapOpt()`, the authz library (`authorizer.*` in a VAP), or a function whose kube-cel feature is disabled | evaluates (the authz library against the cluster) | compiles, and passes where evaluation does not reach the call; `UnsupportedReference` where it does | fail-closed (the authz library is out of scope) |
| A list or map `cel` builds (a literal, a macro's result, a plain list field) on the left of `==` / `!=` against a set list, map list or object, or holding one that `in` searches (`['b', 'a'] != self.s`, `self.s.filter(x, true) != self.s`, `dyn({'y': 1}) != dyn(self.o)`, `['a', 'b'] in [self.s]`, `'x' in [dyn(self).u]` for a value under a typeless `x-kubernetes-preserve-unknown-fields` node); and `+` onto a plain list field (`type((self.l + dyn([1]))[0]) == int` for a number list) | compares by position or key (cel-go `baseList.Equal`, `baseMap.Equal`); `in` asks the searched value (`baseList.Contains`); `+` binds what it adds under the left list's items (a double there) | `cel`'s built-in list or map is unequal to one of another type, and its `in` asks the list's element: `!=` holds, `==` fails. kube-cel binds a plain list as `cel`'s own list, so `+` keeps the added value's own binding; it binds plain lists as schema-aware ones once `cel` carries the fix, which would otherwise break `[..] == self.l`. **Fixed by cel-rust#462; remove this row before 0.9.0** | fail-open |
| No type checker: type errors (`[1, 2] == [1.0, 2.0]`, `[1, 2.0].isSorted()`), arity errors (`'a'.lowerAscii(42)`), call-shape errors (`family(ip(..))`, a literal `format()` call with more arguments than clauses or an argument its clause cannot take, such as `'%s'.format([1, 2])`, the one-variable `[1].existsOne(x, x > 0)`, a macro only `cel` has (the apiserver has `existsOne(i, v, p)` alone, so it passes even when evaluated), and the `reverse()` / `.string()` rows above), a field the type does not have (`self.undeclared` on a schema without it), and a non-string `messageExpression`, where evaluation does not reach the error | rejects the CRD at registration (type-checks every expression) | passes; `cel` has no type checker yet ([cel-rust#350](https://github.com/cel-rust/cel-rust/issues/350)) | **fail-open** (exception class) |
| No cost model: a rule whose estimated cost exceeds the budget (`self.l.all(x, self.l.exists(y, x == y))` on a list with no `maxItems`), or whose evaluation exceeds the per-expression cost limit (`lists.range(1000000).size() > 0`, `lists.range(600000).map(x, x).size() > 0`) | rejects the CRD at registration ("estimated rule cost exceeds budget"), or the object at evaluation ("actual cost limit exceeded") | passes; `cel` has no cost model yet ([cel-rust#230](https://github.com/cel-rust/cel-rust/discussions/230)) | **fail-open** (exception class) |

The validation pipeline compiles and runs rules in the environment `new_env()`
builds, modelled on the apiserver's base CEL environment (`k8s.io/apiserver`
`environment.MustBaseEnvSet(..).NewExpressionsEnv()`): it enables the
two-variable comprehensions (`all(i, v, …)`, `transformList`, `transformMap`, …)
and, with the `lists` feature, cel-go's `ext.Lists` (`sortBy`, `slice`,
`flatten`, `distinct`, `reverse`, `sort`, `lists.range`), which evaluate as in
the apiserver, equal sort keys included, and it leaves out
what the base environment lacks: `cel.bind` and the `math` and `base64.*`
libraries. (`cel` has them as its `bindings`, `math` and `encoders` extensions,
for an `Env` of your own.) `cel` only parses,
so kube-cel walks the AST of every rule and `messageExpression` at compile time
and refuses a call to a function the apiserver does not declare, and an
identifier not declared where it appears, as `ErrorKind::CompilationFailure`,
even in a branch evaluation never reaches (`self.s == '' || isIPv4(self.s)`,
`self.s == '' || slef.t == ''`), as the apiserver does at registration. A CRD
rule and its `messageExpression` read `self` and `oldSelf`; a VAP expression
reads `object`, `oldObject`, `request`, `namespaceObject` (`null` when not
set), `authorizer` (in the expression, not its `messageExpression`), and
`params` when the policy has a `paramKind` (`VapEvaluatorBuilder::has_param_kind`;
unset, whether a parameter object is set), `null` when no parameter object is
set; `variables` comes with `spec.variables`, which kube-cel does not model, so
it does not compile; a macro's variables are declared inside the macro, and
type names (`int`, `net.IP`, `google.protobuf.Timestamp`) everywhere.
The declared function names
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
reference evaluation cannot resolve (such a function, a declared variable
kube-cel does not bind, such as `authorizer`, or a function called in a form
none of its overloads takes) surfaces as
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
