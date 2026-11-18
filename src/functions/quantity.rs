//! Kubernetes CEL quantity extension functions.
//!
//! A port of what k8s.io/apimachinery v0.36.1's `resource.Quantity` does for
//! the apiserver's quantity library (`k8s.io/apiserver/pkg/cel/library/quantity.go`):
//! `ParseQuantity`, which keeps a quantity either as an `int64Amount` (an int64
//! times a power of ten, as written) or as an `inf.Dec` rounded up to nano
//! units, and the methods the library calls (`AsInt64`, `AsApproximateFloat64`,
//! `Sign`, `Cmp`, `Add`, `Sub`, `Equal`). Results that depend on that
//! representation follow it: `quantity('1000m').isInteger()` is false (an
//! `int64Amount` with a negative scale), `quantity('0e1000').asApproximateFloat()`
//! is NaN (`0 * Pow10(1000)`).

use cel::{
    ExecutionError, FunctionContext, ResolveResult,
    extractors::{Arguments, This},
    objects::{Opaque, Value},
};
use std::{
    cmp::Ordering,
    fmt,
    sync::{Arc, Mutex},
};

use crate::functions::Registrar;

// ---------------------------------------------------------------------------
// Internal representation
// ---------------------------------------------------------------------------

/// A Kubernetes resource quantity: what the apiserver's quantity values point
/// to, a Go `resource.Quantity`.
///
/// The apiserver passes quantities by pointer and `resource.Quantity` keeps its
/// `inf.Dec` behind one too, so a quantity's state is shared and changes:
/// `add` / `sub` copy the receiver (`copy := *q`), and when it holds an
/// `inf.Dec` the copy points to the same one, which `copy.Add(..)` updates in
/// place; the receiver, and every result sharing that `inf.Dec`, take the new
/// value (`q.add(1).compareTo(q) == 0`). A CEL value here is an `Arc` of this
/// type, shared as the apiserver's pointer is, and the `inf.Dec` an
/// `Arc<Mutex<..>>` shared as Go's.
#[derive(Debug)]
pub struct KubeQuantity {
    repr: Mutex<Repr>,
    /// The value of an `add` / `sub`, which the apiserver returns as a pointer
    /// (`*apiservercel.Quantity`); its `Equal` refuses a pointer on the right.
    from_arithmetic: bool,
}

/// What a `resource.Quantity` holds: an `int64Amount`, or a pointer to an
/// `inf.Dec`.
#[derive(Debug, Clone)]
enum Repr {
    Int { value: i64, scale: i32 },
    Dec(Arc<Mutex<Dec>>),
}

/// An `inf.Dec`: `mantissa × 10^zeros × 10^-scale` (its unscaled value is
/// `mantissa × 10^zeros`, kept apart so that rounding to nano units does not
/// overflow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dec {
    mantissa: i128,
    zeros: u32,
    scale: i32,
}

/// A quantity's value as it stands, in the representation it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Amount {
    /// Go's `int64Amount`: `value × 10^scale`.
    Int { value: i64, scale: i32 },
    /// Go's `inf.Dec`.
    Dec(Dec),
}

/// Locks `m`; a poisoned lock still holds a whole value (no update panics
/// half-way).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Repr {
    fn new(amount: Amount) -> Self {
        match amount {
            Amount::Int { value, scale } => Repr::Int { value, scale },
            Amount::Dec(dec) => Repr::Dec(Arc::new(Mutex::new(dec))),
        }
    }

    fn amount(&self) -> Amount {
        match self {
            Repr::Int { value, scale } => Amount::Int {
                value: *value,
                scale: *scale,
            },
            Repr::Dec(dec) => Amount::Dec(*lock(dec)),
        }
    }
}

impl KubeQuantity {
    fn new(amount: Amount) -> Self {
        KubeQuantity {
            repr: Mutex::new(Repr::new(amount)),
            from_arithmetic: false,
        }
    }

    /// The value as it stands now.
    fn amount(&self) -> Amount {
        lock(&self.repr).amount()
    }

    /// Go's `quantityAdd` / `quantitySub` with this receiver: `copy := *q`,
    /// then `copy.Add(y)` / `copy.Sub(y)`, whose `inf.Dec` path writes into the
    /// copy's `inf.Dec`, the receiver's own when it has one. `y` is a value
    /// read before (Go passes a copy). `None` when the result leaves `i128`
    /// (an `inf.Dec` has no such bound: a documented fail-closed limit), and
    /// then nothing is written.
    fn add_or_sub(&self, y: Amount, subtract: bool) -> Option<Repr> {
        let copy = lock(&self.repr).clone();
        match copy {
            Repr::Int { value, scale } => {
                // The int64Amount path when both are one and the sum fits; the
                // `Sub` side negates `y`, wrapping as Go does for MinInt64.
                if let Amount::Int { value: yv, scale: ys } = y {
                    let yv = if subtract { yv.wrapping_neg() } else { yv };
                    if let Some(sum) = int64_amount_add((value, scale), (yv, ys)) {
                        return Some(Repr::new(sum));
                    }
                }
                // `ToDec` gives the copy an `inf.Dec` of its own.
                let x = Amount::Int { value, scale }.as_dec();
                Some(Repr::new(Amount::Dec(dec_add(x, y.as_dec(), subtract)?)))
            }
            Repr::Dec(shared) => {
                let mut dec = lock(&shared);
                *dec = dec_add(*dec, y.as_dec(), subtract)?;
                drop(dec);
                Some(Repr::Dec(shared))
            }
        }
    }
}

