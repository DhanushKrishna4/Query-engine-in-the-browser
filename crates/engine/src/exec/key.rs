//! Hashable encodings of values, for join keys and grouping keys.
//!
//! `ScalarValue` cannot be a `HashMap` key: it holds `f64`, which is neither
//! `Eq` nor `Hash`. This module gives a canonical form that is, and pins down
//! two decisions that are easy to get wrong:
//!
//!   * `0.0` and `-0.0` are the same key, because SQL says they are equal;
//!   * every NaN is the same key, so that grouping puts them together -- but
//!     joins consult [`unmatchable`] and refuse to match on them at all.
//!
//! NULL is where grouping and joining part company, and the difference is not
//! a detail: `GROUP BY` puts all NULLs in one group, while an equi-join must
//! never match a NULL against anything, because `NULL = NULL` is unknown.
//! Grouping uses `KeyValue::Null`; joins filter those rows out first.

use crate::types::ScalarValue;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyValue {
    Null,
    Bool(bool),
    Int(i64),
    /// Float bits, with -0.0 and NaN canonicalized.
    Float(u64),
    Str(String),
    Decimal(i128),
}

impl KeyValue {
    pub fn of(v: &ScalarValue) -> KeyValue {
        match v {
            ScalarValue::Null => KeyValue::Null,
            ScalarValue::Boolean(b) => KeyValue::Bool(*b),
            ScalarValue::Int32(i) => KeyValue::Int(*i as i64),
            ScalarValue::Int64(i) => KeyValue::Int(*i),
            ScalarValue::Date32(d) => KeyValue::Int(*d as i64),
            ScalarValue::Timestamp(t) => KeyValue::Int(*t),
            ScalarValue::Float64(f) => KeyValue::Float(canonical_bits(*f)),
            ScalarValue::Utf8(s) => KeyValue::Str(s.clone()),
            ScalarValue::Decimal128 { value, scale, .. } => {
                // Rescale to a common scale so 1.50 and 1.5 hash alike, as they
                // compare alike.
                KeyValue::Decimal(crate::types::rescale(*value, *scale, 18).unwrap_or(*value))
            }
        }
    }
}

fn canonical_bits(f: f64) -> u64 {
    if f == 0.0 {
        // +0.0 and -0.0 are equal, so they must hash the same.
        0.0f64.to_bits()
    } else if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    }
}

/// Whether a value can never take part in an equi-join match.
///
/// NULL because `NULL = NULL` is unknown, and NaN because it is unordered and
/// compares equal to nothing, itself included. Rows keyed on either are absent
/// from the hash table and find nothing when they probe -- but they are still
/// rows, so an outer join still pads and emits them.
pub fn unmatchable(v: &ScalarValue) -> bool {
    match v {
        ScalarValue::Null => true,
        ScalarValue::Float64(f) => f.is_nan(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_zeros_and_nans_canonicalize() {
        assert_eq!(
            KeyValue::of(&ScalarValue::Float64(0.0)),
            KeyValue::of(&ScalarValue::Float64(-0.0))
        );
        assert_eq!(
            KeyValue::of(&ScalarValue::Float64(f64::NAN)),
            KeyValue::of(&ScalarValue::Float64(-f64::NAN))
        );
    }

    #[test]
    fn integers_of_different_widths_share_a_key() {
        assert_eq!(
            KeyValue::of(&ScalarValue::Int32(7)),
            KeyValue::of(&ScalarValue::Int64(7))
        );
    }

    #[test]
    fn nulls_and_nans_never_join() {
        assert!(unmatchable(&ScalarValue::Null));
        assert!(unmatchable(&ScalarValue::Float64(f64::NAN)));
        assert!(!unmatchable(&ScalarValue::Float64(1.0)));
        assert!(!unmatchable(&ScalarValue::Int64(0)));
    }
}
