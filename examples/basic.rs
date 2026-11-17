//! Basic usage of Kubernetes CEL extension functions.
//!
//! Run with: `cargo run --example basic`

use std::sync::Arc;

use kube_cel::cel::{Context, Env};

fn run(env: &Env, ctx: &Context, label: &str, expr: &str) {
    let result = env.compile(expr).unwrap().execute(ctx).unwrap();
    println!("{label}: {result:?}");
}

fn main() {
    // kube-cel's Env (stdlib + the overloads kube-cel declares on it) and a
    // context on it with every compiled-in function registered.
    let env = Arc::new(kube_cel::new_env());
    let ctx = kube_cel::new_context(Arc::clone(&env)).unwrap();
    let run = |label: &str, expr: &str| run(&env, &ctx, label, expr);

    // String functions
    run("upperAscii", "'hello world'.upperAscii()");
    run("reverse", "'hello'.reverse()");

    // List functions
    run("isSorted", "[3, 1, 2].isSorted()");
    run("sort", "[3, 1, 2].sort()");
    run("lists.range", "lists.range(5)");
    run("flatten(2)", "[[1, [2]], [3]].flatten(2)");

    // Quantity comparison
    run("1Gi > 500Mi", "quantity('1Gi').isGreaterThan(quantity('500Mi'))");

    // Semver
    run("2.0.0 > 1.9.9", "semver('2.0.0').isGreaterThan(semver('1.9.9'))");

    // String formatting
    run("format", "'hello %s, you have %d items'.format(['world', 5])");

    // IP / CIDR
    run("ip family", "ip('192.168.1.1').family()");
    run("cidr.ip()", "cidr('10.0.0.0/24').ip() == ip('10.0.0.0')");
    run("string(ip)", "string(ip('192.168.1.1'))");
    run("isIPv4", "isIPv4('192.168.1.1')");
    run("isCIDRv6", "isCIDRv6('fd00::/64')");

    // Named format validation
    run(
        "dns1123Label",
        "!format.dns1123Label().validate('my-name').hasValue()",
    );

    // Math
    run("math.ceil", "math.ceil(1.2)");
    run("math.abs", "math.abs(-5)");
    run("math.bitAnd", "math.bitAnd(3, 5)");
    run("math.greatest", "math.greatest(1, 3, 2)");

    // Base64
    run("base64.encode", "base64.encode(b'hello')");
    run("base64.decode", "base64.decode('aGVsbG8=')");
}
