//! Named format validation for Kubernetes CEL.
//!
//! Provides `format.<name>()` factory functions that return opaque format objects,
//! and a `validate(<string>)` method that returns `optional<list<string>>`:
//! - `optional.none()` when the value is valid
//! - `optional.of([...errors])` when the value is invalid
//!
//! This matches the K8s pattern: `!format.dns1123Label().validate(name).hasValue()`
//!
//! Mirrors `k8s.io/apiserver/pkg/cel/library/format.go` (Kubernetes 1.32+).

use cel::{
    ExecutionError, ResolveResult,
    extractors::This,
    objects::{Opaque, OptionalValue, Value},
};
use std::{fmt, sync::Arc};

use crate::functions::Registrar;

// ---------------------------------------------------------------------------
// Format kind enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormatKind {
    Dns1123Label,
    Dns1123Subdomain,
    Dns1035Label,
    Dns1035LabelPrefix,
    Dns1123LabelPrefix,
    Dns1123SubdomainPrefix,
    QualifiedName,
    LabelValue,
    Uri,
    Uuid,
    Byte,
    Date,
    DateTime,
}

impl fmt::Display for FormatKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            FormatKind::Dns1123Label => "dns1123Label",
            FormatKind::Dns1123Subdomain => "dns1123Subdomain",
            FormatKind::Dns1035Label => "dns1035Label",
            FormatKind::Dns1035LabelPrefix => "dns1035LabelPrefix",
            FormatKind::Dns1123LabelPrefix => "dns1123LabelPrefix",
            FormatKind::Dns1123SubdomainPrefix => "dns1123SubdomainPrefix",
            FormatKind::QualifiedName => "qualifiedName",
            FormatKind::LabelValue => "labelValue",
            FormatKind::Uri => "uri",
            FormatKind::Uuid => "uuid",
            FormatKind::Byte => "byte",
            FormatKind::Date => "date",
            FormatKind::DateTime => "datetime",
        };
        write!(f, "{name}")
    }
}

// ---------------------------------------------------------------------------
// Opaque wrapper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KubeFormat(FormatKind);

impl Opaque for KubeFormat {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.Format"
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all named format extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    // Factory functions
    r.add("format.dns1123Label", format_dns1123_label);
    r.add("format.dns1123Subdomain", format_dns1123_subdomain);
    r.add("format.dns1035Label", format_dns1035_label);
    r.add("format.dns1035LabelPrefix", format_dns1035_label_prefix);
    r.add("format.dns1123LabelPrefix", format_dns1123_label_prefix);
    r.add("format.dns1123SubdomainPrefix", format_dns1123_subdomain_prefix);
    r.add("format.qualifiedName", format_qualified_name);
    r.add("format.labelValue", format_label_value);
    r.add("format.uri", format_uri);
    r.add("format.uuid", format_uuid);
    r.add("format.byte", format_byte);
    r.add("format.date", format_date);
    r.add("format.datetime", format_datetime);
    r.add("format.named", format_named);

    // Validate method
    r.add("validate", format_validate);
}

// ---------------------------------------------------------------------------
// Factory functions
// ---------------------------------------------------------------------------

macro_rules! format_factory {
    ($fn_name:ident, $kind:ident) => {
        fn $fn_name() -> ResolveResult {
            Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::$kind))))
        }
    };
}

format_factory!(format_dns1123_label, Dns1123Label);
format_factory!(format_dns1123_subdomain, Dns1123Subdomain);
format_factory!(format_dns1035_label, Dns1035Label);
format_factory!(format_dns1035_label_prefix, Dns1035LabelPrefix);
format_factory!(format_dns1123_label_prefix, Dns1123LabelPrefix);
format_factory!(format_dns1123_subdomain_prefix, Dns1123SubdomainPrefix);
format_factory!(format_qualified_name, QualifiedName);
format_factory!(format_label_value, LabelValue);
format_factory!(format_uri, Uri);
format_factory!(format_uuid, Uuid);
format_factory!(format_byte, Byte);
format_factory!(format_date, Date);
format_factory!(format_datetime, DateTime);

// ---------------------------------------------------------------------------
// format.named(string)
// ---------------------------------------------------------------------------

