//! Compile-time check of the functions an expression calls.
//!
//! `cel` has no type checker yet (cel-rust#350): [`cel::Env::compile`] only
//! parses, and a call to a function the environment lacks fails only when it
//! is evaluated. A call evaluation never reaches (`true || isIPv4(s)`, the body
//! of a comprehension over an empty list, a `messageExpression` of a rule that
//! holds) would then pass, where the apiserver, which checks every call when
//! the CRD or policy is registered, rejects it. This walks the whole AST and
//! refuses a call to any function the apiserver does not declare, so such an
//! expression fails to compile client-side too.
//!
//! The names accepted are the ones the apiserver declares
//! (`apiserver_functions.txt`, from the `k8s.io/apiserver` base environment,
//! checked against a live apiserver by `tests/apiserver_parity.rs`), not the
//! ones kube-cel implements. CRD rules,
//! their `messageExpression`s and policy expressions share the list: the
//! apiserver declares the same functions for all three, the `authorizer`
//! library included (what a policy adds is the `authorizer` variable). A
//! function the apiserver declares but kube-cel does not implement
//! (`optional.unwrap`, `<list>.unwrapOpt()`, the `authorizer` library, a
//! function whose kube-cel feature is disabled) therefore compiles, as the
//! apiserver accepts it, and fails closed with `UnsupportedReference` only if
//! evaluation reaches the call.
//!
//! It checks names only. Type, arity and call-shape errors (`[1, 2.0].isSorted()`,
//! `'a'.lowerAscii(42)`, `family(ip(..))`) still need a type checker: see the
//! README "apiserver divergence" table. The one call shape it does refuse is a
//! global `min(..)`/`max(..)`: the apiserver only has `<list>.min()`/
//! `<list>.max()`, while `cel` evaluates any member function called as a
//! global, so `min([1, 2])` would run.

use std::{collections::HashSet, sync::LazyLock};

use cel::common::ast::{EntryExpr, Expr, IdedExpr};

/// Functions the apiserver has as `<list>.f()` but kube-cel also takes as
/// `f(..)`.
const MEMBER_ONLY: &[&str] = &["min", "max"];

/// The functions the apiserver declares, from `apiserver_functions.txt`.
fn apiserver_functions() -> &'static HashSet<&'static str> {
    static NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
        include_str!("apiserver_functions.txt")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    });
    &NAMES
}

/// Why [`check`] refused an expression.
#[derive(Debug)]
pub(crate) enum UnsupportedCall {
    /// A call to a function the apiserver does not declare, as written:
    /// `isIPv4`, `math.ceil`.
    Function(String),
    /// A global call to a function the apiserver only has as a member.
    GlobalForm(&'static str),
}

impl std::fmt::Display for UnsupportedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsupportedCall::Function(name) => {
                write!(
                    f,
                    "call to '{name}': no such function in the Kubernetes CEL environment"
                )
            }
            UnsupportedCall::GlobalForm(name) => write!(
                f,
                "no global '{name}' in the Kubernetes CEL environment; call it on a list: <list>.{name}()"
            ),
        }
    }
}

impl std::error::Error for UnsupportedCall {}

/// Checks every call in `expr` against the functions the apiserver declares.
pub(crate) fn check(expr: &IdedExpr) -> Result<(), UnsupportedCall> {
    Checker {
        declared: apiserver_functions(),
    }
    .walk(expr)
}

struct Checker {
    declared: &'static HashSet<&'static str>,
}

impl Checker {
    /// Whether the apiserver declares `name`. A leading `.` makes a name
    /// absolute (`.size(..)`, `.optional.of(..)`); `cel` drops it to resolve.
    fn known(&self, name: &str) -> bool {
        self.declared.contains(name.strip_prefix('.').unwrap_or(name))
    }

