//! Kubernetes CEL IP and CIDR extension functions.
//!
//! Provides IP address and CIDR functions,
//! matching `k8s.io/apiserver/pkg/cel/library/ip.go` and `cidr.go`.

use cel::extractors::This;
use cel::objects::{Opaque, Value};
use cel::{Context, ResolveResult};
use ipnet::IpNet;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

// --- Custom CEL types ---

/// A Kubernetes CEL IP address value.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct KubeIP(pub(crate) IpAddr);

impl KubeIP {
    pub(crate) fn new(addr: IpAddr) -> Self {
        Self(addr)
    }
}

impl Opaque for KubeIP {
    fn runtime_type_name(&self) -> &str {
        "net.IP"
    }
}

/// A Kubernetes CEL CIDR value.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct KubeCIDR(IpNet);

impl Opaque for KubeCIDR {
    fn runtime_type_name(&self) -> &str {
        "net.CIDR"
    }
}

/// Register all IP and CIDR extension functions.
pub fn register(ctx: &mut Context<'_>) {
    // IP functions
    ctx.add_function("ip", parse_ip);
    ctx.add_function("isIP", is_ip);
    ctx.add_function("ip.isCanonical", ip_is_canonical);
    ctx.add_function("family", ip_family);
    ctx.add_function("isLoopback", ip_is_loopback);
    ctx.add_function("isUnspecified", ip_is_unspecified);
    ctx.add_function("isLinkLocalMulticast", ip_is_link_local_multicast);
    ctx.add_function("isLinkLocalUnicast", ip_is_link_local_unicast);
    ctx.add_function("isGlobalUnicast", ip_is_global_unicast);

    // IP version convenience functions
    ctx.add_function("isIPv4", is_ipv4);
    ctx.add_function("isIPv6", is_ipv6);

    // CIDR functions
    ctx.add_function("cidr", parse_cidr);
    ctx.add_function("isCIDR", is_cidr);
    ctx.add_function("containsIP", cidr_contains_ip);
    ctx.add_function("containsCIDR", cidr_contains_cidr);
    ctx.add_function("prefixLength", cidr_prefix_length);
    ctx.add_function("masked", cidr_masked);

    // CIDR version convenience functions
    ctx.add_function("isCIDRv4", is_cidr_v4);
    ctx.add_function("isCIDRv6", is_cidr_v6);
}

// --- Parsing helpers ---

/// Parse an IP address string, rejecting IPv4-mapped IPv6 and zone IDs.
pub(crate) fn parse_ip_addr(s: &str) -> Result<IpAddr, String> {
    // Reject zone identifiers (e.g., fe80::1%eth0)
    if s.contains('%') {
        return Err("IP address with zone is not allowed".into());
    }

    let addr = IpAddr::from_str(s).map_err(|e| format!("invalid IP address: {e}"))?;

    // Reject IPv4-mapped IPv6 (e.g., ::ffff:1.2.3.4)
    if let IpAddr::V6(v6) = addr
        && v6.to_ipv4_mapped().is_some()
    {
        return Err("IPv4-mapped IPv6 addresses are not allowed".into());
    }

    Ok(addr)
}

fn parse_cidr_net(s: &str) -> Result<IpNet, String> {
    let net = IpNet::from_str(s).map_err(|e| format!("invalid CIDR: {e}"))?;

    // Reject IPv4-mapped IPv6 in CIDR
    if let IpAddr::V6(v6) = net.addr()
        && v6.to_ipv4_mapped().is_some()
    {
        return Err("IPv4-mapped IPv6 in CIDR is not allowed".into());
    }

    Ok(net)
}

// --- IP functions ---

fn extract_ip(val: &Value) -> Result<&KubeIP, cel::ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeIP>()
            .ok_or_else(|| cel::ExecutionError::function_error("ip", "expected IP type")),
        _ => Err(cel::ExecutionError::function_error(
            "ip",
            "expected IP type",
        )),
    }
}

