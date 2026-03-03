//! Kubernetes CEL string extension functions.
//!
//! Provides the string functions available in Kubernetes CEL expressions,
//! matching the behavior of `cel-go/ext/strings.go`.

use cel::extractors::{Arguments, This};
use cel::objects::Value;
use cel::{Context, ExecutionError, ResolveResult};
use std::sync::Arc;

/// Register all string extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("charAt", char_at);
    // indexOf/lastIndexOf are registered in lists.rs with runtime type dispatch
    // to avoid name collisions between string and list versions.
    ctx.add_function("lowerAscii", lower_ascii);
    ctx.add_function("upperAscii", upper_ascii);
    ctx.add_function("replace", string_replace);
    ctx.add_function("split", string_split);
    ctx.add_function("substring", substring);
    ctx.add_function("trim", trim);
    ctx.add_function("join", join);
    ctx.add_function("strings.quote", strings_quote);
}

/// `<string>.charAt(<int>) -> <string>`
fn char_at(This(this): This<Arc<String>>, idx: i64) -> ResolveResult {
    let chars: Vec<char> = this.chars().collect();
    if idx < 0 || idx as usize >= chars.len() {
        return Err(ExecutionError::function_error(
            "charAt",
            format!(
                "index {idx} out of range for string of length {}",
                chars.len()
            ),
        ));
    }
    Ok(Value::String(Arc::new(chars[idx as usize].to_string())))
}

/// `<string>.indexOf(<string>) -> <int>`
/// `<string>.indexOf(<string>, <int>) -> <int>`
pub(crate) fn string_index_of(
    This(this): This<Arc<String>>,
    Arguments(args): Arguments,
) -> ResolveResult {
    let search = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "indexOf",
                "expected string argument",
            ));
        }
    };
    let offset: usize = match args.get(1) {
        Some(Value::Int(n)) => (*n).max(0) as usize,
        _ => 0,
    };

    let chars: Vec<char> = this.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    if search_chars.is_empty() {
        return Ok(Value::Int(offset as i64));
    }

    for i in offset..chars.len() {
        if i + search_chars.len() <= chars.len()
            && chars[i..i + search_chars.len()] == search_chars[..]
        {
            return Ok(Value::Int(i as i64));
        }
    }
    Ok(Value::Int(-1))
}

/// `<string>.lastIndexOf(<string>) -> <int>`
/// `<string>.lastIndexOf(<string>, <int>) -> <int>`
pub(crate) fn string_last_index_of(
    This(this): This<Arc<String>>,
    Arguments(args): Arguments,
) -> ResolveResult {
    let search = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "lastIndexOf",
                "expected string argument",
            ));
        }
    };

    let chars: Vec<char> = this.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    let end: usize = match args.get(1) {
        Some(Value::Int(n)) => ((*n).max(0) as usize).min(chars.len()),
        _ => chars.len(),
    };

    if search_chars.is_empty() {
        return Ok(Value::Int(end as i64));
    }

    let mut result: i64 = -1;
    for i in 0..end {
        if i + search_chars.len() <= end && chars[i..i + search_chars.len()] == search_chars[..] {
            result = i as i64;
        }
    }
    Ok(Value::Int(result))
}

/// `<string>.lowerAscii() -> <string>`
fn lower_ascii(This(this): This<Arc<String>>) -> ResolveResult {
    Ok(Value::String(Arc::new(this.to_ascii_lowercase())))
}

/// `<string>.upperAscii() -> <string>`
fn upper_ascii(This(this): This<Arc<String>>) -> ResolveResult {
    Ok(Value::String(Arc::new(this.to_ascii_uppercase())))
}

/// `<string>.replace(<string>, <string>) -> <string>`
/// `<string>.replace(<string>, <string>, <int>) -> <string>`
fn string_replace(This(this): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let from = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "replace",
                "expected string argument",
            ));
        }
    };
    let to = match args.get(1) {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "replace",
                "expected string argument",
            ));
        }
    };

    let result = match args.get(2) {
        Some(Value::Int(n)) => this.replacen(from.as_str(), to.as_str(), (*n).max(0) as usize),
        _ => this.replace(from.as_str(), to.as_str()),
    };
    Ok(Value::String(Arc::new(result)))
}

