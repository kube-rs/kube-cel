//! Kubernetes CEL string formatting extension function.
//!
//! Provides printf-style `format()` function,
//! matching `cel-go/ext/strings.go` format implementation.

use cel::{
    ExecutionError, ResolveResult,
    extractors::This,
    objects::{Key, Value},
};
use std::sync::Arc;

use crate::functions::Registrar;

/// Register the format function.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("format", format_string);
}

/// `<string>.format(<list>) -> string`
fn format_string(This(fmt): This<Arc<String>>, args: Value) -> ResolveResult {
    let args = match args {
        Value::List(list) => list,
        _ => {
            return Err(ExecutionError::function_error(
                "format",
                "format() requires a list argument",
            ));
        }
    };

    let mut result = String::new();
    let mut arg_idx: usize = 0;
    let mut chars = fmt.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '%' {
            result.push(ch);
            continue;
        }

        // Next char determines the verb
        let Some(next) = chars.next() else {
            return Err(ExecutionError::function_error(
                "format",
                "format string ends with '%'",
            ));
        };

        // Literal %
        if next == '%' {
            result.push('%');
            continue;
        }

        // Parse optional precision: %.Nf or %.Ne
        let (precision, verb) = if next == '.' {
            let mut prec_str = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    prec_str.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            // Go's `strconv.Atoi`: a precision past the int64 range is an error
            // for every verb, including those that ignore it.
            let prec: i64 = prec_str.parse().map_err(|_| {
                ExecutionError::function_error("format", "invalid precision in format string")
            })?;
            let v = chars.next().ok_or_else(|| {
                ExecutionError::function_error("format", "format string ends after precision")
            })?;
            (Some(prec), v)
        } else {
            (None, next)
        };

        // Consume one argument
        if arg_idx >= args.len() {
            return Err(ExecutionError::function_error(
                "format",
                format!(
                    "not enough arguments: format requires at least {} but got {}",
                    arg_idx + 1,
                    args.len()
                ),
            ));
        }
        let arg = &args[arg_idx];
        arg_idx += 1;

        match verb {
            's' => format_s(arg, &mut result)?,
            'd' => format_d(arg, &mut result)?,
            'f' => format_f(arg, float_precision(precision)?, &mut result)?,
            'e' => format_e(arg, float_precision(precision)?, &mut result)?,
            'b' => format_b(arg, &mut result)?,
            'o' => format_o(arg, &mut result)?,
            'x' => format_hex(arg, false, &mut result)?,
            'X' => format_hex(arg, true, &mut result)?,
            _ => {
                return Err(ExecutionError::function_error(
                    "format",
                    format!("unknown format verb '%{verb}'"),
                ));
            }
        }
    }

    Ok(Value::String(Arc::new(result)))
}

/// `%s`, as cel-go v0.26 `FormatString` (ext/formatting.go: the formatter of
/// `ext.Strings` below version 4, which the apiserver's base environment uses).
/// A scalar is converted like `string(x)`; a list or map is written with its
/// elements as CEL literals (see [`format_nested`]).
fn format_s(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::List(list) => format_list(list, out),
        Value::Map(map) => format_map(map, out),
        Value::Null => {
            out.push_str("null");
            Ok(())
        }
        other => {
            out.push_str(&to_cel_string(other)?);
            Ok(())
        }
    }
}

/// cel-go's `ConvertToType(StringType)` for a scalar: a double is Go `%g`,
/// bytes must be UTF-8, a timestamp is RFC3339Nano and a duration seconds + "s".
/// Anything else (an IP, a quantity, a URL, ..) has no `%s` form in cel-go.
fn to_cel_string(val: &Value) -> Result<String, ExecutionError> {
    Ok(match val {
        Value::String(s) => s.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) => format_float_default(*f),
        Value::Bool(b) => b.to_string(),
        Value::Bytes(b) => utf8_bytes(b)?.to_owned(),
        Value::Timestamp(_) | Value::Duration(_) => {
            cel_value_to_string(val).expect("timestamps and durations always convert")
        }
        other => {
            return Err(ExecutionError::function_error(
                "format",
                format!("%s does not format {}", other.type_of()),
            ));
        }
    })
}