fn extract_cidr(val: &Value) -> Result<&KubeCIDR, cel::ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeCIDR>()
            .ok_or_else(|| cel::ExecutionError::function_error("cidr", "expected CIDR type")),
        _ => Err(cel::ExecutionError::function_error(
            "cidr",
            "expected CIDR type",
        )),
    }
}

/// `ip(<string>) -> IP`
fn parse_ip(s: Arc<String>) -> ResolveResult {
    let addr = parse_ip_addr(&s).map_err(|e| cel::ExecutionError::function_error("ip", e))?;
    Ok(Value::Opaque(Arc::new(KubeIP(addr))))
}

/// `isIP(<string>) -> bool`
fn is_ip(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(parse_ip_addr(&s).is_ok()))
}

/// `ip.isCanonical(<string>) -> bool`
///
/// Returns true if the string is the canonical form of an IP address.
fn ip_is_canonical(s: Arc<String>) -> ResolveResult {
    Ok(match parse_ip_addr(&s) {
        Ok(addr) => Value::Bool(addr.to_string() == s.as_str()),
        Err(_) => Value::Bool(false),
    })
}

/// `<IP>.family() -> int`
///
/// Returns 4 for IPv4, 6 for IPv6.
fn ip_family(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    Ok(Value::Int(if ip.0.is_ipv4() { 4 } else { 6 }))
}

/// `<IP>.isLoopback() -> bool`
fn ip_is_loopback(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    Ok(Value::Bool(ip.0.is_loopback()))
}

/// `<IP>.isUnspecified() -> bool`
fn ip_is_unspecified(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    Ok(Value::Bool(ip.0.is_unspecified()))
}

/// `<IP>.isLinkLocalMulticast() -> bool`
fn ip_is_link_local_multicast(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    let result = match ip.0 {
        IpAddr::V4(v4) => v4.octets()[0] == 224 && v4.octets()[1] == 0 && v4.octets()[2] == 0,
        IpAddr::V6(v6) => v6.segments()[0] & 0xff0f == 0xff02,
    };
    Ok(Value::Bool(result))
}

/// `<IP>.isLinkLocalUnicast() -> bool`
fn ip_is_link_local_unicast(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    let result = match ip.0 {
        IpAddr::V4(v4) => v4.octets()[0] == 169 && v4.octets()[1] == 254,
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    };
    Ok(Value::Bool(result))
}

/// `<IP>.isGlobalUnicast() -> bool`
fn ip_is_global_unicast(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    let result = !ip.0.is_loopback()
        && !ip.0.is_unspecified()
        && !ip.0.is_multicast()
        && !match ip.0 {
            IpAddr::V4(v4) => v4.is_link_local() || v4.is_broadcast(),
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
        };
    Ok(Value::Bool(result))
}

// --- CIDR functions ---

/// `cidr(<string>) -> CIDR`
fn parse_cidr(s: Arc<String>) -> ResolveResult {
    let net = parse_cidr_net(&s).map_err(|e| cel::ExecutionError::function_error("cidr", e))?;
    Ok(Value::Opaque(Arc::new(KubeCIDR(net))))
}

/// `isCIDR(<string>) -> bool`
fn is_cidr(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(parse_cidr_net(&s).is_ok()))
}

/// `<CIDR>.containsIP(<string|IP>) -> bool`
fn cidr_contains_ip(This(this): This<Value>, arg: Value) -> ResolveResult {
    let cidr = extract_cidr(&this)?;
    let ip = match &arg {
        Value::String(s) => {
            parse_ip_addr(s).map_err(|e| cel::ExecutionError::function_error("containsIP", e))?
        }
        Value::Opaque(o) => {
            let kip = o.downcast_ref::<KubeIP>().ok_or_else(|| {
                cel::ExecutionError::function_error("containsIP", "expected IP or string")
            })?;
            kip.0
        }
        _ => {
            return Err(cel::ExecutionError::function_error(
                "containsIP",
                "expected IP or string argument",
            ));
        }
    };
    Ok(Value::Bool(cidr.0.contains(&ip)))
}

