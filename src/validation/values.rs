//! Conversion from `serde_json::Value` to `cel::Value`.
//!
//! This module provides [`json_to_cel`] which recursively converts a JSON value
//! into the CEL value representation used by the `cel` crate. The converted
//! values can then be bound as variables (e.g. `self`, `oldSelf`) in a CEL
//! evaluation context.
//!
//! A CRD rule's `self` and `oldSelf` are bound by `structural`, which binds
//! scalars under the schema's hints (`format: "date-time"`/`"date"`/
//! `"duration"`/`"byte"`, and `type: "number"`) with [`scalar_to_cel`].

use std::{collections::HashMap, sync::Arc};

use base64::Engine;
use cel::{
    Value,
    objects::{Key, Map},
};

use crate::validation::escaping::escape_field_name;

/// The `format` hint from an OpenAPI schema property.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaFormat {
    /// `format: "date-time"` — strings should be parsed as CEL `Timestamp`.
    DateTime,
    /// `format: "duration"` — strings should be parsed as CEL `Duration`.
    Duration,
    /// `format: "byte"` — base64 strings bind as CEL `bytes`. The apiserver
    /// decodes the value, so `size()`/indexing operate on the decoded bytes;
    /// keeping it a string would diverge (e.g. `size()` returns the encoded
    /// length).
    Bytes,
    /// `format: "date"` — `YYYY-MM-DD` strings bind as CEL `Timestamp` (midnight
    /// UTC). The apiserver parses them with `strfmt.RFC3339FullDate`, so date
    /// fields support the timestamp accessors (`getFullYear()` etc.); keeping
    /// them strings would diverge.
    Date,
    /// `x-kubernetes-int-or-string: true` — field can be int or string.
    /// Primarily a marker to prevent format: "date-time" etc from being interpreted.
    IntOrString,
    /// `type: "number"` — JSON numbers bind as CEL `double`, even when the value
    /// is integral. YAML→JSON translation turns `1.0` into `1`, so the apiserver
    /// converts *every* numeric variant to `types.Double` under `type: number`;
    /// binding `1` as an int would make `self.x / 2` integer division.
    Number,
    /// No recognized format or not a string type.
    #[default]
    None,
}

impl SchemaFormat {
    /// Extract a `SchemaFormat` from a raw JSON schema node.
    pub(crate) fn from_schema(schema: &serde_json::Value) -> Self {
        if schema.get("x-kubernetes-int-or-string").and_then(|v| v.as_bool()) == Some(true) {
            return SchemaFormat::IntOrString;
        }
        // The apiserver switches on `type` first (`UnstructuredToVal`): a
        // `type: number` value is a double whatever its `format` says.
        if schema.get("type").and_then(|t| t.as_str()) == Some("number") {
            return SchemaFormat::Number;
        }
        match schema.get("format").and_then(|f| f.as_str()) {
            Some("date-time") => SchemaFormat::DateTime,
            Some("date") => SchemaFormat::Date,
            Some("duration") => SchemaFormat::Duration,
            Some("byte") => SchemaFormat::Bytes,
            _ => SchemaFormat::None,
        }
    }
}

/// Convert a [`serde_json::Value`] into a [`cel::Value`].
///
/// Object keys are escaped via [`escape_field_name`]
/// to handle CEL reserved words and special characters (`.`, `-`, `/`, `_`).
///
/// # Number conversion priority
///
/// JSON numbers are converted as Kubernetes' JSON decoder reads them
/// (`sigs.k8s.io/json`): an integer that fits in `i64` as `int`, any other
/// number (`2^63` included) as `double`.
#[must_use]
pub(crate) fn json_to_cel(value: &serde_json::Value) -> Value {
    json_to_cel_impl(value, true)
}

/// Like [`json_to_cel`] but leaves object keys **unescaped** (literal names).
///
/// VAP objects are unstructured — there is no schema declaring struct fields — so
/// the apiserver binds `object`/`oldObject`/`params` without the field-name
/// escaping that CRD `self` uses. Map entries are accessed by their literal keys
/// (e.g. `object.metadata.labels['app.kubernetes.io/name']`); escaping them would
/// break every special-character key, label and annotation lookup included.
#[must_use]
pub(crate) fn json_to_cel_raw(value: &serde_json::Value) -> Value {
    json_to_cel_impl(value, false)
}

fn json_to_cel_impl(value: &serde_json::Value, escape_keys: bool) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN)), Value::Int),
        serde_json::Value::String(s) => Value::String(Arc::new(s.clone())),
        serde_json::Value::Array(arr) => {
            let items: Vec<Value> = arr.iter().map(|v| json_to_cel_impl(v, escape_keys)).collect();
            Value::List(Arc::new(items))
        }
        serde_json::Value::Object(obj) => {
            let mut map = HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                let key = if escape_keys {
                    escape_field_name(k)
                } else {
                    k.clone()
                };
                map.insert(Key::String(Arc::new(key)), json_to_cel_impl(v, escape_keys));
            }
            Value::Map(Map { map: Arc::new(map) })
        }
    }
}

