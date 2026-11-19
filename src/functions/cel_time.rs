//! cel-go's standard time conversions and duration arithmetic, where `cel`'s
//! differ.
//!
//! The apiserver runs cel-go v0.26, which builds durations with Go's
//! `time.ParseDuration` and timestamps with `time.Parse(time.RFC3339, ..)`
//! (go1.26.2), keeps a duration to int64 nanoseconds, and prints a duration as
//! float64 seconds and a timestamp in its own offset. `cel` 0.16 follows the
//! CEL spec instead: a duration up to 315,576,000,000 seconds, a timestamp
//! with a lowercase `t` / `z`, a space for the `T` or a leap second, exact
//! nanoseconds in `string(<duration>)`, and UTC in `string(<timestamp>)`.
//!
//! kube-cel keeps `cel`'s overloads and puts its own step in front of them:
//! [`declare`] adds macros that rewrite, at compile time,
//!
//! - `duration(e)` to `duration(@kube.durationInput(e))`, and the same for
//!   `timestamp`: a string goes through the Go parser, which hands `cel`'s
//!   identity overload a value Go would build, or fails as Go does; any other
//!   value (`timestamp(0)`, `duration(d)`) passes through to `cel`'s overload;
//! - `string(e)` to `string(@kube.stringInput(e))`, which prints a duration, a
//!   timestamp or a double as cel-go does and passes any other value through;
//! - `a + b`, `a - b` and `-a` to `@kube.add(a, b)`, `@kube.sub(a, b)` and
//!   `@kube.negate(a)`, which do duration arithmetic in int64 nanoseconds with
//!   Go's overflow checks and hand everything else to `cel`'s operator (cel-go
//!   negates a duration reached through `dyn`; `cel` has no such operator),
//!   noting a bytes concatenation as unsupported (see `add`);
//!
//! the leading-dot forms (`.duration(e)`, ..) included; and it declares
//! cel-go's `int(<duration>)`, the nanoseconds, which `cel` lacks. A literal argument is
//! checked when the expression compiles, as the apiserver's
//! `ValidateDurationLiterals` / `ValidateTimestampLiterals` do: a literal the
//! conversion refuses fails to compile, even where evaluation would not reach
//! it.
//!
//! The helpers are spelled with an `@`, so no CEL source can call them, and
//! macros rewrite only what the parser reads: a program compiled with
//! [`cel::Program::compile`] rather than [`cel::Env::compile`] keeps `cel`'s
//! behavior.

use cel::{
    DeclarationError, Env, ExecutionError,
    common::{
        ast::{CallExpr, Expr, IdedExpr, LiteralValue, operators},
        decls::Signature,
        types::{
            CelBytes, CelDouble, CelDuration, CelInt, CelString, CelTimestamp, DURATION_TYPE, DYN_TYPE,
            INT_TYPE,
        },
        value::{CowVal, Val},
    },
    objects::Value,
    parser::{Macro, MacroExprHelper},
};
use chrono::{DateTime, Datelike, FixedOffset};

use crate::functions::go_float::{fmt_f, go_g, go_shortest};

const DURATION_INPUT: &str = "@kube.durationInput";
const TIMESTAMP_INPUT: &str = "@kube.timestampInput";
const STRING_INPUT: &str = "@kube.stringInput";
const ADD: &str = "@kube.add";
const SUB: &str = "@kube.sub";
const NEGATE: &str = "@kube.negate";

/// Seconds from the Unix epoch to `0001-01-01T00:00:00Z` and to
/// `9999-12-31T23:59:59Z`, cel-go's timestamp range.
const MIN_UNIX: i64 = -62_135_596_800;
const MAX_UNIX: i64 = 253_402_300_799;

/// Adds the macros and helpers described in the [module docs](self).
pub(crate) fn declare(env: &mut Env) -> Result<(), DeclarationError> {
    for (helper, f) in [
        (DURATION_INPUT, duration_input as cel::common::functions::Function),
        (TIMESTAMP_INPUT, timestamp_input),
        (STRING_INPUT, string_input),
    ] {
        env.add_overload(helper, helper, Signature::new(vec![DYN_TYPE]), f)?;
    }
    env.add_overload(ADD, ADD, Signature::new(vec![DYN_TYPE, DYN_TYPE]), add)?;
    env.add_overload(SUB, SUB, Signature::new(vec![DYN_TYPE, DYN_TYPE]), sub)?;
    env.add_overload(NEGATE, NEGATE, Signature::new(vec![DYN_TYPE]), negate)?;
    env.add_overload(
        "int",
        "duration_to_int64",
        Signature::new(vec![DURATION_TYPE]).returns(INT_TYPE),
        duration_to_int,
    )?;

    for (conversion, helper) in [
        ("duration", DURATION_INPUT),
        ("timestamp", TIMESTAMP_INPUT),
        ("string", STRING_INPUT),
    ] {
        for name in [conversion.to_owned(), format!(".{conversion}")] {
            let call = name.clone();
            env.add_macro(Macro::global(name, 1, move |h, _, args| {
                if let Expr::Literal(literal) = &args[0].expr
                    && !literal_converts(conversion, literal)
                {
                    return Err(h.new_error(args[0].id, format!("invalid {conversion} argument")));
                }
                let input = call_expr(h, helper, std::mem::take(args));
                Ok(Some(call_expr(h, &call, vec![input])))
            }))?;
        }
    }
    for (operator, arity, helper) in [
        (operators::ADD, 2, ADD),
        (operators::SUBSTRACT, 2, SUB),
        (operators::NEGATE, 1, NEGATE),
    ] {
        env.add_macro(Macro::global(operator, arity, move |h, _, args| {
            Ok(Some(call_expr(h, helper, std::mem::take(args))))
        }))?;
    }
    Ok(())
}

