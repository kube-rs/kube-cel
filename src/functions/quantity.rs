//! Kubernetes CEL quantity extension functions.
//!
//! Provides parsing, comparison, and arithmetic for Kubernetes resource quantities
//! (e.g., "1.5Gi", "500m", "100n"), matching `k8s.io/apiserver/pkg/cel/library/quantity.go`.

use cel::{
    ExecutionError, ResolveResult,
    extractors::{Arguments, This},
    objects::{Opaque, Value},
};
use std::{cmp::Ordering, fmt, sync::Arc};

use crate::functions::Registrar;

// ---------------------------------------------------------------------------
// Internal representation
// ---------------------------------------------------------------------------

/// A Kubernetes resource quantity.
///
/// Stored as `mantissa * 10^scale` to allow exact decimal arithmetic.
/// Binary SI suffixes (Ki, Mi, …) are converted to their decimal value at
/// parse time so that all quantities share a common representation.
///
/// `scale` is an `i64` although a parsed exponent fits `i32`: the decimal
/// places and the suffix are added to it, and `simplify` raises it, so an
/// exponent near the `i32` bounds (`0.01e-2147483647`) stays exact instead of
/// overflowing.
#[derive(Debug, Clone, Eq)]
pub struct KubeQuantity {
    mantissa: i128,
    scale: i64,
}

impl PartialEq for KubeQuantity {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for KubeQuantity {
    fn cmp(&self, other: &Self) -> Ordering {
        match normalize_pair(self, other) {
            Some((a, b)) => a.cmp(&b),
            // Aligning the scales overflowed i128 (magnitudes beyond ~10^38).
            None => cmp_digits(self, other),
        }
    }
}

/// Exact comparison that never aligns scales in `i128`: by sign, then by the
/// magnitudes' decimal digits (the apiserver's `inf.Dec` compare is exact, and
/// an `f64` fallback called quantities 3 apart near `i128::MAX` equal).
fn cmp_digits(a: &KubeQuantity, b: &KubeQuantity) -> Ordering {
    let by_sign = a.sign().cmp(&b.sign());
    if by_sign != Ordering::Equal || a.mantissa == 0 {
        return by_sign;
    }
    let min_scale = a.scale.min(b.scale);
    let (da, db) = (
        a.mantissa.unsigned_abs().to_string(),
        b.mantissa.unsigned_abs().to_string(),
    );
    // Digit count once both are written at `min_scale`: the mantissa's digits
    // plus the zeros its larger scale adds.
    let width = |d: &str, q: &KubeQuantity| d.len() as i64 + q.scale - min_scale;
    let magnitude = width(&da, a).cmp(&width(&db, b)).then_with(|| {
        // Same width: compare digit by digit; past a mantissa's end its digits are 0.
        let n = da.len().max(db.len());
        let digits = |d: &str| {
            d.bytes()
                .chain(std::iter::repeat(b'0'))
                .take(n)
                .collect::<Vec<u8>>()
        };
        digits(&da).cmp(&digits(&db))
    });
    if a.mantissa < 0 {
        magnitude.reverse()
    } else {
        magnitude
    }
}

impl PartialOrd for KubeQuantity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for KubeQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale >= 0 {
            let mut s = self.mantissa.to_string();
            for _ in 0..self.scale {
                s.push('0');
            }
            write!(f, "{s}")
        } else {
            let abs_scale = usize::try_from(self.scale.unsigned_abs()).unwrap_or(usize::MAX);
            let sign = if self.mantissa < 0 { "-" } else { "" };
            let abs_mantissa = self.mantissa.unsigned_abs();
            let digits = abs_mantissa.to_string();
            if digits.len() <= abs_scale {
                let zeros = abs_scale - digits.len();
                write!(f, "{sign}0.{}{digits}", "0".repeat(zeros))
            } else {
                let split = digits.len() - abs_scale;
                write!(f, "{sign}{}.{}", &digits[..split], &digits[split..])
            }
        }
    }
}

impl Opaque for KubeQuantity {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.Quantity"
    }
}

impl KubeQuantity {
    fn new(mantissa: i128, scale: i64) -> Self {
        let mut q = KubeQuantity { mantissa, scale };
        q.simplify();
        q
    }