/// `<string>.split(<string>) -> <list<string>>`
/// `<string>.split(<string>, <int>) -> <list<string>>`
fn string_split(This(this): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let separator = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "split",
                "expected string argument",
            ));
        }
    };

    let parts: Vec<Value> = match args.get(1) {
        Some(Value::Int(n)) => this
            .splitn((*n).max(1) as usize, separator.as_str())
            .map(|s| Value::String(Arc::new(s.to_string())))
            .collect(),
        _ => this
            .split(separator.as_str())
            .map(|s| Value::String(Arc::new(s.to_string())))
            .collect(),
    };
    Ok(Value::List(Arc::new(parts)))
}

/// `<string>.substring(<int>) -> <string>`
/// `<string>.substring(<int>, <int>) -> <string>`
fn substring(This(this): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let start = match args.first() {
        Some(Value::Int(n)) => *n,
        _ => {
            return Err(ExecutionError::function_error(
                "substring",
                "expected int argument",
            ));
        }
    };

    let chars: Vec<char> = this.chars().collect();
    let len = chars.len();

    if start < 0 || start as usize > len {
        return Err(ExecutionError::function_error(
            "substring",
            format!("start index {start} out of range for string of length {len}"),
        ));
    }

    let end = match args.get(1) {
        Some(Value::Int(n)) => {
            if *n < start || *n as usize > len {
                return Err(ExecutionError::function_error(
                    "substring",
                    format!("end index {n} out of range"),
                ));
            }
            *n as usize
        }
        _ => len,
    };

    let result: String = chars[start as usize..end].iter().collect();
    Ok(Value::String(Arc::new(result)))
}

/// `<string>.trim() -> <string>`
fn trim(This(this): This<Arc<String>>) -> ResolveResult {
    Ok(Value::String(Arc::new(this.trim().to_string())))
}

/// `<list<string>>.join() -> <string>`
/// `<list<string>>.join(<string>) -> <string>`
fn join(This(this): This<Arc<Vec<Value>>>, Arguments(args): Arguments) -> ResolveResult {
    let separator = match args.first() {
        Some(Value::String(s)) => s.to_string(),
        _ => String::new(),
    };

    let parts: Vec<String> = this
        .iter()
        .map(|v| match v {
            Value::String(s) => s.to_string(),
            other => format!("{other:?}"),
        })
        .collect();

    Ok(Value::String(Arc::new(parts.join(&separator))))
}

/// `<string>.reverse() -> <string>`
///
/// Returns a new string with the characters in reverse order.
pub(crate) fn string_reverse(This(this): This<Arc<String>>) -> ResolveResult {
    let reversed: String = this.chars().rev().collect();
    Ok(Value::String(Arc::new(reversed)))
}

