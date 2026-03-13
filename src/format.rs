//! Kubernetes CEL string formatting extension function.
//!
//! Provides printf-style `format()` function,
//! matching `cel-go/ext/strings.go` format implementation.

use cel::{
    Context, ExecutionError, ResolveResult,
    extractors::This,
    objects::{Key, Value},
};
use std::sync::Arc;

/// Register the format function.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("format", format_string);
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
            let prec: usize = prec_str.parse().map_err(|_| {
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
            's' => format_s(arg, &mut result),
            'd' => format_d(arg, &mut result)?,
            'f' => format_f(arg, precision.unwrap_or(6), &mut result)?,
            'e' => format_e(arg, precision.unwrap_or(6), &mut result)?,
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

/// %s — string representation of any value.
fn format_s(val: &Value, out: &mut String) {
    match val {
        Value::String(s) => out.push_str(s),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::UInt(n) => out.push_str(&n.to_string()),
        Value::Float(f) => out.push_str(&format_float_default(*f)),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
        Value::List(list) => {
            out.push('[');
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_s_quoted(item, out);
            }
            out.push(']');
        }
        Value::Map(map) => {
            out.push('{');
            let mut first = true;
            for (key, value) in map.map.iter() {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                format_key(key, out);
                out.push_str(": ");
                format_s_quoted(value, out);
            }
            out.push('}');
        }
        other => out.push_str(&format!("{other:?}")),
    }
}

/// Format a map key.
fn format_key(key: &Key, out: &mut String) {
    match key {
        Key::String(s) => {
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        Key::Int(n) => out.push_str(&n.to_string()),
        Key::Uint(n) => out.push_str(&n.to_string()),
        Key::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    }
}

/// Like format_s but wraps strings in quotes (for nested display).
fn format_s_quoted(val: &Value, out: &mut String) {
    match val {
        Value::String(s) => {
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        _ => format_s(val, out),
    }
}

fn format_float_default(f: f64) -> String {
    if f == f.trunc() && f.is_finite() {
        // Display whole floats without trailing zeros but with at least one decimal
        let s = format!("{f:.1}");
        s
    } else {
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

/// %f — fixed-point float.
fn format_f(val: &Value, precision: usize, out: &mut String) -> Result<(), ExecutionError> {
    let f = extract_float(val, 'f')?;
    out.push_str(&format!("{f:.precision$}"));
    Ok(())
}

/// %e — scientific notation.
fn format_e(val: &Value, precision: usize, out: &mut String) -> Result<(), ExecutionError> {
    let f = extract_float(val, 'e')?;
    out.push_str(&format!("{f:.precision$e}"));
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
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
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
    fn test_format_e() {
        assert_eq!(eval_str("'val: %.2e'.format([1500.0])"), "val: 1.50e3");
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
        let mut ctx = Context::default();
        register(&mut ctx);
        let result = Program::compile("'%s %s'.format(['only_one'])")
            .unwrap()
            .execute(&ctx);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_unknown_verb() {
        let mut ctx = Context::default();
        register(&mut ctx);
        let result = Program::compile("'%z'.format([1])").unwrap().execute(&ctx);
        assert!(result.is_err());
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
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
        assert_eq!(eval_str("'%s'.format([1.0])"), "1.0");
        assert_eq!(eval_str("'%s'.format([1.5])"), "1.5");
    }

    #[test]
    fn test_format_f_int() {
        // %f should accept int as well
        assert_eq!(eval_str("'%.1f'.format([5])"), "5.0");
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
    fn test_format_b_negative_int() {
        // Rust outputs two's complement for negative i64
        assert_eq!(eval_str("'%b'.format([-5])"), format!("{:b}", -5i64));
    }

    #[test]
    fn test_format_hex_bytes() {
        // Bytes hex encoding with leading zeros preserved
        let mut ctx = Context::default();
        register(&mut ctx);
        let result = Program::compile("'%x'.format([b'\\x00\\x00AB'])")
            .unwrap()
            .execute(&ctx)
            .unwrap();
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