/// A list element or map value, as cel-go's `clauseForType` + `quoteForCEL`
/// write it: a valid CEL literal. Strings and bytes are Go-quoted (`"a"`,
/// `b"a"`), a double is `%.6f` (an infinity or NaN quoted), and a timestamp or
/// duration is wrapped as `timestamp("…")` / `duration("…")`.
fn format_nested(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::String(s) => out.push_str(&go_quote(s)),
        Value::Bytes(b) => {
            out.push('b');
            out.push_str(&go_quote(utf8_bytes(b)?));
        }
        Value::Float(f) if f.is_finite() => out.push_str(&format!("{f:.6}")),
        // Go's `%.6f` of ±Inf/NaN is "+Inf"/"-Inf"/"NaN", the same as `%g`.
        Value::Float(f) => out.push_str(&go_quote(&format_float_default(*f))),
        Value::Timestamp(_) => {
            out.push_str("timestamp(");
            out.push_str(&go_quote(&to_cel_string(val)?));
            out.push(')');
        }
        Value::Duration(_) => {
            out.push_str("duration(");
            out.push_str(&go_quote(&to_cel_string(val)?));
            out.push(')');
        }
        other => format_s(other, out)?,
    }
    Ok(())
}

fn format_list(list: &[Value], out: &mut String) -> Result<(), ExecutionError> {
    out.push('[');
    for (i, item) in list.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        format_nested(item, out)?;
    }
    out.push(']');
    Ok(())
}

/// `{key:value, ..}`, entries sorted by the written key (cel-go
/// `sort.SliceStable` on the quoted key string, so `-1` < `10` < `2`).
fn format_map(map: &cel::objects::Map, out: &mut String) -> Result<(), ExecutionError> {
    let mut entries = Vec::with_capacity(map.map.len());
    for (key, value) in map.map.iter() {
        let key = match key {
            Key::String(s) => go_quote(s),
            Key::Int(n) => n.to_string(),
            Key::Uint(n) => n.to_string(),
            Key::Bool(b) => b.to_string(),
        };
        let mut written = String::new();
        format_nested(value, &mut written)?;
        entries.push((key, written));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    out.push('{');
    for (i, (key, value)) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(key);
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    Ok(())
}

/// Go's `%q` (`strconv.Quote`): double quotes, `\"` and `\\`, the C escapes,
/// `\xNN` below space and for DEL, `\uNNNN` / `\UNNNNNNNN` for other characters
/// Go does not print. Go's `unicode.IsPrint` needs category tables; this treats
/// controls, separators other than the space, and the format characters and
/// private-use ranges below as non-printing, and keeps the rest (an unassigned
/// code point is escaped by Go but kept here).
fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{07}' => out.push_str("\\a"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0b}' => out.push_str("\\v"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if go_prints(c) => out.push(c),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// An approximation of Go's `unicode.IsPrint` (see [`go_quote`]).
fn go_prints(c: char) -> bool {
    let u = c as u32;
    let format_char = matches!(
        u,
        0xAD | 0x600..=0x605
            | 0x61C
            | 0x6DD
            | 0x70F
            | 0x890..=0x891
            | 0x8E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    );
    let private_use = matches!(u, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD);
    c == ' ' || !(c.is_control() || c.is_whitespace() || format_char || private_use)
}

/// Bytes as UTF-8 text, as cel-go's `ConvertToType(StringType)` requires.
fn utf8_bytes(b: &[u8]) -> Result<&str, ExecutionError> {
    std::str::from_utf8(b).map_err(|_| {
        ExecutionError::function_error("format", "invalid UTF-8 in bytes, cannot convert to string")
    })
}

/// Render a `Timestamp`/`Duration` to string exactly as cel-go's
/// `ConvertToType(StringType)` — the conversion behind both `string(x)` and the
/// `%s` format verb on the apiserver. Timestamps use `time.RFC3339Nano`;
/// durations use total seconds + `"s"`
/// (`strconv.FormatFloat(d.Seconds(), 'f', -1, 64) + "s"`), NOT Go's
/// `Duration.String()` `"1h0m0s"` form. Returns `None` for other value types.
///
/// Uses only inherent `chrono` methods on the bound value, so it needs no
/// direct `chrono` dependency.
fn cel_value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::Timestamp(t) => {
            // RFC3339Nano: date-time, then fractional seconds with ALL trailing
            // zeros removed (Go trims; chrono's `%.f` would keep them, so trim the
            // zero-padded `%9f` by hand), then "Z" for UTC else the ±hh:mm offset.
            let mut s = t.format("%Y-%m-%dT%H:%M:%S").to_string();
            let frac = t.format("%9f").to_string();
            let frac = frac.trim_end_matches('0');
            if !frac.is_empty() {
                s.push('.');
                s.push_str(frac);
            }
            if t.offset().local_minus_utc() == 0 {
                s.push('Z');
            } else {
                s.push_str(&t.format("%:z").to_string());
            }
            Some(s)
        }
        Value::Duration(d) => {
            // cel-go: FormatFloat(d.Seconds(), 'f', -1, 64) + "s". Go's `'f'` and
            // Rust's `Display` are both shortest and never exponential, so agree.
            // `Seconds()` adds whole seconds and the nanosecond remainder as two
            // floats; dividing the nanosecond total rounds differently.
            let secs = d.num_nanoseconds().map_or_else(
                || d.num_seconds() as f64,
                |n| (n / 1_000_000_000) as f64 + (n % 1_000_000_000) as f64 / 1e9,
            );
            Some(format!("{secs}s"))
        }
        _ => None,
    }
}