/// `<CIDR>.containsCIDR(<string|CIDR>) -> bool`
fn cidr_contains_cidr(This(this): This<Value>, arg: Value) -> ResolveResult {
    let outer = extract_cidr(&this)?;
    let inner = match &arg {
        Value::String(s) => {
            parse_cidr_net(s).map_err(|e| cel::ExecutionError::function_error("containsCIDR", e))?
        }
        Value::Opaque(o) => {
            let kc = o.downcast_ref::<KubeCIDR>().ok_or_else(|| {
                cel::ExecutionError::function_error("containsCIDR", "expected CIDR or string")
            })?;
            kc.0
        }
        _ => {
            return Err(cel::ExecutionError::function_error(
                "containsCIDR",
                "expected CIDR or string argument",
            ));
        }
    };
    // A contains B if A's network contains B's network address AND A's prefix <= B's prefix
    let contains = outer.0.contains(&inner.addr()) && outer.0.prefix_len() <= inner.prefix_len();
    Ok(Value::Bool(contains))
}

/// `<CIDR>.prefixLength() -> int`
fn cidr_prefix_length(This(this): This<Value>) -> ResolveResult {
    let cidr = extract_cidr(&this)?;
    Ok(Value::Int(cidr.0.prefix_len() as i64))
}

/// `<CIDR>.masked() -> CIDR`
///
/// Returns the canonical/masked form of the CIDR (network address with prefix).
fn cidr_masked(This(this): This<Value>) -> ResolveResult {
    let cidr = extract_cidr(&this)?;
    Ok(Value::Opaque(Arc::new(KubeCIDR(cidr.0.trunc()))))
}

/// `<CIDR>.ip() -> IP`
///
/// Extracts the network address from a CIDR value.
pub(crate) fn cidr_ip(This(this): This<Value>) -> ResolveResult {
    let cidr = extract_cidr(&this)?;
    Ok(Value::Opaque(Arc::new(KubeIP(cidr.0.addr()))))
}

// --- IP/CIDR string conversion ---

/// `<IP>.string() -> string`
///
/// Returns the string representation of the IP address.
pub(crate) fn ip_string(This(this): This<Value>) -> ResolveResult {
    let ip = extract_ip(&this)?;
    Ok(Value::String(std::sync::Arc::new(ip.0.to_string())))
}

/// `<CIDR>.string() -> string`
///
/// Returns the string representation of the CIDR.
pub(crate) fn cidr_string(This(this): This<Value>) -> ResolveResult {
    let cidr = extract_cidr(&this)?;
    Ok(Value::String(std::sync::Arc::new(cidr.0.to_string())))
}

// --- IP/CIDR version convenience functions ---

/// `isIPv4(<string>) -> bool`
fn is_ipv4(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(
        parse_ip_addr(&s).is_ok_and(|addr| addr.is_ipv4()),
    ))
}

/// `isIPv6(<string>) -> bool`
fn is_ipv6(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(
        parse_ip_addr(&s).is_ok_and(|addr| addr.is_ipv6()),
    ))
}

/// `isCIDRv4(<string>) -> bool`
fn is_cidr_v4(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(
        parse_cidr_net(&s).is_ok_and(|net| net.addr().is_ipv4()),
    ))
}

