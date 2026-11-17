//! Go's `net/url.ParseRequestURI`, as a validity check.
//!
//! The apiserver decides `isURL(s)`, whether `url(s)` converts, and
//! `format.uri().validate(s)` with `url.ParseRequestURI` (library/urls.go,
//! library/format.go). This is a port of that function's acceptance rules from
//! go1.26.2, the toolchain kube-apiserver v1.36.1 is built with: what it
//! accepts and refuses, not the parsed URL. The `urls` accessors still read a
//! WHATWG parse of the accepted string.

use std::net::Ipv6Addr;

/// `Ok` when Go's `url.ParseRequestURI(raw)` succeeds, else its error text.
pub(crate) fn parse_request_uri(raw: &str) -> Result<(), String> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("net/url: invalid control character in URL".into());
    }
    if raw.is_empty() {
        return Err("empty url".into());
    }
    if raw == "*" {
        return Ok(());
    }

    let (scheme, rest) = scheme(raw)?;
    let scheme = scheme.to_ascii_lowercase();

    // The query is split off and never checked.
    let rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        &rest[..rest.len() - 1]
    } else {
        rest.split_once('?').map_or(rest, |(before, _)| before)
    };

    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // A rootless path after a scheme is opaque, and not checked further.
            return Ok(());
        }
        return Err("invalid URI for request".into());
    }

    // Only a URI with a scheme has an authority; `//host/p` alone is a path.
    let mut path = rest;
    if !scheme.is_empty()
        && let Some(after) = rest.strip_prefix("//")
    {
        let (authority, tail) = after.find('/').map_or((after, ""), |i| after.split_at(i));
        authority_ok(&scheme, authority)?;
        path = tail;
    }
    unescape_ok(path, Mode::Path)
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
fn authority_ok(scheme: &str, authority: &str) -> Result<(), String> {
    let (userinfo, host) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    host_ok(scheme, host)?;
    if let Some(userinfo) = userinfo {
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
        unescape_ok(userinfo, Mode::UserPassword)?;
    }
    Ok(())
}

/// `parseHost`: an `[IPv6]` literal with an optional port, or a reg-name /
/// IPv4 with an optional port.
fn host_ok(scheme: &str, host: &str) -> Result<(), String> {
    match host.rfind('[') {
        Some(i) if i > 0 => Err("invalid IP-literal".into()),
        Some(_) => {
            let close = host.rfind(']').ok_or("missing ']' in host")?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return Err(format!("invalid port {colon_port:?} after host"));
            }
            let hostname = &host[1..close];
            // `%25` starts an RFC 6874 zone, which may %-encode more than a host.
            let (addr, zone) = match hostname.find("%25") {
                Some(z) => {
                    unescape_ok(&hostname[..z], Mode::Host)?;
                    unescape_ok(&hostname[z..], Mode::Zone)?;
                    (&hostname[..z], Some(&hostname[z + 3..]))
                }
                None => {
                    unescape_ok(hostname, Mode::Host)?;
                    (hostname, None)
                }
            };
            // `netip.ParseAddr` on the unescaped literal must give IPv6 (an
            // IPv4-mapped address counts; plain IPv4 does not), and a zone must
            // not be empty. Only the address part is parsed here: it has no
            // escapes left, since an escaped ASCII byte failed `unescape_ok`.
            if addr.parse::<Ipv6Addr>().is_err() || zone == Some("") {
                return Err("invalid host".into());
            }
            Ok(())
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
                if !valid_optional_port(colon_port) {
                    return Err(format!("invalid port {colon_port:?} after host"));
                }
            }
            unescape_ok(host, Mode::Host)
        }
    }
}

/// `validOptionalPort`: empty, or `:` followed by digits only.
fn valid_optional_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Path,
    UserPassword,
    Host,
    Zone,
}

/// The ASCII bytes Go writes unescaped in a host (`shouldEscape(c, encodeHost)`
/// is false): alphanumerics, sub-delims, `:[]<>"` and the unreserved marks.
fn host_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
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
                | b'-'
                | b'_'
                | b'.'
                | b'~'
        )
}

/// The checks Go's `unescape(s, mode)` makes before decoding: every `%` starts a
/// two-hex-digit escape; a host may only escape non-ASCII bytes (or `%25`) and
/// may not hold a raw ASCII byte outside [`host_byte`]; a zone may escape only
/// what it could hold raw (or a space, or `%25`).
fn unescape_ok(s: &str, mode: Mode) -> Result<(), String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
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
                Mode::Zone => escape != b"%25" && decoded != b' ' && !(decoded < 0x80 && host_byte(decoded)),
                Mode::Path | Mode::UserPassword => false,
            };
            if refused {
                return Err(format!(
                    "invalid URL escape {:?}",
                    String::from_utf8_lossy(escape)
                ));
            }
            i += 3;
        } else {
            if matches!(mode, Mode::Host | Mode::Zone) && b[i] < 0x80 && !host_byte(b[i]) {
                return Err(format!("invalid character {:?} in host name", b[i] as char));
            }
            i += 1;
        }
    }
    Ok(())
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