    /// Remove trailing zeros from mantissa by increasing scale.
    fn simplify(&mut self) {
        if self.mantissa == 0 {
            self.scale = 0;
            return;
        }
        while self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale += 1;
        }
    }

    fn sign(&self) -> i64 {
        match self.mantissa.cmp(&0) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    fn is_integer(&self) -> bool {
        if self.scale >= 0 {
            return true;
        }
        // Check if mantissa is divisible by 10^(-scale)
        match pow10(-self.scale) {
            Some(d) => self.mantissa % d == 0,
            None => false,
        }
    }

    fn as_integer(&self) -> Result<i64, ExecutionError> {
        if self.scale >= 0 {
            let multiplier = pow10(self.scale).ok_or_else(|| {
                ExecutionError::function_error("asInteger", "quantity too large for integer")
            })?;
            let val = self.mantissa.checked_mul(multiplier).ok_or_else(|| {
                ExecutionError::function_error("asInteger", "quantity too large for integer")
            })?;
            i64::try_from(val)
                .map_err(|_| ExecutionError::function_error("asInteger", "quantity too large for integer"))
        } else {
            let divisor = pow10(-self.scale).ok_or_else(|| {
                ExecutionError::function_error("asInteger", "quantity too large for integer")
            })?;
            if self.mantissa % divisor != 0 {
                return Err(ExecutionError::function_error(
                    "asInteger",
                    "quantity is not an integer",
                ));
            }
            let val = self.mantissa / divisor;
            i64::try_from(val)
                .map_err(|_| ExecutionError::function_error("asInteger", "quantity too large for integer"))
        }
    }

    fn as_approximate_float(&self) -> f64 {
        // Past the i32 range the power is 0 or infinite either way.
        let exp = i32::try_from(self.scale).unwrap_or(if self.scale < 0 { i32::MIN } else { i32::MAX });
        self.mantissa as f64 * 10f64.powi(exp)
    }

    fn add(&self, other: &KubeQuantity) -> Option<KubeQuantity> {
        let min_scale = self.scale.min(other.scale);
        let a = scale_mantissa(self.mantissa, self.scale, min_scale)?;
        let b = scale_mantissa(other.mantissa, other.scale, min_scale)?;
        Some(KubeQuantity::new(a.checked_add(b)?, min_scale))
    }

    fn sub(&self, other: &KubeQuantity) -> Option<KubeQuantity> {
        let min_scale = self.scale.min(other.scale);
        let a = scale_mantissa(self.mantissa, self.scale, min_scale)?;
        let b = scale_mantissa(other.mantissa, other.scale, min_scale)?;
        Some(KubeQuantity::new(a.checked_sub(b)?, min_scale))
    }
}

/// `10^exp` in `i128`, `None` when `exp` is negative or the power overflows.
fn pow10(exp: i64) -> Option<i128> {
    10i128.checked_pow(u32::try_from(exp).ok()?)
}

/// Scale a mantissa from `from_scale` down to `to_scale` (to_scale <= from_scale).
/// Returns `None` if the scaled value would overflow `i128` (magnitude > ~10^38).
fn scale_mantissa(mantissa: i128, from_scale: i64, to_scale: i64) -> Option<i128> {
    let diff = from_scale - to_scale;
    if diff <= 0 {
        Some(mantissa)
    } else {
        pow10(diff).and_then(|p| mantissa.checked_mul(p))
    }
}

