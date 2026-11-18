//! Regular expressions as the apiserver has them: RE2's syntax and meaning
//! (Go's `regexp`), translated for the `regex` crate.
//!
//! `cel`'s `matches` and kube-cel's `find` / `findAll` compile with the
//! `regex` crate, whose syntax is not Go's: it takes `\u0041`, `(?x)`,
//! `a**`, `a{1001}`, `[a--b]` or `\p{Emoji}`, which Go refuses, and reads
//! `\d`, `\w`, `\s`, `\b`, `\<` or `[a&&b]` otherwise than Go. A pattern
//! is therefore parsed with a port of Go's parser (`re2`), refused as the
//! apiserver refuses it, and otherwise translated into the crate's syntax with
//! Go's meaning (Unicode tables aside: Go's are Unicode 15.0.0). One the crate
//! still cannot compile (past its nesting limit, or a compiled size past 128
//! MiB) is an error noted as unsupported, so the rule fails even where CEL
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

/// The compiled size the `regex` crate may reach: 128 MiB, so that a
/// Unicode class repeated as often as Go allows (`[\p{L}]{1,1000}`, about
/// 80 MiB) compiles. Go counts such a class once per copy and takes up to
/// about 3.3 million instructions, so it still takes more than this
/// (`\pL{1000}` four times).
const SIZE_LIMIT: usize = 128 << 20;

/// How a pattern compiled: the translated pattern compiled, Go's error, or
/// the `regex` crate's.
#[derive(Clone)]
enum Compiled {
    Regex(Regex),
    Go(String),
    Crate(String),
}

thread_local! {
    /// Patterns compiled on this thread: compiling a large Unicode class can
    /// take a tenth of a second, and a rule compiles its pattern on every
    /// evaluation (for every element of a list it walks).
    static COMPILED: std::cell::RefCell<std::collections::HashMap<String, Compiled>> =
        std::cell::RefCell::default();
}

/// How many patterns [`COMPILED`] keeps before it starts over: few, as one
/// can take up to [`SIZE_LIMIT`].
const CACHED: usize = 16;

fn compiled(pattern: &str) -> Compiled {
    if let Some(hit) = COMPILED.with(|c| c.borrow().get(pattern).cloned()) {
        return hit;
    }
    let result = match re2::translate(pattern) {
        Err(e) => Compiled::Go(e.to_string()),
        Ok(translated) => match regex::RegexBuilder::new(&translated)
            .size_limit(SIZE_LIMIT)
            .build()
        {
            Ok(re) => Compiled::Regex(re),
            Err(e) => Compiled::Crate(e.to_string()),
        },
    };
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHED {
            c.clear();
        }
        c.insert(pattern.to_owned(), result.clone());
    });
    result
}

/// Why `pattern` cannot be compiled where the apiserver compiles it when it
/// registers a rule: Go refuses it, or its translation is past the `regex`
/// crate's limits, which are below Go's (a nesting past about 250 levels,
/// where Go takes 1000; a compiled size past [`SIZE_LIMIT`]): refused too,
/// fail-closed.
pub(crate) fn pattern_error(pattern: &str) -> Option<String> {
    match compiled(pattern) {
        Compiled::Regex(_) => None,
        Compiled::Go(e) => Some(e),
        Compiled::Crate(e) => Some(format!("kube-cel cannot compile this regular expression: {e}")),
    }
}

/// Compiles `pattern` for `function` at evaluation: an error where Go's
/// `regexp.Compile` fails, and an error noted as unsupported where Go
/// compiles it and the `regex` crate cannot compile its translation.
pub(crate) fn compile(function: &str, pattern: &str) -> Result<Regex, ExecutionError> {
    match compiled(pattern) {
        Compiled::Regex(re) => Ok(re),
        Compiled::Go(e) => Err(ExecutionError::function_error(function, e)),
        Compiled::Crate(e) => {
            let error = || {
                ExecutionError::function_error(
                    function,
                    format!("kube-cel cannot compile this regular expression, which RE2 takes: {e}"),
                )
            };
            crate::functions::note_unsupported_error(error);
            Err(error())
        }
    }
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
        // Go's syntax the regex crate lacks is translated.
        holds("'a'.matches(dyn('\\\\Qa\\\\E')) && 'a{'.matches(dyn('^a{$'))");
        // Go's meaning: ASCII classes, literal `&&` and `[` in a class.
        holds(
            "!'٣'.matches('\\\\d') && !'é'.matches('^\\\\w$') && '&'.matches('[a&&b]') && '[]'.matches('^[a[b]]$')",
        );
    }
}
