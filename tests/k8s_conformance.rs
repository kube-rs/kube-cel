//! K8s CEL conformance tests.
//!
//! Test cases ported from upstream Kubernetes CEL library tests:
//! - `k8s.io/apiserver/pkg/cel/library/ip_test.go`
//! - `k8s.io/apiserver/pkg/cel/library/cidr_test.go`
//! - `k8s.io/apiserver/pkg/cel/library/quantity_test.go`
//! - `k8s.io/apiserver/pkg/cel/library/semver_test.go`

#![allow(dead_code, unused_imports)]

use cel::{Context, Program, Value};
use std::sync::Arc;

fn eval(expr: &str) -> Value {
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    Program::compile(expr).unwrap().execute(&ctx).unwrap()
}

fn assert_true(expr: &str) {
    assert_eq!(eval(expr), Value::Bool(true), "expected true: {expr}");
}

fn assert_false(expr: &str) {
    assert_eq!(eval(expr), Value::Bool(false), "expected false: {expr}");
}

fn assert_int(expr: &str, expected: i64) {
    assert_eq!(eval(expr), Value::Int(expected), "expected {expected}: {expr}");
}

fn assert_string(expr: &str, expected: &str) {
    assert_eq!(
        eval(expr),
        Value::String(Arc::new(expected.into())),
        "expected \"{expected}\": {expr}"
    );
}

fn assert_runtime_err(expr: &str) {
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    let result = Program::compile(expr).unwrap().execute(&ctx);
    assert!(result.is_err(), "expected runtime error: {expr}");
}

// ===========================================================================
// IP tests (from ip_test.go)
// ===========================================================================

#[cfg(feature = "ip")]
mod ip {
    use super::*;

    // --- IPv4 ---

    #[test]
    fn parse_valid() {
        // ip("192.168.0.1") should succeed (returns opaque)
        eval("ip(\"192.168.0.1\")");
    }

    #[test]
    fn parse_invalid() {
        assert_runtime_err("ip(\"192.168.0.1.0\")");
    }

    #[test]
    fn is_ip_valid() {
        assert_true("isIP(\"192.168.0.1\")");
    }

    #[test]
    fn is_ip_invalid() {
        assert_false("isIP(\"192.168.0.1.0\")");
    }

    #[test]
    fn is_canonical_v4() {
        assert_true("ip.isCanonical(\"127.0.0.1\")");
    }

    #[test]
    fn is_canonical_v4_invalid() {
        assert_runtime_err("ip.isCanonical(\"127.0.0.1.0\")");
    }

    #[test]
    fn family_v4() {
        assert_int("ip(\"192.168.0.1\").family()", 4);
    }

    #[test]
    fn unspecified_v4_true() {
        assert_true("ip(\"0.0.0.0\").isUnspecified()");
    }

    #[test]
    fn unspecified_v4_false() {
        assert_false("ip(\"127.0.0.1\").isUnspecified()");
    }

    #[test]
    fn loopback_v4_true() {
        assert_true("ip(\"127.0.0.1\").isLoopback()");
    }

    #[test]
    fn loopback_v4_false() {
        assert_false("ip(\"1.2.3.4\").isLoopback()");
    }

    #[test]
    fn link_local_multicast_v4_true() {
        assert_true("ip(\"224.0.0.1\").isLinkLocalMulticast()");
    }

    #[test]
    fn link_local_multicast_v4_false() {
        assert_false("ip(\"224.0.1.1\").isLinkLocalMulticast()");
    }

    #[test]
    fn link_local_unicast_v4_true() {
        assert_true("ip(\"169.254.169.254\").isLinkLocalUnicast()");
    }

    #[test]
    fn link_local_unicast_v4_false() {
        assert_false("ip(\"192.168.0.1\").isLinkLocalUnicast()");
    }

    #[test]
    fn global_unicast_v4_true() {
        assert_true("ip(\"192.168.0.1\").isGlobalUnicast()");
    }

    #[test]
    fn global_unicast_v4_false() {
        assert_false("ip(\"255.255.255.255\").isGlobalUnicast()");
    }

    // --- IPv6 ---

    #[test]
    fn parse_v6_valid() {
        eval("ip(\"2001:db8::68\")");
    }

    #[test]
    fn parse_v6_invalid() {
        assert_runtime_err("ip(\"2001:db8:::68\")");
    }

    #[test]
    fn is_ip_v6_valid() {
        assert_true("isIP(\"2001:db8::68\")");
    }

