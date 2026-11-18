//! "Never panics on any input" property tests for the string parsers behind
//! kube-cel's functions. Each test compiles a few expressions once and
//! evaluates them on generated strings bound as variables. An evaluation error
//! is fine; a panic fails the test. The strategies mix arbitrary Unicode with
//! the boundaries that panicked before (huge exponents, a multi-byte character
//! where a byte offset is sliced, counts past `u32::MAX`, long input).
//!
//! Sized to keep `just check` fast: 1024 cases per parser, well under a second
//! for all of them.

#![allow(dead_code, unused_imports)]

use std::sync::Arc;

use kube_cel::cel::{Context, Program, Value};
use proptest::prelude::*;

struct Exprs {
    ctx: Context<'static, 'static>,
    programs: Vec<(&'static str, Program)>,
}

impl Exprs {
    fn new(exprs: &[&'static str]) -> Self {
        let env = Arc::new(kube_cel::new_env());
        let ctx = kube_cel::new_context(Arc::clone(&env)).unwrap();
        let programs = exprs
            .iter()
            .map(|e| (*e, env.compile(e).unwrap_or_else(|err| panic!("{e}: {err:?}"))))
            .collect();
        Exprs { ctx, programs }
    }

    /// Evaluates every expression with `vars` bound; only a panic fails.
    fn run(&self, vars: &[(&str, Value)]) {
        let mut scope = self.ctx.new_inner_scope();
        for (name, value) in vars {
            scope.add_variable_from_value(*name, value.clone());
        }
        for (_, program) in &self.programs {
            let _ = program.execute(&scope);
        }
    }
}

fn s(v: &str) -> Value {
    Value::String(Arc::new(v.to_owned()))
}

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 1024,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Arbitrary text: any Unicode, short, or ASCII-heavy and long. `any::<String>()`
/// and `\PC` leave out category C (controls, format characters such as
/// U+200D, private use, unassigned code points), so a branch collects
/// arbitrary `char`s, which include them.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        "\\PC{0,24}",
        prop::collection::vec(any::<char>(), 0..32).prop_map(String::from_iter),
        "[ -~é€😀\u{0}]{0,512}",
        "[0-9.:+eE-]{0,40}",
    ]
}

/// Integers clustered on the boundaries that narrowing and offsets get wrong.
fn boundary_int() -> impl Strategy<Value = i64> {
    prop_oneof![
        Just(0i64),
        Just(1),
        Just(-1),
        Just(i64::MIN),
        Just(i64::MAX),
        Just(i64::from(i32::MIN)),
        Just(i64::from(i32::MAX)),
        Just(i64::from(i32::MAX) + 1),
        Just(i64::from(u32::MAX)),
        Just(i64::from(u32::MAX) + 1),
        -8i64..64,
        any::<i64>(),
    ]
}

/// An exponent, often at or past the `i32` / `i64` bounds.
fn exponent() -> impl Strategy<Value = String> {
    prop_oneof![
        boundary_int().prop_map(|n| n.to_string()),
        Just("9223372036854775808".to_owned()),
        Just("-9223372036854775809".to_owned()),
        "[+-]?[0-9]{1,25}",
    ]
}

#[cfg(feature = "quantity")]
fn quantity_str() -> impl Strategy<Value = String> {
    let suffix = prop_oneof![
        Just(String::new()),
        prop::sample::select(vec![
            "n", "u", "m", "k", "M", "G", "T", "P", "E", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei",
        ])
        .prop_map(str::to_owned),
        exponent().prop_map(|e| format!("e{e}")),
        exponent().prop_map(|e| format!("E{e}")),
    ];
    prop_oneof![
        3 => ("[+-]?[0-9]{0,42}", prop::option::of("[0-9]{0,42}"), suffix, "[ é]?")
            .prop_map(|(int, frac, suffix, tail)| match frac {
                Some(frac) => format!("{int}.{frac}{suffix}{tail}"),
                None => format!("{int}{suffix}{tail}"),
            }),
        1 => text(),
    ]
}

#[cfg(feature = "quantity")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn quantity_never_panics(a in quantity_str(), b in quantity_str(), n in boundary_int()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "isQuantity(a)",
                "quantity(a).isInteger()",
                "quantity(a).asInteger()",
                "quantity(a).asApproximateFloat()",
                "quantity(a).sign()",
                "quantity(a).compareTo(quantity(b))",
                "quantity(a).isGreaterThan(quantity(b))",
                "quantity(a) == quantity(b)",
                "quantity(a).add(quantity(b)).isInteger()",
                "quantity(a).sub(quantity(b)).asInteger()",
                "quantity(a).add(n).asApproximateFloat()",
                "quantity(a).sub(n).compareTo(quantity(b))",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("b", s(&b)), ("n", Value::Int(n))]));
    }
}

