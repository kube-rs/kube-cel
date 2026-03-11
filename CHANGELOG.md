# Changelog

## [1.0.0] - 2026-03-11

### Changed

- **Repository transferred to [kube-rs](https://github.com/kube-rs) organization**
- Version bump to 1.0.0 — API stabilized (no breaking changes from 0.4.3)
- `rust-version = "1.85"` now explicitly specified
- `homepage = "https://kube.rs"` added
- README rewritten with proper documentation, usage examples, and feature flag reference
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
