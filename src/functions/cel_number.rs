//! cel-go's conversions of a string to a number, where `cel`'s differ.
//!
//! cel-go converts with Go's `strconv`: `double(s)` is `ParseFloat(s, 64)`,
//! `uint(s)` is `ParseUint(s, 10, 64)` (and `int(s)` is `ParseInt(s, 10,
//! 64)`, which `cel`'s `i64` parse matches). `cel` 0.16 parses with Rust's
//! `f64` and `u64` parsers instead, which give `inf` for a value past the
//! range where Go fails, refuse Go's `_` digit separators and hexadecimal
//! floats (`0x1p-2`), take `+nan`, and take a `+` before an unsigned integer.
//!
//! As for the time conversions (see `cel_time`), [`declare`] adds macros that
//! rewrite `double(e)` to `double(@kube.doubleInput(e))` and `uint(e)` to
//! `uint(@kube.uintInput(e))`, the leading-dot forms included: a string goes
//! through Go's parser, any other value passes through to `cel`'s overload.
//!
//! It also rewrites the operands of `<`, `<=`, `>` and `>=`, a literal aside,
//! to `@kube.ordered(e)`, which passes the value through and notes a NaN as
//! unsupported. cel-go before v0.30 (the apiserver before 1.38) fails to order
//! a NaN against a number ("NaN values cannot be ordered"), where `cel` and
//! cel-go v0.30 answer false: `!(double('NaN') < 1.0)` holds in kube-cel and
//! fails on the apiserver. The validation engine refuses an evaluation that
//! ordered a NaN, even where CEL absorbs the comparison (`… || true`, which
//! passes there). `==` and `!=` with a NaN agree, and are left alone, and
//! `sort()`, `sortBy`, `min()`, `max()` and `isSorted()` order a NaN as the
//! apiserver does (see `lists`, `lists_ext`).

use cel::{
    DeclarationError, Env, ExecutionError,
    common::{
        ast::{CallExpr, Expr, operators},
        decls::Signature,
        types::{CelDouble, CelString, CelUInt, DYN_TYPE},
        value::CowVal,
    },
    parser::Macro,
};

use super::cel_time::{conversion_error, single};

const DOUBLE_INPUT: &str = "@kube.doubleInput";
const UINT_INPUT: &str = "@kube.uintInput";
/// The helper the operands of an ordering comparison go through.
const ORDERED: &str = "@kube.ordered";

/// Why an evaluation that ordered a NaN is refused.
const UNSUPPORTED_NAN_ORDER: &str = "an ordering comparison (<, <=, >, >=) with a NaN operand, which \
                                     the apiserver before 1.38 fails (\"NaN values cannot be \
                                     ordered\") and 1.38 answers false";

/// Adds the macros and helpers described in the [module docs](self).
pub(crate) fn declare(env: &mut Env) -> Result<(), DeclarationError> {
    env.add_overload(
        DOUBLE_INPUT,
        DOUBLE_INPUT,
        Signature::new(vec![DYN_TYPE]),
        double_input,
    )?;
    env.add_overload(UINT_INPUT, UINT_INPUT, Signature::new(vec![DYN_TYPE]), uint_input)?;
    for (conversion, helper) in [("double", DOUBLE_INPUT), ("uint", UINT_INPUT)] {
        for name in [conversion.to_owned(), format!(".{conversion}")] {
            let call = name.clone();
            env.add_macro(Macro::global(name, 1, move |h, _, args| {
                let input = h.next_expr(Expr::Call(CallExpr {
                    target: None,
                    func_name: helper.to_owned(),
                    args: std::mem::take(args),
                }));
                Ok(Some(h.next_expr(Expr::Call(CallExpr {
                    target: None,
                    func_name: call.clone(),
                    args: vec![input],
                }))))
            }))?;
        }
    }
    env.add_overload(ORDERED, ORDERED, Signature::new(vec![DYN_TYPE]), ordered)?;
    for operator in [
        operators::LESS,
        operators::LESS_EQUALS,
        operators::GREATER,
        operators::GREATER_EQUALS,
    ] {
        env.add_macro(Macro::global(operator, 2, move |h, _, args| {
            // A literal is never a NaN (CEL has no NaN literal).
            if args.iter().all(|arg| matches!(arg.expr, Expr::Literal(_))) {
                return Ok(None);
            }
            let operands = std::mem::take(args)
                .into_iter()
                .map(|arg| {
                    if matches!(arg.expr, Expr::Literal(_)) {
                        arg
                    } else {
                        h.next_expr(Expr::Call(CallExpr {
                            target: None,
                            func_name: ORDERED.to_owned(),
                            args: vec![arg],
                        }))
                    }
                })
                .collect();
            Ok(Some(h.next_expr(Expr::Call(CallExpr {
                target: None,
                func_name: operator.to_owned(),
                args: operands,
            }))))
        }))?;
    }
    Ok(())
}

