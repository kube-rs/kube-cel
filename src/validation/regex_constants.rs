//! The regular expressions the apiserver compiles when it registers a rule.
//!
//! The apiserver builds each rule's program when it registers the CRD or
//! policy, with cel-go's `OptOptimize`: a type conversion of a constant
//! (`dyn('..')`, `string(b'..')`) is folded into a constant, and the constant
//! pattern of `matches` (`MatchesRegexOptimization`) and of `find` /
//! `findAll` (the Kubernetes regex library's optimizations) is compiled
//! then; a pattern that does not compile, or a conversion that fails, rejects
//! the CRD ("program instantiation failed"), reached or not. [`check`] does
//! the same for those patterns: it folds the conversions of a literal with
//! kube-cel's own conversions and checks the pattern as
//! `functions::cel_regex` does at evaluation.
//!
//! A pattern that is only checked when it runs is not refused here. The
//! string literal first argument of a `matches` call, which cel-go's
//! `ValidateRegexLiterals` checks, is refused by the macro `new_env()` has.

use std::cell::OnceCell;

use cel::{
    Context, Value,
    common::ast::{EntryExpr, Expr, IdedExpr},
};

/// Why [`check`] refused an expression.
#[derive(Debug)]
pub(crate) struct InvalidPattern {
    function: &'static str,
    reason: String,
}

impl std::fmt::Display for InvalidPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the constant pattern of {}() cannot be compiled where the apiserver compiles it when \
             it registers the rule: {}",
            self.function, self.reason
        )
    }
}

impl std::error::Error for InvalidPattern {}

/// The type conversions cel-go folds when their argument is a constant
/// (`overloads.IsTypeConversionFunction`), and the helpers `new_env()`'s
/// macros put inside them.
const FOLDED: &[&str] = &[
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
    "@kube.durationInput",
    "@kube.timestampInput",
    "@kube.stringInput",
];

fn bare(name: &str) -> &str {
    name.strip_prefix('.').unwrap_or(name)
}

/// Whether cel-go's planner folds `e` into a constant: a literal, or a
/// conversion of one.
fn is_constant(e: &IdedExpr) -> bool {
    match &e.expr {
        Expr::Literal(_) => true,
        Expr::Call(call) => {
            call.target.is_none()
                && call.args.len() == 1
                && FOLDED.contains(&bare(&call.func_name))
                && is_constant(&call.args[0])
        }
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

/// Refuses `expr` if a constant pattern the apiserver compiles at
/// registration fails to compile, or its folding fails.
pub(crate) fn check(expr: &IdedExpr) -> Result<(), InvalidPattern> {
    let ctx = OnceCell::new();
    walk(expr, &ctx)
}

fn walk(e: &IdedExpr, ctx: &OnceCell<Context<'static, 'static>>) -> Result<(), InvalidPattern> {
    if let Expr::Call(call) = &e.expr
        && let Some((function, pattern)) = pattern_of(call)
        && is_constant(pattern)
    {
        let ctx = ctx.get_or_init(super::base_context);
        let (value, noted) = crate::functions::tracking_unsupported(|| Value::resolve(pattern, ctx));
        match value {
            Ok(Value::String(s)) => {
                if let Some(reason) = crate::functions::regex_pattern_error(&s) {
                    return Err(InvalidPattern { function, reason });
                }
            }
            // Not a string: no overload, an error when it runs, as in cel-go.
            Ok(_) => {}
            // A conversion kube-cel cannot do where cel-go can: left to the
            // evaluation, which fails on it.
            Err(_) if noted.is_some() => {}
            Err(error) => {
                return Err(InvalidPattern {
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
