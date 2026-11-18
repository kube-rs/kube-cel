//! Go's `net/url.ParseRequestURI` and the parts of `url.URL` the apiserver
//! reads.
//!
//! The apiserver decides `isURL(s)`, whether `url(s)` converts, and
//! `format.uri().validate(s)` with `url.ParseRequestURI` (library/urls.go,
//! library/format.go). `url(s)` then parses `s` again with `url.Parse`, which
//! splits off a `#fragment` first, and the accessors return that `url.URL`'s
//! fields and methods: `Scheme`, `Host`, `Hostname()`, `Port()`,
//! `EscapedPath()`, `Query()`, and `String()` for `==`. This is a port of those from go1.26.2,
//! the toolchain kube-apiserver v1.36.1 is built with (`src/net/url/url.go`
//! and the escaping rules of `gen_encoding_table.go`). Go strings are bytes:
//! the unescaped host, path and user info are kept as bytes, which need not be
//! UTF-8 (`%FF`).

// `named_format` uses only the validity check; the URL parts are for `urls`.
#![cfg_attr(not(feature = "urls"), allow(dead_code))]

use std::net::Ipv6Addr;

/// A URL as Go's `url.ParseRequestURI` or `url.Parse` leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoUrl {
    /// Lowercased.
    pub(crate) scheme: String,
    /// The rest of `scheme:opaque`, when no `/` follows the scheme.
    pub(crate) opaque: String,
    pub(crate) user: Option<UserInfo>,
    /// Unescaped, with its port and an IPv6 literal's brackets.
    pub(crate) host: Vec<u8>,
    /// Unescaped.
    pub(crate) path: Vec<u8>,
    /// The path as written, when that is not the default escaping of `path`.
    pub(crate) raw_path: String,
    /// `scheme:/path`, with no authority.
    pub(crate) omit_host: bool,
    /// A `?` with nothing after it.
    pub(crate) force_query: bool,
    /// As written, after the first `?`.
    pub(crate) raw_query: String,
    /// Unescaped (`url.Parse` only; a request URI keeps `#` in its path or
    /// query).
    pub(crate) fragment: Vec<u8>,
    /// The fragment as written, when that is not the default escaping.
    pub(crate) raw_fragment: String,
}

/// Go's `url.Userinfo`, unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserInfo {
    username: Vec<u8>,
    password: Option<Vec<u8>>,
}

/// Go's `url.ParseRequestURI(raw)`, or its error text.
pub(crate) fn parse_request_uri(raw: &str) -> Result<GoUrl, String> {
    parse(raw, true)
}

/// Go's `url.Parse(raw)`: the `#fragment` split off, the rest parsed as a URI
/// reference, which may also be `//host/path` with no scheme.
pub(crate) fn parse_reference(raw: &str) -> Result<GoUrl, String> {
    let (before, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    let mut url = parse(before, false)?;
    if !fragment.is_empty() {
        url.fragment = unescape(fragment, Mode::Fragment)?;
        if escape(&url.fragment, Mode::Fragment) != fragment {
            url.raw_fragment = fragment.to_owned();
        }
    }
    Ok(url)
}

/// Go's `parse(raw, viaRequest)`.
fn parse(raw: &str, via_request: bool) -> Result<GoUrl, String> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("net/url: invalid control character in URL".into());
    }
    if raw.is_empty() && via_request {
        return Err("empty url".into());
    }
    let mut url = GoUrl {
        scheme: String::new(),
        opaque: String::new(),
        user: None,
        host: Vec::new(),
        path: Vec::new(),
        raw_path: String::new(),
        omit_host: false,
        force_query: false,
        raw_query: String::new(),
        fragment: Vec::new(),
        raw_fragment: String::new(),
    };
    if raw == "*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }

    let (scheme, rest) = scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();

    let rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        url.force_query = true;
        &rest[..rest.len() - 1]
    } else {
        match rest.split_once('?') {
            Some((before, query)) => {
                url.raw_query = query.to_owned();
                before
            }
            None => rest,
        }
    };

    if !rest.starts_with('/') {
        if !url.scheme.is_empty() {
            // A rootless path after a scheme is opaque, and not checked further.
            url.opaque = rest.to_owned();
            return Ok(url);
        }
        if via_request {
            return Err("invalid URI for request".into());
        }
        if rest
            .split('/')
            .next()
            .is_some_and(|segment| segment.contains(':'))
        {
            return Err("first path segment in URL cannot contain colon".into());
        }
    }

    // A request URI has an authority only after a scheme (`//host/p` alone is
    // a path); a reference also without one, unless it starts with `///`.
    let mut path = rest;
    let authority = (!url.scheme.is_empty() || !via_request && !rest.starts_with("///"))
        .then(|| rest.strip_prefix("//"))
        .flatten();
    if let Some(after) = authority {
        let (authority, tail) = after.find('/').map_or((after, ""), |i| after.split_at(i));
        let (user, host) = authority_parts(&url.scheme, authority)?;
        url.user = user;
        url.host = host;
        path = tail;
    } else if !url.scheme.is_empty() && rest.starts_with('/') {
        url.omit_host = true;
    }
    // `setPath`: the unescaped path, and the written one when the default
    // escaping would differ.
    url.path = unescape(path, Mode::Path)?;
    if escape(&url.path, Mode::Path) != path {
        url.raw_path = path.to_owned();
    }
    Ok(url)
}