    fn walk(&self, e: &IdedExpr) -> Result<(), UnsupportedCall> {
        match &e.expr {
            Expr::Unspecified | Expr::Ident(_) | Expr::Literal(_) => Ok(()),
            Expr::Select(select) => self.walk(&select.operand),
            Expr::List(list) => list.elements.iter().try_for_each(|e| self.walk(e)),
            Expr::Map(map) => map.entries.iter().try_for_each(|e| self.walk_entry(&e.expr)),
            Expr::Struct(s) => s.entries.iter().try_for_each(|e| self.walk_entry(&e.expr)),
            Expr::Comprehension(c) => [&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result]
                .into_iter()
                .try_for_each(|e| self.walk(e)),
            Expr::Call(call) => {
                let name = call.func_name.as_str();
                // Operators (`_+_`, `@in`, ..) and the helpers macros expand to
                // (`cel.@mapInsert`, ..) are spelled so no source can call them.
                if is_identifier_path(name) {
                    match &call.target {
                        None => {
                            let bare = name.strip_prefix('.').unwrap_or(name);
                            if let Some(member_only) = MEMBER_ONLY.iter().find(|n| **n == bare) {
                                return Err(UnsupportedCall::GlobalForm(member_only));
                            }
                            if !self.known(name) {
                                return Err(UnsupportedCall::Function(name.to_owned()));
                            }
                        }
                        // As `cel` resolves it: `a.b.f(..)` calls the function
                        // `a.b.f` when one is declared, else `f` on `a.b`.
                        Some(target) => {
                            let qualified = qualified_name(target).map(|q| format!("{q}.{name}"));
                            match qualified {
                                Some(qualified) if self.known(&qualified) => {}
                                qualified if !self.known(name) => {
                                    return Err(UnsupportedCall::Function(
                                        qualified.unwrap_or_else(|| name.to_owned()),
                                    ));
                                }
                                _ => self.walk(target)?,
                            }
                        }
                    }
                } else if let Some(target) = &call.target {
                    self.walk(target)?;
                }
                call.args.iter().try_for_each(|e| self.walk(e))
            }
        }
    }

    fn walk_entry(&self, entry: &EntryExpr) -> Result<(), UnsupportedCall> {
        match entry {
            EntryExpr::StructField(field) => self.walk(&field.value),
            EntryExpr::MapEntry(entry) => {
                self.walk(&entry.key)?;
                self.walk(&entry.value)
            }
        }
    }
}