    #[test]
    fn is_ip_v6_invalid() {
        assert_false("isIP(\"2001:db8:::68\")");
    }

    #[test]
    fn is_canonical_v6_true() {
        assert_true("ip.isCanonical(\"2001:db8::68\")");
    }

    #[test]
    fn is_canonical_v6_false() {
        assert_false("ip.isCanonical(\"2001:DB8::68\")");
    }

    #[test]
    fn is_canonical_v6_invalid() {
        assert_runtime_err("ip.isCanonical(\"2001:db8:::68\")");
    }

    #[test]
    fn family_v6() {
        assert_int("ip(\"2001:db8::68\").family()", 6);
    }

    #[test]
    fn unspecified_v6_true() {
        assert_true("ip(\"::\").isUnspecified()");
    }

    #[test]
    fn unspecified_v6_false() {
        assert_false("ip(\"::1\").isUnspecified()");
    }

    #[test]
    fn loopback_v6_true() {
        assert_true("ip(\"::1\").isLoopback()");
    }

    #[test]
    fn loopback_v6_false() {
        assert_false("ip(\"2001:db8::abcd\").isLoopback()");
    }

    #[test]
    fn link_local_multicast_v6_true() {
        assert_true("ip(\"ff02::1\").isLinkLocalMulticast()");
    }

    #[test]
    fn link_local_multicast_v6_false() {
        assert_false("ip(\"fd00::1\").isLinkLocalMulticast()");
    }

    #[test]
    fn link_local_unicast_v6_true() {
        assert_true("ip(\"fe80::1\").isLinkLocalUnicast()");
    }

    #[test]
    fn link_local_unicast_v6_false() {
        assert_false("ip(\"fd80::1\").isLinkLocalUnicast()");
    }

    #[test]
    fn global_unicast_v6_true() {
        assert_true("ip(\"2001:db8::abcd\").isGlobalUnicast()");
    }

    #[test]
    fn global_unicast_v6_false() {
        assert_false("ip(\"ff00::1\").isGlobalUnicast()");
    }

    // --- string conversion ---

    #[test]
    fn string_ip_v4() {
        assert_string("string(ip(\"192.168.0.1\"))", "192.168.0.1");
    }
}

// ===========================================================================
// CIDR tests (from cidr_test.go)
// ===========================================================================

#[cfg(feature = "ip")]
mod cidr {
    use super::*;

    // --- IPv4 ---

    #[test]
    fn parse_valid() {
        eval("cidr(\"192.168.0.0/24\")");
    }

    #[test]
    fn parse_invalid() {
        assert_runtime_err("cidr(\"192.168.0.0/\")");
    }

    #[test]
    fn contains_ip_true() {
        assert_true("cidr(\"192.168.0.0/24\").containsIP(ip(\"192.168.0.1\"))");
    }

    #[test]
    fn contains_ip_false() {
        assert_false("cidr(\"192.168.0.0/24\").containsIP(ip(\"192.168.1.1\"))");
    }

    #[test]
    fn contains_cidr_true() {
        assert_true("cidr(\"192.168.0.0/24\").containsCIDR(cidr(\"192.168.0.0/25\"))");
    }

    #[test]
    fn contains_cidr_32_true() {
        assert_true("cidr(\"192.168.0.0/24\").containsCIDR(cidr(\"192.168.0.1/32\"))");
    }

    #[test]
    fn contains_cidr_larger_false() {
        assert_false("cidr(\"192.168.0.0/24\").containsCIDR(cidr(\"192.168.0.0/23\"))");
    }

    #[test]
    fn contains_cidr_outside_false() {
        assert_false("cidr(\"192.168.0.0/24\").containsCIDR(cidr(\"192.169.0.1/32\"))");
    }

    #[test]
    fn prefix_length() {
        assert_int("cidr(\"192.168.0.0/24\").prefixLength()", 24);
    }

    #[test]
    fn string_cidr() {
        assert_string("string(cidr(\"192.168.0.0/24\"))", "192.168.0.0/24");
    }

    // --- IPv6 ---

    #[test]
    fn parse_v6_valid() {
        eval("cidr(\"2001:db8::/32\")");
    }

    #[test]
    fn contains_ip_v6_true() {
        assert_true("cidr(\"2001:db8::/32\").containsIP(ip(\"2001:db8::1\"))");
    }

    #[test]
    fn contains_ip_v6_false() {
        assert_false("cidr(\"2001:db8::/32\").containsIP(ip(\"2001:dc8::1\"))");
    }