/// A global call to `name` with `args`, as written where the expanded call is.
/// The parser does not expand macros in what a macro returns, so a call to
/// `duration` here reaches `cel`'s overloads.
fn call_expr(h: &mut MacroExprHelper<'_>, name: &str, args: Vec<IdedExpr>) -> IdedExpr {
    h.next_expr(Expr::Call(CallExpr {
        target: None,
        func_name: name.to_owned(),
        args,
    }))
}

/// Whether the apiserver's literal validator lets `conversion(literal)`
/// compile: the conversion succeeds on it. Only a string (and for `timestamp`
/// an int) literal has an overload; any other is a type error there.
fn literal_converts(conversion: &str, literal: &LiteralValue) -> bool {
    match (conversion, literal) {
        ("duration", LiteralValue::String(s)) => go_parse_duration(s.inner().as_bytes()).is_some(),
        ("duration", _) => false,
        ("timestamp", LiteralValue::String(s)) => {
            // A zone offset of 24 hours or more is Go's, which `chrono` cannot
            // hold: evaluation fails on it (see `go_timestamp`), compiling does not.
            parse_rfc3339(s.inner().as_bytes(), false)
                .is_some_and(|t| (MIN_UNIX..=MAX_UNIX).contains(&t.unix))
        }
        ("timestamp", LiteralValue::Int(i)) => (MIN_UNIX..=MAX_UNIX).contains(i.inner()),
        ("timestamp", _) => false,
        _ => true,
    }
}

pub(super) fn single<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let n = args.len();
    let [arg]: [CowVal<'b, 'v>; 1] = args
        .try_into()
        .map_err(|_| ExecutionError::invalid_argument_count(1, n))?;
    Ok(arg)
}

fn pair<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<[CowVal<'b, 'v>; 2], ExecutionError> {
    let n = args.len();
    args.try_into()
        .map_err(|_| ExecutionError::invalid_argument_count(2, n))
}

/// cel-go's error for a string the conversion to `type_name` refuses.
pub(super) fn conversion_error(function: &str, type_name: &str) -> ExecutionError {
    ExecutionError::function_error(
        function,
        format!("type conversion error from 'string' to '{type_name}'"),
    )
}

/// `duration(<string>)` as cel-go has it: Go's `time.ParseDuration`.
fn duration_input<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let Some(s) = arg.downcast_ref::<CelString>() else {
        return Ok(arg);
    };
    let nanos = go_parse_duration(s.inner().as_bytes())
        .ok_or_else(|| conversion_error("duration", "google.protobuf.Duration"))?;
    Ok(CowVal::owned(CelDuration::from(chrono::Duration::nanoseconds(
        nanos,
    ))))
}

/// `timestamp(<string>)` as cel-go has it: Go's `time.Parse(time.RFC3339, s)`,
/// then cel-go's range.
fn timestamp_input<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let Some(s) = arg.downcast_ref::<CelString>() else {
        return Ok(arg);
    };
    Ok(CowVal::owned(CelTimestamp::from(go_timestamp(s.inner())?)))
}

/// `string(<duration>)`, `string(<timestamp>)` and `string(<double>)` as
/// cel-go prints them (a double with `strconv.FormatFloat(d, 'g', -1, 64)`:
/// `1e+06`, `+Inf`, where `cel` writes `1000000`, `inf`).
fn string_input<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let s = if let Some(d) = arg.downcast_ref::<CelDuration>() {
        duration_string(d.inner())
    } else if let Some(t) = arg.downcast_ref::<CelTimestamp>() {
        timestamp_string(t.inner())
    } else if let Some(d) = arg.downcast_ref::<CelDouble>() {
        go_g(*d.inner())
    } else {
        return Ok(arg);
    };
    Ok(CowVal::owned(CelString::from(s)))
}