impl KubeQuantity {
    /// Go's `q.Cmp(y)` with this receiver, behind `compareTo`, `isGreaterThan`
    /// and `isLessThan`: when either side holds an `inf.Dec`, `q.AsDec()` turns
    /// an int64Amount receiver into an `inf.Dec` for good, so a later
    /// `isInteger()` is false and a later `add` shares it. `y` is a value read
    /// before (Go passes a copy), and stays as it is.
    fn compare(&self, y: Amount) -> Ordering {
        let mut repr = lock(&self.repr);
        if let (&Repr::Int { value, scale }, Amount::Dec(_)) = (&*repr, y) {
            *repr = Repr::new(Amount::Dec(Amount::Int { value, scale }.as_dec()));
        }
        let x = repr.amount();
        drop(repr);
        cmp_values(x.value(), y.value())
    }
}

impl Amount {
    /// The value as `(m, e)` with value `m × 10^e`.
    fn value(self) -> (i128, i64) {
        match self {
            Amount::Int { value, scale } => (i128::from(value), i64::from(scale)),
            Amount::Dec(Dec {
                mantissa,
                zeros,
                scale,
            }) => (mantissa, i64::from(zeros) - i64::from(scale)),
        }
    }

    fn sign(self) -> i64 {
        match self.value().0.cmp(&0) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    /// Go's `Quantity.AsInt64()`: only an `int64Amount` of a non-negative scale
    /// whose value fits; an `inf.Dec` never.
    fn as_int64(self) -> Option<i64> {
        match self {
            Amount::Int { value, scale } => match scale.cmp(&0) {
                Ordering::Equal => Some(value),
                Ordering::Less => None,
                Ordering::Greater => (0..scale).try_fold(value, |v, _| v.checked_mul(10)),
            },
            Amount::Dec(_) => None,
        }
    }

    /// Go's `Quantity.AsApproximateFloat64()`: the unscaled value as a float
    /// times `math.Pow10` of the exponent (unless it is 0).
    fn as_approximate_float(self) -> f64 {
        let (base, exponent) = match self {
            Amount::Int { value, scale } => (value as f64, i64::from(scale)),
            Amount::Dec(Dec {
                mantissa,
                zeros,
                scale,
            }) => {
                // big.Float's conversion of the unscaled value, correctly rounded.
                let base = format!("{mantissa}e{zeros}").parse::<f64>().unwrap_or(f64::NAN);
                (base, -i64::from(scale))
            }
        };
        if exponent == 0 {
            return base;
        }
        base * go_pow10(exponent)
    }

    /// The amount as Go's `AsDec()`.
    fn as_dec(self) -> Dec {
        match self {
            Amount::Int { value, scale } => Dec {
                mantissa: i128::from(value),
                zeros: 0,
                scale: -scale,
            },
            Amount::Dec(dec) => dec,
        }
    }
}

/// Go's `int64Amount.Add`, `None` where Go falls back to `inf.Dec`.
fn int64_amount_add((av, as_): (i64, i32), (bv, bs): (i64, i32)) -> Option<Amount> {
    let (value, scale) = if bv == 0 {
        (av, as_)
    } else if av == 0 {
        (bv, bs)
    } else if as_ == bs {
        (av.checked_add(bv)?, as_)
    } else if as_ > bs {
        (positive_scale_int64(av, as_ - bs)?.checked_add(bv)?, bs)
    } else {
        (av.checked_add(positive_scale_int64(bv, bs - as_)?)?, as_)
    };
    Some(Amount::Int { value, scale })
}

/// Go's `positiveScaleInt64`: `base × 10^scale`, if it fits.
fn positive_scale_int64(base: i64, scale: i32) -> Option<i64> {
    (0..scale).try_fold(base, |v, _| v.checked_mul(10))
}

/// `inf.Dec` addition (or subtraction): both at the greater scale, then summed.
fn dec_add(a: Dec, b: Dec, subtract: bool) -> Option<Dec> {
    let Dec {
        mantissa: am,
        zeros: az,
        scale: as_,
    } = a;
    let Dec {
        mantissa: bm,
        zeros: bz,
        scale: bs,
    } = b;
    let scale = as_.max(bs);
    // Rescaling up multiplies the unscaled value by a power of ten.
    let az = az.checked_add(u32::try_from(scale - as_).ok()?)?;
    let bz = bz.checked_add(u32::try_from(scale - bs).ok()?)?;
    // A zero has no digits to align: the other side's zeros are kept.
    let zeros = match (am, bm) {
        (0, _) => bz,
        (_, 0) => az,
        _ => az.min(bz),
    };
    let (az, bz) = (az.max(zeros), bz.max(zeros));
    // A zero stays zero at any scale: no power of ten to compute (`0` plus
    // `0e-1000` would need 10^1000).
    let align = |m: i128, n: u32| {
        if m == 0 {
            Some(0)
        } else {
            m.checked_mul(10i128.checked_pow(n)?)
        }
    };
    let a = align(am, az - zeros)?;
    let b = align(bm, bz - zeros)?;
    let mantissa = if subtract {
        a.checked_sub(b)?
    } else {
        a.checked_add(b)?
    };
    Some(Dec {
        mantissa,
        zeros,
        scale,
    })
}

/// Go's `math.Pow10`, from its tables (not always the correctly rounded power).
fn go_pow10(n: i64) -> f64 {
    const TAB: [f64; 32] = [
        1e00, 1e01, 1e02, 1e03, 1e04, 1e05, 1e06, 1e07, 1e08, 1e09, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29, 1e30, 1e31,
    ];
    const POS32: [f64; 10] = [1e00, 1e32, 1e64, 1e96, 1e128, 1e160, 1e192, 1e224, 1e256, 1e288];
    const NEG32: [f64; 11] = [
        1e-00, 1e-32, 1e-64, 1e-96, 1e-128, 1e-160, 1e-192, 1e-224, 1e-256, 1e-288, 1e-320,
    ];
    if (0..=308).contains(&n) {
        let n = n as usize;
        return POS32[n / 32] * TAB[n % 32];
    }
    if (-323..=0).contains(&n) {
        let n = (-n) as usize;
        return NEG32[n / 32] / TAB[n % 32];
    }
    if n > 0 { f64::INFINITY } else { 0.0 }
}

/// The apiserver's `Equal` compares with `Cmp`, but asserts that the right side
/// is a `Quantity` value: the pointer an `add` / `sub` returns makes it an
/// error. `cel`'s `==` cannot fail, so that case is false here and noted, and
/// the validation engine refuses the rule (see
/// [`note_unsupported`](crate::functions::note_unsupported)).
impl PartialEq for KubeQuantity {
    fn eq(&self, other: &Self) -> bool {
        if other.from_arithmetic {
            crate::functions::note_unsupported();
            return false;
        }
        // `Equal` has a value receiver: it compares copies and changes neither.
        cmp_values(self.amount().value(), other.amount().value()) == Ordering::Equal
    }
}

impl Eq for KubeQuantity {}

/// Exact comparison of `a.0 × 10^a.1` and `b.0 × 10^b.1`: by sign, then by the
/// magnitudes' decimal digits, never scaling into a wider integer.
fn cmp_values(a: (i128, i64), b: (i128, i64)) -> Ordering {
    let sign = |m: i128| m.cmp(&0);
    let by_sign = sign(a.0).cmp(&sign(b.0));
    if by_sign != Ordering::Equal || a.0 == 0 {
        return by_sign;
    }
    let min_exp = a.1.min(b.1);
    let (da, db) = (a.0.unsigned_abs().to_string(), b.0.unsigned_abs().to_string());
    // Digit count once both are written at `min_exp`.
    let width = |d: &str, e: i64| d.len() as i64 + e - min_exp;
    let magnitude = width(&da, a.1).cmp(&width(&db, b.1)).then_with(|| {
        // Same width: digit by digit; past a mantissa's end its digits are 0.
        let n = da.len().max(db.len());
        let digits = |d: &str| {
            d.bytes()
                .chain(std::iter::repeat(b'0'))
                .take(n)
                .collect::<Vec<u8>>()
        };
        digits(&da).cmp(&digits(&db))
    });
    if a.0 < 0 { magnitude.reverse() } else { magnitude }
}

/// The exact decimal value (for tests and debugging).
impl fmt::Display for KubeQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (m, e) = self.amount().value();
        if m == 0 {
            return write!(f, "0");
        }
        let sign = if m < 0 { "-" } else { "" };
        let digits = m.unsigned_abs().to_string();
        if e >= 0 {
            return write!(f, "{sign}{digits}{}", "0".repeat(e as usize));
        }
        let frac = (-e) as usize;
        let (int, fraction) = if digits.len() > frac {
            let (int, fraction) = digits.split_at(digits.len() - frac);
            (int.to_owned(), fraction.to_owned())
        } else {
            (
                "0".to_owned(),
                format!("{}{digits}", "0".repeat(frac - digits.len())),
            )
        };
        let fraction = fraction.trim_end_matches('0');
        if fraction.is_empty() {
            write!(f, "{sign}{int}")
        } else {
            write!(f, "{sign}{int}.{fraction}")
        }
    }
}