fn format_float_default(f: f64) -> String {
    // cel-go renders a double for `%s` through Go's `%g` (common/types/double.go
    // `ConvertToType(String)` = `fmt.Sprintf("%g", f)`): shortest round-trip
    // digits, infinities as "+Inf"/"-Inf", NaN as "NaN", and scientific notation
    // when the decimal exponent is < -4 or >= 6 (strconv uses precision 6 for
    // that choice when printing the shortest digits; 21 is `%v`'s threshold).
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    // Rust's `{:e}` gives the shortest mantissa + a bare exponent (e.g. "1e0",
    // "1.5e0", "1e21", "-1.2e-7"); use the exponent to pick the notation.
    let sci = format!("{f:e}");
    let epos = sci.find('e').expect("{:e} always emits 'e'");
    let exp: i32 = sci[epos + 1..].parse().expect("{:e} exponent is an integer");
    if !(-4..6).contains(&exp) {
        // Go `%g` scientific form: shortest mantissa + `e` + signed 2-digit exp.
        let mantissa = &sci[..epos];
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        // In this range Go `%g` uses plain decimal, matching Rust's `Display`.
        f.to_string()
    }
}

/// %d — decimal integer.
fn format_d(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::UInt(n) => out.push_str(&n.to_string()),
        _ => {
            return Err(ExecutionError::function_error(
                "format",
                format!("%d requires int or uint, got {:?}", val.type_of()),
            ));
        }
    }
    Ok(())
}

/// The precision `%f` / `%e` pass to Rust's formatter, 6 when none is given.
/// Rust's formatter panics past `u16::MAX`, so a larger one is an error; the
/// apiserver formats it (a documented fail-closed difference).
fn float_precision(precision: Option<i64>) -> Result<usize, ExecutionError> {
    match precision {
        None => Ok(6),
        Some(p) => u16::try_from(p)
            .map(usize::from)
            .map_err(|_| ExecutionError::function_error("format", format!("precision {p} is too large"))),
    }
}

/// %f — fixed-point float.
fn format_f(val: &Value, precision: usize, out: &mut String) -> Result<(), ExecutionError> {
    let f = extract_float(val, 'f')?;
    out.push_str(&format!("{f:.precision$}"));
    Ok(())
}

/// %e — scientific notation.
///
/// cel-go formats `%e` through Go's `fmt`, whose exponent ALWAYS carries a sign
/// and is zero-padded to at least two digits (e.g. `1.50e+03`). Rust's `{:e}`
/// omits both (`1.50e3`), so reformat the exponent to match the apiserver.
fn format_e(val: &Value, precision: usize, out: &mut String) -> Result<(), ExecutionError> {
    let f = extract_float(val, 'e')?;
    let rust = format!("{f:.precision$e}");
    let (mantissa, exp) = rust.split_once('e').unwrap_or((rust.as_str(), "0"));
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ('-', d),
        None => ('+', exp),
    };
    out.push_str(mantissa);
    out.push('e');
    out.push(sign);
    if digits.len() < 2 {
        out.push('0');
    }
    out.push_str(digits);
    Ok(())
}

/// %b — binary representation for int/uint, or "1"/"0" for bool (matching cel-go).
fn format_b(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::Int(n) => out.push_str(&format!("{n:b}")),
        Value::UInt(n) => out.push_str(&format!("{n:b}")),
        Value::Bool(b) => out.push_str(if *b { "1" } else { "0" }),
        _ => {
            return Err(ExecutionError::function_error(
                "format",
                format!("%b requires int, uint, or bool, got {:?}", val.type_of()),
            ));
        }
    }
    Ok(())
}