/// An operand of `<`, `<=`, `>` or `>=`, passed through to `cel`'s operator; a
/// NaN is noted as unsupported (see the [module docs](self)).
fn ordered<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    if arg
        .downcast_ref::<CelDouble>()
        .is_some_and(|d| d.inner().is_nan())
    {
        crate::functions::note_unsupported_error(|| {
            ExecutionError::function_error("ordering", UNSUPPORTED_NAN_ORDER)
        });
    }
    Ok(arg)
}

/// `double(<string>)` as cel-go has it: Go's `strconv.ParseFloat(s, 64)`.
fn double_input<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let Some(s) = arg.downcast_ref::<CelString>() else {
        return Ok(arg);
    };
    match go_parse_float(s.inner()) {
        GoFloat::Value(f) => Ok(CowVal::owned(CelDouble::from(f))),
        GoFloat::Fails => Err(conversion_error("double", "double")),
        GoFloat::Unknown => {
            let error = || {
                ExecutionError::function_error(
                    "double",
                    "kube-cel cannot tell what Go reads from more than 800 significant digits",
                )
            };
            crate::functions::note_unsupported_error(error);
            Err(error())
        }
    }
}

/// `uint(<string>)` as cel-go has it: Go's `strconv.ParseUint(s, 10, 64)`,
/// decimal digits only (no sign, no `_`).
fn uint_input<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let Some(s) = arg.downcast_ref::<CelString>() else {
        return Ok(arg);
    };
    let s = s.inner();
    let n = (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse::<u64>().ok())
        .flatten()
        .ok_or_else(|| conversion_error("uint", "uint"))?;
    Ok(CowVal::owned(CelUInt::from(n)))
}

/// What Go's `strconv.ParseFloat(s, 64)` returns.
enum GoFloat {
    Value(f64),
    /// It fails: a syntax error, or a value past the float64 range.
    Fails,
    /// A value or another, as Go's Eisel-Lemire step gives up or not, which
    /// kube-cel does not port (see [`go_parse_float`]).
    Unknown,
}

/// Go's `strconv.ParseFloat(s, 64)` (go1.26.2). Its syntax checks are a port
/// of `special` and `readFloat` (which reads an exponent's digits only while
/// it is below 10,000), a hexadecimal value a port of `atofHex`. A decimal
/// value is Rust's correctly rounded parse of the value Go read, as Go's
/// Eisel-Lemire step has it. Where that step gives up (always past the
/// float64 range), Go falls back to a decimal of 800 significant digits
/// (trailing zeros included), which drops the magnitude of the digits before
/// the point past the 800th (`'1' + 800 zeros + 'e-800'` reads as 0.1);
/// where the two values differ, which one Go gives is [`GoFloat::Unknown`].
fn go_parse_float(s: &str) -> GoFloat {
    let value = |f: Option<f64>| f.map_or(GoFloat::Fails, GoFloat::Value);
    if let Some((f, n)) = special(s) {
        return value((n == s.len()).then_some(f));
    }
    let Some(read) = read_float(s).filter(|read| read.consumed == s.len()) else {
        return GoFloat::Fails;
    };
    if read.hex {
        return value(atof_hex(read.mantissa, read.exp, read.neg, read.trunc));
    }
    let exact = decimal(read.neg, &read.digits, false, read.dp);
    if read.digits.len() <= 800 {
        return value(exact);
    }
    // Go's decimal fallback: the first 800 digits, a sticky bit for any other
    // that is not zero, and the point where the 800th digit is at most.
    let (kept, dropped) = read.digits.split_at(800);
    let sticky = dropped.bytes().any(|c| c != b'0');
    let fallback = decimal(read.neg, kept, sticky, read.dp - (read.point - 800).max(0));
    match exact {
        None => value(fallback),
        Some(f) if fallback.map(f64::to_bits) == Some(f.to_bits()) => GoFloat::Value(f),
        Some(_) => GoFloat::Unknown,
    }
}