/// A JSON scalar under a schema node's binding hint (`null`, a boolean, a
/// number, a string); an array or object as plain JSON.
pub(crate) fn scalar_to_cel(value: &serde_json::Value, format: &SchemaFormat) -> Value {
    match value {
        serde_json::Value::Number(n) => convert_number(n, format),
        serde_json::Value::String(s) => convert_string_with_format(s, format),
        _ => json_to_cel(value),
    }
}

/// Convert a JSON number using the schema's binding hint.
///
/// Under `type: number` the schema wins over the JSON representation: the
/// apiserver's `UnstructuredToVal` maps `int`/`int32`/`int64`/`float32`/`float64`
/// alike to `types.Double` there, because YAML→JSON translation collapses whole
/// floats (`1.0`) to integers (`1`).
#[inline]
fn convert_number(n: &serde_json::Number, format: &SchemaFormat) -> Value {
    if *format == SchemaFormat::Number {
        return Value::Float(n.as_f64().unwrap_or(f64::NAN));
    }
    if let Some(i) = n.as_i64() {
        Value::Int(i)
    } else if let Some(u) = n.as_u64() {
        Value::UInt(u)
    } else {
        Value::Float(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// Convert a string using the schema format hint.
#[inline]
fn convert_string_with_format(s: &str, format: &SchemaFormat) -> Value {
    match format {
        SchemaFormat::DateTime => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Value::Timestamp(dt);
            }
            Value::String(Arc::new(s.to_string()))
        }
        // `strfmt.RFC3339FullDate` is Go's fixed-width `2006-01-02`, so a
        // one-digit month/day ("2026-7-2") is a parse error there while chrono's
        // `%m`/`%d` would accept it — hence the exact-length guard.
        // An invalid date stays a string here; the validator reports it as an
        // error at its field (see `binding_error`), as the apiserver does.
        SchemaFormat::Date => match parse_date(s) {
            Some(ts) => Value::Timestamp(ts),
            None => Value::String(Arc::new(s.to_string())),
        },
        // As for a date, an invalid duration is reported by the validator.
        SchemaFormat::Duration => match parse_schema_duration(s) {
            Some(d) => Value::Duration(d),
            None => Value::String(Arc::new(s.to_string())),
        },
        SchemaFormat::Bytes => match base64::engine::general_purpose::STANDARD.decode(s) {
            Ok(bytes) => Value::Bytes(Arc::new(bytes)),
            // Structural validation would already have rejected malformed base64
            // upstream; fall back to the raw string rather than failing here.
            Err(_) => Value::String(Arc::new(s.to_string())),
        },
        SchemaFormat::IntOrString | SchemaFormat::Number | SchemaFormat::None => {
            Value::String(Arc::new(s.to_string()))
        }
    }
}

/// A `format: date` string as the apiserver binds it:
/// `time.Parse(strfmt.RFC3339FullDate, s)`, Go's fixed-width `2006-01-02`, so
/// a one-digit month or day ("2026-7-2") is an error there while chrono's
/// `%m`/`%d` would accept it, hence the exact-length guard.
pub(crate) fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if s.len() != 10 {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().fixed_offset())
}

/// Why the apiserver cannot bind `value` under `format`, if it cannot: its
/// binding returns an error, and its OpenAPI format check refuses the object
/// whether or not a rule reads the field. Only formats whose parser here
/// matches the apiserver's exactly (`date`, `duration`) are checked, so this
/// never refuses a value the apiserver accepts.
pub(crate) fn binding_error(value: &serde_json::Value, format: &SchemaFormat) -> Option<String> {
    let s = value.as_str()?;
    match format {
        SchemaFormat::Date if parse_date(s).is_none() => Some(format!("Invalid date formatted string {s}")),
        SchemaFormat::Duration if parse_schema_duration(s).is_none() => Some(format!("Invalid duration {s}")),
        _ => None,
    }
}

/// Go's `time.ParseDuration` (go1.26.2), exactly: integer arithmetic, the
/// fractional part through `float64` as Go does it, and every overflow an
/// error (`None`).
pub(crate) fn parse_go_duration(input: &str) -> Option<chrono::Duration> {
    crate::functions::go_parse_duration(input.as_bytes()).map(chrono::Duration::nanoseconds)
}

