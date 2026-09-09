//! The type system: logical data types, scalar values, and the coercion and
//! comparison rules that the binder and the expression evaluator share.
//!
//! NULL semantics live here and are commented in detail, because three-valued
//! logic is where correctness quietly dies. The rule this file enforces
//! everywhere: NULL is *unknown*, not a value. Any comparison involving an
//! unknown is itself unknown, which is why `compare()` returns `Option`.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Diagnostic, Result};

/// Logical types. These are the types the binder reasons about; the storage
/// layer picks a physical representation for each (see `storage::ColumnData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// The type of the literal `NULL` before it has been coerced to anything.
    /// It is implicitly convertible to every other type.
    Null,
    Boolean,
    Int32,
    Int64,
    Float64,
    Utf8,
    /// Days since 1970-01-01.
    Date32,
    /// Microseconds since 1970-01-01T00:00:00Z.
    Timestamp,
    /// Fixed-point decimal held as an i128 of unscaled units.
    Decimal128 { precision: u8, scale: i8 },
}

impl DataType {
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Decimal128 { .. }
        )
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, DataType::Int32 | DataType::Int64)
    }

    pub fn is_temporal(&self) -> bool {
        matches!(self, DataType::Date32 | DataType::Timestamp)
    }

    /// Types that can be ordered and compared to one another once coerced.
    pub fn is_comparable(&self) -> bool {
        !matches!(self, DataType::Null)
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Null => f.write_str("NULL"),
            DataType::Boolean => f.write_str("BOOLEAN"),
            DataType::Int32 => f.write_str("INT32"),
            DataType::Int64 => f.write_str("INT64"),
            DataType::Float64 => f.write_str("FLOAT64"),
            DataType::Utf8 => f.write_str("UTF8"),
            DataType::Date32 => f.write_str("DATE32"),
            DataType::Timestamp => f.write_str("TIMESTAMP"),
            DataType::Decimal128 { precision, scale } => {
                write!(f, "DECIMAL128({precision},{scale})")
            }
        }
    }
}

/// A single value. Used for literals, zone-map min/max, and as the unit of the
/// row-at-a-time reference evaluator.
#[derive(Debug, Clone)]
pub enum ScalarValue {
    Null,
    Boolean(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Date32(i32),
    Timestamp(i64),
    Decimal128 { value: i128, precision: u8, scale: i8 },
}

impl ScalarValue {
    pub fn data_type(&self) -> DataType {
        match self {
            ScalarValue::Null => DataType::Null,
            ScalarValue::Boolean(_) => DataType::Boolean,
            ScalarValue::Int32(_) => DataType::Int32,
            ScalarValue::Int64(_) => DataType::Int64,
            ScalarValue::Float64(_) => DataType::Float64,
            ScalarValue::Utf8(_) => DataType::Utf8,
            ScalarValue::Date32(_) => DataType::Date32,
            ScalarValue::Timestamp(_) => DataType::Timestamp,
            ScalarValue::Decimal128 { precision, scale, .. } => DataType::Decimal128 {
                precision: *precision,
                scale: *scale,
            },
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, ScalarValue::Null)
    }

    /// Interpret the value as a SQL boolean under three-valued logic:
    /// `None` means UNKNOWN, not false.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ScalarValue::Boolean(b) => Some(*b),
            ScalarValue::Null => None,
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ScalarValue::Int32(v) => Some(*v as f64),
            ScalarValue::Int64(v) => Some(*v as f64),
            ScalarValue::Float64(v) => Some(*v),
            ScalarValue::Decimal128 { value, scale, .. } => {
                Some(*value as f64 / 10f64.powi(*scale as i32))
            }
            _ => None,
        }
    }

    /// A value of the right type to sit in an array slot that is NULL. The
    /// validity bitmap is what makes it invisible; this is just filler so the
    /// typed array stays dense and indexable by row position.
    pub fn null_placeholder(dt: &DataType) -> ScalarValue {
        match dt {
            DataType::Boolean => ScalarValue::Boolean(false),
            DataType::Int32 => ScalarValue::Int32(0),
            DataType::Int64 => ScalarValue::Int64(0),
            DataType::Float64 => ScalarValue::Float64(0.0),
            DataType::Utf8 => ScalarValue::Utf8(String::new()),
            DataType::Date32 => ScalarValue::Date32(0),
            DataType::Timestamp => ScalarValue::Timestamp(0),
            DataType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
                value: 0,
                precision: *precision,
                scale: *scale,
            },
            DataType::Null => ScalarValue::Null,
        }
    }
}

