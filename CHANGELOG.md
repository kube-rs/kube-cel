# Changelog

## [0.9.0] - Unreleased

Migrates to `cel` 0.16 ([#14](https://github.com/kube-rs/kube-cel/issues/14))
and brings the validation engine's verdicts in line with kube-apiserver
v1.36.1 (cel-go v0.26), failing closed where it cannot match them (README
"apiserver divergence"): each verdict change below was checked against a live
apiserver (`just parity`). "Breaking" lists what code and rules may need to
change, with migrations. Most entries under "Fixed" close a
**fail-open** divergence (kube-cel passed what the apiserver rejects) and need
no migration for rules the apiserver accepts.

### Breaking

#### Rust API

- **`cel` 0.13 → 0.16** (0.8.0 depended on 0.13). `cel` is a public
  dependency (`pub use cel`), so this is breaking on its own. `cel::Context`
  gained a second lifetime (`Context<'p, 'v>`); `KubeCelExt` is implemented
  for `Context<'_, '_>`.
- **One environment: build it with `new_env()` and a context on it with
  `new_context()`.** General use now gets the environment the validation
  engine uses. `new_env()` adds kube-cel's port of cel-go's `ext.Lists` (with
  the `lists` feature, and its `sortBy` macro), the two-variable
  comprehension macros (`all(i, v, …)`, `exists(i, v, …)`,
  `existsOne(i, v, …)`, `transformList`, `transformMap`, `transformMapEntry`)
  and the `string(<IP>)` / `string(<CIDR>)` overloads, and `new_context()`
  registers the same functions as validation. What moved to the `Env` is
  missing from a context built otherwise:
  - `string(<IP>)` / `string(<CIDR>)` need kube-cel's `Env`. `cel` 0.16
    refuses a context function whose name the `Env` already declares, and the
    standard library declares `string`, so kube-cel can no longer override
    `string` on a context (in 0.8 it reimplemented the standard conversions to
    do so). On a plain `Context::default().with_all()`, `string(ip(...))`
    fails with "no such overload".
  - `sortBy` and the two-variable comprehensions are macros: compile with
    `env.compile(..)` on the environment from `new_env()`.
    `Program::compile` knows only the standard macros, so such a program
    fails on its iteration variable (`UndeclaredReference("x")`).
  - `register_all` / `with_all` panic instead of silently registering
    nothing. On a child scope (`Context::new_inner_scope`) `cel` reports
    success and registers nothing; on a context whose `Env` already declares
    one of kube-cel's names, `cel` 0.16 refuses the name. Both used to pass
    silently. Neither happens on `Context::default()` or on `new_env()`.
  - Migration: build the env and context, and compile with
    `env.compile(..)`:
    ```rust
    let env = Arc::new(kube_cel::new_env());
    let ctx = kube_cel::new_context(Arc::clone(&env))?;
    ```
    `KubeCelExt` is deprecated (see Deprecated).
- **kube-cel ships only functions the apiserver declares.** The `math.*`
  functions (18) and `base64.encode` / `base64.decode`, kube-cel's copies of
  cel-go's `ext.Math` and `ext.Encoders`, and `isIPv4`, `isIPv6`, `isCIDRv4`,
  `isCIDRv6` and `jsonpatch.escapeKey` are removed from `new_env()`,
  `new_context()` and `KubeCelExt`, together with the `math`, `encoders` and
  `jsonpatch` Cargo features (a manifest that still enables one fails to
  resolve). The validation engine refuses all of them as undeclared (see
  Fixed), so verdicts do not change.
  - Migration: add `cel`'s own extensions to the `Env` before sharing it.
    `encoders` needs `cel`'s `ext_encoders` feature, on by default.
    ```rust
    let mut env = kube_cel::new_env();
    env.add_extension(kube_cel::cel::extensions::math)?;
    env.add_extension(kube_cel::cel::extensions::encoders)?;
    let env = Arc::new(env);
    let ctx = kube_cel::new_context(Arc::clone(&env))?;
    ```
    `cel`'s `math` has every removed `math.*` function and evaluates
    kube-cel's former test expressions alike. `math.greatest` and
    `math.least` are macros there, as in cel-go, so they need
    `env.compile(..)`: `Program::compile` leaves them undeclared
    (`UndeclaredReference("math")`).
  - The rest have no `cel` counterpart; these expressions give the same
    results: `isIPv4(s)` → `isIP(s) && ip(s).family() == 4` (`6` for
    `isIPv6`), `isCIDRv4(s)` → `isCIDR(s) && cidr(s).ip().family() == 4` (`6`
    for `isCIDRv6`), `jsonpatch.escapeKey(k)` →
    `k.replace('~', '~0').replace('/', '~1')`.
- **Function results change outside validation too.** kube-cel's own
  `slice`, `flatten`, `distinct`, `reverse`, `sort` and `lists.range` are
  removed in favour of that port on the `Env`, and so is its string
  `reverse()`; `sum`, `isSorted`, the list `min` / `max`, `indexOf`,
  `lastIndexOf`, `first` and `last` stay. The results below now match the
  apiserver's, in validation and in general use alike:
  - `'abc'.reverse()` is an error: the apiserver's environment has no string
    `reverse()` and rejects it at registration (in validation it is an
    `EvaluationError` where evaluation reaches it, fail-closed).
  - `sort()` refuses a list mixing numeric types (`[3, 1u, 2.0].sort()`, "list
    elements must have the same type"), a `null`, or elements it cannot
    compare even alone (`[{}].sort()`, `[[1]].sort()`), all of which
    kube-cel's own sorted; it sorts `bytes` and a list holding a NaN now,
    which were errors (see Fixed).
  - The list functions are member functions only: the global forms
    `sort([2, 1])`, `reverse([1, 2])`, `distinct([1, 1])` and
    `slice([1, 2], 0, 1)` are errors.
  - An extra argument is refused (`[1].sort(99)`, `[1, 2].slice(0, 1, 99)`,
    `[[1]].flatten(1, 2)`, `[1].distinct(99)`, `[1].reverse(99)`,
    `lists.range(2, 99)`), where kube-cel's own ignored it.
  - `lists.range(n)` makes at most 1,000,000 elements (kube-cel's own had no
    limit).
  - Global `min(..)` / `max(..)` (`min(5, 3)`) are errors: kube-cel's list
    `min` / `max` fell back to a variadic form that neither the apiserver
    (only `<list>.min()` / `<list>.max()`) nor the `cel` standard library has.
    The list members stay.
  - The member form `<IP>.string()` / `<CIDR>.string()` (and `42.string()`,
    `'a'.string()`, …) is an error, matching the apiserver, which rejects it
    at compile time. Use the global form `string(x)`.
  - `format.named(name)` returns `optional(Format)`, as in the apiserver:
    `optional.none()` for an unknown name. Unwrap it with `.value()`.
  - Quantity `sign()` is a global function only, as in the apiserver
    (`quantity(..).sign()` is an error): write `sign(q)`.
  - Migration: write a global `min(a, b)` as `[a, b].min()`, a global list
    call as a member call (`[2, 1].sort()`), and `x.string()` as
    `string(x)`. For a string `reverse()` outside the apiserver's environment,
    declare the overload on the `Env` yourself:
    ```rust
    use kube_cel::cel::{self, common::types::CelString};
    fn reverse(this: &CelString<'_>) -> CelString<'static> {
        CelString::from(this.inner().chars().rev().collect::<String>())
    }
    let mut env = kube_cel::new_env();
    cel::add_member_overload!(env, fn reverse: (CelString) -> CelString)?;
    ```
    `cel`'s `strings` extension has one too, but it also declares `charAt`,
    `indexOf`, `lowerAscii`, `format`, … under kube-cel's names, so
    `new_context()` refuses an environment with it (`RegisterError::Rejected`)
    unless kube-cel's `strings`, `lists` and `format` features are all off.
- **Error kinds follow where the apiserver refuses.** A rule, VAP expression
  or `messageExpression` that calls a function the apiserver does not declare
  is a `CompilationFailure` (see Fixed). `ErrorKind::UnsupportedReference`
  now covers a reference evaluation cannot resolve: a variable kube-cel does
  not bind, a function the apiserver declares but kube-cel does not implement
  or has behind a disabled feature, or a function called in a form none of
  its overloads takes, such as `ip(..).string()`. Its message no longer
  blames an unsupported CEL macro or function; it names the reference.
  `cel.bind`, which used to fail closed as `UnsupportedReference`, is now a
  `CompilationFailure`.

#### Rules to update

- **Field-name escaping escapes only `__` (to `__underscores__`), `.`, `-`
  and `/`**; a single `_` stays as it is. A rule on a field `my_field` is
  `self.my_field`, as in the apiserver, no longer `self.my__field` (which
  0.8's README table showed).
  - Migration: write `self.my_field`.
- **Under `optionalOldSelf: true`, `oldSelf` is a CEL optional**:
  `optional.none()` on create, `optional.of(old)` on update. As cel-go
  resolves an optional variable through its native value, `oldSelf.value()`
  holds a list or map of the old object as plain JSON (a set a plain list, an
  object's `null` and undeclared fields present, numbers as JSON has them),
  and the rule's `messageExpression` sees `oldSelf` itself, not the optional.
  - Migration: write `!oldSelf.hasValue() || … oldSelf.value() …` instead of
    `oldSelf == null || …`.
- **`apiVersion`, `apiGroup` and `kind` are no longer variables of root
  rules.** kube-cel bound them from a `RootContext`; the apiserver declares
  no such variables and rejects a CRD whose rule names one ("undeclared
  reference to 'kind'", checked on v1.36.1). Such a rule now fails to
  compile (`ErrorKind::CompilationFailure`) on every entry point, with or
  without a `RootContext`.
  - Migration: read the object's own fields in a root rule:
    `self.apiVersion`, `self.kind`, `self.metadata.name` (the API group is
    the part of `self.apiVersion` before the `/`). `RootContext` and the
    `*_with_context` methods are deprecated (see Deprecated).
- **CEL rules under `allOf`, `anyOf`, `oneOf` or `not` fail to compile.**
  The apiserver refuses `x-kubernetes-validations` anywhere under a schema
  combinator, at any depth (in its `properties`, `items`, nested
  combinators), as not structural ("must be empty to be structural",
  checked on v1.36.1). kube-cel evaluated the rules under `allOf` / `anyOf`
  / `oneOf` as if they were on the node and ignored those under `not`, so an
  object satisfying them passed: a **fail-open** divergence, now closed.
  Every entry point rejects the object with one root `CompilationFailure`
  per node holding rules (`CompilationError::RuleInCombinator`, also listed
  by `CompiledSchema::compilation_errors()`); an empty rule list is fine.
  - Migration: move the rules onto the node the combinator sits on.
- **Rule fields the apiserver refuses at registration are refused.**
  Beside the expressions, the apiserver checks a rule's other fields when it
  registers the CRD or policy (checked on v1.36.1): a static `message` that
  is blank but not empty, or holds a line break once trimmed (CRD rules and
  VAP validations); a CRD rule holding a line break without a `message`; a
  `reason` other than `FieldValueRequired`, `FieldValueForbidden`,
  `FieldValueInvalid` or `FieldValueDuplicate`; a `fieldPath` that is blank,
  holds a line break or names no field of the rule's node (it starts with
  `.` or `[`, steps through `.name` or `['name']`, and cannot index a list).
  kube-cel accepted all of them. A CRD rule with one is now rejected on
  every object (`ErrorKind::InvalidRule`, `CompilationError::InvalidRuleField`),
  and a VAP expression with a refused message fails to compile (`VapError`).
  - Migration: fix the field. A `fieldPath` names a field relative to the
    rule's node and starts with `.`: `spec.x` on the root becomes `.spec.x`.
- **A VAP's declarations come from the policy, its values from the
  request**, as in the apiserver. `VapEvaluator` declares `params` only for
  a policy with a `paramKind`: the new
  `VapEvaluatorBuilder::has_param_kind(bool)` says whether it has one;
  unset, it follows `params(..)` (a parameter object set means a
  `paramKind`). `has_param_kind(false)` with `params(..)`,
  in either order, is a policy without one: `params` is not declared and the
  object is not bound. Each `CompiledVapExpression` keeps whether `params`
  was declared, and `evaluate_compiled` binds `params` to `null` when it was
  and the evaluator has no parameter object, as for a binding with no
  `paramRef`. An expression naming `params` on an evaluator with neither
  used to compile and fail only when evaluation reached `params`; it now
  fails to compile. So does one naming `variables`: it comes with
  `spec.variables`, which kube-cel does not model, and the apiserver rejects
  it without them.
  - Migration: none for expressions the apiserver accepts in the policy they
    come from, but for those naming `variables`. To compile a policy's
    expressions once, ahead of the requests, pass its
    `spec.paramKind.is_some()`; per request, set the parameter object, or
    nothing for a binding with no `paramRef`:
    ```rust
    let compiled = VapEvaluator::builder()
        .has_param_kind(true)
        .build()
        .compile_expressions(&expressions);
    let results = VapEvaluator::builder()
        .object(object)
        .params(param_object) // or omit it: `params` is null
        .build()
        .evaluate_compiled(&compiled);
    ```
    A policy with `spec.variables` cannot be evaluated client-side until
    kube-cel models them (README "apiserver divergence").

### Added

- `new_env() -> cel::Env`: kube-cel's environment, the apiserver's base CEL
  environment as far as kube-cel models it: the `cel` standard library,
  cel-go's `ext.Lists` (feature `lists`), the two-variable comprehension
  macros and `string(<IP>)` / `string(<CIDR>)` (feature `ip`). The validation
  engine uses it too. It does not install `cel`'s `lists` extension, whose
  functions it declares itself, so adding that extension to it fails.
- `new_context(env) -> Result<cel::Context, RegisterError>`: a root context on
  `env` with every compiled-in function registered, the path that replaces
  `KubeCelExt`. `env` must start from `new_env()`, with `cel`'s extensions or
  overloads of your own added as you need.
- `RegisterError`: `IncompleteEnv` for an `env` that does not start from
  `new_env()` (such as `Env::stdlib()`), `Rejected { function, source }` for an
  `env` that declares one of kube-cel's names (such as with `cel`'s `strings`
  extension).
- `VapEvaluatorBuilder::has_param_kind(bool)`: whether the policy has a
  `paramKind`, which declares `params` (see Breaking).
- `CompilationError::RuleInCombinator` and `CompilationError::InvalidRuleField`
  (see Breaking).
- `SchemaFormat::Date` (`format: date` binds as a timestamp) and
  `SchemaFormat::Number` (`type: number` binds as a `double`); see Fixed.
- `CompiledSchema::list_map_keys` (an `x-kubernetes-list-type: map` node's
  keys, which pair new and old items for transition rules) and
  `CompiledSchema::nullable`.

### Deprecated

- `RootContext`, `Validator::validate_with_context`,
  `Validator::validate_compiled_with_context` and
  `Validator::validate_with_defaults_and_context`. No rule can read what
  `RootContext` bound (see Breaking), so the methods ignore it and validate
  as `validate` / `validate_compiled`. Use those, and `self.apiVersion` /
  `self.kind` in a root rule.
- `KubeCelExt::register_all` / `with_all`. They keep working, but register
  only the functions that live on a context: on `Context::default()` the
  `lists` extension, the macros and `string(<IP>)` are missing. Use
  `new_env()` and `new_context()`. They will be removed once kube-cel's
  functions move to `Env` overloads
  ([#17](https://github.com/kube-rs/kube-cel/issues/17)).

### Changed

- `regex` is a dependency whatever the features (it was one of
  `regex_funcs`): `matches` compiles with it. `chrono` is a dependency
  whatever the features (it was one of `validation`): the standard time
  conversions build `cel`'s time values. `cel` depends on both already. The
  `urls` feature no longer depends on the `url` crate.
- Dependency lower bounds are raised to the oldest versions that build and
  pass the tests under `-Z direct-minimal-versions`: `chrono` 0.4.39 (`cel`
  0.16 needs it), `regex` 1.10.5, `serde` 1.0.103, `serde_json` 1.0.25.
- The validation engine (`Validator`, `compile_schema`, `VapEvaluator`, static
  analysis) compiles with `Env::compile` on a shared `Env` from `new_env()`,
  modelled on the apiserver's base environment (`k8s.io/apiserver`
  `environment.MustBaseEnvSet(..)`), instead of `Program::compile`, so it
  sees the environment's macros and overloads.
- Static analysis (`check_rule_scope`, `analyze_rule`) follows the apiserver's
  declarations: `ScopeContext::CrdValidation` takes `self` and `oldSelf`
  only (no `apiVersion`, `apiGroup`, `kind`), and
  `ScopeContext::AdmissionPolicy` no longer takes `self` and `oldSelf`. It
  resolves names as the compile-time check does, so a type name (`int`,
  `net.IP`), a function namespace (`optional.of`, `format.dns1123Label()`) or
  a macro's variable inside the macro is no longer reported as a variable
  outside its scope, while a macro's variable outside the macro is.
- `sortBy` and the two-variable comprehensions (`all(i, v, …)`,
  `exists(i, v, …)`, `transformList`, `transformMap`, …) evaluate in the
  validation engine instead of failing closed with `UnsupportedReference`.
  The list functions (`sortBy`, `slice`, `flatten`, `distinct`, `reverse`,
  `sort`, `lists.range`) are now a port of cel-go's `ext.Lists` declared on
  the `Env` (see Breaking for how their results changed) and need the
  `lists` feature: without it, a rule reaching a list function is rejected
  (`UnsupportedReference`), and one using `sortBy` (a macro) does not
  compile.

### Fixed

Each group below follows kube-apiserver v1.36.1, checked live.

- **Rules the apiserver refuses at registration fail to compile, reached or
  not** (`ErrorKind::CompilationFailure`, `CompilationError::Parse` /
  `MessageExpressionParse`, `VapError`). The apiserver rejects all of these
  when it registers the CRD or policy; they used to pass client-side unless
  evaluation reached them. More such checks sit with the areas they belong
  to below (regular expressions, time conversions, constant conversions,
  quantities).
  - A call to a function the apiserver does not declare (`cel.bind`, the
    `math` library, `base64.*`, `jsonpatch.escapeKey`, kube-cel's former
    `isIPv4` / `isIPv6` / `isCIDRv4` / `isCIDRv6`), a global `min(..)` /
    `max(..)` (the apiserver only has `<list>.min()` / `<list>.max()`). The
    whole AST is checked, so a call evaluation never
    reaches (`self.s == '' || isIPv4(self.s)`, the body of a comprehension
    over an empty list, the `messageExpression` of a rule that holds) is
    refused too. All but `cel.bind` used to pass client-side. The names
    accepted are the functions the `k8s.io/apiserver` v0.36.1 base
    environment declares (`src/validation/apiserver_functions.txt`, checked
    against a live apiserver by `just parity`). One that kube-cel does not
    implement (`optional.unwrap`, `<list>.unwrapOpt()`, the authorizer library
    in a VAP) compiles as before, and fails closed only when evaluation
    reaches it.
  - An identifier not declared where it appears. A CRD rule and its
    `messageExpression` read `self` and `oldSelf`; a VAP expression reads
    `object`, `oldObject`, `request`, `namespaceObject`, `authorizer` (not in
    its `messageExpression`), and `params` when the policy has a `paramKind`
    (see Breaking); a macro's variables (`all`, `exists`, `map`, `filter`,
    `sortBy`, `optMap`, the two-variable forms, …) are declared inside the
    macro only; type names (`int`, `string`, `net.IP`,
    `google.protobuf.Timestamp`, …) everywhere. A typo
    (`self.x > 0 || slef.y == 1`), a macro's variable outside the macro (or
    bound as `.x` and read as `x`), `__result__` as the variable of `all`,
    `exists`, `map`, `filter` or `sortBy` (cel-go's legacy accumulator name),
    `authorizer` or `object` in a CRD rule, `self` in a VAP expression,
    `authorizer` in a VAP `messageExpression`, `params` in a VAP expression
    of a policy without a `paramKind`, `variables` in any VAP expression,
    `dyn` or `duration` used as a value, or the construction of an
    undeclared message type (`Foo{a: 1}`) is refused; the README "no type
    checker" row no longer covers them. Field names are not checked
    (`self.undeclared` on a schema without it still needs a type checker).
    `.x` naming a macro's variable `x` is refused too, though the apiserver
    reads the macro's `x`: `cel` looks a leading-dot name up outside the
    macros, so it cannot evaluate it alike (README "apiserver divergence",
    fail-closed). The declared type names and message types were measured
    one by one, as values and as constructions, so a name the apiserver
    accepts (`google.protobuf.Struct.FieldsEntry`, `duration{}`) compiles.
  - Every rule in the schema is compiled before the object is walked, as the
    apiserver compiles them all when it registers the CRD. A rule or
    `messageExpression` that does not compile on a node the object does not
    reach (an omitted property, the items of an empty list, a node past the
    64-level depth cap) used to pass unchecked. The object is then rejected
    with those compilation errors alone; their `field_path` names the schema
    node (`spec.items[*]`, a map value as `*`). A valid rule past the depth
    cap still passes under a shallow object: the apiserver has no nesting
    limit (v1.36.1 registers a 400-level CRD), so the README no longer
    claims it rejects deep schemas.
  - A rule reading `oldSelf` in the items of a list that is not
    `x-kubernetes-list-type: map` (at any depth below it), and
    `optionalOldSelf` set on a rule that does not read `oldSelf`, are
    `CompilationFailure`s for every object. Both used to pass (the first
    skipped its rule). As in the apiserver, a rule uses `oldSelf` when any
    identifier in it is named `oldSelf`, a comprehension variable included
    (`[1].all(oldSelf, oldSelf > 0)`), so such a rule is also skipped on
    create. An empty `messageExpression` (`""`) counts as unset.
- **Values bind as the apiserver binds them, by their schema.**
  - Item-level transition rules pair each new list item with the old item of
    the same `x-kubernetes-list-map-keys` in an `x-kubernetes-list-type: map`
    list, instead of the old item at the same index. With several keys, float
    key values compare to six decimals, as the apiserver's `%f` key does.
  - A JSON-null old value counts as no old value: a transition rule skips,
    and under `optionalOldSelf` the rule sees `optional.none()`.
  - `type: number` binds as `double` even when the JSON number is integral
    (`self.x / 2.0` with `x: 1` is `0.5`), whatever its `format`; `format:
    date` binds as a timestamp, and a `format: date` string that is not a
    valid date, Go's `2006-01-02` at fixed width with ASCII digits only
    (`+024-01-01` and `2026- 1-01` are not), is an `EvaluationError` at its
    field. `format: date-time` binds as the apiserver's
    `strfmt.ParseDateTime`, Go's RFC 3339 parse: a `,` fraction binds (it was
    left a string, so `getFullYear()` failed), and lowercase `t` / `z`, a
    space for the `T` and a leap second do not (chrono's parse took them, a
    **fail-open**). A value the apiserver's OpenAPI check `strfmt.IsDateTime`
    refuses (`not-a-date`, no zone, a one-digit hour) is an `EvaluationError`
    at its field, as the apiserver refuses the object; it used to bind as a
    string that a rule could compare. One the check takes and the parse
    refuses, or with an offset of 24 hours or more, is refused too, where the
    apiserver fails only a rule reading it (fail-closed, a README row). A
    schema `format: duration` parses as the apiserver's
    `strfmt.ParseDuration`: Go's `time.ParseDuration` exactly (`µs` / `μs`, a
    `+` sign, every overflow refused), then strfmt's word units (`1d`,
    `2 hours`); a duration it cannot parse is an `EvaluationError` at its
    field.
  - Map-list number keys compare as the apiserver decodes JSON: an integer
    past int64 is a float (2^63 and 2^63 + 1 are one key), and a single float
    key `-0.0` equals `0.0`.
  - An `x-kubernetes-list-type: set` list compares as a set (`["b", "a"] ==
    ['a', 'b']`; the apiserver's test is equal size and every right-hand
    element held, so `["a", "b"] == ['a', 'a']` too) and `+` on it is a union
    keeping its order. An `x-kubernetes-list-type: map` list compares by its
    keys, and `+` merges by key: the right-hand item replaces the left one in
    place, new keys are appended. Both used to compare by position and
    concatenate, so `self.s != oldSelf.s` held for a reordered set
    (fail-open). Set `==` and `+` look up each right-hand item's value among
    the elements as the apiserver holds them: a number set decoded from
    `[1, 2]` is unequal to itself (`int64` keys, double items), a duration
    set compares strings against durations, and a set of bytes or objects
    fails (a Go panic). `+` binds what it adds under the left list's items:
    `(self.s + dyn([1]))[0]` is a double in a number set, and an added
    map-list item hides its null fields. Where the apiserver's set `+`
    state could change the outcome, the rule is refused (fail-closed). A map
    list keys its items as the apiserver does, by the key fields read from
    each decoded item under their escaped names: a key field whose name
    escaping changes (`a-b`, `namespace`) is nil in every item, and four
    keys or more meet where Go's `%v` of the tuples does. An object finds a
    field from its CEL name through the apiserver's exact unescaping, and
    its equality passes each JSON name to that lookup, as the apiserver's
    does, so a field whose name does not unescape to itself (`__foo__`,
    `a__dot__b`) is skipped there.
  - A nullable property that is `null` is absent: `has(self.o.x)` is false,
    `self.o.?x` is `optional.none()` and `'x' in self.o` is false, while
    `size(self.o)` and iteration still count it, and formatting the object
    fails, as in the apiserver. A key the schema does not declare
    (`x-kubernetes-preserve-unknown-fields`) is absent as well, counted, and
    iterated as `null`. It used to read as `null`, so `has(self.o.x)`
    held (fail-open).
  - Every value binds by its own schema node, below `additionalProperties`
    too (a set there compares as a set). An object with neither
    `properties` nor `additionalProperties` (free-form under
    `x-kubernetes-preserve-unknown-fields`, or `additionalProperties: true`)
    hides its keys, as in the apiserver: `dyn(self.f).a` is no such key and
    `has()` is false, while `size()` counts them. The `metadata` of the
    resource root and of an embedded resource declares `name` and
    `generateName` only, and an embedded resource no longer gets an empty
    `apiVersion`, `kind` and `metadata` filled in. All of them used to read
    as plain maps (`has(dyn(self.metadata).labels)` held: fail-open).
    Migration: a rule reading a free-form object's keys, or `metadata`
    beyond `name` and `generateName`, is one the apiserver refuses at
    registration (undefined field) or fails.
  - A value under a schema node with no `type` but
    `x-kubernetes-preserve-unknown-fields` binds as the apiserver's
    `unknownPreserved`: `dyn(self).u == 'x'` compares its JSON, `has()` on it
    is false, and `size()`, `+`, indexing, `string()` and `type()` fail
    (refused even where `|| true` would absorb the error, but for indexing:
    fail-closed). It used to bind as plain JSON (`dyn(self).u.size() == 1`
    held: fail-open).
  - `==` between such a list or object and a value of another kind (`dyn`)
    fails, as the apiserver's no-such-overload error, and so does `==`
    between an object holding fields its schema does not declare and a map
    not of its kind. The apiserver's `!=` holds there; kube-cel refuses it
    too (fail-closed), as `cel`'s equality answers only true or false.
  - kube-cel's list functions (`join`, `sum`, `min`, `max`, `isSorted`,
    `indexOf`, `lastIndexOf`, `first`, `last`, `sets.*`, `format`) take such
    lists and objects; `indexOf`, `lastIndexOf` and `sets.*` compare a set
    element as a set, and `first` / `last` return it as one.