/// The apiserver's `format: duration`, kube-openapi `strfmt.ParseDuration`:
/// Go's `time.ParseDuration`, else every `<digits><spaces><letters>` match of
/// `(\d+)\s*([A-Za-zµ]+)` whose letters name a unit (`d`, `day`, `w`, `hour`,
/// `sec`, ...), summed with Go's wrapping int64 arithmetic. A digit run past
/// int64 is an error (`strconv.Atoi`).
pub(crate) fn parse_schema_duration(input: &str) -> Option<chrono::Duration> {
    if let Some(d) = parse_go_duration(input) {
        return Some(d);
    }
    const UNITS: &[(&[&str], i64)] = &[
        (&["ns", "nano"], 1),
        (&["us", "µs", "micro"], 1_000),
        (&["ms", "milli"], 1_000_000),
        (&["s", "sec"], 1_000_000_000),
        (&["m", "min"], 60_000_000_000),
        (&["h", "hr", "hour"], 3_600_000_000_000),
        (&["d", "day"], 86_400_000_000_000),
        (&["w", "wk", "week"], 604_800_000_000_000),
    ];
    let mut total: i64 = 0;
    let mut matched = false;
    for (digits, letters) in duration_matches(input) {
        let factor: i64 = digits.parse().ok()?;
        // `strings.ToLower`; `strings.EqualFold` against these lowercase
        // variants is then plain equality.
        let unit = letters.to_lowercase();
        for (variants, nanos) in UNITS {
            let last = variants.len() - 1;
            for (i, variant) in variants.iter().enumerate() {
                if (i == last && unit.starts_with(variant)) || unit == *variant {
                    matched = true;
                    total = total.wrapping_add(factor.wrapping_mul(*nanos));
                }
            }
        }
    }
    matched.then(|| chrono::Duration::nanoseconds(total))
}