    #[test]
    fn contains_cidr_v6_true() {
        assert_true("cidr(\"2001:db8::/32\").containsCIDR(cidr(\"2001:db8::/33\"))");
    }

    #[test]
    fn contains_cidr_v6_false() {
        assert_false("cidr(\"2001:db8::/32\").containsCIDR(cidr(\"2001:db8::/31\"))");
    }

    #[test]
    fn prefix_length_v6() {
        assert_int("cidr(\"2001:db8::/32\").prefixLength()", 32);
    }
}

// ===========================================================================
// Quantity tests (from quantity_test.go)
// ===========================================================================

#[cfg(feature = "quantity")]
mod quantity {
    use super::*;

    #[test]
    fn parse_valid() {
        eval("quantity(\"12Mi\")");
    }

    #[test]
    fn parse_invalid_suffix() {
        assert_runtime_err("quantity(\"10Mo\")");
    }

    #[test]
    fn is_quantity_true() {
        assert_true("isQuantity(\"20\")");
    }

    #[test]
    fn is_quantity_megabytes() {
        assert_true("isQuantity(\"20M\")");
    }

    #[test]
    fn is_quantity_mebibytes() {
        assert_true("isQuantity(\"20Mi\")");
    }

    #[test]
    fn is_quantity_invalid_suffix() {
        assert_false("isQuantity(\"20Mo\")");
    }

    #[test]
    fn equality_reflexivity() {
        assert_true("quantity(\"200M\") == quantity(\"200M\")");
    }

    #[test]
    fn equality_symmetry() {
        assert_true("quantity(\"200M\") == quantity(\"0.2G\") && quantity(\"0.2G\") == quantity(\"200M\")");
    }

    #[test]
    fn equality_transitivity() {
        assert_true(
            "quantity(\"2M\") == quantity(\"0.002G\") && quantity(\"2000k\") == quantity(\"2M\") && quantity(\"0.002G\") == quantity(\"2000k\")",
        );
    }

    #[test]
    fn inequality() {
        assert_false("quantity(\"200M\") == quantity(\"0.3G\")");
    }

    #[test]
    fn less_true() {
        assert_true("quantity(\"50M\").isLessThan(quantity(\"50Mi\"))");
    }

    #[test]
    fn less_obvious() {
        assert_true("quantity(\"50M\").isLessThan(quantity(\"100M\"))");
    }

    #[test]
    fn less_false() {
        assert_false("quantity(\"100M\").isLessThan(quantity(\"50M\"))");
    }

    #[test]
    fn greater_true() {
        assert_true("quantity(\"50Mi\").isGreaterThan(quantity(\"50M\"))");
    }

    #[test]
    fn greater_obvious() {
        assert_true("quantity(\"150Mi\").isGreaterThan(quantity(\"100Mi\"))");
    }

    #[test]
    fn greater_false() {
        assert_false("quantity(\"50M\").isGreaterThan(quantity(\"100M\"))");
    }

    #[test]
    fn compare_equal() {
        assert_int("quantity(\"200M\").compareTo(quantity(\"0.2G\"))", 0);
    }

    #[test]
    fn compare_less() {
        assert_int("quantity(\"50M\").compareTo(quantity(\"50Mi\"))", -1);
    }

    #[test]
    fn compare_greater() {
        assert_int("quantity(\"50Mi\").compareTo(quantity(\"50M\"))", 1);
    }

    #[test]
    fn add_quantity() {
        assert_true("quantity(\"50k\").add(quantity(\"20\")) == quantity(\"50.02k\")");
    }

    #[test]
    fn sub_quantity() {
        assert_true("quantity(\"50k\").sub(quantity(\"20\")) == quantity(\"49.98k\")");
    }

    #[test]
    fn sub_int() {
        assert_true("quantity(\"50k\").sub(20) == quantity(\"49980\")");
    }

    #[test]
    fn as_integer() {
        assert_int("quantity(\"50k\").asInteger()", 50000);
    }

    #[test]
    fn is_integer_true() {
        assert_true("quantity(\"50\").isInteger()");
    }