- **Defaults, pruning and messages.**
  - Schema defaults fill in under a parent that was itself just defaulted
    (`spec: {default: {}}` with a defaulted `spec.x`), and a present-but-null
    field that is not `nullable` takes its default, as in the apiserver. A
    `default: null` is no default and is skipped.
  - A `null` field or map value under a schema node that is neither
    `nullable` nor has a `default` is pruned before defaulting and before the
    rules run, as the apiserver does (`apply_defaults` included): a rule sees
    it absent (`!has(self.x)` holds). A `null` list item stays. And no rule
    runs at a null node, nor below it; kube-cel used to evaluate the rules of
    a nullable object that was `null` with `self == null`.
  - Under `x-kubernetes-preserve-unknown-fields`, the rules under
    `additionalProperties` are evaluated and its `default`s applied, as the
    apiserver does. Both used to be skipped, so an object breaking such a rule
    passed (fail-open) and a rule reading such a default failed.
  - A `messageExpression` result is trimmed, and an empty, multi-line or
    over-5 KiB result falls back to `message` (CRD rules and VAP validations).
- **VAP variables.**
  - `object` / `oldObject` / `params` / `namespaceObject` are bound without
    field-name escaping, so `object.metadata.labels['app.kubernetes.io/name']`
    resolves. An integer past `i64` in them (and in an `optionalOldSelf`
    rule's `oldSelf.value()`) binds as a `double`, as Kubernetes' JSON decoder
    reads it, not a `uint`.
  - `namespaceObject` is `null` when no namespace object is set (a
    cluster-scoped request), as on the apiserver; `namespaceObject == null`
    used to fail as an unresolved reference.
- **The standard time and number conversions, `string()` of a time value,
  duration arithmetic and optional `or` / `orValue` follow cel-go v0.26**:
  `new_env()` puts cel-go's in front of `cel`'s, through macros, so general
  use and validation both see them.
  - `duration(string)` is Go's `time.ParseDuration` and `timestamp(string)`
    Go's `time.Parse(time.RFC3339, ..)` with cel-go's range. A duration past
    int64 nanoseconds, an exponent, a sign between components, trailing text,
    a lowercase `t` / `z`, a space for the `T` and a leap second are errors;
    they were accepted, a **fail-open**, now closed. A `,` before the
    fraction, a one-digit hour, an offset minute of 60 and the units `µs` /
    `μs` convert; they were errors. A timestamp offset of 24 hours or more,
    which Go takes, is still an error (fail-closed, a README "apiserver
    divergence" row).
  - A literal argument the conversion refuses fails to compile, reached or not
    (`true || duration('1e3s') > duration('0s')`), as the apiserver's
    literal validators refuse it at registration; so does `timestamp(n)` with
    an int literal outside cel-go's range, and a literal of any other type.
  - `string(<duration>)` prints float64 seconds (`9223372036.854776s`, it
    printed exact nanoseconds), and `string(<timestamp>)` keeps the
    timestamp's offset (`2024-01-15T10:30:00+09:00`, it printed UTC).
    `string(<double>)` prints Go's shortest `%g` form (`1e+06`, `1e-05`,
    `+Inf`; it printed `1000000`, `0.00001`, `inf`): a **fail-open**, now
    closed.
  - `double(string)` is Go's `strconv.ParseFloat` and `uint(string)` Go's
    `strconv.ParseUint(s, 10, 64)`. A value past the float64 range
    (`'1e400'`), `+nan` and a `+` before an unsigned integer are errors; they
    gave `±inf` or a value, a **fail-open**, now closed. `_` between digits
    (`'1_000'`) and a hexadecimal float (`'0x1p-2'`) convert; they were
    errors. A decimal of more than 800 significant digits that Go reads two
    ways is an error where the readings differ (fail-closed). `int(string)`
    already matched Go's `strconv.ParseInt`.
  - Duration `+` and `-`, the difference of two timestamps and a timestamp
    minus a duration stay within int64 nanoseconds, with cel-go's overflow
    checks; they went up to `cel`'s ±315,576,000,000 seconds. `-` negates a
    duration reached through `dyn`, and `int(<duration>)` is its nanoseconds;
    both were errors.
  - `x.or(y)` and `x.orValue(y)` evaluate `y` only when `x` is an optional
    holding no value (`optional.of(1).orValue(1 / 0)` is `1`, it was an
    error), and return an `x` that is not an optional as it is.
  - A map literal that repeats a key keeps the last entry (`{'k': 1, 'k':
    2}['k']` is 2), and `1` and `1u` are two keys, as in cel-go: `new_env()`
    turns off `cel`'s duplicate-key error, which refused both (fail-closed).
- **A type conversion of a constant that fails is refused at compile time**
  (`CompilationFailure`, reached or not): `int('x')`, `uint(-1)`,
  `string(b'\xff')`, `int(dyn('x'))`. The apiserver builds each rule's
  program at registration with cel-go's `OptOptimize`, which folds a
  conversion (`bool`, `bytes`, `double`, `duration`, `dyn`, `int`, `string`,
  `timestamp`, `type`, `uint`) of a literal, of a folded conversion or of a
  list or map literal of them, and rejects the CRD when one fails; such a
  rule passed client-side where evaluation did not reach it, a **fail-open**,
  now closed. Nothing else is folded, as in cel-go, except that cel-go also
  folds a type name read as a value (`type(int)`, `dyn(int)`) and kube-cel
  does not. No verdict changes: the only conversions of a type that
  type-check (`type`, `dyn`) cannot fail, and any other (`int(int)`) is in
  the no-type-checker exception.
- **Regular expressions are RE2's**, as in the apiserver (Go's `regexp`):
  `matches`, `find` and `findAll` check a pattern with a port of Go's
  `regexp/syntax` before the `regex` crate compiles it.
  - A pattern Go refuses and the `regex` crate takes (`\u0041`, `(?x)`, `a**`,
    `a{1001}`, `[a--b]`, `[[:foo:]]`, `\p{Emoji}`, `\p{sc=Greek}`, a Unicode 16
    script, `\p{Old_Italic}`) is an error; it matched, a **fail-open**, now
    closed.
  - A pattern the apiserver compiles at registration fails to compile, reached
    or not: the literal first argument of `matches` (cel-go's
    `ValidateRegexLiterals`; in the global `matches(s, p)` that is `s`), and the
    constant pattern of `matches`, `find` and `findAll`, a literal or a
    conversion of one (`dyn('..')`), which the apiserver compiles when it
    builds the program.
  - A pattern matches what Go matches with it: it is translated into the
    `regex` crate's syntax with Go's meaning. `\d`, `\w`, `\s` and `\b` are
    ASCII (`\s` without `\v`), `\<` and `\>` literals, `&&`, `--`, `~~` and
    `[` inside a class literals; the crate read them as Unicode classes, word
    boundaries and class operations, so `'٣'.matches('\\d')` held, a
    **fail-open**, now closed. Go's `\Q..\E`, octal escapes, a `{` that
    starts no repetition, `(?)`, repeated flags (`(?ii)`, `(?i-i)`),
    `\p{^Greek}`, `\p{Cs}`, a group name starting with a digit or used twice,
    which the crate refused, are carried over, and the crate may compile up
    to 128 MiB (it stopped at 10 MiB, short of `^[\p{L}\p{N}_-]{1,253}$`).
    Compiled patterns are kept process-wide, up to about 128 MiB of memory
    (estimated from the size each compiled under); one that needs more than
    32 MiB (`[\p{L}]{1,783}` and up) is compiled again on each use, in about
    0.2 to 0.4 seconds as measured. When the cache is full it is emptied
    whole, so patterns in use compile again too: the price of the bound.
    A translation past the crate's limits (nesting past about 250 levels; a
    size past 128 MiB, `\pL{1000}` four times) fails to compile where the
    apiserver compiles it at registration, and is an error noted as
    unsupported at evaluation (fail-closed). The crate's Unicode tables are
    16.0.0's, not Go's 15.0.0, and it folds a few characters Go does not: a
    match of a pattern that reads them (a Unicode class, or case folding of a
    character that has a case; not `(?i)[0-9]`) on a string holding a
    character the two may read otherwise (one Unicode
    15.0.0 does not assign, such as U+1C89, a letter since 16.0; `ΐ`, `ɤ`, a
    handful of others) is an error noted as unsupported (fail-closed); so is
    any match of such a pattern where `regex` resolves to tables other than
    16.0.0's, which kube-cel checks once.