impl Opaque for KubeQuantity {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.Quantity"
    }
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

fn out_of_range(s: &str) -> ParseError {
    ParseError::Unsupported(format!(
        "'{s}' has an exponent or scale outside ±1000, which kube-cel does not support"
    ))
}

fn check_scale(scale: i64, s: &str) -> Result<(), ParseError> {
    if SUPPORTED_SCALE.contains(&scale) {
        Ok(())
    } else {
        Err(out_of_range(s))
    }
}

/// Whether a value's exponent is within [`SUPPORTED_SCALE`], as held and with
/// its mantissa's trailing zeros moved into it (`10e1000` is 1e1001).
fn in_range(amount: &Amount) -> bool {
    let (m, e) = amount.value();
    if m == 0 {
        return true;
    }
    let trailing = m
        .unsigned_abs()
        .to_string()
        .bytes()
        .rev()
        .take_while(|&b| b == b'0')
        .count() as i64;
    SUPPORTED_SCALE.contains(&e) && SUPPORTED_SCALE.contains(&(e + trailing))
}

const INVALID: &str =
    "quantities must match the regular expression '^([+-]?[0-9.]+)([eEinumkKMGTP]*[-+]?[0-9]*)$'";

/// Go's `resource.ParseQuantity`. The syntax is checked in full before the
/// range (a malformed string is [`ParseError::Invalid`] whatever its exponent),
/// and the range before the value is simplified or rounded, so `0e2147483648`
/// does not pass as 0.
fn parse_quantity(s: &str) -> Result<KubeQuantity, ParseError> {
    if s.is_empty() {
        return Err(ParseError::Invalid(INVALID.into()));
    }
    if s == "0" {
        return Ok(KubeQuantity::new(Amount::Int { value: 0, scale: 0 }));
    }
    let parts = parse_quantity_string(s).ok_or_else(|| ParseError::Invalid(INVALID.into()))?;
    let (base, exponent, binary) = interpret_suffix(parts.suffix)?;
    // A number without digits (`e5`, `.`, `-e-3`): Go's int64Amount path reads
    // it as 0, while its inf.Dec path parses it and finds no digit, so it is
    // not a quantity. Go takes the inf.Dec path for a decimal exponent, narrowed
    // to int32, below -9, and that decides even past the supported range
    // (`e-1001` is not a quantity; `e1001` is a 0 kube-cel does not support).
    // Binary suffixes are within range and take the paths below.
    if !binary && !parts.value.bytes().any(|b| b.is_ascii_digit()) && (exponent as i32) < -9 {
        return Err(ParseError::Invalid(INVALID.into()));
    }
    check_scale(exponent, s)?;

    let (mut scale, precision, mantissa) = if binary {
        if exponent >= 0 && parts.denom.is_empty() {
            let precision = 15 - parts.num.len() as i64 - exponent * 3 / 10 - 1;
            (0i64, precision, 1i64 << exponent)
        } else {
            (0, -1, 1)
        }
    } else {
        (exponent, 18 - (parts.num.len() + parts.denom.len()) as i64, 1)
    };
    // The scale once the fraction digits are moved into it.
    check_scale(scale - parts.denom.len() as i64, s)?;

    if precision >= 0 {
        scale -= parts.denom.len() as i64;
        if scale >= -9 {
            let shifted = format!("{}{}", parts.num, parts.denom);
            let value: i64 = shifted.parse().map_err(|_| ParseError::Invalid(INVALID.into()))?;
            if let Some(result) = int64_multiply(value, mantissa) {
                let value = if parts.positive {
                    result
                } else {
                    result.wrapping_neg()
                };
                let amount = Amount::Int {
                    value,
                    scale: scale as i32,
                };
                return if in_range(&amount) {
                    Ok(KubeQuantity::new(amount))
                } else {
                    Err(out_of_range(s))
                };
            }
        }
    }

    // `inf.Dec` from the written number, then the suffix applied, computed on
    // decimal digits so that a binary suffix or the rounding cannot overflow.
    let (negative, mut unscaled, frac) =
        inf_dec_digits(parts.value).ok_or_else(|| ParseError::Invalid(INVALID.into()))?;
    let mut dec_scale = frac;
    if base == 10 {
        dec_scale -= exponent;
    } else {
        for _ in 0..exponent {
            unscaled = double_digits(&unscaled);
        }
    }
    // Non-zero values are rounded up to nano units (scale 9), away from zero.
    if unscaled != "0" {
        (unscaled, dec_scale) = round_up_to_nano(unscaled, dec_scale);
    }
    // A binary quantity is capped at the largest int64.
    if binary
        && cmp_digit_strings(
            &unscaled,
            &format!("{}{}", i64::MAX, "0".repeat(dec_scale.max(0) as usize)),
        ) == Ordering::Greater
    {
        (unscaled, dec_scale) = (i64::MAX.to_string(), 0);
    }
    let kept = unscaled.trim_end_matches('0');
    let zeros = u32::try_from(unscaled.len() - kept.len()).map_err(|_| out_of_range(s))?;
    // Parsed with its sign: i128::MIN has no positive counterpart.
    let mantissa: i128 = if kept.is_empty() {
        0
    } else {
        let sign = if negative { "-" } else { "" };
        format!("{sign}{kept}").parse().map_err(|_| {
            ParseError::Unsupported(format!(
                "'{s}' has more significant digits than kube-cel supports"
            ))
        })?
    };
    let amount = Amount::Dec(Dec {
        mantissa,
        zeros: if mantissa == 0 { 0 } else { zeros },
        scale: dec_scale as i32,
    });
    if in_range(&amount) {
        Ok(KubeQuantity::new(amount))
    } else {
        Err(out_of_range(s))
    }
}