/// The `(digits, letters)` of each match of Go's `(\d+)\s*([A-Za-zµ]+)`, left
/// to right, without overlap: a maximal ASCII digit run, then `\s` (tab,
/// newline, form feed, carriage return, space), then a maximal run of ASCII
/// letters and `µ` (U+00B5).
fn duration_matches(s: &str) -> Vec<(&str, &str)> {
    let b = s.as_bytes();
    let letter_len = |i: usize| -> usize {
        match b.get(i) {
            Some(c) if c.is_ascii_alphabetic() => 1,
            Some(0xC2) if b.get(i + 1) == Some(&0xB5) => 2,
            _ => 0,
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let digits_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits_end = i;
        let mut j = i;
        while matches!(b.get(j), Some(b'\t' | b'\n' | 0x0C | b'\r' | b' ')) {
            j += 1;
        }
        let letters_start = j;
        while letter_len(j) > 0 {
            j += letter_len(j);
        }
        if j > letters_start {
            out.push((&s[digits_start..digits_end], &s[letters_start..j]));
            i = j;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::validation::structural::{Node, json_to_val_with_compiled, json_to_val_with_schema};

    /// `value` as a rule's `self` binds it under `schema`, as a plain value.
    fn json_to_cel_with_schema(value: &serde_json::Value, schema: &serde_json::Value) -> Value {
        crate::functions::val_args::to_value(json_to_val_with_schema(value, schema, Node::Inner).as_ref())
            .expect("the bound value converts")
    }

    /// As [`json_to_cel_with_schema`], under a compiled schema.
    fn json_to_cel_with_compiled(
        value: &serde_json::Value,
        compiled: &crate::validation::compilation::CompiledSchema,
    ) -> Value {
        crate::functions::val_args::to_value(json_to_val_with_compiled(value, compiled, Node::Inner).as_ref())
            .expect("the bound value converts")
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn test_null() {
        assert_eq!(json_to_cel(&json!(null)), Value::Null);
    }

    #[test]
    fn test_bool() {
        assert_eq!(json_to_cel(&json!(true)), Value::Bool(true));
        assert_eq!(json_to_cel(&json!(false)), Value::Bool(false));
    }

    #[test]
    fn test_i64() {
        assert_eq!(json_to_cel(&json!(42)), Value::Int(42));
        assert_eq!(json_to_cel(&json!(-1)), Value::Int(-1));
        assert_eq!(json_to_cel(&json!(0)), Value::Int(0));
    }

    /// Kubernetes' JSON decoder reads an integer past `i64` as a `float64`.
    #[test]
    fn test_u64_beyond_i64() {
        let big: u64 = (i64::MAX as u64) + 1;
        let v = json_to_cel(&serde_json::Value::Number(serde_json::Number::from(big)));
        assert_eq!(v, Value::Float(big as f64));
    }

    #[test]
    fn test_float() {
        assert_eq!(json_to_cel(&json!(2.5)), Value::Float(2.5));
        assert_eq!(json_to_cel(&json!(0.0)), Value::Float(0.0));
    }

    #[test]
    fn test_string() {
        assert_eq!(
            json_to_cel(&json!("hello")),
            Value::String(Arc::new("hello".into()))
        );
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(json_to_cel(&json!("")), Value::String(Arc::new(String::new())));
    }

    #[test]
    fn test_array_mixed() {
        let v = json_to_cel(&json!([1, "two", true, null]));
        let expected = Value::List(Arc::new(vec![
            Value::Int(1),
            Value::String(Arc::new("two".into())),
            Value::Bool(true),
            Value::Null,
        ]));
        assert_eq!(v, expected);
    }

    #[test]
    fn test_empty_array() {
        assert_eq!(json_to_cel(&json!([])), Value::List(Arc::new(vec![])));
    }

    #[test]
    fn test_object() {
        let v = json_to_cel(&json!({"name": "test", "count": 5}));
        if let Value::Map(map) = v {
            assert_eq!(
                map.map.get(&Key::String(Arc::new("name".into()))),
                Some(&Value::String(Arc::new("test".into())))
            );
            assert_eq!(
                map.map.get(&Key::String(Arc::new("count".into()))),
                Some(&Value::Int(5))
            );
        } else {
            panic!("expected Map");
        }
    }

    #[test]
    fn test_empty_object() {
        let v = json_to_cel(&json!({}));
        if let Value::Map(map) = v {
            assert!(map.map.is_empty());
        } else {
            panic!("expected Map");
        }
    }

    #[test]
    fn test_nested_structure() {
        let v = json_to_cel(&json!({
            "spec": {
                "replicas": 3,
                "items": [1, 2, 3]
            }
        }));
        if let Value::Map(outer) = v {
            let spec = outer.map.get(&Key::String(Arc::new("spec".into()))).unwrap();
            if let Value::Map(inner) = spec {
                assert_eq!(
                    inner.map.get(&Key::String(Arc::new("replicas".into()))),
                    Some(&Value::Int(3))
                );
                assert_eq!(
                    inner.map.get(&Key::String(Arc::new("items".into()))),
                    Some(&Value::List(Arc::new(vec![
                        Value::Int(1),
                        Value::Int(2),
                        Value::Int(3),
                    ])))
                );
            } else {
                panic!("expected inner Map");
            }
        } else {
            panic!("expected outer Map");
        }
    }

    #[test]
    fn test_number_priority() {
        // i64 range → Int
        assert_eq!(json_to_cel(&json!(42)), Value::Int(42));
        // past i64 → Float, as Kubernetes' JSON decoder reads it
        let big: u64 = (i64::MAX as u64) + 1;
        assert_eq!(
            json_to_cel(&serde_json::Value::Number(serde_json::Number::from(big))),
            Value::Float(big as f64)
        );
        // float → Float
        assert_eq!(json_to_cel(&json!(1.5)), Value::Float(1.5));
    }

    // ── parse_go_duration tests ─────────────────────────────────────

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_go_duration("1h"), Some(chrono::Duration::hours(1)));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_go_duration("30m"), Some(chrono::Duration::minutes(30)));
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_go_duration("45s"), Some(chrono::Duration::seconds(45)));
    }

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(
            parse_go_duration("500ms"),
            Some(chrono::Duration::milliseconds(500))
        );
    }

    #[test]
    fn parse_duration_microseconds() {
        assert_eq!(
            parse_go_duration("100us"),
            Some(chrono::Duration::microseconds(100))
        );
    }

    #[test]
    fn parse_duration_nanoseconds() {
        assert_eq!(
            parse_go_duration("999ns"),
            Some(chrono::Duration::nanoseconds(999))
        );
    }

    #[test]
    fn parse_duration_compound() {
        assert_eq!(
            parse_go_duration("1h30m"),
            Some(chrono::Duration::hours(1) + chrono::Duration::minutes(30))
        );
        assert_eq!(
            parse_go_duration("1h30m10s"),
            Some(chrono::Duration::hours(1) + chrono::Duration::minutes(30) + chrono::Duration::seconds(10))
        );
    }

    #[test]
    fn parse_duration_negative() {
        assert_eq!(parse_go_duration("-1h"), Some(chrono::Duration::hours(-1)));
        assert_eq!(parse_go_duration("-30s"), Some(chrono::Duration::seconds(-30)));
    }

    #[test]
    fn parse_duration_zero() {
        assert_eq!(parse_go_duration("0"), Some(chrono::Duration::zero()));
    }

    #[test]
    fn schema_duration_matches_strfmt() {
        // Expected values are kube-openapi `strfmt.ParseDuration`'s (the
        // apiserver's `format: duration`), run with k8s.io/kube-openapi.
        let ns = |s: &str| parse_schema_duration(s).map(|d| d.num_nanoseconds().unwrap());
        // Past int64: an error, neither saturated nor a panic.
        assert_eq!(ns("9223372036854775808µs"), None);
        assert_eq!(ns("5000000000000000μs5000000000000000μs"), None);
        // Go's own syntax, including a sign and the int64 minimum.
        assert_eq!(ns("+1s"), Some(1_000_000_000));
        assert_eq!(ns("-9223372036854775808ns"), Some(i64::MIN));
        // strfmt's fallback: word units, and Go's wrapping int64 sum.
        assert_eq!(ns("1d"), Some(86_400_000_000_000));
        assert_eq!(ns("1 day 2 hours"), Some(93_600_000_000_000));
        assert_eq!(ns("1.5d"), Some(432_000_000_000_000));
        assert_eq!(ns("3000000h"), Some(-7_646_744_073_709_551_616));
        assert_eq!(ns("2562047h47m16.854775808s"), Some(-8_368_596_245_709_551_616));
    }

    #[test]
    fn parse_duration_invalid() {
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration("1x"), None);
        assert_eq!(parse_go_duration("h"), None);
    }

    // ── Schema-aware conversion tests ───────────────────────────────

    #[test]
    fn timestamp_parsed_from_schema() {
        let schema = json!({
            "type": "string",
            "format": "date-time"
        });
        let value = json!("2024-01-01T00:00:00Z");
        let result = json_to_cel_with_schema(&value, &schema);
        assert!(matches!(result, Value::Timestamp(_)));
    }

    #[test]
    fn number_type_wins_over_a_string_format() {
        for format in ["date", "date-time", "duration", "byte"] {
            let schema = json!({ "type": "number", "format": format });
            assert_eq!(
                SchemaFormat::from_schema(&schema),
                SchemaFormat::Number,
                "{format}"
            );
            assert_eq!(
                json_to_cel_with_schema(&json!(1), &schema),
                Value::Float(1.0),
                "{format}"
            );
        }
    }

    #[test]
    fn date_format_parses_to_timestamp() {
        // apiserver: `case "date": time.Parse(strfmt.RFC3339FullDate, str)` →
        // types.Timestamp, so date fields support the timestamp accessors.
        let schema = json!({ "type": "string", "format": "date" });
        let result = json_to_cel_with_schema(&json!("2024-03-05"), &schema);
        let Value::Timestamp(ts) = result else {
            panic!("expected Timestamp, got {result:?}");
        };
        assert_eq!(ts.to_rfc3339(), "2024-03-05T00:00:00+00:00");
    }

    #[test]
    fn date_format_invalid_is_a_binding_error() {
        let schema = json!({ "type": "string", "format": "date" });
        let format = SchemaFormat::from_schema(&schema);
        // Go's fixed-width layout rejects a one-digit month, and Go's calendar
        // a day out of range; so must we.
        for s in ["2024-3-05", "2024-02-30", "not-a-date", "2024-03-05T00:00:00Z"] {
            assert!(parse_date(s).is_none(), "{s} should not parse as a date");
            assert!(binding_error(&json!(s), &format).is_some(), "{s}");
        }
        assert!(binding_error(&json!("2024-03-05"), &format).is_none());
    }

    #[test]
    fn number_type_binds_integral_value_as_double() {
        // The divergence this pins: `{"x": 1}` under `type: number` is a CEL
        // double upstream, so `self.x / 2` is 0.5 — not integer division.
        let schema = json!({ "type": "number" });
        assert_eq!(json_to_cel_with_schema(&json!(1), &schema), Value::Float(1.0));
        assert_eq!(json_to_cel_with_schema(&json!(1.5), &schema), Value::Float(1.5));
    }

    #[test]
    fn integer_type_stays_int() {
        let schema = json!({ "type": "integer" });
        assert_eq!(json_to_cel_with_schema(&json!(1), &schema), Value::Int(1));
    }

    #[test]
    fn number_type_binds_double_through_compiled_schema() {
        // The compiled path carries the same hint, so both entry points agree.
        let schema = json!({
            "type": "object",
            "properties": { "x": { "type": "number" } }
        });
        let compiled = crate::validation::compilation::compile_schema(&schema);
        let Value::Map(m) = json_to_cel_with_compiled(&json!({"x": 1}), &compiled) else {
            panic!("expected Map");
        };
        assert_eq!(
            m.map.get(&Key::String(Arc::new("x".into()))),
            Some(&Value::Float(1.0))
        );
    }

    #[test]
    fn byte_format_decodes_to_bytes() {
        let schema = json!({ "type": "string", "format": "byte" });
        // "aGVsbG8=" decodes to "hello" (5 bytes); the apiserver binds it as
        // CEL bytes, so size() must see the decoded length, not the encoded one.
        let result = json_to_cel_with_schema(&json!("aGVsbG8="), &schema);
        assert_eq!(result, Value::Bytes(Arc::new(b"hello".to_vec())));
    }

    #[test]
    fn byte_format_invalid_falls_back_to_string() {
        let schema = json!({ "type": "string", "format": "byte" });
        let result = json_to_cel_with_schema(&json!("!!!not-base64!!!"), &schema);
        assert_eq!(result, Value::String(Arc::new("!!!not-base64!!!".into())));
    }

    #[test]
    fn timestamp_parse_failure_falls_back_to_string() {
        let schema = json!({
            "type": "string",
            "format": "date-time"
        });
        let value = json!("not-a-date");
        let result = json_to_cel_with_schema(&value, &schema);
        assert_eq!(result, Value::String(Arc::new("not-a-date".into())));
    }

    #[test]
    fn duration_parsed_from_schema() {
        let schema = json!({
            "type": "string",
            "format": "duration"
        });
        let value = json!("1h30m");
        let result = json_to_cel_with_schema(&value, &schema);
        assert!(matches!(result, Value::Duration(_)));
    }

    #[test]
    fn duration_parse_failure_falls_back_to_string() {
        let schema = json!({
            "type": "string",
            "format": "duration"
        });
        let value = json!("not-a-duration");
        let result = json_to_cel_with_schema(&value, &schema);
        assert_eq!(result, Value::String(Arc::new("not-a-duration".into())));
    }

    #[test]
    fn nested_object_properties_format() {
        let schema = json!({
            "type": "object",
            "properties": {
                "createdAt": {
                    "type": "string",
                    "format": "date-time"
                },
                "name": {
                    "type": "string"
                },
                "timeout": {
                    "type": "string",
                    "format": "duration"
                }
            }
        });
        let value = json!({
            "createdAt": "2024-06-15T10:30:00Z",
            "name": "test",
            "timeout": "30s"
        });
        let result = json_to_cel_with_schema(&value, &schema);
        if let Value::Map(map) = result {
            assert!(matches!(
                map.map.get(&Key::String(Arc::new("createdAt".into()))),
                Some(Value::Timestamp(_))
            ));
            assert!(matches!(
                map.map.get(&Key::String(Arc::new("name".into()))),
                Some(Value::String(_))
            ));
            assert!(matches!(
                map.map.get(&Key::String(Arc::new("timeout".into()))),
                Some(Value::Duration(_))
            ));
        } else {
            panic!("expected Map");
        }
    }

    #[test]
    fn array_items_format() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "string",
                "format": "date-time"
            }
        });
        let value = json!(["2024-01-01T00:00:00Z", "2024-06-15T12:00:00+09:00"]);
        let result = json_to_cel_with_schema(&value, &schema);
        if let Value::List(items) = result {
            assert!(matches!(items[0], Value::Timestamp(_)));
            assert!(matches!(items[1], Value::Timestamp(_)));
        } else {
            panic!("expected List");
        }
    }

    #[test]
    fn no_format_leaves_string_unchanged() {
        let schema = json!({
            "type": "string"
        });
        let value = json!("2024-01-01T00:00:00Z");
        let result = json_to_cel_with_schema(&value, &schema);
        assert_eq!(result, Value::String(Arc::new("2024-01-01T00:00:00Z".into())));
    }

    #[test]
    fn json_to_cel_unchanged_with_no_schema() {
        // Original json_to_cel should still work as before
        let value = json!("2024-01-01T00:00:00Z");
        let result = json_to_cel(&value);
        assert_eq!(result, Value::String(Arc::new("2024-01-01T00:00:00Z".into())));
    }

    #[test]
    fn int_or_string_schema_format_detected() {
        let schema = json!({"x-kubernetes-int-or-string": true});
        assert_eq!(SchemaFormat::from_schema(&schema), SchemaFormat::IntOrString);
    }

    #[test]
    fn int_or_string_int_value_preserved() {
        let schema = json!({"x-kubernetes-int-or-string": true});
        let result = json_to_cel_with_schema(&json!(8080), &schema);
        assert_eq!(result, Value::Int(8080));
    }

    #[test]
    fn int_or_string_string_value_preserved() {
        let schema = json!({"x-kubernetes-int-or-string": true});
        let result = json_to_cel_with_schema(&json!("http"), &schema);
        assert_eq!(result, Value::String(Arc::new("http".into())));
    }

    #[test]
    fn int_or_string_overrides_format() {
        // Even with format: date-time, int-or-string takes precedence
        let schema = json!({"x-kubernetes-int-or-string": true, "format": "date-time"});
        assert_eq!(SchemaFormat::from_schema(&schema), SchemaFormat::IntOrString);
    }

    // ── additionalProperties map-key escaping (#8) ──────────────────
    // The apiserver escapes only declared `properties` (struct field names);
    // `additionalProperties` map keys are passed through literally. We must
    // match that, or rules like `'a.b/c' in self.m` never fire client-side.

    /// Look up a key in a converted object, asserting the result is a Map.
    fn map_has_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Map(m) => m.map.contains_key(&Key::String(Arc::new(key.into()))),
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn additional_properties_object_keys_not_escaped() {
        // map[string]string: additionalProperties is an object schema.
        let schema = json!({
            "type": "object",
            "additionalProperties": {"type": "string"}
        });
        let value = json!({"rio.build/fetcher": "true"});
        let result = json_to_cel_with_schema(&value, &schema);
        // Literal key present (apiserver behavior); escaped key absent (the #8 bug).
        assert!(map_has_key(&result, "rio.build/fetcher"));
        assert!(!map_has_key(&result, "rio__dot__build__slash__fetcher"));
    }

    /// `additionalProperties: true` declares no schema for the keys: the
    /// apiserver binds such a map as a free-form object, every key absent to
    /// CEL but counted (Go oracle, v0.36.1), whether or not it is escaped.
    #[test]
    fn additional_properties_true_keys_are_absent() {
        let schema = json!({
            "type": "object",
            "properties": { "m": { "type": "object", "additionalProperties": true } },
            "x-kubernetes-validations": [{
                "rule": "!('a.b/c' in dyn(self.m)) && !('a__dot__b__slash__c' in dyn(self.m)) && size(dyn(self.m)) == 1"
            }]
        });
        let object = json!({ "m": { "a.b/c": "x" } });
        let validator = crate::validation::Validator::new();
        assert!(validator.validate(&schema, &object, None).is_ok());
        let compiled = crate::validation::compilation::compile_schema(&schema);
        assert!(validator.validate_compiled(&compiled, &object, None).is_ok());
    }

    #[test]
    fn declared_properties_still_escaped() {
        // Regression guard: struct field names (declared properties) keep escaping.
        // additionalProperties:false marks a closed struct, not a map.
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "app.kubernetes.io/name": {"type": "string"}
            }
        });
        let value = json!({"app.kubernetes.io/name": "web"});
        let result = json_to_cel_with_schema(&value, &schema);
        assert!(map_has_key(&result, "app__dot__kubernetes__dot__io__slash__name"));
        assert!(!map_has_key(&result, "app.kubernetes.io/name"));
    }

    #[test]
    fn nested_map_under_property_keys_not_escaped() {
        // nodeSelector typed as map[string]string under a declared property.
        let schema = json!({
            "type": "object",
            "properties": {
                "nodeSelector": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                }
            }
        });
        let value = json!({"nodeSelector": {"rio.build/fetcher": "true"}});
        let result = json_to_cel_with_schema(&value, &schema);
        let Value::Map(outer) = &result else {
            panic!("expected Map")
        };
        let inner = outer
            .map
            .get(&Key::String(Arc::new("nodeSelector".into())))
            .unwrap();
        assert!(map_has_key(inner, "rio.build/fetcher"));
        assert!(!map_has_key(inner, "rio__dot__build__slash__fetcher"));
    }

    #[test]
    fn compiled_additional_properties_keys_not_escaped() {
        // Same as the object case but through the pre-compiled schema path.
        let schema = json!({
            "type": "object",
            "additionalProperties": {"type": "string"}
        });
        let compiled = crate::validation::compilation::compile_schema(&schema);
        let value = json!({"rio.build/fetcher": "true"});
        let result = json_to_cel_with_compiled(&value, &compiled);
        assert!(map_has_key(&result, "rio.build/fetcher"));
        assert!(!map_has_key(&result, "rio__dot__build__slash__fetcher"));
    }

    #[test]
    fn compiled_declared_properties_still_escaped() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "app.kubernetes.io/name": {"type": "string"}
            }
        });
        let compiled = crate::validation::compilation::compile_schema(&schema);
        let value = json!({"app.kubernetes.io/name": "web"});
        let result = json_to_cel_with_compiled(&value, &compiled);
        assert!(map_has_key(&result, "app__dot__kubernetes__dot__io__slash__name"));
        assert!(!map_has_key(&result, "app.kubernetes.io/name"));
    }
}

