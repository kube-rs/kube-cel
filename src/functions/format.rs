//! Kubernetes CEL string formatting: `<string>.format(<list>)`.
//!
//! The apiserver's base environment has cel-go v0.26's `ext.Strings` at version
//! 2, whose `format()` is cel-go's first formatter (`ext/formatting.go`; the
//! `formatting_v2.go` one starts at version 4). This is a port of it: the clause
//! parser, `%s` through cel-go's string conversion, `%d` / `%b` / `%o` / `%x` /
//! `%X` through Go's `fmt`, and `%f` / `%e` through the en-US number formatter
//! of golang.org/x/text v0.33.0's `message` printer, which groups thousands
//! (`1,500.00`) and writes the exponent in superscript (`1.500000×10⁰³`).

use cel::{
    ExecutionError, FunctionContext, ResolveResult,
    common::{
        types::{CelList, CelMap, CelType, Kind},
        value::Val,
    },
    extractors::This,
    objects::{Key, Map, Opaque, Value},
};
use std::{collections::HashMap, sync::Arc};

use crate::functions::{
    Registrar,
    cel_time::time_string,
    go_float::{exact_decimal, fmt_f, go_g, go_shortest, push_digit, round_go},
    go_isprint,
};

/// Register the format function.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("format", format_string);
}

fn err(message: impl ToString) -> ExecutionError {
    ExecutionError::function_error("format", message)
}

/// `<string>.format(<list>) -> string`, cel-go's `parseFormatString`. The
/// arguments are read as `cel` values rather than [`Value`]s, so that a type
/// value (`type(1)`) stays a type, which `%s` writes unquoted in a list.
fn format_string(ftx: &FunctionContext, This(format): This<Arc<String>>) -> ResolveResult {
    let list = ftx
        .args
        .get(ftx.arg_idx)
        .ok_or_else(|| err("format() requires a list argument"))?;
    let Value::List(args) = format_value(list.as_ref())? else {
        return Err(err("format() requires a list argument"));
    };

    let bytes = format.as_bytes();
    let mut out = String::with_capacity(format.len());
    let (mut i, mut arg_index) = (0, 0);
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let end = bytes[i..]
                .iter()
                .position(|&b| b == b'%')
                .map_or(bytes.len(), |p| i + p);
            out.push_str(&format[i..end]);
            i = end;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'%') {
            out.push('%');
            i += 2;
            continue;
        }
        // cel-go takes the argument before it looks at the clause.
        let arg = args
            .get(arg_index)
            .ok_or_else(|| err(format!("index {arg_index} out of range")))?;
        if i + 1 >= bytes.len() {
            return Err(err("unexpected end of string"));
        }
        let (read, verb, precision) = parse_clause(&bytes[i + 1..])?;
        format_clause(verb, precision, arg, &mut out)?;
        i += 1 + read;
        arg_index += 1;
    }
    Ok(Value::String(Arc::new(out)))
}

/// cel-go's `parseFormattingClause` on the bytes after a `%` (never empty): an
/// optional `.` and decimal precision, then the verb byte. Returns how many
/// bytes it read, the verb and the precision. The precision is an `int` (Go's
/// `strconv.Atoi`), so one past the int64 range is an error for every verb,
/// although only `%f` and `%e` use it.
fn parse_clause(s: &[u8]) -> Result<(usize, u8, Option<i64>), ExecutionError> {
    let mut i = 0;
    let mut precision = None;
    if s[0] == b'.' {
        i = 1;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        if i >= s.len() {
            return Err(err("could not find end of precision specifier"));
        }
        let digits = std::str::from_utf8(&s[1..i]).expect("ASCII digits");
        precision = Some(
            digits
                .parse::<i64>()
                .map_err(|_| err(format!("invalid precision {digits:?}")))?,
        );
    }
    Ok((i + 1, s[i], precision))
}

