//! Kubernetes CEL URL extension functions.
//!
//! Provides URL parsing and accessor functions,
//! matching `k8s.io/apiserver/pkg/cel/library/urls.go`.

use cel::{
    ExecutionError, ResolveResult,
    extractors::This,
    objects::{Key, Map, Opaque, Value},
};
use std::sync::Arc;

use crate::functions::{Registrar, go_url::GoUrl};

/// A Kubernetes CEL URL value: what Go's `url.ParseRequestURI` returns, as the
/// apiserver keeps it.
#[derive(Debug, Clone)]
pub struct KubeUrl(GoUrl);

/// The apiserver compares two URLs by their `String()`.
impl PartialEq for KubeUrl {
    fn eq(&self, other: &Self) -> bool {
        self.0.go_string() == other.0.go_string()
    }
}

impl Eq for KubeUrl {}

impl Opaque for KubeUrl {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.URL"
    }
}

/// Register all URL extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("url", parse_url);
    r.add("isURL", is_url);
    r.add("getScheme", get_scheme);
    r.add("getHost", get_host);
    r.add("getHostname", get_hostname);
    r.add("getPort", get_port);
    r.add("getEscapedPath", get_escaped_path);
    r.add("getQuery", get_query);
}

/// `url(<string>) -> URL`: checked with Go's `url.ParseRequestURI` (an absolute
/// URI or an absolute path), then parsed with `url.Parse`, which splits off a
/// `#fragment`, as the apiserver does. A string the second refuses
/// (`'//[::1/x'`, which the first takes as a path) is an error though `isURL`
/// holds for it.
fn parse_url(s: Arc<String>) -> ResolveResult {
    let invalid = |e: String| ExecutionError::function_error("url", format!("invalid URL: {e}"));
    crate::functions::go_url::parse_request_uri(&s).map_err(invalid)?;
    let parsed = crate::functions::go_url::parse_reference(&s).map_err(invalid)?;
    Ok(Value::Opaque(Arc::new(KubeUrl(parsed))))
}

/// `isURL(<string>) -> bool`: whether `url(s)` converts.
fn is_url(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(
        crate::functions::go_url::parse_request_uri(&s).is_ok(),
    ))
}

/// Helper to extract KubeUrl from an opaque Value.
fn extract_url(val: &Value) -> Result<&KubeUrl, ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeUrl>()
            .ok_or_else(|| ExecutionError::function_error("url", "expected URL type")),
        _ => Err(ExecutionError::function_error("url", "expected URL type")),
    }
}

/// A component as a CEL string. Go's unescaped components are bytes: one that
/// is not UTF-8 (`%FF` in a host) is an error rather than a different string.
///
/// The apiserver has a value there, so the error is noted as unsupported: the
/// validation engine refuses the rule even where CEL absorbs the error.
fn text(function: &str, bytes: &[u8]) -> Result<Value, ExecutionError> {
    std::str::from_utf8(bytes)
        .map(|s| Value::String(Arc::new(s.to_owned())))
        .map_err(|_| {
            let error = || ExecutionError::function_error(function, "the URL component is not valid UTF-8");
            crate::functions::note_unsupported_error(error);
            error()
        })
}

/// `<URL>.getScheme() -> string`: Go's `URL.Scheme`, lowercased; empty for a
/// path.
fn get_scheme(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    Ok(Value::String(Arc::new(url.0.scheme.clone())))
}

/// `<URL>.getHost() -> string`: Go's `URL.Host`, unescaped, with the port and
/// an IPv6 literal's brackets; empty without an authority.
fn get_host(This(this): This<Value>) -> ResolveResult {
    text("getHost", &extract_url(&this)?.0.host)
}

/// `<URL>.getHostname() -> string`: Go's `URL.Hostname()`, without the port and
/// brackets.
fn get_hostname(This(this): This<Value>) -> ResolveResult {
    text("getHostname", extract_url(&this)?.0.hostname())
}

/// `<URL>.getPort() -> string`: Go's `URL.Port()`, as written (a scheme's
/// default port is kept), empty when there is none.
fn get_port(This(this): This<Value>) -> ResolveResult {
    text("getPort", extract_url(&this)?.0.port())
}

/// `<URL>.getEscapedPath() -> string`: Go's `URL.EscapedPath()`.
fn get_escaped_path(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    Ok(Value::String(Arc::new(url.0.escaped_path())))
}

/// `<URL>.getQuery() -> map<string, list<string>>`: Go's `URL.Query()`.
fn get_query(This(this): This<Value>) -> ResolveResult {
    let url = extract_url(&this)?;
    let mut map = std::collections::HashMap::new();
    for (key, values) in url.0.query() {
        let Value::String(key) = text("getQuery", &key)? else {
            unreachable!("text makes a string")
        };
        let values = values
            .iter()
            .map(|v| text("getQuery", v))
            .collect::<Result<Vec<_>, _>>()?;
        map.insert(Key::String(key), Value::List(Arc::new(values)));
    }
    Ok(Value::Map(Map { map: Arc::new(map) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
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
        crate::functions::eval(expr).unwrap_err()
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
    fn test_accessors_follow_go_url() {
        // Expected values from the reference apiserver (go1.26.2 net/url).
        let s = |expr: &str| match eval(expr) {
            Value::String(s) => (*s).clone(),
            other => panic!("{expr}: {other:?}"),
        };
        assert_eq!(s("url('http://h:80/').getPort()"), "80");
        assert_eq!(s("url('http://h:80/').getHost()"), "h:80");
        assert_eq!(s("url('/p').getHost()"), "");
        assert_eq!(s("url('*').getEscapedPath()"), "*");
        assert_eq!(s("url('http://h/a#frag').getEscapedPath()"), "/a");
        assert_eq!(s("url('//foo/bar').getHostname()"), "foo");
        assert_eq!(s("url('HTTP://H/').getHostname()"), "H");
        assert_eq!(
            s("url('http://[fe80::1%25en0]:8080/p').getHost()"),
            "[fe80::1%en0]:8080"
        );
        assert_eq!(
            s("url('http://[fe80::1%25en0]:8080/p').getHostname()"),
            "fe80::1%en0"
        );
        assert_eq!(s("url('/a/b%2fc').getEscapedPath()"), "/a/b%2fc");
        assert_eq!(s("url('/a b').getEscapedPath()"), "/a%20b");
        assert_eq!(s("url('/café').getEscapedPath()"), "/caf%C3%A9");
        assert_eq!(
            eval("url('http://h/?q=a+b%20c&q=%2B').getQuery()['q'] == ['a b c', '+']"),
            Value::Bool(true)
        );
        assert_eq!(s("url('mailto:user@example.com').getHost()"), "");
        assert_eq!(s("url('http://h:/').getPort()"), "");
        assert_eq!(
            eval("url('/a?x=1;y=2&z=3').getQuery() == {'z': ['3']}"),
            Value::Bool(true)
        );
        // `==` compares Go's `String()`.
        assert_eq!(
            eval("url('http://H:80/') == url('http://h/')"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("url('http://h/p#a%20b') == url('http://h/p#a b')"),
            Value::Bool(true)
        );
        // isURL holds, but url.Parse refuses the authority it now sees.
        assert_eq!(eval("isURL('//[::1/x')"), Value::Bool(true));
        assert!(crate::functions::eval("url('//[::1/x')").is_err());
        // A host that is not UTF-8 once unescaped is an error.
        assert!(crate::functions::eval("url('http://h%80x/').getHost()").is_err());
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