/// Normalize a pair of quantities to the same scale, returning their mantissas.
/// `None` if either scaling overflows `i128`.
fn normalize_pair(a: &KubeQuantity, b: &KubeQuantity) -> Option<(i128, i128)> {
    let min_scale = a.scale.min(b.scale);
    Some((
        scale_mantissa(a.mantissa, a.scale, min_scale)?,
        scale_mantissa(b.mantissa, b.scale, min_scale)?,
    ))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The decimal exponents and scales kube-cel supports. The apiserver reads an
/// exponent as an int64 and narrows it to int32 (`1e4294967296` is 1), and near
/// the int32 bounds computes powers of ten of hundreds of MiB, or panics; past
/// ±1000 kube-cel refuses the quantity instead (a documented fail-closed limit).
const SUPPORTED_SCALE: std::ops::RangeInclusive<i64> = -1000..=1000;

/// Why a string is not a quantity kube-cel can use.
#[derive(Debug)]
enum ParseError {
    /// Not a quantity, for the apiserver too: `quantity()` errors and
    /// `isQuantity()` is false.
    Invalid(String),
    /// A quantity the apiserver parses that kube-cel does not support: an
    /// exponent or scale outside [`SUPPORTED_SCALE`], or more significant
    /// digits than an `i128` holds. `quantity()` and `isQuantity()` both error,
    /// and the validation engine refuses the rule even where CEL absorbs the
    /// error (`|| true`), see [`note_unsupported`](crate::functions::note_unsupported).
    Unsupported(String),
}

impl From<ParseError> for ExecutionError {
    fn from(e: ParseError) -> Self {
        match e {
            ParseError::Invalid(message) => ExecutionError::function_error("quantity", message),
            ParseError::Unsupported(message) => unsupported(message),
        }
    }
}

/// An error for a quantity kube-cel does not support, noted for the validation
/// engine.
fn unsupported(message: String) -> ExecutionError {
    crate::functions::note_unsupported();
    ExecutionError::function_error("quantity", message)
}

fn check_scale(scale: i64, s: &str) -> Result<(), ParseError> {
    if SUPPORTED_SCALE.contains(&scale) {
        Ok(())
    } else {
        Err(ParseError::Unsupported(format!(
            "'{s}' has an exponent or scale outside ±1000, which kube-cel does not support"
        )))
    }
}

/// Parse a Kubernetes quantity string. The syntax is checked in full before
/// the range, so a malformed string is [`ParseError::Invalid`] whatever its
/// exponent, and the range is checked before the value is simplified, so
/// `0e2147483648` does not pass as 0.
fn parse_quantity(s: &str) -> Result<KubeQuantity, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Invalid("empty quantity string".into()));
    }

    // Find where the number ends and the suffix begins.
    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(s.len());

    // Special case: suffix starts with 'e' or 'E' for decimal exponent.
    // But only if followed by digits (not "Ei" for exbi).
    let (num_part, suffix) = if num_end < s.len() {
        let rest = &s[num_end..];
        if (rest.starts_with('e') || rest.starts_with('E'))
            && !rest.starts_with("Ei")
            && rest.len() > 1
            && rest[1..]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '+' || c == '-')
        {
            // Decimal exponent: treat whole string as number
            (s, "")
        } else {
            (&s[..num_end], rest)
        }
    } else {
        (&s[..num_end], "")
    };

    if num_part.is_empty() {
        return Err(ParseError::Invalid(format!("no numeric value in '{s}'")));
    }

    let (suffix_scale, binary_multiplier) = parse_suffix(suffix).map_err(ParseError::Invalid)?;
    // Parse number part into mantissa and decimal shift.
    let (mantissa, decimal_shift) = parse_number(num_part)?;

    if let Some(bin_mult) = binary_multiplier {
        check_scale(decimal_shift, s)?;
        // Binary SI: multiply mantissa by binary multiplier.
        let m = mantissa
            .checked_mul(bin_mult)
            .ok_or_else(|| ParseError::Unsupported(format!("'{s}' is larger than kube-cel supports")))?;
        Ok(KubeQuantity::new(m, decimal_shift))
    } else {
        // Decimal SI or exponent: combine scales.
        let scale = decimal_shift + i64::from(suffix_scale);
        check_scale(scale, s)?;
        Ok(KubeQuantity::new(mantissa, scale))
    }
}

/// Parse a number string, returning (mantissa, decimal_shift).
///
/// "1.5" → (15, -1): represents 15 * 10^-1
/// "100" → (100, 0)
/// "1e3" → (1, 3): represents 1 * 10^3
///
/// The exponent is read as an int64, as Go does: past that range it is not a
/// quantity. Within it, an exponent outside [`SUPPORTED_SCALE`] is unsupported.
fn parse_number(s: &str) -> Result<(i128, i64), ParseError> {
    // Handle scientific notation.
    if let Some(e_pos) = s.find(['e', 'E']) {
        let base_str = &s[..e_pos];
        let exp_str = &s[e_pos + 1..];
        let exp: i64 = exp_str
            .parse()
            .map_err(|_| ParseError::Invalid(format!("invalid exponent in '{s}'")))?;
        let (base_mantissa, base_shift) = parse_decimal(base_str)?;
        check_scale(exp, s)?;
        return Ok((base_mantissa, base_shift + exp));
    }

    parse_decimal(s)
}