/// %o — octal.
fn format_o(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::Int(n) => out.push_str(&format!("{n:o}")),
        Value::UInt(n) => out.push_str(&format!("{n:o}")),
        _ => {
            return Err(ExecutionError::function_error(
                "format",
                format!("%o requires int or uint, got {:?}", val.type_of()),
            ));
        }
    }
    Ok(())
}

/// %x / %X — hexadecimal.
fn format_hex(val: &Value, upper: bool, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::Int(n) => {
            if upper {
                out.push_str(&format!("{n:X}"));
            } else {
                out.push_str(&format!("{n:x}"));
            }
        }
        Value::UInt(n) => {
            if upper {
                out.push_str(&format!("{n:X}"));
            } else {
                out.push_str(&format!("{n:x}"));
            }
        }
        Value::String(s) => {
            for byte in s.as_bytes() {
                if upper {
                    out.push_str(&format!("{byte:02X}"));
                } else {
                    out.push_str(&format!("{byte:02x}"));
                }
            }
        }
        Value::Bytes(b) => {
            for byte in b.iter() {
                if upper {
                    out.push_str(&format!("{byte:02X}"));
                } else {
                    out.push_str(&format!("{byte:02x}"));
                }
            }
        }
        _ => {
            return Err(ExecutionError::function_error(
                "format",
                format!("%x requires int, uint, string, or bytes, got {:?}", val.type_of()),
            ));
        }
    }
    Ok(())
}

