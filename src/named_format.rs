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

use cel::extractors::This;
use cel::objects::{Opaque, OptionalValue, Value};
use cel::{Context, ExecutionError, ResolveResult};
use std::fmt;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Format kind enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormatKind {
    Dns1123Label,
    Dns1123Subdomain,
    Dns1035Label,
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
pub fn register(ctx: &mut Context<'_>) {
    // Factory functions
    ctx.add_function("format.dns1123Label", format_dns1123_label);
    ctx.add_function("format.dns1123Subdomain", format_dns1123_subdomain);
    ctx.add_function("format.dns1035Label", format_dns1035_label);
    ctx.add_function("format.dns1123LabelPrefix", format_dns1123_label_prefix);
    ctx.add_function(
        "format.dns1123SubdomainPrefix",
        format_dns1123_subdomain_prefix,
    );
    ctx.add_function("format.qualifiedName", format_qualified_name);
    ctx.add_function("format.labelValue", format_label_value);
    ctx.add_function("format.uri", format_uri);
    ctx.add_function("format.uuid", format_uuid);
    ctx.add_function("format.byte", format_byte);
    ctx.add_function("format.date", format_date);
    ctx.add_function("format.datetime", format_datetime);
    ctx.add_function("format.named", format_named);

    // Validate method
    ctx.add_function("validate", format_validate);
}

// ---------------------------------------------------------------------------
// Factory functions
// ---------------------------------------------------------------------------

fn format_dns1123_label() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::Dns1123Label,
    ))))
}

fn format_dns1123_subdomain() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::Dns1123Subdomain,
    ))))
}

fn format_dns1035_label() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::Dns1035Label,
    ))))
}

fn format_dns1123_label_prefix() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::Dns1123LabelPrefix,
    ))))
}

fn format_dns1123_subdomain_prefix() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::Dns1123SubdomainPrefix,
    ))))
}

fn format_qualified_name() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(
        FormatKind::QualifiedName,
    ))))
}

fn format_label_value() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::LabelValue))))
}

fn format_uri() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::Uri))))
}

fn format_uuid() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::Uuid))))
}

fn format_byte() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::Byte))))
}

fn format_date() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::Date))))
}

fn format_datetime() -> ResolveResult {
    Ok(Value::Opaque(Arc::new(KubeFormat(FormatKind::DateTime))))
}

// ---------------------------------------------------------------------------
// format.named(string)
// ---------------------------------------------------------------------------

fn format_named(name: Arc<String>) -> ResolveResult {
    let kind = match name.as_str() {
        "dns1123Label" => FormatKind::Dns1123Label,
        "dns1123Subdomain" => FormatKind::Dns1123Subdomain,
        "dns1035Label" => FormatKind::Dns1035Label,
        "dns1123LabelPrefix" => FormatKind::Dns1123LabelPrefix,
        "dns1123SubdomainPrefix" => FormatKind::Dns1123SubdomainPrefix,
        "qualifiedName" => FormatKind::QualifiedName,
        "labelValue" => FormatKind::LabelValue,
        "uri" => FormatKind::Uri,
        "uuid" => FormatKind::Uuid,
        "byte" => FormatKind::Byte,
        "date" => FormatKind::Date,
        "datetime" => FormatKind::DateTime,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Opaque(Arc::new(KubeFormat(kind))))
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
            return Err(ExecutionError::function_error(
                "validate",
                "expected Format type",
            ));
        }
    };

    let errors = validate_format(&fmt.0, &s);
    if errors.is_empty() {
        Ok(Value::Opaque(Arc::new(OptionalValue::none())))
    } else {
        let list: Vec<Value> = errors
            .into_iter()
            .map(|e| Value::String(Arc::new(e)))
            .collect();
        Ok(Value::Opaque(Arc::new(OptionalValue::of(Value::List(
            Arc::new(list),
        )))))
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
        errors.push(format!(
            "must be no more than 63 characters (is {})",
            s.len()
        ));
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
        errors.push(format!(
            "must be no more than 63 characters (is {})",
            s.len()
        ));
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

// -- DNS 1123 Subdomain --

fn validate_dns1123_subdomain(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    if s.len() > 253 {
        errors.push(format!(
            "must be no more than 253 characters (is {})",
            s.len()
        ));
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
        errors.push(format!(
            "must be no more than 63 characters (is {})",
            s.len()
        ));
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
        errors.push(format!(
            "must be no more than 253 characters (is {})",
            s.len()
        ));
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
        errors.push(format!(
            "must be no more than 63 characters (is {})",
            s.len()
        ));
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
        errors.push(format!(
            "must be no more than 63 characters (is {})",
            s.len()
        ));
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
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be non-empty".to_string());
        return errors;
    }
    // Must have a scheme (letters followed by ':')
    let scheme_end = s.find(':');
    match scheme_end {
        None => {
            errors.push("must have a scheme (e.g., 'https:')".to_string());
        }
        Some(pos) => {
            let scheme = &s[..pos];
            if scheme.is_empty()
                || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                || !scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            {
                errors.push("invalid scheme".to_string());
            }
        }
    }
    errors
}

// -- UUID --

fn validate_uuid(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    // 8-4-4-4-12 hex pattern
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        errors.push(
            "must be in the form 8-4-4-4-12 (e.g., xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)"
                .to_string(),
        );
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
        errors.push(
            "must be in the form 8-4-4-4-12 (e.g., xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)"
                .to_string(),
        );
    }
    errors
}