/// Go's `int64Multiply`, `None` on overflow.
fn int64_multiply(a: i64, b: i64) -> Option<i64> {
    if a == 0 || b == 0 || a == 1 || b == 1 {
        return Some(a.wrapping_mul(b));
    }
    a.checked_mul(b)
}

/// `inf.Dec`'s `Round(x, 9, RoundUp)` on a non-zero unscaled value written as
/// digits: rescaled up exactly, or cut and rounded up (away from zero).
fn round_up_to_nano(digits: String, scale: i64) -> (String, i64) {
    const NANO: i64 = 9;
    if scale <= NANO {
        // Bounded by the supported range.
        return (format!("{digits}{}", "0".repeat((NANO - scale) as usize)), NANO);
    }
    let drop = (scale - NANO) as usize;
    if drop >= digits.len() {
        return ("1".to_owned(), NANO);
    }
    let (kept, dropped) = digits.split_at(digits.len() - drop);
    let kept = if kept.is_empty() { "0" } else { kept };
    if dropped.bytes().any(|b| b != b'0') {
        (increment_digits(kept), NANO)
    } else {
        (kept.to_owned(), NANO)
    }
}

/// `inf.Dec.SetString` of the written number (sign, digits, at most one `.`):
/// its sign, its unscaled value as digits without leading zeros ("0" for
/// zero), and its scale (the fraction digits). `None` without a digit.
fn inf_dec_digits(value: &str) -> Option<(bool, String, i64)> {
    let (negative, body) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let digits = format!("{int}{frac}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let significant = digits.trim_start_matches('0');
    let unscaled = if significant.is_empty() { "0" } else { significant };
    Some((negative, unscaled.to_owned(), frac.len() as i64))
}

/// Twice a non-negative number written as digits.
fn double_digits(digits: &str) -> String {
    let mut out = Vec::with_capacity(digits.len() + 1);
    let mut carry = 0;
    for b in digits.bytes().rev() {
        let d = (b - b'0') * 2 + carry;
        out.push(b'0' + d % 10);
        carry = d / 10;
    }
    if carry > 0 {
        out.push(b'0' + carry);
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII digits")
}

/// One more than a non-negative number written as digits.
fn increment_digits(digits: &str) -> String {
    let mut out: Vec<u8> = digits.bytes().collect();
    for b in out.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return String::from_utf8(out).expect("ASCII digits");
        }
    }
    out.insert(0, b'1');
    String::from_utf8(out).expect("ASCII digits")
}