fn extract_float(val: &Value, verb: char) -> Result<f64, ExecutionError> {
    match val {
        Value::Float(f) => Ok(*f),
        Value::Int(n) => Ok(*n as f64),
        Value::UInt(n) => Ok(*n as f64),
        _ => Err(ExecutionError::function_error(
            "format",
            format!("%{verb} requires float, int, or uint, got {:?}", val.type_of()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    fn eval_str(expr: &str) -> String {
        match eval(expr) {
            Value::String(s) => (*s).clone(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn test_format_s() {
        assert_eq!(eval_str("'hello %s'.format(['world'])"), "hello world");
        assert_eq!(eval_str("'%s is %s'.format(['age', 'number'])"), "age is number");
    }

    #[test]
    fn test_format_d() {
        assert_eq!(eval_str("'count: %d'.format([42])"), "count: 42");
    }

    #[test]
    fn test_format_f() {
        assert_eq!(eval_str("'pi: %.2f'.format([3.14159])"), "pi: 3.14");
        assert_eq!(eval_str("'val: %f'.format([1.5])"), "val: 1.500000");
    }

    #[test]
    fn test_format_precision_past_u16_is_an_error() {
        // Rust's `{:.N}` panics ("Formatting argument out of range") once N
        // exceeds u16::MAX; found by the fuzz sweep.
        assert_eq!(eval_str("'%.65535f'.format([1.5])").len(), 65537);
        assert!(crate::functions::eval("'%.65536f'.format([1.5])").is_err());
        assert!(crate::functions::eval("'%.4294967296e'.format([1.5])").is_err());
        // Verbs that ignore the precision take any int64 one, as in cel-go.
        assert_eq!(eval_str("'%.65536s'.format(['hello'])"), "hello");
        assert_eq!(eval_str("'%.9223372036854775807d'.format([12])"), "12");
        // Past int64 it is an error for every verb (Go strconv.Atoi).
        assert!(crate::functions::eval("'%.9223372036854775808s'.format(['a'])").is_err());
    }

    #[test]
    fn test_format_s_map() {
        // cel-go renders `key:value` with no space after the colon.
        assert_eq!(eval_str("'%s'.format([{'a': 1}])"), "{\"a\":1}");
    }

    #[test]
    fn test_format_b() {
        assert_eq!(eval_str("'bin: %b'.format([10])"), "bin: 1010");
        assert_eq!(eval_str("'val: %b'.format([true])"), "val: 1");
    }

    #[test]
    fn test_format_o() {
        assert_eq!(eval_str("'oct: %o'.format([8])"), "oct: 10");
    }

    #[test]
    fn test_format_x() {
        assert_eq!(eval_str("'hex: %x'.format([255])"), "hex: ff");
        assert_eq!(eval_str("'hex: %X'.format([255])"), "hex: FF");
    }

    #[test]
    fn test_format_hex_string() {
        assert_eq!(eval_str("'%x'.format(['AB'])"), "4142");
    }

    #[test]
    fn test_format_escape() {
        assert_eq!(eval_str("'100%%'.format([])"), "100%");
    }

    #[test]
    fn test_format_multiple_args() {
        assert_eq!(
            eval_str("'%s has %d items'.format(['cart', 5])"),
            "cart has 5 items"
        );
    }

    #[test]
    fn test_format_list() {
        assert_eq!(eval_str("'val: %s'.format([[1, 2, 3]])"), "val: [1, 2, 3]");
    }

    #[test]
    fn test_format_s_int() {
        assert_eq!(eval_str("'%s'.format([42])"), "42");
    }

    #[test]
    fn test_format_s_bool() {
        assert_eq!(eval_str("'%s'.format([true])"), "true");
    }

    #[test]
    fn test_format_too_few_args() {
        let result = crate::functions::eval("'%s %s'.format(['only_one'])");
        assert!(result.is_err());
    }

    #[test]
    fn test_format_unknown_verb() {
        let result = crate::functions::eval("'%z'.format([1])");
        assert!(result.is_err());
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        crate::functions::eval(expr).unwrap_err()
    }

    #[test]
    fn test_format_trailing_percent() {
        eval_err("'hello%'.format([])");
    }

    #[test]
    fn test_format_d_type_error() {
        eval_err("'%d'.format([1.5])");
        eval_err("'%d'.format([true])");
    }

    #[test]
    fn test_format_b_type_error() {
        eval_err("'%b'.format([1.5])");
    }

    #[test]
    fn test_format_o_type_error() {
        eval_err("'%o'.format([1.5])");
    }

    #[test]
    fn test_format_x_uppercase_string() {
        assert_eq!(eval_str("'%X'.format(['AB'])"), "4142");
    }

    #[test]
    fn test_format_s_null() {
        assert_eq!(eval_str("'%s'.format([null])"), "null");
    }

    #[test]
    fn test_format_s_float() {
        // cel-go's %s on a double is Go's `%g`: a whole double is "1", not "1.0".
        assert_eq!(eval_str("'%s'.format([1.0])"), "1");
        assert_eq!(eval_str("'%s'.format([1.5])"), "1.5");
        assert_eq!(eval_str("'%s'.format([100000.0])"), "100000");
        assert_eq!(eval_str("'%s'.format([0.0001])"), "0.0001");
        // Scientific form kicks in at exponent < -4 or >= 21 (Go %g).
        assert_eq!(eval_str("'%s'.format([0.00001])"), "1e-05");
        assert_eq!(eval_str("'%s'.format([1e21])"), "1e+21");
    }

    #[test]
    fn test_format_s_infinity() {
        // cel-go's double→string goes through Go's %g: "+Inf"/"-Inf", not "inf".
        assert_eq!(eval_str("'%s'.format([1.0 / 0.0])"), "+Inf");
        assert_eq!(eval_str("'%s'.format([-1.0 / 0.0])"), "-Inf");
    }

    #[test]
    fn test_format_s_bytes() {
        // Top-level %s renders bytes as their UTF-8 text (no b"" wrapper), not
        // Rust's Debug `Bytes([...])`.
        assert_eq!(eval_str("'%s'.format([b'hello'])"), "hello");
    }

    #[test]
    fn test_format_s_matches_cel_go_v026() {
        // Expected strings are cel-go v0.26 output, through
        // k8s.io/apiserver v0.36.1 `MustBaseEnvSet(1.36)`.
        let cases: &[(&str, &str)] = &[
            ("'%s'.format([1000000.0])", "1e+06"),
            ("'%s'.format([123456.0])", "123456"),
            ("'%s'.format([0.00001])", "1e-05"),
            ("'%s'.format([1e21])", "1e+21"),
            ("'%s'.format([1.5])", "1.5"),
            ("'%s'.format([-0.0])", "-0"),
            ("'%s'.format([[1.5]])", "[1.500000]"),
            ("'%s'.format([[1.0/0.0, -1.0/0.0]])", "[\"+Inf\", \"-Inf\"]"),
            ("'%s'.format([{'b': 1, 'a': 2}])", "{\"a\":2, \"b\":1}"),
            ("'%s'.format([[duration('1h')]])", "[duration(\"3600s\")]"),
            (
                "'%s'.format([[timestamp('2024-01-15T10:30:00Z')]])",
                "[timestamp(\"2024-01-15T10:30:00Z\")]",
            ),
            ("'%s'.format([[b'abc']])", "[b\"abc\"]"),
            ("'%s'.format([['a\"b']])", "[\"a\\\"b\"]"),
            ("'%s'.format([[1u]])", "[1]"),
            ("'%s'.format([[true, null]])", "[true, null]"),
            ("'%s'.format([{1: 'x', 2: 'y'}])", "{1:\"x\", 2:\"y\"}"),
            ("'%s'.format([{true: 1, false: 2}])", "{false:2, true:1}"),
            (
                "'%s'.format([duration('894385949.183117216s')])",
                "894385949.1831173s",
            ),
            ("'%s'.format([duration('-1.5s')])", "-1.5s"),
            (
                "'%s'.format([timestamp('2024-01-15T10:30:00+09:00')])",
                "2024-01-15T10:30:00+09:00",
            ),
            ("'%s'.format([[['x']]])", "[[\"x\"]]"),
            ("'%s'.format([{'k': [1.5]}])", "{\"k\":[1.500000]}"),
            ("'%s'.format([['é\\n\\t\\\\']])", "[\"é\\n\\t\\\\\"]"),
            ("'%s'.format([[-0.0]])", "[-0.000000]"),
            (
                "'%s'.format([{'z': 1, 'Z': 2, 'a': 3}])",
                "{\"Z\":2, \"a\":3, \"z\":1}",
            ),
            (
                "'%s'.format([{-1: 'a', 10: 'b', 2: 'c'}])",
                "{-1:\"a\", 10:\"b\", 2:\"c\"}",
            ),
            (
                "'%s'.format([['\u{a0}', '\u{200b}']])",
                "[\"\\u00a0\", \"\\u200b\"]",
            ),
            ("'%s'.format([1e-7])", "1e-07"),
            ("'%s'.format([1.7976931348623157e308])", "1.7976931348623157e+308"),
            ("'%s'.format([[1e21]])", "[1000000000000000000000.000000]"),
        ];
        for (expr, want) in cases {
            assert_eq!(eval_str(expr), *want, "{expr}");
        }
        eval_err("'%s'.format([b'\\xff'])");
        eval_err("'%s'.format([[b'\\xff']])");
    }

    #[test]
    fn test_format_s_refuses_what_cel_go_cannot_convert() {
        // Invalid UTF-8 errors, top level and nested, instead of U+FFFD.
        eval_err("'%s'.format([b'\\xff'])");
        eval_err("'%s'.format([[b'\\xff']])");
        assert_eq!(eval_str("'%s'.format([b'h\\xc3\\xa9'])"), "hé");
    }

    #[test]
    fn test_format_s_duration_timestamp() {
        // cel-go's %s renders these via ConvertToType(String): duration →
        // seconds+"s", timestamp → RFC3339Nano — not Rust's Debug fallback.
        assert_eq!(eval_str("'%s'.format([duration('1h30m')])"), "5400s");
        assert_eq!(
            eval_str("'%s'.format([timestamp('2024-01-15T10:30:00Z')])"),
            "2024-01-15T10:30:00Z"
        );
    }

    #[test]
    fn test_format_extra_args_ignored() {
        // Extra arguments beyond what's needed should be silently ignored
        assert_eq!(eval_str("'%s'.format(['a', 'b'])"), "a");
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_format_percent_around_substitution() {
        assert_eq!(eval_str("'%%%s%%'.format(['text'])"), "%text%");
        assert_eq!(
            eval_str("'%%%s'.format(['percent on the left'])"),
            "%percent on the left"
        );
        assert_eq!(
            eval_str("'%s%%'.format(['percent on the right'])"),
            "percent on the right%"
        );
    }

    #[test]
    fn test_format_b_bool_false() {
        assert_eq!(eval_str("'%b'.format([false])"), "0");
    }

    #[test]
    fn test_format_hex_bytes() {
        // Bytes hex encoding with leading zeros preserved
        let result = crate::functions::eval("'%x'.format([b'\\x00\\x00AB'])").unwrap();
        assert_eq!(result, Value::String(Arc::new("00004142".into())));
    }

    #[test]
    fn test_format_x_uppercase_full() {
        assert_eq!(
            eval_str("'%X'.format(['Hello world!'])"),
            "48656C6C6F20776F726C6421"
        );
    }
}
