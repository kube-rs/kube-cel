# Changelog

## [Unreleased]

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
