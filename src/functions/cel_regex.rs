//! Regular expressions as the apiserver has them: RE2's syntax and meaning
//! (Go's `regexp`), translated for the `regex` crate.
//!
//! `cel`'s `matches` and kube-cel's `find` / `findAll` compile with the
//! `regex` crate, whose syntax is not Go's: it takes `\u0041`, `(?x)`,
//! `a**`, `a{1001}`, `[a--b]` or `\p{Emoji}`, which Go refuses, and reads
//! `\d`, `\w`, `\s`, `\b`, `\<` or `[a&&b]` otherwise than Go. A pattern
//! is therefore parsed with a port of Go's parser (`re2`), refused as the
//! apiserver refuses it, and otherwise translated into the crate's syntax with
//! Go's meaning. One the crate still cannot compile (past its nesting limit,
//! or a compiled size past 128 MiB) is an error noted as unsupported, so the
//! rule fails even where CEL absorbs it (fail-closed). So is a match of a
//! pattern that reads Unicode tables (a Unicode class, or case folding of a
//! character that has a case) on a string holding a character Go's tables
//! (Unicode 15.0.0) may read otherwise than the crate's (`go_unicode`).
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

/// The compiled sizes a translation is tried at, smallest first: what a
/// compiled pattern takes is known only as the size it compiles under.
const TIERS: [usize; 4] = [256 << 10, 4 << 20, 32 << 20, SIZE_LIMIT];

/// What the patterns [`CACHE`] keeps may take together, counting each as
/// twice the size it compiled under (a compiled pattern takes up to about
/// 1.4 times that). A pattern past half of it (one that needs more than 32
/// MiB, `[\p{L}]{1,783}` and up) is not kept, and is compiled again where it
/// is used, in about 0.2 to 0.4 seconds as measured (the sizes it is tried at
/// included).
const CACHE_BUDGET: usize = 128 << 20;

/// The least an entry of [`CACHE`] counts for, whatever it holds: so many
/// patterns that fail cheaply to compile cannot pile up (at most 2,048
/// entries).
const MIN_ENTRY: usize = 64 << 10;

/// The largest size a pattern [`CACHE`] keeps may compile under: half of
/// [`CACHE_BUDGET`] once counted twice.
const LARGEST_KEPT: usize = CACHE_BUDGET / 4;

/// How a pattern compiled: the translated pattern compiled, and whether it
/// reads Unicode tables; Go's error; or the `regex` crate's.
#[derive(Clone)]
enum Compiled {
    Regex(std::sync::Arc<Regex>, re2::Tables),
    Go(String),
    Crate(String),
}

/// Compiled patterns, shared by every thread of the process, up to
/// [`CACHE_BUDGET`]: compiling a large Unicode class can take a tenth of a
/// second or more, and a rule compiles its pattern on every evaluation (for
/// every element of a list it walks). They are looked up under a read lock
/// and handed out behind an `Arc`: cloning a `Regex` would give it a new pool
/// of search caches, to fill again on every evaluation.
#[derive(Default)]
struct Cache {
    patterns: std::collections::HashMap<String, (Compiled, usize)>,
    used: usize,
}

static CACHE: std::sync::LazyLock<std::sync::RwLock<Cache>> = std::sync::LazyLock::new(Default::default);

fn cache() -> std::sync::RwLockReadGuard<'static, Cache> {
    CACHE.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn compiled(pattern: &str) -> Compiled {
    if let Some((hit, _)) = cache().patterns.get(pattern) {
        return hit.clone();
    }
    let (result, cost) = compile_uncached(pattern);
    if let Some(cost) = cost {
        let mut cache = CACHE.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Full: start over, dropping hot patterns too, which then compile
        // again; a bound on memory bought with that, and with no bookkeeping
        // of which pattern was used last.
        if cache.used + cost > CACHE_BUDGET {
            cache.patterns.clear();
            cache.used = 0;
        }
        if let Some((_, replaced)) = cache.patterns.insert(pattern.to_owned(), (result.clone(), cost)) {
            cache.used -= replaced;
        }
        cache.used += cost;
    }
    result
}