/// `format.named(string) -> optional(Format)`
///
/// apiserver `library/format.go` binds this as `OptionalType(FormatType)`:
/// a known name yields `optional.of(format)`, an unknown one `optional.none()`.
/// The caller unwraps with `.value()` before `.validate()`. Returning the raw
/// Format (or null) instead would diverge from the apiserver.
fn format_named(name: Arc<String>) -> ResolveResult {
    let kind = match name.as_str() {
        "dns1123Label" => FormatKind::Dns1123Label,
        "dns1123Subdomain" => FormatKind::Dns1123Subdomain,
        "dns1035Label" => FormatKind::Dns1035Label,
        "dns1035LabelPrefix" => FormatKind::Dns1035LabelPrefix,
        "dns1123LabelPrefix" => FormatKind::Dns1123LabelPrefix,
        "dns1123SubdomainPrefix" => FormatKind::Dns1123SubdomainPrefix,
        "qualifiedName" => FormatKind::QualifiedName,
        "labelValue" => FormatKind::LabelValue,
        "uri" => FormatKind::Uri,
        "uuid" => FormatKind::Uuid,
        "byte" => FormatKind::Byte,
        "date" => FormatKind::Date,
        "datetime" => FormatKind::DateTime,
        _ => return Ok(Value::Opaque(Arc::new(OptionalValue::none()))),
    };
    Ok(Value::Opaque(Arc::new(OptionalValue::of(Value::Opaque(
        Arc::new(KubeFormat(kind)),
    )))))
}

// ---------------------------------------------------------------------------
// validate method
// ---------------------------------------------------------------------------

/// `<Format>.validate(<string>) -> list<string>`
pub(crate) fn format_validate(This(this): This<Value>, s: Arc<String>) -> ResolveResult {
    let fmt = match &this {
        Value::Opaque(o) => o
            .downcast_ref::<KubeFormat>()
            .ok_or_else(|| ExecutionError::function_error("validate", "expected Format type"))?,
        _ => {
            return Err(ExecutionError::function_error("validate", "expected Format type"));
        }
    };

    let errors = validate_format(&fmt.0, &s);
    if errors.is_empty() {
        Ok(Value::Opaque(Arc::new(OptionalValue::none())))
    } else {
        let list: Vec<Value> = errors.into_iter().map(|e| Value::String(Arc::new(e))).collect();
        Ok(Value::Opaque(Arc::new(OptionalValue::of(Value::List(Arc::new(
            list,
        ))))))
    }
}

// ---------------------------------------------------------------------------
// Validation logic per format
// ---------------------------------------------------------------------------

fn validate_format(kind: &FormatKind, s: &str) -> Vec<String> {
    match kind {
        FormatKind::Dns1123Label => validate_dns1123_label(s),
        FormatKind::Dns1123Subdomain => validate_dns1123_subdomain(s),
        FormatKind::Dns1035Label => validate_dns1035_label(s),
        FormatKind::Dns1035LabelPrefix => validate_dns1035_label_prefix(s),
        FormatKind::Dns1123LabelPrefix => validate_dns1123_label_prefix(s),
        FormatKind::Dns1123SubdomainPrefix => validate_dns1123_subdomain_prefix(s),
        FormatKind::QualifiedName => validate_qualified_name(s),
        FormatKind::LabelValue => validate_label_value(s),
        FormatKind::Uri => validate_uri(s),
        FormatKind::Uuid => validate_uuid(s),
        FormatKind::Byte => validate_byte(s),
        FormatKind::Date => validate_date(s),
        FormatKind::DateTime => validate_datetime(s),
    }
}

// -- DNS 1123 Label --

fn validate_dns1123_label(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_dns1123_label_char_set(s) {
        errors.push("must consist of lower case alphanumeric characters or '-'".to_string());
    }
    if s.starts_with('-') || s.ends_with('-') {
        errors.push("must start and end with an alphanumeric character".to_string());
    }
    errors
}