/// `0.<digits> × 10^dp`, with a last digit 1 after them if `sticky`, rounded
/// to nearest even by Rust's parser, which is correctly rounded as Go's is;
/// `None` past the float64 range, where Go fails (ErrRange) and Rust rounds
/// to an infinity.
fn decimal(neg: bool, digits: &str, sticky: bool, dp: i64) -> Option<f64> {
    if digits.is_empty() {
        return Some(if neg { -0.0 } else { 0.0 });
    }
    let sign = if neg { "-" } else { "" };
    let sticky = if sticky { "1" } else { "" };
    let f: f64 = format!("{sign}0.{digits}{sticky}e{dp}").parse().ok()?;
    f.is_finite().then_some(f)
}

/// Go's `special`: an infinity (`inf`, `infinity`, signed) or `nan`
/// (unsigned) at the start of `s`, any case, and how many bytes it takes.
fn special(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let common = |t: &[u8], word: &[u8]| {
        t.iter()
            .zip(word)
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .count()
    };
    let (sign, nsign, t) = match b.first()? {
        b'+' => (1.0, 1, &b[1..]),
        b'-' => (-1.0, 1, &b[1..]),
        b'i' | b'I' => (1.0, 0, b),
        b'n' | b'N' => return (common(b, b"nan") == 3).then_some((f64::NAN, 3)),
        _ => return None,
    };
    let mut n = common(t, b"infinity");
    // Anything longer than "inf" but short of "infinity" takes "inf".
    if 3 < n && n < 8 {
        n = 3;
    }
    (n == 3 || n == 8).then_some((sign * f64::INFINITY, nsign + n))
}

/// What Go's `readFloat` reads, and for a decimal the significant digits,
/// where the point is (`0.<digits> × 10^dp`), and how many digits are
/// before the point as written (`point`).
struct Read {
    mantissa: u64,
    exp: i64,
    digits: String,
    dp: i64,
    point: i64,
    neg: bool,
    trunc: bool,
    hex: bool,
    consumed: usize,
}

/// Go's `readFloat`: a sign, decimal or `0x` hexadecimal digits with a
/// point, then an exponent (`e`, or `p`, which hexadecimal requires); `_`
/// between digits. `None` where Go reports a syntax error.
fn read_float(s: &str) -> Option<Read> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    let mut underscores = false;
    match b.first()? {
        b'+' => i += 1,
        b'-' => {
            neg = true;
            i += 1;
        }
        _ => {}
    }
    let mut base: u64 = 10;
    let mut max_mant_digits = 19;
    let mut exp_char = b'e';
    let mut hex = false;
    if i + 2 < b.len() && b[i] == b'0' && b[i + 1].eq_ignore_ascii_case(&b'x') {
        base = 16;
        max_mant_digits = 16;
        i += 2;
        exp_char = b'p';
        hex = true;
    }
    let (mut sawdot, mut sawdigits) = (false, false);
    let (mut nd, mut nd_mant, mut dp, mut point) = (0i64, 0i64, 0i64, 0i64);
    let mut mantissa: u64 = 0;
    let mut trunc = false;
    let mut digits = String::new();
    while i < b.len() {
        let c = b[i];
        let digit = match c {
            b'_' => {
                underscores = true;
                i += 1;
                continue;
            }
            b'.' => {
                if sawdot {
                    break;
                }
                sawdot = true;
                dp = nd;
                point = nd;
                i += 1;
                continue;
            }
            b'0'..=b'9' => u64::from(c - b'0'),
            c if base == 16 && c.to_ascii_lowercase().is_ascii_hexdigit() => {
                u64::from(c.to_ascii_lowercase() - b'a' + 10)
            }
            _ => break,
        };
        sawdigits = true;
        i += 1;
        if c == b'0' && nd == 0 {
            // Leading zeros are not counted.
            dp -= 1;
            continue;
        }
        nd += 1;
        if !hex {
            digits.push(char::from(c));
        }
        if nd_mant < max_mant_digits {
            mantissa = mantissa * base + digit;
            nd_mant += 1;
        } else if c != b'0' {
            trunc = true;
        }
    }
    if !sawdigits {
        return None;
    }
    if !sawdot {
        dp = nd;
        point = nd;
    }
    if base == 16 {
        dp *= 4;
        nd_mant *= 4;
    }
    if i < b.len() && b[i].to_ascii_lowercase() == exp_char {
        i += 1;
        let mut esign = 1;
        match b.get(i)? {
            b'+' => i += 1,
            b'-' => {
                i += 1;
                esign = -1;
            }
            _ => {}
        }
        if !b.get(i)?.is_ascii_digit() {
            return None;
        }
        let mut e: i64 = 0;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            if b[i] == b'_' {
                underscores = true;
            } else if e < 10_000 {
                e = e * 10 + i64::from(b[i] - b'0');
            }
            i += 1;
        }
        dp += e * esign;
    } else if base == 16 {
        // A hexadecimal mantissa needs an exponent.
        return None;
    }
    let exp = if mantissa != 0 { dp - nd_mant } else { 0 };
    if underscores && !underscore_ok(&s[..i]) {
        return None;
    }
    Some(Read {
        mantissa,
        exp,
        digits,
        dp,
        point,
        neg,
        trunc,
        hex,
        consumed: i,
    })
}