#[cfg(feature = "named_format")]
fn datetime_str() -> impl Strategy<Value = String> {
    // Mostly well-formed, with a multi-byte character where a field or a
    // separator should be (byte offsets past it are not char boundaries).
    let field = || prop_oneof![6 => "[0-9]{2}", 2 => "[0-9]é", 1 => "é[0-9]?", 1 => "[0-9+ ]{1,3}"];
    let sep = || prop_oneof![4 => Just(":"), 1 => Just("é")];
    // A valid date most of the time, so the time part is reached.
    let date = prop_oneof![
        4 => (0u32..10000, 1u32..13, 1u32..29).prop_map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")),
        1 => "[0-9é-]{8,11}",
    ];
    let zone = prop_oneof![
        Just("Z".to_owned()),
        Just("z".to_owned()),
        Just(String::new()),
        ("[+-]", field(), sep(), field()).prop_map(|(sign, h, sep, m)| format!("{sign}{h}{sep}{m}")),
    ];
    prop_oneof![
        3 => (date, "[Tt]|[ é]", field(), sep(), field(), sep(), field(), prop::option::of("[.,][0-9é]{0,12}"), zone)
            .prop_map(|(date, sep, h, c1, m, c2, sec, frac, zone)| {
                format!("{date}{sep}{h}{c1}{m}{c2}{sec}{}{zone}", frac.unwrap_or_default())
            }),
        1 => text(),
    ]
}

#[cfg(feature = "named_format")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn named_formats_never_panic(a in prop_oneof![datetime_str(), text()], name in text()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "format.dns1123Label().validate(a)",
                "format.dns1123Subdomain().validate(a)",
                "format.dns1035Label().validate(a)",
                "format.dns1035LabelPrefix().validate(a)",
                "format.dns1123LabelPrefix().validate(a)",
                "format.dns1123SubdomainPrefix().validate(a)",
                "format.qualifiedName().validate(a)",
                "format.labelValue().validate(a)",
                "format.uri().validate(a)",
                "format.uuid().validate(a)",
                "format.byte().validate(a)",
                "format.date().validate(a)",
                "format.datetime().validate(a)",
                "format.named(name)",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("name", s(&name))]));
    }
}

#[cfg(feature = "urls")]
fn url_str() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => (
            prop::sample::select(vec!["http://", "https://", "foo://", "mailto:", "/", "", "file:///"]),
            prop_oneof!["[a-z0-9.é-]{0,12}", "\\[[0-9a-f:%é.]{0,24}\\]", Just("*".to_owned())],
            prop::option::of("[0-9é]{0,7}"),
            "[/a-z%0-9é?#=&]{0,24}",
        )
            .prop_map(|(scheme, host, port, rest)| match port {
                Some(port) => format!("{scheme}{host}:{port}{rest}"),
                None => format!("{scheme}{host}{rest}"),
            }),
        1 => text(),
    ]
}

#[cfg(feature = "urls")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn url_never_panics(a in url_str()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "isURL(a)",
                "url(a).getScheme()",
                "url(a).getHost()",
                "url(a).getHostname()",
                "url(a).getPort()",
                "url(a).getEscapedPath()",
                "url(a).getQuery()",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a))]));
    }
}

#[cfg(feature = "ip")]
fn ip_str() -> impl Strategy<Value = String> {
    prop_oneof![
        2 => ("[0-9]{1,4}\\.[0-9]{1,4}\\.[0-9é]{1,4}\\.[0-9]{1,4}", prop::option::of("/[0-9é]{0,4}"))
            .prop_map(|(ip, prefix)| format!("{ip}{}", prefix.unwrap_or_default())),
        2 => ("[0-9a-fA-F:.%é]{0,40}", prop::option::of("/[0-9]{0,4}"))
            .prop_map(|(ip, prefix)| format!("{ip}{}", prefix.unwrap_or_default())),
        1 => text(),
    ]
}

#[cfg(feature = "ip")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn ip_cidr_never_panic(a in ip_str(), b in ip_str()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "isIP(a)",
                "ip.isCanonical(a)",
                "ip(a).family()",
                "ip(a).isGlobalUnicast()",
                "string(ip(a))",
                "isCIDR(a)",
                "cidr(a).prefixLength()",
                "string(cidr(a).masked())",
                "cidr(a).containsIP(b)",
                "cidr(a).containsCIDR(b)",
                "ip(a) == ip(b)",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("b", s(&b))]));
    }
}

