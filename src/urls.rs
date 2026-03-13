//! Kubernetes CEL URL extension functions.
//!
//! Provides URL parsing and accessor functions,
//! matching `k8s.io/apiserver/pkg/cel/library/urls.go`.

use cel::{
    Context, ResolveResult,
    extractors::This,
    objects::{Key, Map, Opaque, Value},
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

/// A Kubernetes CEL URL value wrapping `url::Url`.
#[derive(Debug, Clone)]
pub struct KubeUrl(Url);

impl PartialEq for KubeUrl {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for KubeUrl {}

impl Opaque for KubeUrl {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.URL"
    }
}

/// Register all URL extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("url", parse_url);
    ctx.add_function("isURL", is_url);
    ctx.add_function("getScheme", get_scheme);
    ctx.add_function("getHost", get_host);
    ctx.add_function("getHostname", get_hostname);
    ctx.add_function("getPort", get_port);
    ctx.add_function("getEscapedPath", get_escaped_path);
    ctx.add_function("getQuery", get_query);
}

/// Validates and parses a URL string.
/// Accepts absolute URIs (with scheme) and absolute paths (starting with /).
fn validate_and_parse(s: &str) -> Result<Url, String> {
    // Accept absolute paths by prepending a dummy scheme
    if s.starts_with('/') {
        let full = format!("http://localhost{s}");
        let parsed = Url::parse(&full).map_err(|e| format!("invalid URL: {e}"))?;
        return Ok(parsed);
    }

    let parsed = Url::parse(s).map_err(|e| format!("invalid URL: {e}"))?;
    Ok(parsed)
}

/// `url(<string>) -> URL`
///
/// Parses a string into a URL. The string must be an absolute URI or an absolute path.
fn parse_url(s: Arc<String>) -> ResolveResult {
    let parsed = validate_and_parse(&s).map_err(|e| cel::ExecutionError::function_error("url", e))?;
    Ok(Value::Opaque(Arc::new(KubeUrl(parsed))))
}

/// `isURL(<string>) -> bool`
///
/// Returns true if the string is a valid URL (absolute URI or absolute path).
fn is_url(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(validate_and_parse(&s).is_ok()))
}

/// Helper to extract KubeUrl from an opaque Value.
fn extract_url(val: &Value) -> Result<&KubeUrl, cel::ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeUrl>()
            .ok_or_else(|| cel::ExecutionError::function_error("url", "expected URL type")),
        _ => Err(cel::ExecutionError::function_error("url", "expected URL type")),
    }
}

/// `<URL>.getScheme() -> string`
fn get_scheme(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    // For absolute paths, the scheme is empty
    let scheme = if url.0.scheme() == "http" && url.0.host_str() == Some("localhost") {
        // This was an absolute path, no real scheme
        ""
    } else {
        url.0.scheme()
    };
    Ok(Value::String(Arc::new(scheme.to_string())))
}

/// `<URL>.getHost() -> string`
///
/// Returns host including port and IPv6 brackets.
fn get_host(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    let host = url.0.host_str().unwrap_or("");
    let result = match url.0.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Ok(Value::String(Arc::new(result)))
}

/// `<URL>.getHostname() -> string`
///
/// Returns hostname without port and without IPv6 brackets.
fn get_hostname(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    let hostname = url.0.host_str().unwrap_or("");
    // Strip IPv6 brackets if present
    let hostname = hostname
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(hostname);
    Ok(Value::String(Arc::new(hostname.to_string())))
}

/// `<URL>.getPort() -> string`
///
/// Returns port as a string. Empty string if no port specified.
fn get_port(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    let port = url.0.port().map(|p| p.to_string()).unwrap_or_default();
    Ok(Value::String(Arc::new(port)))
}

/// `<URL>.getEscapedPath() -> string`
///
/// Returns the percent-encoded path.
fn get_escaped_path(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    Ok(Value::String(Arc::new(url.0.path().to_string())))
}

/// `<URL>.getQuery() -> map<string, list<string>>`
///
/// Returns query parameters as a map of string keys to lists of string values.
fn get_query(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    let mut raw: HashMap<String, Vec<Value>> = HashMap::new();
    for (key, value) in url.0.query_pairs() {
        raw.entry(key.into_owned())
            .or_default()
            .push(Value::String(Arc::new(value.into_owned())));
    }
    let map = raw
        .into_iter()
        .map(|(k, v)| (Key::String(Arc::new(k)), Value::List(Arc::new(v))))
        .collect();
    Ok(Value::Map(Map { map: Arc::new(map) }))
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
    fn test_is_url() {
        assert_eq!(eval("isURL('https://example.com')"), Value::Bool(true));
        assert_eq!(eval("isURL('/absolute/path')"), Value::Bool(true));
        assert_eq!(eval("isURL('not a url')"), Value::Bool(false));
    }

    #[test]
    fn test_get_scheme() {
        assert_eq!(
            eval("url('https://example.com').getScheme()"),
            Value::String(Arc::new("https".into()))
        );
    }

    #[test]
    fn test_get_host() {
        assert_eq!(
            eval("url('https://example.com:8080/path').getHost()"),
            Value::String(Arc::new("example.com:8080".into()))
        );
    }

    #[test]
    fn test_get_hostname() {
        assert_eq!(
            eval("url('https://example.com:8080/path').getHostname()"),
            Value::String(Arc::new("example.com".into()))
        );
    }

    #[test]
    fn test_get_port() {
        assert_eq!(
            eval("url('https://example.com:8080').getPort()"),
            Value::String(Arc::new("8080".into()))
        );
        assert_eq!(
            eval("url('https://example.com').getPort()"),
            Value::String(Arc::new(String::new()))
        );
    }

    #[test]
    fn test_get_escaped_path() {
        assert_eq!(
            eval("url('https://example.com/my%20path').getEscapedPath()"),
            Value::String(Arc::new("/my%20path".into()))
        );
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_url_invalid() {
        eval_err("url('not a url')");
    }

    #[test]
    fn test_absolute_path_scheme() {
        // Absolute paths should have empty scheme
        assert_eq!(
            eval("url('/some/path').getScheme()"),
            Value::String(Arc::new(String::new()))
        );
    }

    #[test]
    fn test_get_host_no_port() {
        assert_eq!(
            eval("url('https://example.com/path').getHost()"),
            Value::String(Arc::new("example.com".into()))
        );
    }

    #[test]
    fn test_get_port_no_port() {
        assert_eq!(
            eval("url('https://example.com').getPort()"),
            Value::String(Arc::new(String::new()))
        );
    }

    #[test]
    fn test_get_query_multi_value() {
        let result = eval("url('https://example.com?a=1&a=2&b=3').getQuery()");
        if let Value::Map(map) = result {
            let a_key = Key::String(Arc::new("a".into()));
            let a_val = map.map.get(&a_key).unwrap();
            assert_eq!(
                *a_val,
                Value::List(Arc::new(vec![
                    Value::String(Arc::new("1".into())),
                    Value::String(Arc::new("2".into())),
                ]))
            );
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn test_get_query_no_query() {
        let result = eval("url('https://example.com/path').getQuery()");
        if let Value::Map(map) = result {
            assert!(map.map.is_empty());
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn test_different_schemes() {
        assert_eq!(
            eval("url('http://example.com').getScheme()"),
            Value::String(Arc::new("http".into()))
        );
        assert_eq!(
            eval("url('ftp://example.com').getScheme()"),
            Value::String(Arc::new("ftp".into()))
        );
    }
}
