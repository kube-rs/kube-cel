//! Kubernetes CEL regex extension functions.
//!
//! Provides `find` and `findAll` regex functions,
//! matching `k8s.io/apiserver/pkg/cel/library/regex.go`.

use cel::extractors::{Arguments, This};
use cel::objects::Value;
use cel::{Context, ExecutionError, ResolveResult};
use regex::Regex;
use std::sync::Arc;

/// Register all regex extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("find", find);
    ctx.add_function("findAll", find_all);
}

/// `<string>.find(<string>) -> <string>`
fn find(This(this): This<Arc<String>>, pattern: Arc<String>) -> ResolveResult {
    let re = Regex::new(&pattern)
        .map_err(|e| ExecutionError::function_error("find", format!("invalid regex: {e}")))?;
    let result = re
        .find(&this)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    Ok(Value::String(Arc::new(result)))
}

/// `<string>.findAll(<string>) -> <list<string>>`
/// `<string>.findAll(<string>, <int>) -> <list<string>>`
fn find_all(This(this): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let pattern = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ExecutionError::function_error(
                "findAll",
                "expected string pattern",
            ));
        }
    };

    let re = Regex::new(&pattern)
        .map_err(|e| ExecutionError::function_error("findAll", format!("invalid regex: {e}")))?;

    let limit = match args.get(1) {
        Some(Value::Int(n)) => Some(*n as usize),
        _ => None,
    };

    let matches: Vec<Value> = match limit {
        Some(n) => re
            .find_iter(&this)
            .take(n)
            .map(|m| Value::String(Arc::new(m.as_str().to_string())))
            .collect(),
        None => re
            .find_iter(&this)
            .map(|m| Value::String(Arc::new(m.as_str().to_string())))
            .collect(),
    };

    Ok(Value::List(Arc::new(matches)))
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

    #[test]
    fn test_find() {
        assert_eq!(
            eval("'hello world'.find('[a-z]+')"),
            Value::String(Arc::new("hello".into()))
        );
        assert_eq!(
            eval("'12345'.find('[a-z]+')"),
            Value::String(Arc::new(String::new()))
        );
    }

    #[test]
    fn test_find_all() {
        assert_eq!(
            eval("'hello world'.findAll('[a-z]+')"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("hello".into())),
                Value::String(Arc::new("world".into())),
            ]))
        );
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_find_invalid_regex() {
        eval_err("'hello'.find('[')");
    }

    #[test]
    fn test_find_all_invalid_regex() {
        eval_err("'hello'.findAll('[')");
    }

    #[test]
    fn test_find_all_with_limit() {
        assert_eq!(
            eval("'a1b2c3'.findAll('[a-z]', 2)"),
            Value::List(Arc::new(vec![
                Value::String(Arc::new("a".into())),
                Value::String(Arc::new("b".into())),
            ]))
        );
    }

    #[test]
    fn test_find_all_no_matches() {
        assert_eq!(
            eval("'12345'.findAll('[a-z]+')"),
            Value::List(Arc::new(vec![]))
        );
    }

    #[test]
    fn test_find_all_limit_zero() {
        assert_eq!(
            eval("'hello'.findAll('[a-z]+', 0)"),
            Value::List(Arc::new(vec![]))
        );
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_find_full_match() {
        assert_eq!(
            eval("'hello'.find('hello')"),
            Value::String(Arc::new("hello".into()))
        );
    }

    #[test]
    fn test_find_empty_string() {
        assert_eq!(
            eval("''.find('[a-z]+')"),
            Value::String(Arc::new(String::new()))
        );
    }

    #[test]
    fn test_find_all_limit_exceeds_matches() {
        assert_eq!(
            eval("'hello'.findAll('[a-z]+', 100)"),
            Value::List(Arc::new(vec![Value::String(Arc::new("hello".into()))]))
        );
    }
}