/// A duration's nanoseconds, which cel-go keeps in an int64. Every duration
/// kube-cel builds fits; one bound from outside that does not is refused
/// rather than computed with.
fn go_nanos(d: &CelDuration) -> Result<i64, ExecutionError> {
    d.inner().num_nanoseconds().ok_or_else(|| {
        ExecutionError::function_error("duration", "duration outside the int64 nanosecond range")
    })
}

fn overflow(op: &'static str, lhs: &dyn Val, rhs: &dyn Val) -> ExecutionError {
    ExecutionError::Overflow(
        op,
        Value::try_from(lhs).unwrap_or(Value::Null),
        Value::try_from(rhs).unwrap_or(Value::Null),
    )
}

fn duration_val<'b, 'v>(nanos: i64) -> CowVal<'b, 'v> {
    CowVal::owned(CelDuration::from(chrono::Duration::nanoseconds(nanos)))
}

/// Why an evaluation that concatenated bytes is refused.
const UNSUPPORTED_BYTES_ADD: &str = "a bytes `+`, whose result the apiserver before 1.38 can change \
                                     with a later `+` on the same bytes value (cel-go appends into \
                                     the left value's spare capacity), which kube-cel does not \
                                     model";

/// `a + b`: two durations add in int64 nanoseconds (cel-go's
/// `addDurationChecked`); anything else is `cel`'s `+`, whose timestamp range
/// is cel-go's.
///
/// Two bytes concatenate, and the evaluation is noted as unsupported: cel-go
/// before v0.30 (the apiserver before 1.38) is `append(a, b...)`, so where `a`
/// has spare capacity the result shares `a`'s array, and a later `a + c` writes
/// over it (`[b'abc'].all(x, [x + b'1', x + b'2'][0] == b'abc1')` is false
/// there). The capacity hangs on Go's allocator, and a literal or a folded
/// `bytes('..')` keeps one array across a loop's iterations, so kube-cel
/// refuses any bytes concatenation evaluation reaches, even where CEL absorbs
/// the result.
fn add<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [lhs, rhs] = pair(args)?;
    match (
        lhs.downcast_ref::<CelDuration>(),
        rhs.downcast_ref::<CelDuration>(),
    ) {
        (Some(a), Some(b)) => go_nanos(a)?
            .checked_add(go_nanos(b)?)
            .map(duration_val)
            .ok_or_else(|| overflow("add", lhs.as_ref(), rhs.as_ref())),
        (a, b) => {
            for d in [a, b].into_iter().flatten() {
                go_nanos(d)?;
            }
            if lhs.downcast_ref::<CelBytes>().is_some() && rhs.downcast_ref::<CelBytes>().is_some() {
                crate::functions::note_unsupported_error(|| {
                    ExecutionError::function_error("_+_", UNSUPPORTED_BYTES_ADD)
                });
            }
            let adder = lhs.as_adder().ok_or_else(|| {
                ExecutionError::UnsupportedBinaryOperator(
                    "add",
                    Value::try_from(lhs.as_ref()).unwrap_or(Value::Null),
                    Value::try_from(rhs.as_ref()).unwrap_or(Value::Null),
                )
            })?;
            Ok(CowVal::Owned(adder.add(rhs.as_ref())?.into_owned()))
        }
    }
}

/// `a - b`: two durations subtract in int64 nanoseconds, two timestamps as
/// cel-go's `subtractTimeChecked` does, and a timestamp minus a duration
/// negates the duration first (`subtractTimeDurationChecked`, an overflow for
/// the most negative one); anything else is `cel`'s `-`.
fn sub<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [lhs, rhs] = pair(args)?;
    if let (Some(a), Some(b)) = (
        lhs.downcast_ref::<CelDuration>(),
        rhs.downcast_ref::<CelDuration>(),
    ) {
        return go_nanos(a)?
            .checked_sub(go_nanos(b)?)
            .map(duration_val)
            .ok_or_else(|| overflow("sub", lhs.as_ref(), rhs.as_ref()));
    }
    if let (Some(x), Some(y)) = (
        lhs.downcast_ref::<CelTimestamp>(),
        rhs.downcast_ref::<CelTimestamp>(),
    ) {
        return subtract_time(x.inner(), y.inner())
            .map(duration_val)
            .ok_or_else(|| overflow("sub", lhs.as_ref(), rhs.as_ref()));
    }
    if let Some(d) = rhs.downcast_ref::<CelDuration>() {
        let nanos = go_nanos(d)?;
        if nanos == i64::MIN && lhs.downcast_ref::<CelTimestamp>().is_some() {
            return Err(overflow("sub", lhs.as_ref(), rhs.as_ref()));
        }
    }
    if let Some(d) = lhs.downcast_ref::<CelDuration>() {
        go_nanos(d)?;
    }
    let subtractor = lhs.as_subtractor().ok_or_else(|| {
        ExecutionError::UnsupportedBinaryOperator(
            "sub",
            Value::try_from(lhs.as_ref()).unwrap_or(Value::Null),
            Value::try_from(rhs.as_ref()).unwrap_or(Value::Null),
        )
    })?;
    Ok(CowVal::Owned(subtractor.sub(rhs.as_ref())?.into_owned()))
}