impl fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarValue::Null => f.write_str("NULL"),
            ScalarValue::Boolean(b) => write!(f, "{}", if *b { "true" } else { "false" }),
            ScalarValue::Int32(v) => write!(f, "{v}"),
            ScalarValue::Int64(v) => write!(f, "{v}"),
            ScalarValue::Float64(v) => {
                if v.fract() == 0.0 && v.is_finite() && v.abs() < 1e15 {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            ScalarValue::Utf8(s) => f.write_str(s),
            ScalarValue::Date32(d) => f.write_str(&format_date(*d)),
            ScalarValue::Timestamp(t) => f.write_str(&format_timestamp(*t)),
            ScalarValue::Decimal128 { value, scale, .. } => {
                f.write_str(&format_decimal(*value, *scale))
            }
        }
    }
}

/// Equality that treats NULL as equal to NULL. This is *not* SQL `=`; it is the
/// identity used by tests and by the plan printer. SQL `=` goes through
/// `compare()`, which returns `None` for NULL.
impl PartialEq for ScalarValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ScalarValue::Null, ScalarValue::Null) => true,
            (ScalarValue::Float64(a), ScalarValue::Float64(b)) => a == b || (a.is_nan() && b.is_nan()),
            _ => matches!(compare(self, other), Some(Ordering::Equal)),
        }
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Three-valued comparison. `None` means the result is UNKNOWN, which happens
/// when either operand is NULL, or when the two values are of types that have
/// no ordering between them.
///
/// Callers must not collapse `None` into `false`: `WHERE x = NULL` matches no
/// rows, but `WHERE NOT (x = NULL)` must *also* match no rows. Only
/// `Some(true)` may pass a filter.
pub fn compare(a: &ScalarValue, b: &ScalarValue) -> Option<Ordering> {
    use ScalarValue::*;
    if a.is_null() || b.is_null() {
        return None;
    }
    match (a, b) {
        (Boolean(x), Boolean(y)) => Some(x.cmp(y)),
        (Utf8(x), Utf8(y)) => Some(x.cmp(y)),
        (Date32(x), Date32(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        (Int32(x), Int32(y)) => Some(x.cmp(y)),
        (Int64(x), Int64(y)) => Some(x.cmp(y)),
        (Int32(x), Int64(y)) => Some((*x as i64).cmp(y)),
        (Int64(x), Int32(y)) => Some(x.cmp(&(*y as i64))),
        (
            Decimal128 { value: x, scale: sx, .. },
            Decimal128 { value: y, scale: sy, .. },
        ) => rescale_pair(*x, *sx, *y, *sy).map(|(x, y)| x.cmp(&y)),
        _ => {
            // Anything left that is numeric on both sides compares as f64.
            // NaN is unordered, which we surface as UNKNOWN rather than
            // inventing a total order.
            let (x, y) = (a.as_f64()?, b.as_f64()?);
            x.partial_cmp(&y)
        }
    }
}

/// Bring two decimals to a common scale so they can be compared exactly.
/// Returns `None` if the rescale would overflow i128.
fn rescale_pair(x: i128, sx: i8, y: i128, sy: i8) -> Option<(i128, i128)> {
    let target = sx.max(sy);
    let x = rescale(x, sx, target)?;
    let y = rescale(y, sy, target)?;
    Some((x, y))
}

pub fn rescale(value: i128, from: i8, to: i8) -> Option<i128> {
    match to.cmp(&from) {
        Ordering::Equal => Some(value),
        Ordering::Greater => {
            let factor = 10i128.checked_pow((to - from) as u32)?;
            value.checked_mul(factor)
        }
        // Scaling down truncates toward zero, matching SQL CAST behaviour.
        Ordering::Less => {
            let factor = 10i128.checked_pow((from - to) as u32)?;
            Some(value / factor)
        }
    }
}

// ---------------------------------------------------------------------------
// Type coercion
// ---------------------------------------------------------------------------

/// The common type two operands of an arithmetic or comparison expression are
/// both converted to. Returning `None` means the combination is a type error;
/// the binder turns that into a diagnostic pointing at the operator.
///
/// The rules, in the order they are applied:
///   1. NULL takes on the other side's type (a NULL literal is polymorphic).
///   2. Identical types stay put.
///   3. Integers widen: Int32 + Int64 -> Int64.
///   4. Any integer meeting Float64 becomes Float64.
///   5. Decimal meeting any other numeric degrades to Float64. This loses
///      precision, which is the wrong answer for money; exact decimal
///      arithmetic is deliberately deferred rather than faked.
///   6. Date32 and Timestamp unify as Timestamp.
///   7. Everything else is an error. In particular Utf8 does *not* silently
///      coerce to a number here -- string/number comparison is handled by the
///      binder, which only folds a *literal* string into a temporal or numeric
///      type, and only when it parses cleanly.
pub fn common_type(l: DataType, r: DataType) -> Option<DataType> {
    use DataType::*;
    if l == r {
        return Some(l);
    }
    match (l, r) {
        (Null, t) | (t, Null) => Some(t),

        (Int32, Int64) | (Int64, Int32) => Some(Int64),
        (Float64, t) | (t, Float64) if t.is_numeric() => Some(Float64),

        (Decimal128 { .. }, t) | (t, Decimal128 { .. }) if t.is_numeric() => Some(Float64),

        (Date32, Timestamp) | (Timestamp, Date32) => Some(Timestamp),

        _ => None,
    }
}

/// Whether a value of type `from` can be produced as type `to` by an implicit
/// widening cast (no information lost, no parsing, never fails at runtime).
pub fn can_widen(from: DataType, to: DataType) -> bool {
    use DataType::*;
    if from == to || from == Null {
        return true;
    }
    matches!(
        (from, to),
        (Int32, Int64) | (Int32, Float64) | (Int64, Float64) | (Date32, Timestamp)
    )
}

/// Explicit CAST. Fallible: a string that does not parse yields an error rather
/// than NULL, so that a typo in a query is reported instead of silently
/// producing empty results.
pub fn cast_scalar(v: &ScalarValue, to: &DataType) -> Result<ScalarValue> {
    use DataType as T;
    if v.is_null() {
        return Ok(ScalarValue::Null);
    }
    let err = |v: &ScalarValue, to: &DataType| {
        Diagnostic::exec(format!("cannot cast value `{v}` to {to}"))
    };
    let out = match to {
        T::Null => ScalarValue::Null,
        T::Boolean => match v {
            ScalarValue::Boolean(b) => ScalarValue::Boolean(*b),
            ScalarValue::Int32(i) => ScalarValue::Boolean(*i != 0),
            ScalarValue::Int64(i) => ScalarValue::Boolean(*i != 0),
            ScalarValue::Utf8(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" => ScalarValue::Boolean(true),
                "false" | "f" | "no" | "n" | "0" => ScalarValue::Boolean(false),
                _ => return Err(err(v, to)),
            },
            _ => return Err(err(v, to)),
        },
        T::Int32 => {
            let i = to_i64_or_parse(v).ok_or_else(|| err(v, to))?;
            let i = i32::try_from(i).map_err(|_| err(v, to))?;
            ScalarValue::Int32(i)
        }
        T::Int64 => ScalarValue::Int64(to_i64_or_parse(v).ok_or_else(|| err(v, to))?),
        T::Float64 => match v {
            ScalarValue::Utf8(s) => {
                ScalarValue::Float64(s.trim().parse::<f64>().map_err(|_| err(v, to))?)
            }
            _ => ScalarValue::Float64(v.as_f64().ok_or_else(|| err(v, to))?),
        },
        T::Utf8 => ScalarValue::Utf8(v.to_string()),
        T::Date32 => match v {
            ScalarValue::Date32(d) => ScalarValue::Date32(*d),
            ScalarValue::Utf8(s) => ScalarValue::Date32(parse_date(s).ok_or_else(|| err(v, to))?),
            // Truncating a timestamp to a date floors toward the past, so that
            // pre-epoch timestamps land on the day that contains them.
            ScalarValue::Timestamp(t) => {
                ScalarValue::Date32(t.div_euclid(MICROS_PER_DAY) as i32)
            }
            _ => return Err(err(v, to)),
        },
        T::Timestamp => match v {
            ScalarValue::Timestamp(t) => ScalarValue::Timestamp(*t),
            ScalarValue::Date32(d) => ScalarValue::Timestamp(*d as i64 * MICROS_PER_DAY),
            ScalarValue::Utf8(s) => {
                ScalarValue::Timestamp(parse_timestamp(s).ok_or_else(|| err(v, to))?)
            }
            _ => return Err(err(v, to)),
        },
        T::Decimal128 { precision, scale } => {
            let unscaled = match v {
                ScalarValue::Decimal128 { value, scale: from, .. } => {
                    rescale(*value, *from, *scale).ok_or_else(|| err(v, to))?
                }
                ScalarValue::Int32(_) | ScalarValue::Int64(_) => {
                    let i = to_i64(v).ok_or_else(|| err(v, to))? as i128;
                    rescale(i, 0, *scale).ok_or_else(|| err(v, to))?
                }
                ScalarValue::Float64(f) => (f * 10f64.powi(*scale as i32)).round() as i128,
                ScalarValue::Utf8(s) => parse_decimal(s, *scale).ok_or_else(|| err(v, to))?,
                _ => return Err(err(v, to)),
            };
            ScalarValue::Decimal128 {
                value: unscaled,
                precision: *precision,
                scale: *scale,
            }
        }
    };
    Ok(out)
}

/// `to_i64`, plus parsing for string inputs. Only explicit CAST goes through
/// here; implicit coercion never parses a string on the fly.
fn to_i64_or_parse(v: &ScalarValue) -> Option<i64> {
    match v {
        ScalarValue::Utf8(s) => s.trim().parse::<i64>().ok(),
        other => to_i64(other),
    }
}

fn to_i64(v: &ScalarValue) -> Option<i64> {
    match v {
        ScalarValue::Int32(i) => Some(*i as i64),
        ScalarValue::Int64(i) => Some(*i),
        ScalarValue::Boolean(b) => Some(*b as i64),
        ScalarValue::Date32(d) => Some(*d as i64),
        ScalarValue::Timestamp(t) => Some(*t),
        ScalarValue::Float64(f) => {
            if f.is_finite() && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                Some(f.trunc() as i64)
            } else {
                None
            }
        }
        ScalarValue::Decimal128 { value, scale, .. } => {
            i64::try_from(rescale(*value, *scale, 0)?).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Temporal helpers
// ---------------------------------------------------------------------------

/// Values whose `hash_scalar` and `compare` both behave exactly within the
/// class, and may not across it.
///
/// Two guarantees ride on this. The bloom filters hash column values at build
/// time and literals at probe time, so a literal that hashes differently from
/// an equal column value would silently prune a row group that contains it. And
/// the B+ tree orders keys with `compare`, so a literal whose comparison
/// against the column's type is lossy -- an `Int64` beyond 2^53 against a
/// `Float64`, say -- could place a range boundary in the wrong place and lose
/// rows. Both structures therefore refuse a literal from a different class and
/// fall back to a full scan, which is always correct.
///
/// The integer-ish types share a class because `hash_scalar` widens them all to
/// `i64` and compares them exactly. Colliding a `Date32` with an `Int64` of the
/// same number is harmless: it can only cost a scan, never an answer, and the
/// binder rejects the comparison long before either structure sees it.
pub fn value_class(t: &DataType) -> u8 {
    match t {
        DataType::Int32 | DataType::Int64 | DataType::Date32 | DataType::Timestamp => 1,
        DataType::Float64 => 2,
        DataType::Utf8 => 3,
        DataType::Boolean => 4,
        DataType::Decimal128 { .. } => 5,
        DataType::Null => 0,
    }
}

/// A 64-bit hash of a value, canonical enough to stand in for equality.
///
/// Floats are normalized so `0.0` and `-0.0` hash alike, matching the equality
/// that grouping, joining and set operations all use. NULL hashes to a fixed
/// value; callers that care about NULL semantics filter it out first, since
/// "how many distinct values" and "how many rows are missing one" are different
/// questions.
pub fn hash_scalar(v: &ScalarValue) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    match v {
        ScalarValue::Null => 0u8.hash(&mut h),
        ScalarValue::Boolean(b) => b.hash(&mut h),
        ScalarValue::Int32(i) => (*i as i64).hash(&mut h),
        ScalarValue::Int64(i) => i.hash(&mut h),
        ScalarValue::Date32(d) => (*d as i64).hash(&mut h),
        ScalarValue::Timestamp(t) => t.hash(&mut h),
        ScalarValue::Float64(f) => {
            let bits = if *f == 0.0 {
                0.0f64.to_bits()
            } else if f.is_nan() {
                f64::NAN.to_bits()
            } else {
                f.to_bits()
            };
            bits.hash(&mut h);
        }
        ScalarValue::Utf8(s) => s.hash(&mut h),
        ScalarValue::Decimal128 { value, scale, .. } => {
            rescale(*value, *scale, 18).unwrap_or(*value).hash(&mut h);
        }
    }
    h.finish()
}

pub const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Days from the civil calendar date to 1970-01-01, by Howard Hinnant's
/// `days_from_civil`. Valid for the whole proleptic Gregorian calendar and
/// branch-free, which is why it is used instead of a table of month lengths.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i32 - 719_468
}

/// Inverse of `days_from_civil`.
pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses `YYYY-MM-DD`. Deliberately strict: no locale formats, no two-digit
/// years, no slashes. Ambiguous date formats in CSVs are a data-quality problem
/// that should be surfaced, not guessed at.
pub fn parse_date(s: &str) -> Option<i32> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Reject 2023-02-30 and friends by round-tripping through the calendar.
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return None;
    }
    Some(days)
}

pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY) as i32;
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let secs = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let base = format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    if frac == 0 {
        base
    } else {
        format!("{base}.{frac:06}")
    }
}

/// Parses `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]]`, optionally with a trailing `Z`.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date_part, time_part) = match s.find([' ', 'T']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let days = parse_date(date_part)? as i64;
    if time_part.is_empty() {
        return Some(days * MICROS_PER_DAY);
    }
    let (hms, frac) = match time_part.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time_part, ""),
    };
    let mut it = hms.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let mi: i64 = it.next()?.parse().ok()?;
    let sec: i64 = match it.next() {
        Some(v) => v.parse().ok()?,
        None => 0,
    };
    if it.next().is_some() || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let micros = if frac.is_empty() {
        0
    } else {
        // Right-pad or truncate the fraction to exactly 6 digits.
        let mut f = frac.to_string();
        f.truncate(6);
        while f.len() < 6 {
            f.push('0');
        }
        f.parse::<i64>().ok()?
    };
    Some(days * MICROS_PER_DAY + (h * 3600 + mi * 60 + sec) * 1_000_000 + micros)
}

pub fn format_decimal(unscaled: i128, scale: i8) -> String {
    if scale <= 0 {
        let factor = 10i128.pow((-scale) as u32);
        return (unscaled * factor).to_string();
    }
    let neg = unscaled < 0;
    let mag = unscaled.unsigned_abs();
    let factor = 10u128.pow(scale as u32);
    let int = mag / factor;
    let frac = mag % factor;
    format!(
        "{}{int}.{frac:0width$}",
        if neg { "-" } else { "" },
        width = scale as usize
    )
}