fn is_dns1123_label_char_set(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// -- DNS 1035 Label --

fn validate_dns1035_label(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_dns1035_char_set(s) {
        errors.push("must consist of lower case alphanumeric characters or '-'".to_string());
    }
    if !s.starts_with(|c: char| c.is_ascii_lowercase()) {
        errors.push("must start with a lowercase alphabetic character".to_string());
    }
    if s.ends_with('-') {
        errors.push("must end with an alphanumeric character".to_string());
    }
    errors
}

fn is_dns1035_char_set(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// -- DNS 1035 Label Prefix --

fn validate_dns1035_label_prefix(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    // Empty string is valid for prefix
    if s.is_empty() {
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_dns1035_char_set(s) {
        errors.push("must consist of lower case alphanumeric characters or '-'".to_string());
    }
    // Must start with a lowercase alphabetic character
    if !s.starts_with(|c: char| c.is_ascii_lowercase()) {
        errors.push("must start with a lowercase alphabetic character".to_string());
    }
    // Trailing hyphen is allowed (prefix), so no end check
    errors
}

// -- DNS 1123 Subdomain --

fn validate_dns1123_subdomain(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 253 {
        errors.push(format!("must be no more than 253 characters (is {})", s.len()));
    }
    for part in s.split('.') {
        let part_errors = validate_dns1123_label(part);
        errors.extend(part_errors);
    }
    errors
}

// -- DNS 1123 Label Prefix --

fn validate_dns1123_label_prefix(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_dns1123_label_char_set(s) {
        errors.push("must consist of lower case alphanumeric characters or '-'".to_string());
    }
    // Like dns1123Label but trailing hyphen is allowed
    if s.starts_with('-') {
        errors.push("must start with an alphanumeric character".to_string());
    }
    errors
}

// -- DNS 1123 Subdomain Prefix --

fn validate_dns1123_subdomain_prefix(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 253 {
        errors.push(format!("must be no more than 253 characters (is {})", s.len()));
    }
    let parts: Vec<&str> = s.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last label uses prefix rules (trailing hyphen allowed)
            let part_errors = validate_dns1123_label_prefix(part);
            errors.extend(part_errors);
        } else {
            let part_errors = validate_dns1123_label(part);
            errors.extend(part_errors);
        }
    }
    errors
}

// -- Qualified Name --

fn validate_qualified_name(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }

    let (prefix, name) = if let Some(slash_pos) = s.rfind('/') {
        let prefix = &s[..slash_pos];
        let name = &s[slash_pos + 1..];
        if prefix.is_empty() {
            errors.push("prefix must be non-empty".to_string());
        } else {
            let prefix_errors = validate_dns1123_subdomain(prefix);
            for e in prefix_errors {
                errors.push(format!("prefix: {e}"));
            }
        }
        (Some(prefix), name)
    } else {
        (None, s)
    };

    let name_errors = validate_qualified_name_local(name);
    if prefix.is_some() {
        for e in name_errors {
            errors.push(format!("name: {e}"));
        }
    } else {
        errors.extend(name_errors);
    }

    errors
}

fn validate_qualified_name_local(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_qualified_name_char_set(s) {
        errors.push("must consist of alphanumeric characters, '-', '_', or '.'".to_string());
    }
    if !s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        errors.push("must start with an alphanumeric character".to_string());
    }
    if !s.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        errors.push("must end with an alphanumeric character".to_string());
    }
    errors
}

fn is_qualified_name_char_set(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

// -- Label Value --

fn validate_label_value(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    // Empty string is valid for label values
    if s.is_empty() {
        return errors;
    }
    if s.len() > 63 {
        errors.push(format!("must be no more than 63 characters (is {})", s.len()));
    }
    if !is_label_value_char_set(s) {
        errors.push("must consist of alphanumeric characters, '-', '_', or '.'".to_string());
    }
    if !s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        errors.push("must start with an alphanumeric character".to_string());
    }
    if !s.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        errors.push("must end with an alphanumeric character".to_string());
    }
    errors
}

fn is_label_value_char_set(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

// -- URI --

fn validate_uri(s: &str) -> Vec<String> {
    // apiserver `format.uri()` is Go's `url.ParseRequestURI` (absolute URI or
    // absolute path), reporting its error.
    match crate::functions::go_url::parse_request_uri(s) {
        Ok(_) => Vec::new(),
        Err(e) => vec![e],
    }
}

// -- UUID --

fn validate_uuid(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    // 8-4-4-4-12 hex pattern
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        errors
            .push("must be in the form 8-4-4-4-12 (e.g., xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)".to_string());
        return errors;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    let mut valid = true;
    for (part, &expected_len) in parts.iter().zip(expected_lens.iter()) {
        if part.len() != expected_len || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            valid = false;
            break;
        }
    }
    if !valid {
        errors
            .push("must be in the form 8-4-4-4-12 (e.g., xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)".to_string());
    }
    errors
}