- **Strings and `format()` follow cel-go v0.26**, the version kube-apiserver
  v1.36.1 uses:
  - `indexOf` / `lastIndexOf`: a negative offset is an error; with an offset an
    empty needle returns the offset unchanged (`'hello'.indexOf('', -5)` is
    `-5`), and `lastIndexOf` with an offset at or past the length is `-1`.
  - `split('')` splits into characters without empty ends; `join()` errors on
    a non-string element.
  - `replace(old, new, n)` with a negative `n` replaces every match, as Go's
    `strings.Replace` does (it replaced nothing, a **fail-open**:
    `'aaa'.replace('a', 'b', -1) == 'aaa'` held). `replace`, `findAll`,
    `charAt`, `indexOf` and `substring` no longer narrow an `int` with `as
    usize`, which truncated values past `u32::MAX` on 32-bit targets.
  - `%s`: a whole double prints without `.0` and turns scientific from exponent
    6 (`1e+06`); inside a list or map a double is `%.6f` and an infinity is
    quoted, map entries are sorted, and timestamps, durations, bytes and
    strings are written as CEL literals (`[duration("3600s")]`, `[b"abc"]`);
    a top-level timestamp is RFC 3339 and a duration its seconds (`5400s`).
    Bytes that are not UTF-8, and values with no `%s` form (an IP, a quantity),
    are errors.
  - The other verbs too: `format()` is a port of cel-go v0.26's formatter
    (`ext.Strings` version 2). `%f` and `%e` print through golang.org/x/text's
    en-US number formatter, as the apiserver's do: `%f` groups thousands
    (`'%.2f'.format([1500.0])` is `1,500.00`), `%e` always shows seven
    significant digits with a superscript exponent and reads the precision
    as a field width (`'%.2e'.format([1500.0])` is `1.500000×10⁰³`), both
    write NaN and the infinities as `NaN`, `∞`, `-∞`, take the strings
    `"NaN"`, `"Infinity"`, `"-Infinity"`, refuse an int, and keep a large
    precision the way x/text does (`'%.65536f'.format([1.5])` is `2`). A
    negative `%b` / `%o` / `%x` / `%X` is a sign and the magnitude (`-101`,
    not two's complement). `%s` takes Go's shortest digits, which break a
    rounding tie towards even (`1.0000000000000002e+15`), escapes in a list
    or map every code point go1.26.2's `strconv.IsPrint` does not print
    (Unicode 15.0.0, so `["\u0378"]`), and writes a type value bare
    (`[int]`). The output differed before, a documented **fail-open**, now
    closed.
  - `format()` with a precision past 65535 (`'%.65536f'`, `'%.65535e'`) no
    longer panics in Rust's formatter. A precision past the int64 range is an
    error for every verb, as in cel-go.
- **Lists.**
  - `sort()` and `sortBy()` order elements with equal keys as the apiserver
    does: both reorder the indices with a port of go1.26.2's `sort.Slice` (the
    toolchain of kube-apiserver v1.36.1), as cel-go v0.26 does, so
    `lists.range(20).sortBy(x, x % 2)[0] == 0` is false, as there. kube-cel's
    own `sort()` sorted stably, so from 13 elements on the order differed: a
    rule reading where `sort()` put a `-0.0` among equal zeros passed, a
    **fail-open**. `lists.range(-1)` is `[]`, and a list holding a NaN sorts
    (no comparison with it is "less"), as in cel-go; both used to be errors
    (fail-closed).
  - `isSorted()`, `min()` and `max()` compare as the apiserver's lists library
    does: bytes order (a `format: byte` list, `[b'c', b'a'].min()`, was an
    error), a pair cel-go cannot compare, such as one with a NaN, replaces or
    unsorts nothing (`[1.0, double('NaN'), 0.5].min()` is 0.5, was an error),
    and an element that is not comparable is an error even alone
    (`[null].min()` and `[null, null].isSorted()` passed: **fail-open**).
  - `<list>.sum()` overflow is an error instead of a panic (debug) or a wrapped
    result (release).
