# apiserver divergence

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
| A quantity `add` / `sub` / `compareTo` / `isGreaterThan` / `isLessThan` on a quantity held by a variable (a comprehension's, or reached through `dyn(..)`, `[q][0]`, ..) in a call's receiver or arguments next to a part that could fail or read a quantity, or in the second operand of a two-operand call whose first could fail (`[quantity('1')].all(q, sign(q) > q.compareTo(quantity('2')))`), reached or not; names are matched without types, so a semver's `compareTo` there too | evaluates: a call's receiver before its arguments, both operands even when the first fails, and its quantities share state (`add` / `sub` write into the receiver, the comparisons can turn it into an `inf.Dec`), which the order can show | `CompilationFailure` (`cel` evaluates arguments first and skips the second operand after a failure); not refused: an update of a new quantity (`quantity(s)`, `add` / `sub` on one), one next to parts that can neither fail nor read a quantity (a comprehension variable, a literal, `quantity(<literal>)`), one where both orders agree (`&&`, `\|\|`, `?:`, list literals, indexing) | fail-closed |
| A regular expression past the `regex` crate's limits once translated: nesting past about 250 levels (Go takes 1000), or a compiled size past 128 MiB, which a Unicode class repeated up to Go's 1000 stays under but four of them do not (`\pL{1000}` four times) | compiles and matches it (Go's `regexp`) | `CompilationFailure` for a pattern the apiserver compiles at registration (the constant pattern of `matches`, `find` or `findAll`, a literal or a conversion of one such as `dyn('..')`, and the literal first argument of `matches`); otherwise an `EvaluationError` when it runs, even where CEL absorbs the error (`… \|\| true`) | fail-closed |
| A regular expression that reads Unicode tables (a Unicode class such as `\pL` or `\p{Greek}`, or case folding of a character that has a case, not `(?i)[0-9]`) matched against a string holding a character Go's tables may read otherwise than the `regex` crate's: one Unicode 15.0 does not assign (`'\u1C89'.matches('\\pL')`, a letter since Unicode 16.0), or one of a few the two fold or class otherwise (`ΐ`, `ɤ`) | reads Go's Unicode 15.0.0 tables: U+1C89 is not a letter, `false` | `EvaluationError`, even where CEL absorbs the error (`… \|\| true`): the `regex` crate has Unicode 16.0.0's tables, which the translation cannot carry over (and on any string where `regex` resolves to other tables, which kube-cel checks once) | fail-closed |
| The standard `timestamp(string)` conversion with a zone offset of 24 hours or more (`+24:00`, `+23:60`, `-24:00`; Go takes up to `+24:60`) | parses it (Go's `time.Parse`) | `EvaluationError`, even where CEL absorbs the error (`… \|\| true`); a literal one compiles | fail-closed |
| A `format: date-time` value `strfmt.IsDateTime` takes and `strfmt.ParseDateTime` refuses (`t` / `z` in lowercase, an offset hour past 24 or minute past 60 such as `+99:99` or `+05:61`, a trailing `tXYZ`, `10:30:00x5Z`), or one with a zone offset of 24 hours or more (`+24:00`) | passes the object; binds an error (refused only by a rule reading the field), or the 24-hour offset | `EvaluationError` at the field, whatever the rules read: kube-cel cannot bind an error value (`==` on it would answer), nor hold a 24-hour offset | fail-closed |
| `double(string)` of a decimal with more than 800 significant digits, trailing zeros included, read two ways by Go (`'1' + 800 zeros + 'e-800'`) | Go's `strconv.ParseFloat` gives the Eisel-Lemire value (`1.0`) or, where that step gives up, a decimal of 800 digits that drops the magnitude of the rest (`0.1`) | `EvaluationError` where the two readings differ, even where CEL absorbs the error (`… \|\| true`); a literal one compiles | fail-closed |
| A URL component that is not UTF-8 once unescaped (`url('http://h%80x/').getHost()`) | returns the bytes as a string | `EvaluationError` (a CEL string here must be UTF-8), even where CEL absorbs the error (`… \|\| true`) | fail-closed |
| A bytes `+` evaluation reaches, where its outcome holds (`b'abc' + b'1' == b'abc1'`, `[b'abc'].all(x, [x + b'1', x + b'2'][0] == b'abc1') \|\| true`, a `format: byte` field whose decoded buffer has no spare capacity) | evaluates; but v1.33 to v1.37 (cel-go before v0.30) append into the left value's spare capacity, so a later `+` on the same value writes over an earlier result: `[b'abc'].all(x, [x + b'1', x + b'2'][0] == b'abc1')` is false there, as is `[b'1', b'2'].map(y, b'abc' + y)[0] == b'abc1'` (a literal keeps one array across a loop); v1.38 copies ([cel-go#1381](https://github.com/cel-expr/cel-go/pull/1381)) | `EvaluationError` for any bytes `+` evaluation reaches, even where CEL absorbs it: the capacity hangs on Go's allocator (a literal or `bytes(s)` rounds up to a size class, a `format: byte` field keeps the base64 decoder's buffer), which kube-cel does not model. A window refusal: it stays when the reference moves to v1.38 | fail-closed |
| An ordering comparison (`<`, `<=`, `>`, `>=`) with a NaN operand whose error CEL absorbs (`!(double('NaN') < 1.0) \|\| true`, `[double('NaN'), 0.5].exists(x, x < 1.0)`) | evaluates: v1.33 to v1.37 (cel-go before v0.30) fail the comparison ("NaN values cannot be ordered") and absorb the error; v1.38 answers false ([cel-go#1370](https://github.com/cel-expr/cel-go/pull/1370)), so `!(double('NaN') < 1.0)` passes there and fails before | `EvaluationError` for an evaluation that ordered a NaN, even where CEL absorbs it: kube-cel evaluates the comparison to false, as v1.38 does, and refuses the rule. A window refusal: it stays when the reference moves to v1.38. `==` / `!=` with a NaN and the list functions that order one (`sort()`, `sortBy`, `min()`, `max()`, `isSorted()`) agree with every version, and are left alone | fail-closed |
| An object reaching more than 64 levels of schema nesting | no nesting limit; evaluates rules at any depth | `SchemaTooDeep` error | fail-closed |
| Rule whose `messageExpression` does not compile (a syntax error, or a call refused above) | rejects the CRD at registration | `CompilationFailure` (the rule is rejected, not just its message) | fail-closed |
| The `messageExpression` of a failing rule that reaches a value kube-cel refuses in a rule even where CEL absorbs it (the rows above that say so, a bytes `+`, an ordering of a NaN) | uses the message it computes, falling back to the static `message` only where it fails: `static` for `double('NaN') < 1.0 ? 'lt' : 'ge'` and `quantity('2') == quantity('1').add(1) ? 'eq' : 'ne'`, but `v=ab` for `'v=' + string(b'a' + b'b')`, `v=abc2` for an aliased bytes `+`, `y` for `timestamp('2024-01-15T10:30:00+24:00').getFullYear() == 2024 ? 'y' : 'n'` | the static `message` (or the default one), as for a `messageExpression` that fails: kube-cel cannot tell what the apiserver computes there | message only: the rule fails on both |
| Functions the apiserver declares that kube-cel does not implement: `optional.unwrap(..)`, `<list>.unwrapOpt()`, the authz library (`authorizer.*` in a VAP), or a function whose kube-cel feature is disabled | evaluates (the authz library against the cluster) | compiles, and passes where evaluation does not reach the call; `UnsupportedReference` where it does (or reads a policy's `authorizer`, which kube-cel does not bind), even where CEL absorbs the error (`… \|\| true`) | fail-closed (the authz library is out of scope) |
| A list or map `cel` builds (a literal, a macro's result, a plain list field) on the left of `==` / `!=` against a set list, map list or object, or holding one that `in` searches (`['b', 'a'] != self.s`, `self.s.filter(x, true) != self.s`, `dyn({'y': 1}) != dyn(self.o)`, `['a', 'b'] in [self.s]`, `'x' in [dyn(self).u]` for a value under a typeless `x-kubernetes-preserve-unknown-fields` node); and `+` onto a plain list field (`type((self.l + dyn([1]))[0]) == int` for a number list) | compares by position or key (cel-go `baseList.Equal`, `baseMap.Equal`); `in` asks the searched value (`baseList.Contains`); `+` binds what it adds under the left list's items (a double there) | `cel`'s built-in list or map is unequal to one of another type, and its `in` asks the list's element: `!=` holds, `==` fails. kube-cel binds a plain list as `cel`'s own list, so `+` keeps the added value's own binding; it binds plain lists as schema-aware ones once `cel` carries the fix, which would otherwise break `[..] == self.l`. **Fixed by cel-rust#462; remove this row before 0.9.0** | fail-open |
| The timestamp getters without a time zone (`getFullYear()`, `getMonth()`, `getDayOfYear()`, `getDayOfMonth()`, `getDate()`, `getDayOfWeek()`, `getHours()`, `getMinutes()`) on a timestamp with a non-zero offset, from `timestamp(..)` or a `format: date-time` field (`timestamp('2024-01-01T02:30:00+05:45').getHours() == 2`) | reads them in UTC, as the CEL spec says (`getHours()` is `20` there) | `cel` reads them in the timestamp's own offset: such a rule passes. **Fixed by cel-rust#460; remove this row before 0.9.0** | fail-open |
| No type checker: type errors (`[1, 2] == [1.0, 2.0]`, `[1, 2.0].isSorted()`), arity errors (`'a'.lowerAscii(42)`), call-shape errors (`family(ip(..))`, a literal `format()` call with more arguments than clauses or an argument its clause cannot take, such as `'%s'.format([1, 2])`, the one-variable `[1].existsOne(x, x > 0)`, a macro only `cel` has (the apiserver has `existsOne(i, v, p)` alone, so it passes even when evaluated), and the `reverse()` / `.string()` rows above), a field the type does not have (`self.undeclared` on a schema without it), and a non-string `messageExpression`, where evaluation does not reach the error | rejects the CRD at registration (type-checks every expression) | passes; `cel` has no type checker yet ([cel-rust#350](https://github.com/cel-rust/cel-rust/issues/350)) | **fail-open** (exception class) |
| No cost model: a rule whose estimated cost exceeds the budget (`self.l.all(x, self.l.exists(y, x == y))` on a list with no `maxItems`), or whose evaluation exceeds the per-expression cost limit (`lists.range(1000000).size() > 0`, `lists.range(600000).map(x, x).size() > 0`) | rejects the CRD at registration ("estimated rule cost exceeds budget"), or the object at evaluation ("actual cost limit exceeded") | passes; `cel` has no cost model yet ([cel-rust#230](https://github.com/cel-rust/cel-rust/discussions/230)) | **fail-open** (exception class) |

## How the validation environment mirrors the apiserver

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
library) therefore compiles, and fails only when evaluation reaches it. The
same walk refuses a quantity update whose result depends on evaluation order
(see the table). As
the apiserver's base environment does, a literal argument of `duration(..)` or
`timestamp(..)` the conversion refuses fails to compile, reached or not
(`true || duration('1e3s') > duration('0s')`); `new_env()` puts cel-go's
conversions, printing and duration arithmetic in front of `cel`'s standard
ones (Go's `time.ParseDuration` and `time.Parse(time.RFC3339, ..)`, a duration
within int64 nanoseconds, `string(<duration>)` in float64 seconds,
`string(<timestamp>)` in its own offset), and evaluates `or` / `orValue` on
an optional lazily, as cel-go does. Regular expressions are RE2's: `matches`,
`find` and `findAll` parse a pattern with a port of Go's `regexp/syntax`,
refuse what Go refuses (`\u0041`, `(?x)`, `a**`, `a{1001}`, `[a--b]`,
`\p{Emoji}`, which the `regex` crate takes), and translate the rest into the
`regex` crate's syntax with Go's meaning (ASCII `\d`, `\w`, `\s`, `\b`;
literal `\<`, `\>` and `&&`, `--`, `~~`, `[` inside a class; `\Q..\E`,
octal escapes and a literal `{`, which the crate lacks); a pattern the
apiserver compiles at registration (see the table) fails to compile, reached
or not, as there. The apiserver also folds a type
conversion of a constant when it builds the program (cel-go's
`OptOptimize`), so one that fails (`int('x')`, `uint(-1)`, `string(b'\xff')`)
rejects the CRD at registration; kube-cel evaluates such a conversion when it
compiles the rule, and refuses it as `CompilationFailure`. Every
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
