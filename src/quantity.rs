//! Kubernetes CEL quantity extension functions.
//!
//! Provides parsing, comparison, and arithmetic for Kubernetes resource quantities
//! (e.g., "1.5Gi", "500m", "100n"), matching `k8s.io/apiserver/pkg/cel/library/quantity.go`.

use cel::{
    Context, ExecutionError, ResolveResult,
    extractors::{Arguments, This},
    objects::{Opaque, Value},
};
use std::{cmp::Ordering, fmt, sync::Arc};

// ---------------------------------------------------------------------------
// Internal representation
// ---------------------------------------------------------------------------

/// A Kubernetes resource quantity.
///
/// Stored as `mantissa * 10^scale` to allow exact decimal arithmetic.
/// Binary SI suffixes (Ki, Mi, …) are converted to their decimal value at
/// parse time so that all quantities share a common representation.
#[derive(Debug, Clone, Eq)]
pub struct KubeQuantity {
    mantissa: i128,
    scale: i32,
}

impl PartialEq for KubeQuantity {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for KubeQuantity {
    fn cmp(&self, other: &Self) -> Ordering {
        let (a, b) = normalize_pair(self, other);
        a.cmp(&b)
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
            let abs_scale = (-self.scale) as usize;
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
    fn new(mantissa: i128, scale: i32) -> Self {
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
        let divisor = 10i128.checked_pow((-self.scale) as u32);
        match divisor {
            Some(d) => self.mantissa % d == 0,
            None => false,
        }
    }

    fn as_integer(&self) -> Result<i64, ExecutionError> {
        if self.scale >= 0 {
            let multiplier = 10i128.checked_pow(self.scale as u32).ok_or_else(|| {
                ExecutionError::function_error("asInteger", "quantity too large for integer")
            })?;
            let val = self.mantissa.checked_mul(multiplier).ok_or_else(|| {
                ExecutionError::function_error("asInteger", "quantity too large for integer")
            })?;
            i64::try_from(val)
                .map_err(|_| ExecutionError::function_error("asInteger", "quantity too large for integer"))
        } else {
            let divisor = 10i128.checked_pow((-self.scale) as u32).ok_or_else(|| {
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
        self.mantissa as f64 * 10f64.powi(self.scale)
    }

    fn add(&self, other: &KubeQuantity) -> KubeQuantity {
        let min_scale = self.scale.min(other.scale);
        let a = scale_mantissa(self.mantissa, self.scale, min_scale);
        let b = scale_mantissa(other.mantissa, other.scale, min_scale);
        KubeQuantity::new(a + b, min_scale)
    }

    fn sub(&self, other: &KubeQuantity) -> KubeQuantity {
        let min_scale = self.scale.min(other.scale);
        let a = scale_mantissa(self.mantissa, self.scale, min_scale);
        let b = scale_mantissa(other.mantissa, other.scale, min_scale);
        KubeQuantity::new(a - b, min_scale)
    }
}

/// Scale a mantissa from `from_scale` down to `to_scale` (to_scale <= from_scale).
fn scale_mantissa(mantissa: i128, from_scale: i32, to_scale: i32) -> i128 {
    let diff = from_scale - to_scale;
    if diff <= 0 {
        mantissa
    } else {
        mantissa * 10i128.pow(diff as u32)
    }
}

/// Normalize a pair of quantities to the same scale, returning their mantissas.
fn normalize_pair(a: &KubeQuantity, b: &KubeQuantity) -> (i128, i128) {
    let min_scale = a.scale.min(b.scale);
    (
        scale_mantissa(a.mantissa, a.scale, min_scale),
        scale_mantissa(b.mantissa, b.scale, min_scale),
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a Kubernetes quantity string.
fn parse_quantity(s: &str) -> Result<KubeQuantity, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty quantity string".into());
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
        return Err(format!("no numeric value in '{s}'"));
    }

    // Parse number part into mantissa and decimal shift.
    let (mantissa, decimal_shift) = parse_number(num_part)?;

    // Parse suffix.
    let (suffix_scale, binary_multiplier) = parse_suffix(suffix)?;

    if let Some(bin_mult) = binary_multiplier {
        // Binary SI: multiply mantissa by binary multiplier.
        let m = mantissa
            .checked_mul(bin_mult)
            .ok_or_else(|| format!("quantity overflow: '{s}'"))?;
        Ok(KubeQuantity::new(m, decimal_shift))
    } else {
        // Decimal SI or exponent: combine scales.
        Ok(KubeQuantity::new(mantissa, decimal_shift + suffix_scale))
    }
}

/// Parse a number string, returning (mantissa, decimal_shift).
///
/// "1.5" → (15, -1): represents 15 * 10^-1
/// "100" → (100, 0)
/// "1e3" → (1, 3): represents 1 * 10^3
fn parse_number(s: &str) -> Result<(i128, i32), String> {
    // Handle scientific notation.
    if let Some(e_pos) = s.find(['e', 'E']) {
        let base_str = &s[..e_pos];
        let exp_str = &s[e_pos + 1..];
        let (base_mantissa, base_shift) = parse_decimal(base_str)?;
        let exp: i32 = exp_str
            .parse()
            .map_err(|_| format!("invalid exponent in '{s}'"))?;
        return Ok((base_mantissa, base_shift + exp));
    }

    parse_decimal(s)
}

/// Parse a decimal number (no exponent), returning (mantissa, decimal_shift).
fn parse_decimal(s: &str) -> Result<(i128, i32), String> {
    if let Some(dot_pos) = s.find('.') {
        let int_part = &s[..dot_pos];
        let frac_part = &s[dot_pos + 1..];
        let decimal_places = frac_part.len() as i32;

        let combined = format!("{int_part}{frac_part}");
        let mantissa: i128 = combined.parse().map_err(|_| format!("invalid number: '{s}'"))?;
        Ok((mantissa, -decimal_places))
    } else {
        let mantissa: i128 = s.parse().map_err(|_| format!("invalid number: '{s}'"))?;
        Ok((mantissa, 0))
    }
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
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("quantity", cel_quantity);
    ctx.add_function("isQuantity", cel_is_quantity);
    ctx.add_function("isInteger", cel_is_integer);
    ctx.add_function("asInteger", cel_as_integer);
    ctx.add_function("asApproximateFloat", cel_as_approximate_float);
    ctx.add_function("sign", cel_sign);
    ctx.add_function("add", cel_add);
    ctx.add_function("sub", cel_sub);
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
    let q = parse_quantity(&s).map_err(|e| ExecutionError::function_error("quantity", e))?;
    Ok(Value::Opaque(Arc::new(q)))
}

/// `isQuantity(<string>) -> bool`
fn cel_is_quantity(s: Arc<String>) -> ResolveResult {
    Ok(Value::Bool(parse_quantity(&s).is_ok()))
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
    let result = q.add(&other);
    Ok(Value::Opaque(Arc::new(result)))
}

/// `<Quantity>.sub(<Quantity | int>) -> Quantity`
pub(crate) fn cel_sub(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let q = extract_quantity(&this)?;
    if args.is_empty() {
        return Err(ExecutionError::function_error("sub", "missing argument"));
    }
    let other = quantity_or_int(&args[0], "sub")?;
    let result = q.sub(&other);
    Ok(Value::Opaque(Arc::new(result)))
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
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
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
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
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