/// End-to-end tests of the internal `json_to_cel` conversion through real CEL
/// compilation + evaluation. Relocated from `tests/cel_evaluation.rs` when
/// `json_to_cel` became `pub(crate)` — an integration test can no longer reach
/// it, so its coverage lives here in-crate.
#[cfg(test)]
mod cel_evaluation_tests {
    use std::sync::Arc;

    use cel::Value;
    use serde_json::json;

    use super::json_to_cel;
    use crate::validation::{
        compilation::compile_schema,
        structural::{Node, json_to_val_with_compiled},
        validation_env,
    };

    /// Build a context with kube-cel functions, bind `self` from JSON, compile,
    /// and return the evaluation result.
    fn eval_self(json_val: serde_json::Value, expr: &str) -> Value {
        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&json_val));
        validation_env().compile(expr).unwrap().execute(&ctx).unwrap()
    }

    /// `'key' in self.<map>` must fire on the literal user-supplied key, not an
    /// escaped form. Mirrors the kube-rs/kube-cel#8 repro end-to-end: without the
    /// fix the rule passes for inputs the apiserver rejects (fail-open).
    #[test]
    fn in_operator_matches_unescaped_map_key() {
        let schema = json!({
            "type": "object",
            "properties": {
                "m": {"type": "object", "additionalProperties": {"type": "string"}}
            }
        });
        let compiled = compile_schema(&schema);
        let self_val = json!({"m": {"a.b/c": "x"}});

        let mut ctx = crate::validation::base_context();
        ctx.add_variable_as_val(
            "self",
            json_to_val_with_compiled(&self_val, &compiled, Node::Inner),
        );

        // Rule from the issue: must REJECT (false) when the forbidden key is present.
        let prog = validation_env().compile("!('a.b/c' in self.m)").unwrap();
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(false));
    }

    /// Same as `eval_self` but also binds `oldSelf`.
    fn eval_transition(json_self: serde_json::Value, json_old: serde_json::Value, expr: &str) -> Value {
        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&json_self));
        ctx.add_variable_from_value("oldSelf", json_to_cel(&json_old));
        validation_env().compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    fn scalar_comparison() {
        assert_eq!(eval_self(json!(10), "self >= 0"), Value::Bool(true));
        assert_eq!(eval_self(json!(-1), "self >= 0"), Value::Bool(false));
    }

    #[test]
    fn field_access_int() {
        let obj = json!({"replicas": 3});
        assert_eq!(eval_self(obj, "self.replicas"), Value::Int(3));
    }

    #[test]
    fn field_access_string() {
        let obj = json!({"name": "my-app"});
        assert_eq!(
            eval_self(obj, "self.name"),
            Value::String(Arc::new("my-app".into()))
        );
    }

    #[test]
    fn nested_field_comparison() {
        let obj = json!({"spec": {"replicas": 5, "minReplicas": 2}});
        assert_eq!(
            eval_self(obj, "self.spec.replicas >= self.spec.minReplicas"),
            Value::Bool(true)
        );
    }

    #[test]
    fn transition_rule_oldself() {
        let new = json!({"replicas": 5});
        let old = json!({"replicas": 3});
        assert_eq!(
            eval_transition(new, old, "self.replicas >= oldSelf.replicas"),
            Value::Bool(true)
        );
    }

    #[test]
    fn transition_rule_downscale_rejected() {
        let new = json!({"replicas": 1});
        let old = json!({"replicas": 3});
        assert_eq!(
            eval_transition(new, old, "self.replicas >= oldSelf.replicas"),
            Value::Bool(false)
        );
    }

    #[test]
    fn detect_oldself_reference() {
        let prog1 = validation_env()
            .compile("self.replicas >= oldSelf.replicas")
            .unwrap();
        assert!(prog1.references().has_variable("oldSelf"));
        assert!(prog1.references().has_variable("self"));

        let prog2 = validation_env().compile("self.replicas >= 0").unwrap();
        assert!(!prog2.references().has_variable("oldSelf"));
        assert!(prog2.references().has_variable("self"));
    }

    #[test]
    #[cfg(feature = "strings")]
    fn extension_trim_lower_ascii() {
        let obj = json!({"name": "  Hello World  "});
        assert_eq!(
            eval_self(obj, "self.name.trim().lowerAscii()"),
            Value::String(Arc::new("hello world".into()))
        );
    }

    #[test]
    #[cfg(feature = "lists")]
    fn extension_is_sorted() {
        let obj = json!({"items": [1, 2, 3, 4]});
        assert_eq!(eval_self(obj, "self.items.isSorted()"), Value::Bool(true));

        let obj2 = json!({"items": [3, 1, 2]});
        assert_eq!(eval_self(obj2, "self.items.isSorted()"), Value::Bool(false));
    }

    #[test]
    fn array_indexing() {
        let obj = json!({"containers": [{"name": "nginx"}, {"name": "sidecar"}]});
        assert_eq!(
            eval_self(obj, "self.containers[0].name"),
            Value::String(Arc::new("nginx".into()))
        );
    }

    #[test]
    fn null_comparison() {
        let obj = json!({"extra": null});
        assert_eq!(eval_self(obj, "self.extra == null"), Value::Bool(true));
    }

    #[test]
    fn non_null_comparison() {
        let obj = json!({"extra": "present"});
        assert_eq!(eval_self(obj, "self.extra == null"), Value::Bool(false));
    }

    #[test]
    fn has_macro_present() {
        let obj = json!({"name": "test"});
        assert_eq!(eval_self(obj, "has(self.name)"), Value::Bool(true));
    }

    #[test]
    fn has_macro_missing() {
        let obj = json!({"name": "test"});
        assert_eq!(eval_self(obj, "has(self.missing)"), Value::Bool(false));
    }

    #[test]
    fn size_of_list() {
        let obj = json!({"items": [1, 2, 3]});
        assert_eq!(eval_self(obj, "size(self.items)"), Value::Int(3));
    }

    #[test]
    fn size_of_string() {
        let obj = json!({"name": "hello"});
        assert_eq!(eval_self(obj, "size(self.name)"), Value::Int(5));
    }
}