/// Parse a decimal number (no exponent), returning (mantissa, decimal_shift).
fn parse_decimal(s: &str) -> Result<(i128, i64), ParseError> {
    let (digits, decimal_places) = match s.find('.') {
        Some(dot_pos) => {
            let frac_part = &s[dot_pos + 1..];
            let places = i64::try_from(frac_part.len())
                .map_err(|_| ParseError::Invalid(format!("invalid number: '{s}'")))?;
            (format!("{}{frac_part}", &s[..dot_pos]), places)
        }
        None => (s.to_owned(), 0),
    };
    // Well-formed digits past the i128 range are a quantity kube-cel cannot
    // hold, not a malformed one.
    let mantissa = digits.parse::<i128>().map_err(|e| match e.kind() {
        std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => ParseError::Unsupported(
            format!("'{s}' has more significant digits than kube-cel supports"),
        ),
        _ => ParseError::Invalid(format!("invalid number: '{s}'")),
    })?;
    Ok((mantissa, -decimal_places))
}

/// Parse a quantity suffix, returning (scale_offset, optional_binary_multiplier).
fn parse_suffix(suffix: &str) -> Result<(i32, Option<i128>), String> {
    match suffix {
        "" => Ok((0, None)),
        // Decimal SI
        "n" => Ok((-9, None)),
        "u" => Ok((-6, None)),
        "m" => Ok((-3, None)),
        "k" => Ok((3, None)),
        "M" => Ok((6, None)),
        "G" => Ok((9, None)),
        "T" => Ok((12, None)),
        "P" => Ok((15, None)),
        "E" => Ok((18, None)),
        // Binary SI
        "Ki" => Ok((0, Some(1 << 10))),
        "Mi" => Ok((0, Some(1 << 20))),
        "Gi" => Ok((0, Some(1 << 30))),
        "Ti" => Ok((0, Some(1 << 40))),
        "Pi" => Ok((0, Some(1 << 50))),
        "Ei" => Ok((0, Some(1 << 60))),
        _ => Err(format!("unknown quantity suffix: '{suffix}'")),
    }
}

// ---------------------------------------------------------------------------
// CEL function registration
// ---------------------------------------------------------------------------

/// Register all quantity extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("quantity", cel_quantity);
    r.add("isQuantity", cel_is_quantity);
    r.add("isInteger", cel_is_integer);
    r.add("asInteger", cel_as_integer);
    r.add("asApproximateFloat", cel_as_approximate_float);
    r.add("sign", cel_sign);
    r.add("add", cel_add);
    r.add("sub", cel_sub);
    // isGreaterThan, isLessThan, compareTo registered via dispatch
    // (shared with semver_funcs)
}

fn extract_quantity(val: &Value) -> Result<&KubeQuantity, ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeQuantity>()
            .ok_or_else(|| ExecutionError::function_error("quantity", "expected Quantity type")),
        _ => Err(ExecutionError::function_error(
            "quantity",
            "expected Quantity type",
        )),
    }
}

/// `quantity(<string>) -> Quantity`
fn cel_quantity(s: Arc<String>) -> ResolveResult {
    let q = parse_quantity(&s)?;
    Ok(Value::Opaque(Arc::new(q)))
}

/// `isQuantity(<string>) -> bool`. A quantity kube-cel does not support is an
/// error, not `false`: the apiserver parses it, so `!isQuantity(s)` must not
/// pass.
fn cel_is_quantity(s: Arc<String>) -> ResolveResult {
    match parse_quantity(&s) {
        Ok(_) => Ok(Value::Bool(true)),
        Err(ParseError::Invalid(_)) => Ok(Value::Bool(false)),
        Err(e) => Err(e.into()),
    }
}

/// `<Quantity>.isInteger() -> bool`
fn cel_is_integer(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Bool(q.is_integer()))
}

/// `<Quantity>.asInteger() -> int`
fn cel_as_integer(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Int(q.as_integer()?))
}

/// `<Quantity>.asApproximateFloat() -> float`
fn cel_as_approximate_float(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Float(q.as_approximate_float()))
}

/// `<Quantity>.sign() -> int`
fn cel_sign(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Int(q.sign()))
}