    #[test]
    fn as_approximate_float() {
        let result = eval("quantity(\"50.703k\").asApproximateFloat()");
        match result {
            Value::Float(f) => assert!((f - 50703.0).abs() < 0.01, "expected ~50703.0, got {f}"),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn arith_chain() {
        assert_int(
            "quantity(\"50k\").add(20).sub(quantity(\"100k\")).sub(-50000).asInteger()",
            20,
        );
    }
}

// ===========================================================================
// Semver tests (from semver_test.go)
// ===========================================================================

#[cfg(feature = "semver_funcs")]
mod semver {
    use super::*;

    #[test]
    fn parse_valid() {
        eval("semver(\"1.2.3\")");
    }

    #[test]
    fn parse_invalid() {
        assert_runtime_err("semver(\"v1.0\")");
    }

    #[test]
    fn is_semver_valid() {
        assert_true("isSemver(\"1.2.3-beta.1+build.1\")");
    }

    #[test]
    fn is_semver_empty() {
        assert_false("isSemver(\"\")");
    }

    #[test]
    fn is_semver_v_prefix() {
        // K8s rejects v-prefix without lenient flag
        assert_false("isSemver(\"v1.0.0\")");
    }

    #[test]
    fn is_semver_leading_space() {
        assert_false("isSemver(\" 1.0.0\")");
    }

    #[test]
    fn is_semver_inner_space() {
        assert_false("isSemver(\"1. 0.0\")");
    }

    #[test]
    fn is_semver_trailing_space() {
        assert_false("isSemver(\"1.0.0 \")");
    }

    #[test]
    fn is_semver_leading_zeros() {
        assert_false("isSemver(\"01.01.01\")");
    }

    #[test]
    fn is_semver_major_only() {
        assert_false("isSemver(\"1\")");
    }

    #[test]
    fn is_semver_major_minor() {
        assert_false("isSemver(\"1.1\")");
    }

    #[test]
    fn equality() {
        assert_true("semver(\"1.2.3\") == semver(\"1.2.3\")");
    }

    #[test]
    fn inequality() {
        assert_false("semver(\"1.2.3\") == semver(\"1.0.0\")");
    }

    #[test]
    fn less_than_true() {
        assert_true("semver(\"1.0.0\").isLessThan(semver(\"1.2.3\"))");
    }

    #[test]
    fn less_than_equal() {
        assert_false("semver(\"1.0.0\").isLessThan(semver(\"1.0.0\"))");
    }

    #[test]
    fn greater_than_true() {
        assert_true("semver(\"1.2.3\").isGreaterThan(semver(\"1.0.0\"))");
    }

    #[test]
    fn greater_than_equal() {
        assert_false("semver(\"1.0.0\").isGreaterThan(semver(\"1.0.0\"))");
    }

    #[test]
    fn compare_equal() {
        assert_int("semver(\"1.2.3\").compareTo(semver(\"1.2.3\"))", 0);
    }

    #[test]
    fn compare_less() {
        assert_int("semver(\"1.0.0\").compareTo(semver(\"1.2.3\"))", -1);
    }

    #[test]
    fn compare_greater() {
        assert_int("semver(\"1.2.3\").compareTo(semver(\"1.0.0\"))", 1);
    }

    #[test]
    fn major() {
        assert_int("semver(\"1.2.3\").major()", 1);
    }

    #[test]
    fn minor() {
        assert_int("semver(\"1.2.3\").minor()", 2);
    }

    #[test]
    fn patch() {
        assert_int("semver(\"1.2.3\").patch()", 3);
    }
}

// ===========================================================================
// Lists tests (from lists.go function definitions)
// ===========================================================================

#[cfg(feature = "lists")]
mod lists {
    use super::*;

    #[test]
    fn is_sorted_true() {
        assert_true("[1, 2, 3].isSorted()");
    }

    #[test]
    fn is_sorted_false() {
        assert_false("[3, 1, 2].isSorted()");
    }

    #[test]
    fn sum_int() {
        assert_int("[1, 2, 3].sum()", 6);
    }

    #[test]
    fn min_list() {
        assert_int("[3, 1, 2].min()", 1);
    }

    #[test]
    fn max_list() {
        assert_int("[3, 1, 2].max()", 3);
    }

    #[test]
    fn index_of_found() {
        assert_int("[1, 2, 3].indexOf(2)", 1);
    }

    #[test]
    fn index_of_not_found() {
        assert_int("[1, 2, 3].indexOf(4)", -1);
    }

    #[test]
    fn last_index_of_found() {
        assert_int("[1, 2, 3, 2].lastIndexOf(2)", 3);
    }

    #[test]
    fn last_index_of_not_found() {
        assert_int("[1, 2, 3].lastIndexOf(4)", -1);
    }

    // min/max global variadic must still work (cel built-in)
    #[test]
    fn min_global_variadic() {
        assert_int("min(5, 3)", 3);
    }

    #[test]
    fn max_global_variadic() {
        assert_int("max(5, 3)", 5);
    }
}
