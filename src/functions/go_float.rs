//! Go's float formatting (`strconv`), on an exact decimal expansion: the
//! digits `format()` and cel-go's string conversion of a duration print.

/// The exact decimal expansion of a finite `x >= 0`: its significant digits
/// (none for zero) and the exponent `dp` with `x = 0.d₁d₂… × 10^dp`. A double
/// has at most 767 significant digits, and Rust's `{:.N e}` is exact.
pub(crate) fn exact_decimal(x: f64) -> (Vec<u8>, i32) {
    let s = format!("{x:.767e}");
    let (mantissa, exp) = s.split_once('e').expect("{:e} writes an exponent");
    let mut digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        return (digits, 0);
    }
    (
        digits,
        exp.parse::<i32>().expect("{:e} exponent is an integer") + 1,
    )
}

/// Go's `decimal.Round(nd)`: keeps `nd` digits, rounding half to even, except
/// that an exact half of the first digit (`nd == 0`) rounds down. Trailing
/// zeros are dropped. A negative `nd` drops every digit: strconv's `%f` prints
/// none of them, and the value is below half a unit there.
#[cfg_attr(not(feature = "format"), allow(dead_code))]
pub(crate) fn round_go(digits: &mut Vec<u8>, dp: &mut i32, nd: i64) {
    let Ok(nd) = usize::try_from(nd) else {
        digits.clear();
        *dp = 0;
        return;
    };
    if nd >= digits.len() {
        return;
    }
    let up = if digits[nd] == 5 && nd + 1 == digits.len() {
        nd > 0 && digits[nd - 1] % 2 == 1
    } else {
        digits[nd] >= 5
    };
    digits.truncate(nd);
    if up {
        match digits.iter().rposition(|&d| d < 9) {
            Some(i) => {
                digits[i] += 1;
                digits.truncate(i + 1);
            }
            None => {
                digits.clear();
                digits.push(1);
                *dp += 1;
            }
        }
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        *dp = 0;
    }
}

/// Go's shortest decimal for a finite `x >= 0` (strconv with precision -1): the
/// fewest digits that parse back to `x`, and of two such, the nearer to `x`,
/// the even one on a tie (Rust's shortest form breaks that tie the other way:
/// `1.0000000000000003e+15` where Go writes `…02e+15`).
pub(crate) fn go_shortest(x: f64) -> (Vec<u8>, i32) {
    let (exact, dp) = exact_decimal(x);
    let parses_back = |digits: &[u8], dp: i32| {
        let s: String = digits.iter().map(|&d| char::from(b'0' + d)).collect();
        format!("0.{s}e{dp}").parse::<f64>() == Ok(x)
    };
    for n in 1..exact.len() {
        let floor = exact[..n].to_vec();
        let (mut ceil, mut ceil_dp) = (floor.clone(), dp);
        round_up_last(&mut ceil, &mut ceil_dp);
        let pick_ceil = match (parses_back(&floor, dp), parses_back(&ceil, ceil_dp)) {
            (false, false) => continue,
            (true, false) => false,
            (false, true) => true,
            (true, true) => {
                // The nearer one; on an exact half, the one with an even last digit.
                let rest = &exact[n..];
                match rest[0].cmp(&5) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal if rest.len() > 1 => true,
                    std::cmp::Ordering::Equal => floor[n - 1] % 2 == 1,
                }
            }
        };
        let (mut digits, dp) = if pick_ceil { (ceil, ceil_dp) } else { (floor, dp) };
        while digits.last() == Some(&0) {
            digits.pop();
        }
        return (digits, dp);
    }
    (exact, dp)
}

/// Adds one unit in the last place of `digits`.
fn round_up_last(digits: &mut Vec<u8>, dp: &mut i32) {
    match digits.iter().rposition(|&d| d < 9) {
        Some(i) => {
            digits[i] += 1;
            digits.truncate(i + 1);
        }
        None => {
            digits.clear();
            digits.push(1);
            *dp += 1;
        }
    }
}

pub(crate) fn push_digit(out: &mut String, d: u8) {
    out.push(char::from(b'0' + d));
}

/// strconv's `fmtF`: the integer digits (or 0), then `prec` fraction digits.
pub(crate) fn fmt_f(neg: bool, digits: &[u8], dp: i32, prec: usize) -> String {
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let at = |j: i64| {
        usize::try_from(j)
            .ok()
            .and_then(|j| digits.get(j))
            .copied()
            .unwrap_or(0)
    };
    if dp > 0 {
        for j in 0..i64::from(dp) {
            push_digit(&mut out, at(j));
        }
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        for i in 0..prec as i64 {
            push_digit(&mut out, at(i64::from(dp) + i));
        }
    }
    out
}

/// Go's `fmt` `%g`, which is `strconv.FormatFloat(x, 'g', -1, 64)`: the shortest digits, in scientific form when the exponent
/// is below -4 or at least 6 (`1e+06`, `1e-05`), "+Inf", "-Inf" and "NaN".
pub(crate) fn go_g(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    let neg = x.is_sign_negative();
    let (digits, dp) = go_shortest(x.abs());
    let exp = dp - 1;
    if digits.is_empty() || (-4..6).contains(&exp) {
        let prec = (digits.len() as i64 - i64::from(dp)).max(0) as usize;
        return fmt_f(neg, &digits, dp, prec);
    }
    // strconv's `fmtE` with the shortest digits: d[.ddd]e±XX.
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    push_digit(&mut out, digits[0]);
    if digits.len() > 1 {
        out.push('.');
        for &d in &digits[1..] {
            push_digit(&mut out, d);
        }
    }
    out.push('e');
    out.push(if exp < 0 { '-' } else { '+' });
    out.push_str(&format!("{:02}", exp.abs()));
    out
}