impl GoUrl {
    /// `Hostname()`: the host without a valid port and brackets.
    pub(crate) fn hostname(&self) -> &[u8] {
        split_host_port(&self.host).0
    }

    /// `Port()`: the digits after the host's last colon, if they are all digits.
    pub(crate) fn port(&self) -> &[u8] {
        split_host_port(&self.host).1
    }

    /// `EscapedPath()`: the path as written when that is a valid escaping of
    /// it, else Go's escaping of it.
    pub(crate) fn escaped_path(&self) -> String {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Mode::Path)
            && unescape(&self.raw_path, Mode::Path).is_ok_and(|p| p == self.path)
        {
            return self.raw_path.clone();
        }
        if self.path == b"*" {
            return "*".to_owned();
        }
        escape(&self.path, Mode::Path)
    }

    /// `Query()`: `ParseQuery(RawQuery)` with its error dropped. Each key's
    /// values, in order; a pair with a `;` or a bad escape is skipped, and past
    /// 10,000 parameters there are none (Go's `urlmaxqueryparams` default).
    pub(crate) fn query(&self) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
        let mut out: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
        if self.raw_query.matches('&').count() + 1 > 10_000 {
            return out;
        }
        for pair in self.raw_query.split('&') {
            if pair.contains(';') || pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (Ok(key), Ok(value)) = (
                unescape(key, Mode::QueryComponent),
                unescape(value, Mode::QueryComponent),
            ) else {
                continue;
            };
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some((_, values)) => values.push(value),
                None => out.push((key, vec![value])),
            }
        }
        out
    }

    /// `String()`, which the apiserver compares for `==`.
    pub(crate) fn go_string(&self) -> String {
        let mut buf = String::new();
        if !self.scheme.is_empty() {
            buf.push_str(&self.scheme);
            buf.push(':');
        }
        if !self.opaque.is_empty() {
            buf.push_str(&self.opaque);
        } else {
            let authority = !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some();
            let omit_empty_host = self.omit_host && self.host.is_empty() && self.user.is_none();
            if authority && !omit_empty_host {
                if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                    buf.push_str("//");
                }
                if let Some(user) = &self.user {
                    buf.push_str(&escape(&user.username, Mode::UserPassword));
                    if let Some(password) = &user.password {
                        buf.push(':');
                        buf.push_str(&escape(password, Mode::UserPassword));
                    }
                    buf.push('@');
                }
                if !self.host.is_empty() {
                    buf.push_str(&escape(&self.host, Mode::Host));
                }
            }
            let path = self.escaped_path();
            if !path.is_empty() && !path.starts_with('/') && !self.host.is_empty() {
                buf.push('/');
            }
            if buf.is_empty()
                && path
                    .split('/')
                    .next()
                    .is_some_and(|segment| segment.contains(':'))
            {
                buf.push_str("./");
            }
            buf.push_str(&path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push('?');
            buf.push_str(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            buf.push('#');
            buf.push_str(&self.escaped_fragment());
        }
        buf
    }

    /// `EscapedFragment()`, as `escaped_path` for the fragment.
    fn escaped_fragment(&self) -> String {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Mode::Fragment)
            && unescape(&self.raw_fragment, Mode::Fragment).is_ok_and(|f| f == self.fragment)
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Mode::Fragment)
    }
}

/// `splitHostPort`: a valid `:port` after the last colon is split off, then
/// brackets around what is left.
fn split_host_port(host_port: &[u8]) -> (&[u8], &[u8]) {
    let (mut host, mut port) = (host_port, &host_port[..0]);
    if let Some(colon) = host.iter().rposition(|&b| b == b':')
        && valid_optional_port(&host[colon..])
    {
        (host, port) = (&host_port[..colon], &host_port[colon + 1..]);
    }
    if host.len() >= 2 && host.starts_with(b"[") && host.ends_with(b"]") {
        host = &host[1..host.len() - 1];
    }
    (host, port)
}