/// Compiles `pattern`, and what keeping it in [`CACHE`] counts for: twice
/// the size it compiled under, the pattern itself, and at least
/// [`MIN_ENTRY`]; `None` where it is not kept. Go's error is not, as the
/// translation finds it again as fast as it can be looked up, nor a pattern
/// compiled past [`LARGEST_KEPT`]; the `regex` crate's error is (refusing a
/// size past [`SIZE_LIMIT`] takes as long as compiling it).
fn compile_uncached(pattern: &str) -> (Compiled, Option<usize>) {
    let re2::Translation {
        pattern: translated,
        tables,
    } = match re2::translate(pattern) {
        Ok(translation) => translation,
        Err(e) => return (Compiled::Go(e.to_string()), None),
    };
    let cost =
        |size: usize| Some((2 * size + pattern.len()).max(MIN_ENTRY)).filter(|&cost| cost <= CACHE_BUDGET);
    let mut error = None;
    for limit in TIERS {
        match regex::RegexBuilder::new(&translated).size_limit(limit).build() {
            Ok(re) => {
                let cost = cost(limit).filter(|_| limit <= LARGEST_KEPT);
                return (Compiled::Regex(std::sync::Arc::new(re), tables), cost);
            }
            Err(e @ regex::Error::CompiledTooBig(_)) => error = Some(e),
            Err(e) => {
                let e = e.to_string();
                let size = e.len();
                return (Compiled::Crate(e), cost(size));
            }
        }
    }
    let error = error.map_or_else(String::new, |e| e.to_string());
    let size = error.len();
    (Compiled::Crate(error), cost(size))
}

/// Why `pattern` cannot be compiled where the apiserver compiles it when it
/// registers a rule: Go refuses it, or its translation is past the `regex`
/// crate's limits, which are below Go's (a nesting past about 250 levels,
/// where Go takes 1000; a compiled size past [`SIZE_LIMIT`]): refused too,
/// fail-closed.
pub(crate) fn pattern_error(pattern: &str) -> Option<String> {
    match compiled(pattern) {
        Compiled::Regex(..) => None,
        Compiled::Go(e) => Some(e),
        Compiled::Crate(e) => Some(format!("kube-cel cannot compile this regular expression: {e}")),
    }
}

/// Compiles `pattern` for `function` to match `input` at evaluation: an
/// error where Go's `regexp.Compile` fails, and an error noted as unsupported
/// where Go compiles it and the `regex` crate cannot compile its translation,
/// or where the pattern reads Unicode tables and `input` holds a character
/// Go's tables may read otherwise (any, if the crate's tables are not the
/// ones kube-cel knows).
pub(crate) fn compile(
    function: &str,
    pattern: &str,
    input: &str,
) -> Result<std::sync::Arc<Regex>, ExecutionError> {
    match compiled(pattern) {
        Compiled::Regex(re, tables) => {
            let reason = if tables == re2::Tables::None {
                None
            } else if !crate::functions::go_unicode::known_tables() {
                Some(
                    "the regex crate's Unicode tables are not the 16.0.0 ones kube-cel knows how Go's \
                     differ from, and this regular expression reads them",
                )
            } else if tables == re2::Tables::Read && input.chars().any(crate::functions::go_unicode::differs)
            {
                Some(
                    "the string holds a character Go's Unicode 15.0 tables may read otherwise than \
                     kube-cel's in this regular expression",
                )
            } else {
                None
            };
            if let Some(reason) = reason {
                let error = || ExecutionError::function_error(function, reason);
                crate::functions::note_unsupported_error(error);
                return Err(error());
            }
            Ok(re)
        }
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
    let re = compile("matches", p.inner(), s.inner())?;
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

    // Expected verdicts are Go's `regexp` (go1.26.2) through the reference
    // apiserver's environment.

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

    /// The cache keeps a pattern compiled under 32 MiB and the `regex`
    /// crate's errors, and not Go's errors, which are as fast to find again.
    #[test]
    fn the_cache_keeps_what_is_slow_to_compile_again() {
        let kept = |pattern: &str| super::compile_uncached(pattern).1.is_some();
        let deep = format!("{}a{}", "(".repeat(260), ")".repeat(260));
        assert!(kept("^[\\p{L}\\p{N}_-]{1,253}$") && kept("a") && kept(&deep));
        assert!(!kept("a**"));
    }

    /// A pattern that reads Unicode tables fails on a character Go's tables
    /// may read otherwise, as unsupported; any other matches.
    #[test]
    fn unicode_tables_fail_on_characters_go_reads_otherwise() {
        let fails = |expr: &str| {
            let (result, unsupported) = crate::functions::tracking_unsupported(|| eval(expr));
            assert!(result.is_err() && unsupported.is_some(), "{expr}");
        };
        fails("'\\u1C89'.matches('\\\\pL')");
        fails("'\\u1FD3'.matches('(?i)\\u0390')");
        holds("'\\u1C89'.matches('^.$') && 'é'.matches('^\\\\pL$') && 'Ω'.matches('(?i)ω')");
        holds("!'\\u1C89'.matches('(?i)\\\\d') && '\\u1C89'.matches('(?i)[^0-9]')");
        #[cfg(feature = "regex_funcs")]
        {
            fails("'a\\u0378'.findAll('(?i)a').size() == 1");
            holds("'a\\u0378'.findAll('a').size() == 1");
        }
    }
}