/// Go's `underscoreOK`: each `_` sits between two digits, or between a base
/// prefix and a digit.
fn underscore_ok(s: &str) -> bool {
    let mut b = s.as_bytes();
    if let Some(b'+' | b'-') = b.first() {
        b = &b[1..];
    }
    // `^` for the start, `0` for a digit or base prefix, `_`, `!` for others.
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if b.len() >= 2 && b[0] == b'0' && matches!(b[1].to_ascii_lowercase(), b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = b[1].eq_ignore_ascii_case(&b'x');
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || (hex && c.to_ascii_lowercase().is_ascii_hexdigit()) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
        i += 1;
    }
    saw != b'_'
}

/// Go's `atofHex` for float64: `mantissa × 2^exp`, rounded to nearest even
/// with `trunc` as a sticky bit; `None` past the range (Go's ErrRange).
fn atof_hex(mut mantissa: u64, exp: i64, neg: bool, trunc: bool) -> Option<f64> {
    const MANTBITS: u32 = 52;
    const EXPBITS: u32 = 11;
    const BIAS: i64 = -1023;
    let max_exp = (1 << EXPBITS) + BIAS - 2;
    let min_exp = BIAS + 1;
    let mut exp = exp + i64::from(MANTBITS);
    while mantissa != 0 && mantissa >> (MANTBITS + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if trunc {
        mantissa |= 1;
    }
    while mantissa >> (1 + MANTBITS + 2) != 0 {
        mantissa = (mantissa >> 1) | (mantissa & 1);
        exp += 1;
    }
    while mantissa > 1 && exp < min_exp - 2 {
        mantissa = (mantissa >> 1) | (mantissa & 1);
        exp += 1;
    }
    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1;
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + MANTBITS) {
            mantissa >>= 1;
            exp += 1;
        }
    }
    if mantissa >> MANTBITS == 0 {
        exp = BIAS;
    }
    if exp > max_exp {
        return None;
    }
    let mut bits = mantissa & ((1 << MANTBITS) - 1);
    bits |= (((exp - BIAS) & ((1 << EXPBITS) - 1)) as u64) << MANTBITS;
    if neg {
        bits |= 1 << MANTBITS << EXPBITS;
    }
    Some(f64::from_bits(bits))
}

#[cfg(test)]
mod tests {
    use cel::Value;

    use crate::functions::eval;

    // Expected values are cel-go v0.26 (go1.26.2 strconv) through the
    // reference apiserver's environment.

    #[test]
    fn strings_convert_as_go_s_strconv() {
        for holds in [
            "double('1_000') == 1000.0 && double('0x1p-2') == 0.25 && double('0x1.8p1') == 3.0",
            "double('1e-400') == 0.0 && double('Infinity') == double('inf') && double('-INF') < 0.0",
            "double('nan') != double('nan')",
            "uint('18446744073709551615') == 18446744073709551615u && int('+1') == 1 && int('-0') == 0",
            "double(1) == 1.0 && uint(1) == 1u && .double('0x1p0') == 1.0 && .uint('7') == 7u",
        ] {
            assert_eq!(eval(holds), Ok(Value::Bool(true)), "{holds}");
        }
        for fails in [
            "double('1e400')",
            "double('-1e400')",
            "double('0x1p1024')",
            "double('+nan')",
            "double('_1')",
            "double('1__0')",
            "double('0x1')",
            "double('infin')",
            "uint('+1')",
            "uint('18446744073709551616')",
            "uint('1_0')",
            "int('1_0')",
        ] {
            assert!(eval(fails).is_err(), "{fails}");
        }
    }