pub fn parse_decimal(s: &str, scale: i8) -> Option<i128> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int: i128 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let mut value = rescale(int, 0, scale)?;
    if scale > 0 && !frac_part.is_empty() {
        let mut f = frac_part.to_string();
        f.truncate(scale as usize);
        let digits = f.len();
        let frac: i128 = f.parse().ok()?;
        value = value.checked_add(rescale(frac, digits as i8, scale)?)?;
    }
    Some(if neg { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_compares_as_unknown_not_false() {
        // The whole point of three-valued logic: NULL = NULL is UNKNOWN.
        assert_eq!(compare(&ScalarValue::Null, &ScalarValue::Null), None);
        assert_eq!(compare(&ScalarValue::Int64(1), &ScalarValue::Null), None);
        assert_eq!(
            compare(&ScalarValue::Int64(1), &ScalarValue::Int64(2)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn integers_compare_across_widths() {
        assert_eq!(
            compare(&ScalarValue::Int32(5), &ScalarValue::Int64(5)),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn coercion_rules() {
        use DataType::*;
        assert_eq!(common_type(Int32, Int64), Some(Int64));
        assert_eq!(common_type(Int64, Float64), Some(Float64));
        assert_eq!(common_type(Null, Utf8), Some(Utf8));
        assert_eq!(common_type(Date32, Timestamp), Some(Timestamp));
        assert_eq!(common_type(Utf8, Int64), None);
        assert_eq!(common_type(Boolean, Int64), None);
    }

    #[test]
    fn civil_calendar_roundtrips() {
        for &(y, m, d) in &[
            (1970, 1, 1),
            (1969, 12, 31),
            (2000, 2, 29),
            (1900, 3, 1),
            (2024, 12, 31),
            (1, 1, 1),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(format_date(0), "1970-01-01");
    }

    #[test]
    fn rejects_impossible_dates() {
        assert_eq!(parse_date("2023-02-30"), None);
        assert_eq!(parse_date("2023-13-01"), None);
        assert_eq!(parse_date("23-01-01"), None);
        assert!(parse_date("2024-02-29").is_some());
    }

    #[test]
    fn timestamps_roundtrip_including_before_epoch() {
        for s in ["1970-01-01 00:00:00", "2024-03-05 13:45:01.500000", "1899-12-31 23:59:59"] {
            let t = parse_timestamp(s).unwrap();
            assert_eq!(format_timestamp(t), s, "{s}");
        }
        assert_eq!(parse_timestamp("2024-03-05"), parse_timestamp("2024-03-05 00:00:00"));
        assert_eq!(parse_timestamp("2024-03-05T01:02:03Z"), parse_timestamp("2024-03-05 01:02:03"));
    }

    #[test]
    fn decimal_display_and_parse() {
        assert_eq!(format_decimal(123456, 2), "1234.56");
        assert_eq!(format_decimal(-5, 2), "-0.05");
        assert_eq!(parse_decimal("1234.56", 2), Some(123456));
        assert_eq!(parse_decimal("-0.05", 2), Some(-5));
        assert_eq!(parse_decimal("abc", 2), None);
    }

    #[test]
    fn decimals_compare_across_scales() {
        let a = ScalarValue::Decimal128 { value: 150, precision: 10, scale: 2 }; // 1.50
        let b = ScalarValue::Decimal128 { value: 15, precision: 10, scale: 1 }; // 1.5
        assert_eq!(compare(&a, &b), Some(Ordering::Equal));
    }

    #[test]
    fn casts_report_failure_rather_than_returning_null() {
        assert!(cast_scalar(&ScalarValue::Utf8("nope".into()), &DataType::Int64).is_err());
        assert_eq!(
            cast_scalar(&ScalarValue::Utf8(" 42 ".into()), &DataType::Int64).unwrap(),
            ScalarValue::Int64(42)
        );
        // ... but casting an actual NULL is always fine and stays NULL.
        assert!(cast_scalar(&ScalarValue::Null, &DataType::Int64).unwrap().is_null());
    }
}
