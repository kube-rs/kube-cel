//! What the apiserver evaluates when it registers a rule.
//!
//! The apiserver builds each rule's program when it registers the CRD or
//! policy, with cel-go's `OptOptimize` (k8s.io/apiserver's base environment),
//! and a failure there rejects the CRD ("program instantiation failed"),
//! reached or not:
//!
//! - a type conversion (`bool`, `bytes`, `double`, `duration`, `dyn`, `int`,
//!   `string`, `timestamp`, `type`, `uint`) of a constant is folded into a
//!   constant (`decOptimize`, `maybeOptimizeConstUnary`): `int('x')`,
//!   `uint(-1)` or `string(b'\xff')` fails then. A constant is a literal, a
//!   folded conversion, or a list or map literal of constants (cel-go also
//!   folds a reference to a type name, which this does not);
//! - the constant pattern of `matches` (`MatchesRegexOptimization`) and of
//!   `find` / `findAll` (the Kubernetes regex library's optimizations) is
//!   compiled.
//!
//! [`check`] does the same with kube-cel's own conversions, and checks a
//! pattern as `functions::cel_regex` does at evaluation. A conversion kube-cel
//! cannot do where cel-go can (an error it notes as unsupported) is left to
//! the evaluation. The string literal first argument of a `matches` call,
//! which cel-go's `ValidateRegexLiterals` checks, is refused by the macro
//! `new_env()` has.

use std::cell::OnceCell;

use cel::{
    Context, Value,
    common::ast::{EntryExpr, Expr, IdedExpr},
};

/// Why [`check`] refused an expression.
#[derive(Debug)]
pub(crate) enum FailsAtRegistration {
    /// A conversion of a constant fails.
    Conversion { function: String, reason: String },
    /// A constant pattern does not compile.
    Pattern { function: &'static str, reason: String },
}

impl std::fmt::Display for FailsAtRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailsAtRegistration::Conversion { function, reason } => write!(
                f,
                "the conversion {function}() of a constant fails where the apiserver folds it when \
                 it registers the rule: {reason}"
            ),
            FailsAtRegistration::Pattern { function, reason } => write!(
                f,
                "the constant pattern of {function}() cannot be compiled where the apiserver \
                 compiles it when it registers the rule: {reason}"
            ),
        }
    }
}

impl std::error::Error for FailsAtRegistration {}

/// The type conversions cel-go folds when their argument is a constant
/// (`overloads.IsTypeConversionFunction`).
const CONVERSIONS: &[&str] = &[
    "bool",
    "bytes",
    "double",
    "duration",
    "dyn",
    "int",
    "string",
    "timestamp",
    "type",
    "uint",
];

/// The helpers `new_env()`'s macros put inside a conversion: part of it.
const HELPERS: &[&str] = &[
    "@kube.durationInput",
    "@kube.timestampInput",
    "@kube.stringInput",
    "@kube.doubleInput",
    "@kube.uintInput",
];

/// Whether `call` is a conversion cel-go folds when its argument is a
/// constant.
fn is_conversion(call: &cel::common::ast::CallExpr) -> bool {
    call.target.is_none() && call.args.len() == 1 && CONVERSIONS.contains(&bare(&call.func_name))
}

fn bare(name: &str) -> &str {
    name.strip_prefix('.').unwrap_or(name)
}

/// Whether cel-go's planner folds `e` into a constant: a literal, a
/// conversion of one, or a list or map literal of them.
fn is_constant(e: &IdedExpr) -> bool {
    match &e.expr {
        Expr::Literal(_) => true,
        Expr::Call(call) => {
            (is_conversion(call)
                || (call.target.is_none()
                    && call.args.len() == 1
                    && HELPERS.contains(&call.func_name.as_str())))
                && is_constant(&call.args[0])
        }
        Expr::List(list) => list.optional_indices.is_empty() && list.elements.iter().all(is_constant),
        Expr::Map(map) => map.entries.iter().all(|entry| match &entry.expr {
            EntryExpr::MapEntry(entry) => {
                !entry.optional && is_constant(&entry.key) && is_constant(&entry.value)
            }
            EntryExpr::StructField(_) => false,
        }),
        _ => false,
    }
}

/// The pattern argument of a call the apiserver compiles a constant pattern
/// of, if `call` is one: `@kube.matches(s, p)` (what `new_env()` makes of
/// `matches`), `s.find(p)`, `s.findAll(p)` and `s.findAll(p, n)`.
fn pattern_of(call: &cel::common::ast::CallExpr) -> Option<(&'static str, &IdedExpr)> {
    match (call.func_name.as_str(), &call.target, call.args.as_slice()) {
        ("@kube.matches", None, [_, p]) => Some(("matches", p)),
        ("find", Some(_), [p]) => Some(("find", p)),
        ("findAll", Some(_), [p] | [p, _]) => Some(("findAll", p)),
        _ => None,
    }
}

