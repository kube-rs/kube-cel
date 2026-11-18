//! Kubernetes CEL string extension functions.
//!
//! Provides the string functions available in Kubernetes CEL expressions,
//! matching the behavior of `cel-go/ext/strings.go`.

use cel::{
    ExecutionError, ResolveResult,
    extractors::{Arguments, This},
    objects::Value,
};
use std::sync::Arc;

use crate::functions::Registrar;

/// Register all string extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("charAt", char_at);
    // indexOf/lastIndexOf are registered in lists.rs with runtime type dispatch
    // to avoid name collisions between string and list versions.
    r.add("lowerAscii", lower_ascii);
    r.add("upperAscii", upper_ascii);
    r.add("replace", string_replace);
    r.add("split", string_split);
    r.add("substring", substring);
    r.add("trim", trim);
    r.add("join", join);
    r.add("strings.quote", strings_quote);
}

/// `<string>.charAt(<int>) -> <string>`
///
/// Returns the character at the given index as a single-character string.
/// If `idx == len`, returns `""` (matching cel-go behavior).
fn char_at(This(this): This<Arc<String>>, idx: i64) -> ResolveResult {
    let chars: Vec<char> = this.chars().collect();
    // `try_from`, not `as`: on a 32-bit target `idx as usize` truncates
    // (4294967296 → 0) and would read the wrong character.
    let Some(i) = usize::try_from(idx).ok().filter(|&i| i <= chars.len()) else {
        return Err(ExecutionError::function_error(
            "charAt",
            format!("index {idx} out of range for string of length {}", chars.len()),
        ));
    };
    Ok(Value::String(Arc::new(
        chars.get(i).map(char::to_string).unwrap_or_default(),
    )))
}

