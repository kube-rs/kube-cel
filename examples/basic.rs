//! Basic usage of Kubernetes CEL extension functions.
//!
//! Run with: `cargo run --example basic`

use cel::{Context, Program};
use kube_cel::register_all;

fn run(ctx: &Context, label: &str, expr: &str) {
    let result = Program::compile(expr).unwrap().execute(ctx).unwrap();
    println!("{label}: {result:?}");
}

fn main() {
    let mut ctx = Context::default();
    register_all(&mut ctx);

    // String functions
    run(&ctx, "upperAscii", "'hello world'.upperAscii()");
    run(&ctx, "reverse", "'hello'.reverse()");

    // List functions
    run(&ctx, "isSorted", "[3, 1, 2].isSorted()");
    run(&ctx, "sort", "[3, 1, 2].sort()");
    run(&ctx, "lists.range", "lists.range(5)");
    run(&ctx, "flatten(2)", "[[1, [2]], [3]].flatten(2)");

    // Quantity comparison
    run(
        &ctx,
        "1Gi > 500Mi",
        "quantity('1Gi').isGreaterThan(quantity('500Mi'))",
    );

    // Semver
    run(
        &ctx,
        "2.0.0 > 1.9.9",
        "semver('2.0.0').isGreaterThan(semver('1.9.9'))",
    );

    // String formatting
    run(
        &ctx,
        "format",
        "'hello %s, you have %d items'.format(['world', 5])",
    );

    // IP / CIDR
    run(&ctx, "ip family", "ip('192.168.1.1').family()");
    run(&ctx, "cidr.ip()", "cidr('10.0.0.0/24').ip() == ip('10.0.0.0')");
    run(&ctx, "isIPv4", "isIPv4('192.168.1.1')");
    run(&ctx, "isCIDRv6", "isCIDRv6('fd00::/64')");

    // Named format validation
    run(
        &ctx,
        "dns1123Label",
        "!format.dns1123Label().validate('my-name').hasValue()",
    );

    // Math
    run(&ctx, "math.ceil", "math.ceil(1.2)");
    run(&ctx, "math.abs", "math.abs(-5)");
    run(&ctx, "math.bitAnd", "math.bitAnd(3, 5)");
    run(&ctx, "math.greatest", "math.greatest(1, 3, 2)");

    // Base64
    run(&ctx, "base64.encode", "base64.encode(b'hello')");
    run(&ctx, "base64.decode", "base64.decode('aGVsbG8=')");
}