/// `<Quantity>.add(<Quantity | int>) -> Quantity`
pub(crate) fn cel_add(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let q = extract_quantity(&this)?;
    if args.is_empty() {
        return Err(ExecutionError::function_error("add", "missing argument"));
    }
    let other = quantity_or_int(&args[0], "add")?;
    check_result_scale(q, &other)?;
    let result = q
        .add(&other)
        .ok_or_else(|| ExecutionError::function_error("add", "quantity overflow"))?;
    Ok(Value::Opaque(Arc::new(result)))
}

/// `<Quantity>.sub(<Quantity | int>) -> Quantity`
pub(crate) fn cel_sub(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let q = extract_quantity(&this)?;
    if args.is_empty() {
        return Err(ExecutionError::function_error("sub", "missing argument"));
    }
    let other = quantity_or_int(&args[0], "sub")?;
    check_result_scale(q, &other)?;
    let result = q
        .sub(&other)
        .ok_or_else(|| ExecutionError::function_error("sub", "quantity overflow"))?;
    Ok(Value::Opaque(Arc::new(result)))
}

/// The scale of a sum or difference, the smaller of the two, must be within
/// [`SUPPORTED_SCALE`], as a parsed quantity's is.
fn check_result_scale(a: &KubeQuantity, b: &KubeQuantity) -> Result<(), ExecutionError> {
    if SUPPORTED_SCALE.contains(&a.scale.min(b.scale)) {
        Ok(())
    } else {
        Err(unsupported(
            "the result has a scale outside ±1000, which kube-cel does not support".to_owned(),
        ))
    }
}

/// `<Quantity>.isGreaterThan(<Quantity>) -> bool`
pub(crate) fn cel_is_greater_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?;
    Ok(Value::Bool(a > b))
}

/// `<Quantity>.isLessThan(<Quantity>) -> bool`
pub(crate) fn cel_is_less_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?;
    Ok(Value::Bool(a < b))
}

/// `<Quantity>.compareTo(<Quantity>) -> int`
pub(crate) fn cel_compare_to(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?;
    let result = match a.cmp(b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Value::Int(result))
}