/// `strings.quote(<string>) -> <string>`
fn strings_quote(s: Arc<String>) -> ResolveResult {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    Ok(Value::String(Arc::new(format!("\"{escaped}\""))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        // indexOf/lastIndexOf registered via dispatch
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    fn test_char_at() {
        assert_eq!(
            eval("'hello'.charAt(0)"),
            Value::String(Arc::new("h".into()))
        );
        assert_eq!(
            eval("'hello'.charAt(4)"),
            Value::String(Arc::new("o".into()))
        );
    }

    #[test]
    fn test_index_of() {
        assert_eq!(eval("'hello world'.indexOf('world')"), Value::Int(6));
        assert_eq!(eval("'hello'.indexOf('x')"), Value::Int(-1));
        assert_eq!(eval("'hello'.indexOf('')"), Value::Int(0));
    }

    #[test]
    fn test_last_index_of() {
        assert_eq!(eval("'abcabc'.lastIndexOf('abc')"), Value::Int(3));
        assert_eq!(eval("'hello'.lastIndexOf('x')"), Value::Int(-1));
    }

    #[test]
    fn test_lower_upper_ascii() {
        assert_eq!(
            eval("'Hello World'.lowerAscii()"),
            Value::String(Arc::new("hello world".into()))
        );
        assert_eq!(
            eval("'Hello World'.upperAscii()"),
            Value::String(Arc::new("HELLO WORLD".into()))
        );
    }

    #[test]
    fn test_trim() {
        assert_eq!(
            eval("'  hello  '.trim()"),
            Value::String(Arc::new("hello".into()))
        );
    }

    #[test]
    fn test_split() {
        assert_eq!(
            eval("'a,b,c'.split(',')"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("a".into())),
                Value::String(Arc::new("b".into())),
                Value::String(Arc::new("c".into())),
            ]))
        );
    }

    #[test]
    fn test_join() {
        assert_eq!(
            eval("['a', 'b', 'c'].join('-')"),
            Value::String(Arc::new("a-b-c".into()))
        );
    }

    #[test]
    fn test_replace() {
        assert_eq!(
            eval("'hello world'.replace('world', 'CEL')"),
            Value::String(Arc::new("hello CEL".into()))
        );
    }

    #[test]
    fn test_substring() {
        assert_eq!(
            eval("'hello'.substring(1)"),
            Value::String(Arc::new("ello".into()))
        );
    }

    #[test]
    fn test_strings_quote() {
        assert_eq!(
            eval("strings.quote('hello')"),
            Value::String(Arc::new("\"hello\"".into()))
        );
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_char_at_out_of_bounds() {
        eval_err("'hello'.charAt(-1)");
        eval_err("'hello'.charAt(5)");
    }

    #[test]
    fn test_char_at_unicode() {
        assert_eq!(
            eval("'héllo'.charAt(1)"),
            Value::String(Arc::new("é".into()))
        );
    }

    #[test]
    fn test_index_of_with_offset() {
        // offset past first occurrence
        assert_eq!(eval("'abcabc'.indexOf('abc', 1)"), Value::Int(3));
        // negative offset clamps to 0
        assert_eq!(eval("'hello'.indexOf('h', -5)"), Value::Int(0));
        // offset past end
        assert_eq!(eval("'hello'.indexOf('h', 100)"), Value::Int(-1));
    }

    #[test]
    fn test_last_index_of_with_offset() {
        assert_eq!(eval("'abcabc'.lastIndexOf('abc', 3)"), Value::Int(0));
        // empty search returns the offset
        assert_eq!(eval("'hello'.lastIndexOf('', 3)"), Value::Int(3));
    }

    #[test]
    fn test_substring_two_args() {
        assert_eq!(
            eval("'hello'.substring(1, 3)"),
            Value::String(Arc::new("el".into()))
        );
    }

    #[test]
    fn test_substring_errors() {
        eval_err("'hello'.substring(-1)");
        eval_err("'hello'.substring(10)");
        eval_err("'hello'.substring(3, 2)"); // end < start
        eval_err("'hello'.substring(0, 10)"); // end > len
    }

    #[test]
    fn test_replace_with_count() {
        assert_eq!(
            eval("'aaa'.replace('a', 'b', 2)"),
            Value::String(Arc::new("bba".into()))
        );
        // count 0 replaces nothing
        assert_eq!(
            eval("'aaa'.replace('a', 'b', 0)"),
            Value::String(Arc::new("aaa".into()))
        );
    }

    #[test]
    fn test_split_with_limit() {
        assert_eq!(
            eval("'a,b,c'.split(',', 2)"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("a".into())),
                Value::String(Arc::new("b,c".into())),
            ]))
        );
    }

    #[test]
    fn test_join_no_separator() {
        assert_eq!(
            eval("['a', 'b', 'c'].join()"),
            Value::String(Arc::new("abc".into()))
        );
    }

    #[test]
    fn test_string_reverse() {
        assert_eq!(
            eval("'hello'.reverse()"),
            Value::String(Arc::new("olleh".into()))
        );
        assert_eq!(eval("''.reverse()"), Value::String(Arc::new("".into())));
        assert_eq!(eval("'a'.reverse()"), Value::String(Arc::new("a".into())));
    }

    #[test]
    fn test_strings_quote_escapes() {
        assert_eq!(
            eval("strings.quote('a\\nb')"),
            Value::String(Arc::new("\"a\\nb\"".into()))
        );
        assert_eq!(
            eval("strings.quote('a\\tb')"),
            Value::String(Arc::new("\"a\\tb\"".into()))
        );
    }
}