/// `getScheme`: the scheme before the first `:`, if the bytes before it form one.
fn scheme(raw: &str) -> Result<(&str, &str), String> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i > 0 => {}
            b':' if i == 0 => return Err("missing protocol scheme".into()),
            b':' => return Ok((&raw[..i], &raw[i + 1..])),
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

/// `parseAuthority`: `[userinfo@]host[:port]`, split at the last `@`.
fn authority_parts(scheme: &str, authority: &str) -> Result<(Option<UserInfo>, Vec<u8>), String> {
    let (userinfo, host) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let host = parse_host(scheme, host)?;
    let Some(userinfo) = userinfo else {
        return Ok((None, host));
    };
    let valid = userinfo.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | ':'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
                    | '@'
            )
    });
    if !valid {
        return Err("net/url: invalid userinfo".into());
    }
    let user = match userinfo.split_once(':') {
        None => UserInfo {
            username: unescape(userinfo, Mode::UserPassword)?,
            password: None,
        },
        Some((username, password)) => UserInfo {
            username: unescape(username, Mode::UserPassword)?,
            password: Some(unescape(password, Mode::UserPassword)?),
        },
    };
    Ok((Some(user), host))
}

/// `parseHost`: an `[IPv6]` literal with an optional port, or a reg-name /
/// IPv4 with an optional port, unescaped.
fn parse_host(scheme: &str, host: &str) -> Result<Vec<u8>, String> {
    match host.rfind('[') {
        Some(i) if i > 0 => Err("invalid IP-literal".into()),
        Some(_) => {
            let close = host.rfind(']').ok_or("missing ']' in host")?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port.as_bytes()) {
                return Err(format!("invalid port {colon_port:?} after host"));
            }
            let colon_port = unescape(colon_port, Mode::Host)?;
            let hostname = &host[1..close];
            // `%25` starts an RFC 6874 zone, which may %-encode more than a host.
            let (addr, zone) = match hostname.find("%25") {
                Some(z) => (
                    unescape(&hostname[..z], Mode::Host)?,
                    Some(unescape(&hostname[z..], Mode::Zone)?),
                ),
                None => (unescape(hostname, Mode::Host)?, None),
            };
            // `netip.ParseAddr` on the unescaped literal must give IPv6 (an
            // IPv4-mapped address counts; plain IPv4 does not), and a zone must
            // not be empty. The address part has no escape left here: an
            // escaped ASCII byte was refused by `unescape`.
            let ipv6 = std::str::from_utf8(&addr).is_ok_and(|a| a.parse::<Ipv6Addr>().is_ok());
            if !ipv6 || zone.as_deref() == Some(b"%") {
                return Err("invalid host".into());
            }
            let mut out = b"[".to_vec();
            out.extend_from_slice(&addr);
            out.extend_from_slice(zone.as_deref().unwrap_or_default());
            out.push(b']');
            out.extend_from_slice(&colon_port);
            Ok(out)
        }
        None => {
            if let Some(first) = host.find(':') {
                // RFC 3986 allows no colon in a host. Go 1.26 enforces that for
                // http and https (GODEBUG urlstrictcolons=1, on in the apiserver)
                // and reads any other scheme's port after the last colon.
                let start = if scheme == "http" || scheme == "https" {
                    first
                } else {
                    host.rfind(':').unwrap_or(first)
                };
                let colon_port = &host[start..];
                if !valid_optional_port(colon_port.as_bytes()) {
                    return Err(format!("invalid port {colon_port:?} after host"));
                }
            }
            unescape(host, Mode::Host)
        }
    }
}

/// `validOptionalPort`: empty, or `:` followed by digits only.
fn valid_optional_port(port: &[u8]) -> bool {
    port.is_empty()
        || port
            .strip_prefix(b":")
            .is_some_and(|p| p.iter().all(u8::is_ascii_digit))
}

/// The escaping modes of Go's `encoding`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Path,
    UserPassword,
    Host,
    Zone,
    QueryComponent,
    Fragment,
}