fn format_clause(
    verb: u8,
    precision: Option<i64>,
    arg: &Value,
    out: &mut String,
) -> Result<(), ExecutionError> {
    match verb {
        b's' => return format_s(arg, out),
        b'd' => out.push_str(&format_decimal(arg)?),
        b'f' => out.push_str(&format_fixed(arg, precision.unwrap_or(6))?),
        b'e' => out.push_str(&format_scientific(arg, precision.unwrap_or(6))?),
        b'b' => out.push_str(&format_binary(arg)?),
        b'o' => out.push_str(&format_octal(arg)?),
        b'x' => out.push_str(&format_hex(arg, false)?),
        b'X' => out.push_str(&format_hex(arg, true)?),
        other => {
            return Err(err(format!(
                "unrecognized formatting clause \"{}\"",
                char::from(other)
            )));
        }
    }
    Ok(())
}

/// A type value among `format()`'s arguments. [`Value`] holds a type as its
/// name in a string, which `%s` would quote inside a list; cel-go writes the
/// bare name (`[int]`).
#[derive(Debug, PartialEq, Eq)]
struct TypeName(String);

impl Opaque for TypeName {
    fn runtime_type_name(&self) -> &str {
        "type"
    }
}

/// A `cel` value as a [`Value`], but for a type value, which becomes a
/// [`TypeName`], also inside lists and maps.
fn format_value(v: &dyn Val) -> Result<Value, ExecutionError> {
    match v.get_type().kind() {
        Kind::Type => {
            if let Some(t) = v.downcast_ref::<CelType>() {
                return Ok(Value::Opaque(Arc::new(TypeName(t.name().to_owned()))));
            }
        }
        Kind::List => {
            if let Some(list) = v.downcast_ref::<CelList>() {
                let items = list
                    .inner()
                    .iter()
                    .map(|item| format_value(item.as_ref()))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Value::List(Arc::new(items)));
            }
            // A list of an application's own type (the validation engine's
            // set and map lists): through its elements, in order.
            if let Some(iterable) = v.as_iterable() {
                let mut elements = iterable.iter();
                let mut items = Vec::new();
                while let Some(item) = elements.next() {
                    items.push(format_value(item)?);
                }
                return Ok(Value::List(Arc::new(items)));
            }
        }
        Kind::Map => {
            if let Some(map) = v.downcast_ref::<CelMap>() {
                let entries = map
                    .inner()
                    .iter()
                    .map(|(k, v)| Ok((Key::from(k.clone()), format_value(v.as_ref())?)))
                    .collect::<Result<HashMap<_, _>, ExecutionError>>()?;
                return Ok(Value::Map(Map {
                    map: Arc::new(entries),
                }));
            }
            // A map of an application's own type (the validation engine's
            // objects): each key it iterates, looked up. A key it iterates
            // but does not hold (an object's `null` property) fails, as the
            // apiserver's formatter does.
            if let (Some(iterable), Some(indexer)) = (v.as_iterable(), v.as_indexer()) {
                let mut keys = iterable.iter();
                let mut entries = HashMap::new();
                while let Some(key) = keys.next() {
                    let value = format_value(indexer.get(key)?.as_ref())?;
                    let key = match Value::try_from(key)? {
                        Value::String(s) => Key::String(s),
                        Value::Int(i) => Key::Int(i),
                        Value::UInt(u) => Key::Uint(u),
                        Value::Bool(b) => Key::Bool(b),
                        other => return Err(err(format!("unsupported map key {other:?}"))),
                    };
                    entries.insert(key, value);
                }
                return Ok(Value::Map(Map {
                    map: Arc::new(entries),
                }));
            }
        }
        _ => {}
    }
    Value::try_from(v)
}

// ---------------------------------------------------------------------------
// %s
// ---------------------------------------------------------------------------

/// `%s`, cel-go's `FormatString`: a scalar converted as `string(x)` would be
/// (cel-go's `ConvertToType(StringType)`), a list or map with its elements as
/// CEL literals (see [`format_nested`]).
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