// -- Byte (base64) --

fn validate_byte(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        return errors;
    }
    // Strip padding
    let stripped = s.trim_end_matches('=');
    // Check all chars are base64 (standard or URL-safe)
    let valid = stripped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '-' || c == '_');
    if !valid {
        errors.push("must be a valid base64 encoded string".to_string());
        return errors;
    }
    // Check padding is correct
    let padding_count = s.len() - stripped.len();
    if padding_count > 2 {
        errors.push("must be a valid base64 encoded string".to_string());
        return errors;
    }
    // Check length alignment: base64 without padding should have len % 4 ∈ {0, 2, 3}
    let remainder = stripped.len() % 4;
    if remainder == 1 {
        errors.push("must be a valid base64 encoded string".to_string());
        return errors;
    }
    // If padding is present, total length must be multiple of 4
    if padding_count > 0 && !s.len().is_multiple_of(4) {
        errors.push("must be a valid base64 encoded string".to_string());
    }
    errors
}

// -- Date --

fn validate_date(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    // YYYY-MM-DD format
    if s.len() != 10 {
        errors.push("must be a date in YYYY-MM-DD format".to_string());
        return errors;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        errors.push("must be a date in YYYY-MM-DD format".to_string());
        return errors;
    }
    let year_str = &s[0..4];
    let month_str = &s[5..7];
    let day_str = &s[8..10];

    let Ok(year) = year_str.parse::<u32>() else {
        errors.push("must be a date in YYYY-MM-DD format".to_string());
        return errors;
    };
    let Ok(month) = month_str.parse::<u32>() else {
        errors.push("must be a date in YYYY-MM-DD format".to_string());
        return errors;
    };
    let Ok(day) = day_str.parse::<u32>() else {
        errors.push("must be a date in YYYY-MM-DD format".to_string());
        return errors;
    };

    if !(1..=12).contains(&month) {
        errors.push("must be a valid date".to_string());
        return errors;
    }

    let max_day = days_in_month(year, month);
    if day < 1 || day > max_day {
        errors.push("must be a valid date".to_string());
    }
    errors
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

// -- DateTime (RFC 3339) --

fn validate_datetime(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.is_empty() {
        errors.push("must be a valid RFC 3339 date-time".to_string());
        return errors;
    }

    // Find the 'T' or 't' separator
    let t_pos = s.find(['T', 't']);
    let Some(t_pos) = t_pos else {
        errors.push("must be a valid RFC 3339 date-time".to_string());
        return errors;
    };

    // Validate date part
    let date_part = &s[..t_pos];
    let date_errors = validate_date(date_part);
    if !date_errors.is_empty() {
        errors.push("must be a valid RFC 3339 date-time".to_string());
        return errors;
    }

    // Parse time part (after T)
    let time_part = &s[t_pos + 1..];
    if time_part.is_empty() {
        errors.push("must be a valid RFC 3339 date-time".to_string());
        return errors;
    }

    // Find timezone offset
    let (time_str, tz_str) = find_timezone(time_part);

    // Timezone is required
    if tz_str.is_empty() {
        errors.push("must have a timezone".to_string());
        return errors;
    }

    // Validate time
    if !validate_time_str(time_str) {
        errors.push("must be a valid RFC 3339 date-time".to_string());
        return errors;
    }

    // Validate timezone
    if !validate_timezone(tz_str) {
        errors.push("must be a valid RFC 3339 date-time".to_string());
    }

    errors
}

fn find_timezone(time_part: &str) -> (&str, &str) {
    // Z or z
    if let Some(pos) = time_part.rfind(['Z', 'z']) {
        return (&time_part[..pos], &time_part[pos..]);
    }
    // +HH:MM or -HH:MM — search from end, skip fractional seconds
    // Look for last '+' or '-' that is after the time digits
    for (i, ch) in time_part.char_indices().rev() {
        if (ch == '+' || ch == '-') && i >= 2 {
            return (&time_part[..i], &time_part[i..]);
        }
    }
    (time_part, "")
}

fn validate_time_str(s: &str) -> bool {
    if s.len() < 8 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[2] != b':' || bytes[5] != b':' {
        return false;
    }
    let Ok(hour) = s[0..2].parse::<u32>() else {
        return false;
    };
    let Ok(min) = s[3..5].parse::<u32>() else {
        return false;
    };
    let Ok(sec) = s[6..8].parse::<u32>() else {
        return false;
    };
    if hour > 23 || min > 59 || sec > 60 {
        // sec=60 for leap seconds
        return false;
    }
    // Check fractional seconds if present
    if s.len() > 8 {
        if bytes[8] != b'.' {
            return false;
        }
        let frac = &s[9..];
        if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

fn validate_timezone(tz: &str) -> bool {
    if tz == "Z" || tz == "z" {
        return true;
    }
    // +HH:MM or -HH:MM
    if tz.len() != 6 {
        return false;
    }
    let bytes = tz.as_bytes();
    if bytes[0] != b'+' && bytes[0] != b'-' {
        return false;
    }
    if bytes[3] != b':' {
        return false;
    }
    let Ok(hour) = tz[1..3].parse::<u32>() else {
        return false;
    };
    let Ok(min) = tz[4..6].parse::<u32>() else {
        return false;
    };
    hour <= 23 && min <= 59
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
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
        assert_valid("format.byte().validate('')"); // empty is valid
        assert_valid("format.byte().validate('aGVsbG8=')"); // "hello"
        assert_valid("format.byte().validate('aGVsbG8')"); // no padding
        assert_valid("format.byte().validate('YQ==')"); // "a"
        assert_valid("format.byte().validate('YWI=')"); // "ab"
        assert_valid("format.byte().validate('aGVsbG8-')"); // url-safe
        assert_valid("format.byte().validate('aGVsbG8_')"); // url-safe
    }

    #[test]
    fn test_byte_invalid() {
        assert_invalid("format.byte().validate('not valid!')");
        assert_invalid("format.byte().validate('abc===')"); // too much padding
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
    }

    // -- DateTime (RFC 3339) --

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
    }

    // -- format.named() --

    #[test]
    fn test_format_named_known() {
        assert_valid("format.named('dns1123Label').validate('my-name')");
        assert_valid("format.named('uuid').validate('550e8400-e29b-41d4-a716-446655440000')");
    }

    #[test]
    fn test_format_named_unknown() {
        assert_eq!(eval("format.named('unknown')"), Value::Null);
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