/// `<string>.indexOf(<string>) -> <int>`
/// `<string>.indexOf(<string>, <int>) -> <int>`
pub(crate) fn string_index_of(This(this): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let search = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "indexOf",
                "expected string argument",
            ));
        }
    };
    let offset = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    // cel-go v0.26 (`indexOfOffset`) returns the offset as is for an empty
    // needle, before any range check: `'hello'.indexOf('', -5)` is -5.
    if search.is_empty() {
        return Ok(Value::Int(offset));
    }
    // Otherwise a negative offset errors (it does NOT clamp to 0), and one past
    // the end falls through to an empty scan → -1. A positive offset past
    // `usize` (on a 32-bit target) is past the end too.
    if offset < 0 {
        return Err(ExecutionError::function_error(
            "indexOf",
            format!("index out of range: {offset}"),
        ));
    }
    let Ok(offset) = usize::try_from(offset) else {
        return Ok(Value::Int(-1));
    };

    let chars: Vec<char> = this.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    for i in offset..chars.len() {
        if i + search_chars.len() <= chars.len() && chars[i..i + search_chars.len()] == search_chars[..] {
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
    let len = chars.len() as i64;

    // The optional second argument is the highest index allowed as the START of
    // a match (inclusive), matching cel-go's `lastIndexOf(substr, i)`: a match
    // may extend PAST that index. (The previous implementation treated it as an
    // exclusive upper bound the whole match had to fit under, which diverged
    // fail-open from the apiserver.) Without it, any start position is allowed.
    // cel-go v0.26 `lastIndexOfOffset`: an empty needle returns the offset as
    // is (even negative or past the end); otherwise a negative offset errors and
    // one at or past the length returns -1. Without an offset an empty needle is
    // found at `len`, and any start position is allowed.
    let max_start: i64 = match args.get(1) {
        Some(Value::Int(n)) if search_chars.is_empty() => return Ok(Value::Int(*n)),
        Some(Value::Int(n)) if *n < 0 => {
            return Err(ExecutionError::function_error(
                "lastIndexOf",
                format!("index out of range: {n}"),
            ));
        }
        Some(Value::Int(n)) if *n >= len => return Ok(Value::Int(-1)),
        Some(Value::Int(n)) => *n,
        _ if search_chars.is_empty() => return Ok(Value::Int(len)),
        _ => len,
    };

    // Highest start index we may scan: bounded by `max_start` and by the room
    // the search string needs to fit within the haystack.
    let scan_upper = max_start.min(len - search_chars.len() as i64);
    let mut result: i64 = -1;
    let mut i: i64 = 0;
    while i <= scan_upper {
        let s = i as usize;
        if chars[s..s + search_chars.len()] == search_chars[..] {
            result = i;
        }
        i += 1;
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

    // Go's `strings.Replace` takes a negative count as "no limit" (cel-go v0.26
    // passes it through). Narrowed with `try_from`, not `as`: a count past
    // `usize::MAX` on a 32-bit target is still "every match".
    let result = match args.get(2) {
        Some(Value::Int(n)) if *n >= 0 => this.replacen(
            from.as_str(),
            to.as_str(),
            usize::try_from(*n).unwrap_or(usize::MAX),
        ),
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

    // Go's strings.Split/SplitN treat an empty separator as "split after each
    // UTF-8 rune" (`"abc" -> [a b c]`); Rust's str::split("") instead yields
    // leading/trailing empty strings. Handle the empty separator to match cel-go.
    if separator.is_empty() {
        let chars: Vec<char> = this.chars().collect();
        let parts: Vec<Value> = match args.get(1) {
            Some(Value::Int(n)) if *n == 0 => vec![],
            // n>0 and short of the rune count: first n-1 runes, then the remainder
            // as a single element (Go SplitN semantics).
            // Compared as i64 before narrowing: on a 32-bit target `n as usize`
            // truncates (4294967296 → 0) and the slicing below would panic.
            Some(Value::Int(n)) if *n > 0 && *n < chars.len() as i64 => {
                let n = *n as usize;
                let mut v: Vec<Value> = chars[..n - 1]
                    .iter()
                    .map(|c| Value::String(Arc::new(c.to_string())))
                    .collect();
                let rest: String = chars[n - 1..].iter().collect();
                v.push(Value::String(Arc::new(rest)));
                v
            }
            // No limit, negative limit, or limit >= rune count: every rune.
            _ => chars
                .iter()
                .map(|c| Value::String(Arc::new(c.to_string())))
                .collect(),
        };
        return Ok(Value::List(Arc::new(parts)));
    }

    let parts: Vec<Value> = match args.get(1) {
        Some(Value::Int(n)) if *n == 0 => vec![],
        Some(Value::Int(n)) if *n < 0 => this
            .split(separator.as_str())
            .map(|s| Value::String(Arc::new(s.to_string())))
            .collect(),
        Some(Value::Int(n)) => this
            .splitn(usize::try_from(*n).unwrap_or(usize::MAX), separator.as_str())
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

    // Indices narrowed with `try_from`, not `as`: on a 32-bit target `as usize`
    // truncates (4294967296 → 0) and would cut the wrong range.
    let Some(begin) = usize::try_from(start).ok().filter(|&b| b <= len) else {
        return Err(ExecutionError::function_error(
            "substring",
            format!("start index {start} out of range for string of length {len}"),
        ));
    };

    let end = match args.get(1) {
        Some(Value::Int(n)) => match usize::try_from(*n).ok().filter(|&e| e >= begin && e <= len) {
            Some(e) => e,
            None => {
                return Err(ExecutionError::function_error(
                    "substring",
                    format!("end index {n} out of range"),
                ));
            }
        },
        _ => len,
    };

    let result: String = chars[begin..end].iter().collect();
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

    // cel-go's join is typed `list(string)` and errors on a non-string element
    // rather than rendering it (the old Debug format leaked Rust reprs like
    // `Int(1)` into user-visible output and was a fail-open verdict).
    let mut parts: Vec<String> = Vec::with_capacity(this.len());
    for v in this.iter() {
        match v {
            Value::String(s) => parts.push(s.to_string()),
            other => {
                return Err(ExecutionError::function_error(
                    "join",
                    format!("join: invalid input: expected string, got {:?}", other.type_of()),
                ));
            }
        }
    }

    Ok(Value::String(Arc::new(parts.join(&separator))))
}

/// `strings.quote(<string>) -> <string>`
fn strings_quote(s: Arc<String>) -> ResolveResult {
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\x07' => escaped.push_str("\\a"),
            '\x08' => escaped.push_str("\\b"),
            '\x0C' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\x0B' => escaped.push_str("\\v"),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    Ok(Value::String(Arc::new(escaped)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    #[test]
    fn test_char_at() {
        assert_eq!(eval("'hello'.charAt(0)"), Value::String(Arc::new("h".into())));
        assert_eq!(eval("'hello'.charAt(4)"), Value::String(Arc::new("o".into())));
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
        crate::functions::eval(expr).unwrap_err()
    }

    #[test]
    fn test_char_at_out_of_bounds() {
        eval_err("'hello'.charAt(-1)");
        eval_err("'hello'.charAt(6)");
        // Past u32::MAX: `as usize` truncated this to 0 on a 32-bit target.
        eval_err("'hello'.charAt(4294967296)");
    }

    #[test]
    fn test_char_at_at_length() {
        // charAt(len) returns empty string (cel-go behavior)
        assert_eq!(eval("'hello'.charAt(5)"), Value::String(Arc::new("".into())));
        assert_eq!(eval("'tacocat'.charAt(7)"), Value::String(Arc::new("".into())));
    }

    #[test]
    fn test_char_at_unicode() {
        assert_eq!(eval("'héllo'.charAt(1)"), Value::String(Arc::new("é".into())));
    }

    #[test]
    fn test_index_of_with_offset() {
        // offset past first occurrence
        assert_eq!(eval("'abcabc'.indexOf('abc', 1)"), Value::Int(3));
        // negative offset errors (cel-go: "index out of range"), not clamps to 0
        eval_err("'hello'.indexOf('h', -5)");
        // offset past end
        assert_eq!(eval("'hello'.indexOf('h', 100)"), Value::Int(-1));
    }

    #[test]
    fn test_last_index_of_with_offset() {
        // offset = last index allowed as the match START (inclusive), so the
        // occurrence beginning at 3 is found (cel-go semantics).
        assert_eq!(eval("'abcabc'.lastIndexOf('abc', 3)"), Value::Int(3));
        // empty search returns the offset
        assert_eq!(eval("'hello'.lastIndexOf('', 3)"), Value::Int(3));
        // negative offset errors (cel-go), not -1/clamp
        eval_err("'hello'.lastIndexOf('l', -1)");
    }

    #[test]
    fn index_of_offsets_follow_cel_go_v026() {
        // Each checked on kube-apiserver v1.36.1 (cel-go v0.26).
        // An offset at or past the length finds nothing for lastIndexOf.
        assert_eq!(eval("'hello'.lastIndexOf('o', 5)"), Value::Int(-1));
        assert_eq!(eval("'hello'.lastIndexOf('o', 4)"), Value::Int(4));
        // An empty needle returns the offset as is, unchecked.
        assert_eq!(eval("'hello'.indexOf('', -5)"), Value::Int(-5));
        assert_eq!(eval("'hello'.indexOf('', 100)"), Value::Int(100));
        assert_eq!(eval("'hello'.lastIndexOf('', 100)"), Value::Int(100));
        assert_eq!(eval("'hello'.lastIndexOf('', -3)"), Value::Int(-3));
        // Without an offset an empty needle is found at the length.
        assert_eq!(eval("'hello'.lastIndexOf('')"), Value::Int(5));
        eval_err("'hello'.indexOf('h', -5)");
    }

    #[test]
    fn test_split_limit_past_u32() {
        // A limit past u32::MAX keeps every part (Go SplitN with n > count). On
        // a 32-bit target `n as usize` used to truncate it and panic.
        let parts = |expr: &str| match eval(expr) {
            Value::List(l) => l.len(),
            other => panic!("{other:?}"),
        };
        assert_eq!(parts("'abc'.split('', 4294967296)"), 3);
        assert_eq!(parts("'abc'.split('', 4294967298)"), 3);
        assert_eq!(parts("'a,b,c'.split(',', 4294967296)"), 3);
    }

    #[test]
    fn test_split_empty_separator() {
        // Go strings.Split with "" splits after each rune (no leading/trailing "").
        assert_eq!(
            eval("'abc'.split('')"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("a".into())),
                Value::String(Arc::new("b".into())),
                Value::String(Arc::new("c".into())),
            ]))
        );
        // SplitN: first n-1 runes, then the remainder as one element.
        assert_eq!(
            eval("'abcde'.split('', 3)"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("a".into())),
                Value::String(Arc::new("b".into())),
                Value::String(Arc::new("cde".into())),
            ]))
        );
    }

    #[test]
    fn test_join_non_string_errors() {
        // cel-go's join is list(string): non-string elements error, not Debug-format.
        eval_err("[1, 2].join('-')");
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
        // Past u32::MAX: `as usize` truncated these on a 32-bit target.
        eval_err("'hello'.substring(4294967296)");
        eval_err("'hello'.substring(0, 4294967297)");
        eval_err("'hello'.indexOf('h', -4294967296)");
        // A positive offset past u32::MAX is past the end: -1, not an error
        // (on a 32-bit target it does not fit a usize).
        assert_eq!(eval("'abc'.indexOf('b', 4294967296)"), Value::Int(-1));
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
        // A negative count replaces every match (Go strings.Replace), as does
        // one past u32::MAX (it used to be narrowed with `as usize`).
        for n in ["-1", "-9223372036854775808", "4294967296", "9223372036854775807"] {
            assert_eq!(
                eval(&format!("'aaa'.replace('a', 'b', {n})")),
                Value::String(Arc::new("bbb".into())),
                "count {n}"
            );
        }
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

    // --- cel-go parity tests ---

    #[test]
    fn test_char_at_unicode_multi() {
        assert_eq!(eval("'©αT'.charAt(0)"), Value::String(Arc::new("©".into())));
        assert_eq!(eval("'©αT'.charAt(1)"), Value::String(Arc::new("α".into())));
        assert_eq!(eval("'©αT'.charAt(2)"), Value::String(Arc::new("T".into())));
    }

    #[test]
    fn test_index_of_unicode() {
        assert_eq!(eval("'ta©o©αT'.indexOf('©')"), Value::Int(2));
        assert_eq!(eval("'ta©o©αT'.indexOf('©', 3)"), Value::Int(4));
        assert_eq!(eval("'ta©o©αT'.indexOf('©αT', 3)"), Value::Int(4));
    }

    #[test]
    fn test_index_of_full_match() {
        assert_eq!(eval("'hello wello'.indexOf('hello wello')"), Value::Int(0));
    }

    #[test]
    fn test_index_of_not_found_longer() {
        assert_eq!(eval("'hello wello'.indexOf('elbo room!!!')"), Value::Int(-1));
    }

    #[test]
    fn test_last_index_of_unicode() {
        assert_eq!(eval("'ta©o©αT'.lastIndexOf('©')"), Value::Int(4));
        assert_eq!(eval("'ta©o©αT'.lastIndexOf('©', 3)"), Value::Int(2));
    }

    #[test]
    fn test_last_index_of_empty_string() {
        assert_eq!(eval("''.lastIndexOf('@@')"), Value::Int(-1));
        assert_eq!(eval("'tacocat'.lastIndexOf('')"), Value::Int(7));
    }

    #[test]
    fn test_last_index_of_full_match() {
        assert_eq!(eval("'hello wello'.lastIndexOf('hello wello')"), Value::Int(0));
    }

    #[test]
    fn test_last_index_of_overlapping() {
        // offset bounds the match START (inclusive): occurrences of "nana"
        // begin at 2,4,6,8; with offset 7 the last allowed start is 6.
        assert_eq!(eval("'bananananana'.lastIndexOf('nana', 7)"), Value::Int(6));
        assert_eq!(eval("'bananananana'.lastIndexOf('nana')"), Value::Int(8));
    }

    #[test]
    fn test_replace_empty_pattern() {
        assert_eq!(
            eval("'hello hello'.replace('', '_')"),
            Value::String(Arc::new("_h_e_l_l_o_ _h_e_l_l_o_".into()))
        );
    }

    #[test]
    fn test_split_limit_zero() {
        // limit 0 returns empty list (cel-go behavior)
        assert_eq!(eval("'a,b,c'.split(',', 0)"), Value::List(Arc::new(vec![])));
    }

    #[test]
    fn test_split_negative_limit() {
        // negative limit returns all splits (cel-go behavior)
        assert_eq!(
            eval("'o©o©o©o'.split('©', -1)"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("o".into())),
                Value::String(Arc::new("o".into())),
                Value::String(Arc::new("o".into())),
                Value::String(Arc::new("o".into())),
            ]))
        );
    }

    #[test]
    fn test_substring_unicode() {
        assert_eq!(
            eval("'ta©o©αT'.substring(2, 6)"),
            Value::String(Arc::new("©o©α".into()))
        );
    }

    #[test]
    fn test_substring_at_end() {
        assert_eq!(
            eval("'ta©o©αT'.substring(7, 7)"),
            Value::String(Arc::new("".into()))
        );
    }

    #[test]
    fn test_lower_ascii_non_ascii_preserved() {
        // Non-ASCII characters should not be lowercased
        assert_eq!(
            eval("'TacoCÆt'.lowerAscii()"),
            Value::String(Arc::new("tacocÆt".into()))
        );
    }

    #[test]
    fn test_upper_ascii_non_ascii_preserved() {
        // Non-ASCII characters should not be uppercased
        assert_eq!(
            eval("'tacoCαt'.upperAscii()"),
            Value::String(Arc::new("TACOCαT".into()))
        );
    }

    #[test]
    fn test_strings_quote_special_escapes() {
        // \a (bell)
        let result = strings_quote(Arc::new("\x07".into())).unwrap();
        assert_eq!(result, Value::String(Arc::new("\"\\a\"".into())));
        // \b (backspace)
        let result = strings_quote(Arc::new("\x08".into())).unwrap();
        assert_eq!(result, Value::String(Arc::new("\"\\b\"".into())));
        // \f (form feed)
        let result = strings_quote(Arc::new("\x0C".into())).unwrap();
        assert_eq!(result, Value::String(Arc::new("\"\\f\"".into())));
        // \v (vertical tab)
        let result = strings_quote(Arc::new("\x0B".into())).unwrap();
        assert_eq!(result, Value::String(Arc::new("\"\\v\"".into())));
    }

    #[test]
    fn test_strings_quote_unicode_passthrough() {
        // Unicode and emoji should pass through unescaped
        let result = strings_quote(Arc::new("завтра".into())).unwrap();
        assert_eq!(result, Value::String(Arc::new("\"завтра\"".into())));
    }

    #[test]
    fn test_strings_quote_embedded_quote() {
        let result = strings_quote(Arc::new("mid string \" quote".into())).unwrap();
        assert_eq!(
            result,
            Value::String(Arc::new("\"mid string \\\" quote\"".into()))
        );
    }
}