#[cfg(feature = "format")]
fn format_str() -> impl Strategy<Value = String> {
    let verb = (
        "[-+# 0]{0,2}",
        "[0-9]{0,3}",
        prop::option::of(prop_oneof!["[0-9]{0,4}", exponent()]),
        "[sdfeboxXc%é]",
    )
        .prop_map(|(flags, width, precision, verb)| match precision {
            Some(p) => format!("%{flags}{width}.{p}{verb}"),
            None => format!("%{flags}{width}{verb}"),
        });
    prop::collection::vec(prop_oneof![verb, "[a-zé %]{0,4}"], 0..5).prop_map(|parts| parts.concat())
}

#[cfg(feature = "format")]
fn format_arg() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        boundary_int().prop_map(Value::Int),
        any::<u64>().prop_map(Value::UInt),
        prop_oneof![
            any::<f64>(),
            Just(f64::NAN),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            Just(-0.0),
            Just(f64::MAX),
            Just(f64::MIN_POSITIVE),
            Just(5e-324),
        ]
        .prop_map(Value::Float),
        text().prop_map(|t| s(&t)),
        prop::collection::vec(any::<u8>(), 0..8).prop_map(|b| Value::Bytes(Arc::new(b))),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ];
    leaf.prop_recursive(2, 8, 4, |inner| {
        prop::collection::vec(inner, 0..4).prop_map(|l| Value::List(Arc::new(l)))
    })
}

#[cfg(feature = "format")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn format_never_panics(f in format_str(), args in prop::collection::vec(format_arg(), 0..10)) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "f.format(args)",
                "'%s'.format(args)",
                "'%d %f %e %b %o %x %X %.3f %.0e'.format(args)",
            ]);
        }
        E.with(|e| e.run(&[("f", s(&f)), ("args", Value::List(Arc::new(args.clone())))]));
    }
}

#[cfg(feature = "regex_funcs")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn regex_never_panics(a in text(), pattern in "[a-cé.*+?()\\[\\]{}0-9,|^$\\\\]{0,12}", n in boundary_int()) {
        thread_local! {
            static E: Exprs = Exprs::new(&["a.find(p)", "a.findAll(p)", "a.findAll(p, n)", "a.matches(p)"]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("p", s(&pattern)), ("n", Value::Int(n))]));
    }
}

#[cfg(feature = "semver_funcs")]
fn semver_str() -> impl Strategy<Value = String> {
    let num = || {
        prop_oneof![
            "[0-9]{1,3}",
            Just("18446744073709551615".to_owned()),
            Just("18446744073709551616".to_owned()),
            "[0-9]{19,30}",
        ]
    };
    prop_oneof![
        3 => ("v?", num(), "[.é]", num(), prop::option::of(("[.]", num())), prop::option::of("-[0-9A-Za-z.é-]{0,24}"), prop::option::of("\\+[0-9A-Za-z.-]{0,12}"))
            .prop_map(|(v, a, sep, b, c, pre, build)| {
                let c = c.map(|(dot, c)| format!("{dot}{c}")).unwrap_or_default();
                format!("{v}{a}{sep}{b}{c}{}{}", pre.unwrap_or_default(), build.unwrap_or_default())
            }),
        1 => text(),
    ]
}

#[cfg(feature = "semver_funcs")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn semver_never_panics(a in semver_str(), b in semver_str()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "isSemver(a)",
                "isSemver(a, true)",
                "semver(a).major()",
                "semver(a, true).patch()",
                "semver(a).compareTo(semver(b))",
                "semver(a, true).isLessThan(semver(b, true))",
                "semver(a) == semver(b)",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("b", s(&b))]));
    }
}

#[cfg(feature = "strings")]
proptest! {
    #![proptest_config(config())]

    #[test]
    fn strings_never_panic(a in text(), b in "[aé😀]{0,3}", n in boundary_int(), m in boundary_int()) {
        thread_local! {
            static E: Exprs = Exprs::new(&[
                "a.charAt(n)",
                "a.indexOf(b, n)",
                "a.lastIndexOf(b, n)",
                "a.substring(n)",
                "a.substring(n, m)",
                "a.split(b, n)",
                "a.replace(b, 'x', n)",
                "strings.quote(a)",
            ]);
        }
        E.with(|e| e.run(&[("a", s(&a)), ("b", s(&b)), ("n", Value::Int(n)), ("m", Value::Int(m))]));
    }
}