/// Refuses `expr` if a conversion the apiserver folds at registration fails,
/// or a constant pattern it compiles then does not compile.
pub(crate) fn check(expr: &IdedExpr) -> Result<(), FailsAtRegistration> {
    let ctx = OnceCell::new();
    walk(expr, &ctx)
}

fn walk(e: &IdedExpr, ctx: &OnceCell<Context<'static, 'static>>) -> Result<(), FailsAtRegistration> {
    if let Expr::Call(call) = &e.expr
        && is_conversion(call)
        && is_constant(&call.args[0])
    {
        let ctx = ctx.get_or_init(super::base_context);
        if let (Err(error), None) = crate::functions::tracking_unsupported(|| Value::resolve(e, ctx)) {
            return Err(FailsAtRegistration::Conversion {
                function: bare(&call.func_name).to_owned(),
                reason: error.to_string(),
            });
        }
    }
    if let Expr::Call(call) = &e.expr
        && let Some((function, pattern)) = pattern_of(call)
        && is_constant(pattern)
    {
        let ctx = ctx.get_or_init(super::base_context);
        let (value, noted) = crate::functions::tracking_unsupported(|| Value::resolve(pattern, ctx));
        match value {
            Ok(Value::String(s)) => {
                if let Some(reason) = crate::functions::regex_pattern_error(&s) {
                    return Err(FailsAtRegistration::Pattern { function, reason });
                }
            }
            // Not a string: no overload, an error when it runs, as in cel-go.
            Ok(_) => {}
            // A conversion kube-cel cannot do where cel-go can: left to the
            // evaluation, which fails on it.
            Err(_) if noted.is_some() => {}
            Err(error) => {
                return Err(FailsAtRegistration::Pattern {
                    function,
                    reason: error.to_string(),
                });
            }
        }
    }
    for child in children(e) {
        walk(child, ctx)?;
    }
    Ok(())
}

fn children(e: &IdedExpr) -> Vec<&IdedExpr> {
    match &e.expr {
        Expr::Select(select) => vec![&select.operand],
        Expr::Call(call) => call
            .target
            .as_deref()
            .into_iter()
            .chain(call.args.iter())
            .collect(),
        Expr::Comprehension(c) => vec![&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result],
        Expr::List(list) => list.elements.iter().collect(),
        Expr::Map(map) => map.entries.iter().flat_map(|e| entry_children(&e.expr)).collect(),
        Expr::Struct(s) => s.entries.iter().flat_map(|e| entry_children(&e.expr)).collect(),
        Expr::Unspecified | Expr::Ident(_) | Expr::Literal(_) => Vec::new(),
    }
}

fn entry_children(entry: &EntryExpr) -> Vec<&IdedExpr> {
    match entry {
        EntryExpr::StructField(field) => vec![&field.value],
        EntryExpr::MapEntry(entry) => vec![&entry.key, &entry.value],
    }
}

#[cfg(test)]
mod tests {
    fn refused(expr: &str) -> bool {
        let program = crate::validation::validation_env().compile(expr).expect("parses");
        super::check(program.expression()).is_err()
    }

    // Expected verdicts are the reference apiserver's at registration
    // (`env.Program` with `OptOptimize`).

    #[test]
    fn refuses_a_conversion_of_a_constant_that_fails() {
        for expr in [
            "true || int('x') == 1",
            "true || uint(-1) == 1u",
            "true || string(b'\\xff') == ''",
            "true || int(dyn('x')) == 1",
            "true || [int(string(1.5))].size() == 0",
            "true || int(9.3e18) == 1",
            "true || .bool('maybe')",
            "true || 'a'.matches(string(b'\\xff'))",
            "true || double('1e400') > 0.0",
            "true || uint('+1') > 0u",
        ] {
            assert!(refused(expr), "{expr}");
        }
    }

    #[test]
    fn leaves_what_cel_go_does_not_fold() {
        for expr in [
            "int('1') == 1 && uint(1) == 1u && string(b'a') == 'a' && dyn([1, 2]) == [1, 2]",
            // Arithmetic and other calls are not folded.
            "true || 1 / 0 == 1",
            "true || int('x' + 'y') == 1",
            "true || int(size('x')) == 1",
            "[1].all(x, true || int(x) == 1)",
            // A conversion kube-cel cannot do where cel-go can is left to the
            // evaluation.
            "true || timestamp('2024-01-15T10:30:00+24:00') == timestamp(0)",
        ] {
            assert!(!refused(expr), "{expr}");
        }
    }
}