/// Compares two non-negative numbers written as digits.
fn cmp_digit_strings(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// The parts Go's `parseQuantityString` splits a quantity into.
struct QuantityParts<'a> {
    positive: bool,
    /// The number as written (what `inf.Dec` parses), up to the suffix.
    value: &'a str,
    /// The integer digits without leading zeros ("0" if none).
    num: &'a str,
    /// The fraction digits.
    denom: &'a str,
    suffix: &'a str,
}

/// Go's `parseQuantityString`: `[+-]`, digits, `.digits`, then a suffix of the
/// letters `eEinumkKMGTP`, an optional sign and digits, to the end. No space
/// is allowed anywhere.
fn parse_quantity_string(s: &str) -> Option<QuantityParts<'_>> {
    let b = s.as_bytes();
    let end = b.len();
    let mut pos = 0;
    let mut positive = true;
    if pos < end {
        match b[0] {
            b'-' => {
                positive = false;
                pos += 1;
            }
            b'+' => pos += 1,
            _ => {}
        }
    }
    // Leading zeros.
    while pos < end && b[pos] == b'0' {
        pos += 1;
    }
    if pos >= end {
        return Some(QuantityParts {
            positive,
            value: "0",
            num: "0",
            denom: "",
            suffix: "",
        });
    }
    let num_start = pos;
    while pos < end && b[pos].is_ascii_digit() {
        pos += 1;
    }
    let mut num = &s[num_start..pos];
    if pos >= end {
        return Some(QuantityParts {
            positive,
            value: s,
            num,
            denom: "",
            suffix: "",
        });
    }
    if num.is_empty() {
        num = "0";
    }
    let mut denom = "";
    if b[pos] == b'.' {
        pos += 1;
        let denom_start = pos;
        while pos < end && b[pos].is_ascii_digit() {
            pos += 1;
        }
        denom = &s[denom_start..pos];
        if pos >= end {
            return Some(QuantityParts {
                positive,
                value: s,
                num,
                denom,
                suffix: "",
            });
        }
    }
    let value = &s[..pos];
    let suffix_start = pos;
    while pos < end && b"eEinumkKMGTP".contains(&b[pos]) {
        pos += 1;
    }
    if pos < end && (b[pos] == b'-' || b[pos] == b'+') {
        pos += 1;
    }
    while pos < end && b[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos < end {
        return None;
    }
    Some(QuantityParts {
        positive,
        value,
        num,
        denom,
        suffix: &s[suffix_start..],
    })
}