- **URLs.**
  - Validity in `isURL`, `url()` and `format.uri()` follows Go's
    `url.ParseRequestURI`: a bad `%` escape, a control character, a leading
    space or, for http/https, a second colon in the host is refused; an
    absolute path (`/foo`) is accepted by `format.uri()`.
  - The `url()` accessors return what Go's `url.URL` holds, as in the
    apiserver, which checks the string with `url.ParseRequestURI` and parses
    it with `url.Parse`: `getHost()` / `getPort()` keep a default port
    (`url('http://h:80/').getPort()` is `80`, it was empty), `getHost()` of a
    path is empty (it was `localhost`), the host is unescaped but not
    lowercased, a `#fragment` is split off, `//host/p` has a host,
    `getEscapedPath()` keeps the path's own escaping, `getQuery()` skips a
    pair with a `;` or a bad escape, `==` compares Go's `String()` (so
    `url('http://H:80/') != url('http://h/')`), and `url('*')` converts. The
    accessors read a WHATWG parse before, which differed in each of these: a
    **fail-open**, now closed. A component that is not UTF-8 once unescaped
    (`url('http://h%80x/').getHost()`) is an error (fail-closed), as a CEL
    string here must be UTF-8.
- **Named formats.**
  - `format.byte()` requires standard, padded base64 and refuses the empty
    string.
  - `format.date()` / `format.datetime()` refuse a `+`-signed component and a
    leap second. `format.datetime()` otherwise follows kube-openapi's
    `strfmt.IsDateTime`, which the apiserver calls, instead of RFC 3339: the
    offset is not range-checked (`+99:99`), any character may stand before
    the fraction digits (`10:30:00,5Z`, `10:30:0012Z`), and only the first two
    `t`-separated pieces are read (`2024-01-15T10:30:00ZtXYZ`); each of these
    was refused here (fail-closed). Both report the apiserver's messages
    (`invalid date`, `invalid datetime`). `format.datetime().validate(..)` on
    a time with a multi-byte character in the seconds
    (`'2024-01-15T12:30:0é:00Z'`) reports an invalid date-time instead of
    panicking.
  - `format.uri().validate(s)` reports Go's `*url.Error` text, as the
    apiserver does: `parse "<s>": <reason>`, with Go's `%q` quoting
    (non-printable characters and bytes that are not UTF-8 escaped as Go
    escapes them) and, for a bracketed host, `netip.ParseAddr`'s reason
    (`invalid host: ParseAddr("x"): unable to parse IP`). Before, the
    `parse "<s>": ` part and the `ParseAddr` reason were missing and Rust
    quoting was used.
  - The name formats (`format.dns1123Label()`, `dns1123Subdomain`,
    `dns1035Label`, their `*Prefix` forms, `qualifiedName` and `labelValue`)
    are ports of k8s.io/apimachinery's validators, which the apiserver calls,
    and `.validate(s)` reports their messages verbatim; before, every message
    differed (a rule reading `.validate(s).value()` could pass where the
    apiserver rejects). The `*Prefix` forms mask a trailing `-` as
    apimachinery does, so `format.dns1035LabelPrefix().validate('')` is
    invalid (was valid: **fail-open**). `format.uuid()` is strfmt's `IsUUID`,
    whose dashes are each optional (`550e8400e29b41d4a716446655440000` is
    valid, was refused), with the apiserver's message.