/// `-a`: a duration negates in int64 nanoseconds (cel-go's
/// `negateDurationChecked`, an overflow for the most negative one); anything
/// else is `cel`'s `-`.
fn negate<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    if let Some(d) = arg.downcast_ref::<CelDuration>() {
        return go_nanos(d)?
            .checked_neg()
            .map(duration_val)
            .ok_or_else(|| ExecutionError::function_error("-_", "integer overflow"));
    }
    let negator = arg
        .as_negator()
        .ok_or_else(|| ExecutionError::function_error("-_", "no such overload"))?;
    Ok(CowVal::Owned(negator.negate()?))
}

/// cel-go's `int(<duration>)`: the nanoseconds.
fn duration_to_int<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let d = arg
        .downcast_ref::<CelDuration>()
        .ok_or_else(|| ExecutionError::function_error("int", "expected a duration"))?;
    Ok(CowVal::owned(CelInt::from(go_nanos(d)?)))
}

/// cel-go's `subtractTimeChecked`: whole seconds and the nanosecond remainders
/// subtract apart, the seconds scale to nanoseconds with an overflow check and
/// the remainders add back, so a difference just inside the int64 range can
/// still overflow on the way.
fn subtract_time(x: &DateTime<FixedOffset>, y: &DateTime<FixedOffset>) -> Option<i64> {
    let sec = x.timestamp().checked_sub(y.timestamp())?;
    let nsec = i64::from(x.timestamp_subsec_nanos()) - i64::from(y.timestamp_subsec_nanos());
    sec.checked_mul(1_000_000_000)?.checked_add(nsec)
}

// ---------------------------------------------------------------------------
// Printing, as cel-go's `ConvertToType(StringType)`
// ---------------------------------------------------------------------------

/// `time.RFC3339Nano`: the year in at least four digits with no sign (an
/// offset can make the local year 10000 or 0000), the fraction without its
/// trailing zeros, "Z" for a zero offset, else the ±hh:mm offset the timestamp
/// keeps. (`chrono`'s `%Y` writes `+10000`.)
pub(crate) fn timestamp_string(t: &DateTime<FixedOffset>) -> String {
    let mut s = format!("{:04}", t.year());
    s.push_str(&t.format("-%m-%dT%H:%M:%S").to_string());
    let frac = t.format("%9f").to_string();
    let frac = frac.trim_end_matches('0');
    if !frac.is_empty() {
        s.push('.');
        s.push_str(frac);
    }
    if t.offset().local_minus_utc() == 0 {
        s.push('Z');
    } else {
        s.push_str(&t.format("%:z").to_string());
    }
    s
}

/// `strconv.FormatFloat(d.Seconds(), 'f', -1, 64) + "s"`, where `Seconds()`
/// adds the whole seconds and the nanosecond remainder as two floats.
pub(crate) fn duration_string(d: &chrono::Duration) -> String {
    let secs = d.num_nanoseconds().map_or_else(
        || d.num_seconds() as f64,
        |n| (n / 1_000_000_000) as f64 + (n % 1_000_000_000) as f64 / 1e9,
    );
    let (digits, dp) = go_shortest(secs.abs());
    let prec = (digits.len() as i64 - i64::from(dp)).max(0) as usize;
    let mut s = fmt_f(secs < 0.0, &digits, dp, prec);
    s.push('s');
    s
}