    /// Past 800 significant digits Go reads a decimal one of two ways.
    #[test]
    fn long_decimals_read_as_go_reads_them() {
        let zeros = "0".repeat(799);
        // 800 digits: Go's two readings agree.
        assert_eq!(
            eval(&format!("double('1{zeros}e-799') == 1.0")),
            Ok(Value::Bool(true))
        );
        // Past 800 digits after the point they agree too; past 800 before it
        // they differ, and which one Go gives is not known.
        let fives = "5".repeat(900);
        assert_eq!(eval(&format!("double('0.{fives}') > 0.5")), Ok(Value::Bool(true)));
        assert!(eval(&format!("double('1{zeros}0e-800')")).is_err());
        // Past the range Go's fallback is the reading.
        assert!(eval(&format!("double('9{}')", "9".repeat(800))).is_err());
    }

    /// An ordering comparison with a NaN operand keeps `cel`'s value (false)
    /// and is noted as unsupported: the apiserver before 1.38 fails it, even
    /// where CEL absorbs the comparison. `==`, `!=` and the list functions
    /// that order a NaN as the apiserver does are not noted. Values are cel-go
    /// v0.26 through the apiserver's environment (k8s.io/apiserver v0.36.1),
    /// and v0.33 (v0.38.0-beta.0) for the false of an ordering.
    #[test]
    fn ordering_a_nan_is_noted() {
        let tracked = |expr: &str| crate::functions::tracking_unsupported(|| eval(expr));
        for (expr, value) in [
            ("double('NaN') < 1.0", false),
            ("1 <= double('NaN')", false),
            ("1u > double('NaN')", false),
            ("double('NaN') >= double('NaN')", false),
            ("!(double('NaN') < 1.0) || true", true),
            ("[double('NaN'), 0.5].exists(x, x < 1.0)", true),
            ("[1.5].map(x, x * 0.0 / 0.0).all(x, !(x > 1.0))", true),
        ] {
            let (result, noted) = tracked(expr);
            assert_eq!(result, Ok(Value::Bool(value)), "{expr}");
            assert!(noted.is_some(), "{expr} must be noted");
        }
        let not_noted = |expr: &str| {
            let (result, noted) = tracked(expr);
            assert_eq!(result, Ok(Value::Bool(true)), "{expr}");
            assert!(noted.is_none(), "{expr} must not be noted");
        };
        not_noted("double('NaN') != double('NaN') && !(double('NaN') == 1.0)");
        not_noted("1.0 < 2.0 && 2 >= 1u && double('Inf') > 1.0 && -double('Inf') < 1.0");
        not_noted("true || double('NaN') < 1.0");
        #[cfg(feature = "lists")]
        {
            not_noted("[1.0, double('NaN')].min() == 1.0 && [1.0, double('NaN'), 0.5].isSorted()");
            not_noted("[double('NaN'), 1.0].sort().size() == 2");
        }
    }

    /// The operands of an ordering comparison go through `@kube.ordered` in
    /// place, a literal aside, so each is still evaluated once.
    #[test]
    fn ordering_operands_are_wrapped_once() {
        let env = crate::new_env();
        let ordered = |expr: &str| {
            let program = env.compile(expr).expect("compiles");
            let mut wrapped = Vec::new();
            fn walk(e: &cel::common::ast::IdedExpr, wrapped: &mut Vec<bool>) {
                if let cel::common::ast::Expr::Call(call) = &e.expr {
                    use cel::common::ast::operators::{GREATER, GREATER_EQUALS, LESS, LESS_EQUALS};
                    if [LESS, LESS_EQUALS, GREATER, GREATER_EQUALS].contains(&call.func_name.as_str()) {
                        wrapped.extend(call.args.iter().map(|a| {
                            matches!(&a.expr, cel::common::ast::Expr::Call(c) if c.func_name == super::ORDERED)
                        }));
                    }
                    call.target.iter().for_each(|t| walk(t, wrapped));
                    call.args.iter().for_each(|a| walk(a, wrapped));
                }
            }
            walk(program.expression(), &mut wrapped);
            wrapped
        };
        assert_eq!(ordered("1 < 2"), [false, false]);
        assert_eq!(ordered("double('NaN') < 1.0"), [true, false]);
        assert_eq!(ordered("1 >= dyn(2) && dyn(1) <= dyn(2)"), [
            false, true, true, true
        ]);
        assert_eq!(ordered("dyn(1) > 0"), [true, false]);
    }
}