/// `shouldEscape(c, mode)`, from Go's `gen_encoding_table.go`.
fn should_escape(c: u8, mode: Mode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if matches!(mode, Mode::Host | Mode::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => c == b'?',
            Mode::UserPassword => matches!(c, b'@' | b'/' | b'?' | b':'),
            Mode::QueryComponent => true,
            Mode::Fragment => false,
            // The host and zone keep the sub-delims above; the rest escape.
            Mode::Host | Mode::Zone => true,
        },
        // A fragment keeps these sub-delims too (but not `'`).
        b'!' | b'(' | b')' | b'*' if mode == Mode::Fragment => false,
        _ => true,
    }
}

/// Go's `unescape(s, mode)`: every `%` starts a two-hex-digit escape; a host may
/// only escape non-ASCII bytes (or `%25`) and may not hold a raw ASCII byte it
/// would escape; a zone may escape only what it could hold raw (or a space, or
/// `%25`); in a query component `+` is a space.
fn unescape(s: &str, mode: Mode) -> Result<Vec<u8>, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let bad_escape = || {
                    let end = (i + 3).min(b.len());
                    Err(format!(
                        "invalid URL escape {:?}",
                        String::from_utf8_lossy(&b[i..end])
                    ))
                };
                let (Some(&h), Some(&l)) = (b.get(i + 1), b.get(i + 2)) else {
                    return bad_escape();
                };
                if !h.is_ascii_hexdigit() || !l.is_ascii_hexdigit() {
                    return bad_escape();
                }
                let escape = &b[i..i + 3];
                let decoded = (hex(h) << 4) | hex(l);
                let refused = match mode {
                    Mode::Host => hex(h) < 8 && escape != b"%25",
                    Mode::Zone => escape != b"%25" && decoded != b' ' && should_escape(decoded, Mode::Host),
                    Mode::Path | Mode::UserPassword | Mode::QueryComponent | Mode::Fragment => false,
                };
                if refused {
                    return Err(format!(
                        "invalid URL escape {:?}",
                        String::from_utf8_lossy(escape)
                    ));
                }
                out.push(decoded);
                i += 3;
            }
            b'+' => {
                out.push(if mode == Mode::QueryComponent { b' ' } else { b'+' });
                i += 1;
            }
            c => {
                if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && should_escape(c, mode) {
                    return Err(format!("invalid character {:?} in host name", c as char));
                }
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Go's `escape(s, mode)`: a byte `should_escape` names becomes `%XX` (upper
/// case), a space in a query component `+`.
fn escape(s: &[u8], mode: Mode) -> String {
    const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &c in s {
        if c == b' ' && mode == Mode::QueryComponent {
            out.push('+');
        } else if should_escape(c, mode) {
            out.push('%');
            out.push(char::from(UPPER_HEX[usize::from(c >> 4)]));
            out.push(char::from(UPPER_HEX[usize::from(c & 15)]));
        } else {
            out.push(char::from(c));
        }
    }
    out
}

/// `validEncoded(s, mode)`: nothing in `s` would be escaped, sub-delims, `:`,
/// `@`, brackets and `%` aside.
fn valid_encoded(s: &str, mode: Mode) -> bool {
    s.bytes().all(|c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_request_uri;

    // Every row was run through go1.26.2 `url.ParseRequestURI`, and the live
    // apiserver (kind v1.36.1) agreed on each via `isURL`.
    const ACCEPTED: &[&str] = &[
        "foo://a:1:2/",
        "http://[fe80::1%25en0]:8080/",
        "http://[::ffff:1.2.3.4]/",
        "http://exa%C3%A9mple.com/",
        "mailto:%zz",
        "http:///foo",
        "//foo/bar",
        "*",
        "http://user@name@host/",
        "http://h/p?%zz",
        "http://h/p#frag",
        "http://[fe80::1%25en%200]/",
        "/absolute/path",
        "https://example.com",
        "urn:isbn:0451450523",
    ];
    const REFUSED: &[&str] = &[
        "",
        "http://a:1:2/",
        "https://a:1:2/",
        "/foo%zz",
        "/foo\n",
        "/foo\t",
        " http://example.com",
        "http://[1.2.3.4]/",
        "http://ex ample.com/",
        "http://exa%41mple.com/",
        "http://us er@host/",
        "http://h:abc/",
        "http://[fe80::1%25]/",
        "no-scheme",
        "://missing-scheme",
        "/%Gé",
    ];

    #[test]
    fn matches_go_parse_request_uri() {
        for s in ACCEPTED {
            assert!(
                parse_request_uri(s).is_ok(),
                "Go accepts {s:?}: {:?}",
                parse_request_uri(s)
            );
        }
        for s in REFUSED {
            assert!(parse_request_uri(s).is_err(), "Go refuses {s:?}");
        }
    }
}