- **Semver follows blang/semver v4**, which the apiserver calls: a numeric
  pre-release identifier past a uint64 is not a version
  (`isSemver('1.0.0-18446744073709551616')` is false, was true:
  **fail-open**), and `==`, `compareTo`, `isGreaterThan` and `isLessThan`
  ignore build metadata (`semver('1.0.0+a') == semver('1.0.0+b')`). Lenient
  `semver(x, true)` / `isSemver(x, true)` strip only a lowercase `v` and
  refuse a fourth component (`1.2.3.4`).
- **Quantities follow k8s.io/apimachinery's `resource.Quantity`**, which the
  apiserver's library calls, representation included. Each of these
  differed, most of them **fail-open**.
  - Parsing is `ParseQuantity`: no trimming (`isQuantity(' 1 ')` is false),
    `-` or `+` alone is 0, a non-zero value finer than nano units rounds up
    to them (`quantity('1e-10') == quantity('1n')`), and a binary quantity is
    capped at the largest int64 (`10Ei`). `isInteger()` / `asInteger()` are
    `AsInt64()`: only a quantity kept as an int64 times a non-negative power
    of ten converts, so `1000m`, `1.0`, `1e40`, `1.5Gi` and `1Pi` are not
    integers (`asInteger()` returned a value where the apiserver errors).
    `asApproximateFloat()` multiplies by Go's `math.Pow10` table (`0e1000` is
    NaN). `add` / `sub` keep Go's representation, and `==` with
    an `add` / `sub` result on the right is an error in the apiserver; here
    it is false, and a rule reaching it is refused.
  - Comparison is exact for magnitudes beyond `i128` scaling (`compareTo`
    used to call quantities 3 apart equal there).
  - Quantities have a supported range: exponents and scales (the exponent
    less the fraction digits, with an SI or binary suffix) within ±1000, and
    a mantissa within `i128`. The exponent is read as an int64, as in the
    apiserver, so one past that range is not a quantity (`isQuantity` false),
    as is a malformed string however many digits it has, and a number without
    digits that the apiserver parses on its `inf.Dec` path (`e-1001`; on its
    int64 path, `e1001` is a 0 past the range). One within it but past
    ±1000, before or after trailing zeros move into the scale (`1e1001`,
    `10e1000`, `0e2147483648`, a sum such as `5e1000 + 5e1000`), or a
    mantissa past `i128`, is an error from `quantity()` and `isQuantity()`
    alike, and a rule or VAP expression that reaches one fails with an
    `EvaluationError`, even where CEL absorbs the error (`… || true`). The
    apiserver parses these, narrowing the exponent to int32
    (`quantity('1e4294967296') == quantity('1')`) and, near the int32 bounds,
    computing for minutes or panicking: a documented fail-closed limit.
    Before, `isQuantity` was false for an exponent past the int32 range or a
    mantissa past `i128`, so `!isQuantity(s)` passed (a **fail-open**), and an
    exponent near the int32 bounds overflowed the scale arithmetic (a panic
    in debug builds, a wrapped scale in release).
  - `add` / `sub` past the `i128` range is an error instead of a panic or a
    wrapped result (the apiserver's quantities are exact, so this is a
    documented fail-closed limit), and it fails the rule even where CEL
    absorbs its error (`… || true`): the apiserver has written the exact
    result into a receiver holding an `inf.Dec`, which kube-cel cannot, so a
    later read of it could pass (**fail-open**).
  - `add` / `sub` with a zero term no longer fails with `quantity overflow`
    when the scales are far apart
    (`quantity('0').add(quantity('0e-1000'))`): a zero needs no power of ten
    to align.
  - A quantity's state is shared and updated as in the apiserver, which
    keeps quantities behind pointers: `add` / `sub` copy the receiver, and a
    receiver held as an `inf.Dec` shares it with the copy, which the result
    is written into, so the receiver takes the new value
    (`[quantity('1.5Gi')].all(q, q.add(1).compareTo(q) == 0)` is true).
    Before, the receiver kept its value, and a rule reading it after `add`
    could pass where the apiserver rejects (**fail-open**).
  - `compareTo`, `isGreaterThan` and `isLessThan` turn an `int64Amount`
    receiver into an `inf.Dec` when the other side holds one, as Go's `Cmp`
    does with `AsDec()`: `[quantity('1')].all(q, q.isLessThan(quantity('1.5Gi'))
    && q.isInteger())` is false, and a later `asInteger()` an error. Before,
    the receiver stayed an integer (**fail-open**). `==` compares copies and
    changes neither side, as in the apiserver.
  - A quantity update whose result depends on evaluation order is refused at
    compile time (`CompilationFailure`, reached or not): an `add`, `sub`,
    `compareTo`, `isGreaterThan` or `isLessThan` on a quantity held by a
    variable (a comprehension's, or reached through `dyn(..)` or `[q][0]`)
    in a call's receiver or arguments, or in the second operand of a
    two-operand call, next to a part that could fail or read a quantity. The
    apiserver's quantities share state, and `cel` evaluates a call's
    arguments before its receiver and skips a second operand after a
    failure, where cel-go does neither; such a rule could pass client-side
    where the apiserver rejects it. Fail-closed: an update of a new quantity
    (`quantity(s)`, `add` / `sub` on one), one next to parts that can neither
    fail nor read a quantity (a comprehension variable, a literal,
    `quantity(<literal>)`) and one where both orders agree (`&&`, `||`) are
    not refused. For the same reason an error kube-cel has where the
    apiserver has a value (a timestamp offset of a day or more, a URL
    component that is not UTF-8, a declared function kube-cel does not
    implement, a policy's `authorizer`, which kube-cel does not bind) now
    fails the rule even where CEL absorbs the error (`… || true`).
- With every function feature off, `indexOf`, `lastIndexOf`, `isGreaterThan`,
  `isLessThan` and `compareTo` are no longer registered: there was nothing for
  them to call, and they took those names from a user's own functions. Each
  pair now needs a feature that implements it (`strings` or `lists`;
  `semver_funcs` or `quantity`).
- **README "apiserver divergence" table.** The apiserver does **not**
  evaluate `cel.bind` (its base environment has no `ext.Bindings`); it
  rejects the CRD at registration. The `sortBy` and two-variable
  comprehension rows are gone (both now evaluate), and rows for `math.*`,
  the `isIPv4`-style helpers, `base64.*` / `jsonpatch.escapeKey`, global
  `min` / `max`, string `reverse()` and `<IP>.string()` are added (all fail
  closed). The authz library row now covers every function the apiserver
  declares that kube-cel does not implement (`optional.unwrap`,
  `<list>.unwrapOpt()`, ..). A fail-closed row covers a `timestamp(string)`
  zone offset of 24 hours or more, which Go parses. Other new fail-closed
  rows cover what `cel` cannot evaluate as cel-go does: type names with no
  `cel` value and message constructions (`net.IP`,
  `google.protobuf.Duration{seconds: 1}`), `.self` / `.oldSelf` in a CRD
  rule, a two-variable comprehension over fields the schema does not
  declare, `has()` / `.?` on a scalar, a set read from a list or map a
  variable holds and `+`ed twice, iterating a set or map list that `+` gave
  an element its items refuse, a refused list or map field of an element `+`
  added, a plain map or list (an `additionalProperties` map) that a set or
  map list's `+` or `==` takes or a typeless preserve-unknown value compares
  with, and a map list with four or more keys and a number key value. Two
  last rows document the known fail-open classes. `cel` has no type checker yet
  ([cel-rust#350](https://github.com/cel-rust/cel-rust/issues/350)), so a
  type, arity or call-shape error the apiserver rejects at registration
  passes when evaluation does not reach it. And `cel` has no cost model yet
  ([cel-rust#230](https://github.com/cel-rust/cel-rust/discussions/230)), so
  a rule the apiserver rejects for its estimated cost at registration
  (`self.l.all(x, self.l.exists(y, x == y))` on a list with no `maxItems`),
  or an object it rejects for a rule's runtime cost
  (`lists.range(1000000).size() > 0`), passes. The claim that a passing
  result is always at least as strict as the apiserver is narrowed
  accordingly.

## [0.8.0] - 2026-06-19

Apiserver fidelity for schema `default`s
([#9](https://github.com/kube-rs/kube-cel/issues/9)). The apiserver applies
`default`s during admission **before** evaluating CEL; plain `validate` now does
the same, so a CR that omits a defaulted field is validated as the apiserver
sees it.

### Breaking
- Plain validation now applies schema `default`s before evaluating rules.
  `Validator::{validate, validate_with_context, validate_compiled,
  validate_compiled_with_context}` and the free `validate` / `validate_compiled`
  all default first. The function **signatures are unchanged**, but **runtime
  verdicts can change**: a CR omitting a defaulted field may flip. This closes a
  divergence in both directions, the dangerous one being **fail-open** — e.g. a
  rule `!has(self.x)` with a defaulted `x` and an object omitting `x` previously
  ACCEPTed an input the apiserver REJECTs (verified live against kind via
  `just parity`). Under cargo 0.x caret rules this behavior change is breaking,
  hence 0.8.0.
  - Migration: usually none — the new behavior matches the apiserver, so most
    callers become *more* correct for free. There is no longer a no-defaulting
    fast path; if you were calling `validate_with_defaults` for fidelity, plain
    `validate` now suffices.

### Fixed
- `apply_defaults` now recurses into `additionalProperties` (map values), not
  just `properties` and array `items`. A `default` inside a nested map value was
  previously skipped, leaving `validate_with_defaults` itself **fail-open** on
  that shape (live-confirmed: apiserver REJECT, kube-cel ACCEPT). Defaults at the
  schema root under `additionalProperties` remain moot — the apiserver rejects
  that shape at registration ("must not be used at the root").
- Apiserver-fidelity sweep (161 cases across all 13 extension libraries + the
  validation engine, measured live against kind) closed three divergences:
  - **`string.format()` `%e`** now emits Go-style scientific notation
    (`1.50e+03`: signed exponent, zero-padded to ≥2 digits) instead of Rust's
    `1.50e3`. **fail-open** before (kube-cel accepted a `.format()` equality the
    apiserver rejects).
  - **`string.format()` `%s` on a map** now renders `{"a":1}` (no space after
    the colon), matching cel-go's `fmt.Sprintf("%s:%s", …)`. **fail-open** before.
  - **`string.lastIndexOf(sub, i)`** now treats `i` as the inclusive last *start*
    index of a match (cel-go semantics) — a match may extend past `i`. Previously
    `i` was an exclusive end bound, so `'abcabc'.lastIndexOf('abc', 3)` returned
    `0` instead of `3`. **fail-open** before.
  - **`format: byte`** string fields now bind as CEL `bytes` (base64-decoded, as
    the apiserver does), so `size()`/indexing operate on the decoded bytes. Was
    **fail-closed** (kube-cel kept the encoded string and over-rejected).

### Added
- `CompiledSchema` carries a `default` field, so the compiled validation path
  (`validate_compiled*`) applies defaults identically to the schema path.
- Parity coverage for defaults: `defaults_parity_*` cases in
  `tests/apiserver_parity.rs` assert plain `validate` matches the apiserver
  across `properties`, array `items`, nested structs, `additionalProperties` map
  values, and `required` + default interaction.

### Changed
- `just clippy` now lints `--all-targets` (tests, examples, benches), not just
  the library.


## [0.7.1] - 2026-06-19

### Fixed
- `additionalProperties` map keys are no longer field-name escaped during value
  conversion ([#8](https://github.com/kube-rs/kube-cel/issues/8)). The apiserver
  escapes only declared `properties` (struct field names); map keys under
  `additionalProperties` pass through literally (`apiserver/pkg/cel/common`
  `MapValue`). Before this fix every object key was escaped, so an instance key
  like `rio.build/fetcher` became `rio__dot__build__slash__fetcher` and a rule
  such as `'rio.build/fetcher' in self.nodeSelector` could never fire — a
  **fail-open** divergence that passed inputs the apiserver rejects. Reserved-word
  map keys (e.g. `in`) are likewise no longer escaped. Behavior change: a
  validation that previously (incorrectly) passed may now correctly fail.

### Added
- `tests/apiserver_parity.rs` + `just parity`: a gated harness that runs each
  `(schema, object)` case against a throwaway kind cluster via
  `kubectl apply --dry-run=server` and asserts kube-cel's verdict matches the
  real apiserver. Confirms the #8 fix is faithful on every reachable case (typed
  `map[string]T`); free-form `additionalProperties: true` cannot use `in` at all
  (the apiserver rejects such a rule at registration), so its keys are moot.


## [0.7.0] - 2026-06-16

Breaking release that reshapes the validation surface around the idiomatic Rust
`validate()?` flow, addressing review feedback on the client-side CEL derive
([kube#2011](https://github.com/kube-rs/kube/pull/2011#discussion_r3419318288)).

### Breaking
- The eight validation entry points now return `Result<(), ValidationErrors>`
  instead of `Vec<ValidationError>`:
  `Validator::{validate, validate_with_context, validate_compiled,
  validate_compiled_with_context, validate_with_defaults,
  validate_with_defaults_and_context}` and the free `validate` /
  `validate_compiled`. They return `Ok(())` when every rule passes (so callers
  can `?`-propagate failure) and `Err(ValidationErrors)` otherwise — no more
  explicit `is_empty()` check inverting the success path. Migration: a passing
  assertion `assert!(errors.is_empty())` becomes `assert!(result.is_ok())`; to
  inspect failures, `let errors = result.unwrap_err()` yields the same data
  (`ValidationErrors` derefs to `[ValidationError]`).

### Added
- `ValidationErrors`, the aggregate `Err` payload: a newtype over
  `Vec<ValidationError>` with `Deref<Target = [ValidationError]>`, owned and
  borrowed `IntoIterator`, `Display` (one failure per line), `std::error::Error`,
  `From<Vec<ValidationError>>`, and `into_vec` / `as_slice` / `len` / `is_empty`
  accessors. No information is lost relative to the old `Vec` return — the full
  slice stays reachable inside the `Err`.

## [0.6.1] - 2026-06-13

### Fixed
- docs.rs build for 0.6.0 failed: the crate gated `feature(doc_auto_cfg)` behind
  `#![cfg_attr(docsrs, …)]`, and that nightly feature was removed in Rust 1.92.0
  (merged into `doc_cfg`), so the docs.rs build (nightly + `--cfg docsrs`)
  aborted with `error[E0557]: feature has been removed`. Switched to
  `feature(doc_cfg)`. Library builds were unaffected — `docsrs` is only set by
  docs.rs — so this is a documentation-only fix. The `just doc` recipe now also
  reproduces the docs.rs build (nightly + `--cfg docsrs`) so this class of
  docsrs-only breakage can no longer pass a stable `cargo doc`.


## [0.6.0] - 2026-06-13

Breaking release that reshapes the registration surface around two user
journeys (register functions / validate a CRD) and tightens 1.0-grade API
hygiene. See [#6](https://github.com/kube-rs/kube-cel/issues/6).

### Breaking
- `register_all(&mut ctx)` free function removed; register via the new
  `KubeCelExt` trait instead.
- The 13 extension-function modules (`strings`, `lists`, `sets`, `regex_funcs`,
  `urls`, `ip`, `semver_funcs`, `format`, `quantity`, `jsonpatch`,
  `named_format`, `math`, `encoders`) are now private. They only ever exposed a
  `register` function plus opaque CEL newtypes, so no usable API is lost.
- `#[non_exhaustive]` added to the public `ScopeContext`, `WarningKind`,
  `ErrorKind`, and `CompilationError` enums. Downstream `match` on these now
  needs a wildcard arm.
- Schema nesting deeper than the depth cap (64) now **fails closed**: validation
  surfaces a `SchemaTooDeep` error instead of silently skipping the over-deep
  subtree. Objects that previously passed validation against a too-deep schema
  (the deep rules were never evaluated) now report an error. This closes a
  fail-*open* false-negative against the crate's apiserver-equivalence claim.
- A rule whose `messageExpression` fails to compile now **fails closed**: it
  surfaces a `CompilationFailure` (CRD path) / failed result (VAP path) instead
  of being silently dropped and the rule evaluated with the static `message`.
  The apiserver rejects such a CRD/policy at registration, so objects that
  previously passed (the broken `messageExpression` only affected message text)
  now report an error. Closes a second fail-*open* divergence, mirroring the
  existing `rule`-compilation path. A new additive `CompilationError::MessageExpressionParse`
  variant carries the rule, the offending `messageExpression`, and the cause via
  `source()`.
- The validation submodules (`values`, `analysis`, `escaping`, `defaults`,
  `compilation`, `validation`, `vap`) are now **private**; the public API is a
  flat set of crate-root re-exports. Replace `kube_cel::<module>::Item` with
  `kube_cel::Item` (e.g. `kube_cel::vap::AdmissionRequest` →
  `kube_cel::AdmissionRequest`, `kube_cel::compilation::compile_schema` →
  `kube_cel::compile_schema`). The internal file layout is no longer part of the
  API. **Downstream note (kube-core):** the `cel` feature's `to_cel_request`
  bridge must update `kube_cel::vap::AdmissionRequest` → `kube_cel::AdmissionRequest`.
- `json_to_cel`, `json_to_cel_with_schema`, `json_to_cel_with_compiled`, and
  `escape_field_name` are no longer public — they are internal conversion helpers
  that returned the pre-1.0 `cel::Value` type, and had no external callers.
- `#[non_exhaustive]` added to the output structs `ValidationError`,
  `AnalysisWarning`, `VapResult`, `CompiledSchema`, and `CompilationResult`.
  These are constructed by the crate, never by callers, so no usable API is lost;
  downstream code still reads their fields (and may need `..` in any destructuring
  pattern). Future fields can now be added without a breaking change. `RootContext`
  stays exhaustive on purpose — it is a caller-constructed *input* type.
- `VapEvaluator::compile_expressions` and `evaluate_compiled` now use the typed
  `VapError` instead of `String` in their `Result` error type. A bare `String`
  does not implement `std::error::Error`, could not be a `source()`, and clashed
  with the structured `CompilationError` the crate already ships.
- `CompilationError::Parse`'s `source` field is now
  `Box<dyn std::error::Error + Send + Sync>` instead of the concrete pre-1.0
  `cel::ParseErrors`, so that `cel` type is no longer frozen into a public,
  matchable enum-variant field (reachable via `CompiledSchema`). The parse cause
  is still reachable through `std::error::Error::source()`. (`InvalidRule` keeps
  its `serde_json::Error` payload: `serde_json` is a stable 1.0 public dependency,
  not a freeze hazard.)

### Added
- `ErrorKind::SchemaTooDeep` and `CompilationError::SchemaTooDeep { depth }`
  variants, emitted when schema nesting exceeds the depth cap (see Breaking).
- `VapError`: a typed compile error for the VAP path, carrying the offending
  expression and chaining the underlying `cel` parse error via
  `std::error::Error::source()`.
- `ValidationError` now implements `std::error::Error::source()`, chaining the
  underlying `cel` `ExecutionError` for runtime evaluation failures
  (`EvaluationError` / `UnsupportedReference`). Previously the `Error` impl was
  empty and the cause was only flattened into the message string. (Compile-time
  causes are not chained: `cel::ParseErrors` is `!Clone` and reached only behind
  a shared borrow; its detail stays in the message and the typed cause remains
  reachable via `CompiledSchema::compilation_errors`.)
- `ErrorKind::UnsupportedReference`: a rule that references a CEL macro the
  `cel` crate does not implement (`sortBy`, `cel.bind`, two-arg comprehensions)
  or a feature disabled at compile time now reports this distinct kind instead
  of a generic `EvaluationError`, so callers can tell a kube-cel coverage gap
  apart from a genuine runtime error. Still fail-closed (the object is rejected).
- `full` umbrella feature, enabling every extension-function group **and** the
  `validation` engine. `default` enables the 13 function groups only (not
  `validation`); use `default-features = false` to narrow, then `full` to
  restore the whole surface in one flag.
- `Deserialize` for the validation output types `ValidationError`, `ErrorKind`,
  `AnalysisWarning`, and `WarningKind` (they already derived `Serialize`),
  matching the round-trip support the VAP types already have. `ValidationError`'s
  non-serialized `source` cause deserializes to `None`.
- Examples: `kube_workflow` (client-side validation before a kube-rs apply),
  `error_chain` (walking `ErrorKind` + `source()`), `message_expression`
  (CRD-path dynamic messages), `transition_rules` (`oldSelf` UPDATE semantics).

### Documentation
- "Versioning and stability" section (README + crate docs): kube-cel cannot reach
  1.0 until `cel` does (C-STABLE), and a two-tier stability contract — Tier 1
  (registration surface, committed) vs Tier 2 (validation engine, evolving).
- README gained an "apiserver divergence" table mapping every known way the
  validator's verdict differs from the API server, with direction (all
  fail-closed). Pinned by `tests/apiserver_divergence.rs`. Notably, unsupported
  CEL macros (`sortBy`, `cel.bind`, two-arg comprehensions) surface as
  `EvaluationError`, not a compile error, because they parse but lack a `cel`
  0.13 implementation.
- `KubeCelExt` trait: `register_all(&mut self) -> &mut Self` and the builder
  sugar `with_all(self) -> Self`. This is the single registration entry point.
  The trait is sealed (implemented only for `cel::Context`) so methods can be
  added later without a breaking change.
- `Debug` for `VapEvaluator`, `VapEvaluatorBuilder`, and `CompiledVapExpression`
  (the latter via a manual impl that skips the `!Debug` `cel::Program` fields).
- `Hash` for `GroupVersionKind` and `GroupVersionResource` so they can be used
  as `HashMap`/`HashSet` keys, as is idiomatic for k8s identifiers.
- `pub use cel;` — the `cel` crate is re-exported at the crate root for version
  coherence. Import `cel` types via `kube_cel::cel`.
- Crate-root re-exports for the validation journey: `Validator`,
  `ValidationError`, `CompiledSchema`, `VapEvaluator`, `SchemaFormat`.

### Fixed
- Broken crate-root intra-doc links on docs.rs: `[package.metadata.docs.rs]`
  now builds all features, and CI gained a default-feature doc check so the
  links can no longer regress.
- `KubeCelExt::register_all` rustdoc no longer claims to mirror the apiserver's
  `KnownLibraries()` mechanism (which enumerates libraries, it does not
  register); it now describes the shared bundle-the-whole-set philosophy.

### Internal
- The schema-depth guards in `compilation`, `validation`, and `defaults` now
  share a single `MAX_SCHEMA_DEPTH` constant instead of three hardcoded `64`s,
  so compile/validate/default depth limits can no longer silently diverge.

### Migration
- Registration:
  - Before: `kube_cel::register_all(&mut ctx);`
  - After:  `use kube_cel::KubeCelExt;` then
    `let ctx = cel::Context::default().with_all();`
    (or `ctx.register_all();` for an existing borrowed context).
- `cel` is now re-exported: prefer `use kube_cel::cel;` over a separate `cel`
  dependency to guarantee a matching version.
- Feature narrowing requires `default-features = false`; listing features
  without it just re-adds them on top of the (complete) default set.


## [0.5.4] - 2026-05-14

### Fixed
- `sets.contains` / `sets.equivalent` / `sets.intersects`, `lists.indexOf` / `lastIndexOf` / `distinct` now match cel-go's standard equality: cross-type numeric coercion across `Int`/`UInt`/`Float` (e.g. `sets.equivalent([1, 2, 3], [3u, 2.0, 1])` → `true`) and structural equality for nested `List` / `Map` elements (e.g. `sets.equivalent([['a']], [['a']])` → `true`). Previously every cross-type or nested comparison fell through a catch-all and returned `false`. ([#5](https://github.com/kube-rs/kube-cel/issues/5))
- `lists.isSorted` / `min` / `max` / `sort` now coerce across `Int`/`UInt`/`Float` instead of erroring on mixed numeric types (e.g. `[1, 2u, 3.0].isSorted()` → `true`).

### Changed
- Internal `value_ops::{val_eq, val_lt, val_le, compare_values}` helpers now delegate to `cel::Value`'s `PartialEq` / `PartialOrd`. Behavior note: `[NaN, 1.0].isSorted()` previously returned `false`; it now errors (`"cannot compare"`), matching IEEE 754 / cel-go semantics around undefined NaN ordering.


## [0.5.3] - 2026-03-16
### Added
- `Validator::validate_with_defaults_and_context()` — combines schema defaults + root context in one call
- `Serialize` derive on `ValidationError`, `ErrorKind`, `AnalysisWarning`, `WarningKind` — enables JSON output for CI tooling
- `Deserialize` derive on `VapResult`
- `PartialEq, Eq` derive on all VAP types (`VapResult`, `VapExpression`, `AdmissionRequest`, `GroupVersionKind`, `GroupVersionResource`)
- `compile_rule` and `compile_schema_validations` are now `pub` (were `pub(crate)`)
- Examples: `vap_evaluation`, `static_analysis`, `defaults_and_context`
- README sections for VAP evaluation, static analysis, schema defaults, and root-level variables

### Fixed
- `convert_number` no longer panics on non-representable floats (uses `unwrap_or(NAN)`)
- Recursion depth limit (64) on `compile_schema`, `walk_schema`, `walk_compiled`, `apply_defaults` — prevents stack overflow on deeply nested schemas
- Convenience functions `validate()` / `validate_compiled()` now share a single `thread_local` Validator instance (was two separate)


## [0.5.2] - 2026-03-16
### Added
- `VapEvaluator::compile_expressions()` / `evaluate_compiled()` — pre-compile VAP expressions once, evaluate many times
- `analysis::analyze_rule()` — combined scope + cost analysis in a single CEL compilation pass

### Changed
- `Validator::new()` now pre-registers all CEL functions (was per-call). Repeated `validate()` calls skip ~90 function registrations
- `Validator` no longer derives `Clone` (`Context` is not `Clone`; use `Validator::new()` to create instances)
- Convenience functions `validate()` / `validate_compiled()` use `thread_local!` caching

### Performance
- Eliminated redundant `register_all` + `Context::default()` on every validation call
- `apply_defaults` skips cloning when no defaults need to be applied
- `json_to_cel_with_schema` hoists `schema.get("items")` outside array iteration
- `#[inline]` on hot-path helpers (`convert_number`, `join_path`, `escape_field_name`, etc.)


## [0.5.1] - 2026-03-16
### Added
- **Schema composition walking** — `allOf`/`oneOf`/`anyOf` branches are now walked for `x-kubernetes-validations` rules
- **Root-level variables** — `RootContext` struct for binding `apiVersion`, `apiGroup`, `kind` at the CRD root level
  - `Validator::validate_with_context()` / `validate_compiled_with_context()`
- **`x-kubernetes-preserve-unknown-fields`** — `additionalProperties` walking is skipped when this flag is set
- **`x-kubernetes-embedded-resource`** — `apiVersion`, `kind`, `metadata` fields are injected as defaults for embedded resource nodes
- **`SchemaFormat::IntOrString`** — `x-kubernetes-int-or-string` annotation is now recognized (prevents `format:` from being misinterpreted)
- **Default value injection** — `defaults::apply_defaults()` pre-processes objects with schema `default` values
  - `Validator::validate_with_defaults()` convenience method
- **Static analysis** (`analysis` module)
  - `check_rule_scope()` — detect variables not available in the given scope (CRD validation vs admission policy)
  - `estimate_rule_cost()` — heuristic cost estimation warning when rules may exceed K8s budget (1M cost units)
  - `ScopeContext::CrdValidation` / `ScopeContext::AdmissionPolicy`
- **ValidatingAdmissionPolicy evaluator** (`vap` module)
  - `VapEvaluator` with builder pattern — client-side evaluation of VAP CEL expressions
  - `AdmissionRequest`, `GroupVersionKind`, `GroupVersionResource` types with serde support
  - Binds `object`, `oldObject`, `request`, `params`, `namespaceObject` variables
  - `messageExpression` support with static message fallback
- **Schema bounds in `CompiledSchema`** — `max_length`, `max_items`, `max_properties` fields for cost estimation
- `#[non_exhaustive]` on `SchemaFormat` enum (future-proofing)

### Changed

- `AnalysisWarning` now derives `PartialEq, Eq`


## [0.5.0] - 2026-03-13

### Changed

- **BREAKING**: `cel` dependency updated from 0.12 to 0.13 (`paste` → `pastey`, resolves RUSTSEC-2024-0436)
- `extractions/setup-just` CI action updated from v2 to v3
- `rustfmt.toml` added (aligned with kube-rs/kube style)
- `dependabot.yml` added for automated dependency updates

## [0.4.4] - 2026-03-11

### Changed

- Repository transferred to [kube-rs](https://github.com/kube-rs) organization
- `rust-version = "1.88"` explicitly specified (aligned with kube-rs/kube)
- `homepage = "https://kube.rs"` added
- README rewritten with full documentation
- LICENSE copyright notice filled in
- CI updated: `master` → `main` branch

## [0.4.3] - 2026-03-03

### Fixed

- `string()` dispatch no longer overrides cel built-in type conversion — `string(int)`, `string(bool)`, `string(timestamp)`, `string(duration)` now work correctly alongside IP/CIDR `.string()`

### Added

- `justfile` for local pre-publish checks (`just check` runs fmt, clippy, test-all, feature-check, doc)

## [0.4.2] - 2026-03-03

### Added

- `math.sqrt(double)` — square root function (returns NaN for negative input, matching cel-go)
- `<IP>.string()` / `<CIDR>.string()` — convert IP/CIDR opaque types back to string
- `<list>.first()` — returns `optional.of(first_element)` or `optional.none()` for empty list
- `<list>.last()` — returns `optional.of(last_element)` or `optional.none()` for empty list

### Fixed (cel-go parity)

- `charAt(len)` now returns `""` instead of error (matching cel-go behavior)
- `split(sep, 0)` now returns `[]` instead of `[""]`; negative limit returns all splits
- `strings.quote` now escapes `\a`, `\b`, `\f`, `\v` control characters
- `base64.decode` now accepts unpadded input (matching cel-go behavior)
- `%b` format verb with bool now outputs `"1"`/`"0"` instead of `"true"`/`"false"`

### Tests

- Added ~60 cel-go parity tests across all modules (strings, math, lists, sets, ip/cidr, quantity, regex, format, semver, encoders)

## [0.4.1] - 2026-03-03

### Changed

- CI feature-check now covers `math` and `encoders` features
- Renamed `tests/validation_spike.rs` → `tests/cel_evaluation.rs`
- Updated `examples/basic.rs` to showcase all new 0.4.0 features (math, base64, reverse, sort, range, flatten, CIDR.ip, IP version helpers, named format)

## [0.4.0] - 2026-03-03

### Changed

- **BREAKING**: `compile_rule()`, `compile_schema_validations()`, `SchemaFormat::from_schema()`, `parse_go_duration()` are now `pub(crate)` (no longer part of the public API)
- **BREAKING**: `ValidationError` now has a `kind: ErrorKind` field
- `ValidationError` now derives `PartialEq` and `Eq`
- `Validator` now derives `Clone` and `Debug`
- Added `#[must_use]` to `validate()`, `validate_compiled()`, `compile_schema()`, `json_to_cel()`, `json_to_cel_with_schema()`, `json_to_cel_with_compiled()`, `escape_field_name()`

### Added

- **Math extension library** (`math` feature) — 17 functions
  - Rounding: `math.ceil`, `math.floor`, `math.round`, `math.trunc`
  - Numeric: `math.abs`, `math.sign` (int/uint/double polymorphic)
  - Inspection: `math.isInf`, `math.isNaN`, `math.isFinite`
  - Bitwise: `math.bitAnd`, `math.bitOr`, `math.bitXor`, `math.bitNot`, `math.bitShiftLeft`, `math.bitShiftRight`
  - Variadic: `math.greatest`, `math.least`
- **Base64 encode/decode** (`encoders` feature)
  - `base64.decode(<string>) -> bytes`
  - `base64.encode(<bytes>) -> string`
- **CIDR.ip()** — extract network address from CIDR: `cidr('192.168.0.0/24').ip()`
- **IP/CIDR version helpers** — `isIPv4`, `isIPv6`, `isCIDRv4`, `isCIDRv6`
- **String reverse** — `<string>.reverse()`
- **List sort** — `<list>.sort()` returns sorted list
- **lists.range(n)** — generates integer sequence `[0, n)`
- **Flatten with depth** — `<list>.flatten(<depth>)` supports optional depth parameter
- **dns1035LabelPrefix** named format — `format.dns1035LabelPrefix()` (like dns1035Label but trailing hyphen and empty string allowed)
- `ErrorKind` enum — classifies errors as `CompilationFailure`, `InvalidRule`, `ValidationFailure`, `InvalidResult`, or `EvaluationError`
- `CompiledSchema::compilation_errors()` and `CompiledSchema::has_errors()` convenience methods
- Rule `fieldPath` is now applied to override auto-generated error paths
- Thread safety and key escaping documentation

### Fixed

- Rule `fieldPath` was parsed but unused — now correctly overrides the error path
- CEL context is now reused per validation run instead of re-created per rule (performance improvement)

### Known Limitations

| Feature | Reason |
|---------|--------|
| `cel.bind(var, init, expr)` | CEL compiler macro — requires `cel` crate support |
| `<list>.sortBy(var, expr)` | Lambda evaluation — requires `cel` crate support |
| TwoVarComprehensions | CEL compiler macro — K8s 1.33+ |
| Authz library | Requires API server connection — outside client library scope |

## [0.3.1] - 2026-03-03

### Added

- **Named format validation library** (`named_format` feature)
  - `format.dns1123Label()`, `format.dns1123Subdomain()`, `format.dns1035Label()` — DNS name validators
  - `format.dns1123LabelPrefix()`, `format.dns1123SubdomainPrefix()` — prefix validators (trailing hyphen allowed)
  - `format.qualifiedName()`, `format.labelValue()` — K8s label validators
  - `format.uri()`, `format.uuid()`, `format.byte()`, `format.date()`, `format.datetime()` — common format validators
  - `format.named(<string>)` — dynamic format lookup by name
  - `<Format>.validate(<string>) -> optional<list<string>>` — `optional.none()` if valid, `optional.of([...errors])` if invalid
  - K8s pattern: `!format.dns1123Label().validate(name).hasValue()`
- **JSONPatch key escaping** (`jsonpatch` feature)
  - `jsonpatch.escapeKey(<string>) -> string` — RFC 6901 escape (`~` → `~0`, `/` → `~1`)
- **Field name escaping for Kubernetes CEL**
  - `escaping::escape_field_name()` — escape CEL reserved words and special character field names
  - CEL reserved words (`namespace`, `in`, `return`, etc.) → `__keyword__`
  - Special characters (`_`, `.`, `-`, `/`) → per-character substitution (`__`, `__dot__`, `__dash__`, `__slash__`)
  - Matches K8s Go apiserver logic (`apiserver/schema/cel/model`)
  - Applied in `json_to_cel`, `json_to_cel_with_schema`, and `json_to_cel_with_compiled`

## [0.3.0] - 2026-02-26

### Added

- **Schema-aware `format: date-time` / `format: duration` support**
  - `values::SchemaFormat` enum — `DateTime`, `Duration`, `None`
  - `values::json_to_cel_with_schema()` — recursive conversion based on raw JSON schema
  - `values::json_to_cel_with_compiled()` — conversion based on `CompiledSchema` metadata
  - `values::parse_go_duration()` — parse Go-style durations (`"1h30m"`, `"-5s"`, etc.)
  - Added `compilation::CompiledSchema.format` field
  - Automatic schema-aware conversion applied in `validation` module
  - Graceful fallback to `Value::String` on parse failure
- Example: `timestamp_duration`
- `chrono` dependency (included in `validation` feature)

## [0.2.1] - 2026-02-25

### Fixed

- Gate validation examples with `required-features` (fixes `--no-default-features` build)

### Added

- Examples: `basic`, `validate_crd`, `compiled_schema`
- CHANGELOG.md
- Crate-level doc for `validation` feature

## [0.2.0] - 2026-02-25

### Added

- **CRD Validation Pipeline** (`validation` feature)
  - `values::json_to_cel()` — convert `serde_json::Value` to `cel::Value`
  - `compilation::compile_rule()` / `compile_schema_validations()` — compile `x-kubernetes-validations` CEL rules
  - `compilation::compile_schema()` / `CompiledSchema` — pre-compile entire schema trees for reuse
  - `validation::Validator` — walk schema trees, evaluate rules, collect errors
  - `validation::validate()` / `validate_compiled()` — convenience functions
  - `messageExpression` support with best-effort compilation and static fallback
  - `optionalOldSelf` support (transition rules evaluated on create with `oldSelf = null`)
  - Transition rule detection via `oldSelf` reference analysis
  - Schema tree walking: `properties`, `items`, `additionalProperties`
  - Field path tracking (e.g., `spec.containers[1]`)
  - kube-rs `kube-core::cel::Rule` JSON compatibility

## [0.1.1] - 2026-02-24

### Fixed

- Fix `cel-interpreter` references to `cel` crate after upstream rename

## [0.1.0] - 2026-02-24

### Added

- Kubernetes CEL extension functions: `strings`, `lists`, `sets`, `regex_funcs`, `urls`, `ip`, `semver_funcs`, `format`, `quantity`
- Unified type dispatch for shared function names (`indexOf`, `lastIndexOf`, `isGreaterThan`, `isLessThan`, `compareTo`)
- Feature flags for each function group (all enabled by default)