// -- Byte (base64) --

fn validate_byte(s: &str) -> Vec<String> {
    if is_std_base64(s) {
        Vec::new()
    } else {
        vec!["invalid base64".to_string()]
    }
}

/// Standard-alphabet base64 with required padding, matching the regex that
/// apiserver's `format.byte()` uses via `strfmt` → `govalidator.IsBase64`:
///
/// ```text
/// ^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{4})$
/// ```
///
/// Consequences vs. the old lenient check: the URL-safe alphabet (`-`/`_`) is
/// rejected, padding is mandatory (length must be a multiple of 4), and the
/// empty string is invalid — all three diverged before.
fn is_std_base64(s: &str) -> bool {
    // The final group is required, so the empty string never matches, and the
    // total length is always a multiple of 4.
    if s.is_empty() || !s.len().is_multiple_of(4) {
        return false;
    }
    let bytes = s.as_bytes();
    let n = bytes.len();
    // Count trailing '=' padding (the regex permits only one or two, at the end).
    let pad = if bytes[n - 1] == b'=' {
        if bytes[n - 2] == b'=' { 2 } else { 1 }
    } else {
        0
    };
    // Every non-padding byte must be in the standard base64 alphabet; any stray
    // '=' outside the final group falls here and fails.
    bytes[..n - pad]
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
}

// -- Date --

/// A non-empty run of ASCII digits. Go's `time.Parse` (and strfmt's RFC3339
/// regex `[0-9]{2}`) accept only digits in numeric fields, but Rust's
/// `parse::<u32>` also accepts a leading `+` (e.g. `"+024"` → `24`); this guard
/// closes that gap so date/time components match the apiserver.
fn all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// kube-openapi's `strfmt.IsDate`, Go's `time.Parse("2006-01-02", s)`, with the
/// apiserver's message.
fn validate_date(s: &str) -> Vec<String> {
    if is_date(s) {
        Vec::new()
    } else {
        vec!["invalid date".to_string()]
    }
}