/// cel-go's `ConvertToType(StringType)` for a scalar: a double is Go's `%g`,
/// bytes must be UTF-8, a timestamp is RFC3339Nano, a duration its seconds and
/// "s", a type its name. Anything else (an IP, a quantity, a URL, ..) has no
/// `%s` form in cel-go.
fn to_cel_string(val: &Value) -> Result<String, ExecutionError> {
    Ok(match val {
        Value::String(s) => s.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) => go_g(*f),
        Value::Bool(b) => b.to_string(),
        Value::Bytes(b) => utf8_bytes(b)?.to_owned(),
        Value::Timestamp(_) | Value::Duration(_) => {
            time_string(val).expect("timestamps and durations convert")
        }
        Value::Opaque(o) => match o.downcast_ref::<TypeName>() {
            Some(t) => t.0.clone(),
            None => return Err(no_string_form(val)),
        },
        other => return Err(no_string_form(other)),
    })
}

fn no_string_form(val: &Value) -> ExecutionError {
    err(format!(
        "string clause can only be used on strings, bools, bytes, ints, doubles, maps, lists, \
         types, durations, and timestamps, was given {}",
        val.type_of()
    ))
}

/// A list element or map value, as cel-go's `clauseForType` + `quoteForCEL`
/// write it: a valid CEL literal. Strings and bytes are Go-quoted (`"a"`,
/// `b"a"`), a double is Go's `%.6f` (an infinity or NaN quoted), a timestamp or
/// duration is wrapped as `timestamp("…")` / `duration("…")`, and a type is its
/// bare name.
fn format_nested(val: &Value, out: &mut String) -> Result<(), ExecutionError> {
    match val {
        Value::String(s) => out.push_str(&go_quote(s)),
        Value::Bytes(b) => {
            out.push('b');
            out.push_str(&go_quote(utf8_bytes(b)?));
        }
        Value::Float(f) if f.is_finite() => out.push_str(&go_fixed(*f, 6)),
        // Go's `%.6f` of ±Inf / NaN is "+Inf" / "-Inf" / "NaN", as `%g` is.
        Value::Float(f) => out.push_str(&go_quote(&go_g(*f))),
        Value::Timestamp(_) | Value::Duration(_) => {
            let name = if matches!(val, Value::Timestamp(_)) {
                "timestamp"
            } else {
                "duration"
            };
            out.push_str(name);
            out.push('(');
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
fn format_map(map: &Map, out: &mut String) -> Result<(), ExecutionError> {
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

/// Go's `%q` (`strconv.Quote`), see [`go_isprint::quote`].
fn go_quote(s: &str) -> String {
    go_isprint::quote(s.as_bytes())
}

/// Bytes as UTF-8 text, as cel-go's `ConvertToType(StringType)` requires.
fn utf8_bytes(b: &[u8]) -> Result<&str, ExecutionError> {
    std::str::from_utf8(b).map_err(|_| err("invalid UTF-8 in bytes, cannot convert to string"))
}

// ---------------------------------------------------------------------------
// Go's float formatting (strconv), on the expansion of `go_float`
// ---------------------------------------------------------------------------

/// Go's `fmt` `%.{prec}f` (a negative zero keeps its sign).
fn go_fixed(x: f64, prec: usize) -> String {
    let (mut digits, mut dp) = exact_decimal(x.abs());
    let nd = i64::from(dp) + prec as i64;
    round_go(&mut digits, &mut dp, nd);
    fmt_f(x.is_sign_negative(), &digits, dp, prec)
}

// ---------------------------------------------------------------------------
// %f / %e: golang.org/x/text's en-US number formatter
// ---------------------------------------------------------------------------

/// The argument `%f` / `%e` take in cel-go: a double, or one of the strings
/// "NaN", "Infinity" and "-Infinity" (an int is refused).
fn float_clause_arg(arg: &Value, clause: &str) -> Result<f64, ExecutionError> {
    match arg {
        Value::Float(f) => Ok(*f),
        Value::String(s) if s.as_str() == "NaN" => Ok(f64::NAN),
        Value::String(s) if s.as_str() == "Infinity" => Ok(f64::INFINITY),
        Value::String(s) if s.as_str() == "-Infinity" => Ok(f64::NEG_INFINITY),
        other => Err(err(format!(
            "{clause} clause can only be used on doubles, was given {}",
            other.type_of()
        ))),
    }
}

/// x/text parses the number in `%.{n}f` / `%{n}e` itself, and gives up on one
/// past 10,000,009 (it stops before a digit once the value so far exceeds 1e6),
/// printing this in place of the value.
const XTEXT_NO_VERB: &str = "%!(NOVERB)";

fn xtext_number_too_large(n: i64) -> bool {
    n / 10 > 1_000_000
}

/// `%f`: cel-go hands `%.{prec}f` to x/text's printer, which formats the value
/// with the en-US decimal pattern at `prec` fraction digits. The fraction
/// bounds are kept in a `u8` (at least) and an `i16` (at most), so a precision
/// of 256 shows no forced zeros and one of 32768 or more none of the bound:
/// `'%.65536f'.format([1.5])` is `2`.
fn format_fixed(arg: &Value, prec: i64) -> Result<String, ExecutionError> {
    let x = float_clause_arg(arg, "fixed-point")?;
    if xtext_number_too_large(prec) {
        return Ok(XTEXT_NO_VERB.to_owned());
    }
    if x.is_nan() {
        return Ok("NaN".to_owned());
    }
    // A negative zero is not negative here (x/text checks `x < 0`).
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if x.is_infinite() {
        out.push('∞');
        return Ok(out);
    }
    let min_frac = i64::from(prec as u8);
    let max_frac = i64::from(prec as i16);
    // Rounded by strconv (`'f'` at `max_frac`), or its shortest digits when
    // there is no bound; x/text's own rounding then has nothing left to do.
    let (digits, dp) = if max_frac >= 0 {
        let (mut digits, mut dp) = exact_decimal(x.abs());
        let nd = i64::from(dp) + max_frac;
        round_go(&mut digits, &mut dp, nd);
        (digits, dp)
    } else {
        go_shortest(x.abs())
    };
    let exp = if digits.is_empty() { 0 } else { i64::from(dp) };
    let end = if digits.is_empty() {
        min_frac
    } else {
        (digits.len() as i64).max(exp + min_frac)
    };
    // `appendDecimal`: at least one integer digit, groups of three.
    let num_frac = end - exp;
    let num_int = exp.max(0);
    let split = usize::try_from(num_int).unwrap_or(usize::MAX).min(digits.len());
    let (int_digits, frac_digits) = digits.split_at(split);
    if num_int == 0 {
        out.push('0');
    }
    for i in 0..num_int {
        let d = usize::try_from(i)
            .ok()
            .and_then(|i| int_digits.get(i))
            .copied()
            .unwrap_or(0);
        push_digit(&mut out, d);
        if needs_group_separator(num_int - i) {
            out.push(',');
        }
    }
    if num_frac > 0 {
        out.push('.');
    }
    let mut written = 0;
    for _ in 0..(-exp).max(0) {
        out.push('0');
        written += 1;
    }
    for &d in frac_digits {
        push_digit(&mut out, d);
        written += 1;
    }
    while written < num_frac {
        out.push('0');
        written += 1;
    }
    Ok(out)
}

/// x/text's `needsSep` for the en-US grouping of three: a separator after the
/// integer digit `pos` places before the decimal point.
fn needs_group_separator(pos: i64) -> bool {
    let p = pos - 1;
    p > 0 && p % 3 == 0
}

/// `%e`: cel-go hands x/text `%{prec}e` (not `%.{prec}e`), so the precision is
/// a field width, padded with spaces on the left and kept in a `u16`; `%0e`
/// reads the 0 as a flag. The en-US scientific pattern shows seven significant
/// digits whatever the precision, and a superscript exponent of two digits at
/// least: `'%.2e'.format([1500.0])` is `1.500000×10⁰³`.
fn format_scientific(arg: &Value, prec: i64) -> Result<String, ExecutionError> {
    let x = float_clause_arg(arg, "scientific")?;
    if xtext_number_too_large(prec) {
        return Ok(XTEXT_NO_VERB.to_owned());
    }
    let mut out = String::new();
    if x.is_nan() {
        out.push_str("NaN");
    } else {
        if x < 0.0 {
            out.push('-');
        }
        if x.is_infinite() {
            out.push('∞');
        } else {
            // strconv `'g'` with 7 digits, then 1 integer and 6 fraction digits.
            let (mut digits, mut dp) = exact_decimal(x.abs());
            round_go(&mut digits, &mut dp, 7);
            if digits.is_empty() {
                digits.push(0);
                dp = 1;
            }
            push_digit(&mut out, digits[0]);
            out.push('.');
            for i in 1..7 {
                push_digit(&mut out, digits.get(i).copied().unwrap_or(0));
            }
            out.push_str("×10");
            let exp = dp - 1;
            if exp < 0 {
                out.push('⁻');
            }
            const SUPERSCRIPT: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
            for d in format!("{:02}", exp.unsigned_abs()).bytes() {
                out.push(SUPERSCRIPT[usize::from(d - b'0')]);
            }
        }
    }
    let width = usize::from(prec as u16);
    let len = out.chars().count();
    if len < width {
        out.insert_str(0, &" ".repeat(width - len));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// %d / %b / %o / %x / %X: Go's fmt
// ---------------------------------------------------------------------------

/// A negative int as Go's `fmt` writes it in any base: a sign, then the
/// magnitude (`'%x'.format([-255])` is `-ff`, not two's complement).
fn signed(n: i64, magnitude: impl Fn(u64) -> String) -> String {
    if n < 0 {
        format!("-{}", magnitude(n.unsigned_abs()))
    } else {
        magnitude(n.unsigned_abs())
    }
}

fn format_decimal(arg: &Value) -> Result<String, ExecutionError> {
    match arg {
        Value::Int(n) => Ok(n.to_string()),
        Value::UInt(n) => Ok(n.to_string()),
        other => Err(err(format!(
            "decimal clause can only be used on integers, was given {}",
            other.type_of()
        ))),
    }
}

fn format_binary(arg: &Value) -> Result<String, ExecutionError> {
    match arg {
        Value::Int(n) => Ok(signed(*n, |m| format!("{m:b}"))),
        Value::UInt(n) => Ok(format!("{n:b}")),
        Value::Bool(b) => Ok(if *b { "1" } else { "0" }.to_owned()),
        other => Err(err(format!(
            "only integers and bools can be formatted as binary, was given {}",
            other.type_of()
        ))),
    }
}

fn format_octal(arg: &Value) -> Result<String, ExecutionError> {
    match arg {
        Value::Int(n) => Ok(signed(*n, |m| format!("{m:o}"))),
        Value::UInt(n) => Ok(format!("{n:o}")),
        other => Err(err(format!(
            "octal clause can only be used on integers, was given {}",
            other.type_of()
        ))),
    }
}

/// `%x` / `%X`: an int or uint in hex, a string's UTF-8 bytes or bytes as two
/// hex digits each.
fn format_hex(arg: &Value, upper: bool) -> Result<String, ExecutionError> {
    let hex = |m: u64| if upper { format!("{m:X}") } else { format!("{m:x}") };
    let bytes_hex = |b: &[u8]| -> String {
        b.iter()
            .map(|byte| {
                if upper {
                    format!("{byte:02X}")
                } else {
                    format!("{byte:02x}")
                }
            })
            .collect()
    };
    match arg {
        Value::Int(n) => Ok(signed(*n, hex)),
        Value::UInt(n) => Ok(hex(*n)),
        Value::String(s) => Ok(bytes_hex(s.as_bytes())),
        Value::Bytes(b) => Ok(bytes_hex(b)),
        other => Err(err(format!(
            "only integers, byte buffers, and strings can be formatted as hex, was given {}",
            other.type_of()
        ))),
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
    fn test_format_f_is_the_xtext_en_us_decimal() {
        // Expected strings are k8s.io/apiserver v0.36.1 `MustBaseEnvSet(1.36)`
        // output (cel-go v0.26, golang.org/x/text v0.33.0, go1.26.2).
        let cases: &[(&str, &str)] = &[
            ("'%.2f'.format([1500.0])", "1,500.00"),
            ("'%f'.format([1234567.891])", "1,234,567.891000"),
            ("'%.0f'.format([2.5])", "2"),
            ("'%.2f'.format([0.125])", "0.12"),
            ("'%.2f'.format([-0.001])", "-0.00"),
            ("'%f'.format([-0.0])", "0.000000"),
            ("'%f'.format(['NaN'])", "NaN"),
            ("'%f'.format(['-Infinity'])", "-∞"),
            ("'%f'.format([double('Infinity')])", "∞"),
            // The precision is kept in an i16 (at most) and a u8 (at least).
            ("'%.65536f'.format([1.5])", "2"),
            ("'%.10000010f'.format([1.5])", "%!(NOVERB)"),
        ];
        for (expr, want) in cases {
            assert_eq!(eval_str(expr), *want, "{expr}");
        }
        assert_eq!(eval_str("'%.40000f'.format([1.5])").len(), 66);
        // An int is not a double.
        eval_err("'%f'.format([1])");
        eval_err("'%.1f'.format([dyn(5)])");
    }

    #[test]
    fn test_format_e_is_the_xtext_en_us_scientific() {
        // Same source as above. The precision is a field width; seven
        // significant digits always.
        let cases: &[(&str, &str)] = &[
            ("'%e'.format([1500.0])", "1.500000×10⁰³"),
            ("'%.2e'.format([1500.0])", "1.500000×10⁰³"),
            ("'%.2e'.format([0.0000015])", "1.500000×10⁻⁰⁶"),
            ("'%.20e'.format([-1500.0])", "      -1.500000×10⁰³"),
            ("'%e'.format([double('NaN')])", "   NaN"),
            ("'%.0e'.format([double('NaN')])", "NaN"),
            ("'%e'.format(['Infinity'])", "     ∞"),
            ("'%e'.format([1234567.5])", "1.234568×10⁰⁶"),
            ("'%.10000010e'.format([1.5])", "%!(NOVERB)"),
        ];
        for (expr, want) in cases {
            assert_eq!(eval_str(expr), *want, "{expr}");
        }
        // `{:.65535e}` panicked inside Rust's formatter (precision + 1 wraps).
        assert_eq!(eval_str("'%.65535e'.format([1.5])").chars().count(), 65535);
    }

    #[test]
    fn test_format_precision_parses_as_int64() {
        // Verbs that ignore the precision take any int64 one, as in cel-go.
        assert_eq!(eval_str("'%.65536s'.format(['hello'])"), "hello");
        assert_eq!(eval_str("'%.9223372036854775807d'.format([12])"), "12");
        // Past int64 it is an error for every verb (Go strconv.Atoi).
        eval_err("'%.9223372036854775808s'.format(['a'])");
        eval_err("'%.f'.format([1.0])");
        eval_err("'%.5'.format([1.0])");
    }

    #[test]
    fn test_format_negative_ints_are_sign_and_magnitude() {
        assert_eq!(eval_str("'%b'.format([-5])"), "-101");
        assert_eq!(eval_str("'%o'.format([-8])"), "-10");
        assert_eq!(eval_str("'%x'.format([-255])"), "-ff");
        assert_eq!(eval_str("'%X'.format([-255])"), "-FF");
        assert_eq!(
            eval_str("'%x'.format([-9223372036854775808])"),
            "-8000000000000000"
        );
    }

    #[test]
    fn test_format_s_residuals_follow_cel_go() {
        // Go's shortest digits break a tie towards even.
        assert_eq!(
            eval_str("'%s'.format([1000000000000000.25])"),
            "1.0000000000000002e+15"
        );
        // Go 1.26.2's strconv.IsPrint (Unicode 15.0.0) escapes U+0378 and
        // U+FFFF, unassigned, and U+1C89 / U+1FAE9, assigned only later.
        assert_eq!(
            eval_str("'%s'.format([['\\u0378', '\\uffff', '\\u1c89', '\\U0001fae9']])"),
            "[\"\\u0378\", \"\\uffff\", \"\\u1c89\", \"\\U0001fae9\"]"
        );
        // A type stays a type: bare in a list or map, its name at top level.
        assert_eq!(
            eval_str("'%s'.format([[type(1), type('a'), type(null)]])"),
            "[int, string, null_type]"
        );
        assert_eq!(eval_str("'%s'.format([{'k': type(1u)}])"), "{\"k\":uint}");
        assert_eq!(eval_str("'%s'.format([type(1)])"), "int");
        assert_eq!(
            eval_str("'%s'.format([duration('1.000000001s')])"),
            "1.000000001s"
        );
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