/// Convert a Value to a KubeQuantity, accepting both Quantity and int.
fn quantity_or_int(val: &Value, func: &str) -> Result<KubeQuantity, ExecutionError> {
    match val {
        Value::Opaque(o) => {
            let q = o
                .downcast_ref::<KubeQuantity>()
                .ok_or_else(|| ExecutionError::function_error(func, "expected Quantity or int"))?;
            Ok(q.clone())
        }
        Value::Int(n) => Ok(KubeQuantity::new(*n as i128, 0)),
        Value::UInt(n) => Ok(KubeQuantity::new(*n as i128, 0)),
        _ => Err(ExecutionError::function_error(
            func,
            format!("expected Quantity or int, got {:?}", val.type_of()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    #[test]
    fn compare_extreme_magnitude_no_panic() {
        // quantity('1e40') exceeds i128; comparison must fall back gracefully, not
        // panic (debug) or wrap (release).
        assert_eq!(eval("quantity('1e40').compareTo(quantity('1'))"), Value::Int(1));
        assert_eq!(eval("quantity('1').compareTo(quantity('1e40'))"), Value::Int(-1));
    }

    #[test]
    fn compare_is_exact_when_scales_cannot_align() {
        // Compare i128::MAX against i128::MAX + 3 spelled with an exponent, so
        // the scales differ and aligning them overflows i128.
        let a = "quantity('170141183460469231731687303715884105727')";
        let b = "quantity('17014118346046923173168730371588410573e1')";
        assert_eq!(eval(&format!("{a}.compareTo({b})")), Value::Int(-1));
        assert_eq!(eval(&format!("{b}.compareTo({a})")), Value::Int(1));
        assert_eq!(eval(&format!("{a}.isLessThan({b})")), Value::Bool(true));
        // Negative magnitudes order the other way.
        let (na, nb) = (a.replace("('", "('-"), b.replace("('", "('-"));
        assert_eq!(eval(&format!("{na}.compareTo({nb})")), Value::Int(1));
        // Same digit count, decided by the last digit (…727 against …720).
        assert_eq!(
            eval(&format!(
                "{a}.compareTo(quantity('17014118346046923173168730371588410572e1'))"
            )),
            Value::Int(1)
        );
        // A tiny fractional part against a huge integer.
        assert_eq!(eval("quantity('1e40').compareTo(quantity('1n'))"), Value::Int(1));
    }

    /// `(result, whether the evaluation noted an unsupported quantity)`.
    fn eval_tracked(expr: &str) -> (Result<Value, ExecutionError>, bool) {
        crate::functions::tracking_unsupported(|| crate::functions::eval(expr))
    }

    #[test]
    fn exponents_and_scales_within_1000_are_supported() {
        // Exponent and scale (exponent minus fraction digits, plus an SI suffix)
        // up to ±1000 inclusive; the apiserver parses all of these.
        for s in [
            "1e1000",
            "1e-1000",
            "1.5e-999",
            "0e1000",
            "1.0e1000",
            "10e1000",
            "1E-1000",
            "1e+0001000",
        ] {
            assert_eq!(eval(&format!("isQuantity('{s}')")), Value::Bool(true), "{s}");
        }
        let thousand_places = format!("0.{}1", "0".repeat(999));
        assert_eq!(
            eval(&format!("isQuantity('{thousand_places}')")),
            Value::Bool(true)
        );
        // A suffix brings a long fraction back into range.
        let k = format!("0.{}1k", "0".repeat(1000));
        assert_eq!(eval(&format!("isQuantity('{k}')")), Value::Bool(true));
        assert_eq!(
            eval("quantity('1e1000').compareTo(quantity('1e999'))"),
            Value::Int(1)
        );
    }

    #[test]
    fn unsupported_quantities_error_and_are_noted() {
        // The apiserver parses each of these (some it narrows to int32, some
        // take it minutes); kube-cel refuses them, and `isQuantity` is an
        // error, not false, so `!isQuantity(s)` cannot pass.
        let long_fraction = format!("0.{}1", "0".repeat(1000));
        let binary_fraction = format!("0.{}1Ki", "0".repeat(1000));
        for s in [
            "1e1001",
            "1e-1001",
            "0.1e-1000",
            "0e2147483648",
            "1e4294967296",
            "0.1e-2147483647",
            "1e2147483648",
            "1.0e-2147483648",
            "1000000000000000000e2147483639",
            "12345678901234567890123456789012345678901",
            long_fraction.as_str(),
            binary_fraction.as_str(),
        ] {
            for expr in [format!("quantity('{s}')"), format!("isQuantity('{s}')")] {
                let (result, noted) = eval_tracked(&expr);
                assert!(result.is_err(), "{expr}: {result:?}");
                assert!(noted, "{expr}: not noted");
            }
        }
        // A sum's scale (the smaller of the two) must be in range too.
        let (result, noted) = eval_tracked("quantity('100e1000').add(quantity('100e1000'))");
        assert!(result.is_err() && noted, "{result:?}");
    }

    #[test]
    fn malformed_quantities_stay_invalid() {
        // Not a quantity for the apiserver either: false, nothing noted. An
        // exponent past int64 is a syntax error in Go (strconv.ParseInt).
        for s in [
            "1e9223372036854775808",
            "1e-9223372036854775809",
            "1e+-5",
            "1e",
            "1.2.3e2000",
        ] {
            let (result, noted) = eval_tracked(&format!("isQuantity('{s}')"));
            assert_eq!(result, Ok(Value::Bool(false)), "{s}");
            assert!(!noted, "{s}");
        }
    }

    #[test]
    fn noted_even_when_cel_absorbs_the_error() {
        let (result, noted) = eval_tracked("isQuantity('1e1001') || true");
        assert_eq!(result, Ok(Value::Bool(true)));
        assert!(noted);
        // A call evaluation does not reach is not noted.
        let (result, noted) = eval_tracked("true || isQuantity('1e1001')");
        assert_eq!(result, Ok(Value::Bool(true)));
        assert!(!noted);
    }

    #[test]
    fn add_overflow_errors() {
        // 2e38 scaled to scale 0 overflows i128 → error, not panic/wrap.
        let r = crate::functions::eval("quantity('2e38').add(quantity('1'))");
        assert!(r.is_err(), "expected overflow error, got {r:?}");
    }

    // -- Parsing --

    #[test]
    fn test_parse_plain_integer() {
        assert_eq!(eval("isQuantity('100')"), Value::Bool(true));
        assert_eq!(eval("quantity('100').asInteger()"), Value::Int(100));
    }

    #[test]
    fn test_parse_decimal() {
        assert_eq!(eval("quantity('1.5').asApproximateFloat()"), Value::Float(1.5));
    }

    #[test]
    fn test_parse_decimal_si() {
        assert_eq!(eval("quantity('1k').asInteger()"), Value::Int(1000));
        assert_eq!(eval("quantity('1M').asInteger()"), Value::Int(1_000_000));
        assert_eq!(eval("quantity('500m').asApproximateFloat()"), Value::Float(0.5));
        assert_eq!(eval("quantity('100n').asApproximateFloat()"), Value::Float(1e-7));
    }

    #[test]
    fn test_parse_binary_si() {
        assert_eq!(eval("quantity('1Ki').asInteger()"), Value::Int(1024));
        assert_eq!(eval("quantity('1Mi').asInteger()"), Value::Int(1_048_576));
        assert_eq!(eval("quantity('1Gi').asInteger()"), Value::Int(1_073_741_824));
    }

    #[test]
    fn test_parse_binary_si_decimal() {
        // 1.5Gi = 1.5 * 2^30 = 1610612736
        assert_eq!(eval("quantity('1.5Gi').asInteger()"), Value::Int(1_610_612_736));
    }

    #[test]
    fn test_parse_decimal_exponent() {
        assert_eq!(eval("quantity('1e3').asInteger()"), Value::Int(1000));
        assert_eq!(eval("quantity('5e2').asInteger()"), Value::Int(500));
    }

    #[test]
    fn test_parse_negative() {
        assert_eq!(eval("quantity('-1').asInteger()"), Value::Int(-1));
        assert_eq!(eval("quantity('-500m').asApproximateFloat()"), Value::Float(-0.5));
    }

    #[test]
    fn test_is_quantity() {
        assert_eq!(eval("isQuantity('1Gi')"), Value::Bool(true));
        assert_eq!(eval("isQuantity('not-a-quantity')"), Value::Bool(false));
        assert_eq!(eval("isQuantity('')"), Value::Bool(false));
    }

    // -- Properties --

    #[test]
    fn test_is_integer() {
        assert_eq!(eval("quantity('1k').isInteger()"), Value::Bool(true));
        assert_eq!(eval("quantity('1.5').isInteger()"), Value::Bool(false));
        assert_eq!(eval("quantity('500m').isInteger()"), Value::Bool(false));
    }

    #[test]
    fn test_sign() {
        assert_eq!(eval("quantity('100').sign()"), Value::Int(1));
        assert_eq!(eval("quantity('-100').sign()"), Value::Int(-1));
        assert_eq!(eval("quantity('0').sign()"), Value::Int(0));
    }

    // -- Comparison --

    #[test]
    fn test_is_greater_than() {
        assert_eq!(
            eval("quantity('1Gi').isGreaterThan(quantity('500Mi'))"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("quantity('500Mi').isGreaterThan(quantity('1Gi'))"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_is_less_than() {
        assert_eq!(
            eval("quantity('500m').isLessThan(quantity('1'))"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_compare_to() {
        assert_eq!(eval("quantity('1k').compareTo(quantity('1000'))"), Value::Int(0));
        assert_eq!(eval("quantity('1k').compareTo(quantity('2k'))"), Value::Int(-1));
        assert_eq!(eval("quantity('2k').compareTo(quantity('1k'))"), Value::Int(1));
    }

    #[test]
    fn test_compare_cross_suffix() {
        // 1Gi vs 1G: 1073741824 vs 1000000000
        assert_eq!(
            eval("quantity('1Gi').isGreaterThan(quantity('1G'))"),
            Value::Bool(true)
        );
    }

    // -- Arithmetic --

    #[test]
    fn test_add_quantities() {
        // 1Gi + 512Mi = 1073741824 + 536870912 = 1610612736
        assert_eq!(
            eval("quantity('1Gi').add(quantity('512Mi')).asInteger()"),
            Value::Int(1_610_612_736)
        );
    }

    #[test]
    fn test_add_int() {
        assert_eq!(eval("quantity('1k').add(500).asInteger()"), Value::Int(1500));
    }

    #[test]
    fn test_sub_quantities() {
        assert_eq!(
            eval("quantity('1k').sub(quantity('200')).asInteger()"),
            Value::Int(800)
        );
    }

    #[test]
    fn test_sub_int() {
        assert_eq!(eval("quantity('1000').sub(1).asInteger()"), Value::Int(999));
    }

    #[test]
    fn test_add_results_in_zero() {
        assert_eq!(eval("quantity('1k').sub(quantity('1k')).sign()"), Value::Int(0));
    }

    // -- Display --

    #[test]
    fn test_display() {
        let q = parse_quantity("1.5k").unwrap();
        assert_eq!(q.to_string(), "1500");

        let q = parse_quantity("500m").unwrap();
        assert_eq!(q.to_string(), "0.5");

        let q = parse_quantity("0").unwrap();
        assert_eq!(q.to_string(), "0");
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        crate::functions::eval(expr).unwrap_err()
    }

    #[test]
    fn test_quantity_invalid_error() {
        eval_err("quantity('')");
        eval_err("quantity('not-a-quantity')");
    }

    #[test]
    fn test_unknown_suffix() {
        assert_eq!(eval("isQuantity('5Z')"), Value::Bool(false));
        eval_err("quantity('5Z')");
    }

    #[test]
    fn test_as_integer_non_integer() {
        eval_err("quantity('1.5').asInteger()");
        eval_err("quantity('500m').asInteger()");
    }

    #[test]
    fn test_sub_negative_result() {
        assert_eq!(
            eval("quantity('100').sub(quantity('200')).sign()"),
            Value::Int(-1)
        );
        assert_eq!(
            eval("quantity('100').sub(quantity('200')).asInteger()"),
            Value::Int(-100)
        );
    }

    #[test]
    fn test_parse_remaining_si_suffixes() {
        // Decimal SI: u, T, P, E
        assert_eq!(eval("quantity('1u').asApproximateFloat()"), Value::Float(1e-6));
        assert_eq!(eval("quantity('1T').asInteger()"), Value::Int(1_000_000_000_000));
        assert_eq!(
            eval("quantity('1P').asInteger()"),
            Value::Int(1_000_000_000_000_000)
        );
        assert_eq!(
            eval("quantity('1E').asInteger()"),
            Value::Int(1_000_000_000_000_000_000)
        );
    }

    #[test]
    fn test_parse_remaining_binary_si() {
        // Ti, Pi, Ei
        assert_eq!(eval("quantity('1Ti').asInteger()"), Value::Int(1 << 40));
        assert_eq!(eval("quantity('1Pi').asInteger()"), Value::Int(1 << 50));
    }

    #[test]
    fn test_equal_comparison() {
        assert_eq!(
            eval("quantity('1k').isGreaterThan(quantity('1000'))"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("quantity('1k').isLessThan(quantity('1000'))"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_display_negative() {
        let q = parse_quantity("-500m").unwrap();
        assert_eq!(q.to_string(), "-0.5");

        let q = parse_quantity("-1500").unwrap();
        assert_eq!(q.to_string(), "-1500");
    }

    #[test]
    fn test_as_integer_integer_via_scale() {
        // 500m + 500m = 1000m = 1 (integer)
        assert_eq!(
            eval("quantity('500m').add(quantity('500m')).asInteger()"),
            Value::Int(1)
        );
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_cross_suffix_equality() {
        // 200M == 0.2G
        assert_eq!(
            eval("quantity('200M').compareTo(quantity('0.2G'))"),
            Value::Int(0)
        );
        // 2000k == 2M
        assert_eq!(eval("quantity('2000k').compareTo(quantity('2M'))"), Value::Int(0));
    }

    #[test]
    fn test_chained_arithmetic() {
        // 50k + 20 - 100k = -49980
        assert_eq!(
            eval("quantity('50k').add(20).sub(quantity('100k')).asInteger()"),
            Value::Int(-49980)
        );
    }

    #[test]
    fn test_chained_arithmetic_negative_sub() {
        // 50k + 20 - 100k - (-50000) = 20
        assert_eq!(
            eval("quantity('50k').add(20).sub(quantity('100k')).sub(-50000).asInteger()"),
            Value::Int(20)
        );
    }

    #[test]
    fn test_millicores_float() {
        assert_eq!(
            eval("quantity('50703m').asApproximateFloat()"),
            Value::Float(50.703)
        );
    }

    #[test]
    fn test_quantity_zero_equality() {
        assert_eq!(eval("quantity('0').compareTo(quantity('0M'))"), Value::Int(0));
    }
}