/// `isCIDRv6(<string>) -> bool`
fn is_cidr_v6(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(
        parse_cidr_net(&s).is_ok_and(|net| net.addr().is_ipv6()),
    ))
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
    fn test_is_ip() {
        assert_eq!(eval("isIP('192.168.1.1')"), Value::Bool(true));
        assert_eq!(eval("isIP('::1')"), Value::Bool(true));
        assert_eq!(eval("isIP('not an ip')"), Value::Bool(false));
    }

    #[test]
    fn test_ip_is_canonical() {
        assert_eq!(eval("ip.isCanonical('127.0.0.1')"), Value::Bool(true));
        assert_eq!(eval("ip.isCanonical('0127.0.0.1')"), Value::Bool(false));
    }

    #[test]
    fn test_ip_family() {
        assert_eq!(eval("ip('192.168.1.1').family()"), Value::Int(4));
        assert_eq!(eval("ip('::1').family()"), Value::Int(6));
    }

    #[test]
    fn test_ip_loopback() {
        assert_eq!(eval("ip('127.0.0.1').isLoopback()"), Value::Bool(true));
        assert_eq!(eval("ip('192.168.1.1').isLoopback()"), Value::Bool(false));
        assert_eq!(eval("ip('::1').isLoopback()"), Value::Bool(true));
    }

    #[test]
    fn test_ip_unspecified() {
        assert_eq!(eval("ip('0.0.0.0').isUnspecified()"), Value::Bool(true));
        assert_eq!(eval("ip('::').isUnspecified()"), Value::Bool(true));
    }

    #[test]
    fn test_ip_global_unicast() {
        assert_eq!(eval("ip('8.8.8.8').isGlobalUnicast()"), Value::Bool(true));
        assert_eq!(
            eval("ip('127.0.0.1').isGlobalUnicast()"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_is_cidr() {
        assert_eq!(eval("isCIDR('192.168.0.0/24')"), Value::Bool(true));
        assert_eq!(eval("isCIDR('not a cidr')"), Value::Bool(false));
    }

    #[test]
    fn test_cidr_contains_ip() {
        assert_eq!(
            eval("cidr('192.168.0.0/24').containsIP('192.168.0.1')"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("cidr('192.168.0.0/24').containsIP('10.0.0.1')"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_cidr_contains_cidr() {
        assert_eq!(
            eval("cidr('192.168.0.0/16').containsCIDR('192.168.1.0/24')"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("cidr('192.168.1.0/24').containsCIDR('192.168.0.0/16')"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_cidr_prefix_length() {
        assert_eq!(
            eval("cidr('192.168.0.0/24').prefixLength()"),
            Value::Int(24)
        );
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_ip_rejects_ipv4_mapped_ipv6() {
        assert_eq!(eval("isIP('::ffff:1.2.3.4')"), Value::Bool(false));
        eval_err("ip('::ffff:1.2.3.4')");
    }

    #[test]
    fn test_ip_rejects_zone_id() {
        assert_eq!(eval("isIP('fe80::1%eth0')"), Value::Bool(false));
        eval_err("ip('fe80::1%eth0')");
    }

    #[test]
    fn test_ip_is_link_local_multicast() {
        // IPv4 link-local multicast: 224.0.0.x
        assert_eq!(
            eval("ip('224.0.0.1').isLinkLocalMulticast()"),
            Value::Bool(true)
        );
        // Global multicast, not link-local
        assert_eq!(
            eval("ip('224.0.1.1').isLinkLocalMulticast()"),
            Value::Bool(false)
        );
        // IPv6 link-local multicast: ff02::x
        assert_eq!(
            eval("ip('ff02::1').isLinkLocalMulticast()"),
            Value::Bool(true)
        );
        // Not link-local multicast
        assert_eq!(
            eval("ip('ff05::1').isLinkLocalMulticast()"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_ip_is_link_local_unicast() {
        // IPv4 link-local: 169.254.x.x
        assert_eq!(
            eval("ip('169.254.1.1').isLinkLocalUnicast()"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("ip('169.253.1.1').isLinkLocalUnicast()"),
            Value::Bool(false)
        );
        // IPv6 link-local: fe80::x
        assert_eq!(
            eval("ip('fe80::1').isLinkLocalUnicast()"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("ip('fec0::1').isLinkLocalUnicast()"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_ip_global_unicast_edge_cases() {
        // Link-local unicast should not be global
        assert_eq!(
            eval("ip('169.254.1.1').isGlobalUnicast()"),
            Value::Bool(false)
        );
        // Multicast should not be global
        assert_eq!(
            eval("ip('224.0.0.1').isGlobalUnicast()"),
            Value::Bool(false)
        );
        // IPv6 link-local should not be global
        assert_eq!(eval("ip('fe80::1').isGlobalUnicast()"), Value::Bool(false));
    }

    #[test]
    fn test_cidr_masked() {
        // 192.168.1.5/24 masked → 192.168.1.0/24
        let result = eval("cidr('192.168.1.5/24').masked().prefixLength()");
        assert_eq!(result, Value::Int(24));

        // containsIP after masking should work for addresses in the same /24
        assert_eq!(
            eval("cidr('192.168.1.5/24').masked().containsIP('192.168.1.1')"),
            Value::Bool(true)
        );
        // Address outside the /24 should not be contained
        assert_eq!(
            eval("cidr('192.168.1.5/24').masked().containsIP('192.168.2.1')"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_cidr_contains_itself() {
        assert_eq!(
            eval("cidr('10.0.0.0/24').containsCIDR('10.0.0.0/24')"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_cidr_ipv6() {
        assert_eq!(eval("isCIDR('fd00::/8')"), Value::Bool(true));
        assert_eq!(eval("cidr('fd00::/8').prefixLength()"), Value::Int(8));
        assert_eq!(
            eval("cidr('fd00::/8').containsIP('fd00::1')"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_cidr_rejects_ipv4_mapped_ipv6() {
        assert_eq!(eval("isCIDR('::ffff:1.2.3.4/96')"), Value::Bool(false));
    }

    #[test]
    fn test_ip_is_canonical_ipv6() {
        // Non-canonical IPv6
        assert_eq!(
            eval("ip.isCanonical('0:0:0:0:0:0:0:1')"),
            Value::Bool(false)
        );
        // Canonical form
        assert_eq!(eval("ip.isCanonical('::1')"), Value::Bool(true));
    }

    // --- IP/CIDR string tests ---

    #[test]
    fn test_ip_string() {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        let result = Program::compile("ip('192.168.1.1').string()")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert_eq!(result, Value::String(Arc::new("192.168.1.1".into())));
    }

    #[test]
    fn test_ip_string_v6() {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        let result = Program::compile("ip('::1').string()")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert_eq!(result, Value::String(Arc::new("::1".into())));
    }

    #[test]
    fn test_cidr_string() {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        let result = Program::compile("cidr('192.168.0.0/24').string()")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert_eq!(result, Value::String(Arc::new("192.168.0.0/24".into())));
    }

    // --- CIDR.ip() tests ---

    #[test]
    fn test_cidr_ip_v4() {
        // cidr_ip is tested via dispatch, but also directly
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        let result = Program::compile("cidr('192.168.0.0/24').ip() == ip('192.168.0.0')")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn test_cidr_ip_v6() {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        let result = Program::compile("cidr('fd00::/64').ip() == ip('fd00::')")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    // --- IP version convenience functions ---

    #[test]
    fn test_is_ipv4() {
        assert_eq!(eval("isIPv4('1.2.3.4')"), Value::Bool(true));
        assert_eq!(eval("isIPv4('::1')"), Value::Bool(false));
        assert_eq!(eval("isIPv4('not-an-ip')"), Value::Bool(false));
    }

    #[test]
    fn test_is_ipv6() {
        assert_eq!(eval("isIPv6('::1')"), Value::Bool(true));
        assert_eq!(eval("isIPv6('1.2.3.4')"), Value::Bool(false));
        assert_eq!(eval("isIPv6('not-an-ip')"), Value::Bool(false));
    }

    #[test]
    fn test_is_cidr_v4() {
        assert_eq!(eval("isCIDRv4('10.0.0.0/8')"), Value::Bool(true));
        assert_eq!(eval("isCIDRv4('fd00::/64')"), Value::Bool(false));
        assert_eq!(eval("isCIDRv4('not-a-cidr')"), Value::Bool(false));
    }

    #[test]
    fn test_is_cidr_v6() {
        assert_eq!(eval("isCIDRv6('fd00::/64')"), Value::Bool(true));
        assert_eq!(eval("isCIDRv6('10.0.0.0/8')"), Value::Bool(false));
        assert_eq!(eval("isCIDRv6('not-a-cidr')"), Value::Bool(false));
    }
}