/// cel-go's string conversion of a timestamp or a duration (see
/// [`timestamp_string`], [`duration_string`]); `None` for any other value.
#[cfg_attr(not(feature = "format"), allow(dead_code))]
pub(crate) fn time_string(v: &Value) -> Option<String> {
    match v {
        Value::Timestamp(t) => Some(timestamp_string(t)),
        Value::Duration(d) => Some(duration_string(d)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Go's time.Parse(time.RFC3339, ..)
// ---------------------------------------------------------------------------

/// A time as Go's `time.Parse(time.RFC3339, ..)` reads it: seconds from the
/// Unix epoch (floored), the nanoseconds past them and the zone offset in
/// seconds.
pub(super) struct GoTime {
    pub(super) unix: i64,
    pub(super) nanos: u32,
    pub(super) offset: i32,
}

/// `timestamp(<string>)` as cel-go has it: Go's parse, then cel-go's range. A
/// zone offset of 24 hours or more, which Go accepts up to `+24:60`, is an
/// error here, `chrono` cannot hold it, and noted as unsupported: the
/// validation engine refuses the rule even where CEL absorbs the error.
fn go_timestamp(s: &str) -> Result<DateTime<FixedOffset>, ExecutionError> {
    let t = parse_rfc3339(s.as_bytes(), false)
        .ok_or_else(|| conversion_error("timestamp", "google.protobuf.Timestamp"))?;
    if !(MIN_UNIX..=MAX_UNIX).contains(&t.unix) {
        return Err(ExecutionError::function_error("timestamp", "timestamp overflow"));
    }
    let Some(offset) = FixedOffset::east_opt(t.offset) else {
        let error = || {
            ExecutionError::function_error(
                "timestamp",
                "a zone offset of 24 hours or more, which kube-cel does not support",
            )
        };
        crate::functions::note_unsupported_error(error);
        return Err(error());
    };
    let utc = DateTime::from_timestamp(t.unix, t.nanos).expect("cel-go's timestamp range fits chrono");
    Ok(utc.with_timezone(&offset))
}

/// Go's `time.Parse(time.RFC3339, s)` (go1.26.2), its general parser (the
/// RFC 3339 fast path it tries first accepts a subset of it, with the same
/// result): a four-digit year, two-digit month and day, a `T`, a one- or
/// two-digit hour, two-digit minute and second, an optional fraction after a
/// `.` or `,` (any number of digits, nine kept), then `Z` or `±hh:mm` with
/// `hh` up to 24 and `mm` up to 60. `None` where Go fails. With `zone_optional`
/// the zone may also be left out, read as UTC, as Go's parse with the layout
/// `2006-01-02T15:04:05` (strfmt's `ISO8601LocalTime`) has it.
pub(super) fn parse_rfc3339(s: &[u8], zone_optional: bool) -> Option<GoTime> {
    /// Go's `getnum`: one or two digits, exactly two if `fixed`.
    fn getnum(s: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
        let d0 = u32::from(s.first().filter(|c| c.is_ascii_digit())? - b'0');
        match s.get(1).filter(|c| c.is_ascii_digit()) {
            Some(d1) => Some((d0 * 10 + u32::from(d1 - b'0'), &s[2..])),
            None if fixed => None,
            None => Some((d0, &s[1..])),
        }
    }
    fn skip(s: &[u8], c: u8) -> Option<&[u8]> {
        (s.first() == Some(&c)).then(|| &s[1..])
    }

    let year_digits = s.get(..4).filter(|y| y.iter().all(u8::is_ascii_digit))?;
    let year = year_digits.iter().fold(0i64, |y, c| y * 10 + i64::from(c - b'0'));
    let s = skip(&s[4..], b'-')?;
    let (month, s) = getnum(s, true)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let s = skip(s, b'-')?;
    let (day, s) = getnum(s, true)?;
    let s = skip(s, b'T')?;
    let (hour, s) = getnum(s, false)?;
    let s = skip(s, b':')?;
    let (min, s) = getnum(s, true)?;
    let s = skip(s, b':')?;
    let (sec, mut s) = getnum(s, true)?;
    if hour >= 24 || min >= 60 || sec >= 60 {
        return None;
    }
    let mut nanos = 0;
    if s.len() >= 2 && matches!(s[0], b'.' | b',') && s[1].is_ascii_digit() {
        let n = 1 + s[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        // `parseNanoseconds`: the first nine digits, scaled to nanoseconds.
        let kept = &s[1..n.min(10)];
        nanos =
            kept.iter().fold(0u32, |v, c| v * 10 + u32::from(c - b'0')) * 10u32.pow(9 - kept.len() as u32);
        s = &s[n..];
    }
    let offset = if let Some(rest) = skip(s, b'Z') {
        s = rest;
        0
    } else if zone_optional && s.is_empty() {
        0
    } else {
        if s.len() < 6 || s[3] != b':' {
            return None;
        }
        let (hr, _) = getnum(&s[1..3], true)?;
        let (mm, _) = getnum(&s[4..6], true)?;
        if hr > 24 || mm > 60 {
            return None;
        }
        let offset = ((hr * 60 + mm) * 60) as i32;
        let offset = match s[0] {
            b'+' => offset,
            b'-' => -offset,
            _ => return None,
        };
        s = &s[6..];
        offset
    };
    if !s.is_empty() || day < 1 || day > days_in(month, year) {
        return None;
    }
    let local = days_from_civil(year, month, day) * 86_400 + i64::from(hour * 3600 + min * 60 + sec);
    Some(GoTime {
        unix: local - i64::from(offset),
        nanos,
        offset,
    })
}

fn days_in(month: u32, year: i64) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Go's time.ParseDuration
// ---------------------------------------------------------------------------

/// Go's `time.ParseDuration` (go1.26.2), exactly: integer arithmetic, the
/// fractional part through `float64` as Go does it, and every overflow an
/// error (`None`). The nanoseconds.
pub(crate) fn go_parse_duration(orig: &[u8]) -> Option<i64> {
    const LIMIT: u64 = 1 << 63;
    let mut s = orig;
    let mut neg = false;
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    if s == b"0" {
        return Some(0);
    }
    if s.is_empty() {
        return None;
    }
    let mut d: u64 = 0;
    while !s.is_empty() {
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return None;
        }
        let before = s.len();
        let (v, rest) = leading_int(s)?;
        s = rest;
        let pre = before != s.len();
        let (mut f, mut scale, mut post) = (0u64, 1f64, false);
        if s.first() == Some(&b'.') {
            s = &s[1..];
            let before = s.len();
            (f, scale, s) = leading_fraction(s);
            post = before != s.len();
        }
        if !pre && !post {
            return None;
        }
        let unit_len = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if unit_len == 0 {
            return None;
        }
        let unit: u64 = match &s[..unit_len] {
            b"ns" => 1,
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60_000_000_000,
            b"h" => 3_600_000_000_000,
            _ => return None,
        };
        s = &s[unit_len..];
        if v > LIMIT / unit {
            return None;
        }
        let mut v = v * unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > LIMIT {
                return None;
            }
        }
        d = d.wrapping_add(v);
        if d > LIMIT {
            return None;
        }
    }
    if neg {
        return Some((d as i64).wrapping_neg());
    }
    if d > LIMIT - 1 {
        return None;
    }
    Some(d as i64)
}

/// Go's `leadingInt`: the leading `[0-9]*`, `None` past 2^63.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut x: u64 = 0;
    let mut i = 0;
    while let Some(&c) = s.get(i).filter(|c| c.is_ascii_digit()) {
        if x > (1u64 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1u64 << 63 {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

/// Go's `leadingFraction`: the leading `[0-9]*` as `x / scale`, which stops
/// gaining precision (but keeps consuming digits) once it would overflow.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let (mut x, mut scale, mut overflow) = (0u64, 1f64, false);
    let mut i = 0;
    while let Some(&c) = s.get(i).filter(|c| c.is_ascii_digit()) {
        i += 1;
        if overflow {
            continue;
        }
        if x > ((1u64 << 63) - 1) / 10 {
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1u64 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, &s[i..])
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cel::Value;

    use crate::functions::eval;

    fn holds(expr: &str) {
        assert_eq!(eval(expr), Ok(Value::Bool(true)), "{expr}");
    }

    fn errs(expr: &str) {
        assert!(eval(expr).is_err(), "{expr} must be an error");
    }

    fn compiles(expr: &str) -> bool {
        crate::new_env().compile(expr).is_ok()
    }

    /// `expr` with the string variable `s` bound to `value`: a conversion that
    /// is not of a literal, so it is checked when it runs.
    fn eval_with(expr: &str, value: &str) -> Result<Value, cel::ExecutionError> {
        let env = Arc::new(crate::new_env());
        let mut ctx = crate::new_context(Arc::clone(&env)).unwrap();
        ctx.add_variable_from_value("s", Value::String(Arc::new(value.into())));
        env.compile(expr).unwrap().execute(&ctx)
    }

    fn string_of(expr: &str, value: &str) -> Option<String> {
        match eval_with(expr, value) {
            Ok(Value::String(s)) => Some(s.to_string()),
            Ok(other) => panic!("{expr} on {value:?}: {other:?}"),
            Err(_) => None,
        }
    }

    // Expected values below are cel-go's, from the reference apiserver's base
    // environment (go1.26.2, `TZ=UTC`).

    #[test]
    fn duration_strings_follow_go() {
        let d = |s: &str| string_of("string(duration(s))", s);
        assert_eq!(d("9223372036.854775807s").as_deref(), Some("9223372036.854776s"));
        assert_eq!(
            d("-9223372036.854775808s").as_deref(),
            Some("-9223372036.854776s")
        );
        assert_eq!(d("9223372036.854775808s"), None);
        assert_eq!(d("-9223372036.854775809s"), None);
        assert_eq!(d("2562047h47m16.854775808s"), None);
        assert_eq!(d("1s1s").as_deref(), Some("2s"));
        assert_eq!(d("+1s").as_deref(), Some("1s"));
        assert_eq!(d(".5s").as_deref(), Some("0.5s"));
        assert_eq!(d("5.s").as_deref(), Some("5s"));
        assert_eq!(d("1.5ns").as_deref(), Some("0.000000001s"));
        assert_eq!(d("0.1234567890123456789s").as_deref(), Some("0.123456789s"));
        assert_eq!(d("1µs").as_deref(), Some("0.000001s"));
        assert_eq!(d("-0").as_deref(), Some("0s"));
        // What `cel`'s own parser takes and Go's refuses.
        for refused in ["1e3s", "1h-1m", "1s ", "1d", ""] {
            assert_eq!(d(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn timestamp_strings_follow_go() {
        let t = |s: &str| string_of("string(timestamp(s))", s);
        assert_eq!(t("2024-01-15T1:30:00Z").as_deref(), Some("2024-01-15T01:30:00Z"));
        assert_eq!(
            t("2024-01-15T10:30:00,5Z").as_deref(),
            Some("2024-01-15T10:30:00.5Z")
        );
        assert_eq!(
            t("2024-01-15T10:30:00.1234567891234Z").as_deref(),
            Some("2024-01-15T10:30:00.123456789Z")
        );
        assert_eq!(
            t("2024-01-15T10:30:00-00:00").as_deref(),
            Some("2024-01-15T10:30:00Z")
        );
        assert_eq!(
            t("0000-12-31T23:59:59-00:01").as_deref(),
            Some("0000-12-31T23:59:59-00:01")
        );
        // What `cel`'s (chrono's) parser takes and Go's refuses, and cel-go's range.
        for refused in [
            "2024-01-15t10:30:00Z",
            "2024-01-15T10:30:00z",
            "2024-01-15 10:30:00Z",
            "2024-12-31T23:59:60Z",
            "2024-01-15T24:00:00Z",
            "0001-01-01T00:00:00+00:01",
            "9999-12-31T23:59:59-00:01",
        ] {
            assert_eq!(t(refused), None, "{refused:?}");
        }
    }

    /// Go takes a zone offset up to `+24:60`; `chrono` holds less than 24
    /// hours, so kube-cel fails on the rest (fail-closed).
    #[test]
    fn a_zone_offset_of_a_day_is_an_error() {
        assert_eq!(
            string_of("string(timestamp(s))", "2024-01-15T10:30:00+24:00"),
            None
        );
        assert_eq!(
            string_of("string(timestamp(s))", "2024-01-15T10:30:00+23:59").as_deref(),
            Some("2024-01-15T10:30:00+23:59")
        );
        assert!(compiles("timestamp('2024-01-15T10:30:00+24:00')"));
    }

    #[test]
    fn printing_follows_go() {
        holds("string(timestamp('2024-01-15T10:30:00+09:00')) == '2024-01-15T10:30:00+09:00'");
        holds("string(timestamp('2024-01-15T10:30:00.100+05:45')) == '2024-01-15T10:30:00.1+05:45'");
        holds(
            "string(timestamp('2024-01-15T10:30:00+09:00') + duration('1h')) == '2024-01-15T11:30:00+09:00'",
        );
        holds("string(duration('123456789.123456789s')) == '123456789.12345679s'");
        holds("string(duration('-0.000000001s')) == '-0.000000001s'");
        holds("string([duration('1s')][0]) == '1s' && string(dyn(duration('1.5s'))) == '1.5s'");
        holds(".string(duration('1h')) == '3600s'");
        // An offset can make the local year 10000: no sign, as Go writes it.
        holds(
            "string(timestamp('9999-12-31T23:00:00+01:00') + duration('1h')) == '10000-01-01T00:00:00+01:00' \
             && string(timestamp('0000-12-31T23:00:00-01:00')) == '0000-12-31T23:00:00-01:00'",
        );
        // A double as Go's `strconv.FormatFloat(d, 'g', -1, 64)`.
        holds(
            "[string(1e6), string(1e-5), string(123456.0), string(1e21), string(0.0001), string(-0.0), \
             string(1.0 / 0.0), string(-1.0 / 0.0), string(0.0 / 0.0), string(1.5), string(100.0)] \
             == ['1e+06', '1e-05', '123456', '1e+21', '0.0001', '-0', '+Inf', '-Inf', 'NaN', '1.5', '100']",
        );
        holds(".string(dyn(1e6)) == '1e+06'");
        // Any other value keeps `cel`'s conversion.
        holds("string(1) + string(1u) + string(b'x') + string(true) == '11xtrue'");
    }

    #[test]
    fn durations_stay_within_int64_nanoseconds() {
        errs("duration('9223372036s') + duration('1s') > duration('0s')");
        errs("duration('-9223372036.854775808s') - duration('1ns') < duration('0s')");
        errs("timestamp('9999-01-01T00:00:00Z') - timestamp('0001-01-01T00:00:00Z') > duration('0s')");
        // Go negates the duration first, which overflows for the most negative one.
        errs("timestamp('2000-01-01T00:00:00Z') - duration('-9223372036.854775808s') > timestamp(0)");
        holds(
            "string(timestamp('1970-01-01T00:00:00Z') - timestamp('2262-04-11T23:47:16.854775808Z')) \
             == '-9223372036.854776s'",
        );
        // The difference fits in an int64, but its whole seconds scaled to
        // nanoseconds do not: an overflow in cel-go.
        errs("timestamp('2147-10-30T01:40:18.743673823Z') - timestamp('1855-07-21T01:53:01.950570360Z')");
        holds("timestamp('2024-01-15T10:30:00Z') + duration('1h') == timestamp('2024-01-15T11:30:00Z')");
        errs("timestamp('9999-12-31T23:59:59.999999999Z') + duration('1ns')");
        // Everything else keeps `cel`'s operators.
        holds("1 + 2 == 3 && [1] + [2] == [1, 2] && 'a' + 'b' == 'ab' && 5 - 3 == 2 && 2.5 - 0.5 == 2.0");
        errs("9223372036854775807 + 1");
    }

    /// The apiserver refuses a literal the conversion fails on when the
    /// expression compiles, reached or not.
    #[test]
    fn literals_are_checked_when_compiling() {
        for refused in [
            "true || duration('9223372037s') > duration('0s')",
            "true || duration('1e3s') > duration('0s')",
            "true || .duration('1h-1m') > duration('0s')",
            "true || timestamp('2024-01-15t10:30:00Z') == timestamp(0)",
            "true || .timestamp('x') == timestamp(0)",
            "true || timestamp(253402300800) == timestamp(0)",
            "true || timestamp(-62135596801) == timestamp(0)",
            "true || duration(1) == duration('1s')",
            "true || timestamp(1u) == timestamp(0)",
            "true || timestamp(1.0) == timestamp(0)",
        ] {
            assert!(!compiles(refused), "{refused} must not compile");
        }
        assert!(compiles("timestamp(253402300799) > timestamp(-62135596800)"));
        // Not a literal: an error when it runs, which CEL can absorb.
        assert_eq!(
            eval_with("timestamp(s) == timestamp(0) || true", "x"),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            eval_with("duration(s) > duration('0s') || true", "1e3s"),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn other_overloads_and_names_are_unchanged() {
        holds("timestamp(timestamp(0)) == timestamp(0) && duration(duration('1s')) == duration('1s')");
        holds("timestamp(1) == timestamp('1970-01-01T00:00:01Z')");
        holds(
            "type(duration('1s')) == google.protobuf.Duration && type(timestamp(0)) == google.protobuf.Timestamp",
        );
        holds("[duration('1s')].all(duration, type(duration) == google.protobuf.Duration)");
        holds("[timestamp(0)].all(timestamp, type(timestamp) == google.protobuf.Timestamp)");
        holds("optional.of('1s').optMap(x, duration(x)).value() == duration('1s')");
        errs("dyn('1s').duration()");
    }

    /// A bytes concatenation keeps its value and is noted as unsupported,
    /// reached alone, absorbed or not: the apiserver before 1.38 appends into
    /// the left value's spare capacity (`[b'abc'].map(x, [x + b'1', x +
    /// b'2'])` is `[[b'abc2', b'abc2']]` there, cel-go v0.26 through
    /// k8s.io/apiserver v0.36.1). Other `+`s, and a bytes `+` evaluation does
    /// not reach, are not noted.
    #[test]
    fn bytes_concatenation_is_noted() {
        let tracked = |expr: &str| crate::functions::tracking_unsupported(|| eval(expr));
        for expr in [
            "[b'abc'].map(x, [x + b'1', x + b'2']) == [[b'abc1', b'abc2']]",
            "b'abc' + b'1' == b'abc1'",
            "b'' + b'' == b''",
            "[b'abc'].all(x, [x + b'1', x + b'2'][0] == b'abc2') || true",
        ] {
            let (result, noted) = tracked(expr);
            assert_eq!(result, Ok(Value::Bool(true)), "{expr}");
            assert!(noted.is_some(), "{expr} must be noted");
        }
        for expr in [
            "'ab' + 'c' == 'abc' && [b'a'] + [b'b'] == [b'a', b'b'] && 1 + 2 == 3",
            "duration('1s') + duration('1s') == duration('2s')",
            "true || b'a' + b'b' == b''",
        ] {
            let (result, noted) = tracked(expr);
            assert_eq!(result, Ok(Value::Bool(true)), "{expr}");
            assert!(noted.is_none(), "{expr} must not be noted");
        }
        // Bytes and another type: no overload, an error as before, not noted.
        let (result, noted) = tracked("dyn(b'a') + dyn('b')");
        assert!(result.is_err() && noted.is_none());
    }
}