/// Whether `name` could be written in CEL source as a (qualified) function name.
fn is_identifier_path(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// The qualified name `expr` spells (`a.b` for `a.b`), if it is an identifier
/// or field selections on one.
fn qualified_name(expr: &IdedExpr) -> Option<String> {
    match &expr.expr {
        Expr::Ident(name) => Some(name.clone()),
        Expr::Select(select) if !select.test => {
            qualified_name(&select.operand).map(|q| format!("{q}.{}", select.field))
        }
        _ => None,
    }
}

/// Whether `expr` references an identifier `name` anywhere, a comprehension
/// variable of that name included: the apiserver's `UsesOldSelf` scans the
/// checker's reference map, which records every resolved identifier, so
/// `[1].all(oldSelf, oldSelf > 0)` "uses oldSelf" there.
pub(crate) fn references_ident(expr: &IdedExpr, name: &str) -> bool {
    match &expr.expr {
        Expr::Ident(ident) => ident.strip_prefix('.').unwrap_or(ident) == name,
        Expr::Unspecified | Expr::Literal(_) => false,
        Expr::Select(select) => references_ident(&select.operand, name),
        Expr::List(list) => list.elements.iter().any(|e| references_ident(e, name)),
        Expr::Map(map) => map.entries.iter().any(|e| entry_references_ident(&e.expr, name)),
        Expr::Struct(s) => s.entries.iter().any(|e| entry_references_ident(&e.expr, name)),
        Expr::Comprehension(c) => [&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result]
            .into_iter()
            .any(|e| references_ident(e, name)),
        Expr::Call(call) => {
            call.target.as_deref().is_some_and(|t| references_ident(t, name))
                || call.args.iter().any(|e| references_ident(e, name))
        }
    }
}

fn entry_references_ident(entry: &EntryExpr, name: &str) -> bool {
    match entry {
        EntryExpr::StructField(field) => references_ident(&field.value, name),
        EntryExpr::MapEntry(entry) => {
            references_ident(&entry.key, name) || references_ident(&entry.value, name)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use super::apiserver_functions;
    use crate::{
        functions::register_all,
        validation::{compile, validation_env},
    };

    /// Functions the apiserver declares that kube-cel does not implement even
    /// with every feature on. A call to one compiles and fails closed with
    /// `UnsupportedReference` if evaluation reaches it.
    const DECLARED_ONLY: &[&str] = &[
        // cel-go's optional library, version 1.
        "optional.unwrap",
        "unwrapOpt",
        // The authorizer library (`authorizer` and `authorizer.requestResource`
        // are policy variables kube-cel does not bind).
        "allowed",
        "check",
        "error",
        "errored",
        "fieldSelector",
        "group",
        "labelSelector",
        "name",
        "namespace",
        "path",
        "reason",
        "resource",
        "serviceAccount",
        "subresource",
    ];

    /// Whether kube-cel can call `name` in a rule: `cel` declares it on the
    /// validation env, or kube-cel registers it on a context.
    fn implemented(name: &str, registered: &BTreeSet<String>) -> bool {
        validation_env().function(name).is_some() || registered.contains(name)
    }

    fn registered() -> BTreeSet<String> {
        let mut ctx = cel::Context::with_env(Arc::clone(validation_env()));
        register_all(&mut ctx)
            .expect("kube-cel's functions register onto its Env")
            .into_iter()
            .collect()
    }

    /// Every function kube-cel registers is one the apiserver declares:
    /// kube-cel ships no function Kubernetes lacks, and the name check would
    /// refuse a call to one in a rule. General use and validation share the
    /// set, so this covers both.
    #[test]
    fn registered_functions_are_declared_by_the_apiserver() {
        let stray: Vec<String> = registered()
            .into_iter()
            .filter(|name| !apiserver_functions().contains(name.as_str()))
            .collect();
        assert!(
            stray.is_empty(),
            "kube-cel registers functions the apiserver does not declare: {stray:?}"
        );
    }

    /// Declared = implemented ∪ declared-only, with the two disjoint. Fails
    /// when an updated `apiserver_functions.txt` adds a name kube-cel does not
    /// implement, or when kube-cel starts implementing one listed in
    /// [`DECLARED_ONLY`].
    #[test]
    #[cfg(all(
        feature = "strings",
        feature = "lists",
        feature = "sets",
        feature = "regex_funcs",
        feature = "urls",
        feature = "ip",
        feature = "semver_funcs",
        feature = "format",
        feature = "quantity",
        feature = "named_format"
    ))]
    fn declared_functions_are_implemented_or_declared_only() {
        let registered = registered();
        let missing: BTreeSet<&str> = apiserver_functions()
            .iter()
            .copied()
            .filter(|name| !implemented(name, &registered))
            .collect();
        let listed: BTreeSet<&str> = DECLARED_ONLY.iter().copied().collect();
        assert_eq!(
            missing, listed,
            "declared by the apiserver but not implemented, vs DECLARED_ONLY"
        );
    }

    /// Calls to functions the apiserver declares but kube-cel does not
    /// implement compile. Each was compiled and evaluated with the Go oracle
    /// (`k8s.io/apiserver` v0.36.1); the `authorizer` rows in the policy
    /// compiler, the others in the CRD rule compiler.
    #[test]
    fn accepts_declared_functions_kube_cel_does_not_implement() {
        assert_accepted(&[
            "self == '' || optional.unwrap([optional.of(self), optional.none()]) == [self]",
            "true || [optional.of(1), optional.none()].unwrapOpt() == [1]",
            ".optional.unwrap([optional.of(1)]) == [1]",
            "true || authorizer.group('').resource('pods').check('get').allowed()",
            "authorizer.serviceAccount('default', 'sa').group('').resource('pods').subresource('log').namespace('default').name('p').check('get').allowed()",
            "authorizer.group('').resource('pods').fieldSelector('metadata.name=p').labelSelector('a=b').check('list').allowed()",
            "authorizer.path('/healthz').check('get').allowed() && authorizer.requestResource.check('get').allowed()",
            "!authorizer.group('').resource('pods').check('get').errored() && authorizer.group('').resource('pods').check('get').error() == ''",
            "authorizer.group('').resource('pods').check('get').reason() == ''",
        ]);
    }

    /// The check must refuse none of `exprs`, all of which the apiserver's base
    /// environment accepts: each was evaluated to `true` with the Go oracle
    /// (`k8s.io/apiserver` v0.36.1, `MustBaseEnvSet(1.36).NewExpressionsEnv()`).
    fn assert_accepted(exprs: &[&str]) {
        for expr in exprs {
            if let Err(err) = compile(expr) {
                panic!("{expr}: refused: {err}");
            }
        }
    }

    #[test]
    fn accepts_the_standard_environment() {
        assert_accepted(&[
            "size('abc') == 3 && 'abc'.size() == 3",
            "int('1') + int(uint(2)) == 3 && double(1) == 1.0 && uint(1) == 1u",
            "string(1) == '1' && bytes('a') == b'a' && bool('true')",
            "type(1) == int && dyn(1) == 1",
            "'abc'.contains('b') && 'abc'.startsWith('a') && 'abc'.endsWith('c') && 'abc'.matches('^a')",
            "duration('1h') > duration('1m') && timestamp('2024-01-01T00:00:00Z').getFullYear() == 2024",
            "timestamp('2024-01-01T00:00:00Z').getHours() == 0 && duration('1h').getMinutes() == 60",
            "has({'a': 1}.a) && [1, 2].all(x, x > 0) && [1, 2].exists(x, x == 2) && [1, 2].exists_one(x, x == 2)",
            "[1, 2].map(x, x * 2) == [2, 4] && [1, 2].filter(x, x > 1) == [2] && [1, 2].map(x, x > 0, x) == [1, 2]",
            "optional.of(1).hasValue() && optional.none().orValue(2) == 2 && optional.of(1).value() == 1",
            "optional.ofNonZeroValue(0).or(optional.of(1)).value() == 1 && optional.of(1).optMap(x, x + 1).value() == 2",
            "{'a': 1}.?a.orValue(0) == 1 && [1, 2][?0].value() == 1",
            "[1, 2].all(i, v, v > i) && [1, 2].exists(i, v, v == 2) && [1, 2].transformList(i, v, v * 2) == [2, 4]",
            "{'a': 1}.transformMap(k, v, v + 1) == {'a': 2} && [1, 2].transformMapEntry(i, v, {string(i): v}) == {'0': 1, '1': 2}",
            "[1, 2].existsOne(i, v, v == 2) && [1, 2].transformList(i, v, v > 1, v) == [2]",
            "1 in [1, 2] && 'a' in {'a': 1} && !(1 > 2) && -1 < 0 && 1 % 2 == 1 && 1 / 1 == 1 && (true ? 1 : 2) == 1",
            ".size('a') == 1 && .optional.of(1).value() == 1",
        ]);
    }

    #[test]
    #[cfg(feature = "lists")]
    fn accepts_the_lists_extension() {
        assert_accepted(&[
            "[3, 1, 2].sortBy(x, x) == [1, 2, 3] && lists.range(3) == [0, 1, 2] && [1, 2, 3].slice(0, 2) == [1, 2]",
            "[[1], [2]].flatten() == [1, 2] && [1, 1].distinct() == [1] && [2, 1].sort() == [1, 2] && [1, 2].reverse() == [2, 1]",
            "[1, 2].isSorted() && [1, 2].sum() == 3 && [1, 2].min() == 1 && [1, 2].max() == 2 && [1, 2].indexOf(2) == 1 && [1, 2].lastIndexOf(2) == 1",
            ".lists.range(3) == [0, 1, 2]",
        ]);
    }

    #[test]
    #[cfg(all(
        feature = "strings",
        feature = "format",
        feature = "regex_funcs",
        feature = "urls",
        feature = "ip",
        feature = "quantity",
        feature = "semver_funcs",
        feature = "sets",
        feature = "named_format"
    ))]
    fn accepts_the_kubernetes_libraries() {
        assert_accepted(&[
            "'%s-%d'.format(['a', 1]) == 'a-1' && 'a,b'.split(',') == ['a', 'b'] && ['a', 'b'].join('-') == 'a-b'",
            "'abc'.charAt(1) == 'b' && 'abc'.indexOf('b') == 1 && 'abcb'.lastIndexOf('b') == 3 && 'ABC'.lowerAscii() == 'abc'",
            "'abc'.upperAscii() == 'ABC' && 'aXc'.replace('X', 'b') == 'abc' && 'abc'.substring(1) == 'bc' && ' a '.trim() == 'a'",
            "strings.quote('a') == '\"a\"'",
            "'abc'.find('b') == 'b' && 'abab'.findAll('b') == ['b', 'b'] && 'abab'.findAll('b', 1) == ['b']",
            "url('https://example.com/p?q=1').getHost() == 'example.com' && isURL('https://example.com') && url('https://a.com/b').getEscapedPath() == '/b'",
            "url('https://a.com:8080').getPort() == '8080' && url('https://a.com').getScheme() == 'https' && url('https://a.com/?x=1').getQuery() == {'x': ['1']}",
            "url('https://a.com').getHostname() == 'a.com'",
            "isIP('1.2.3.4') && ip('1.2.3.4').family() == 4 && ip('1.2.3.4').isLoopback() == false && ip.isCanonical('1.2.3.4')",
            "ip('10.0.0.1').isGlobalUnicast() && !ip('1.2.3.4').isUnspecified() && !ip('1.2.3.4').isLinkLocalMulticast() && !ip('1.2.3.4').isLinkLocalUnicast()",
            "isCIDR('10.0.0.0/8') && cidr('10.0.0.0/8').containsIP(ip('10.0.0.1')) && cidr('10.0.0.0/8').containsCIDR(cidr('10.1.0.0/16'))",
            "cidr('10.0.0.0/8').prefixLength() == 8 && cidr('10.0.0.1/8').masked() == cidr('10.0.0.0/8') && cidr('10.0.0.0/8').ip() == ip('10.0.0.0')",
            "string(ip('1.2.3.4')) == '1.2.3.4' && string(cidr('10.0.0.0/8')) == '10.0.0.0/8'",
            "isQuantity('1Gi') && quantity('1Gi').isGreaterThan(quantity('1Mi')) && quantity('1').isLessThan(quantity('2')) && quantity('1').compareTo(quantity('1')) == 0",
            "quantity('1').add(quantity('1')).compareTo(quantity('2')) == 0 && quantity('2').sub(1).compareTo(quantity('1')) == 0 && sign(quantity('1')) == 1 && quantity('1').isInteger()",
            "quantity('1').asInteger() == 1 && quantity('1.5').asApproximateFloat() == 1.5",
            "isSemver('1.0.0') && semver('1.2.3').major() == 1 && semver('1.2.3').minor() == 2 && semver('1.2.3').patch() == 3",
            "semver('1.0.0').isGreaterThan(semver('0.9.0')) && semver('1.0.0').isLessThan(semver('2.0.0')) && semver('1.0.0').compareTo(semver('1.0.0')) == 0",
            "sets.contains([1, 2], [1]) && sets.equivalent([1, 2], [2, 1]) && sets.intersects([1], [1, 2])",
            ".sets.equivalent([1], [1])",
            "!format.dns1123Label().validate('abc').hasValue() && !format.named('dns1123Label').value().validate('abc').hasValue()",
            "!format.dns1123Subdomain().validate('a.b').hasValue() && !format.labelValue().validate('a').hasValue() && !format.qualifiedName().validate('a').hasValue()",
            "!format.uuid().validate('123e4567-e89b-12d3-a456-426614174000').hasValue() && !format.date().validate('2024-01-01').hasValue()",
            "!format.byte().validate('aGk=').hasValue() && !format.uri().validate('https://a.com').hasValue() && !format.datetime().validate('2024-01-01T00:00:00Z').hasValue()",
            "!format.dns1035Label().validate('abc').hasValue() && !format.dns1123LabelPrefix().validate('abc').hasValue() && !format.dns1035LabelPrefix().validate('abc').hasValue() && !format.dns1123SubdomainPrefix().validate('abc').hasValue()",
        ]);
    }
}