fn is_date(s: &str) -> bool {
    // YYYY-MM-DD, digits only, and a day that exists in that month.
    let bytes = s.as_bytes();
    if s.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (year_str, month_str, day_str) = (&s[0..4], &s[5..7], &s[8..10]);
    if !all_ascii_digits(year_str) || !all_ascii_digits(month_str) || !all_ascii_digits(day_str) {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        year_str.parse::<u32>(),
        month_str.parse::<u32>(),
        day_str.parse::<u32>(),
    ) else {
        return false;
    };
    (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

// -- DateTime --

/// kube-openapi's `strfmt.IsDateTime`, with the apiserver's message. It is not
/// RFC 3339: the string is lowercased and split on every `t`, and only the
/// first two pieces are read (`2024-01-15T10:30:00ZtXYZ` passes); the time
/// matches `^([0-9]{2}):([0-9]{2}):([0-9]{2})(.[0-9]+)?(z|([+-][0-9]{2}:[0-9]{2}))$`,
/// whose `.` is any character but a newline (`10:30:00,5Z`, `10:30:0012Z`), and
/// the offset is not range-checked (`+99:99`); only the hour, minute and second
/// are (`<= "23"`, `<= "59"`, `<= "59"`).
fn validate_datetime(s: &str) -> Vec<String> {
    if is_date_time(s) {
        Vec::new()
    } else {
        vec!["invalid datetime".to_string()]
    }
}

fn is_date_time(s: &str) -> bool {
    // Lowercasing only matters for `T` and `Z`: no other character lowercases
    // to `t`, `z`, a digit, `:`, `+`, `-` or a newline, and Go's simple case
    // mapping keeps one character per character.
    let mut pieces = s.split(['T', 't']);
    let date = pieces.next().unwrap_or_default();
    let Some(time) = pieces.next() else {
        return false;
    };
    is_date(date) && is_strfmt_time(time)
}

/// The time piece of [`is_date_time`].
fn is_strfmt_time(t: &str) -> bool {
    let b = t.as_bytes();
    let two_digits = |i: usize| b.get(i..i + 2).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if !(two_digits(0)
        && b.get(2) == Some(&b':')
        && two_digits(3)
        && b.get(5) == Some(&b':')
        && two_digits(6))
    {
        return false;
    }
    // Two ASCII digits compare as strings the way they do as numbers.
    if &t[0..2] > "23" || &t[3..5] > "59" || &t[6..8] > "59" {
        return false;
    }
    let rest = &t[8..];
    // The zone ends the string: `z` (or `Z`), else `[+-]dd:dd`.
    let fraction = match rest.strip_suffix(['z', 'Z']) {
        Some(fraction) => fraction,
        None => {
            let Some(at) = rest.len().checked_sub(6) else {
                return false;
            };
            let zone = &rest.as_bytes()[at..];
            let offset = matches!(zone[0], b'+' | b'-')
                && zone[1].is_ascii_digit()
                && zone[2].is_ascii_digit()
                && zone[3] == b':'
                && zone[4].is_ascii_digit()
                && zone[5].is_ascii_digit();
            if !offset {
                return false;
            }
            // The zone is ASCII, so `at` is a character boundary.
            &rest[..at]
        }
    };
    // `(.[0-9]+)?`: nothing, or any character but a newline and then digits.
    let mut chars = fraction.chars();
    match chars.next() {
        None => true,
        Some('\n') => false,
        Some(_) => {
            let digits = chars.as_str();
            !digits.is_empty() && digits.bytes().all(|d| d.is_ascii_digit())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    fn assert_valid(expr: &str) {
        // valid → optional.none() → hasValue() == false
        assert_eq!(
            eval(&format!("{expr}.hasValue()")),
            Value::Bool(false),
            "expected valid (optional.none) for {expr}"
        );
    }

    fn assert_invalid(expr: &str) {
        // invalid → optional.of([...]) → hasValue() == true
        assert_eq!(
            eval(&format!("{expr}.hasValue()")),
            Value::Bool(true),
            "expected invalid (optional.of) for {expr}"
        );
    }

    // -- DNS 1123 Label --

    #[test]
    fn test_dns1123_label_valid() {
        assert_valid("format.dns1123Label().validate('my-name')");
        assert_valid("format.dns1123Label().validate('a')");
        assert_valid("format.dns1123Label().validate('abc123')");
        assert_valid("format.dns1123Label().validate('a-b')");
    }

    #[test]
    fn test_dns1123_label_invalid() {
        assert_invalid("format.dns1123Label().validate('')");
        assert_invalid("format.dns1123Label().validate('-start')");
        assert_invalid("format.dns1123Label().validate('end-')");
        assert_invalid("format.dns1123Label().validate('UPPER')");
        assert_invalid("format.dns1123Label().validate('has.dot')");
        // Too long (64 chars)
        let long = "a".repeat(64);
        assert_invalid(&format!("format.dns1123Label().validate('{long}')"));
    }

    #[test]
    fn test_dns1123_label_max_length() {
        let max = "a".repeat(63);
        assert_valid(&format!("format.dns1123Label().validate('{max}')"));
    }

    // -- DNS 1035 Label --

    #[test]
    fn test_dns1035_label_valid() {
        assert_valid("format.dns1035Label().validate('my-name')");
        assert_valid("format.dns1035Label().validate('a')");
        assert_valid("format.dns1035Label().validate('abc')");
    }

    #[test]
    fn test_dns1035_label_invalid() {
        assert_invalid("format.dns1035Label().validate('')");
        assert_invalid("format.dns1035Label().validate('1start')"); // must start with letter
        assert_invalid("format.dns1035Label().validate('end-')");
        assert_invalid("format.dns1035Label().validate('UPPER')");
    }

    // -- DNS 1123 Subdomain --

    #[test]
    fn test_dns1123_subdomain_valid() {
        assert_valid("format.dns1123Subdomain().validate('example.com')");
        assert_valid("format.dns1123Subdomain().validate('my-app.example.com')");
        assert_valid("format.dns1123Subdomain().validate('a')");
    }

    #[test]
    fn test_dns1123_subdomain_invalid() {
        assert_invalid("format.dns1123Subdomain().validate('')");
        assert_invalid("format.dns1123Subdomain().validate('.leading.dot')");
        assert_invalid("format.dns1123Subdomain().validate('trailing.dot.')");
        // Too long (254 chars)
        let long = format!("{}.{}", "a".repeat(63), "b".repeat(63 * 3));
        if long.len() > 253 {
            assert_invalid(&format!("format.dns1123Subdomain().validate('{long}')"));
        }
    }

    // -- DNS 1123 Label Prefix --

    #[test]
    fn test_dns1123_label_prefix_valid() {
        assert_valid("format.dns1123LabelPrefix().validate('my-name')");
        assert_valid("format.dns1123LabelPrefix().validate('my-name-')"); // trailing hyphen OK
        assert_valid("format.dns1123LabelPrefix().validate('a')");
    }

    #[test]
    fn test_dns1123_label_prefix_invalid() {
        assert_invalid("format.dns1123LabelPrefix().validate('')");
        assert_invalid("format.dns1123LabelPrefix().validate('-start')");
        assert_invalid("format.dns1123LabelPrefix().validate('UPPER')");
    }

    // -- DNS 1035 Label Prefix --

    #[test]
    fn test_dns1035_label_prefix_valid() {
        assert_valid("format.dns1035LabelPrefix().validate('my-prefix-')");
        assert_valid("format.dns1035LabelPrefix().validate('')"); // empty is valid
        assert_valid("format.dns1035LabelPrefix().validate('a')");
        assert_valid("format.dns1035LabelPrefix().validate('ok-')");
    }

    #[test]
    fn test_dns1035_label_prefix_invalid() {
        assert_invalid("format.dns1035LabelPrefix().validate('1start-')"); // must start with letter
        assert_invalid("format.dns1035LabelPrefix().validate('UPPER')"); // uppercase
    }

    #[test]
    fn test_dns1035_label_prefix_via_named() {
        assert_valid("format.named('dns1035LabelPrefix').value().validate('ok-')");
    }

    // -- DNS 1123 Subdomain Prefix --

    #[test]
    fn test_dns1123_subdomain_prefix_valid() {
        assert_valid("format.dns1123SubdomainPrefix().validate('example.com')");
        assert_valid("format.dns1123SubdomainPrefix().validate('my-app.example-')"); // last label prefix
        assert_valid("format.dns1123SubdomainPrefix().validate('a')");
    }

    #[test]
    fn test_dns1123_subdomain_prefix_invalid() {
        assert_invalid("format.dns1123SubdomainPrefix().validate('')");
        // Non-last label cannot have trailing hyphen
        assert_invalid("format.dns1123SubdomainPrefix().validate('bad-.example')");
    }

    // -- Qualified Name --

    #[test]
    fn test_qualified_name_valid() {
        assert_valid("format.qualifiedName().validate('my-name')");
        assert_valid("format.qualifiedName().validate('example.com/my-name')");
        assert_valid("format.qualifiedName().validate('my.name')");
        assert_valid("format.qualifiedName().validate('my_name')");
        assert_valid("format.qualifiedName().validate('A-Za-z0')");
    }

    #[test]
    fn test_qualified_name_invalid() {
        assert_invalid("format.qualifiedName().validate('')");
        assert_invalid("format.qualifiedName().validate('/name')"); // empty prefix
        assert_invalid("format.qualifiedName().validate('prefix/')"); // empty name
        assert_invalid("format.qualifiedName().validate('.bad/name')"); // invalid prefix
        assert_invalid("format.qualifiedName().validate('prefix/.bad')"); // invalid name start
    }

    // -- Label Value --

    #[test]
    fn test_label_value_valid() {
        assert_valid("format.labelValue().validate('')"); // empty is valid
        assert_valid("format.labelValue().validate('a')");
        assert_valid("format.labelValue().validate('my-value')");
        assert_valid("format.labelValue().validate('my.value')");
        assert_valid("format.labelValue().validate('my_value')");
        assert_valid("format.labelValue().validate('MyValue')");
    }

    #[test]
    fn test_label_value_invalid() {
        assert_invalid("format.labelValue().validate('-start')");
        assert_invalid("format.labelValue().validate('end-')");
        assert_invalid("format.labelValue().validate('has space')");
        let long = "a".repeat(64);
        assert_invalid(&format!("format.labelValue().validate('{long}')"));
    }

    // -- URI --

    #[test]
    fn test_uri_valid() {
        assert_valid("format.uri().validate('https://example.com')");
        assert_valid("format.uri().validate('http://example.com/path')");
        assert_valid("format.uri().validate('ftp://files.example.com')");
        assert_valid("format.uri().validate('urn:isbn:0451450523')");
        assert_valid("format.uri().validate('mailto:user@example.com')");
        // apiserver's url.ParseRequestURI accepts an absolute path, not just an
        // absolute URI — the old scheme-required check wrongly rejected this.
        assert_valid("format.uri().validate('/absolute/path')");
    }

    #[test]
    fn test_uri_invalid() {
        assert_invalid("format.uri().validate('')");
        assert_invalid("format.uri().validate('no-scheme')");
        assert_invalid("format.uri().validate('://missing-scheme')");
    }

    // -- UUID --

    #[test]
    fn test_uuid_valid() {
        assert_valid("format.uuid().validate('550e8400-e29b-41d4-a716-446655440000')");
        assert_valid("format.uuid().validate('550E8400-E29B-41D4-A716-446655440000')"); // uppercase
    }

    #[test]
    fn test_uuid_invalid() {
        assert_invalid("format.uuid().validate('')");
        assert_invalid("format.uuid().validate('not-a-uuid')");
        assert_invalid("format.uuid().validate('550e8400-e29b-41d4-a716')"); // too short
        assert_invalid("format.uuid().validate('550e8400-e29b-41d4-a716-44665544000g')"); // non-hex
    }

    // -- Byte (base64) --

    #[test]
    fn test_byte_valid() {
        // Standard-alphabet base64 with correct padding (govalidator.IsBase64).
        assert_valid("format.byte().validate('aGVsbG8=')"); // "hello"
        assert_valid("format.byte().validate('YQ==')"); // "a"
        assert_valid("format.byte().validate('YWI=')"); // "ab"
        assert_valid("format.byte().validate('YWJjZA==')"); // "abcd"
    }

    #[test]
    fn test_byte_invalid() {
        assert_invalid("format.byte().validate('not valid!')");
        assert_invalid("format.byte().validate('abc===')"); // too much padding
        // strfmt/govalidator rejects these that the old lenient check accepted:
        assert_invalid("format.byte().validate('')"); // empty (final group is required)
        assert_invalid("format.byte().validate('aGVsbG8')"); // missing padding (len 7)
        assert_invalid("format.byte().validate('aGVsbG8-')"); // URL-safe '-'
        assert_invalid("format.byte().validate('aGVsbG8_')"); // URL-safe '_'
    }

    // -- Date --

    #[test]
    fn test_date_valid() {
        assert_valid("format.date().validate('2024-01-15')");
        assert_valid("format.date().validate('2024-02-29')"); // leap year
        assert_valid("format.date().validate('2024-12-31')");
    }

    #[test]
    fn test_date_invalid() {
        assert_invalid("format.date().validate('')");
        assert_invalid("format.date().validate('2024-13-01')"); // month > 12
        assert_invalid("format.date().validate('2023-02-29')"); // not leap year
        assert_invalid("format.date().validate('2024-1-1')"); // wrong format
        assert_invalid("format.date().validate('not-a-date')");
        // Go's time.Parse rejects '+'-prefixed components; Rust's parse::<u32>
        // would accept them, so these must be rejected explicitly.
        assert_invalid("format.date().validate('+024-01-01')"); // '+' year
        assert_invalid("format.date().validate('2024-+1-01')"); // '+' month
        assert_invalid("format.date().validate('2024-01-+1')"); // '+' day
    }

    // -- DateTime --

    #[test]
    fn test_datetime_valid() {
        assert_valid("format.datetime().validate('2024-01-15T10:30:00Z')");
        assert_valid("format.datetime().validate('2024-01-15T10:30:00+09:00')");
        assert_valid("format.datetime().validate('2024-01-15T10:30:00-05:00')");
        assert_valid("format.datetime().validate('2024-01-15T10:30:00.123Z')");
        assert_valid("format.datetime().validate('2024-01-15t10:30:00z')"); // lowercase
    }

    #[test]
    fn test_datetime_invalid() {
        assert_invalid("format.datetime().validate('')");
        assert_invalid("format.datetime().validate('2024-01-15')"); // no time
        assert_invalid("format.datetime().validate('2024-01-15T10:30:00')"); // no tz
        assert_invalid("format.datetime().validate('not-a-datetime')");
        assert_invalid("format.datetime().validate('2024-01-15T25:00:00Z')"); // hour > 23
        // strfmt's regex caps seconds at "59"; leap seconds are rejected.
        assert_invalid("format.datetime().validate('2024-01-15T23:59:60Z')"); // sec == 60
        // '+'-prefixed time component (Rust parse accepts, strfmt regex rejects).
        assert_invalid("format.datetime().validate('2024-01-15T+1:30:00Z')"); // '+' hour
    }

    #[test]
    fn test_datetime_non_ascii_time() {
        // 'é' straddles byte 8 of the time; slicing the str there panicked.
        assert_invalid("format.datetime().validate('2024-01-15T12:30:0é:00Z')");
        assert_invalid("format.datetime().validate('2024-01-15T1é:30:00Z')");
        // After the seconds, strfmt's `.` takes any character but a newline.
        assert_valid("format.datetime().validate('2024-01-15T12:30:00é00Z')");
        assert_invalid("format.datetime().validate('2024-01-15T12:30:00\\n00Z')");
    }

    #[test]
    fn test_datetime_follows_strfmt() {
        // kube-openapi's IsDateTime, not RFC 3339 (each checked against the
        // apiserver).
        for valid in [
            "2024-01-15T10:30:00+99:99",
            "2024-01-15T10:30:00,5Z",
            "2024-01-15T10:30:0012Z",
            "2024-01-15T10:30:00ZtXYZ",
            "2024-01-15t10:30:00z",
            "2024-01-15T10:30:00İ5Z",
        ] {
            assert_valid(&format!("format.datetime().validate('{valid}')"));
        }
        for invalid in [
            "2024-01-15T10:30:00",
            "2024-01-15T24:00:00Z",
            "2024-01-15T23:59:60Z",
            "2024-01-15T10:30:00.Z",
            "2024-01-15T10:30:00+0530",
            "2024-01-15tt10:30:00Z",
            "2023-02-29T10:30:00Z",
        ] {
            assert_invalid(&format!("format.datetime().validate('{invalid}')"));
        }
        assert_eq!(
            eval("format.datetime().validate('x').value()"),
            eval("['invalid datetime']")
        );
        assert_eq!(
            eval("format.date().validate('x').value()"),
            eval("['invalid date']")
        );
    }

    // -- format.named() --

    #[test]
    fn test_format_named_known() {
        // apiserver: format.named(x) -> optional(Format); unwrap with .value().
        assert_valid("format.named('dns1123Label').value().validate('my-name')");
        assert_valid("format.named('uuid').value().validate('550e8400-e29b-41d4-a716-446655440000')");
    }

    #[test]
    fn test_format_named_known_has_value() {
        // A known name yields optional.of(format).
        assert_eq!(eval("format.named('dns1123Label').hasValue()"), Value::Bool(true));
    }

    #[test]
    fn test_format_named_unknown() {
        // An unknown name yields optional.none(), not null.
        assert_eq!(eval("format.named('unknown').hasValue()"), Value::Bool(false));
    }

    // -- validate return type --

    #[test]
    fn test_validate_returns_optional() {
        // valid → optional.none()
        assert_eq!(
            eval("format.dns1123Label().validate('valid-name').hasValue()"),
            Value::Bool(false)
        );
        // invalid → optional.of([...])
        assert_eq!(
            eval("format.dns1123Label().validate('').hasValue()"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_validate_has_value_pattern() {
        // K8s pattern: !format.dns1123Label().validate(name).hasValue()
        assert_eq!(
            eval("!format.dns1123Label().validate('valid').hasValue()"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("!format.dns1123Label().validate('').hasValue()"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_validate_error_list_accessible() {
        // Can extract error list via value()
        assert_eq!(
            eval("format.dns1123Label().validate('').value().size() > 0"),
            Value::Bool(true)
        );
    }
}
