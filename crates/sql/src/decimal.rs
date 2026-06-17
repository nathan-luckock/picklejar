//! Exact decimal numbers, represented as `(mantissa: i128, scale: u32)` where
//! the value is `mantissa / 10^scale`. No external crate.
//!
//! `picklejar` uses this for the `DECIMAL` / `NUMERIC` type: exact base-10
//! arithmetic (no binary floating-point rounding). An `i128` mantissa holds
//! about 38 significant digits, which is enough for money and most fixed-point
//! needs; this is not bignum-unbounded `NUMERIC`, and extreme magnitudes can
//! overflow (reported as an error by the caller).

/// The largest scale (fractional digits) a division result is carried to.
const DIV_SCALE_FLOOR: u32 = 6;

/// Parse a decimal string `[-+]?digits[.digits]` into `(mantissa, scale)`.
#[must_use]
pub fn parse(input: &str) -> Option<(i128, u32)> {
    let s = input.trim();
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits: String = format!("{int_part}{frac_part}");
    let mantissa: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let scale = u32::try_from(frac_part.len()).ok()?;
    Some((if neg { -mantissa } else { mantissa }, scale))
}

/// Format `(mantissa, scale)` as a decimal string, with exactly `scale`
/// fractional digits.
#[must_use]
pub fn format(mantissa: i128, scale: u32) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let scale = scale as usize;
    let neg = mantissa < 0;
    let digits = mantissa.unsigned_abs().to_string();
    let body = if digits.len() <= scale {
        format!("0.{digits:0>scale$}")
    } else {
        let point = digits.len() - scale;
        format!("{}.{}", &digits[..point], &digits[point..])
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// `10^exp` as `i128`, or `None` on overflow.
const fn pow10(exp: u32) -> Option<i128> {
    10i128.checked_pow(exp)
}

/// Rescale `mantissa` (currently at `from`) up to scale `to >= from`.
fn rescale(mantissa: i128, from: u32, to: u32) -> Option<i128> {
    mantissa.checked_mul(pow10(to - from)?)
}

/// Bring two decimals to a common scale (the larger of the two), returning
/// `(a_mantissa, b_mantissa, common_scale)`.
fn align(am: i128, asc: u32, bm: i128, bsc: u32) -> Option<(i128, i128, u32)> {
    let scale = asc.max(bsc);
    Some((rescale(am, asc, scale)?, rescale(bm, bsc, scale)?, scale))
}

/// Add two decimals exactly.
#[must_use]
pub fn add(am: i128, asc: u32, bm: i128, bsc: u32) -> Option<(i128, u32)> {
    let (a, b, scale) = align(am, asc, bm, bsc)?;
    Some((a.checked_add(b)?, scale))
}

/// Subtract `b` from `a` exactly.
#[must_use]
pub fn sub(am: i128, asc: u32, bm: i128, bsc: u32) -> Option<(i128, u32)> {
    let (a, b, scale) = align(am, asc, bm, bsc)?;
    Some((a.checked_sub(b)?, scale))
}

/// Multiply two decimals exactly (scales add).
#[must_use]
pub fn mul(am: i128, asc: u32, bm: i128, bsc: u32) -> Option<(i128, u32)> {
    Some((am.checked_mul(bm)?, asc + bsc))
}

/// Divide `a` by `b`, truncating to a result scale of at least
/// [`DIV_SCALE_FLOOR`]. Returns `None` on divide-by-zero or overflow.
#[must_use]
pub fn div(am: i128, asc: u32, bm: i128, bsc: u32) -> Option<(i128, u32)> {
    if bm == 0 {
        return None;
    }
    let result_scale = asc.max(bsc).max(DIV_SCALE_FLOOR);
    // result_mantissa = am * 10^(bsc + result_scale - asc) / bm
    let exp = i64::from(bsc) + i64::from(result_scale) - i64::from(asc);
    let scaled = if exp >= 0 {
        am.checked_mul(pow10(u32::try_from(exp).ok()?)?)?
    } else {
        am / pow10(u32::try_from(-exp).ok()?)?
    };
    Some((scaled / bm, result_scale))
}

/// Compare two decimals.
#[must_use]
pub fn compare(am: i128, asc: u32, bm: i128, bsc: u32) -> std::cmp::Ordering {
    match align(am, asc, bm, bsc) {
        Some((a, b, _)) => a.cmp(&b),
        // On overflow during alignment, fall back to comparing as f64.
        None => to_f64(am, asc).total_cmp(&to_f64(bm, bsc)),
    }
}

/// The `f64` approximation of a decimal.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
pub fn to_f64(mantissa: i128, scale: u32) -> f64 {
    mantissa as f64 / 10f64.powi(scale as i32)
}

/// A whole integer as a scale-0 decimal.
#[must_use]
pub const fn from_i64(n: i64) -> (i128, u32) {
    (n as i128, 0)
}

/// The canonical form of a decimal: trailing fractional zeros removed, so
/// equal values share one representation (for hashing and grouping). Zero
/// normalizes to scale 0.
#[must_use]
pub const fn normalize(mut mantissa: i128, mut scale: u32) -> (i128, u32) {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    (mantissa, scale)
}

/// Round a decimal to the nearest integer (half away from zero).
#[must_use]
pub fn to_i64(mantissa: i128, scale: u32) -> Option<i64> {
    if scale == 0 {
        return i64::try_from(mantissa).ok();
    }
    let factor = pow10(scale)?;
    let half = factor / 2;
    let rounded = if mantissa >= 0 {
        (mantissa + half) / factor
    } else {
        (mantissa - half) / factor
    };
    i64::try_from(rounded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn p(s: &str) -> (i128, u32) {
        parse(s).expect("parse")
    }

    #[test]
    fn parse_and_format_round_trip() {
        for s in ["0", "12", "12.34", "-5.6", "0.001", "1000000.000001"] {
            let (m, sc) = p(s);
            assert_eq!(format(m, sc), s);
        }
        assert_eq!(parse("12.30"), Some((1230, 2)));
        assert_eq!(format(1230, 2), "12.30");
        assert!(parse("1.2.3").is_none());
        assert!(parse("abc").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn exact_arithmetic() {
        // The classic 0.1 + 0.2 that binary floats get wrong.
        let (m, s) = add(p("0.1").0, p("0.1").1, p("0.2").0, p("0.2").1).unwrap();
        assert_eq!(format(m, s), "0.3");
        let (m, s) = mul(p("1.10").0, p("1.10").1, p("3").0, p("3").1).unwrap();
        assert_eq!(format(m, s), "3.30");
        let (m, s) = sub(p("5").0, p("5").1, p("0.25").0, p("0.25").1).unwrap();
        assert_eq!(format(m, s), "4.75");
    }

    #[test]
    fn division_truncates_to_floor_scale() {
        let (m, s) = div(p("10").0, p("10").1, p("3").0, p("3").1).unwrap();
        assert_eq!(format(m, s), "3.333333");
        assert!(div(1, 0, 0, 0).is_none());
    }

    #[test]
    fn compares_across_scales() {
        assert_eq!(
            compare(p("1.5").0, p("1.5").1, p("1.50").0, p("1.50").1),
            Ordering::Equal
        );
        assert_eq!(
            compare(p("1.5").0, p("1.5").1, p("1.49").0, p("1.49").1),
            Ordering::Greater
        );
        assert_eq!(
            compare(p("-2").0, p("-2").1, p("0.1").0, p("0.1").1),
            Ordering::Less
        );
    }

    #[test]
    fn rounds_to_integer() {
        assert_eq!(to_i64(p("2.5").0, p("2.5").1), Some(3));
        assert_eq!(to_i64(p("-2.5").0, p("-2.5").1), Some(-3));
        assert_eq!(to_i64(p("2.4").0, p("2.4").1), Some(2));
    }
}