/// Go's suffix interpretation: `(base, exponent, binary)`. A decimal exponent
/// (`e3`, `E-2`) is read as an int64; past that it is not a quantity (the
/// caller checks it against [`SUPPORTED_SCALE`]).
fn interpret_suffix(suffix: &str) -> Result<(i64, i64, bool), ParseError> {
    let dec = |e| Ok((10, e, false));
    let bin = |e| Ok((2, e, true));
    match suffix {
        "" => dec(0),
        "n" => dec(-9),
        "u" => dec(-6),
        "m" => dec(-3),
        "k" => dec(3),
        "M" => dec(6),
        "G" => dec(9),
        "T" => dec(12),
        "P" => dec(15),
        "E" => dec(18),
        "Ki" => bin(10),
        "Mi" => bin(20),
        "Gi" => bin(30),
        "Ti" => bin(40),
        "Pi" => bin(50),
        "Ei" => bin(60),
        _ if suffix.len() > 1 && (suffix.starts_with('e') || suffix.starts_with('E')) => {
            let exponent: i64 = suffix[1..]
                .parse()
                .map_err(|_| ParseError::Invalid("unable to parse quantity's suffix".into()))?;
            dec(exponent)
        }
        _ => Err(ParseError::Invalid("unable to parse quantity's suffix".into())),
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

/// `<Quantity>.isInteger() -> bool`: whether Go's `AsInt64` succeeds.
fn cel_is_integer(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Bool(q.amount().as_int64().is_some()))
}

/// `<Quantity>.asInteger() -> int`: Go's `AsInt64`, an error where it fails.
fn cel_as_integer(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    q.amount()
        .as_int64()
        .map(Value::Int)
        .ok_or_else(|| ExecutionError::function_error("asInteger", "cannot convert value to integer"))
}

/// `<Quantity>.asApproximateFloat() -> float`
fn cel_as_approximate_float(This(this): This<Value>) -> ResolveResult {
    let q = extract_quantity(&this)?;
    Ok(Value::Float(q.amount().as_approximate_float()))
}

/// `sign(<Quantity>) -> int`. The apiserver declares only the global form
/// (`cel.Overload`, though its documentation shows `<Quantity>.sign()`), and
/// refuses the member form at registration; here it is an error when
/// evaluated.
fn cel_sign(ftx: &FunctionContext, This(this): This<Value>) -> ResolveResult {
    if ftx.this.is_some() {
        return Err(ExecutionError::function_error(
            "sign",
            "sign is a global function: sign(<Quantity>), not <Quantity>.sign()",
        ));
    }
    let q = extract_quantity(&this)?;
    Ok(Value::Int(q.amount().sign()))
}

/// `<Quantity>.add(<Quantity | int>) -> Quantity`
pub(crate) fn cel_add(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let q = extract_quantity(&this)?;
    if args.is_empty() {
        return Err(ExecutionError::function_error("add", "missing argument"));
    }
    let other = quantity_or_int(&args[0], "add")?;
    let repr = q.add_or_sub(other, false).ok_or_else(|| overflow("add"))?;
    arithmetic_result(repr)
}

/// `<Quantity>.sub(<Quantity | int>) -> Quantity`
pub(crate) fn cel_sub(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let q = extract_quantity(&this)?;
    if args.is_empty() {
        return Err(ExecutionError::function_error("sub", "missing argument"));
    }
    let other = quantity_or_int(&args[0], "sub")?;
    let repr = q.add_or_sub(other, true).ok_or_else(|| overflow("sub"))?;
    arithmetic_result(repr)
}

/// An `add` / `sub` whose result needs more than an `i128` mantissa: the
/// apiserver's `inf.Dec` is exact, and has also written the result into a
/// shared receiver, so the error is noted for the validation engine, which
/// refuses the rule even where CEL absorbs it (`|| true`).
fn overflow(op: &str) -> ExecutionError {
    unsupported(format!(
        "quantity overflow: the {op}() result needs more significant digits than an i128 holds, \
         which kube-cel does not support"
    ))
}

/// A sum or difference: its value must stay within [`SUPPORTED_SCALE`]
/// (`5e1000 + 5e1000` is 1e1001), and it is the apiserver's pointer result.
fn arithmetic_result(repr: Repr) -> ResolveResult {
    if !in_range(&repr.amount()) {
        return Err(unsupported(
            "the result has a scale outside ±1000, which kube-cel does not support".to_owned(),
        ));
    }
    Ok(Value::Opaque(Arc::new(KubeQuantity {
        repr: Mutex::new(repr),
        from_arithmetic: true,
    })))
}

/// `<Quantity>.isGreaterThan(<Quantity>) -> bool`
pub(crate) fn cel_is_greater_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?.amount();
    Ok(Value::Bool(a.compare(b) == Ordering::Greater))
}

/// `<Quantity>.isLessThan(<Quantity>) -> bool`
pub(crate) fn cel_is_less_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?.amount();
    Ok(Value::Bool(a.compare(b) == Ordering::Less))
}

/// `<Quantity>.compareTo(<Quantity>) -> int`
pub(crate) fn cel_compare_to(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_quantity(&this)?;
    let b = extract_quantity(&other)?.amount();
    let result = match a.compare(b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Value::Int(result))
}

