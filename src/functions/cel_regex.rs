//! Regular expressions as the apiserver has them: RE2's syntax (Go's
//! `regexp`), checked before the `regex` crate compiles a pattern.
//!
//! `cel`'s `matches` and kube-cel's `find` / `findAll` compile with the
//! `regex` crate, whose syntax is not Go's: it takes `A`, `(?x)`,
//! `a**`, `a{1001}`, `[a--b]` or `\p{Emoji}`, which Go refuses, and refuses
//! `\Q..\E`, octal escapes or a literal `{`, which Go takes. A pattern is
//! therefore first checked with a port of Go's parser (`re2`), and refused as
//! the apiserver refuses it; one Go takes that the `regex` crate cannot
//! compile is an error noted as unsupported, so the rule fails even where CEL
//! absorbs it (fail-closed).
//!
//! [`declare`] adds macros that rewrite `s.matches(p)` and `matches(s, p)` to
//! `@kube.matches(s, p)`, which checks, compiles and matches; and that refuse
//! at compile time what the apiserver's `ValidateRegexLiterals` refuses: a
//! string literal first argument of a `matches` call that is no valid
//! pattern. That is the pattern of `s.matches(p)`, but in cel-go's AST the
//! first argument of the global `matches(s, p)` is `s`, so there `s` is
//! checked. The patterns the apiserver compiles when it builds the program
//! (constant patterns of `matches`, `find` and `findAll`) are checked by the
//! validation engine.

use cel::{
    DeclarationError, Env, ExecutionError,
    common::{
        ast::{CallExpr, Expr, IdedExpr, LiteralValue},
        decls::Signature,
        types::{CelBool, CelString, DYN_TYPE},
        value::CowVal,
    },
    parser::{Macro, MacroExprHelper, ParseError},
};
use regex::Regex;

use crate::functions::re2;

const MATCHES: &str = "@kube.matches";

/// Adds the `matches` macros and helper described in the [module docs](self).
pub(crate) fn declare(env: &mut Env) -> Result<(), DeclarationError> {
    env.add_overload(
        MATCHES,
        MATCHES,
        Signature::new(vec![DYN_TYPE, DYN_TYPE]),
        matches,
    )?;
    env.add_macro(Macro::receiver("matches", 1, |h, target, args| {
        check_literal(h, &args[0])?;
        let target = target.take().expect("a receiver macro has a target");
        let pattern = args.pop().expect("one argument");
        Ok(Some(call(h, vec![target, pattern])))
    }))?;
    for name in ["matches", ".matches"] {
        env.add_macro(Macro::global(name, 2, |h, _, args| {
            check_literal(h, &args[0])?;
            Ok(Some(call(h, std::mem::take(args))))
        }))?;
    }
    Ok(())
}

fn call(h: &mut MacroExprHelper<'_>, args: Vec<IdedExpr>) -> IdedExpr {
    h.next_expr(Expr::Call(CallExpr {
        target: None,
        func_name: MATCHES.to_owned(),
        args,
    }))
}

/// cel-go's `ValidateRegexLiterals`: a string literal that is no pattern
/// fails to compile.
fn check_literal(h: &MacroExprHelper<'_>, arg: &IdedExpr) -> Result<(), ParseError> {
    if let Expr::Literal(LiteralValue::String(s)) = &arg.expr
        && pattern_error(s.inner()).is_some()
    {
        return Err(h.new_error(arg.id, "invalid matches argument"));
    }
    Ok(())
}

/// Why `pattern` cannot be compiled where the apiserver compiles it when it
/// registers a rule: Go refuses it, or it is one Go takes that the `regex`
/// crate cannot compile (refused too, fail-closed; among them those past
/// Go's own size and nesting limits, which `re2` does not port).
pub(crate) fn pattern_error(pattern: &str) -> Option<String> {
    if let Err(e) = re2::check(pattern) {
        return Some(e.to_string());
    }
    Regex::new(pattern)
        .err()
        .map(|e| format!("kube-cel cannot compile this regular expression: {e}"))
}

/// Compiles `pattern` for `function` at evaluation: an error where Go's
/// `regexp.Compile` fails, and an error noted as unsupported where Go
/// compiles it and the `regex` crate cannot.
pub(crate) fn compile(function: &str, pattern: &str) -> Result<Regex, ExecutionError> {
    re2::check(pattern).map_err(|e| ExecutionError::function_error(function, e.to_string()))?;
    Regex::new(pattern).map_err(|e| {
        let error = || {
            ExecutionError::function_error(
                function,
                format!("kube-cel cannot compile this regular expression, which RE2 takes: {e}"),
            )
        };
        crate::functions::note_unsupported_error(error);
        error()
    })
}

/// `s.matches(p)` / `matches(s, p)`.
fn matches<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let n = args.len();
    let [s, p]: [CowVal<'b, 'v>; 2] = args
        .try_into()
        .map_err(|_| ExecutionError::invalid_argument_count(2, n))?;
    let (Some(s), Some(p)) = (s.downcast_ref::<CelString>(), p.downcast_ref::<CelString>()) else {
        return Err(ExecutionError::function_error(
            "matches",
            "no such overload: matches takes two strings",
        ));
    };
    let re = compile("matches", p.inner())?;
    Ok(CowVal::owned(CelBool::from(re.is_match(s.inner()))))
}

#[cfg(test)]
mod tests {
    use cel::Value;

    use crate::functions::eval;

    fn holds(expr: &str) {
        assert_eq!(eval(expr), Ok(Value::Bool(true)), "{expr}");
    }

    fn compiles(expr: &str) -> bool {
        crate::new_env().compile(expr).is_ok()
    }

    // Expected verdicts are Go's `regexp` (go1.26.2) through the apiserver's
    // environment (k8s.io/apiserver v0.36.1).

    #[test]
    fn matches_works_in_both_forms() {
        holds("'abc'.matches('^a.c$') && matches('abc', 'b') && !'abc'.matches('^b')");
        holds(".matches('abc', 'c$')");
    }

    /// cel-go's `ValidateRegexLiterals` checks the first argument of a
    /// `matches` call: the pattern of `s.matches(p)`, the string of
    /// `matches(s, p)`.
    #[test]
    fn literal_first_arguments_are_checked_when_compiling() {
        for refused in [
            "true || 'a'.matches('(')",
            "true || 'a'.matches('a**')",
            "true || 'a'.matches('\\\\u0041')",
            "true || 'a'.matches('(?x)a')",
            "true || 'a'.matches('a{1001}')",
            "true || 'a'.matches('[a--b]')",
            "true || 'a'.matches('\\\\p{Emoji}')",
            "true || 'a'.matches('\\\\p{Old_Italic}')",
            "matches('(', 'x') || true",
        ] {
            assert!(!compiles(refused), "{refused} must not compile");
        }
        // The pattern of the global form is checked when the program is built
        // (the validation engine does), or when it runs.
        assert!(compiles("matches('a', '(') || true"));
        assert!(compiles("'a'.matches('\\\\p{greek}') && 'a'.matches('\\\\pl')"));
    }

    #[test]
    fn dynamic_patterns_follow_go() {
        // Go refuses the pattern: an error, which CEL can absorb.
        holds("'a'.matches(dyn('a**')) || true");
        assert!(eval("'a'.matches(dyn('a**'))").is_err());
        assert!(eval("'a'.matches(dyn('\\\\u0041'))").is_err());
        // Go takes it, the regex crate does not: an error too.
        assert!(eval("'a'.matches(dyn('\\\\Qa\\\\E'))").is_err());
    }
}