/// A quantity's value as it stands (Go passes it by value), or an int as Go's
/// `NewQuantity(n, DecimalExponent)`.
fn quantity_or_int(val: &Value, func: &str) -> Result<Amount, ExecutionError> {
    match val {
        Value::Opaque(o) => {
            let q = o
                .downcast_ref::<KubeQuantity>()
                .ok_or_else(|| ExecutionError::function_error(func, "expected Quantity or int"))?;
            Ok(q.amount())
        }
        Value::Int(n) => Ok(Amount::Int { value: *n, scale: 0 }),
        Value::UInt(n) => Ok(match i64::try_from(*n) {
            Ok(value) => Amount::Int { value, scale: 0 },
            Err(_) => Amount::Dec(Dec {
                mantissa: i128::from(*n),
                zeros: 0,
                scale: 0,
            }),
        }),
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
        // Simplified, `10e1000` is 1e1001.
        for expr in ["quantity('10e1000')", "isQuantity('10e1000')"] {
            let (result, noted) = eval_tracked(expr);
            assert!(result.is_err() && noted, "{expr}: {result:?}");
        }
        // So is a sum or difference of supported quantities.
        for expr in [
            "quantity('5e1000').add(quantity('5e1000'))",
            "quantity('9e1000').sub(quantity('-1e1000'))",
        ] {
            let (result, noted) = eval_tracked(expr);
            assert!(result.is_err() && noted, "{expr}: {result:?}");
        }
    }

    #[test]
    fn malformed_quantities_stay_invalid() {
        // Not a quantity for the apiserver either: false, nothing noted. An
        // exponent past int64 is a syntax error in Go (strconv.ParseInt).
        let long_bad = format!("{}+-1", "9".repeat(50));
        for s in [
            "1e9223372036854775808",
            "1e-9223372036854775809",
            "1e+-5",
            "1e",
            "1.2.3e2000",
            long_bad.as_str(),
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
        // Go: 100 * math.Pow10(-9), not the nearest double to 1e-7.
        assert_eq!(
            eval("quantity('100n').asApproximateFloat()"),
            Value::Float(1.0000000000000001e-7)
        );
    }

    #[test]
    fn test_parse_binary_si() {
        assert_eq!(eval("quantity('1Ki').asInteger()"), Value::Int(1024));
        assert_eq!(eval("quantity('1Mi').asInteger()"), Value::Int(1_048_576));
        assert_eq!(eval("quantity('1Gi').asInteger()"), Value::Int(1_073_741_824));
    }

    #[test]
    fn test_parse_binary_si_decimal() {
        // 1.5Gi = 1.5 * 2^30 = 1610612736, but parsed as an inf.Dec (it has a
        // fraction), which Go's AsInt64 never converts.
        assert!(crate::functions::eval("quantity('1.5Gi').asInteger()").is_err());
        assert_eq!(
            eval("quantity('1.5Gi').compareTo(quantity('1610612736'))"),
            Value::Int(0)
        );
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
        assert_eq!(eval("sign(quantity('100'))"), Value::Int(1));
        assert_eq!(eval("sign(quantity('-100'))"), Value::Int(-1));
        assert_eq!(eval("sign(quantity('0'))"), Value::Int(0));
        // The apiserver has only the global form.
        assert!(crate::functions::eval("quantity('100').sign()").is_err());
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
        assert_eq!(eval("sign(quantity('1k').sub(quantity('1k')))"), Value::Int(0));
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
        assert_eq!(eval("sign(quantity('100').sub(quantity('200')))"), Value::Int(-1));
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
        // 1Pi takes Go's inf.Dec path (its precision estimate is negative), so
        // AsInt64 refuses it although it fits.
        assert!(crate::functions::eval("quantity('1Pi').asInteger()").is_err());
        assert_eq!(
            eval("quantity('1Pi').compareTo(quantity('1125899906842624'))"),
            Value::Int(0)
        );
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

    /// Go's `copy := *q` shares the receiver's `inf.Dec`, which `copy.Add`
    /// updates in place (each checked against the apiserver).
    #[test]
    fn add_and_sub_share_the_receivers_dec() {
        let holds = |expr: &str| assert_eq!(eval(expr), Value::Bool(true), "{expr}");
        // 1.5Gi has a fraction: an inf.Dec, which the sum is written into.
        holds("[quantity('1.5Gi')].all(q, q.add(1).compareTo(q) == 0)");
        holds(
            "[quantity('1.5Gi')].all(q, !q.add(1).isInteger() && q.compareTo(quantity('1610612737')) == 0)",
        );
        holds(
            "[quantity('1.5Gi')].all(q, q.add(1).add(1) == quantity('1610612738') && q == quantity('1610612738'))",
        );
        holds(
            "[quantity('1.5Gi')].all(q, q.sub(1).compareTo(quantity('1610612735')) == 0 && q.compareTo(quantity('1610612735')) == 0)",
        );
        // Its own inf.Dec on both sides: read before it is written.
        holds(
            "[quantity('1.5Gi')].all(q, q.add(q).compareTo(quantity('3Gi')) == 0 && q.compareTo(quantity('3Gi')) == 0)",
        );
        holds("[quantity('1.5Gi')].all(q, q.sub(q).compareTo(quantity('0')) == 0 && sign(q) == 0)");
        // An int64Amount receiver: the copy is separate.
        holds(
            "[quantity('1')].all(q, q.add(1).compareTo(quantity('2')) == 0 && q.compareTo(quantity('1')) == 0)",
        );
        holds("[quantity('1')].all(q, q.add(quantity('1.5Gi')).isGreaterThan(q) && q.isInteger())");
        // The argument is passed by value.
        holds(
            "[quantity('1.5Gi')].all(q, quantity('1').add(q).compareTo(quantity('1610612737')) == 0 && q.compareTo(quantity('1.5Gi')) == 0)",
        );
        // Separate quantities do not share.
        holds(
            "[quantity('1.5Gi'), quantity('1.5Gi')].all(q, q.add(1).compareTo(quantity('1610612737')) == 0)",
        );
    }

    /// Go's `Cmp` converts an int64Amount receiver with `AsDec()` when the
    /// other side holds an `inf.Dec` (each checked against the apiserver).
    #[test]
    fn comparing_with_a_dec_promotes_the_receiver() {
        let holds = |expr: &str| assert_eq!(eval(expr), Value::Bool(true), "{expr}");
        for cmp in [
            "q.compareTo(quantity('1.5Gi')) < 0",
            "q.isLessThan(quantity('1.5Gi'))",
            "!q.isGreaterThan(quantity('1.5Gi'))",
        ] {
            holds(&format!("[quantity('1')].all(q, {cmp} && !q.isInteger())"));
        }
        // Once an inf.Dec, the receiver shares it with an add result.
        holds("[quantity('1')].all(q, q.isLessThan(quantity('1.5Gi')) && q.add(1).compareTo(q) == 0)");
        assert!(
            crate::functions::eval(
                "[quantity('1')].all(q, q.isLessThan(quantity('0.5Gi')) && q.asInteger() == 1)"
            )
            .is_err()
        );
        // Two int64Amounts, the argument, and `==` change nothing.
        holds("[quantity('1')].all(q, q.compareTo(quantity('2')) < 0 && q.isInteger())");
        holds("[quantity('1')].all(q, quantity('1.5Gi').compareTo(q) > 0 && q.isInteger())");
        holds("[quantity('1')].all(q, q == quantity('1.5Gi') || q.isInteger())");
    }

    /// A zero term needs no alignment, however far apart the scales (each
    /// checked against the apiserver).
    #[test]
    fn zero_terms_align_without_a_power_of_ten() {
        for expr in [
            "quantity('0').add(quantity('0e-1000')).compareTo(quantity('0')) == 0",
            "quantity('0').sub(quantity('0e-1000')).compareTo(quantity('0')) == 0",
            "quantity('0e1000').add(quantity('0e-1000')).compareTo(quantity('0')) == 0",
            "quantity('5').add(quantity('0e-1000')).compareTo(quantity('5')) == 0",
            "quantity('0e-1000').sub(quantity('5')).compareTo(quantity('-5')) == 0",
            "quantity('0').add(quantity('0e-39')).asApproximateFloat() == 0.0",
        ] {
            assert_eq!(eval(expr), Value::Bool(true), "{expr}");
        }
    }

    /// A number without digits is 0 on Go's int64Amount path and not a
    /// quantity on its inf.Dec path, whatever the range (each checked against
    /// the apiserver).
    #[test]
    fn digitless_numbers_follow_gos_path() {
        for (s, quantity) in [
            ("e-1001", false),
            (".e-1001", false),
            ("+.e-1001", false),
            ("-e-1001", false),
            ("e-10", false),
            (".Pi", false),
            ("e-9", true),
            (".", true),
            ("-", true),
            ("+e5", true),
            (".Ti", true),
            ("e-4294967296", true),
        ] {
            match crate::functions::eval(&format!("isQuantity('{s}')")) {
                Ok(v) => assert_eq!(v, Value::Bool(quantity), "{s}"),
                // Past the supported range: a 0 kube-cel does not support.
                Err(_) => assert!(quantity, "{s}"),
            }
        }
        assert!(crate::functions::eval("isQuantity('e1001')").is_err());
    }

    /// The mantissa range is i128's, both ends (each checked against the
    /// apiserver where it parses).
    #[test]
    fn mantissa_spans_i128() {
        let min = i128::MIN;
        let max = i128::MAX;
        assert_eq!(eval(&format!("isQuantity('{min}')")), Value::Bool(true));
        assert_eq!(eval(&format!("isQuantity('{max}')")), Value::Bool(true));
        assert_eq!(
            eval(&format!(
                "sign(quantity('{min}')) == -1 && quantity('{min}').isLessThan(quantity('{max}'))"
            )),
            Value::Bool(true)
        );
        assert_eq!(
            eval(&format!(
                "quantity('{min}').add(quantity('{max}')).compareTo(quantity('-1')) == 0"
            )),
            Value::Bool(true)
        );
        // One past either end is past what kube-cel supports.
        assert!(crate::functions::eval("isQuantity('-170141183460469231731687303715884105729')").is_err());
        assert!(crate::functions::eval("isQuantity('170141183460469231731687303715884105728')").is_err());
    }

    #[test]
    fn test_as_integer_integer_via_scale() {
        // 500m + 500m is the int64Amount 1000 × 10^-3: Go's AsInt64 refuses a
        // negative scale, so it is no integer, though its value is 1.
        assert!(crate::functions::eval("quantity('500m').add(quantity('500m')).asInteger()").is_err());
        assert_eq!(
            eval("quantity('500m').add(quantity('500m')).isInteger()"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("quantity('500m').add(quantity('500m')).compareTo(quantity('1'))"),
            Value::Int(0)
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
